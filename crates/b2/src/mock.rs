//! A stand-in for B2 on this machine, for the tests and the benchmarks: the native API's calls
//! the crate makes, answered as B2 documents them (versions, hide markers, folders by
//! delimiter, large files, ranges), with faults to inject (expired tokens, 503s, 429s, a
//! download cut midway) and, for the benchmarks, a delay per request and a speed per connection.
//!
//! Every answer closes its connection, so each request is counted on its own.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Map, Value, json};

use crate::api::{Api, sha1_hex};

#[derive(Clone, Debug)]
pub struct Ver {
    pub id: String,
    pub bucket: String,
    pub name: String,
    pub action: &'static str,
    pub data: Arc<Vec<u8>>,
    pub sha1: String,
    pub info: Map<String, Value>,
    pub ts: i64,
    pub content_type: String,
}

impl Ver {
    fn json(&self) -> Value {
        json!({
            "accountId": "acc", "action": self.action, "bucketId": self.bucket, "contentLength": if self.action == "upload" { self.data.len() } else { 0 },
            "contentSha1": if self.action == "hide" { Value::Null } else { Value::from(self.sha1.clone()) },
            "contentType": self.content_type, "fileId": self.id, "fileInfo": self.info, "fileName": self.name, "uploadTimestamp": self.ts,
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct Large {
    pub bucket: String,
    pub name: String,
    pub info: Map<String, Value>,
    pub parts: BTreeMap<u64, (Arc<Vec<u8>>, String)>,
    pub ts: i64,
}

#[derive(Default)]
pub struct State {
    /// Id, name, lifecycle rules.
    pub buckets: Vec<(String, String, Value)>,
    pub versions: Vec<Ver>,
    pub large: HashMap<String, Large>,
    clock: i64,
    next: u64,
    pub generation: u64,
    /// Calls by name, and every request.
    pub calls: HashMap<String, usize>,
    pub requests: usize,
    /// Names the listing calls returned: what B2 bills them by (per thousand).
    pub listed: usize,
    /// What the key may reach: bucket ids (and names), and a name prefix.
    pub allowed_buckets: Option<Vec<(String, String)>>,
    pub name_prefix: Option<String>,
    pub capabilities: Vec<String>,
    pub recommended: u64,
    pub minimum: u64,
    /// What a 429 says in `Retry-After`, in seconds.
    pub retry_after: u64,
}

impl State {
    fn tick(&mut self) -> i64 {
        self.clock += 1;
        1_700_000_000_000 + self.clock
    }

    /// Unique across every stand-in of the run, as B2's ids are (the uploads this app is sending
    /// are known by id, process-wide).
    fn id(&mut self) -> String {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        self.next = NEXT.fetch_add(1, Ordering::SeqCst) + 1;
        format!("4_z{:08}", self.next)
    }

    pub fn add(&mut self, bucket: &str, name: &str, data: &[u8]) -> String {
        let id = self.id();
        let ts = self.tick();
        self.versions.push(Ver { id: id.clone(), bucket: bucket.into(), name: name.into(), action: "upload", data: Arc::new(data.to_vec()), sha1: sha1_hex(data), info: Map::new(), ts, content_type: "application/octet-stream".into() });
        id
    }

    pub fn hide(&mut self, bucket: &str, name: &str) -> Ver {
        let id = self.id();
        let ts = self.tick();
        let v = Ver { id, bucket: bucket.into(), name: name.into(), action: "hide", data: Arc::new(Vec::new()), sha1: String::new(), info: Map::new(), ts, content_type: "application/x-bz-hide-marker".into() };
        self.versions.push(v.clone());
        v
    }

    /// The newest version of each name in `bucket`, in name order.
    fn newest(&self, bucket: &str) -> BTreeMap<String, &Ver> {
        let mut out: BTreeMap<String, &Ver> = BTreeMap::new();
        for v in self.versions.iter().filter(|v| v.bucket == bucket) {
            match out.get(&v.name) {
                Some(o) if o.ts > v.ts => {}
                _ => {
                    out.insert(v.name.clone(), v);
                }
            }
        }
        out
    }

    /// What `b2_list_file_names` shows: names whose newest version is an upload.
    pub fn current(&self, bucket: &str) -> Vec<Ver> {
        self.newest(bucket).into_values().filter(|v| v.action == "upload").cloned().collect()
    }

    pub fn file(&self, bucket: &str, name: &str) -> Option<Ver> {
        self.current(bucket).into_iter().find(|v| v.name == name)
    }
}

#[derive(Default)]
pub struct Faults {
    /// The next calls answer 401 `expired_auth_token` (and the token in use stops working).
    pub expire: AtomicUsize,
    /// The next API calls answer 503.
    pub busy: AtomicUsize,
    /// The next API calls answer 429 (with `State::retry_after`).
    pub too_many: AtomicUsize,
    /// The next API calls answer 403 `transaction_cap_exceeded`.
    pub capped: AtomicUsize,
    /// The next `b2_copy_part` calls answer 400.
    pub copy_part_refused: AtomicUsize,
    /// The next `b2_list_parts` calls answer 400, as for a file finished or cancelled meanwhile.
    pub parts_gone: AtomicUsize,
    /// The next uploads (files and parts) answer 503.
    pub upload_busy: AtomicUsize,
    /// The next download sends only this many bytes, then closes.
    pub cut_download: AtomicU64,
    /// A delay before each answer, in milliseconds.
    pub latency_ms: AtomicU64,
    /// Bytes a second one connection's upload body is read at (0: as fast as it comes).
    pub upload_speed: AtomicU64,
    /// File names whose hide, copy (to the name) and version delete are refused (403), every
    /// time: a batch that fails midway, at a known item.
    pub refused: Mutex<Vec<String>>,
}

pub struct Mock {
    pub url: String,
    pub state: Arc<Mutex<State>>,
    pub faults: Arc<Faults>,
    stop: Arc<AtomicBool>,
}

pub const KEY_ID: &str = "000mockkey";
pub const KEY: &str = "K000mocksecret";

impl Mock {
    /// A B2 with buckets `fotos` and `docs`, for a key of the whole account.
    pub fn start() -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let mut st = State { recommended: 100_000_000, minimum: 5_000_000, capabilities: ["listBuckets", "listFiles", "readFiles", "writeFiles", "deleteFiles"].map(String::from).to_vec(), ..State::default() };
        st.buckets.push(("bkt_fotos".into(), "fotos".into(), json!([])));
        st.buckets.push(("bkt_docs".into(), "docs".into(), json!([{"fileNamePrefix": "", "daysFromHidingToDeleting": 1, "daysFromUploadingToHiding": null}])));
        let state = Arc::new(Mutex::new(st));
        let faults = Arc::new(Faults::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (s, f, x, u) = (state.clone(), faults.clone(), stop.clone(), url.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if x.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(conn) = conn else { continue };
                let (s, f, u) = (s.clone(), f.clone(), u.clone());
                std::thread::spawn(move || {
                    let _ = serve(conn, &s, &f, &u);
                });
            }
        });
        Mock { url, state, faults, stop }
    }

    pub fn api(&self) -> Api {
        let mut api = Api::at(&self.url, KEY_ID, KEY);
        api.backoff = Duration::from_millis(1);
        api.test_mode = None;
        api
    }

    pub fn account(&self) -> crate::Account {
        crate::Account::new(self.api(), None)
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    pub fn calls(&self, name: &str) -> usize {
        self.lock().calls.get(name).copied().unwrap_or(0)
    }

    /// Forgets the counts.
    pub fn reset_counts(&self) {
        let mut st = self.lock();
        st.calls.clear();
        st.requests = 0;
        st.listed = 0;
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the accept loop so it sees the stop.
        let _ = TcpStream::connect(self.url.trim_start_matches("http://"));
    }
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()).is_some() => {
                out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_request(s: &mut BufReader<TcpStream>, speed: u64) -> std::io::Result<Request> {
    let mut line = String::new();
    s.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        s.read_line(&mut h)?;
        if h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers.get("content-length").and_then(|l| l.parse().ok()).unwrap_or(0);
    let mut body = vec![0u8; len];
    let started = Instant::now();
    let mut at = 0;
    while at < len {
        let n = s.read(&mut body[at..(at + 64 * 1024).min(len)])?;
        if n == 0 {
            return Err(std::io::Error::other("short body"));
        }
        at += n;
        if speed > 0 {
            let due = Duration::from_secs_f64(at as f64 / speed as f64);
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                std::thread::sleep(wait);
            }
        }
    }
    let (path, q) = target.split_once('?').unwrap_or((&target, ""));
    let query = q.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), decode(v))).collect();
    Ok(Request { method, path: path.to_string(), query, headers, body })
}

fn respond(conn: &mut TcpStream, status: u16, headers: &[(&str, String)], body: &[u8]) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    conn.write_all(head.as_bytes())?;
    conn.write_all(body)?;
    conn.flush()
}

fn json_reply(conn: &mut TcpStream, status: u16, v: &Value) -> std::io::Result<()> {
    let mut headers = vec![("Content-Type", "application/json".to_string())];
    if status == 503 || status == 429 {
        headers.push(("Retry-After", "0".into()));
    }
    respond(conn, status, &headers, v.to_string().as_bytes())
}

fn error(status: u16, code: &str, message: &str) -> (u16, Value) {
    (status, json!({"status": status, "code": code, "message": message}))
}

fn take(n: &AtomicUsize) -> bool {
    let mut v = n.load(Ordering::SeqCst);
    while v > 0 {
        match n.compare_exchange(v, v - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(now) => v = now,
        }
    }
    false
}

fn serve(conn: TcpStream, state: &Mutex<State>, faults: &Faults, url: &str) -> std::io::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut conn = conn;
    let speed = faults.upload_speed.load(Ordering::SeqCst);
    let req = read_request(&mut reader, speed)?;
    let latency = faults.latency_ms.load(Ordering::SeqCst);
    if latency > 0 {
        std::thread::sleep(Duration::from_millis(latency));
    }
    let call = req.path.rsplit('/').next().unwrap_or("").to_string();
    {
        let mut st = state.lock().unwrap();
        st.requests += 1;
        let name = if req.path.starts_with("/upload/") {
            "b2_upload_file".to_string()
        } else if req.path.starts_with("/upload_part/") {
            "b2_upload_part".to_string()
        } else if req.path.starts_with("/file/") {
            "b2_download_file_by_name".to_string()
        } else {
            call.clone()
        };
        *st.calls.entry(name).or_default() += 1;
    }
    if req.path == "/b2api/v4/b2_authorize_account" {
        let want = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{KEY_ID}:{KEY}")));
        if req.headers.get("authorization") != Some(&want) {
            let (s, v) = error(401, "unauthorized", "");
            return json_reply(&mut conn, s, &v);
        }
        let mut st = state.lock().unwrap();
        st.generation += 1;
        let buckets = st.allowed_buckets.as_ref().map(|b| b.iter().map(|(id, name)| json!({"id": id, "name": name})).collect::<Vec<_>>());
        let v = json!({
            "accountId": "acc", "authorizationToken": format!("tok{}", st.generation), "applicationKeyExpirationTimestamp": null,
            "apiInfo": {"storageApi": {"apiUrl": url, "downloadUrl": url, "recommendedPartSize": st.recommended, "absoluteMinimumPartSize": st.minimum, "s3ApiUrl": url,
                "allowed": {"buckets": buckets, "capabilities": st.capabilities, "namePrefix": st.name_prefix}}},
        });
        drop(st);
        return json_reply(&mut conn, 200, &v);
    }
    let token_ok = {
        let st = state.lock().unwrap();
        let auth = req.headers.get("authorization").cloned().unwrap_or_default();
        auth == format!("tok{}", st.generation) || (auth.starts_with("up") && (req.path.starts_with("/upload")))
    };
    if take(&faults.expire) {
        state.lock().unwrap().generation += 1;
        let (s, v) = error(401, "expired_auth_token", "");
        return json_reply(&mut conn, s, &v);
    }
    if !token_ok {
        let (s, v) = error(401, "bad_auth_token", "");
        return json_reply(&mut conn, s, &v);
    }
    if req.path.starts_with("/upload") {
        if take(&faults.upload_busy) {
            let (s, v) = error(503, "service_unavailable", "");
            return json_reply(&mut conn, s, &v);
        }
        let (s, v) = upload(&req, state);
        return json_reply(&mut conn, s, &v);
    }
    if req.method == "GET" && (req.path.starts_with("/file/") || call == "b2_download_file_by_id") {
        return download(&mut conn, &req, state, faults);
    }
    if take(&faults.busy) {
        let (s, v) = error(503, "service_unavailable", "");
        return json_reply(&mut conn, s, &v);
    }
    if take(&faults.too_many) {
        let (s, v) = error(429, "too_many_requests", "");
        let wait = state.lock().unwrap().retry_after;
        return respond(&mut conn, s, &[("Content-Type", "application/json".into()), ("Retry-After", wait.to_string())], v.to_string().as_bytes());
    }
    if call == "b2_list_parts" && take(&faults.parts_gone) {
        let (s, v) = error(400, "bad_request", "no such file");
        return json_reply(&mut conn, s, &v);
    }
    if call == "b2_copy_part" && take(&faults.copy_part_refused) {
        let (s, v) = error(400, "bad_request", "range out of the file");
        return json_reply(&mut conn, s, &v);
    }
    if take(&faults.capped) {
        let (s, v) = error(403, "transaction_cap_exceeded", "Transaction cap exceeded");
        return json_reply(&mut conn, s, &v);
    }
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
    if matches!(call.as_str(), "b2_hide_file" | "b2_copy_file" | "b2_delete_file_version") && faults.refused.lock().unwrap().iter().any(|n| n == s(&body, "fileName")) {
        let (s, v) = error(403, "access_denied", "refused for the test");
        return json_reply(&mut conn, s, &v);
    }
    let (s, v) = api(&call, &body, state, url);
    json_reply(&mut conn, s, &v)
}

fn upload(req: &Request, state: &Mutex<State>) -> (u16, Value) {
    let sha1 = req.headers.get("x-bz-content-sha1").cloned().unwrap_or_default();
    if sha1_hex(&req.body) != sha1 {
        return error(400, "bad_request", "sha1 did not match data received");
    }
    let mut st = state.lock().unwrap();
    if let Some(bucket) = req.path.strip_prefix("/upload/") {
        let name = decode(req.headers.get("x-bz-file-name").map(String::as_str).unwrap_or(""));
        let id = st.add(bucket, &name, &req.body);
        let v = st.versions.iter_mut().find(|v| v.id == id).unwrap();
        // File info comes in X-Bz-Info-* headers (here lowercased, as read).
        for (k, value) in &req.headers {
            if let Some(key) = k.strip_prefix("x-bz-info-") {
                v.info.insert(key.to_string(), decode(value).into());
            }
        }
        return (200, v.json());
    }
    let file_id = req.path.trim_start_matches("/upload_part/").to_string();
    let n: u64 = req.headers.get("x-bz-part-number").and_then(|n| n.parse().ok()).unwrap_or(0);
    let Some(large) = st.large.get_mut(&file_id) else { return error(400, "bad_request", "no such file") };
    large.parts.insert(n, (Arc::new(req.body.clone()), sha1.clone()));
    (200, json!({"fileId": file_id, "partNumber": n, "contentLength": req.body.len(), "contentSha1": sha1, "uploadTimestamp": 1}))
}

fn download(conn: &mut TcpStream, req: &Request, state: &Mutex<State>, faults: &Faults) -> std::io::Result<()> {
    let found = {
        let st = state.lock().unwrap();
        if let Some(rest) = req.path.strip_prefix("/file/") {
            let (bucket, name) = rest.split_once('/').unwrap_or((rest, ""));
            let bucket = decode(bucket);
            let id = st.buckets.iter().find(|b| b.1 == bucket).map(|b| b.0.clone()).unwrap_or_default();
            st.file(&id, &decode(name))
        } else {
            let id = req.query.get("fileId").cloned().unwrap_or_default();
            st.versions.iter().find(|v| v.id == id && v.action == "upload").cloned()
        }
    };
    let Some(v) = found else {
        let (s, b) = error(404, "not_found", "");
        return json_reply(conn, s, &b);
    };
    let from: usize = req.headers.get("range").and_then(|r| r.strip_prefix("bytes=")).and_then(|r| r.trim_end_matches('-').parse().ok()).unwrap_or(0);
    let status = if from > 0 { 206 } else { 200 };
    let body = &v.data[from.min(v.data.len())..];
    let sha1 = if v.content_type == "large" { "none".to_string() } else { v.sha1.clone() };
    let mut head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\nX-Bz-File-Id: {}\r\nX-Bz-Content-Sha1: {sha1}\r\n", body.len(), v.id);
    if from > 0 {
        head.push_str(&format!("Content-Range: bytes {from}-{}/{}\r\n", v.data.len() - 1, v.data.len()));
    }
    head.push_str("\r\n");
    conn.write_all(head.as_bytes())?;
    let cut = faults.cut_download.swap(0, Ordering::SeqCst);
    if cut > 0 && (cut as usize) < body.len() {
        conn.write_all(&body[..cut as usize])?;
        conn.flush()?;
        return conn.shutdown(std::net::Shutdown::Both);
    }
    conn.write_all(body)?;
    conn.flush()
}

fn s<'a>(b: &'a Value, k: &str) -> &'a str {
    b.get(k).and_then(Value::as_str).unwrap_or("")
}

fn api(call: &str, b: &Value, state: &Mutex<State>, url: &str) -> (u16, Value) {
    let mut st = state.lock().unwrap();
    // A key of some buckets reaches only those; a prefix limits names.
    if let Some(allowed) = &st.allowed_buckets {
        let id = s(b, "bucketId");
        if !id.is_empty() && !allowed.iter().any(|(a, _)| a == id) {
            return error(401, "unauthorized", "bucket not allowed");
        }
        if call == "b2_list_buckets" && id.is_empty() && s(b, "bucketName").is_empty() {
            return error(401, "unauthorized", "listing all buckets");
        }
    }
    if let Some(np) = st.name_prefix.clone() {
        let p = b.get("prefix").or_else(|| b.get("namePrefix")).and_then(Value::as_str);
        if let Some(p) = p
            && !p.starts_with(&np)
        {
            return error(401, "unauthorized", "prefix outside the key's");
        }
    }
    let count = b.get("maxFileCount").and_then(Value::as_u64).filter(|n| *n > 0).unwrap_or(100) as usize;
    match call {
        "b2_list_buckets" => {
            let list: Vec<Value> = st
                .buckets
                .iter()
                .filter(|(id, name, _)| (s(b, "bucketId").is_empty() || s(b, "bucketId") == id) && (s(b, "bucketName").is_empty() || s(b, "bucketName") == name))
                .map(|(id, name, rules)| json!({"accountId": "acc", "bucketId": id, "bucketName": name, "bucketType": "allPrivate", "lifecycleRules": rules}))
                .collect();
            (200, json!({"buckets": list}))
        }
        "b2_list_file_names" => {
            let (prefix, delim, start) = (s(b, "prefix"), b.get("delimiter").and_then(Value::as_str), s(b, "startFileName"));
            let mut entries: BTreeMap<String, Value> = BTreeMap::new();
            for v in st.current(s(b, "bucketId")) {
                let Some(rest) = v.name.strip_prefix(prefix) else { continue };
                match delim.and_then(|d| rest.find(d)) {
                    Some(i) => {
                        let folder = format!("{prefix}{}", &rest[..=i]);
                        entries.entry(folder.clone()).or_insert_with(|| json!({"action": "folder", "fileName": folder, "fileId": null, "contentLength": 0, "uploadTimestamp": 0}));
                    }
                    None => {
                        entries.insert(v.name.clone(), v.json());
                    }
                }
            }
            let all: Vec<(String, Value)> = entries.into_iter().filter(|(n, _)| n.as_str() >= start).collect();
            let next = all.get(count).map(|(n, _)| n.clone());
            let files: Vec<Value> = all.into_iter().take(count).map(|(_, v)| v).collect();
            st.listed += files.len();
            (200, json!({"files": files, "nextFileName": next}))
        }
        "b2_list_file_versions" => {
            let (prefix, start, start_id) = (s(b, "prefix"), s(b, "startFileName"), s(b, "startFileId"));
            let mut all: Vec<Ver> = st.versions.iter().filter(|v| v.bucket == s(b, "bucketId") && v.name.starts_with(prefix) && v.name.as_str() >= start).cloned().collect();
            // Unfinished large files show among the versions, as "start".
            for (id, l) in &st.large {
                if l.bucket == s(b, "bucketId") && l.name.starts_with(prefix) && l.name.as_str() >= start {
                    all.push(Ver { id: id.clone(), bucket: l.bucket.clone(), name: l.name.clone(), action: "start", data: Arc::new(Vec::new()), sha1: "none".into(), info: l.info.clone(), ts: l.ts, content_type: "b2/x-auto".into() });
                }
            }
            all.sort_by(|x, y| x.name.cmp(&y.name).then(y.ts.cmp(&x.ts)));
            if !start_id.is_empty()
                && let Some(i) = all.iter().position(|v| v.id == start_id)
            {
                all.drain(..i);
            }
            let next = all.get(count).map(|v| (v.name.clone(), v.id.clone()));
            let files: Vec<Value> = all.iter().take(count).map(Ver::json).collect();
            st.listed += files.len();
            (200, json!({"files": files, "nextFileName": next.as_ref().map(|n| n.0.clone()), "nextFileId": next.map(|n| n.1)}))
        }
        "b2_get_upload_url" => {
            let n = st.tick();
            (200, json!({"bucketId": s(b, "bucketId"), "uploadUrl": format!("{url}/upload/{}", s(b, "bucketId")), "authorizationToken": format!("up{n}")}))
        }
        "b2_get_upload_part_url" => {
            let n = st.tick();
            (200, json!({"fileId": s(b, "fileId"), "uploadUrl": format!("{url}/upload_part/{}", s(b, "fileId")), "authorizationToken": format!("up{n}")}))
        }
        "b2_hide_file" => {
            let (bucket, name) = (s(b, "bucketId").to_string(), s(b, "fileName").to_string());
            if st.file(&bucket, &name).is_none() {
                return error(404, "not_found", "");
            }
            (200, st.hide(&bucket, &name).json())
        }
        "b2_delete_file_version" => {
            // Deleting an unfinished large file cancels it.
            if st.large.get(s(b, "fileId")).is_some_and(|l| l.name == s(b, "fileName")) {
                st.large.remove(s(b, "fileId"));
                return (200, json!({"fileId": s(b, "fileId"), "fileName": s(b, "fileName")}));
            }
            let before = st.versions.len();
            st.versions.retain(|v| !(v.id == s(b, "fileId") && v.name == s(b, "fileName")));
            if st.versions.len() == before {
                return error(400, "file_not_present", "");
            }
            (200, json!({"fileId": s(b, "fileId"), "fileName": s(b, "fileName")}))
        }
        "b2_copy_file" => {
            let Some(src) = st.versions.iter().find(|v| v.id == s(b, "sourceFileId") && v.action == "upload").cloned() else { return error(404, "not_found", "") };
            let bucket = if s(b, "destinationBucketId").is_empty() { src.bucket.clone() } else { s(b, "destinationBucketId").to_string() };
            let id = st.id();
            let ts = st.tick();
            let v = Ver { id, bucket, name: s(b, "fileName").into(), ts, ..src };
            st.versions.push(v.clone());
            (200, v.json())
        }
        "b2_get_file_info" => match st.versions.iter().find(|v| v.id == s(b, "fileId")) {
            Some(v) => (200, v.json()),
            None => error(404, "not_found", ""),
        },
        "b2_start_large_file" => {
            let id = st.id();
            let ts = st.tick();
            let info = b.get("fileInfo").and_then(Value::as_object).cloned().unwrap_or_default();
            st.large.insert(id.clone(), Large { bucket: s(b, "bucketId").into(), name: s(b, "fileName").into(), info: info.clone(), parts: BTreeMap::new(), ts });
            (200, json!({"fileId": id, "fileName": s(b, "fileName"), "bucketId": s(b, "bucketId"), "action": "start", "fileInfo": info, "uploadTimestamp": ts, "contentLength": 0, "contentSha1": "none"}))
        }
        // Oldest first, at most 100 a page, from `startFileId` (as B2 documents).
        "b2_list_unfinished_large_files" => {
            let mut all: Vec<(&String, &Large)> = st.large.iter().filter(|(_, l)| l.bucket == s(b, "bucketId") && l.name.starts_with(s(b, "namePrefix"))).collect();
            all.sort_by_key(|(id, l)| (l.ts, (*id).clone()));
            if let Some(i) = all.iter().position(|(id, _)| id.as_str() == s(b, "startFileId")) {
                all.drain(..i);
            }
            let count = count.min(100);
            let next = all.get(count).map(|(id, _)| (*id).clone());
            let files: Vec<Value> = all
                .iter()
                .take(count)
                .map(|(id, l)| json!({"fileId": id, "fileName": l.name, "bucketId": l.bucket, "action": "start", "fileInfo": l.info, "uploadTimestamp": l.ts, "contentLength": 0, "contentSha1": "none"}))
                .collect();
            (200, json!({"files": files, "nextFileId": next}))
        }
        "b2_list_parts" => match st.large.get(s(b, "fileId")) {
            Some(l) => {
                let from = b.get("startPartNumber").and_then(Value::as_u64).unwrap_or(1);
                let most = b.get("maxPartCount").and_then(Value::as_u64).unwrap_or(1000).min(1000) as usize;
                let all: Vec<_> = l.parts.range(from..).collect();
                let next = all.get(most).map(|(n, _)| **n);
                let parts: Vec<Value> = all.iter().take(most).map(|(n, (d, sha1))| json!({"fileId": s(b, "fileId"), "partNumber": n, "contentLength": d.len(), "contentSha1": sha1, "uploadTimestamp": 1})).collect();
                (200, json!({"parts": parts, "nextPartNumber": next}))
            }
            None => error(400, "bad_request", "no such file"),
        },
        "b2_cancel_large_file" => match st.large.remove(s(b, "fileId")) {
            Some(l) => (200, json!({"fileId": s(b, "fileId"), "fileName": l.name, "bucketId": l.bucket, "accountId": "acc"})),
            None => error(400, "bad_request", "no such file"),
        },
        "b2_copy_part" => {
            let Some(src) = st.versions.iter().find(|v| v.id == s(b, "sourceFileId")).cloned() else { return error(404, "not_found", "") };
            let range = s(b, "range").trim_start_matches("bytes=");
            let (from, to) = range.split_once('-').map(|(a, z)| (a.parse::<usize>().unwrap(), z.parse::<usize>().unwrap())).unwrap_or((0, src.data.len() - 1));
            let data = src.data[from..=to].to_vec();
            let sha1 = sha1_hex(&data);
            let n = b.get("partNumber").and_then(Value::as_u64).unwrap_or(0);
            let Some(l) = st.large.get_mut(s(b, "largeFileId")) else { return error(400, "bad_request", "no such file") };
            l.parts.insert(n, (Arc::new(data.clone()), sha1.clone()));
            (200, json!({"fileId": s(b, "largeFileId"), "partNumber": n, "contentLength": data.len(), "contentSha1": sha1, "uploadTimestamp": 1}))
        }
        "b2_finish_large_file" => {
            let id = s(b, "fileId").to_string();
            let Some(l) = st.large.get(&id).cloned() else { return error(400, "bad_request", "no such file") };
            let want: Vec<&str> = b.get("partSha1Array").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
            let have: Vec<&str> = l.parts.values().map(|(_, s)| s.as_str()).collect();
            if want != have || l.parts.keys().copied().ne(1..=l.parts.len() as u64) || l.parts.len() < 2 {
                return error(400, "bad_request", "parts don't match");
            }
            let data: Vec<u8> = l.parts.values().flat_map(|(d, _)| d.iter().copied()).collect();
            st.large.remove(&id);
            let ts = st.tick();
            let v = Ver { id, bucket: l.bucket, name: l.name, action: "upload", sha1: "none".into(), data: Arc::new(data), info: l.info, ts, content_type: "large".into() };
            st.versions.push(v.clone());
            (200, v.json())
        }
        _ => error(400, "bad_request", &format!("unknown call {call}")),
    }
}
