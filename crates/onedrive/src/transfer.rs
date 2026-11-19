//! Downloads and uploads.
//!
//! A download reads the item's pre-authenticated address and fetches it with no token, asking
//! again from where it stopped (`Range`) when the connection drops, with a fresh address (they
//! expire). An upload of up to 4 MiB goes in one request; a bigger one in an upload session, in
//! parts of 10 MiB (a multiple of 320 KiB, as Graph asks), each part held in memory so it can be
//! sent again from the byte the session says it's missing. Both check the QuickXorHash Graph
//! gives against the bytes that went through. Both report progress as the bytes go, and
//! Cancelar cuts the body short, so what wasn't all sent is never made; what was is kept.

use std::io::{Read, Write};
use std::time::Duration;

use openexplorer_i18n::{t, tr};
use serde_json::{Value, json};

use crate::account::{Account, Item};
use crate::graph::{Counted, transfer_agent};
use crate::quickxor::QuickXor;
use crate::tree::Seen;
use crate::{CANCELLED, Result, log};

/// Files up to this go in one request.
pub const SIMPLE: u64 = 4 * 1024 * 1024;
/// An upload session's part: 32 × 320 KiB.
pub const PART: usize = 32 * 320 * 1024;
/// How many times a dropped transfer is taken up again.
const RETRIES: u32 = 5;

/// The hash Graph gives for a file, if any.
fn graph_hash(v: &Value) -> Option<String> {
    v.pointer("/file/hashes/quickXorHash").and_then(Value::as_str).map(str::to_string)
}

fn mismatch(name: &str) -> String {
    tr!(
        "\"{name}\" chegou diferente do que saiu (o hash QuickXor não confere). Tente de novo.",
        "\"{name}\" arrived different from what was sent (the QuickXor hash doesn't match). Try again."
    )
}

/// Reads up to `buf.len()` bytes, fewer only at the end.
fn fill(src: &mut dyn Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match src.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

impl Account {
    /// Writes the file at `path` into `out`.
    pub fn download(&self, path: &str, out: &mut dyn Write, progress: &mut dyn FnMut(u64) -> bool) -> Result<()> {
        let mut item = self.item_with_url(path)?;
        let name = item.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
        if item.get("folder").is_some() {
            return Err(tr!("\"{name}\" é uma pasta.", "\"{name}\" is a folder."));
        }
        let size = item.get("size").and_then(Value::as_u64).unwrap_or(0);
        let want = graph_hash(&item);
        let agent = transfer_agent();
        let mut done = 0u64;
        let mut hash = QuickXor::new();
        let mut tries = 0;
        let mut buf = vec![0u8; 256 * 1024];
        while done < size {
            let url = item.get("@microsoft.graph.downloadUrl").and_then(Value::as_str).map(str::to_string).ok_or_else(|| t("O OneDrive não deu o endereço do arquivo.", "OneDrive gave no address for the file."))?;
            let mut req = agent.get(&url);
            if done > 0 {
                req = req.header("Range", format!("bytes={done}-"));
            }
            let dropped: String = match req.call() {
                Ok(mut resp) => {
                    let status = resp.status().as_u16();
                    if status == 429 || status == 503 {
                        std::thread::sleep(Duration::from_secs(crate::graph::retry_after(&resp).unwrap_or(2).min(60)));
                        format!("HTTP {status}")
                    } else if !(200..300).contains(&status) {
                        format!("HTTP {status}")
                    } else {
                        // A server that ignored the range sends it all again: skip what's here.
                        let mut skip = if status == 200 { done } else { 0 };
                        let mut body = resp.body_mut().as_reader();
                        let mut failed = None;
                        loop {
                            match body.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => {
                                    let s = (skip.min(n as u64)) as usize;
                                    skip -= s as u64;
                                    let fresh = &buf[s..n];
                                    if fresh.is_empty() {
                                        continue;
                                    }
                                    out.write_all(fresh).map_err(|e| tr!("Não consegui gravar o arquivo: {e}", "Couldn't write the file: {e}"))?;
                                    hash.update(fresh);
                                    done += fresh.len() as u64;
                                    if !progress(done) {
                                        return Err(CANCELLED.into());
                                    }
                                }
                                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                                Err(e) => {
                                    failed = Some(e.to_string());
                                    break;
                                }
                            }
                        }
                        match failed {
                            None if done >= size => break,
                            None => t("a conexão terminou antes do fim", "the connection ended early").to_string(),
                            Some(e) => e,
                        }
                    }
                }
                Err(e) => e.to_string(),
            };
            tries += 1;
            if tries > RETRIES {
                return Err(tr!("O download de \"{name}\" parou: {dropped}", "The download of \"{name}\" stopped: {dropped}"));
            }
            log(&format!("OneDrive: download de {name} caiu em {done} de {size} ({dropped}); retomando"));
            std::thread::sleep(Duration::from_millis(500 << tries.min(4)));
            // The address may have expired: a fresh one.
            item = self.item_with_url(path)?;
        }
        if done != size {
            return Err(tr!("\"{name}\" veio com {done} bytes de {size}.", "\"{name}\" came with {done} bytes of {size}."));
        }
        if want.as_ref().is_some_and(|w| *w != hash.base64()) {
            return Err(mismatch(&name));
        }
        Ok(())
    }

    /// The item at `path` with its download address (which `$select` can't ask for).
    fn item_with_url(&self, path: &str) -> Result<Value> {
        let addr = self.addr_of(path)?;
        Ok(self.graph.get(&addr, t("Baixar", "Download"))?)
    }

    /// Creates or replaces the file at `path` with the `len` bytes of `src`.
    pub fn upload(&self, path: &str, src: &mut dyn Read, len: u64, progress: &mut dyn FnMut(u64) -> bool) -> Result<Item> {
        let (url, name) = self.new_child(path)?;
        let mut hash = QuickXor::new();
        let v = if len <= SIMPLE {
            let mut data = vec![0u8; len as usize];
            let n = fill(src, &mut data).map_err(|e| tr!("Não consegui ler o arquivo: {e}", "Couldn't read the file: {e}"))?;
            if n as u64 != len {
                return Err(tr!("\"{name}\" mudou de tamanho durante o envio.", "\"{name}\" changed size while being sent."));
            }
            hash.update(&data);
            // Cancelar cuts the body short, so the file is never made; once all of it went, the
            // upload is done, and isn't undone by a Cancelar that came while Graph answered.
            self.graph.send_counted("PUT", &format!("{url}/content"), &data, "application/octet-stream", progress)?.json(t("Enviar", "Upload"))?
        } else {
            self.upload_session(&url, &name, src, len, &mut hash, progress)?
        };
        if v.get("size").and_then(Value::as_u64).is_some_and(|s| s != len) {
            return Err(mismatch(&name));
        }
        if let Some(h) = graph_hash(&v)
            && h != hash.base64()
        {
            log(&format!("OneDrive: hash de {name} não confere: {h} ≠ {}", hash.base64()));
            return Err(mismatch(&name));
        }
        self.took(&v);
        Seen::of(&v).map(|s| Item { name: s.name.clone().unwrap_or(name.clone()), is_dir: false, size: Some(len), modified: Some(s.modified), id: Some(s.id) }).ok_or_else(|| t("Resposta inesperada do OneDrive.", "Unexpected answer from OneDrive.").into())
    }

    fn upload_session(&self, url: &str, name: &str, src: &mut dyn Read, len: u64, hash: &mut QuickXor, progress: &mut dyn FnMut(u64) -> bool) -> Result<Value> {
        let body = json!({ "item": { "@microsoft.graph.conflictBehavior": "replace" } });
        let session = self.graph.call("POST", &format!("{url}/createUploadSession"), Some(&body), t("Enviar", "Upload"))?;
        let up = session.get("uploadUrl").and_then(Value::as_str).ok_or_else(|| t("O OneDrive não abriu a sessão de envio.", "OneDrive didn't open the upload session."))?.to_string();
        let agent = transfer_agent();
        let cancel = |agent: &ureq::Agent| {
            let _ = agent.delete(&up).call();
        };
        let mut part = vec![0u8; PART];
        let mut offset = 0u64;
        loop {
            let want = (len - offset).min(PART as u64) as usize;
            let n = fill(src, &mut part[..want]).map_err(|e| {
                cancel(&agent);
                tr!("Não consegui ler o arquivo: {e}", "Couldn't read the file: {e}")
            })?;
            if n != want {
                cancel(&agent);
                return Err(tr!("\"{name}\" mudou de tamanho durante o envio.", "\"{name}\" changed size while being sent."));
            }
            hash.update(&part[..n]);
            // Within this part, from where the session says it's missing bytes.
            let mut from = 0usize;
            let mut tries = 0;
            let answer = loop {
                let start = offset + from as u64;
                let end = offset + n as u64 - 1;
                let mut counted = Counted { data: &part[from..n], at: 0, base: start, progress, stopped: false };
                let result = {
                    let req = ureq::http::Request::builder()
                        .method("PUT")
                        .uri(&up)
                        .header("Content-Length", (n - from).to_string())
                        .header("Content-Range", format!("bytes {start}-{end}/{len}"))
                        .body(ureq::SendBody::from_reader(&mut counted))
                        .map_err(|e| e.to_string())?;
                    agent.run(req)
                };
                if counted.stopped {
                    cancel(&agent);
                    return Err(CANCELLED.into());
                }
                let problem = match result {
                    Ok(mut resp) => {
                        let status = resp.status().as_u16();
                        let text = resp.body_mut().read_to_string().unwrap_or_default();
                        match status {
                            200 | 201 => break Some(serde_json::from_str::<Value>(&text).map_err(|e| e.to_string())?),
                            202 => break None,
                            404 => return Err(tr!("A sessão de envio de \"{name}\" expirou; envie de novo.", "The upload session of \"{name}\" expired; send it again.")),
                            429 | 503 => {
                                std::thread::sleep(Duration::from_secs(crate::graph::retry_after(&resp).unwrap_or(2).min(60)));
                                format!("HTTP {status}")
                            }
                            s if (400..500).contains(&s) && s != 416 => {
                                cancel(&agent);
                                return Err(crate::graph::failure(s, &text, t("Enviar", "Upload")).text);
                            }
                            s => format!("HTTP {s}"),
                        }
                    }
                    Err(e) => e.to_string(),
                };
                tries += 1;
                if tries > RETRIES {
                    cancel(&agent);
                    return Err(tr!("O envio de \"{name}\" parou: {problem}", "The upload of \"{name}\" stopped: {problem}"));
                }
                log(&format!("OneDrive: parte de {name} em {start} caiu ({problem}); perguntando à sessão"));
                std::thread::sleep(Duration::from_millis(500 << tries.min(4)));
                // What the session is missing now.
                match missing_from(&agent, &up) {
                    Some(s) if s >= offset + n as u64 => break None,
                    Some(s) if s >= offset => from = (s - offset) as usize,
                    Some(_) | None => {}
                }
            };
            offset += n as u64;
            if let Some(item) = answer {
                return if offset == len { Ok(item) } else { Err(mismatch(name)) };
            }
            if offset >= len {
                // Every byte went, but the session never gave the file: ask it once more.
                return Err(tr!("O OneDrive não confirmou o envio de \"{name}\".", "OneDrive didn't confirm the upload of \"{name}\"."));
            }
        }
    }
}

/// The first byte an upload session still waits for (`nextExpectedRanges`).
fn missing_from(agent: &ureq::Agent, up: &str) -> Option<u64> {
    let mut resp = agent.get(up).call().ok()?;
    let v: Value = serde_json::from_str(&resp.body_mut().read_to_string().ok()?).ok()?;
    let first = v.get("nextExpectedRanges")?.as_array()?.first()?.as_str()?.to_string();
    first.split('-').next()?.parse().ok()
}

impl Account {
    /// The address of the item at `path`, by id (on this drive or the one it's shared from).
    fn addr_of(&self, path: &str) -> Result<String> {
        Ok(self.found_addr(path)?.url())
    }
}
