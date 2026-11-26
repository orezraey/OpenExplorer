//! Fetching a Drive file to the computer.
//!
//! - **Files with content** come from `files.get?alt=media`, streamed into a `.openexplorer-part`
//!   file next to the target, which takes the target's name only once all of it arrived. When
//!   the connection drops or Google answers 5xx, 429 or a rate limit, the request goes again
//!   after a backoff with an HTTP `Range` from the bytes already written, so nothing is fetched
//!   twice.
//! - **Big files** (from [`SEGMENT_MIN`], when the caller asks for it) come in [`SEGMENTS`]
//!   parallel `Range` requests into one preallocated file, each segment resuming on its own.
//!   A segment done with its part takes over the rest of the one that would finish last, on a
//!   new connection, so a slow or stalled connection doesn't hold the end of the download.
//! - **Google's own formats** (Docs, Sheets, Slides, Drawings) hold no bytes to fetch: they're
//!   converted by `files.export` to the format [`export_format`] picks, up to Google's 10 MB.
//!
//! Either way the copy keeps the file's modified time, and `got` hears how many bytes have
//! arrived (and stops the download by answering `false`).
//! <https://developers.google.com/workspace/drive/api/guides/manage-downloads>

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use openexplorer_i18n::tr;

use crate::api::{Drive, DriveFile, FOLDER_MIME};
use crate::{CANCELLED, Result};

/// Files from this size are fetched in parallel segments, when asked to.
pub const SEGMENT_MIN: u64 = 64 * 1024 * 1024;

/// How many segments a big file is fetched in. Measured on a 500 MiB file (example
/// download_speed, median of 3): 1 segment 10.4 s, 4 segments 6.1 s, 8 segments 5.8 s.
pub const SEGMENTS: usize = 8;

/// How segments take over each other's rest: `ask` is how long a new request waits for its
/// first byte (about 0.6 s from Brazil to Google), `after` how long a request must have been
/// receiving before its speed is trusted, and `min` the least worth a request of its own.
/// A segment's answer that brings no byte for `stall` (three times that for its first) is left
/// behind and asked again from where it got to: Google's answer stalled now and then for the
/// whole 120 s a read may wait, and asking again costs under a second. From half of that without
/// a byte, the others take over its rest.
#[derive(Clone, Copy)]
struct Split {
    ask: f64,
    after: f64,
    min: u64,
    stall: Duration,
}

const SPLIT: Split = Split { ask: 0.7, after: 0.5, min: 256 * 1024, stall: Duration::from_secs(2) };

/// A segment: where it is and where it ends (`None`: the stream's end), and its current
/// request: when it went out, when its first and its last byte came, from where. Only its own
/// fetch moves `at`; a segment done with its part may pull `until` back to take over the rest.
struct Seg {
    at: u64,
    until: Option<u64>,
    asked: Instant,
    first: Option<Instant>,
    last: Instant,
    from: u64,
}

impl Seg {
    fn new(at: u64, until: Option<u64>) -> Self {
        let now = Instant::now();
        Seg { at, until, asked: now, first: None, last: now, from: at }
    }

    fn left(&self) -> u64 {
        self.until.map_or(0, |u| u - self.at)
    }

    /// Bytes a second since its first byte came, once that's long enough to tell; 0 when no
    /// byte came for a while (a stalled answer).
    fn speed(&self, split: Split) -> Option<f64> {
        if self.first.is_some() && self.last.elapsed() >= split.stall / 2 {
            return Some(0.0);
        }
        match self.first {
            Some(t) if t.elapsed().as_secs_f64() >= split.after => Some((self.at - self.from) as f64 / t.elapsed().as_secs_f64()),
            Some(_) => None,
            None if self.asked.elapsed().as_secs_f64() >= split.ask * 3.0 => Some(0.0),
            None => None,
        }
    }
}

type Span = Mutex<Seg>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a segment done with its part does next.
#[derive(PartialEq, Debug)]
enum Next {
    /// It took over part of another's rest, now its own span.
    Took,
    /// Others still run, none worth splitting yet.
    Wait,
    /// Every segment is done.
    Done,
}

/// Gives segment `idle`, done with its part, the end of the segment that would finish last at
/// its speed: as much as makes both end together, counting the new request's wait. A slow
/// segment keeps only what it writes meanwhile; a stalled one keeps nothing.
fn take_over(spans: &[Span], idle: usize, split: Split) -> Next {
    let (mut last, mut fastest, mut busy) = (None::<(usize, f64)>, 0f64, false);
    for (j, span) in spans.iter().enumerate().filter(|&(j, _)| j != idle) {
        let s = lock(span);
        if s.left() == 0 {
            continue;
        }
        busy = true;
        let Some(v) = s.speed(split) else { continue };
        fastest = fastest.max(v);
        let eta = s.left() as f64 / v.max(1.0);
        if last.is_none_or(|(_, e)| eta > e) {
            last = Some((j, eta));
        }
    }
    if !busy {
        return Next::Done;
    }
    let Some((j, _)) = last else { return Next::Wait };
    let (mid, end) = {
        let mut s = lock(&spans[j]);
        let (Some(rv), Some(end)) = (s.speed(split), s.until) else { return Next::Wait };
        // The slow one keeps x, the taker the rest: x / rv = ask + (rest - x) / rt.
        let rest = s.left() as f64;
        let rt = fastest.max(rv).max(1.0);
        let x = (rv * (split.ask * rt + rest) / (rt + rv)).min(rest) as u64;
        if s.left() - x < split.min {
            return Next::Wait;
        }
        let mid = s.at + x;
        s.until = Some(mid);
        (mid, end)
    };
    *lock(&spans[idle]) = Seg::new(mid, Some(end));
    Next::Took
}

/// Times a failed request is tried again in a row without progress (with 1, 2, 4, 8, 16, 32 s
/// between), for dropped connections, 5xx, 429 and Drive's rate limits.
const RETRIES: u32 = 6;

/// What Drive calls its own formats: they have no content of their own, only exports.
const GOOGLE: &str = "application/vnd.google-apps.";
use crate::api::SHORTCUT_MIME as SHORTCUT;

/// The format a file in one of Google's own formats is exported as, (MIME type, extension):
/// the Office format for Docs, Sheets and Slides, PDF for Drawings. `None` for what can't be
/// exported (Forms, Sites, My Maps) and for ordinary files.
pub fn export_format(mime: &str) -> Option<(&'static str, &'static str)> {
    Some(match mime.strip_prefix(GOOGLE)? {
        "document" => ("application/vnd.openxmlformats-officedocument.wordprocessingml.document", "docx"),
        "spreadsheet" => ("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", "xlsx"),
        "presentation" => ("application/vnd.openxmlformats-officedocument.presentationml.presentation", "pptx"),
        "drawing" | "jam" => ("application/pdf", "pdf"),
        "script" => ("application/vnd.google-apps.script+json", "json"),
        _ => return None,
    })
}

/// The name a Drive file gets on the computer: an export gains its format's extension
/// ("Relatório" becomes "Relatório.docx"), unless the name already ends in it.
pub fn local_name(file: &DriveFile) -> String {
    match export_format(&file.mime) {
        Some((_, ext)) if !file.name.to_lowercase().ends_with(&format!(".{ext}")) => format!("{}.{ext}", file.name),
        _ => file.name.clone(),
    }
}

/// Where a download is written until it's complete.
pub fn part_path(to: &Path, n: u64) -> PathBuf {
    let mut name = to.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}-{n}.openexplorer-part", std::process::id()));
    to.with_file_name(name)
}

/// Numbers each download's part file in this process (see [`part_path`]).
static PARTS: AtomicU64 = AtomicU64::new(0);

/// Whether an answer is worth trying again: Google busy or limiting the rate.
fn transient(status: u16, body: &str) -> bool {
    status >= 500 || status == 429 || (status == 403 && (body.contains("rateLimitExceeded") || body.contains("userRateLimitExceeded")))
}

/// Waits before attempt number `attempt + 1`, a fifth of a second at a time so Cancelar is
/// heard; `false` when it was.
fn backoff(attempt: u32, moved: &mut dyn FnMut(u64) -> bool) -> bool {
    let until = Instant::now() + Duration::from_secs(1 << attempt.min(5));
    while Instant::now() < until {
        if !moved(0) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    moved(0)
}

/// The message for an answer that won't get better by asking again.
fn refused(what: &str, status: u16, body: &str) -> String {
    if body.contains("exportSizeLimitExceeded") {
        return tr!(
            "{what}: o Google só exporta arquivos de até 10 MB, e este passa disso. Abra-o no navegador e use Arquivo > Fazer download.",
            "{what}: Google only exports files up to 10 MB, and this one is larger. Open it in the browser and use File > Download."
        );
    }
    if body.contains("cannotDownloadAbusiveFile") {
        return tr!("{what}: o Google marcou este arquivo como malware ou spam e não deixa baixá-lo por aqui.", "{what}: Google marked this file as malware or spam and won't let it be downloaded here.");
    }
    if body.contains("cannotExportFile") || body.contains("fileNotDownloadable") {
        return tr!("{what}: o Google não deixa baixar este arquivo (o dono pode ter desativado o download).", "{what}: Google won't let this file be downloaded (its owner may have turned downloads off).");
    }
    format!("{what}: HTTP {status}: {}", crate::google_reason(body))
}

impl Drive {
    /// Downloads `file` to `to`, the full path of the file to make (see [`local_name`]), which
    /// is replaced if it exists. With `segmented`, a file from [`SEGMENT_MIN`] comes in
    /// [`SEGMENTS`] parallel pieces. Returns the bytes written.
    pub fn download(&self, file: &DriveFile, to: &Path, segmented: bool, got: &mut dyn FnMut(u64) -> bool) -> Result<u64> {
        let pieces = match file.size {
            Some(n) if segmented && n >= SEGMENT_MIN => SEGMENTS,
            _ => 1,
        };
        self.download_in(file, to, pieces, got)
    }

    /// [`Drive::download`] in `pieces` parallel segments (1 for one stream).
    pub fn download_in(&self, file: &DriveFile, to: &Path, pieces: usize, got: &mut dyn FnMut(u64) -> bool) -> Result<u64> {
        self.download_split(file, to, pieces, SPLIT, got)
    }

    /// [`Drive::download_in`], segments taking over each other's rest as `split` says.
    fn download_split(&self, file: &DriveFile, to: &Path, pieces: usize, split: Split, got: &mut dyn FnMut(u64) -> bool) -> Result<u64> {
        let what = tr!("Baixar \"{}\"", "Download \"{}\"", file.name);
        if file.is_dir || file.mime == FOLDER_MIME {
            return Err(tr!("{what}: é uma pasta.", "{what}: it's a folder."));
        }
        if file.mime == SHORTCUT {
            return Err(tr!("{what}: é um atalho; baixe o arquivo original.", "{what}: it's a shortcut; download the original file."));
        }
        if !file.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') || file.id.is_empty() {
            return Err(tr!("Id inválido: {}", "Invalid id: {}", file.id));
        }
        let (url, size) = self.media_url(file, &what)?;
        // A part file of this download's own: two downloads to one place (a copy asked twice)
        // each write theirs, and the last to finish names the file. With one name for both, the
        // first renamed it away under the second, which failed with "file not found".
        let part = part_path(to, PARTS.fetch_add(1, Ordering::Relaxed));
        let started = Instant::now();
        let out = self.fetch_to(&url, &part, size, pieces, split, got, &what).and_then(|written| {
            if let Some(t) = file.modified {
                // The copy dates from when the file changed in Drive, as Explorer's copies do.
                let f = OpenOptions::new().write(true).open(&part).map_err(|e| format!("{what}: {e}"))?;
                f.set_modified(t).map_err(|e| format!("{what}: {e}"))?;
            }
            std::fs::rename(&part, to).map_err(|e| tr!("{what}: não consegui dar o nome final ao arquivo: {e}", "{what}: couldn't give the file its final name: {e}"))?;
            Ok(written)
        });
        if out.is_err() {
            let _ = std::fs::remove_file(&part);
        }
        let secs = started.elapsed().as_secs_f64();
        let how = match &out {
            Ok(n) => format!("ok, {n} bytes em {secs:.1} s ({:.1} MB/s, {pieces} segmento(s))", *n as f64 / secs.max(0.001) / 1_048_576.0),
            Err(e) => e.clone(),
        };
        crate::log(&format!("download {}: {how}", file.id));
        out
    }

    /// Where `file`'s bytes come from, and how many there are when known: the file itself, or
    /// its export for Google's own formats (whose size isn't known before).
    fn media_url(&self, file: &DriveFile, what: &str) -> Result<(String, Option<u64>)> {
        let base = self.api_url.trim_end_matches('/');
        Ok(if file.mime.starts_with(GOOGLE) {
            let Some((mime, _)) = export_format(&file.mime) else {
                return Err(tr!("{what}: este tipo de arquivo do Google não pode ser baixado, só aberto no navegador.", "{what}: this kind of Google file can't be downloaded, only opened in the browser."));
            };
            (format!("{base}/files/{}/export?mimeType={}", file.id, url::form_urlencoded::byte_serialize(mime.as_bytes()).collect::<String>()), None)
        } else {
            (format!("{base}/files/{}?alt=media&supportsAllDrives=true", file.id), file.size)
        })
    }

    /// A reader of `file`'s bytes from `from` on, in one request, for streaming them somewhere
    /// else (a drag into Windows Explorer pulls them this way). The caller asks again from
    /// where it got to when a read fails. An export, which takes no `Range`, skips to `from`.
    pub fn open_media(&self, file: &DriveFile, from: u64) -> Result<Box<dyn Read + Send>> {
        let what = tr!("Baixar \"{}\"", "Download \"{}\"", file.name);
        if file.is_dir || file.mime == FOLDER_MIME || file.mime == SHORTCUT {
            return Err(tr!("{what}: não é um arquivo.", "{what}: it's not a file."));
        }
        if !file.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') || file.id.is_empty() {
            return Err(tr!("Id inválido: {}", "Invalid id: {}", file.id));
        }
        let (url, _) = self.media_url(file, &what)?;
        for renewed in [false, true] {
            let mut req = self.downloads.get(&url).header("Authorization", &format!("Bearer {}", self.token()?)).header("Accept-Encoding", "identity");
            if from > 0 {
                req = req.header("Range", &format!("bytes={from}-"));
            }
            let mut r = req.call().map_err(|e| tr!("{what}: sem conexão com o Google: {e}", "{what}: no connection to Google: {e}"))?;
            match r.status().as_u16() {
                206 => return Ok(Box::new(r.into_body().into_reader())),
                200 => {
                    let mut body = r.into_body().into_reader();
                    if from > 0 && std::io::copy(&mut (&mut body).take(from), &mut std::io::sink()).map_err(|e| format!("{what}: {e}"))? != from {
                        return Err(tr!("{what}: o arquivo acabou antes do esperado.", "{what}: the file ended sooner than expected."));
                    }
                    return Ok(Box::new(body));
                }
                401 if !renewed => self.forget_token(),
                s => {
                    let text = r.body_mut().read_to_string().unwrap_or_default();
                    return Err(refused(&what, s, &text));
                }
            }
        }
        Err(tr!("{what}: o Google recusou o acesso.", "{what}: Google refused access."))
    }

    /// Fetches `url` into `part`: `size` bytes in `pieces` segments when known, else one stream
    /// to its end.
    #[allow(clippy::too_many_arguments)]
    fn fetch_to(&self, url: &str, part: &Path, size: Option<u64>, pieces: usize, split: Split, got: &mut dyn FnMut(u64) -> bool, what: &str) -> Result<u64> {
        let file = File::create(part).map_err(|e| tr!("{what}: não consegui criar o arquivo: {e}", "{what}: couldn't create the file: {e}"))?;
        let Some(size) = size.filter(|_| pieces > 1) else {
            drop(file);
            let mut total = 0u64;
            // To the end of what Google sends, checked against the size after: no Range on the
            // first request, as a plain download asks.
            let end = self.fetch(url, part, &Mutex::new(Seg::new(0, None)), None, &mut |n| {
                total += n;
                got(total)
            }, what)?;
            return match size {
                Some(n) if end != n => Err(tr!("{what}: chegaram {end} de {n} bytes.", "{what}: {end} of {n} bytes arrived.")),
                _ => Ok(end),
            };
        };
        // The whole size up front, so each segment writes its own part of it.
        file.set_len(size).map_err(|e| tr!("{what}: sem espaço para o arquivo? {e}", "{what}: no room for the file? {e}"))?;
        drop(file);
        let total = AtomicU64::new(0);
        let stop = AtomicBool::new(false);
        let step = size.div_ceil(pieces as u64);
        let ranges: Vec<(u64, u64)> = (0..pieces as u64).map(|i| (i * step, ((i + 1) * step).min(size))).filter(|(a, b)| a < b).collect();
        let spans: Vec<Span> = ranges.iter().map(|&(from, until)| Mutex::new(Seg::new(from, Some(until)))).collect();
        let results: Vec<Result<u64>> = std::thread::scope(|scope| {
            let runs: Vec<_> = (0..spans.len())
                .map(|i| {
                    let (total, stop, spans) = (&total, &stop, &spans);
                    scope.spawn(move || {
                        loop {
                            let out = self.fetch(url, part, &spans[i], Some(split.stall), &mut |n| {
                                total.fetch_add(n, Ordering::Relaxed);
                                !stop.load(Ordering::Relaxed)
                            }, what);
                            // One segment failing for good stops the others: the file can't be whole.
                            if out.is_err() {
                                stop.store(true, Ordering::Relaxed);
                            }
                            // Done with its part, it helps whichever would finish last, until all are.
                            loop {
                                if out.is_err() || stop.load(Ordering::Relaxed) {
                                    return out;
                                }
                                match take_over(spans, i, split) {
                                    Next::Took => break,
                                    Next::Done => return out,
                                    Next::Wait => std::thread::sleep(Duration::from_millis(50)),
                                }
                            }
                        }
                    })
                })
                .collect();
            // This thread reports the segments' sum a few times a second, and passes Cancelar on.
            while !runs.iter().all(|r| r.is_finished()) {
                if !got(total.load(Ordering::Relaxed)) {
                    stop.store(true, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            runs.into_iter().map(|r| r.join().unwrap_or_else(|_| Err(tr!("{what}: erro interno num segmento.", "{what}: internal error in a segment.")))).collect()
        });
        let written = total.load(Ordering::Relaxed);
        if !got(written) {
            return Err(CANCELLED.into());
        }
        // The error that stopped the rest, not the cancels it caused.
        if let Some(e) = results.iter().filter_map(|r| r.as_ref().err()).find(|e| *e != CANCELLED).or_else(|| results.iter().find_map(|r| r.as_ref().err())) {
            return Err(e.clone());
        }
        Ok(size)
    }

    /// Fetches the bytes of `url` that `span` holds (to its end when it has none) into the same
    /// place in the file at `path`, asking again from the last byte written after a drop, and
    /// stopping where the span ends when another segment pulled its end back. With `stall`, an
    /// answer that brings nothing for that long is left behind and asked again. `moved` hears each
    /// batch of bytes written (and 0 while waiting); `false` stops it. Returns where the written
    /// bytes end.
    ///
    /// Each request is read on a thread of its own ([`ask`]), so a segment whose rest the others
    /// took over returns at once instead of waiting out a stalled read.
    fn fetch(&self, url: &str, path: &Path, span: &Span, stall: Option<Duration>, moved: &mut dyn FnMut(u64) -> bool, what: &str) -> Result<u64> {
        let mut file = OpenOptions::new().write(true).open(path).map_err(|e| format!("{what}: {e}"))?;
        let mut at = lock(span).at;
        let until = || lock(span).until;
        let mut failures = 0u32;
        loop {
            if until() == Some(at) {
                return Ok(at);
            }
            if !moved(0) {
                return Err(CANCELLED.into());
            }
            let before = at;
            let range = match until() {
                Some(u) => Some(format!("bytes={at}-{}", u - 1)),
                None if at > 0 => Some(format!("bytes={at}-")),
                None => None,
            };
            let mut req = self
                .downloads
                .get(url)
                .header("Authorization", &format!("Bearer {}", self.token()?))
                // Byte offsets are offsets in the file, not in a compressed stream.
                .header("Accept-Encoding", "identity");
            if let Some(r) = &range {
                req = req.header("Range", r);
            }
            if let Some(stall) = stall {
                // So a request left behind ends soon after, and lets its connection go.
                req = req.config().timeout_recv_response(Some(stall * 3)).timeout_per_read(Some(stall)).build();
            }
            let asked = Instant::now();
            {
                let mut s = lock(span);
                (s.asked, s.first, s.from) = (asked, None, at);
            }
            let answer = ask(req, at);
            let mut last = asked;
            let mut started = false;
            // Why this request is asked again, for the log; empty while it goes on.
            let why = loop {
                if until() == Some(at) {
                    return Ok(at);
                }
                if !moved(0) {
                    return Err(CANCELLED.into());
                }
                let quiet = last.elapsed();
                if let Some(stall) = stall
                    && quiet >= if started { stall } else { stall * 3 }
                {
                    break format!("nothing for {:.1} s", quiet.as_secs_f64());
                }
                let got = match answer.recv_timeout(Duration::from_millis(50)) {
                    Ok(got) => got,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break "the request ended".into(),
                };
                last = Instant::now();
                match got {
                    Got::Head(200 | 206, _) => {
                        started = true;
                        file.seek(SeekFrom::Start(at)).map_err(|e| format!("{what}: {e}"))?;
                    }
                    // Asked from past the end: everything is here.
                    Got::Head(416, _) if until().is_none() && at > 0 => return Ok(at),
                    Got::Head(401, _) => {
                        self.forget_token();
                        break "HTTP 401".into();
                    }
                    Got::Head(status, text) => {
                        if !transient(status, &text) {
                            return Err(refused(what, status, &text));
                        }
                        if failures >= RETRIES {
                            return Err(format!("{what}: HTTP {status}: {}", crate::google_reason(&text)));
                        }
                        break format!("HTTP {status}");
                    }
                    Got::Bytes(bytes) => {
                        // What's still this segment's: its end may have moved back while it
                        // read, and the bytes past it are another's now.
                        let keep = {
                            let mut s = lock(span);
                            let keep = s.until.map_or(bytes.len() as u64, |u| (bytes.len() as u64).min(u - s.at));
                            s.at += keep;
                            s.last = last;
                            s.first.get_or_insert(last);
                            keep as usize
                        };
                        file.write_all(&bytes[..keep]).map_err(|e| tr!("{what}: não consegui gravar no disco: {e}", "{what}: couldn't write to the disk: {e}"))?;
                        at += keep as u64;
                        if !moved(keep as u64) {
                            return Err(CANCELLED.into());
                        }
                    }
                    // The body ended early when a known end wasn't reached.
                    Got::End if until().is_none_or(|u| at >= u) => return Ok(at),
                    Got::End => break "the answer ended early".into(),
                    Got::Failed(e) if failures >= RETRIES && at == before => return Err(tr!("{what}: sem conexão com o Google: {e}", "{what}: no connection to Google: {e}")),
                    Got::Failed(e) => break e,
                }
            };
            crate::log(&format!("download {at}..{:?}: {why} after {:.1} s and {} bytes; asking again", until(), asked.elapsed().as_secs_f64(), at - before));
            // A dropped connection that brought bytes is asked again at once; only drops that
            // get nowhere wait, and add up to giving up. A segment whose rest others took over
            // meanwhile is done.
            if at > before || until() == Some(at) {
                failures = 0;
                continue;
            }
            if failures >= RETRIES {
                return Err(tr!("{what}: a conexão caiu várias vezes; tente de novo.", "{what}: the connection dropped several times; try again."));
            }
            if !backoff(failures, moved) {
                return Err(CANCELLED.into());
            }
            failures += 1;
        }
    }
}

/// What a request read on its own thread brings, in order: its status (with the body, for an
/// error), then its bytes, then how it ended.
enum Got {
    Head(u16, String),
    Bytes(Vec<u8>),
    End,
    Failed(String),
}

/// Sends `req` on a thread of its own, which hands over what comes back as it comes; it stops
/// when the receiver is dropped. A whole file where a part from `at` was asked (an export takes
/// no Range) comes without its first `at` bytes.
fn ask(req: ureq::RequestBuilder<ureq::typestate::WithoutBody>, at: u64) -> std::sync::mpsc::Receiver<Got> {
    let (tx, rx) = std::sync::mpsc::sync_channel(16);
    std::thread::spawn(move || {
        let mut r = match req.call() {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(Got::Failed(e.to_string()));
                return;
            }
        };
        let status = r.status().as_u16();
        if !matches!(status, 200 | 206) {
            let text = r.body_mut().read_to_string().unwrap_or_default();
            let _ = tx.send(Got::Head(status, text));
            return;
        }
        if tx.send(Got::Head(status, String::new())).is_err() {
            return;
        }
        let mut body = r.into_body().into_reader();
        if status == 200 && at > 0 && !matches!(std::io::copy(&mut (&mut body).take(at), &mut std::io::sink()), Ok(n) if n == at) {
            let _ = tx.send(Got::Failed("dropped while skipping to where it was".into()));
            return;
        }
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let got = match body.read(&mut buf) {
                Ok(0) => Got::End,
                Ok(n) => Got::Bytes(buf[..n].to_vec()),
                Err(e) => Got::Failed(e.to_string()),
            };
            let last = !matches!(got, Got::Bytes(_));
            if tx.send(got).is_err() || last {
                return;
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;
    use std::time::SystemTime;

    /// A request's first line and headers (names lowercased), as a stand-in server reads them.
    fn request(s: &mut BufReader<TcpStream>) -> (String, Vec<(String, String)>) {
        let mut line = String::new();
        s.read_line(&mut line).unwrap();
        let mut headers = Vec::new();
        loop {
            let mut h = String::new();
            s.read_line(&mut h).unwrap();
            if h.trim().is_empty() {
                break;
            }
            let (k, v) = h.split_once(':').unwrap();
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
        (line, headers)
    }

    fn header<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
        h.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// `bytes=a-b` or `bytes=a-` as (a, b exclusive), against a file of `size`.
    fn parse_range(r: &str, size: u64) -> (u64, u64) {
        let (a, b) = r.strip_prefix("bytes=").unwrap().split_once('-').unwrap();
        (a.parse().unwrap(), if b.is_empty() { size } else { b.parse::<u64>().unwrap() + 1 })
    }

    /// Answers with `body`, but cuts the line after `cut` bytes of it when given.
    fn answer(s: &mut BufReader<TcpStream>, status: &str, extra: &str, body: &[u8], len: usize, cut: Option<usize>) {
        let head = format!("HTTP/1.1 {status}\r\n{extra}Content-Length: {len}\r\nConnection: close\r\n\r\n");
        let w = s.get_mut();
        w.write_all(head.as_bytes()).unwrap();
        w.write_all(&body[..cut.unwrap_or(body.len())]).unwrap();
        w.flush().unwrap();
    }

    fn content(n: usize) -> Vec<u8> {
        (0..n as u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect()
    }

    /// Whether no download left a part file in `dir`.
    fn no_parts(dir: &Path) -> bool {
        std::fs::read_dir(dir).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".openexplorer-part"))
    }

    /// The same file asked twice into one folder at once (a copy started twice): both downloads
    /// finish, the file is whole, and no part file is left.
    #[test]
    fn two_downloads_to_one_place_both_finish() {
        let data = content(2 * 1024 * 1024 + 5);
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.local_addr().unwrap());
        let served = data.clone();
        let fake = std::thread::spawn(move || {
            let conns: Vec<_> = (0..2).map(|_| server.accept().unwrap().0).collect();
            let feeds: Vec<_> = conns
                .into_iter()
                .map(|c| {
                    let served = served.clone();
                    std::thread::spawn(move || {
                        let mut s = BufReader::new(c);
                        request(&mut s);
                        answer(&mut s, "200 OK", "", &served, served.len(), None);
                    })
                })
                .collect();
            feeds.into_iter().for_each(|f| f.join().unwrap());
        });
        let dir = scratch("twice");
        let to = dir.join("a.bin");
        let file = drive_file("ab", "a.bin", Some(data.len() as u64), "application/octet-stream");
        let runs: Vec<_> = (0..2)
            .map(|_| {
                let (d, file, to) = (Drive::for_tests(format!("{base}/")), file.clone(), to.clone());
                std::thread::spawn(move || d.download(&file, &to, false, &mut |_| true))
            })
            .collect();
        for r in runs {
            assert_eq!(r.join().unwrap().unwrap(), data.len() as u64);
        }
        fake.join().unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), data);
        assert!(no_parts(&dir));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("openexplorer-download-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn drive_file(id: &str, name: &str, size: Option<u64>, mime: &str) -> DriveFile {
        DriveFile {
            id: id.into(),
            name: name.into(),
            is_dir: false,
            size,
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            mime: mime.into(),
            target: None,
            md5: None,
            starred: false,
        }
    }

    #[test]
    fn a_dropped_download_resumes_with_a_range_from_what_was_written() {
        let data = content(3 * 1024 * 1024 + 17);
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.local_addr().unwrap());
        let served = data.clone();
        let fake = std::thread::spawn(move || {
            // The whole file asked for; the line drops after 1 MB.
            let mut s = BufReader::new(server.accept().unwrap().0);
            let (line, h) = request(&mut s);
            assert!(line.starts_with("GET /files/abc?alt=media"), "{line}");
            assert_eq!(header(&h, "range"), None);
            assert_eq!(header(&h, "authorization"), Some("Bearer token"));
            answer(&mut s, "200 OK", "", &served, served.len(), Some(1 << 20));
            drop(s);
            // Asked again from the first byte not written.
            let mut s = BufReader::new(server.accept().unwrap().0);
            let (_, h) = request(&mut s);
            assert_eq!(header(&h, "range"), Some("bytes=1048576-"));
            let rest = &served[1 << 20..];
            answer(&mut s, "206 Partial Content", &format!("Content-Range: bytes 1048576-{}/{}\r\n", served.len() - 1, served.len()), rest, rest.len(), None);
        });
        let dir = scratch("resume");
        let to = dir.join("vídeo.bin");
        let d = Drive::for_tests(format!("{base}/"));
        let file = drive_file("abc", "vídeo.bin", Some(data.len() as u64), "application/octet-stream");
        let mut seen = Vec::new();
        let n = d.download(&file, &to, false, &mut |n| {
            seen.push(n);
            true
        });
        fake.join().unwrap();
        assert_eq!(n.unwrap(), data.len() as u64);
        assert_eq!(std::fs::read(&to).unwrap(), data);
        assert!(no_parts(&dir), "the part file took the final name");
        // The bar only went forward: the resumed part continued from 1 MB.
        assert!(seen.windows(2).all(|w| w[0] <= w[1]));
        assert!(seen.contains(&(1 << 20)));
        assert_eq!(seen.last(), Some(&(data.len() as u64)));
        assert_eq!(std::fs::metadata(&to).unwrap().modified().unwrap(), file.modified.unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_big_file_comes_in_parallel_segments_that_each_resume() {
        let data = content(4 * 1024 * 1024 + 3);
        let size = data.len() as u64;
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.local_addr().unwrap());
        let asked: std::sync::Arc<Mutex<Vec<(u64, u64)>>> = Default::default();
        let (served, log) = (data.clone(), asked.clone());
        std::thread::spawn(move || {
            let mut dropped_one = false;
            for conn in server.incoming() {
                let mut s = BufReader::new(conn.unwrap());
                let (_, h) = request(&mut s);
                let (a, b) = parse_range(header(&h, "range").expect("each segment asks for its range"), size);
                log.lock().unwrap().push((a, b));
                let part = served[a as usize..b as usize].to_vec();
                let extra = format!("Content-Range: bytes {a}-{}/{size}\r\n", b - 1);
                // The second segment's first answer breaks off halfway.
                let cut = (!dropped_one && a > 0).then(|| {
                    dropped_one = true;
                    part.len() / 2
                });
                std::thread::spawn(move || answer(&mut s, "206 Partial Content", &extra, &part, part.len(), cut));
            }
        });
        let dir = scratch("segments");
        let to = dir.join("grande.iso");
        let d = Drive::for_tests(format!("{base}/"));
        let file = drive_file("big", "grande.iso", Some(size), "application/octet-stream");
        let mut last = 0;
        let n = d.download_in(&file, &to, 4, &mut |n| {
            assert!(n >= last, "the sum of the segments never goes back");
            last = n;
            true
        });
        assert_eq!(n.unwrap(), size);
        assert_eq!(last, size);
        assert_eq!(std::fs::read(&to).unwrap(), data);
        let asked = asked.lock().unwrap().clone();
        // Four segments, then the cut one again from where it broke off.
        let step = size.div_ceil(4);
        for i in 0..4 {
            assert!(asked.contains(&(i * step, ((i + 1) * step).min(size))), "{asked:?}");
        }
        assert_eq!(asked.len(), 5, "{asked:?}");
        let resumed = asked[4];
        assert!(!resumed.0.is_multiple_of(step) && resumed.1 == ((resumed.0 / step + 1) * step).min(size), "{asked:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_slow_segment_hands_the_rest_of_its_part_to_the_others() {
        let data = content(4 * 1024 * 1024 + 3);
        let size = data.len() as u64;
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.local_addr().unwrap());
        let asked: std::sync::Arc<Mutex<Vec<(u64, u64)>>> = Default::default();
        let (served, log) = (data.clone(), asked.clone());
        std::thread::spawn(move || {
            for conn in server.incoming() {
                let mut s = BufReader::new(conn.unwrap());
                let (_, h) = request(&mut s);
                let (a, b) = parse_range(header(&h, "range").unwrap(), size);
                log.lock().unwrap().push((a, b));
                let part = served[a as usize..b as usize].to_vec();
                std::thread::spawn(move || {
                    let head = format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {a}-{}/{size}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b - 1, part.len());
                    let w = s.get_mut();
                    let _ = w.write_all(head.as_bytes());
                    // The first segment's first answer trickles; the client may hang up on it.
                    for chunk in part.chunks(if a == 0 { 8 * 1024 } else { part.len() }) {
                        if a == 0 {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        if w.write_all(chunk).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        let dir = scratch("split");
        let to = dir.join("grande.iso");
        let d = Drive::for_tests(format!("{base}/"));
        let file = drive_file("big", "grande.iso", Some(size), "application/octet-stream");
        let started = Instant::now();
        let mut last = 0;
        let split = Split { ask: 0.05, after: 0.05, min: 64 * 1024, stall: Duration::from_secs(5) };
        let n = d.download_split(&file, &to, 4, split, &mut |n| {
            last = n;
            true
        });
        assert_eq!(n.unwrap(), size);
        assert_eq!(last, size, "each byte counted once");
        assert_eq!(std::fs::read(&to).unwrap(), data);
        let asked = asked.lock().unwrap().clone();
        let step = size.div_ceil(4);
        assert!(asked.iter().any(|&(a, _)| a > 0 && a < step), "the slow first segment was taken over: {asked:?}");
        // Alone, the first segment trickles for over half a second (128 chunks, 5 ms each).
        assert!(started.elapsed() < Duration::from_millis(400), "{:?}", started.elapsed());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_stalled_segment_is_asked_again_and_its_rest_taken_over() {
        let data = content(4 * 1024 * 1024 + 3);
        let size = data.len() as u64;
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.local_addr().unwrap());
        let served = data.clone();
        std::thread::spawn(move || {
            let mut stalled = false;
            for conn in server.incoming() {
                let mut s = BufReader::new(conn.unwrap());
                let (_, h) = request(&mut s);
                let (a, b) = parse_range(header(&h, "range").unwrap(), size);
                let part = served[a as usize..b as usize].to_vec();
                // The first segment's first answer sends a little, then nothing for 10 s.
                let stall = a == 0 && !std::mem::replace(&mut stalled, true);
                std::thread::spawn(move || {
                    let head = format!("HTTP/1.1 206 Partial Content
Content-Range: bytes {a}-{}/{size}
Content-Length: {}
Connection: close

", b - 1, part.len());
                    let w = s.get_mut();
                    let _ = w.write_all(head.as_bytes());
                    if stall {
                        let _ = w.write_all(&part[..64 * 1024]);
                        let _ = w.flush();
                        std::thread::sleep(Duration::from_secs(10));
                    }
                    let _ = w.write_all(&part);
                });
            }
        });
        let dir = scratch("stall");
        let to = dir.join("grande.iso");
        let d = Drive::for_tests(format!("{base}/"));
        let file = drive_file("big", "grande.iso", Some(size), "application/octet-stream");
        let started = Instant::now();
        let split = Split { ask: 0.05, after: 0.05, min: 64 * 1024, stall: Duration::from_millis(300) };
        let n = d.download_split(&file, &to, 4, split, &mut |_| true);
        assert_eq!(n.unwrap(), size);
        assert_eq!(std::fs::read(&to).unwrap(), data);
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cancelling_stops_the_download_and_leaves_no_part_file() {
        let data = content(2 * 1024 * 1024);
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.local_addr().unwrap());
        let served = data.clone();
        std::thread::spawn(move || {
            let mut s = BufReader::new(server.accept().unwrap().0);
            request(&mut s);
            let _ = s.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", served.len()).as_bytes());
            let _ = s.get_mut().write_all(&served);
        });
        let dir = scratch("cancel");
        let to = dir.join("a.bin");
        let d = Drive::for_tests(format!("{base}/"));
        let file = drive_file("c", "a.bin", Some(data.len() as u64), "application/zip");
        let out = d.download(&file, &to, false, &mut |n| n < 300_000);
        assert_eq!(out.unwrap_err(), CANCELLED);
        assert!(!to.exists() && no_parts(&dir));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn google_docs_are_exported_and_the_size_limit_is_named() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.local_addr().unwrap());
        std::thread::spawn(move || {
            let mut s = BufReader::new(server.accept().unwrap().0);
            let (line, _) = request(&mut s);
            assert!(line.starts_with("GET /files/doc1/export?mimeType=application%2Fvnd.openxmlformats-officedocument.wordprocessingml.document"), "{line}");
            answer(&mut s, "200 OK", "", b"PK docx", 7, None);
            let mut s = BufReader::new(server.accept().unwrap().0);
            request(&mut s);
            let body = br#"{"error":{"code":403,"message":"This file is too large to be exported.","errors":[{"reason":"exportSizeLimitExceeded"}]}}"#;
            answer(&mut s, "403 Forbidden", "", body, body.len(), None);
        });
        let dir = scratch("export");
        let d = Drive::for_tests(format!("{base}/"));
        let doc = drive_file("doc1", "Ata", None, "application/vnd.google-apps.document");
        assert_eq!(local_name(&doc), "Ata.docx");
        let to = dir.join(local_name(&doc));
        assert_eq!(d.download(&doc, &to, true, &mut |_| true).unwrap(), 7);
        assert_eq!(std::fs::read(&to).unwrap(), b"PK docx");
        let big = drive_file("doc2", "Livro", None, "application/vnd.google-apps.document");
        let e = d.download(&big, &dir.join("Livro.docx"), false, &mut |_| true).unwrap_err();
        assert!(e.contains("10 MB"), "{e}");
        let form = drive_file("f", "Pesquisa", None, "application/vnd.google-apps.form");
        assert!(d.download(&form, &dir.join("Pesquisa"), false, &mut |_| true).unwrap_err().contains("navegador"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn exports_name_their_format() {
        let f = |name: &str, mime: &str| local_name(&drive_file("x", name, None, mime));
        assert_eq!(f("Orçamento", "application/vnd.google-apps.spreadsheet"), "Orçamento.xlsx");
        assert_eq!(f("Aula 3", "application/vnd.google-apps.presentation"), "Aula 3.pptx");
        assert_eq!(f("Planta", "application/vnd.google-apps.drawing"), "Planta.pdf");
        assert_eq!(f("Nota.docx", "application/vnd.google-apps.document"), "Nota.docx");
        assert_eq!(f("foto.jpg", "image/jpeg"), "foto.jpg");
        assert_eq!(export_format("application/vnd.google-apps.form"), None);
        assert_eq!(part_path(Path::new(r"C:\a\b.txt"), 7), PathBuf::from(format!(r"C:\a\b.txt.{}-7.openexplorer-part", std::process::id())));
    }
}
