//! Downloading a file: MEGA hands out a URL (`g`) to the encrypted bytes, decrypted here with
//! AES-CTR as they arrive and checked against the file's MAC at the end. A connection that drops
//! midway is picked up from where it stopped.

use std::io::{Read, Write};
use std::time::Duration;

use openexplorer_i18n::{t, tr};
use serde_json::{Value, json};

use crate::account::Account;
use crate::crypto::{Ctr, Mac, split_file_key};
use crate::nodes::{Kind, Node};
use crate::{CANCELLED, Result};

/// Called with the bytes so far; `false` stops the transfer with [`CANCELLED`].
pub type Progress<'a> = &'a mut dyn FnMut(u64) -> bool;

/// How many times a dropped connection is picked up again.
const RESUMES: u32 = 3;

impl Account {
    /// The file at `path` into `out`.
    pub fn download(&self, path: &str, out: &mut dyn Write, progress: Progress) -> Result<()> {
        let n = self.node(path)?;
        self.download_node(&n, out, progress)
    }

    pub fn download_node(&self, n: &Node, out: &mut dyn Write, progress: Progress) -> Result<()> {
        if n.kind != Kind::File || n.key.len() < 32 {
            return Err(tr!("“{}” é uma pasta, não um arquivo.", "“{}” is a folder, not a file.", n.name));
        }
        let (key, nonce, mac) = split_file_key(&n.key);
        let answer = self.api.run(json!({"a": "g", "g": 1, "ssl": 2, "n": n.handle}))?;
        if let Some(code) = answer.get("e").and_then(Value::as_i64).filter(|c| *c < 0) {
            return Err(crate::api::error_text(code));
        }
        let url = answer.get("g").and_then(Value::as_str).ok_or(t("O MEGA não deu o endereço do arquivo.", "MEGA didn't give the file's address."))?.to_string();
        let size = answer.get("s").and_then(Value::as_u64).unwrap_or(n.size);
        let agent = crate::api::transfers();
        let mut check = Mac::new(&key, &nonce);
        let mut done = 0u64;
        let mut resumes = 0;
        let mut buf = vec![0u8; 256 * 1024];
        while done < size {
            let ranged = if done == 0 { url.clone() } else { format!("{url}/{done}-{}", size - 1) };
            let why = match agent.get(&ranged).call() {
                // A ranged request may answer 206 Partial Content.
                Ok(mut resp) if matches!(resp.status().as_u16(), 200 | 206) => {
                    let mut ctr = Ctr::new(&key, &nonce, done);
                    let mut body = resp.body_mut().as_reader();
                    loop {
                        if done == size {
                            break None;
                        }
                        let want = buf.len().min((size - done) as usize);
                        let got = match body.read(&mut buf[..want]) {
                            Ok(0) => break Some(t("a conexão terminou antes do fim", "the connection ended early").to_string()),
                            Ok(k) => k,
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(e) => break Some(e.to_string()),
                        };
                        let chunk = &mut buf[..got];
                        ctr.apply(chunk);
                        check.update(chunk);
                        out.write_all(chunk).map_err(|e| tr!("Não consegui gravar o arquivo: {e}", "Couldn't write the file: {e}"))?;
                        done += got as u64;
                        if !progress(done) {
                            return Err(CANCELLED.into());
                        }
                    }
                }
                Ok(resp) => return Err(tr!("O MEGA respondeu HTTP {} ao baixar “{}”.", "MEGA answered HTTP {} downloading “{}”.", resp.status().as_u16(), n.name)),
                Err(e) => Some(e.to_string()),
            };
            if let Some(why) = why {
                if resumes >= RESUMES {
                    return Err(tr!("O download de “{}” parou: {why}", "The download of “{}” stopped: {why}", n.name));
                }
                resumes += 1;
                crate::api::log(&format!("MEGA download {} parou em {done} de {size}: {why}; retomando", n.handle));
                std::thread::sleep(Duration::from_secs(2));
            }
        }
        // An empty file has nothing to check, and clients disagree on its MAC.
        if size > 0 && check.finish() != mac {
            return Err(tr!("“{}” chegou corrompido (a verificação do MEGA não confere).", "“{}” arrived corrupted (MEGA's check doesn't match).", n.name));
        }
        Ok(())
    }
}
