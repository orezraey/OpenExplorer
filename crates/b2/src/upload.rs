//! Uploading: one request (`b2_upload_file`) for a file up to a part's size, B2's large file API
//! for bigger ones, its parts sent [`PARTS_AT_ONCE`] at a time.
//!
//! - Each connection keeps its upload URL and sends every file through it until B2 turns it down
//!   (Backblaze asks for one URL per thread, reused); each thread sending parts gets a part URL of
//!   its own the same way. A URL turned down (401, 408, 429, 5xx, no answer) is dropped and the
//!   request sent again through a new one.
//! - A part is read whole into memory, its SHA-1 taken, and sent; the app hands over a stream (a
//!   file, or another server's download), so parts can't be read again from it. The part size is
//!   the `recommendedPartSize` the authorization gives, at most [`PART_CAP`]: B2 recommends
//!   100 MB, and a few uploads at once with several parts each in memory would take gigabytes.
//!   All uploads together hold at most [`BUDGET`] of parts in memory; past it, reading the next
//!   part waits for one to be sent.
//! - A large upload that fails midway (no network, say) stays unfinished on B2: sending the same
//!   file to the same name again finds it (`b2_list_unfinished_large_files`, with the size and
//!   the part size the app notes in its `fileInfo`), and sends only the parts whose SHA-1 B2
//!   doesn't already hold (`b2_list_parts`). Cancelar cancels it on B2 (`b2_cancel_large_file`),
//!   so its parts stop taking storage; so does a new upload to the name with another size.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, LazyLock, Mutex};
use std::time::Duration;

use openexplorer_i18n::{t, tr};
use serde_json::{Value, json};

use crate::account::{Account, Bucket, Spot, Version, each};
use crate::api::{Fail, encode, failure, sha1_hex};
use crate::{CANCELLED, Progress, Result};

/// The most a part takes, whatever B2 recommends: memory, see the module docs.
pub const PART_CAP: u64 = 32 * 1024 * 1024;
/// How many parts of one file are sent at once.
pub const PARTS_AT_ONCE: usize = 4;
/// B2's limit of parts per file.
pub const MAX_PARTS: u64 = 10_000;
/// What all uploads together hold of parts in memory.
pub const BUDGET: u64 = 256 * 1024 * 1024;

/// The part size for a file of `len` bytes: B2's recommendation, at most [`PART_CAP`], at least
/// B2's minimum, and big enough to fit in [`MAX_PARTS`]. A file no bigger goes in one request.
pub fn part_size(len: u64, recommended: u64, minimum: u64) -> u64 {
    recommended.min(PART_CAP).max(minimum).max(1).max(len.div_ceil(MAX_PARTS))
}

struct Budget {
    used: Mutex<u64>,
    freed: Condvar,
}

static MEMORY: Budget = Budget { used: Mutex::new(0), freed: Condvar::new() };

/// Room for `n` bytes of a part, given back when dropped.
pub(crate) struct Held(u64);

pub(crate) fn hold(n: u64) -> Held {
    let mut used = MEMORY.used.lock().unwrap_or_else(|e| e.into_inner());
    // One part bigger than the whole budget still goes, alone.
    while *used > 0 && *used + n > BUDGET {
        used = MEMORY.freed.wait(used).unwrap_or_else(|e| e.into_inner());
    }
    *used += n;
    Held(n)
}

impl Drop for Held {
    fn drop(&mut self) {
        *MEMORY.used.lock().unwrap_or_else(|e| e.into_inner()) -= self.0;
        MEMORY.freed.notify_all();
    }
}

/// The parts B2 holds of an unfinished file: by number, their size and SHA-1.
type Parts = HashMap<u64, (u64, String)>;

/// An unfinished large upload, as the Envios inacabados dialog shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Unfinished {
    pub id: String,
    /// Its name in the bucket.
    pub name: String,
    /// When it was started, milliseconds since 1970.
    pub started: i64,
    /// What B2 holds of it: its parts' bytes, and how many.
    pub sent: u64,
    pub parts: usize,
    /// The whole file's size, for an upload this app started (it notes it); `None` for one
    /// another program started.
    pub size: Option<u64>,
    /// This app is sending it now.
    pub sending: bool,
    /// The file on this computer it sends, when this app started it here and the file is still
    /// there as it was: it can be taken up again.
    pub local: Option<PathBuf>,
}

/// The large uploads this app is sending now, by file id: not cancelled from the dialog, nor
/// taken for another upload's leftover.
static SENDING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

/// Whether this app is sending the large upload `file_id` now.
pub fn sending(file_id: &str) -> bool {
    SENDING.lock().unwrap_or_else(|e| e.into_inner()).contains(file_id)
}

/// Marks a large upload as being sent, until dropped.
pub(crate) struct InFlight(String);

impl InFlight {
    pub(crate) fn new(file_id: &str) -> InFlight {
        SENDING.lock().unwrap_or_else(|e| e.into_inner()).insert(file_id.to_string());
        InFlight(file_id.to_string())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        SENDING.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
    }
}

/// An upload URL and its token.
#[derive(Clone, Debug)]
struct Target {
    url: String,
    token: String,
}

/// One connection's upload URLs, one per bucket, reused for every file it sends.
#[derive(Default)]
pub struct Uploads {
    urls: HashMap<String, Target>,
}

/// The bytes of `data`, reporting how many went (from `base`); fails once `go` says stop.
struct Sending<'a> {
    data: &'a [u8],
    at: usize,
    report: &'a mut dyn FnMut(u64) -> bool,
    stopped: bool,
}

impl Read for Sending<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = buf.len().min(self.data.len() - self.at).min(256 * 1024);
        buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
        self.at += n;
        if n > 0 && !(self.report)(self.at as u64) {
            self.stopped = true;
            return Err(std::io::Error::other(CANCELLED));
        }
        Ok(n)
    }
}

/// What a request to an upload URL came to.
enum Sent {
    Done(Value),
    Stopped,
    /// The URL is no use any more: drop it, and send again through a new one.
    Again(Fail),
    Failed(Fail),
}

fn sent(r: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>, stopped: bool) -> Sent {
    if stopped {
        return Sent::Stopped;
    }
    let mut resp = match r {
        Ok(r) => r,
        Err(e) => return Sent::Again(Fail::Network(e.to_string())),
    };
    if resp.status().as_u16() == 200 {
        return match crate::api::answer(Ok(resp)) {
            Ok(v) => Sent::Done(v),
            Err(f) => Sent::Failed(f),
        };
    }
    let f = failure(&mut resp);
    // Upload URLs: an expired token, a timeout, busy or down: another URL.
    if f.expired() || f.transient() { Sent::Again(f) } else { Sent::Failed(f) }
}

impl Account {
    fn upload_url(&self, bucket: &Bucket) -> Result<Target> {
        let v = self.api.run("b2_get_upload_url", &json!({"bucketId": bucket.id}))?;
        target(&v)
    }

    fn part_url(&self, file_id: &str) -> Result<Target> {
        let v = self.api.run("b2_get_upload_part_url", &json!({"fileId": file_id}))?;
        target(&v)
    }

    /// Sends `data` as the file `name` in `bucket` through `up`'s URL, `report` told the bytes
    /// so far.
    fn send_small(&self, up: &mut Uploads, bucket: &Bucket, name: &str, data: &[u8], report: &mut dyn FnMut(u64) -> bool) -> Result<Version> {
        let sha1 = sha1_hex(data);
        let mut delay = self.api.backoff;
        let mut tries = 0;
        loop {
            let target = match up.urls.get(&bucket.id) {
                Some(t) => t.clone(),
                None => self.upload_url(bucket)?,
            };
            let mut body = Sending { data, at: 0, report: &mut *report, stopped: false };
            let mut req = self
                .api
                .agent
                .post(&target.url)
                .header("Authorization", &target.token)
                .header("X-Bz-File-Name", &encode(name))
                .header("Content-Type", "b2/x-auto")
                .header("Content-Length", &data.len().to_string())
                .header("X-Bz-Content-Sha1", &sha1);
            if let Some(m) = &self.api.test_mode {
                req = req.header("X-Bz-Test-Mode", m);
            }
            let r = req.send(ureq::SendBody::from_reader(&mut body));
            match sent(r, body.stopped) {
                Sent::Done(v) => {
                    up.urls.insert(bucket.id.clone(), target);
                    return Version::of(&v).ok_or_else(|| t("Resposta inesperada do B2 ao enviar.", "Unexpected answer from B2 to an upload.").into());
                }
                Sent::Stopped => {
                    up.urls.remove(&bucket.id);
                    return Err(CANCELLED.into());
                }
                Sent::Again(f) if tries + 1 < self.api.attempts => {
                    up.urls.remove(&bucket.id);
                    crate::api::log(&format!("B2 envio de {name}: {} — outro endereço de envio", f.text()));
                    if !f.expired() {
                        std::thread::sleep(retry_wait(&f, delay));
                        delay = (delay * 2).min(Duration::from_secs(32));
                    }
                    tries += 1;
                    report(0);
                }
                Sent::Again(f) | Sent::Failed(f) => {
                    up.urls.remove(&bucket.id);
                    return Err(f.text());
                }
            }
        }
    }

    /// A small file whose bytes are at hand (a folder's marker).
    pub(crate) fn put_bytes(&self, up: &mut Uploads, bucket: &Bucket, name: &str, data: Vec<u8>) -> Result<Version> {
        self.send_small(up, bucket, name, &data, &mut |_| true)
    }

    /// Creates or replaces the file at `path` with the `len` bytes of `src`.
    pub fn upload(&self, up: &mut Uploads, path: &str, src: &mut dyn Read, len: u64, progress: Progress) -> Result<Version> {
        let s = match Spot::of(path) {
            Some(s) if !s.key.is_empty() => s,
            _ => return Err(t("No B2, arquivos ficam dentro de um bucket: abra um deles.", "On B2, files go inside a bucket: open one of them.").into()),
        };
        crate::account::check_name(s.parent().1)?;
        let bucket = self.bucket(&s.bucket)?;
        let auth = self.api.auth()?;
        let part = part_size(len, auth.recommended_part, auth.minimum_part);
        let result = if len <= part {
            let _held = hold(len);
            let mut data = vec![0u8; len as usize];
            src.read_exact(&mut data).map_err(|e| tr!("Não consegui ler o que seria enviado: {e}", "Couldn't read what was to be sent: {e}"))?;
            self.send_small(up, &bucket, &s.key, &data, progress)
        } else {
            self.upload_large(&bucket, &s.key, src, len, part, progress)
        };
        self.changed(&s, false);
        result
    }

    /// Sends the file `local` on this computer to `path`, as [`Account::upload`] does, noting
    /// which file a large upload sends (see [`crate::notes`]) so the Envios inacabados dialog can
    /// take it up again if it stops midway.
    pub fn upload_file(&self, up: &mut Uploads, path: &str, local: &Path, progress: Progress) -> Result<Version> {
        let mut file = std::fs::File::open(local).map_err(|e| tr!("Não consegui abrir {}: {e}", "Couldn't open {}: {e}", local.display()))?;
        let len = file.metadata().map_err(|e| tr!("Não consegui ler {}: {e}", "Couldn't read {}: {e}", local.display()))?.len();
        let auth = self.api.auth()?;
        let large = len > part_size(len, auth.recommended_part, auth.minimum_part);
        let spot = Spot::of(path).filter(|s| !s.key.is_empty() && large);
        if let Some(s) = &spot {
            crate::notes::begin(&self.key_id, &s.bucket, &s.key, len, local);
        }
        let result = self.upload(up, path, &mut std::io::BufReader::with_capacity(1 << 20, &mut file), len, progress);
        // Ended, or cancelled on B2: nothing to take up again. Failed: the note stays.
        if let Some(s) = &spot
            && result.as_ref().map_or_else(|e| e == CANCELLED, |_| true)
        {
            crate::notes::end(&self.key_id, &s.bucket, &s.key, len);
        }
        result
    }

    /// The unfinished large uploads in the bucket named `bucket`, oldest first, each with what
    /// B2 holds of it (its parts summed: B2 gives an unfinished file no length) and, for one the
    /// app started, its size and the file on this computer it sends, if known.
    pub fn unfinished_uploads(&self, bucket: &str) -> Result<Vec<Unfinished>> {
        let bucket = self.bucket(bucket)?;
        // A key limited to a prefix must ask within it.
        let prefix = self.api.auth()?.allowed.name_prefix.clone().unwrap_or_default();
        let found = self.unfinished(&bucket, &prefix)?;
        let parts = each(&found, |v| self.parts(&v.id))?;
        Ok(found
            .into_iter()
            .zip(parts)
            .map(|(v, parts)| {
                let size = v.info.get("oe_size").and_then(Value::as_str).and_then(|s| s.parse().ok());
                Unfinished {
                    local: size.and_then(|n| crate::notes::local_of(&self.key_id, &bucket.name, &v.name, n)),
                    sending: sending(&v.id),
                    sent: parts.values().map(|p| p.0).sum(),
                    parts: parts.len(),
                    size,
                    started: v.uploaded,
                    id: v.id,
                    name: v.name,
                }
            })
            .collect())
    }

    /// Cancels the unfinished uploads `ids` (their parts are deleted), not one this app is
    /// sending now. How many went.
    pub fn cancel_unfinished(&self, ids: &[String]) -> Result<usize> {
        let ids: Vec<&String> = ids.iter().filter(|id| !sending(id)).collect();
        each(&ids, |id| self.cancel_large(id)).map(|done| done.len())
    }

    /// The unfinished large uploads in `bucket` (under `name_prefix`), oldest first.
    pub fn unfinished(&self, bucket: &Bucket, name_prefix: &str) -> Result<Vec<Version>> {
        let mut out = Vec::new();
        let mut start: Option<String> = None;
        loop {
            let mut body = json!({"bucketId": bucket.id, "namePrefix": name_prefix, "maxFileCount": 100});
            if let Some(s) = &start {
                body["startFileId"] = s.clone().into();
            }
            let v = self.api.run("b2_list_unfinished_large_files", &body)?;
            out.extend(v.get("files").and_then(Value::as_array).into_iter().flatten().filter_map(Version::of));
            match v.get("nextFileId").and_then(Value::as_str) {
                Some(n) => start = Some(n.to_string()),
                None => return Ok(out),
            }
        }
    }

    /// Cancels an unfinished large upload: its parts are deleted.
    pub fn cancel_large(&self, file_id: &str) -> Result<()> {
        match self.api.call("b2_cancel_large_file", &json!({"fileId": file_id})) {
            Ok(_) => Ok(()),
            Err(f) if f.status() == 404 || f.code() == "file_not_present" => Ok(()),
            Err(f) => Err(f.text()),
        }
    }

    /// The parts B2 holds of unfinished file `file_id`: by number, their size and SHA-1.
    fn parts(&self, file_id: &str) -> Result<Parts> {
        let mut out = HashMap::new();
        let mut start: Option<u64> = None;
        loop {
            let mut body = json!({"fileId": file_id, "maxPartCount": 1000});
            if let Some(s) = start {
                body["startPartNumber"] = s.into();
            }
            let v = self.api.run("b2_list_parts", &body)?;
            for p in v.get("parts").and_then(Value::as_array).into_iter().flatten() {
                if let (Some(n), Some(len), Some(sha1)) = (p.get("partNumber").and_then(Value::as_u64), p.get("contentLength").and_then(Value::as_u64), p.get("contentSha1").and_then(Value::as_str)) {
                    out.insert(n, (len, sha1.to_string()));
                }
            }
            match v.get("nextPartNumber").and_then(Value::as_u64) {
                Some(n) => start = Some(n),
                None => return Ok(out),
            }
        }
    }

    /// An unfinished upload of this same file to `name` to pick up, and the parts it has. The
    /// app's other unfinished uploads to `name` (another size) are cancelled.
    fn resumable(&self, bucket: &Bucket, name: &str, len: u64, part: u64) -> Option<(String, Parts)> {
        let found = match self.unfinished(bucket, name) {
            Ok(f) => f,
            Err(e) => {
                crate::api::log(&format!("B2: não consegui ver os envios inacabados de {name}: {e}"));
                return None;
            }
        };
        let ours = |v: &Version, key: &str| v.info.get(key).and_then(Value::as_str).map(str::to_string);
        // Not one being sent now (the same file twice in the queue): it's taken by its own upload.
        let mut mine: Vec<Version> = found.into_iter().filter(|v| v.name == name && ours(v, "oe_size").is_some() && !sending(&v.id)).collect();
        mine.sort_by_key(|v| std::cmp::Reverse(v.uploaded));
        let same = mine.iter().position(|v| ours(v, "oe_size") == Some(len.to_string()) && ours(v, "oe_part") == Some(part.to_string()));
        let pick = same.map(|i| mine.remove(i));
        for other in mine {
            if let Err(e) = self.cancel_large(&other.id) {
                crate::api::log(&format!("B2: não consegui cancelar o envio inacabado {}: {e}", other.id));
            }
        }
        let pick = pick?;
        match self.parts(&pick.id) {
            Ok(parts) => {
                crate::api::log(&format!("B2: retomando o envio de {name}: {} partes já no B2", parts.len()));
                Some((pick.id, parts))
            }
            Err(e) => {
                crate::api::log(&format!("B2: não consegui ler as partes de {}: {e}", pick.id));
                None
            }
        }
    }

    fn start_large(&self, bucket: &Bucket, name: &str, content_type: &str, info: Value) -> Result<String> {
        let v = self.api.run("b2_start_large_file", &json!({"bucketId": bucket.id, "fileName": name, "contentType": content_type, "fileInfo": info}))?;
        v.get("fileId").and_then(Value::as_str).map(str::to_string).ok_or_else(|| t("Resposta inesperada do B2 ao começar um envio grande.", "Unexpected answer from B2 starting a large upload.").into())
    }

    /// Joins the parts of `file_id`. A retry of a finish that went through (its answer lost)
    /// gets a 400: then the file is looked up.
    fn finish_large(&self, file_id: &str, sha1s: &[String]) -> Result<Version> {
        let answer = match self.api.call("b2_finish_large_file", &json!({"fileId": file_id, "partSha1Array": sha1s})) {
            Ok(v) => v,
            Err(f) if f.status() == 400 => match self.api.call("b2_get_file_info", &json!({"fileId": file_id})) {
                Ok(v) if v.get("action").and_then(Value::as_str) == Some("upload") => v,
                _ => return Err(f.text()),
            },
            Err(f) => return Err(f.text()),
        };
        Version::of(&answer).ok_or_else(|| t("Resposta inesperada do B2 ao terminar um envio grande.", "Unexpected answer from B2 finishing a large upload.").into())
    }

    /// Sends part `n` of `file_id` through the thread's part URL `target`, adding the bytes sent
    /// to `sent` as they go (taken back if it fails).
    #[allow(clippy::too_many_arguments)]
    fn send_part(&self, target: &mut Option<Target>, file_id: &str, n: u64, data: &[u8], sha1: &str, sent: &AtomicU64, stop: &AtomicBool) -> Result<()> {
        let mut delay = self.api.backoff;
        let mut tries = 0;
        loop {
            let t = match target.take() {
                Some(t) => t,
                None => self.part_url(file_id)?,
            };
            let mut counted = 0u64;
            let mut report = |at: u64| {
                sent.fetch_add(at - counted, Ordering::Relaxed);
                counted = at;
                !stop.load(Ordering::Relaxed)
            };
            let mut body = Sending { data, at: 0, report: &mut report, stopped: false };
            let mut req = self
                .api
                .agent
                .post(&t.url)
                .header("Authorization", &t.token)
                .header("X-Bz-Part-Number", &n.to_string())
                .header("Content-Length", &data.len().to_string())
                .header("X-Bz-Content-Sha1", sha1);
            if let Some(m) = &self.api.test_mode {
                req = req.header("X-Bz-Test-Mode", m);
            }
            let r = req.send(ureq::SendBody::from_reader(&mut body));
            let stopped = body.stopped;
            let outcome = sent_part(r, stopped);
            if !matches!(outcome, Sent::Done(_)) {
                sent.fetch_sub(counted, Ordering::Relaxed);
            }
            match outcome {
                Sent::Done(_) => {
                    *target = Some(t);
                    return Ok(());
                }
                Sent::Stopped => return Err(CANCELLED.into()),
                Sent::Again(f) if tries + 1 < self.api.attempts => {
                    crate::api::log(&format!("B2 parte {n} de {file_id}: {} — outro endereço de envio", f.text()));
                    if !f.expired() {
                        std::thread::sleep(retry_wait(&f, delay));
                        delay = (delay * 2).min(Duration::from_secs(32));
                    }
                    tries += 1;
                }
                Sent::Again(f) | Sent::Failed(f) => return Err(f.text()),
            }
        }
    }

    /// Sends `src` as a large file: its parts read one after another here, sent by
    /// [`PARTS_AT_ONCE`] threads, each with its own part URL.
    fn upload_large(&self, bucket: &Bucket, name: &str, src: &mut dyn Read, len: u64, part: u64, progress: Progress) -> Result<Version> {
        let count = len.div_ceil(part);
        let (file_id, have) = match self.resumable(bucket, name, len, part) {
            Some(found) => found,
            None => (self.start_large(bucket, name, "b2/x-auto", json!({"oe_size": len.to_string(), "oe_part": part.to_string()}))?, HashMap::new()),
        };
        let _sending = InFlight::new(&file_id);
        let sent = AtomicU64::new(0);
        let stop = AtomicBool::new(false);
        let failed: Mutex<Option<String>> = Mutex::new(None);
        let sha1s: Mutex<Vec<String>> = Mutex::new(vec![String::new(); count as usize]);
        let (tx, rx) = std::sync::mpsc::sync_channel::<(u64, Vec<u8>, String, Held)>(1);
        let rx = Mutex::new(rx);
        // A failure stops the reading; the parts already handed over still go, to be kept for
        // a resume. Cancelar (`stop`) stops those too.
        let fail = |e: String| {
            failed.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(e);
        };
        let failing = || failed.lock().unwrap_or_else(|e| e.into_inner()).is_some();
        let mut cancelled = false;
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..(self.parallel.max(1) as u64).min(count))
                .map(|_| {
                    scope.spawn(|| {
                        let mut target = None;
                        loop {
                            let job = rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
                            let Ok((n, data, sha1, held)) = job else { return };
                            if stop.load(Ordering::SeqCst) {
                                continue;
                            }
                            match self.send_part(&mut target, &file_id, n, &data, &sha1, &sent, &stop) {
                                Ok(()) => sha1s.lock().unwrap_or_else(|e| e.into_inner())[n as usize - 1] = sha1,
                                Err(e) => fail(e),
                            }
                            drop(held);
                        }
                    })
                })
                .collect();
            let mut tell = |stop_now: &AtomicBool, cancelled: &mut bool| {
                if !progress(sent.load(Ordering::Relaxed)) {
                    *cancelled = true;
                    stop_now.store(true, Ordering::SeqCst);
                }
            };
            for n in 1..=count {
                if stop.load(Ordering::SeqCst) || failing() {
                    break;
                }
                let size = part.min(len - (n - 1) * part);
                let held = hold(size);
                let mut data = vec![0u8; size as usize];
                if let Err(e) = src.read_exact(&mut data) {
                    fail(tr!("Não consegui ler o que seria enviado: {e}", "Couldn't read what was to be sent: {e}"));
                    break;
                }
                let sha1 = sha1_hex(&data);
                if have.get(&n) == Some(&(size, sha1.clone())) {
                    sent.fetch_add(size, Ordering::Relaxed);
                    sha1s.lock().unwrap_or_else(|e| e.into_inner())[n as usize - 1] = sha1;
                    tell(&stop, &mut cancelled);
                    continue;
                }
                // Handed to the first thread free, telling the progress meanwhile.
                let mut job = (n, data, sha1, held);
                loop {
                    match tx.try_send(job) {
                        Ok(()) => break,
                        Err(std::sync::mpsc::TrySendError::Full(back)) => {
                            job = back;
                            tell(&stop, &mut cancelled);
                            if stop.load(Ordering::SeqCst) || failing() {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(40));
                        }
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                    }
                }
                tell(&stop, &mut cancelled);
            }
            drop(tx);
            while workers.iter().any(|w| !w.is_finished()) {
                tell(&stop, &mut cancelled);
                std::thread::sleep(Duration::from_millis(40));
            }
        });
        let failure = failed.into_inner().unwrap_or_else(|e| e.into_inner());
        if cancelled || failure.as_deref() == Some(CANCELLED) {
            if let Err(e) = self.cancel_large(&file_id) {
                crate::api::log(&format!("B2: não consegui cancelar o envio {file_id}: {e}"));
            }
            return Err(CANCELLED.into());
        }
        if let Some(e) = failure {
            crate::api::log(&format!("B2: envio grande de {name} parou ({e}); as partes enviadas ficam para retomar"));
            return Err(e);
        }
        let sha1s = sha1s.into_inner().unwrap_or_else(|e| e.into_inner());
        let done = self.finish_large(&file_id, &sha1s)?;
        progress(len);
        Ok(done)
    }

    /// Copies version `v` (5 GB or more) to `name` in `into` in parts, on B2's side.
    pub(crate) fn copy_large(&self, v: &Version, into: &Bucket, name: &str) -> Result<()> {
        let auth = self.api.auth()?;
        let part = auth.recommended_part.max(auth.minimum_part).max(v.size.div_ceil(MAX_PARTS));
        let content_type = v.content_type.clone().unwrap_or_else(|| "b2/x-auto".into());
        let file_id = self.start_large(into, name, &content_type, Value::Object(v.info.clone()))?;
        let ranges: Vec<(u64, u64, u64)> = (0..v.size.div_ceil(part)).map(|i| (i + 1, i * part, ((i + 1) * part).min(v.size) - 1)).collect();
        let copied = each(&ranges, |(n, from, to)| {
            let a = self.api.run("b2_copy_part", &json!({"sourceFileId": v.id, "largeFileId": file_id, "partNumber": n, "range": format!("bytes={from}-{to}")}))?;
            Ok(a.get("contentSha1").and_then(Value::as_str).filter(|s| *s != "none").map(str::to_string))
        });
        let copied = match copied {
            Ok(c) => c,
            Err(e) => {
                let _ = self.cancel_large(&file_id);
                return Err(e);
            }
        };
        // The part's checksums as B2 holds them, when the copies didn't say.
        let sha1s: Vec<String> = if copied.iter().all(Option::is_some) {
            copied.into_iter().flatten().collect()
        } else {
            let parts = self.parts(&file_id)?;
            ranges.iter().map(|(n, ..)| parts.get(n).map(|p| p.1.clone()).filter(|s| s != "none").unwrap_or_default()).collect()
        };
        // Without every part's checksum B2 can't join them: the copy is given up, not guessed.
        if sha1s.iter().any(String::is_empty) {
            let _ = self.cancel_large(&file_id);
            return Err(tr!("O B2 não deu o SHA-1 das partes copiadas de “{}”; copie baixando e enviando.", "B2 didn't give the SHA-1 of the copied parts of “{}”; copy it by downloading and uploading.", name));
        }
        self.finish_large(&file_id, &sha1s).map(|_| ())
    }
}

/// Like [`sent`]; a part URL is told apart the same way.
fn sent_part(r: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>, stopped: bool) -> Sent {
    sent(r, stopped)
}

fn retry_wait(f: &Fail, delay: Duration) -> Duration {
    match f {
        Fail::Status { retry_after: Some(s), .. } => Duration::from_secs(*s).min(Duration::from_secs(120)),
        _ => delay,
    }
}

fn target(v: &Value) -> Result<Target> {
    match (v.get("uploadUrl").and_then(Value::as_str), v.get("authorizationToken").and_then(Value::as_str)) {
        (Some(url), Some(token)) => Ok(Target { url: url.to_string(), token: token.to_string() }),
        _ => Err(t("O B2 não deu um endereço de envio.", "B2 didn't give an upload address.").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_follow_the_recommendation_within_memory_and_the_part_limit() {
        // B2's usual recommendation is capped for memory.
        assert_eq!(part_size(1 << 30, 100_000_000, 5_000_000), PART_CAP);
        // A smaller recommendation stands; never below the minimum.
        assert_eq!(part_size(1 << 30, 10_000_000, 5_000_000), 10_000_000);
        assert_eq!(part_size(1 << 30, 1_000_000, 5_000_000), 5_000_000);
        // A huge file needs bigger parts to fit in 10,000.
        let ten_tb = 10_000_000_000_000u64;
        assert_eq!(part_size(ten_tb, 100_000_000, 5_000_000), 1_000_000_000);
        assert!(ten_tb.div_ceil(part_size(ten_tb, 100_000_000, 5_000_000)) <= MAX_PARTS);
    }

    #[test]
    fn the_memory_budget_waits_for_room() {
        let a = hold(BUDGET - 10);
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let d = done.clone();
        let t = std::thread::spawn(move || {
            let _b = hold(20);
            d.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_millis(100));
        assert!(!done.load(Ordering::SeqCst), "the second waits while the first holds the room");
        drop(a);
        t.join().unwrap();
        assert!(done.load(Ordering::SeqCst));
        // Bigger than the whole budget, alone: it goes.
        drop(hold(BUDGET * 2));
    }
}
