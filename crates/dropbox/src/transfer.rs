//! Downloads and uploads.
//!
//! - A download names the file's revision once it knows it, so a connection dropped midway is
//!   asked again with `Range` from the bytes already written, of the same version: two versions
//!   are never spliced. What arrived is checked against the file's `content_hash`. A big file
//!   (from [`SEGMENT_MIN`], when the caller asks for it) comes in [`SEGMENTS`] parallel `Range`
//!   requests of that revision into one preallocated file, each resuming on its own: in one
//!   stream, 500 MiB took 7.8 to 10.8 s.
//! - A file on this computer up to [`ONE_REQUEST`] goes in one request (`files/upload`), read
//!   from the disk again for each try, its hash computed meanwhile: a session's opening and
//!   commit cost about a second more (100 MiB: 4.7 s against 5.5 s). Bytes from elsewhere up to
//!   8 MiB go in one request too, from memory; above, in a concurrent upload session, in blocks of 8 MiB (a multiple of the 4 MiB Dropbox asks for),
//!   [`SENDERS`] at once, the one that ends the file last and closing the session. Block by
//!   block, 100 MiB took 9.8 s. Every request is held in memory, so one Dropbox answers with 429
//!   (`too_many_write_operations`, frequent with several uploads into one account) is waited
//!   out and sent again, as is a block whose connection dropped.
//!   The `content_hash` Dropbox computes is checked against the one computed here. (Files up to
//!   150 MiB once went in one streamed request, which a 429 midway lost for good.)
//! - Many small files go together (see [`crate::batch`]).
//! - [`Account::same_content`] tells, from a kept listing known current and without a request, a
//!   file already on Dropbox with the same bytes, so it isn't sent again.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use openexplorer_i18n::{t, tr};
use serde_json::{Value, json};

use crate::account::Account;
use crate::api::{Body, Fail};
use crate::content::Hasher;
use crate::meta::{Change, Entry, api_path, entry, last};
use crate::{CANCELLED, Result};

/// An upload session's block, and the largest file sent in one request.
pub const BLOCK: usize = 8 << 20;
/// Tries of a download from where it stopped, or of one block.
const TRIES: u32 = 5;
/// Blocks of one file sent at once.
const SENDERS: usize = 6;
/// A concurrent session's block: a multiple of the 4 MiB Dropbox asks for.
const PIECE: usize = 8 << 20;
/// A file on this computer up to this goes in one request, the most Dropbox takes that way.
pub const ONE_REQUEST: u64 = 150 << 20;
/// Files from this size come in parallel segments, when asked to.
pub const SEGMENT_MIN: u64 = 64 << 20;
/// How many segments a big file comes in.
pub const SEGMENTS: usize = 8;

/// Called with the bytes moved so far; `false` stops the transfer with [`CANCELLED`].
pub type Progress<'a> = &'a mut dyn FnMut(u64) -> bool;

/// Fills `buf` from `src` as far as it goes: how much it read.
fn fill(src: &mut dyn Read, buf: &mut [u8]) -> Result<usize> {
    openexplorer_cloud::fill(src, buf).map_err(|e| tr!("Não consegui ler o arquivo a enviar: {e}", "Couldn't read the file to send: {e}"))
}

/// The offset an `incorrect_offset` error says Dropbox holds. A struct member of a union sits
/// beside its tag (`{".tag": "incorrect_offset", "correct_offset": 8}`, Stone's JSON rules); the
/// nested form is read too.
pub(crate) fn correct_offset(error: &Value) -> Option<u64> {
    if error.get(".tag").and_then(Value::as_str) != Some("incorrect_offset") {
        return None;
    }
    error.get("correct_offset").or_else(|| error.pointer("/incorrect_offset/correct_offset")).and_then(Value::as_u64)
}

/// Whether metadata `m` is of a file of `len` bytes whose content hash is `hash`.
pub(crate) fn holds(m: &Value, len: u64, hash: &str) -> bool {
    entry(m).is_some_and(|e| !e.dir && e.size == len && e.hash == hash)
}

/// Writes `buf` at byte `at` of `file`, whatever else writes elsewhere in it meanwhile.
fn write_at(file: &std::fs::File, buf: &[u8], at: u64) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            done += file.seek_write(&buf[done..], at + done as u64)?;
        }
        Ok(())
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::write_all_at(file, buf, at)
    }
}

impl Account {
    /// Writes the file at `path` into `file`, a new one: with `segmented`, a file from
    /// [`SEGMENT_MIN`] in [`SEGMENTS`] parallel pieces, else as [`Account::download`] does.
    pub fn download_file(&self, path: &str, file: &std::fs::File, segmented: bool, progress: Progress) -> Result<()> {
        if segmented {
            // Its revision, size and hash: from the kept listing when it's current, else asked.
            let known = match self.known(path) {
                Some(Some(e)) => Some(e),
                _ => self.metadata(path).map_err(|f| format!("“{}”: {}", last(&api_path(path)), f.text()))?.and_then(|v| entry(&v)),
            };
            if let Some(e) = known.filter(|e| !e.dir && e.size >= SEGMENT_MIN && !e.rev.is_empty()) {
                return self.download_split(&e, file, SEGMENTS, progress);
            }
        }
        let mut out = file;
        self.download(path, &mut out, progress)
    }

    /// Revision `e.rev` into `file` in `pieces` parallel `Range` requests, each asked again
    /// from where it stopped when its connection drops, checked whole against `e.hash`.
    pub(crate) fn download_split(&self, e: &Entry, file: &std::fs::File, pieces: usize, progress: Progress) -> Result<()> {
        let size = e.size;
        file.set_len(size).map_err(|err| tr!("Sem espaço para o arquivo? {err}", "No room for the file? {err}"))?;
        // Whole blocks of the content hash each, so each segment hashes its own.
        let block = crate::content::BLOCK as u64;
        let step = size.div_ceil(pieces as u64).div_ceil(block).max(1) * block;
        let ranges: Vec<(u64, u64)> = (0..pieces as u64).map(|i| (i * step, ((i + 1) * step).min(size))).filter(|(a, b)| a < b).collect();
        let total = AtomicU64::new(0);
        let digests: Vec<Mutex<Vec<[u8; 32]>>> = ranges.iter().map(|_| Mutex::new(Vec::new())).collect();
        let failed: Mutex<Option<String>> = Mutex::new(None);
        let stopped = || failed.lock().unwrap_or_else(PoisonError::into_inner).is_some();
        let fail = |err: String| {
            failed.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert(err);
        };
        let target = json!({"path": format!("rev:{}", e.rev)});
        std::thread::scope(|scope| {
            let runs: Vec<_> = ranges
                .iter()
                .zip(&digests)
                .map(|(&(from, until), digests)| {
                    let (total, target) = (&total, &target);
                    scope.spawn(move || match self.segment(target, file, from, until, &|n| {
                        total.fetch_add(n, Ordering::Relaxed);
                        !stopped()
                    }) {
                        Ok(d) => *digests.lock().unwrap_or_else(PoisonError::into_inner) = d,
                        Err(err) => fail(err),
                    })
                })
                .collect();
            while !runs.iter().all(|r| r.is_finished()) {
                if !progress(total.load(Ordering::Relaxed)) {
                    fail(CANCELLED.into());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        if let Some(err) = failed.into_inner().unwrap_or_else(PoisonError::into_inner) {
            return Err(err);
        }
        if !progress(size) {
            return Err(CANCELLED.into());
        }
        let hash = crate::content::joined(digests.into_iter().flat_map(|d| d.into_inner().unwrap_or_else(PoisonError::into_inner)));
        if !e.hash.is_empty() && hash != e.hash {
            return Err(tr!("O arquivo “{}” chegou diferente do que está no Dropbox (hash não confere).", "The file “{}” arrived different from what's on Dropbox (hash mismatch).", e.name));
        }
        Ok(())
    }

    /// Bytes `from..until` of the revision `target` names into the same place in `file`; asked
    /// again from where it stopped when the connection drops. `moved` hears each batch written
    /// (and 0 while waiting); `false` stops it. The content hash's digests of its blocks.
    fn segment(&self, target: &Value, file: &std::fs::File, from: u64, until: u64, moved: &dyn Fn(u64) -> bool) -> Result<Vec<[u8; 32]>> {
        let mut hash = crate::content::Blocks::default();
        let mut at = from;
        let mut tries = 0;
        let mut buf = vec![0u8; 256 << 10];
        while at < until {
            if !moved(0) {
                return Err(CANCELLED.into());
            }
            let before = at;
            let why = match self.api.download("files/download", target, at) {
                Err(Fail::Other(err)) if err == CANCELLED => return Err(CANCELLED.into()),
                Err(f @ Fail::Route { .. }) => return Err(f.text()),
                Err(f) => f.text(),
                // A server that ignored the Range would send it all again.
                Ok((_, resp)) if at > 0 && resp.status().as_u16() != 206 => return Err(t("O Dropbox não mandou só a parte pedida do arquivo.", "Dropbox didn't send only the asked part of the file.").into()),
                Ok((_, resp)) => {
                    let mut body = resp.into_body().into_reader();
                    loop {
                        let want = buf.len().min((until - at) as usize);
                        if want == 0 {
                            break String::new();
                        }
                        match body.read(&mut buf[..want]) {
                            Ok(0) => break t("a conexão terminou antes do fim", "the connection ended early").to_string(),
                            Ok(n) => {
                                write_at(file, &buf[..n], at).map_err(|err| tr!("Não consegui gravar o arquivo: {err}", "Couldn't write the file: {err}"))?;
                                hash.update(&buf[..n]);
                                at += n as u64;
                                if !moved(n as u64) {
                                    return Err(CANCELLED.into());
                                }
                            }
                            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(err) => break err.to_string(),
                        }
                    }
                }
            };
            if at >= until {
                break;
            }
            // Only drops that get nowhere add up to giving up.
            if at > before {
                tries = 0;
            }
            tries += 1;
            if tries >= TRIES {
                return Err(tr!("O download parou: {why}", "The download stopped: {why}"));
            }
            crate::api::log(&format!("Dropbox download: segmento parou em {at} ({why}), retomando"));
            std::thread::sleep(Duration::from_millis(500 << tries));
        }
        Ok(hash.finish())
    }

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
                    let retry = matches!(f, Fail::Other(_) | Fail::Lost(_)) && tries + 1 < TRIES && done > 0;
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
    /// Sends the file `local` on this computer to `path`: up to [`ONE_REQUEST`] in one request,
    /// above as [`Account::upload`] does.
    pub fn upload_file(&self, path: &str, local: &Path, progress: Progress) -> Result<Entry> {
        let file = std::fs::File::open(local).map_err(|e| tr!("Não consegui abrir {}: {e}", "Couldn't open {}: {e}", local.display()))?;
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if len > ONE_REQUEST {
            return self.upload(path, &mut &file, len, progress);
        }
        let commit = json!({"path": api_path(path), "mode": "overwrite", "autorename": false, "mute": true, "strict_conflict": false});
        // The hash stops when the request fails or is cancelled, so Cancelar doesn't wait for it.
        let failed = std::sync::atomic::AtomicBool::new(false);
        let (sent, hash) = std::thread::scope(|scope| {
            let hashing = scope.spawn(|| crate::content::of_file(local, &|| failed.load(Ordering::Relaxed)));
            let mut told = |n: usize| progress(n as u64);
            let sent = self.api.upload("files/upload", &commit, &mut Body::File(&file, len, Some(&mut told)));
            failed.store(sent.is_err(), Ordering::Relaxed);
            (sent, hashing.join())
        });
        let v = sent.map_err(|f| if matches!(&f, Fail::Other(e) if e == CANCELLED) { CANCELLED.to_string() } else { format!("“{}”: {}", last(&api_path(path)), f.text()) })?;
        let e = entry(&v).ok_or(t("O Dropbox respondeu sem os dados do arquivo.", "Dropbox answered without the file's details."))?;
        if e.size != len {
            return Err(tr!("O arquivo mudou de tamanho durante o envio ({} de {len} bytes).", "The file changed size while being sent ({} of {len} bytes).", e.size));
        }
        if let Ok(Ok(here)) = hash
            && !e.hash.is_empty()
            && e.hash != here
        {
            return Err(tr!("O arquivo “{}” chegou diferente ao Dropbox (hash não confere).", "The file “{}” reached Dropbox different (hash mismatch).", e.name));
        }
        self.note(&[Change::Is(e.clone())]);
        Ok(e)
    }

    pub fn upload(&self, path: &str, src: &mut dyn Read, len: u64, progress: Progress) -> Result<Entry> {
        self.upload_limited(path, src, len, BLOCK as u64, progress)
    }

    /// [`Account::upload`] with files over `simple_max` bytes (at most [`BLOCK`]) sent in an
    /// upload session (the real-account tests send a small file that way).
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

    /// Sends `len` bytes of `src` in blocks, [`SENDERS`] at once, and commits them as `commit`
    /// says. The block that ends the file goes last, closing the session.
    fn upload_session(&self, src: &mut dyn Read, len: u64, commit: &Value, hash: &mut Hasher, progress: Progress) -> Result<Value> {
        let started = self.api.upload("files/upload_session/start", &json!({"close": false, "session_type": "concurrent"}), &mut Body::Bytes(&[], None));
        let id = match started {
            Err(Fail::Other(e)) if e == CANCELLED => return Err(CANCELLED.into()),
            Err(f) => return Err(f.text()),
            Ok(v) => v.get("session_id").and_then(Value::as_str).ok_or(t("O Dropbox não abriu a sessão de envio.", "Dropbox didn't open the upload session."))?.to_string(),
        };
        let failed: Mutex<Option<String>> = Mutex::new(None);
        let fail = |e: String| {
            failed.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert(e);
        };
        let stopped = || failed.lock().unwrap_or_else(PoisonError::into_inner).is_some();
        let sent = AtomicU64::new(0);
        let send = |offset: u64, data: &[u8], close: bool| {
            match self.append(&id, offset, data, close, &|| !stopped()) {
                Ok(()) => {
                    sent.fetch_add(data.len() as u64, Ordering::Relaxed);
                }
                Err(e) => fail(e),
            }
        };
        let (tx, rx) = sync_channel::<(u64, Vec<u8>)>(2);
        let rx = Mutex::new(rx);
        let mut offset = 0u64;
        // The block that ends the file, sent once the others arrived.
        let mut last: Option<(u64, Vec<u8>)> = None;
        std::thread::scope(|scope| {
            let senders: Vec<_> = (0..SENDERS)
                .map(|_| {
                    scope.spawn(|| {
                        loop {
                            // Its own statement, so the lock goes before the block is sent: held
                            // through a `while let`, it let one sender work at a time.
                            let Ok((at, block)) = rx.lock().unwrap_or_else(PoisonError::into_inner).recv() else { break };
                            if !stopped() {
                                send(at, &block, false);
                            }
                        }
                    })
                })
                .collect();
            loop {
                let mut block = vec![0u8; PIECE];
                let n = match fill(src, &mut block) {
                    Ok(n) => n,
                    Err(e) => {
                        fail(e);
                        break;
                    }
                };
                block.truncate(n);
                hash.update(&block);
                let at = offset;
                offset += n as u64;
                if offset >= len || n < PIECE {
                    last = Some((at, block));
                    break;
                }
                if tx.send((at, block)).is_err() || stopped() {
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
        if let Some((at, block)) = last.filter(|_| !stopped()) {
            send(at, &block, true);
        }
        if let Some(e) = failed.into_inner().unwrap_or_else(PoisonError::into_inner) {
            return Err(e);
        }
        if offset != len {
            return Err(tr!("O arquivo mudou de tamanho durante o envio ({offset} de {len} bytes).", "The file changed size while being sent ({offset} of {len} bytes)."));
        }
        if !progress(len) {
            return Err(CANCELLED.into());
        }
        self.finish(&id, offset, commit, &hash.clone().finish()).map_err(String::from)
    }

    /// One block of a concurrent session at `offset`, sent again when the connection drops
    /// (Dropbox's 429s are waited out below). Stops when `go` turns false.
    fn append(&self, id: &str, offset: u64, data: &[u8], close: bool, go: &dyn Fn() -> bool) -> Result<()> {
        let mut tries = 0;
        loop {
            let mut told = |_: usize| go();
            let result = self.api.upload("files/upload_session/append_v2", &json!({"cursor": {"session_id": id, "offset": offset}, "close": close}), &mut Body::Bytes(data, Some(&mut told)));
            match result {
                Ok(_) => return Ok(()),
                Err(Fail::Other(e)) if e == CANCELLED => return Err(CANCELLED.into()),
                // Sent again after an answer that never came: Dropbox may hold it already, and
                // says so its way. The content hash checked at the end tells if it doesn't.
                Err(Fail::Route { .. }) if tries > 0 => return Ok(()),
                Err(Fail::Lost(e) | Fail::Other(e)) if tries + 1 < TRIES => {
                    tries += 1;
                    crate::api::log(&format!("Dropbox envio: bloco em {offset} falhou ({e}), de novo"));
                    std::thread::sleep(Duration::from_millis(500 << tries));
                }
                Err(f) => return Err(f.text()),
            }
        }
    }

    /// Commits upload session `id`, `len` bytes long, as `commit` says. Its answer lost, the
    /// commit is done when the file at the commit's path has these bytes (`len` and `hash`, the
    /// content hash); else the session, not taken yet, is committed again.
    pub(crate) fn finish(&self, id: &str, len: u64, commit: &Value, hash: &str) -> std::result::Result<Value, Fail> {
        let path = commit.get("path").and_then(Value::as_str).unwrap_or("").to_string();
        let arg = json!({"cursor": {"session_id": id, "offset": len}, "commit": commit});
        self.write(
            "upload_session/finish",
            || self.api.upload("files/upload_session/finish", &arg, &mut Body::Bytes(&[], None)),
            || Ok(self.metadata(&path)?.filter(|m| holds(m, len, hash))),
        )
    }

    /// Whether the file at `path` on Dropbox has the same bytes as `local` (same size, same
    /// `content_hash`): then it needn't be sent. Answered from the kept listing of its folder
    /// alone, and only while that's known current (a watcher waiting, the log just read): no
    /// request, ever. A file sent into a new folder or under a free name (the panes' usual case)
    /// would only pay a `get_metadata` to hear "not there"; the case it saves, a file sent again
    /// over the same bytes, is one the pane showing that folder has just listed. Reads `local`
    /// only when the sizes agree.
    pub fn same_content(&self, path: &str, local: &Path, stop: &dyn Fn() -> bool) -> Result<bool> {
        let Some(Some(there)) = self.known(path) else { return Ok(false) };
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
        // Three blocks, the session checked on its own.
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
    fn a_big_file_comes_in_segments_of_one_revision_that_each_resume() {
        let fake = Fake::start();
        let data = bytes(3 * crate::content::BLOCK + 3);
        fake.put_file("/grande.iso", &data);
        let a = fake.account();
        let e = a.stat("/grande.iso").unwrap().unwrap();
        fake.cut_next_download(100_000);
        let dir = std::env::temp_dir().join(format!("oe-dropbox-split-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let local = dir.join("grande.iso");
        let file = std::fs::File::create(&local).unwrap();
        let mut last = 0;
        a.download_split(&e, &file, 4, &mut |n| {
            assert!(n >= last, "the sum never goes back");
            last = n;
            true
        })
        .unwrap();
        drop(file);
        assert_eq!(std::fs::read(&local).unwrap(), data);
        let ranges = fake.ranges();
        assert!(ranges.len() >= 5 && ranges.iter().all(|(target, _)| target.starts_with("rev:")), "four segments and one resumed, of one revision: {ranges:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_on_disk_goes_in_one_request_read_again_after_a_drop() {
        let fake = Fake::start();
        let a = fake.account();
        let dir = std::env::temp_dir().join(format!("oe-dropbox-one-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let local = dir.join("medio.bin");
        let data = bytes(3 * super::BLOCK + 5);
        std::fs::write(&local, &data).unwrap();
        let before = fake.calls();
        let mut last = 0;
        let e = a
            .upload_file("/medio.bin", &local, &mut |n| {
                last = n;
                true
            })
            .unwrap();
        assert_eq!((e.size, last), (data.len() as u64, data.len() as u64));
        assert_eq!(fake.content("/medio.bin"), data);
        assert_eq!(fake.calls_since(&before), ["files/upload"]);
        let _ = std::fs::remove_dir_all(dir);
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
        a.list("/").unwrap();
        a.catch_up().unwrap();
        let before = fake.calls();
        assert!(a.same_content("/x.bin", &local, &|| false).unwrap());
        assert!(fake.calls_since(&before).is_empty(), "answered from the kept listing");
        let mut other = data.clone();
        other[10] ^= 1;
        fake.put_file("/x.bin", &other);
        a.catch_up().unwrap();
        assert!(!a.same_content("/x.bin", &local, &|| false).unwrap());
        assert!(!a.same_content("/nada.bin", &local, &|| false).unwrap());
        // Not known current: sent, without asking first.
        fake.put_file("/x.bin", &data);
        a.forget_fresh();
        let before = fake.calls();
        assert!(!a.same_content("/x.bin", &local, &|| false).unwrap());
        assert!(fake.calls_since(&before).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_commit_whose_answer_was_lost_is_checked_by_its_bytes() {
        let fake = Fake::start();
        let a = fake.account();
        let data = bytes(super::BLOCK + 10);
        // Committed, the answer lost: the file there has these bytes, so it's done.
        fake.lose_answer("files/upload_session/finish");
        let before = fake.calls();
        let e = a.upload("/um.bin", &mut &data[..], data.len() as u64, &mut |_| true).unwrap();
        assert_eq!(e.size, data.len() as u64);
        assert_eq!(fake.count_since(&before, "files/upload_session/finish"), 1);
        assert_eq!(fake.content("/um.bin"), data);
        // Over a file that was there: the old bytes don't count as committed.
        fake.put_file("/dois.bin", b"antigo");
        fake.drop_before("files/upload_session/finish");
        let before = fake.calls();
        a.upload("/dois.bin", &mut &data[..], data.len() as u64, &mut |_| true).unwrap();
        assert_eq!(fake.count_since(&before, "files/upload_session/finish"), 2);
        assert_eq!(fake.content("/dois.bin"), data);
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
    fn a_file_over_a_block_survives_a_429_midway() {
        // Between 8 and 150 MiB: once one streamed request, which a 429 lost for good; now
        // blocks held in memory, the block Dropbox asked to wait for sent again.
        let fake = Fake::start();
        let a = fake.account();
        let data = bytes(2 * super::BLOCK + 77);
        fake.throttle_route("files/upload_session/append_v2");
        let mut last = 0;
        let e = a.upload("/medio.bin", &mut &data[..], data.len() as u64, &mut |n| {
            last = n;
            true
        }).unwrap();
        assert_eq!((e.size, last), (data.len() as u64, data.len() as u64));
        assert_eq!(fake.content("/medio.bin"), data);
        assert_eq!(fake.throttled(), 1);
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
