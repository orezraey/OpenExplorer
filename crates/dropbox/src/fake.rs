//! A stand-in for the Dropbox API on this machine, for tests and benchmarks: an account's files
//! in memory, answering the routes the crate calls the way the API reference says they answer
//! (metadata, 409 route errors with `error_summary`, cursors and their change log, `reset`,
//! longpoll, upload sessions and their offsets, Range downloads, revisions, shared links, 429
//! with `Retry-After`). It also drops connections on purpose, to test resuming.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::account::Account;
use crate::api::{Api, Hosts};
use crate::meta::{last, parent, timestamp};

#[derive(Clone)]
struct Node {
    display: String,
    dir: bool,
    data: Arc<Vec<u8>>,
    rev: String,
}

#[derive(Default)]
struct State {
    nodes: BTreeMap<String, Node>,
    /// Every revision a file had: rev → (display path, bytes).
    revs: HashMap<String, (String, Arc<Vec<u8>>)>,
    /// Files deleted: lower → (last rev, when).
    deleted: HashMap<String, (String, i64)>,
    /// The change log: each item's metadata as it became.
    log: Vec<(String, Value)>,
    epoch: u64,
    next: u64,
    /// Listing pages still to give: cursor → entries.
    pages: HashMap<String, (Vec<Value>, String)>,
    sessions: HashMap<String, Vec<u8>>,
    jobs: HashMap<String, Value>,
    links: HashMap<String, String>,
    team: Option<(String, String)>,
    last_root: Option<String>,
    calls: Vec<String>,
    throttle: usize,
    throttled: usize,
    drop_append: bool,
    cut_download: Option<u64>,
    ranges: Vec<(String, u64)>,
    tokens: u64,
}

/// The stand-in, serving until dropped.
pub struct Fake {
    state: Arc<(Mutex<State>, Condvar)>,
    pub base: String,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn meta(lower: &str, n: &Node) -> Value {
    if n.dir {
        json!({".tag": "folder", "name": last(&n.display), "path_lower": lower, "path_display": n.display, "id": format!("id:{lower}")})
    } else {
        let t = timestamp(1_700_000_000);
        json!({".tag": "file", "name": last(&n.display), "path_lower": lower, "path_display": n.display, "id": format!("id:{lower}"), "client_modified": t, "server_modified": t, "rev": n.rev, "size": n.data.len(), "content_hash": crate::content::of_reader(&mut &n.data[..], &|| false).unwrap()})
    }
}

/// Whether `lower` is in what a cursor on `path` (recursive or not) follows.
fn under(lower: &str, path: &str, recursive: bool) -> bool {
    if recursive { path.is_empty() || lower.starts_with(&format!("{path}/")) } else { parent(lower) == path }
}

impl State {
    fn rev(&mut self) -> String {
        self.next += 1;
        format!("{:09x}", 0x1_0000_0000u64 + self.next)
    }

    fn logged(&mut self, lower: &str) {
        let m = match self.nodes.get(lower) {
            Some(n) => meta(lower, n),
            None => json!({".tag": "deleted", "name": last(lower), "path_lower": lower, "path_display": lower}),
        };
        self.log.push((lower.to_string(), m));
    }

    /// Makes the folders down to `display`'s parent.
    fn parents(&mut self, display: &str) {
        let mut at = String::new();
        let segs: Vec<&str> = display.split('/').filter(|s| !s.is_empty()).collect();
        for s in &segs[..segs.len().saturating_sub(1)] {
            at.push('/');
            at.push_str(s);
            let lower = at.to_lowercase();
            if !self.nodes.contains_key(&lower) {
                self.nodes.insert(lower.clone(), Node { display: at.clone(), dir: true, data: Arc::default(), rev: String::new() });
                self.deleted.remove(&lower);
                self.logged(&lower);
            }
        }
    }

    fn put(&mut self, display: &str, data: Vec<u8>) -> Value {
        self.parents(display);
        let lower = display.to_lowercase();
        let rev = self.rev();
        let data = Arc::new(data);
        self.revs.insert(rev.clone(), (display.to_string(), data.clone()));
        self.nodes.insert(lower.clone(), Node { display: display.to_string(), dir: false, data, rev });
        self.deleted.remove(&lower);
        self.logged(&lower);
        meta(&lower, &self.nodes[&lower])
    }

    fn subtree(&self, lower: &str) -> Vec<String> {
        let inside = format!("{lower}/");
        self.nodes.keys().filter(|k| k.as_str() == lower || k.starts_with(&inside)).cloned().collect()
    }

    fn delete(&mut self, lower: &str) -> Option<Value> {
        let before = meta(lower, self.nodes.get(lower)?);
        for k in self.subtree(lower) {
            let n = self.nodes.remove(&k).unwrap();
            if !n.dir {
                self.deleted.insert(k.clone(), (n.rev, now()));
            }
            self.logged(&k);
        }
        Some(before)
    }

    fn cursor(&self, path: &str, recursive: bool) -> String {
        json!([self.epoch, self.log.len(), path, recursive]).to_string()
    }
}

/// A route's error, as Dropbox sends it (409).
fn route_error(summary: &str, error: Value) -> (u16, Vec<(String, String)>, Vec<u8>) {
    (409, vec![], json!({"error_summary": summary, "error": error}).to_string().into_bytes())
}

fn ok(v: Value) -> (u16, Vec<(String, String)>, Vec<u8>) {
    (200, vec![], v.to_string().into_bytes())
}

fn not_found(what: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
    route_error(&format!("{what}/not_found/.."), json!({".tag": what, what: {".tag": "not_found"}}))
}

impl Fake {
    pub fn start() -> Fake {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.local_addr().unwrap());
        let state: Arc<(Mutex<State>, Condvar)> = Arc::default();
        let st = Arc::downgrade(&state);
        std::thread::spawn(move || {
            for conn in server.incoming() {
                let Some(st) = st.upgrade() else { return };
                let Ok(conn) = conn else { continue };
                std::thread::spawn(move || serve(&st, conn));
            }
        });
        Fake { state, base }
    }

    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.0.lock().unwrap()
    }

    fn changed(&self) {
        self.state.1.notify_all();
    }

    /// An account on the stand-in, as the app opens one (its access renewed from a refresh
    /// token), keeping nothing on disk.
    pub fn account(&self) -> Account {
        Account::open(self.api(), None).unwrap()
    }

    pub fn api(&self) -> Api {
        Api::new(Hosts::all(&self.base), "chave", "renovacao").short_waits()
    }

    /// [`Fake::account`] keeping its listings in `file`.
    pub fn account_kept(&self, file: &Path) -> Account {
        Account::open(self.api(), Some(file.to_path_buf())).unwrap()
    }

    pub fn put_file(&self, display: &str, data: &[u8]) {
        self.st().put(display, data.to_vec());
        self.changed();
    }

    /// `n` small files in a new folder.
    pub fn put_many(&self, folder: &str, n: usize) {
        let mut st = self.st();
        for i in 0..n {
            st.put(&format!("{folder}/IMG_{i:06}.jpg"), vec![(i % 251) as u8; 16]);
        }
        drop(st);
        self.changed();
    }

    pub fn remove(&self, display: &str) {
        self.st().delete(&display.to_lowercase());
        self.changed();
    }

    pub fn has(&self, display: &str) -> bool {
        self.st().nodes.contains_key(&display.to_lowercase())
    }

    pub fn content(&self, display: &str) -> Vec<u8> {
        self.st().nodes[&display.to_lowercase()].data.to_vec()
    }

    /// Every cursor given so far stops working (`reset`).
    pub fn reset_cursors(&self) {
        self.st().epoch += 1;
    }

    pub fn set_team(&self, root: &str, home: &str) {
        self.st().team = Some((root.into(), home.into()));
    }

    pub fn last_root(&self) -> Option<String> {
        self.st().last_root.clone()
    }

    /// The next `n` calls are answered 429.
    pub fn throttle(&self, n: usize) {
        self.st().throttle = n;
    }

    pub fn throttled(&self) -> usize {
        self.st().throttled
    }

    pub fn calls(&self) -> usize {
        self.st().calls.len()
    }

    pub fn calls_since(&self, before: &usize) -> Vec<String> {
        self.st().calls[*before..].iter().filter(|c| c.as_str() != "oauth2/token").cloned().collect()
    }

    pub fn drop_next_append(&self) {
        self.st().drop_append = true;
    }

    pub fn cut_next_download(&self, at: u64) {
        self.st().cut_download = Some(at);
    }

    pub fn ranges(&self) -> Vec<(String, u64)> {
        self.st().ranges.clone()
    }
}

struct Request {
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_request(conn: &TcpStream) -> Option<Request> {
    let mut r = BufReader::new(conn);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let path = line.split_whitespace().nth(1)?.to_string();
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        r.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let mut body = Vec::new();
    if let Some(len) = headers.get("content-length").and_then(|v| v.parse::<usize>().ok()) {
        body.resize(len, 0);
        r.read_exact(&mut body).ok()?;
    } else if headers.get("transfer-encoding").is_some_and(|v| v.contains("chunked")) {
        loop {
            let mut size = String::new();
            r.read_line(&mut size).ok()?;
            let n = usize::from_str_radix(size.trim(), 16).ok()?;
            let mut chunk = vec![0; n + 2];
            r.read_exact(&mut chunk).ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
    }
    Some(Request { path, headers, body })
}

fn respond(conn: &mut TcpStream, status: u16, headers: &[(String, String)], body: &[u8]) {
    let mut head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = conn.write_all(head.as_bytes());
    let _ = conn.write_all(body);
}

fn serve(state: &Arc<(Mutex<State>, Condvar)>, mut conn: TcpStream) {
    let Some(req) = read_request(&conn) else { return };
    let route = req.path.trim_start_matches("/2/").trim_start_matches('/').to_string();
    let arg: Value = match req.headers.get("dropbox-api-arg") {
        Some(a) => serde_json::from_str(a).unwrap_or(Value::Null),
        None => serde_json::from_slice(&req.body).unwrap_or(Value::Null),
    };
    let mut st = state.0.lock().unwrap();
    st.calls.push(route.clone());
    if route != "oauth2/token" && route != "files/list_folder/longpoll" {
        if st.throttle > 0 {
            st.throttle -= 1;
            st.throttled += 1;
            drop(st);
            let body = json!({"error_summary": "too_many_requests/..", "error": {"reason": {".tag": "too_many_requests"}, "retry_after": 1}}).to_string();
            return respond(&mut conn, 429, &[("Retry-After".into(), "1".into())], body.as_bytes());
        }
        let token = req.headers.get("authorization").cloned().unwrap_or_default();
        if !token.starts_with("Bearer tok-") {
            drop(st);
            return respond(&mut conn, 401, &[], json!({"error_summary": "invalid_access_token/..", "error": {".tag": "invalid_access_token"}}).to_string().as_bytes());
        }
        st.last_root = req.headers.get("dropbox-api-path-root").cloned();
    }
    let s = |k: &str| arg.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let lower = |p: String| p.to_lowercase();
    let (status, headers, body): (u16, Vec<(String, String)>, Vec<u8>) = match route.as_str() {
        "oauth2/token" => {
            st.tokens += 1;
            ok(json!({"access_token": format!("tok-{}", st.tokens), "expires_in": 14400, "token_type": "bearer"}))
        }
        "users/get_current_account" => {
            let (root, home) = st.team.clone().unwrap_or(("ns-home".into(), "ns-home".into()));
            let tag = if st.team.is_some() { "team" } else { "user" };
            ok(json!({"account_id": "dbid:FAKE", "email": "Ana@Exemplo.com", "name": {"display_name": "Ana Souza"}, "root_info": {".tag": tag, "root_namespace_id": root, "home_namespace_id": home}}))
        }
        "users/get_space_usage" => {
            let used: usize = st.nodes.values().map(|n| n.data.len()).sum();
            if st.team.is_some() {
                ok(json!({"used": used, "allocation": {".tag": "team", "used": used, "allocated": 5u64 << 40, "user_within_team_space_allocated": 0}}))
            } else {
                ok(json!({"used": used, "allocation": {".tag": "individual", "allocated": 2u64 << 30}}))
            }
        }
        "auth/token/revoke" => ok(Value::Null),
        "files/list_folder" => {
            let path = lower(s("path"));
            let recursive = arg["recursive"].as_bool().unwrap_or(false);
            let deleted = arg["include_deleted"].as_bool().unwrap_or(false);
            if !path.is_empty() && !st.nodes.get(&path).is_some_and(|n| n.dir) {
                not_found("path")
            } else {
                let mut entries: Vec<Value> = st.nodes.iter().filter(|(k, _)| under(k, &path, recursive)).map(|(k, n)| meta(k, n)).collect();
                if deleted {
                    entries.extend(st.deleted.keys().filter(|k| under(k, &path, recursive) && !st.nodes.contains_key(*k)).map(|k| json!({".tag": "deleted", "name": last(k), "path_lower": k, "path_display": st.revs.values().find(|(d, _)| d.to_lowercase() == **k).map(|(d, _)| d.clone()).unwrap_or(k.clone())})));
                }
                let end = st.cursor(&path, recursive);
                let limit = arg["limit"].as_u64().unwrap_or(2000) as usize;
                let rest = entries.split_off(entries.len().min(limit));
                if rest.is_empty() {
                    ok(json!({"entries": entries, "cursor": end, "has_more": false}))
                } else {
                    st.next += 1;
                    let id = format!("page-{}", st.next);
                    st.pages.insert(id.clone(), (rest, end));
                    ok(json!({"entries": entries, "cursor": id, "has_more": true}))
                }
            }
        }
        "files/list_folder/get_latest_cursor" => {
            let c = st.cursor(&lower(s("path")), arg["recursive"].as_bool().unwrap_or(false));
            ok(json!({"cursor": c}))
        }
        "files/list_folder/continue" => {
            let cursor = s("cursor");
            if let Some((mut rest, end)) = st.pages.remove(&cursor) {
                let more = rest.split_off(rest.len().min(2000));
                if more.is_empty() {
                    ok(json!({"entries": rest, "cursor": end, "has_more": false}))
                } else {
                    st.pages.insert(cursor.clone(), (more, end));
                    ok(json!({"entries": rest, "cursor": cursor, "has_more": true}))
                }
            } else {
                let c: Value = serde_json::from_str(&cursor).unwrap_or(Value::Null);
                let (epoch, at, path, recursive) = (c[0].as_u64().unwrap_or(u64::MAX), c[1].as_u64().unwrap_or(0) as usize, c[2].as_str().unwrap_or("").to_string(), c[3].as_bool().unwrap_or(false));
                if epoch != st.epoch {
                    route_error("reset/..", json!({".tag": "reset"}))
                } else {
                    let mut entries = Vec::new();
                    let mut i = at;
                    while i < st.log.len() && entries.len() < 2000 {
                        if under(&st.log[i].0, &path, recursive) {
                            entries.push(st.log[i].1.clone());
                        }
                        i += 1;
                    }
                    let more = i < st.log.len();
                    ok(json!({"entries": entries, "cursor": json!([epoch, i, path, recursive]).to_string(), "has_more": more}))
                }
            }
        }
        "files/list_folder/longpoll" => {
            let c: Value = serde_json::from_str(&s("cursor")).unwrap_or(Value::Null);
            let (at, path, recursive) = (c[1].as_u64().unwrap_or(0) as usize, c[2].as_str().unwrap_or("").to_string(), c[3].as_bool().unwrap_or(false));
            let until = Instant::now() + Duration::from_secs(arg["timeout"].as_u64().unwrap_or(30));
            loop {
                if st.log[at.min(st.log.len())..].iter().any(|(k, _)| under(k, &path, recursive)) {
                    break ok(json!({"changes": true}));
                }
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break ok(json!({"changes": false}));
                }
                st = state.1.wait_timeout(st, left).unwrap().0;
            }
        }
        "files/get_metadata" => match st.nodes.get(&lower(s("path"))) {
            Some(n) => ok(meta(&lower(s("path")), n)),
            None => not_found("path"),
        },
        "files/create_folder_v2" => {
            let (display, l) = (s("path"), lower(s("path")));
            if st.nodes.contains_key(&l) {
                route_error("path/conflict/folder/..", json!({".tag": "path", "path": {".tag": "conflict", "conflict": {".tag": "folder"}}}))
            } else {
                st.parents(&display);
                st.nodes.insert(l.clone(), Node { display, dir: true, data: Arc::default(), rev: String::new() });
                st.logged(&l);
                ok(json!({"metadata": meta(&l, &st.nodes[&l])}))
            }
        }
        "files/move_v2" | "files/copy_v2" => {
            let (from, to) = (lower(s("from_path")), s("to_path"));
            let to_l = to.to_lowercase();
            if !st.nodes.contains_key(&from) {
                not_found("from_lookup")
            } else if st.nodes.contains_key(&to_l) {
                route_error("to/conflict/file/..", json!({".tag": "to", "to": {".tag": "conflict"}}))
            } else {
                let moving = route == "files/move_v2";
                st.parents(&to);
                for k in st.subtree(&from) {
                    let mut n = st.nodes[&k].clone();
                    let new_display = format!("{to}{}", &n.display[from.len()..]);
                    n.display = new_display.clone();
                    let new_l = new_display.to_lowercase();
                    if !n.dir {
                        n.rev = st.rev();
                        st.revs.insert(n.rev.clone(), (new_display, n.data.clone()));
                    }
                    st.nodes.insert(new_l.clone(), n);
                    st.logged(&new_l);
                    if moving {
                        st.nodes.remove(&k);
                        st.logged(&k);
                    }
                }
                ok(json!({"metadata": meta(&to_l, &st.nodes[&to_l])}))
            }
        }
        "files/delete_v2" => match st.delete(&lower(s("path"))) {
            Some(m) => ok(json!({"metadata": m})),
            None => not_found("path_lookup"),
        },
        "files/delete_batch" => {
            let results: Vec<Value> = arg["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| match st.delete(&e["path"].as_str().unwrap().to_lowercase()) {
                    Some(m) => json!({".tag": "success", "metadata": m}),
                    None => json!({".tag": "failure", "failure": {".tag": "path_lookup", "path_lookup": {".tag": "not_found"}}}),
                })
                .collect();
            st.next += 1;
            let id = format!("job-{}", st.next);
            st.jobs.insert(id.clone(), json!({".tag": "complete", "entries": results}));
            ok(json!({".tag": "async_job_id", "async_job_id": id}))
        }
        "files/delete_batch/check" => match st.jobs.get(&s("async_job_id")) {
            Some(v) => ok(v.clone()),
            None => route_error("invalid_async_job_id/", json!({".tag": "invalid_async_job_id"})),
        },
        "files/list_revisions" => {
            let l = lower(s("path"));
            match (st.nodes.get(&l), st.deleted.get(&l)) {
                (Some(n), _) if n.dir => route_error("path/not_file/..", json!({".tag": "path", "path": {".tag": "not_file"}})),
                (Some(n), _) => ok(json!({"is_deleted": false, "entries": [meta(&l, n)]})),
                (None, Some((rev, when))) => {
                    let (display, data) = st.revs[rev].clone();
                    let m = meta(&l, &Node { display, dir: false, data, rev: rev.clone() });
                    ok(json!({"is_deleted": true, "server_deleted": timestamp(*when), "entries": [m]}))
                }
                (None, None) if st.revs.values().any(|(d, _)| d.to_lowercase().starts_with(&format!("{l}/"))) => route_error("path/not_file/..", json!({".tag": "path", "path": {".tag": "not_file"}})),
                (None, None) => not_found("path"),
            }
        }
        "files/restore" => match st.revs.get(&s("rev")).cloned() {
            Some((_, data)) => {
                let m = st.put(&s("path"), data.to_vec());
                ok(m)
            }
            None => route_error("invalid_revision/", json!({".tag": "invalid_revision"})),
        },
        "files/download" => {
            let p = s("path");
            let from = req.headers.get("range").and_then(|r| r.trim_start_matches("bytes=").trim_end_matches('-').parse::<u64>().ok()).unwrap_or(0);
            st.ranges.push((p.clone(), from));
            let found = match p.strip_prefix("rev:") {
                Some(rev) => st.revs.get(rev).map(|(d, data)| (d.to_lowercase(), Node { display: d.clone(), dir: false, data: data.clone(), rev: rev.to_string() })),
                None => st.nodes.get(&p.to_lowercase()).filter(|n| !n.dir).map(|n| (p.to_lowercase(), n.clone())),
            };
            match found {
                None => not_found("path"),
                Some((l, n)) => {
                    let m = meta(&l, &n);
                    let header = crate::api::header_json(&m.to_string());
                    let data = n.data[from as usize..].to_vec();
                    if let Some(cut) = st.cut_download.take() {
                        drop(st);
                        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nDropbox-API-Result: {header}\r\nConnection: close\r\n\r\n", data.len());
                        let _ = conn.write_all(head.as_bytes());
                        let _ = conn.write_all(&data[..cut as usize]);
                        let _ = conn.flush();
                        let _ = conn.shutdown(std::net::Shutdown::Both);
                        return;
                    }
                    (if from > 0 { 206 } else { 200 }, vec![("Dropbox-API-Result".into(), header)], data)
                }
            }
        }
        "files/upload" => {
            let m = st.put(&s("path"), req.body.clone());
            ok(m)
        }
        "files/upload_session/start" => {
            st.next += 1;
            let id = format!("sess-{}", st.next);
            st.sessions.insert(id.clone(), req.body.clone());
            ok(json!({"session_id": id}))
        }
        "files/upload_session/append_v2" => {
            let id = arg.pointer("/cursor/session_id").and_then(Value::as_str).unwrap_or("").to_string();
            let offset = arg.pointer("/cursor/offset").and_then(Value::as_u64).unwrap_or(0) as usize;
            let held = st.sessions.get(&id).map(|d| d.len()).unwrap_or(0);
            if offset != held {
                route_error("incorrect_offset/..", json!({".tag": "incorrect_offset", "incorrect_offset": {"correct_offset": held}}))
            } else {
                st.sessions.get_mut(&id).unwrap().extend_from_slice(&req.body);
                if std::mem::take(&mut st.drop_append) {
                    // Taken in, but the answer never arrives.
                    drop(st);
                    let _ = conn.shutdown(std::net::Shutdown::Both);
                    return;
                }
                ok(Value::Null)
            }
        }
        "files/upload_session/finish" => {
            let id = arg.pointer("/cursor/session_id").and_then(Value::as_str).unwrap_or("").to_string();
            let data = st.sessions.remove(&id).unwrap_or_default();
            let path = arg.pointer("/commit/path").and_then(Value::as_str).unwrap_or("").to_string();
            let m = st.put(&path, data);
            ok(m)
        }
        "sharing/list_shared_links" => {
            let links: Vec<Value> = st.links.get(&lower(s("path"))).map(|u| json!({".tag": "file", "url": u})).into_iter().collect();
            ok(json!({"links": links, "has_more": false}))
        }
        "sharing/create_shared_link_with_settings" => {
            let l = lower(s("path"));
            if let Some(u) = st.links.get(&l) {
                route_error("shared_link_already_exists/..", json!({".tag": "shared_link_already_exists", "shared_link_already_exists": {".tag": "metadata", "metadata": {".tag": "file", "url": u}}}))
            } else if !st.nodes.contains_key(&l) {
                not_found("path")
            } else {
                let u = format!("https://www.dropbox.com/scl/fi/{}/{}?dl=0", st.links.len() + 1, last(&l));
                st.links.insert(l, u.clone());
                ok(json!({".tag": "file", "url": u}))
            }
        }
        "sharing/revoke_shared_link" => {
            let u = s("url");
            let before = st.links.len();
            st.links.retain(|_, v| *v != u);
            if st.links.len() == before { route_error("shared_link_not_found/", json!({".tag": "shared_link_not_found"})) } else { ok(Value::Null) }
        }
        other => (400, vec![], format!("Error in call to API function \"{other}\": unknown route").into_bytes()),
    };
    drop(st);
    state.1.notify_all();
    respond(&mut conn, status, &headers, &body);
}
