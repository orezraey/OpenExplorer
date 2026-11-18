//! Uploading a file: MEGA hands out a URL (`u`), the file goes to it encrypted with a fresh
//! AES-CTR key, a MAC chunk at a time (`<url>/<offset>`), and the last chunk's answer is a token
//! that `p` turns into the new node, its key under the master key.

use std::io::Read;
use std::time::Duration;

use openexplorer_i18n::{t, tr};
use serde_json::{Map, Value, json};

use crate::account::Account;
use crate::crypto::{Ctr, Mac, chunk_len, encrypt_attrs, merge_file_key, random};
use crate::download::Progress;
use crate::nodes::{Kind, Node};
use crate::{CANCELLED, Result};

/// How many times a chunk that didn't arrive is sent again.
const RETRIES: u32 = 3;

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
        let mut ctr = Ctr::new(&key, &nonce, 0);
        let mut mac = Mac::new(&key, &nonce);
        let agent = crate::api::agent(Duration::from_secs(120));
        let mut token = None;
        let mut offset = 0u64;
        let mut index = 0u64;
        loop {
            let n = chunk_len(index).min(len - offset) as usize;
            let mut buf = vec![0u8; n];
            read_full(src, &mut buf)?;
            mac.update(&buf);
            ctr.apply(&mut buf);
            let mut tries = 0;
            let answer = loop {
                match agent.post(format!("{url}/{offset}")).send(&buf[..]) {
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
                Ok(code) if code < 0 => return Err(crate::api::error_text(code)),
                Ok(_) => {}
                Err(_) if !answer.is_empty() => token = Some(answer.to_string()),
                Err(_) => {}
            }
            offset += n as u64;
            index += 1;
            if !progress(offset) {
                return Err(CANCELLED.into());
            }
            if offset >= len {
                break;
            }
        }
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
