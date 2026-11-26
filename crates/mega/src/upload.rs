//! Uploading a file: MEGA hands out a URL (`u`), the file goes to it encrypted with a fresh
//! AES-CTR key, in pieces (`<url>/<offset>`) of whole MAC chunks up to [`PIECE`] bytes, up to
//! [`SENDERS`] at once, and the answer to the piece that completes it is a token that `p` turns
//! into the new node, its key under the master key. A small file goes in one request.
//!
//! A MAC chunk at a time, one after the other, a file of 512 KiB took three round trips to
//! MEGA's servers besides `u` and `p`, and one of 100 MiB about a hundred: 28 s.

use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use openexplorer_i18n::{t, tr};
use serde_json::{Map, Value, json};

use crate::account::Account;
use crate::crypto::{Ctr, Mac, chunk_len, encrypt_attrs, merge_file_key, random};
use crate::download::Progress;
use crate::nodes::{Kind, Node};
use crate::{CANCELLED, Result};

/// How many times a piece that didn't arrive is sent again.
const RETRIES: u32 = 3;
/// The most bytes in one request: whole MAC chunks up to this.
const PIECE: u64 = 8 << 20;
/// Pieces of one file sent at once.
const SENDERS: usize = 6;

impl Account {
    /// Creates the file at `path` with the `len` bytes of `src`; one already there of that name
    /// goes to the Rubbish Bin once the new one is in place.
    pub fn upload(&self, path: &str, src: &mut dyn Read, len: u64, progress: Progress) -> Result<Node> {
        let (parent, name) = self.parent_and_name(path)?;
        self.writable(&parent)?;
        let old: Vec<Node> = self.lock().tree.children(&parent.handle).into_iter().map(|(_, n)| n).filter(|n| n.name == name).cloned().collect();
        if old.iter().any(|n| n.kind != Kind::File) {
            return Err(tr!("Já existe uma pasta “{name}” aí.", "There's already a folder “{name}” there."));
        }
        let answer = self.api.run(json!({"a": "u", "s": len, "ssl": 2}))?;
        let url = answer.get("p").and_then(Value::as_str).ok_or(t("O MEGA não deu o endereço para enviar.", "MEGA didn't give the address to upload to."))?.to_string();
        let (key, nonce) = (random::<16>(), random::<8>());
        let mut mac = Mac::new(&key, &nonce);
        let agent = crate::api::transfers();
        let token: Mutex<Option<String>> = Mutex::new(None);
        let failed: Mutex<Option<String>> = Mutex::new(None);
        let fail = |e: String| {
            failed.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert(e);
        };
        let stopped = || failed.lock().unwrap_or_else(PoisonError::into_inner).is_some();
        let sent = AtomicU64::new(0);
        // Read, MACed and encrypted in order here; sent by the senders as they come.
        let (tx, rx) = sync_channel::<(u64, Vec<u8>)>(2);
        let rx = Mutex::new(rx);
        std::thread::scope(|scope| {
            let senders: Vec<_> = (0..SENDERS)
                .map(|_| {
                    scope.spawn(|| {
                        loop {
                            let Ok((offset, buf)) = rx.lock().unwrap_or_else(PoisonError::into_inner).recv() else { break };
                            if stopped() {
                                continue;
                            }
                            match post(&agent, &url, offset, &buf, &name) {
                                Ok(Some(t)) => *token.lock().unwrap_or_else(PoisonError::into_inner) = Some(t),
                                Ok(None) => {}
                                Err(e) => fail(e),
                            }
                            sent.fetch_add(buf.len() as u64, Ordering::Relaxed);
                        }
                    })
                })
                .collect();
            for (offset, n) in pieces(len) {
                let mut buf = vec![0u8; n as usize];
                if let Err(e) = read_full(src, &mut buf) {
                    fail(e);
                    break;
                }
                mac.update(&buf);
                Ctr::new(&key, &nonce, offset).apply(&mut buf);
                if tx.send((offset, buf)).is_err() || stopped() {
                    break;
                }
                if !progress(sent.load(Ordering::Relaxed)) {
                    fail(CANCELLED.into());
                    break;
                }
            }
            drop(tx);
            while !senders.iter().all(|s| s.is_finished()) {
                if !progress(sent.load(Ordering::Relaxed)) {
                    fail(CANCELLED.into());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        if let Some(e) = failed.into_inner().unwrap_or_else(PoisonError::into_inner) {
            return Err(e);
        }
        if !progress(len) {
            return Err(CANCELLED.into());
        }
        let token = token.into_inner().unwrap_or_else(PoisonError::into_inner);
        let token = token.ok_or(t("O MEGA não confirmou o envio.", "MEGA didn't confirm the upload."))?;
        let merged = merge_file_key(&key, &nonce, &mac.finish());
        let mut attrs = Map::new();
        attrs.insert("n".into(), name.clone().into());
        let node = json!({"h": token, "t": 0, "a": encrypt_attrs(&key, &attrs), "k": self.wrap(&merged)});
        let made = self.put_nodes(&parent.handle, vec![node], &[merged.to_vec()])?.pop().ok_or_else(|| tr!("O MEGA não criou “{name}”.", "MEGA didn't create “{name}”."))?;
        // The file it replaces goes where a delete would send it (inside a share, for good).
        for n in old {
            if let Err(e) = self.discard(&n) {
                crate::api::log(&format!("MEGA: o arquivo substituído ficou: {e}"));
            }
        }
        Ok(made)
    }
}

/// Where each request's piece starts and how long it is: whole MAC chunks up to [`PIECE`] bytes
/// (a smaller file in one), and one empty piece for an empty file.
fn pieces(len: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let (mut start, mut end, mut index) = (0u64, 0u64, 0u64);
    while end < len {
        let chunk = chunk_len(index).min(len - end);
        if end > start && end + chunk - start > PIECE {
            out.push((start, end - start));
            start = end;
        }
        end += chunk;
        index += 1;
    }
    out.push((start, end - start));
    out
}

/// Sends the piece at `offset`, again after a failure: MEGA's token when this piece completed
/// the file.
fn post(agent: &ureq::Agent, url: &str, offset: u64, buf: &[u8], name: &str) -> Result<Option<String>> {
    let mut tries = 0;
    let answer = loop {
        match agent.post(format!("{url}/{offset}")).send(buf) {
            Ok(mut r) if r.status().as_u16() == 200 => break r.body_mut().read_to_string().unwrap_or_default(),
            Ok(r) if tries >= RETRIES => return Err(tr!("O MEGA respondeu HTTP {} ao receber “{name}”.", "MEGA answered HTTP {} receiving “{name}”.", r.status().as_u16())),
            Err(e) if tries >= RETRIES => return Err(tr!("Sem conexão com o MEGA: {e}", "No connection to MEGA: {e}")),
            _ => {
                tries += 1;
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    };
    let answer = answer.trim();
    match answer.parse::<i64>() {
        Ok(code) if code < 0 => Err(crate::api::error_text(code)),
        Ok(_) => Ok(None),
        Err(_) if !answer.is_empty() => Ok(Some(answer.to_string())),
        Err(_) => Ok(None),
    }
}

/// Fills `buf` from `src`, which must have that much.
fn read_full(src: &mut dyn Read, buf: &mut [u8]) -> Result<()> {
    let mut at = 0;
    while at < buf.len() {
        match src.read(&mut buf[at..]) {
            Ok(0) => return Err(t("O arquivo ficou menor durante o envio.", "The file got smaller during the upload.").into()),
            Ok(n) => at += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(tr!("Não consegui ler o arquivo: {e}", "Couldn't read the file: {e}")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_are_whole_mac_chunks_up_to_the_most_a_request_takes() {
        assert_eq!(pieces(0), [(0, 0)]);
        assert_eq!(pieces(512 * 1024), [(0, 512 * 1024)], "a small file goes in one request");
        let len = 100u64 << 20;
        let p = pieces(len);
        assert_eq!(p.iter().map(|(_, n)| n).sum::<u64>(), len);
        let mut bounds = std::collections::HashSet::new();
        let mut at = 0;
        for i in 0.. {
            bounds.insert(at);
            if at >= len {
                break;
            }
            at += chunk_len(i).min(len - at);
        }
        for (i, &(start, n)) in p.iter().enumerate() {
            assert!(n <= PIECE, "{i}: {n}");
            assert!(bounds.contains(&start) && bounds.contains(&(start + n)), "{i}: {start}+{n}");
            if i > 0 {
                assert_eq!(start, p[i - 1].0 + p[i - 1].1);
            }
        }
        assert!(p.len() <= 14, "{} requests", p.len());
    }
}
