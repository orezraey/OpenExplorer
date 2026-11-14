//! Sending a local file to Drive, the way that's quickest for its size:
//!
//! - **Small files** (up to [`SIMPLE_MAX`]) go in one multipart request, name and content
//!   together: one round trip, which is what counts when there are many of them.
//! - **Larger files** go through a resumable session: one request opens it, then the whole file
//!   streams in a single PUT, so nothing is split into chunks that each wait for an answer. If
//!   the connection drops or Google answers 5xx, Drive is asked how much it kept and the rest
//!   is sent from there, after a backoff.
//!
//! The same two routes also send a new version of a file already in Drive ([`Drive::upload_version`]).
//!
//! Either way the file's date comes along, a new file's type is taken from its extension, and `sent`
//! hears how many bytes have gone (and stops the upload by answering `false`).
//! <https://developers.google.com/workspace/drive/api/guides/manage-uploads>

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use crate::api::{Drive, DriveFile, FILE_FIELDS, RawFile, checked};
use crate::{CANCELLED, Result};

pub(crate) const UPLOAD: &str = "https://www.googleapis.com/upload/drive/v3/files";

/// Files up to this size go in one multipart request.
pub const SIMPLE_MAX: u64 = 5 * 1024 * 1024;

/// Times a failed request is tried again (with 1, 2, 4, 8, 16, 32 s between), for dropped
/// connections, 5xx, 429 and Drive's rate limits; for a big file, in a row without progress.
const RETRIES: u32 = 6;

/// Feeds a file to the request and reports what's gone; a `false` from `sent` stops it.
struct Counted<'a> {
    file: std::io::Take<File>,
    done: u64,
    sent: &'a mut dyn FnMut(u64) -> bool,
}

impl Read for Counted<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.file.read(buf)?;
        self.done += n as u64;
        if !(self.sent)(self.done) {
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, CANCELLED));
        }
        Ok(n)
    }
}

/// Whether an answer is worth trying again: Google busy or limiting the rate.
fn transient(status: u16, body: &str) -> bool {
    status >= 500 || status == 429 || (status == 403 && (body.contains("rateLimitExceeded") || body.contains("userRateLimitExceeded")))
}

fn backoff(attempt: u32) {
    std::thread::sleep(Duration::from_secs(1 << attempt.min(5)));
}

/// The file's type from its extension, as Drive shows and previews it.
fn mime_of(name: &str) -> String {
    mime_guess::from_path(name).first_or_octet_stream().essence_str().to_string()
}

fn rfc3339(t: SystemTime) -> Option<String> {
    let t: chrono::DateTime<chrono::Utc> = t.into();
    Some(t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn parse(body: &str, what: &str) -> Result<DriveFile> {
    let raw: RawFile = serde_json::from_str(body).map_err(|e| format!("{what}: resposta inesperada: {e}"))?;
    Ok(raw.into())
}

/// Where an upload's content goes.
enum Dest<'a> {
    /// A new file `name` in folder `parent`.
    New { parent: &'a str, name: &'a str },
    /// A new version of file `id`, which keeps its id, name, folder and sharing.
    Version { id: &'a str },
}

/// One upload's request, the same for the multipart and the resumable route: the method, the
/// URL (without its query), the metadata sent along, and the content's type.
struct Target {
    method: &'static str,
    url: String,
    meta: serde_json::Value,
    mime: String,
}

impl Drive {
    /// Uploads the file at `path` into folder `parent` as `name`.
    pub fn upload(&self, path: &Path, parent: &str, name: &str, sent: &mut dyn FnMut(u64) -> bool) -> Result<DriveFile> {
        self.send_file(path, Dest::New { parent, name }, &format!("Enviar \"{name}\""), sent)
    }

    /// Replaces the content of Drive file `id` with the file at `path`: Drive keeps it as a new
    /// version of the same file (`files.update` with media), so its id, name, folder, sharing
    /// and earlier versions stay. Small or large, it goes the way [`Drive::upload`] does, with
    /// the same retries and resuming.
    pub fn upload_version(&self, id: &str, path: &Path, sent: &mut dyn FnMut(u64) -> bool) -> Result<DriveFile> {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.send_file(path, Dest::Version { id: checked(id)? }, &format!("Substituir \"{name}\""), sent)
    }

    fn send_file(&self, path: &Path, dest: Dest, what: &str, sent: &mut dyn FnMut(u64) -> bool) -> Result<DriveFile> {
        let meta = std::fs::metadata(path).map_err(|e| format!("{what}: {e}"))?;
        let size = meta.len();
        let local_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut target = match dest {
            Dest::New { parent, name } => {
                let mime = mime_of(name);
                let meta = serde_json::json!({ "name": name, "parents": [parent], "mimeType": mime });
                Target { method: "POST", url: self.upload_url.clone(), meta, mime }
            }
            // The type is left as it is: only the content and its date change.
            Dest::Version { id } => {
                Target { method: "PATCH", url: format!("{}/{id}", self.upload_url.trim_end_matches('/')), meta: serde_json::json!({}), mime: mime_of(&local_name) }
            }
        };
        if let Some(t) = meta.modified().ok().and_then(rfc3339) {
            target.meta["modifiedTime"] = t.into();
        }
        let started = Instant::now();
        // The furthest point reached, for the log's speed.
        let mut reached = 0u64;
        let mut track = |n: u64| {
            reached = reached.max(n);
            sent(n)
        };
        let out = if size <= SIMPLE_MAX { self.upload_simple(path, &target, size, &mut track, what) } else { self.upload_resumable(path, &target, size, &mut track, what) };
        let secs = started.elapsed().as_secs_f64();
        let speed = reached as f64 / secs.max(0.001) / 1_048_576.0;
        let how = match &out {
            Ok(_) => "ok".to_string(),
            Err(e) => e.clone(),
        };
        crate::log(&format!("upload ({}) {reached} de {size} bytes em {secs:.1} s ({speed:.1} MB/s): {how}", target.method));
        out
    }

    /// The upload request for `target`: a new file or a new version.
    fn start(&self, target: &Target, url: &str) -> ureq::RequestBuilder<ureq::typestate::WithBody> {
        if target.method == "PATCH" { self.uploads.patch(url) } else { self.uploads.post(url) }
    }

    /// Metadata and content in one multipart/related request.
    fn upload_simple(&self, path: &Path, target: &Target, size: u64, sent: &mut dyn FnMut(u64) -> bool, what: &str) -> Result<DriveFile> {
        let content = std::fs::read(path).map_err(|e| format!("{what}: {e}"))?;
        // A boundary no file is likely to hold.
        let boundary = format!("openexplorer-{:x}", SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        let mut req = Vec::with_capacity(content.len() + 512);
        req.extend_from_slice(format!("--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{}\r\n--{boundary}\r\nContent-Type: {}\r\n\r\n", target.meta, target.mime).as_bytes());
        req.extend_from_slice(&content);
        req.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let url = format!("{}?uploadType=multipart&supportsAllDrives=true&fields={FILE_FIELDS}", target.url);
        for attempt in 0..=RETRIES {
            if !sent(0) {
                return Err(CANCELLED.into());
            }
            let resp = self
                .start(target, &url)
                .header("Authorization", &format!("Bearer {}", self.token()?))
                .header("Content-Type", &format!("multipart/related; boundary={boundary}"))
                .send(&req[..]);
            match resp {
                Ok(mut r) => {
                    let status = r.status().as_u16();
                    let text = r.body_mut().read_to_string().unwrap_or_default();
                    if (200..300).contains(&status) {
                        sent(size);
                        return parse(&text, what);
                    }
                    if status == 401 {
                        self.forget_token();
                    } else if !transient(status, &text) || attempt == RETRIES {
                        return Err(format!("{what}: HTTP {status}: {}", crate::google_reason(&text)));
                    }
                }
                Err(e) if attempt == RETRIES => return Err(format!("{what}: sem conexão com o Google: {e}")),
                Err(_) => {}
            }
            backoff(attempt);
        }
        unreachable!("the last attempt always returns")
    }

    /// Opens a resumable session, then streams the file into it, resuming after failures.
    fn upload_resumable(&self, path: &Path, target: &Target, size: u64, sent: &mut dyn FnMut(u64) -> bool, what: &str) -> Result<DriveFile> {
        let url = format!("{}?uploadType=resumable&supportsAllDrives=true&fields={FILE_FIELDS}", target.url);
        let mut session = None;
        for attempt in 0..=RETRIES {
            let resp = self
                .start(target, &url)
                .header("Authorization", &format!("Bearer {}", self.token()?))
                .header("X-Upload-Content-Type", &target.mime)
                .header("X-Upload-Content-Length", &size.to_string())
                .send_json(&target.meta);
            match resp {
                Ok(mut r) => {
                    let status = r.status().as_u16();
                    if (200..300).contains(&status) {
                        session = r.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_string);
                        break;
                    }
                    let text = r.body_mut().read_to_string().unwrap_or_default();
                    if status == 401 {
                        self.forget_token();
                    } else if !transient(status, &text) || attempt == RETRIES {
                        return Err(format!("{what}: HTTP {status}: {}", crate::google_reason(&text)));
                    }
                }
                Err(e) if attempt == RETRIES => return Err(format!("{what}: sem conexão com o Google: {e}")),
                Err(_) => {}
            }
            backoff(attempt);
        }
        let session = session.ok_or_else(|| format!("{what}: o Google não abriu a sessão de envio."))?;
        let expired = || format!("{what}: a sessão de envio expirou; tente de novo.");
        let mut offset = 0u64;
        let mut failures = 0;
        let mut file = File::open(path).map_err(|e| format!("{what}: {e}"))?;
        loop {
            let from = offset;
            file.seek(SeekFrom::Start(offset)).map_err(|e| format!("{what}: {e}"))?;
            // The whole rest in one PUT: chunks would each wait for an answer. Measured on a
            // 600 Mbit/s line, one PUT ran at 33-37 MB/s, 256 MB chunks at 33, 64 MB at 28;
            // HTTP/3 (QUIC, as Chrome uploads) gave 35-37, the same, and was taken out.
            let mut body = Counted { file: file.try_clone().map_err(|e| format!("{what}: {e}"))?.take(size - offset), done: offset, sent: &mut *sent };
            let resp = self
                .uploads
                .put(&session)
                .header("Authorization", &format!("Bearer {}", self.token()?))
                .header("Content-Length", &(size - offset).to_string())
                .header("Content-Range", &format!("bytes {offset}-{}/{size}", size - 1))
                .send(ureq::SendBody::from_reader(&mut body));
            let done = body.done.min(size);
            if !sent(done) {
                // Tell Google to drop what it kept of this file.
                let _ = self.uploads.delete(&session).call();
                return Err(CANCELLED.into());
            }
            let failed = match resp {
                Ok(mut r) => {
                    let status = r.status().as_u16();
                    let range = r.headers().get("range").and_then(|v| v.to_str().ok()).map(str::to_string);
                    let text = r.body_mut().read_to_string().unwrap_or_default();
                    match status {
                        200 | 201 => {
                            sent(size);
                            return parse(&text, what);
                        }
                        // Not all of it arrived: go on from where Drive's copy ends.
                        308 => {
                            offset = kept(range.as_deref());
                            continue;
                        }
                        401 => {
                            self.forget_token();
                            true
                        }
                        404 | 410 => return Err(expired()),
                        s if transient(s, &text) => true,
                        s => return Err(format!("{what}: HTTP {s}: {}", crate::google_reason(&text))),
                    }
                }
                Err(_) => true,
            };
            if failed {
                failures += 1;
                if failures > RETRIES {
                    return Err(format!("{what}: a conexão caiu várias vezes; tente de novo."));
                }
                backoff(failures - 1);
                match self.kept_by_drive(&session, size) {
                    Kept::From(n) => offset = n,
                    Kept::Done(body) => {
                        sent(size);
                        return parse(&body, what);
                    }
                    Kept::Expired => return Err(expired()),
                    Kept::Unknown => {}
                }
                // A drop after real progress starts the count again: only drops that get
                // nowhere add up to giving up.
                if offset > from {
                    failures = 0;
                }
                sent(offset);
            }
        }
    }
}

/// How much of an upload Drive kept, asked after a failure.
enum Kept {
    /// Go on from this byte.
    From(u64),
    /// It all arrived: Drive's answer, the file made.
    Done(String),
    Expired,
    /// The question itself failed; go on from where we were.
    Unknown,
}

impl Drive {
    fn kept_by_drive(&self, session: &str, size: u64) -> Kept {
        let Ok(mut r) = self.uploads.put(session).header("Content-Length", "0").header("Content-Range", &format!("bytes */{size}")).send_empty() else {
            return Kept::Unknown;
        };
        match r.status().as_u16() {
            200 | 201 => Kept::Done(r.body_mut().read_to_string().unwrap_or_default()),
            308 => Kept::From(kept(r.headers().get("range").and_then(|v| v.to_str().ok()))),
            404 | 410 => Kept::Expired,
            _ => Kept::Unknown,
        }
    }
}

/// Where to go on from after a 308: the byte after Drive's `Range: bytes=0-N`, or the start
/// when it kept nothing.
fn kept(range: Option<&str>) -> u64 {
    range.and_then(|r| r.rsplit('-').next()).and_then(|n| n.trim().parse::<u64>().ok()).map_or(0, |n| n + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};

    /// One request as a stand-in server reads it: its first line, headers (lowercased names)
    /// and up to `limit` bytes of its body.
    fn request(s: &mut BufReader<TcpStream>, limit: usize) -> (String, Vec<(String, String)>, Vec<u8>) {
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
        let len: usize = headers.iter().find(|(k, _)| k == "content-length").map_or(0, |(_, v)| v.parse().unwrap());
        let mut body = vec![0; len.min(limit)];
        s.read_exact(&mut body).unwrap();
        (line, headers, body)
    }

    fn answer(s: &mut BufReader<TcpStream>, status: &str, headers: &str, body: &str) {
        let out = format!("HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        s.get_mut().write_all(out.as_bytes()).unwrap();
    }

    fn header<'a>(h: &'a [(String, String)], name: &str) -> &'a str {
        h.iter().find(|(k, _)| k == name).map_or("", |(_, v)| v.as_str())
    }

    const DONE: &str = r#"{"id":"novo","name":"grande.bin","mimeType":"application/octet-stream","size":"7340032"}"#;

    #[test]
    fn a_big_upload_resumes_where_drive_kept_it_after_the_connection_drops() {
        let file = std::env::temp_dir().join(format!("openexplorer-upload-{}.bin", std::process::id()));
        let content: Vec<u8> = (0..7 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&file, &content).unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.local_addr().unwrap());
        let session = format!("{base}/session");
        let expected = content.clone();
        let fake = std::thread::spawn(move || {
            let next = || BufReader::new(server.accept().unwrap().0);
            // Opening the session.
            let mut s = next();
            let (line, h, meta) = request(&mut s, usize::MAX);
            assert!(line.starts_with("POST /?uploadType=resumable"), "{line}");
            assert_eq!(header(&h, "x-upload-content-length"), "7340032");
            assert!(String::from_utf8_lossy(&meta).contains("\"grande.bin\""));
            answer(&mut s, "200 OK", &format!("Location: {session}\r\n"), "");
            // The whole file in one PUT, of which this end keeps 1 MB before the line drops.
            let mut s = next();
            let (line, h, first) = request(&mut s, 1024 * 1024);
            assert!(line.starts_with("PUT /session"), "{line}");
            assert_eq!(header(&h, "content-range"), "bytes 0-7340031/7340032");
            drop(s);
            // Asked how much it kept.
            let mut s = next();
            let (_, h, _) = request(&mut s, 0);
            assert_eq!(header(&h, "content-range"), "bytes */7340032");
            answer(&mut s, "308 Resume Incomplete", "Range: bytes=0-1048575\r\n", "");
            // The rest, from there.
            let mut s = next();
            let (_, h, rest) = request(&mut s, usize::MAX);
            assert_eq!(header(&h, "content-range"), "bytes 1048576-7340031/7340032");
            assert_eq!([first, rest].concat(), expected);
            answer(&mut s, "200 OK", "Content-Type: application/json\r\n", DONE);
        });
        let d = Drive::for_tests(format!("{base}/"));
        let mut seen = Vec::new();
        let made = d.upload(&file, "pasta", "grande.bin", &mut |n| {
            seen.push(n);
            true
        });
        fake.join().unwrap();
        assert_eq!(made.unwrap().id, "novo");
        // The bar went back to what Drive kept, then to the end.
        assert!(seen.contains(&1_048_576));
        assert_eq!(seen.last(), Some(&7_340_032));
        let _ = std::fs::remove_file(file);
    }

    #[test]
    fn a_small_file_goes_in_one_request_with_its_name() {
        let file = std::env::temp_dir().join(format!("openexplorer-upload-small-{}.txt", std::process::id()));
        std::fs::write(&file, "conteúdo").unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.local_addr().unwrap());
        let fake = std::thread::spawn(move || {
            let mut s = BufReader::new(server.accept().unwrap().0);
            let (line, h, body) = request(&mut s, usize::MAX);
            assert!(line.starts_with("POST /?uploadType=multipart"), "{line}");
            assert!(header(&h, "content-type").starts_with("multipart/related; boundary="));
            let body = String::from_utf8(body).unwrap();
            assert!(body.contains(r#""name":"nota.txt""#) && body.contains(r#""parents":["pasta"]"#) && body.contains("conteúdo"), "{body}");
            answer(&mut s, "200 OK", "", r#"{"id":"n","name":"nota.txt","mimeType":"text/plain"}"#);
        });
        let d = Drive::for_tests(format!("{base}/"));
        let made = d.upload(&file, "pasta", "nota.txt", &mut |_| true).unwrap();
        fake.join().unwrap();
        assert_eq!(made.name, "nota.txt");
        let _ = std::fs::remove_file(file);
    }


    #[test]
    fn a_new_version_of_a_small_file_patches_that_file_in_one_request() {
        let file = std::env::temp_dir().join(format!("openexplorer-version-small-{}.txt", std::process::id()));
        std::fs::write(&file, "versão 2").unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.local_addr().unwrap());
        let fake = std::thread::spawn(move || {
            let mut s = BufReader::new(server.accept().unwrap().0);
            let (line, _, body) = request(&mut s, usize::MAX);
            assert!(line.starts_with("PATCH /abc_123?uploadType=multipart"), "{line}");
            let body = String::from_utf8(body).unwrap();
            // Only the content and its date: the name, folder and type stay as Drive has them.
            assert!(body.contains("versão 2") && body.contains("modifiedTime"), "{body}");
            assert!(!body.contains("parents") && !body.contains("\"name\""), "{body}");
            answer(&mut s, "200 OK", "", r#"{"id":"abc_123","name":"nota.txt","mimeType":"text/plain","size":"9"}"#);
        });
        let d = Drive::for_tests(format!("{base}/"));
        let made = d.upload_version("abc_123", &file, &mut |_| true).unwrap();
        fake.join().unwrap();
        assert_eq!(made.id, "abc_123");
        assert!(d.upload_version("../outro", &file, &mut |_| true).unwrap_err().contains("Id inválido"));
        let _ = std::fs::remove_file(file);
    }

    #[test]
    fn a_new_version_of_a_big_file_resumes_after_the_connection_drops() {
        let file = std::env::temp_dir().join(format!("openexplorer-version-big-{}.bin", std::process::id()));
        let content: Vec<u8> = (0..6 * 1024 * 1024u32).map(|i| (i % 241) as u8).collect();
        std::fs::write(&file, &content).unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.local_addr().unwrap());
        let session = format!("{base}/session");
        let expected = content.clone();
        let fake = std::thread::spawn(move || {
            let next = || BufReader::new(server.accept().unwrap().0);
            // The session is opened on the file itself.
            let mut s = next();
            let (line, h, meta) = request(&mut s, usize::MAX);
            assert!(line.starts_with("PATCH /existente?uploadType=resumable"), "{line}");
            assert_eq!(header(&h, "x-upload-content-length"), "6291456");
            assert!(!String::from_utf8_lossy(&meta).contains("parents"));
            answer(&mut s, "200 OK", &format!("Location: {session}\r\n"), "");
            // The line drops after 2 MB.
            let mut s = next();
            let (_, h, first) = request(&mut s, 2 * 1024 * 1024);
            assert_eq!(header(&h, "content-range"), "bytes 0-6291455/6291456");
            drop(s);
            let mut s = next();
            request(&mut s, 0);
            answer(&mut s, "308 Resume Incomplete", "Range: bytes=0-2097151\r\n", "");
            let mut s = next();
            let (_, h, rest) = request(&mut s, usize::MAX);
            assert_eq!(header(&h, "content-range"), "bytes 2097152-6291455/6291456");
            assert_eq!([first, rest].concat(), expected);
            answer(&mut s, "200 OK", "", r#"{"id":"existente","name":"grande.bin","mimeType":"application/octet-stream","size":"6291456"}"#);
        });
        let d = Drive::for_tests(format!("{base}/"));
        let mut seen = Vec::new();
        let made = d.upload_version("existente", &file, &mut |n| {
            seen.push(n);
            true
        });
        fake.join().unwrap();
        assert_eq!(made.unwrap().id, "existente");
        assert!(seen.contains(&2_097_152));
        assert_eq!(seen.last(), Some(&6_291_456));
        let _ = std::fs::remove_file(file);
    }
    #[test]
    fn a_resumed_upload_goes_on_after_what_drive_kept() {
        assert_eq!(kept(Some("bytes=0-262143")), 262_144);
        assert_eq!(kept(None), 0);
        assert_eq!(kept(Some("garbage")), 0);
    }

    #[test]
    fn types_come_from_the_extension_and_busy_answers_are_retried() {
        assert_eq!(mime_of("Episódio 05.mkv"), "video/x-matroska");
        assert_eq!(mime_of("foto.JPG"), "image/jpeg");
        assert_eq!(mime_of("LEIAME"), "application/octet-stream");
        assert!(transient(503, ""));
        assert!(transient(429, ""));
        assert!(transient(403, r#"{"error":{"errors":[{"reason":"userRateLimitExceeded"}]}}"#));
        assert!(!transient(403, r#"{"error":{"errors":[{"reason":"storageQuotaExceeded"}]}}"#));
        assert!(!transient(400, ""));
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// How fast this side can push a big file with nothing in the way (a local sink).
    /// `cargo test -p openexplorer-gdrive --release bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn upload_throughput_to_a_local_sink() {
        let file = std::env::temp_dir().join("openexplorer-bench.bin");
        if std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0) != 1 << 30 {
            let chunk = vec![7u8; 1 << 20];
            let mut f = File::create(&file).unwrap();
            for _ in 0..1024 {
                f.write_all(&chunk).unwrap();
            }
        }
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.local_addr().unwrap());
        let session = format!("{base}/s");
        std::thread::spawn(move || {
            for (n, conn) in server.incoming().enumerate() {
                let mut s = BufReader::with_capacity(1 << 20, conn.unwrap());
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    s.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut left = len;
                let mut buf = vec![0u8; 1 << 20];
                while left > 0 {
                    let k = s.read(&mut buf[..left.min(1 << 20)]).unwrap();
                    left -= k;
                }
                let (head, body) = if n == 0 { (format!("Location: {session}\r\n"), String::new()) } else { (String::new(), r#"{"id":"x","name":"b","mimeType":"x"}"#.to_string()) };
                let out = format!("HTTP/1.1 200 OK\r\n{head}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                s.get_mut().write_all(out.as_bytes()).unwrap();
            }
        });
        let d = Drive::for_tests(format!("{base}/"));
        let t = Instant::now();
        d.upload(&file, "p", "b.bin", &mut |_| true).unwrap();
        let secs = t.elapsed().as_secs_f64();
        println!("1 GiB em {secs:.2} s: {:.0} MB/s", 1024.0 / secs);
    }
}
