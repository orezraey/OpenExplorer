//! Downloading a file by name; a connection that drops midway is picked up where it stopped with
//! a `Range` request by the version's id (`X-Bz-File-Id` of the first answer), so a file replaced
//! meanwhile can't splice two versions. The bytes are checked against the SHA-1 B2 keeps (small
//! files; a large file has none, unless its upload noted `large_file_sha1`).

use std::io::{Read, Write};
use std::time::Duration;

use openexplorer_i18n::{t, tr};
use sha1::Digest;

use crate::account::{Account, Spot};
use crate::api::{Fail, V, encode, failure, hex};
use crate::{CANCELLED, Progress, Result};

impl Account {
    /// The file at `path` into `out`.
    pub fn download(&self, path: &str, out: &mut dyn Write, progress: Progress) -> Result<()> {
        let s = match Spot::of(path) {
            Some(s) if !s.key.is_empty() => s,
            _ => return Err(t("Isso é um bucket, não um arquivo.", "That's a bucket, not a file.").into()),
        };
        let name = s.parent().1.to_string();
        let mut done = 0u64;
        let mut file_id: Option<String> = None;
        let mut total: Option<u64> = None;
        let mut expected: Option<String> = None;
        let mut hasher = sha1::Sha1::new();
        let mut renewed = 0;
        let mut tries = 0;
        let mut delay = self.api.backoff;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let auth = self.api.auth()?;
            let url = match &file_id {
                Some(id) => format!("{}/{V}/b2_download_file_by_id?fileId={}", auth.download_url, encode(id)),
                None => format!("{}/file/{}/{}", auth.download_url, encode(&s.bucket), encode(&s.key)),
            };
            let mut req = self.api.agent.get(&url).header("Authorization", &auth.token);
            if done > 0 {
                req = req.header("Range", &format!("bytes={done}-"));
            }
            if let Some(m) = &self.api.test_mode {
                req = req.header("X-Bz-Test-Mode", m);
            }
            let why = match req.call() {
                Err(e) => Fail::Network(e.to_string()),
                Ok(mut resp) => {
                    let status = resp.status().as_u16();
                    if status != 200 && status != 206 {
                        failure(&mut resp)
                    } else {
                        let header = |k: &str| resp.headers().get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
                        if file_id.is_none() {
                            file_id = header("X-Bz-File-Id");
                            total = header("Content-Length").and_then(|l| l.parse().ok());
                            let sha1 = header("X-Bz-Content-Sha1").filter(|s| s != "none").or_else(|| header("X-Bz-Info-large_file_sha1"));
                            expected = sha1.map(|s| s.trim_start_matches("unverified:").to_ascii_lowercase());
                        }
                        // A server that ignored the range sends the file from its start.
                        let mut skip = if status == 200 { done } else { 0 };
                        let mut body = resp.body_mut().as_reader();
                        let ended = loop {
                            if total.is_some_and(|t| done >= t) {
                                break None;
                            }
                            match body.read(&mut buf) {
                                Ok(0) => break if total.is_none() { None } else { Some(Fail::Network(t("a conexão terminou antes do fim", "the connection ended early").into())) },
                                Ok(n) => {
                                    let cut = (skip.min(n as u64)) as usize;
                                    skip -= cut as u64;
                                    let chunk = &buf[cut..n];
                                    if chunk.is_empty() {
                                        continue;
                                    }
                                    hasher.update(chunk);
                                    out.write_all(chunk).map_err(|e| tr!("Não consegui gravar o arquivo: {e}", "Couldn't write the file: {e}"))?;
                                    done += chunk.len() as u64;
                                    if !progress(done) {
                                        return Err(CANCELLED.into());
                                    }
                                }
                                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                                Err(e) => break Some(Fail::Network(e.to_string())),
                            }
                        };
                        match ended {
                            None => break,
                            Some(f) => f,
                        }
                    }
                }
            };
            if why.expired() && renewed < 2 {
                renewed += 1;
                self.api.renew(auth.generation)?;
                continue;
            }
            if why.transient() && tries + 1 < self.api.attempts {
                tries += 1;
                crate::api::log(&format!("B2 download de {name} parou em {done} bytes: {} — retomando", why.text()));
                std::thread::sleep(match &why {
                    Fail::Status { retry_after: Some(s), .. } => Duration::from_secs(*s).min(Duration::from_secs(120)),
                    _ => delay,
                });
                delay = (delay * 2).min(Duration::from_secs(32));
                continue;
            }
            return Err(match why.status() {
                404 => tr!("“{name}” não está mais no B2.", "“{name}” is no longer on B2."),
                _ => tr!("O download de “{name}” parou: {}", "The download of “{name}” stopped: {}", why.text()),
            });
        }
        if let Some(want) = expected
            && hex(&hasher.finalize()) != want
        {
            return Err(tr!("“{name}” chegou corrompido (o SHA-1 não confere com o do B2).", "“{name}” arrived corrupted (its SHA-1 doesn't match B2's)."));
        }
        Ok(())
    }
}
