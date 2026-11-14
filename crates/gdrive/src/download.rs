//! Fetching a Drive file to the computer.
//!
//! - **Files with content** come from `files.get?alt=media`, streamed into a `.openexplorer-part`
//!   file next to the target, which takes the target's name only once all of it arrived. When
//!   the connection drops or Google answers 5xx, 429 or a rate limit, the request goes again
//!   after a backoff with an HTTP `Range` from the bytes already written, so nothing is fetched
//!   twice.
//! - **Big files** (from [`SEGMENT_MIN`], when the caller asks for it) come in [`SEGMENTS`]
//!   parallel `Range` requests into one preallocated file, each segment resuming on its own.
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
use std::time::{Duration, Instant};

use crate::api::{Drive, DriveFile, FOLDER_MIME};
use crate::{CANCELLED, Result};

/// Files from this size are fetched in parallel segments, when asked to.
pub const SEGMENT_MIN: u64 = 64 * 1024 * 1024;

/// How many segments a big file is fetched in.
pub const SEGMENTS: usize = 4;

/// Times a failed request is tried again in a row without progress (with 1, 2, 4, 8, 16, 32 s
/// between), for dropped connections, 5xx, 429 and Drive's rate limits.
const RETRIES: u32 = 6;

/// What Drive calls its own formats: they have no content of their own, only exports.
const GOOGLE: &str = "application/vnd.google-apps.";
const SHORTCUT: &str = "application/vnd.google-apps.shortcut";

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
pub fn part_path(to: &Path) -> PathBuf {
    let mut name = to.file_name().unwrap_or_default().to_os_string();
    name.push(".openexplorer-part");
    to.with_file_name(name)
}

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
        return format!("{what}: o Google só exporta arquivos de até 10 MB, e este passa disso. Abra-o no navegador e use Arquivo > Fazer download.");
    }
    if body.contains("cannotDownloadAbusiveFile") {
        return format!("{what}: o Google marcou este arquivo como malware ou spam e não deixa baixá-lo por aqui.");
    }
    if body.contains("cannotExportFile") || body.contains("fileNotDownloadable") {
        return format!("{what}: o Google não deixa baixar este arquivo (o dono pode ter desativado o download).");
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
    pub(crate) fn download_in(&self, file: &DriveFile, to: &Path, pieces: usize, got: &mut dyn FnMut(u64) -> bool) -> Result<u64> {
        let what = format!("Baixar \"{}\"", file.name);
        if file.is_dir || file.mime == FOLDER_MIME {
            return Err(format!("{what}: é uma pasta."));
        }
        if file.mime == SHORTCUT {
            return Err(format!("{what}: é um atalho; baixe o arquivo original."));
        }
        if !file.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') || file.id.is_empty() {
            return Err(format!("Id inválido: {}", file.id));
        }
        let (url, size) = self.media_url(file, &what)?;
        let part = part_path(to);
        let started = Instant::now();
        let out = self.fetch_to(&url, &part, size, pieces, got, &what).and_then(|written| {
            if let Some(t) = file.modified {
                // The copy dates from when the file changed in Drive, as Explorer's copies do.
                let f = OpenOptions::new().write(true).open(&part).map_err(|e| format!("{what}: {e}"))?;
                f.set_modified(t).map_err(|e| format!("{what}: {e}"))?;
            }
            std::fs::rename(&part, to).map_err(|e| format!("{what}: não consegui dar o nome final ao arquivo: {e}"))?;
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
                return Err(format!("{what}: este tipo de arquivo do Google não pode ser baixado, só aberto no navegador."));
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
        let what = format!("Baixar \"{}\"", file.name);
        if file.is_dir || file.mime == FOLDER_MIME || file.mime == SHORTCUT {
            return Err(format!("{what}: não é um arquivo."));
        }
        if !file.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') || file.id.is_empty() {
            return Err(format!("Id inválido: {}", file.id));
        }
        let (url, _) = self.media_url(file, &what)?;
        for renewed in [false, true] {
            let mut req = self.downloads.get(&url).header("Authorization", &format!("Bearer {}", self.token()?)).header("Accept-Encoding", "identity");
            if from > 0 {
                req = req.header("Range", &format!("bytes={from}-"));
            }
            let mut r = req.call().map_err(|e| format!("{what}: sem conexão com o Google: {e}"))?;
            match r.status().as_u16() {
                206 => return Ok(Box::new(r.into_body().into_reader())),
                200 => {
                    let mut body = r.into_body().into_reader();
                    if from > 0 && std::io::copy(&mut (&mut body).take(from), &mut std::io::sink()).map_err(|e| format!("{what}: {e}"))? != from {
                        return Err(format!("{what}: o arquivo acabou antes do esperado."));
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
        Err(format!("{what}: o Google recusou o acesso."))
    }

    /// Fetches `url` into `part`: `size` bytes in `pieces` segments when known, else one stream
    /// to its end.
    fn fetch_to(&self, url: &str, part: &Path, size: Option<u64>, pieces: usize, got: &mut dyn FnMut(u64) -> bool, what: &str) -> Result<u64> {
        let file = File::create(part).map_err(|e| format!("{what}: não consegui criar o arquivo: {e}"))?;
        let Some(size) = size.filter(|_| pieces > 1) else {
            drop(file);
            let mut total = 0u64;
            // To the end of what Google sends, checked against the size after: no Range on the
            // first request, as a plain download asks.
            let end = self.fetch(url, part, 0, None, &mut |n| {
                total += n;
                got(total)
            }, what)?;
            return match size {
                Some(n) if end != n => Err(format!("{what}: chegaram {end} de {n} bytes.")),
                _ => Ok(end),
            };
        };
        // The whole size up front, so each segment writes its own part of it.
        file.set_len(size).map_err(|e| format!("{what}: sem espaço para o arquivo? {e}"))?;
        drop(file);
        let total = AtomicU64::new(0);
        let stop = AtomicBool::new(false);
        let step = size.div_ceil(pieces as u64);
        let ranges: Vec<(u64, u64)> = (0..pieces as u64).map(|i| (i * step, ((i + 1) * step).min(size))).filter(|(a, b)| a < b).collect();
        let results: Vec<Result<u64>> = std::thread::scope(|scope| {
            let runs: Vec<_> = ranges
                .iter()
                .map(|&(from, until)| {
                    let (total, stop) = (&total, &stop);
                    scope.spawn(move || {
                        let out = self.fetch(url, part, from, Some(until), &mut |n| {
                            total.fetch_add(n, Ordering::Relaxed);
                            !stop.load(Ordering::Relaxed)
                        }, what);
                        // One segment failing for good stops the others: the file can't be whole.
                        if out.is_err() {
                            stop.store(true, Ordering::Relaxed);
                        }
                        out
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
            runs.into_iter().map(|r| r.join().unwrap_or_else(|_| Err(format!("{what}: erro interno num segmento.")))).collect()
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

    /// Fetches bytes `from..until` of `url` (to its end when `until` is `None`) into the same
    /// place in the file at `path`, asking again from the last byte written after a drop.
    /// `moved` hears each batch of bytes written (and 0 while waiting); `false` stops it.
    /// Returns where the written bytes end.
    fn fetch(&self, url: &str, path: &Path, from: u64, until: Option<u64>, moved: &mut dyn FnMut(u64) -> bool, what: &str) -> Result<u64> {
        let mut file = OpenOptions::new().write(true).open(path).map_err(|e| format!("{what}: {e}"))?;
        let mut at = from;
        let mut failures = 0u32;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            if until == Some(at) {
                return Ok(at);
            }
            if !moved(0) {
                return Err(CANCELLED.into());
            }
            let before = at;
            let range = match until {
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
            let failed = match req.call() {
                Ok(mut r) => match r.status().as_u16() {
                    s @ (200 | 206) => {
                        let mut body = r.body_mut().as_reader();
                        // A whole file where a part was asked (an export takes no Range): skip
                        // what's already written; a drop while skipping is a drop like any other.
                        let mut dropped = s == 200 && at > 0 && !matches!(std::io::copy(&mut (&mut body).take(at), &mut std::io::sink()), Ok(n) if n == at);
                        file.seek(SeekFrom::Start(at)).map_err(|e| format!("{what}: {e}"))?;
                        while !dropped {
                            let room = until.map_or(buf.len() as u64, |u| (u - at).min(buf.len() as u64)) as usize;
                            if room == 0 {
                                break;
                            }
                            match body.read(&mut buf[..room]) {
                                Ok(0) => break,
                                Ok(n) => {
                                    file.write_all(&buf[..n]).map_err(|e| format!("{what}: não consegui gravar no disco: {e}"))?;
                                    at += n as u64;
                                    if !moved(n as u64) {
                                        return Err(CANCELLED.into());
                                    }
                                }
                                Err(_) => dropped = true,
                            }
                        }
                        // The body ended early when a known end wasn't reached.
                        if !dropped && until.is_none_or(|u| at >= u) {
                            return Ok(at);
                        }
                        true
                    }
                    // Asked from past the end: everything is here.
                    416 if until.is_none() && at > 0 => return Ok(at),
                    401 => {
                        self.forget_token();
                        true
                    }
                    s => {
                        let text = r.body_mut().read_to_string().unwrap_or_default();
                        if !transient(s, &text) {
                            return Err(refused(what, s, &text));
                        }
                        if failures >= RETRIES {
                            return Err(format!("{what}: HTTP {s}: {}", crate::google_reason(&text)));
                        }
                        true
                    }
                },
                Err(e) if failures >= RETRIES => return Err(format!("{what}: sem conexão com o Google: {e}")),
                Err(_) => true,
            };
            if failed {
                // Only drops that get nowhere add up to giving up.
                if at > before {
                    failures = 0;
                }
                if failures >= RETRIES {
                    return Err(format!("{what}: a conexão caiu várias vezes; tente de novo."));
                }
                if !backoff(failures, moved) {
                    return Err(CANCELLED.into());
                }
                failures += 1;
            }
        }
    }
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
        assert!(!part_path(&to).exists(), "the part file took the final name");
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
        assert!(!to.exists() && !part_path(&to).exists());
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
        assert_eq!(part_path(Path::new(r"C:\a\b.txt")), Path::new(r"C:\a\b.txt.openexplorer-part"));
    }
}
