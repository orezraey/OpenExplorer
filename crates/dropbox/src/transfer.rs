//! Downloads and uploads.
//!
//! - A download names the file's revision once it knows it, so a connection dropped midway is
//!   asked again with `Range` from the bytes already written, of the same version: two versions
//!   are never spliced. What arrived is checked against the file's `content_hash`.
//! - An upload up to 150 MiB goes in one request (`files/upload`), streamed; above, in an upload
//!   session, block by block (8 MiB, a multiple of the 4 MiB Dropbox asks for), each block sent
//!   again on a dropped connection, and the offset Dropbox says it holds is believed when the
//!   two disagree. The `content_hash` Dropbox computes is checked against the one computed here.
//! - [`Account::same_content`] tells a file already on Dropbox with the same bytes, so it isn't
//!   sent again.

use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use openexplorer_i18n::{t, tr};
use serde_json::{Value, json};

use crate::account::Account;
use crate::api::{Body, Fail};
use crate::content::Hasher;
use crate::meta::{Change, Entry, api_path, entry, last};
use crate::{CANCELLED, Result};

/// The largest file sent in one request.
pub const SIMPLE_MAX: u64 = 150 << 20;
/// An upload session's block.
pub const BLOCK: usize = 8 << 20;
/// Tries of a download from where it stopped, or of one block.
const TRIES: u32 = 5;

/// Called with the bytes moved so far; `false` stops the transfer with [`CANCELLED`].
pub type Progress<'a> = &'a mut dyn FnMut(u64) -> bool;

/// Counts and hashes what's read through it, and stops when told.
struct Tap<'a> {
    src: &'a mut dyn Read,
    done: u64,
    hash: &'a mut Hasher,
    progress: &'a mut dyn FnMut(u64) -> bool,
    stopped: bool,
}

impl Read for Tap<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.src.read(buf)?;
        self.hash.update(&buf[..n]);
        self.done += n as u64;
        if n > 0 && !(self.progress)(self.done) {
            self.stopped = true;
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, CANCELLED));
        }
        Ok(n)
    }
}

/// Fills `buf` from `src` as far as it goes: how much it read.
fn fill(src: &mut dyn Read, buf: &mut [u8]) -> Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match src.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(tr!("Não consegui ler o arquivo a enviar: {e}", "Couldn't read the file to send: {e}")),
        }
    }
    Ok(got)
}

impl Account {
    /// Writes the file at `path` into `out`.
    pub fn download(&self, path: &str, out: &mut dyn Write, progress: Progress) -> Result<()> {
        let mut done = 0u64;
        let mut hash = Hasher::default();
        // Once known: which version, how big, its hash.
        let mut pinned: Option<(String, u64, String)> = None;
        let mut tries = 0;
        loop {
            let target = match &pinned {
                Some((rev, ..)) => format!("rev:{rev}"),
                None => api_path(path),
            };
            let (meta, resp) = match self.api.download("files/download", &json!({"path": target}), done) {
                Ok(r) => r,
                Err(f) => {
                    let retry = matches!(f, Fail::Other(_)) && tries + 1 < TRIES && done > 0;
                    if !retry {
                        return Err(match f {
                            Fail::Route { .. } => format!("“{}”: {}", last(&api_path(path)), f.text()),
                            f => f.text(),
                        });
                    }
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(500 << tries));
                    continue;
                }
            };
            if pinned.is_none() {
                let rev = meta.get("rev").and_then(Value::as_str).unwrap_or("").to_string();
                let size = meta.get("size").and_then(Value::as_u64).unwrap_or(0);
                let sum = meta.get("content_hash").and_then(Value::as_str).unwrap_or("").to_string();
                pinned = Some((rev, size, sum));
            }
            // A server that ignored the Range would send it all again.
            if done > 0 && resp.status().as_u16() != 206 {
                return Err(t("O Dropbox não retomou o download de onde parou.", "Dropbox didn't resume the download where it stopped.").into());
            }
            let mut body = resp.into_body().into_reader();
            let mut buf = vec![0u8; 256 << 10];
            let failed = loop {
                match body.read(&mut buf) {
                    Ok(0) => break None,
                    Ok(n) => {
                        out.write_all(&buf[..n]).map_err(|e| tr!("Não consegui gravar o arquivo: {e}", "Couldn't write the file: {e}"))?;
                        hash.update(&buf[..n]);
                        done += n as u64;
                        if !progress(done) {
                            return Err(CANCELLED.into());
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => break Some(e),
                }
            };
            let (_, size, sum) = pinned.as_ref().expect("pinned above");
            match failed {
                _ if done == *size || failed.is_none() && *size == 0 => {
                    let got = std::mem::take(&mut hash).finish();
                    if !sum.is_empty() && got != *sum {
                        return Err(tr!("O arquivo “{}” chegou diferente do que está no Dropbox (hash não confere).", "The file “{}” arrived different from what's on Dropbox (hash mismatch).", last(&api_path(path))));
                    }
                    return Ok(());
                }
                None if done > *size => return Err(t("O Dropbox mandou mais bytes do que o arquivo tem.", "Dropbox sent more bytes than the file has.").into()),
                // Cut short: from where it stopped.
                _ if tries + 1 < TRIES => {
                    tries += 1;
                    crate::api::log(&format!("Dropbox download de {path}: parou em {done} bytes, retomando"));
                    std::thread::sleep(Duration::from_millis(500 << tries));
                }
                Some(e) => return Err(tr!("O download parou: {e}", "The download stopped: {e}")),
                None => return Err(t("O download parou antes do fim.", "The download stopped before its end.").into()),
            }
        }
    }

    /// Creates or replaces the file at `path` with the `len` bytes of `src`.
    pub fn upload(&self, path: &str, src: &mut dyn Read, len: u64, progress: Progress) -> Result<Entry> {
        self.upload_limited(path, src, len, SIMPLE_MAX, progress)
    }

    /// [`Account::upload`] with files over `simple_max` bytes sent in an upload session (the
    /// real-account tests send a small file that way).
    pub fn upload_limited(&self, path: &str, src: &mut dyn Read, len: u64, simple_max: u64, progress: Progress) -> Result<Entry> {
        let commit = json!({"path": api_path(path), "mode": "overwrite", "autorename": false, "mute": true, "strict_conflict": false});
        let mut hash = Hasher::default();
        let named = |f: Fail| if matches!(&f, Fail::Other(e) if e == CANCELLED) { CANCELLED.to_string() } else { format!("“{}”: {}", last(&api_path(path)), f.text()) };
        let v = if len <= (BLOCK as u64).min(simple_max) {
            // Small enough to hold: sent again if Dropbox asks to wait (several uploads writing
            // to one account at once get `too_many_write_operations`).
            let mut data = vec![0u8; len as usize];
            let got = fill(src, &mut data)?;
            if got != data.len() || src.read(&mut [0u8; 1]).map_err(|e| e.to_string())? != 0 {
                return Err(tr!("O arquivo mudou de tamanho durante o envio ({got} de {len} bytes).", "The file changed size while being sent ({got} of {len} bytes)."));
            }
            hash.update(&data);
            let mut told = |n: usize| progress(n as u64);
            self.api.upload("files/upload", &commit, &mut Body::Bytes(&data, Some(&mut told))).map_err(named)?
        } else if len <= simple_max {
            let mut tap = Tap { src, done: 0, hash: &mut hash, progress: &mut *progress, stopped: false };
            let sent = self.api.upload("files/upload", &commit, &mut Body::Reader(&mut tap, len));
            if tap.stopped {
                return Err(CANCELLED.into());
            }
            if tap.done != len {
                return Err(tr!("O arquivo mudou de tamanho durante o envio ({} de {len} bytes).", "The file changed size while being sent ({} of {len} bytes).", tap.done));
            }
            sent.map_err(named)?
        } else {
            self.upload_session(src, len, &commit, &mut hash, progress)?
        };
        let e = entry(&v).ok_or(t("O Dropbox respondeu sem os dados do arquivo.", "Dropbox answered without the file's details."))?;
        let sent = hash.finish();
        if !e.hash.is_empty() && e.hash != sent {
            return Err(tr!("O arquivo “{}” chegou diferente ao Dropbox (hash não confere).", "The file “{}” reached Dropbox different (hash mismatch).", e.name));
        }
        self.note(&[Change::Is(e.clone())]);
        Ok(e)
    }

    /// Sends `len` bytes of `src` in blocks and commits them as `commit` says.
    fn upload_session(&self, src: &mut dyn Read, len: u64, commit: &Value, hash: &mut Hasher, progress: Progress) -> Result<Value> {
        let mut block = vec![0u8; BLOCK];
        let mut offset = 0u64;
        let mut session: Option<String> = None;
        loop {
            let n = fill(src, &mut block)?;
            hash.update(&block[..n]);
            let last_block = offset + n as u64 >= len || n == 0;
            let mut sent_from = 0usize;
            let mut tries = 0;
            loop {
                let data = &block[sent_from..n];
                let before = offset + sent_from as u64;
                let mut told = |k: usize| progress(before + k as u64);
                // From memory: sent again when Dropbox asks to wait or the connection drops.
                let mut body = Body::Bytes(data, Some(&mut told));
                let result = match &session {
                    None => self.api.upload("files/upload_session/start", &json!({"close": false}), &mut body),
                    Some(id) => self.api.upload("files/upload_session/append_v2", &json!({"cursor": {"session_id": id, "offset": before}, "close": false}), &mut body),
                };
                match result {
                    Err(Fail::Other(e)) if e == CANCELLED => return Err(CANCELLED.into()),
                    Ok(v) => {
                        if session.is_none() {
                            session = Some(v.get("session_id").and_then(Value::as_str).ok_or(t("O Dropbox não abriu a sessão de envio.", "Dropbox didn't open the upload session."))?.to_string());
                        }
                        break;
                    }
                    // Dropbox holds more (or less) of the block than we thought: from there.
                    Err(Fail::Route { ref error, .. }) if error.pointer("/incorrect_offset/correct_offset").is_some() => {
                        let correct = error.pointer("/incorrect_offset/correct_offset").and_then(Value::as_u64).unwrap_or(0);
                        if correct < offset || correct > offset + n as u64 {
                            return Err(t("O Dropbox perdeu parte do envio; envie de novo.", "Dropbox lost part of the upload; send it again.").into());
                        }
                        sent_from = (correct - offset) as usize;
                        if sent_from == n {
                            break;
                        }
                    }
                    Err(Fail::Other(e)) if tries + 1 < TRIES => {
                        tries += 1;
                        crate::api::log(&format!("Dropbox envio: bloco em {offset} falhou ({e}), de novo"));
                        std::thread::sleep(Duration::from_millis(500 << tries));
                    }
                    Err(f) => return Err(f.text()),
                }
            }
            offset += n as u64;
            if last_block {
                break;
            }
        }
        if offset != len {
            return Err(tr!("O arquivo mudou de tamanho durante o envio ({offset} de {len} bytes).", "The file changed size while being sent ({offset} of {len} bytes)."));
        }
        let id = session.unwrap_or_default();
        self.api.upload("files/upload_session/finish", &json!({"cursor": {"session_id": id, "offset": offset}, "commit": commit}), &mut Body::Bytes(&[], None)).map_err(String::from)
    }

    /// Whether the file at `path` on Dropbox has the same bytes as `local` (same size, same
    /// `content_hash`): then it needn't be sent. Reads `local` only when the sizes agree.
    pub fn same_content(&self, path: &str, local: &Path, stop: &dyn Fn() -> bool) -> Result<bool> {
        let Some(there) = self.stat(path)? else { return Ok(false) };
        let len = std::fs::metadata(local).map(|m| m.len()).unwrap_or(u64::MAX);
        if there.dir || there.size != len || there.hash.is_empty() {
            return Ok(false);
        }
        let here = crate::content::of_file(local, stop).map_err(|e| if e.kind() == std::io::ErrorKind::Interrupted { CANCELLED.to_string() } else { tr!("Não consegui ler {}: {e}", "Couldn't read {}: {e}", local.display()) })?;
        Ok(here == there.hash)
    }

}

#[cfg(test)]
mod tests {
    use crate::fake::Fake;

    fn bytes(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 % 253) as u8).collect()
    }

    #[test]
    fn a_small_file_goes_in_one_request_and_comes_back_whole() {
        let fake = Fake::start();
        let a = fake.account();
        let data = bytes(300_000);
        let mut seen = 0;
        let e = a.upload("/ação/😀.bin", &mut &data[..], data.len() as u64, &mut |n| {
            seen = n;
            true
        }).unwrap();
        assert_eq!((e.size, seen), (300_000, 300_000));
        assert_eq!(fake.content("/ação/😀.bin"), data);
        let mut out = Vec::new();
        a.download("/ação/😀.bin", &mut out, &mut |_| true).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn a_big_file_goes_in_blocks_through_dropped_connections() {
        let fake = Fake::start();
        let a = fake.account();
        // Past the one-request limit, without sending 150 MiB in a test: the limit is a size,
        // so the session is checked on its own with a size that needs three blocks.
        let data = bytes(2 * super::BLOCK + 1234);
        fake.drop_next_append();
        let mut hash = crate::content::Hasher::default();
        let commit = serde_json::json!({"path": "/grande.bin", "mode": "overwrite", "autorename": false});
        let v = a.upload_session(&mut &data[..], data.len() as u64, &commit, &mut hash, &mut |_| true).unwrap();
        assert_eq!(v["size"], data.len() as u64);
        assert_eq!(fake.content("/grande.bin"), data);
        assert_eq!(v["content_hash"].as_str().unwrap(), hash.finish());
    }

    #[test]
    fn a_download_cut_midway_resumes_from_the_same_revision() {
        let fake = Fake::start();
        let data = bytes(1_000_000);
        fake.put_file("/video.mp4", &data);
        fake.cut_next_download(400_000);
        let a = fake.account();
        let mut out = Vec::new();
        a.download("/video.mp4", &mut out, &mut |_| true).unwrap();
        assert_eq!(out, data);
        let ranges = fake.ranges();
        assert_eq!(ranges.len(), 2);
        assert!(ranges[1].0.starts_with("rev:") && ranges[1].1 == 400_000, "{ranges:?}");
    }

    #[test]
    fn a_file_with_the_same_bytes_is_told_apart_from_a_changed_one() {
        let fake = Fake::start();
        let dir = std::env::temp_dir().join(format!("oe-dropbox-same-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let local = dir.join("x.bin");
        let data = bytes(5 << 20);
        std::fs::write(&local, &data).unwrap();
        fake.put_file("/x.bin", &data);
        let a = fake.account();
        assert!(a.same_content("/x.bin", &local, &|| false).unwrap());
        let mut other = data.clone();
        other[10] ^= 1;
        fake.put_file("/x.bin", &other);
        a.reload("/").unwrap();
        assert!(!a.same_content("/x.bin", &local, &|| false).unwrap());
        assert!(!a.same_content("/nada.bin", &local, &|| false).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cancelling_stops_an_upload() {
        let fake = Fake::start();
        let a = fake.account();
        let data = bytes(1_000_000);
        let err = a.upload("/c.bin", &mut &data[..], data.len() as u64, &mut |n| n < 300_000).unwrap_err();
        assert_eq!(err, crate::CANCELLED);
        assert!(!fake.has("/c.bin"));
        let big = bytes(super::BLOCK + 10);
        let err = a.upload_limited("/d.bin", &mut &big[..], big.len() as u64, 0, &mut |n| n < 1_000_000).unwrap_err();
        assert_eq!(err, crate::CANCELLED);
        assert!(!fake.has("/d.bin"));
    }

    #[test]
    fn uploads_wait_as_long_as_dropbox_asks_and_go_again() {
        let fake = Fake::start();
        let a = fake.account();
        a.list("/").unwrap();
        let small = bytes(50_000);
        fake.throttle(2);
        a.upload("/pequeno.bin", &mut &small[..], small.len() as u64, &mut |_| true).unwrap();
        assert_eq!(fake.content("/pequeno.bin"), small);
        let big = bytes(super::BLOCK + 10);
        fake.throttle(1);
        a.upload_limited("/grande.bin", &mut &big[..], big.len() as u64, 0, &mut |_| true).unwrap();
        assert_eq!(fake.content("/grande.bin"), big);
        assert_eq!(fake.throttled(), 3);
    }
}
