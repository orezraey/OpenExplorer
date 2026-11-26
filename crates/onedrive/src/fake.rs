//! A stand-in for Microsoft Graph and its login on this machine, for tests and the benchmark:
//! one drive in memory answering the calls the crate makes as Graph documents them (ids,
//! `parentReference`, the delta feed with its links, 202 copies with a monitor, upload sessions,
//! `Range`, `$batch`, 429 with `Retry-After`), counting requests and bytes.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::graph::Endpoints;
use crate::quickxor::QuickXor;

pub const DRIVE: &str = "d1";
pub const ROOT: &str = "R";

#[derive(Clone, Debug)]
pub struct FItem {
    pub id: String,
    pub parent: Option<String>,
    pub name: String,
    pub is_dir: bool,
    pub data: Vec<u8>,
    pub version: u64,
    pub deleted: bool,
}

struct Session {
    parent: String,
    name: String,
    len: u64,
    data: Vec<u8>,
}

#[derive(Default)]
pub struct State {
    pub items: BTreeMap<String, FItem>,
    pub version: u64,
    next: u64,
    sessions: HashMap<u64, Session>,
    monitors: HashMap<u64, String>,
    /// Item id → link permission ids.
    pub links: HashMap<String, Vec<String>>,
    /// The next this many requests to Graph answer 429.
    pub throttle: usize,
    /// The next download is cut after this many bytes.
    pub cut_download: Option<usize>,
    /// The next upload part is refused with a 500 after the session took half of it.
    pub cut_upload: bool,
    /// Refresh tokens given out.
    pub refreshes: usize,
    /// Every delta link given out before this version is refused (410).
    pub resync_before: u64,
    /// A work account: its drive is a SharePoint site (`/sp/…` here) whose recycle bin gets
    /// what Graph deletes.
    pub business: bool,
    /// SharePoint's permission granted to the app: a token for it is refused until then.
    pub sp_granted: bool,
    /// SharePoint asks for `X-RequestDigest` on POSTs.
    pub sp_digest: bool,
    /// The site collection's recycle bin refused (403): only the site's, first stage.
    pub sp_site_refused: bool,
    /// `$filter` refused (400).
    pub sp_no_filter: bool,
    /// `$filter` answered with this status instead (a 500, say), when not 0.
    pub sp_filter_status: u16,
    /// The site's recycle bin refused too (403).
    pub sp_web_refused: bool,
    /// Recycle bin items per page (`odata.nextLink`), all at once if 0.
    pub sp_page: usize,
    /// `$orderby` and `$top` refused (400).
    pub sp_no_orderby: bool,
    /// Pages of folder listings (`/children`) asked.
    pub children_asked: usize,
    /// `$orderby` taken but not followed: the bin comes oldest first (`$top` still cuts it).
    pub sp_ignore_orderby: bool,
    /// After this many more `$batch` requests, the next one gets no answer (the connection
    /// closes before any of it is done).
    pub drop_batch: Option<usize>,
    /// The recycle bin, oldest first.
    pub bin: Vec<FBin>,
    /// Requests to SharePoint's REST API.
    pub sp_requests: usize,
}

/// A recycle bin item of the work account's site.
#[derive(Clone, Debug)]
pub struct FBin {
    pub guid: String,
    pub item: String,
    pub dir: String,
    /// `DeletedDate`, ISO 8601 in UTC.
    pub deleted: String,
    pub stage: u8,
}

/// The work account's site, below the stand-in's address.
pub const SITE: &str = "/sp/personal/ana_exemplo_com";

impl State {
    /// `DirName` of a folder: the library's path from the host, without the first slash.
    fn dir_name(&self, folder: &str) -> String {
        let mut names = Vec::new();
        let mut at = Some(folder.to_string());
        while let Some(id) = at {
            let Some(it) = self.items.get(&id) else { break };
            if it.parent.is_some() {
                names.push(it.name.clone());
            }
            at = it.parent.clone();
        }
        names.reverse();
        let mut d = format!("{}/Documents", SITE.trim_start_matches('/'));
        for n in names {
            d.push('/');
            d.push_str(&n);
        }
        d
    }

    /// What deleting `id` puts in the recycle bin of a work account.
    pub fn put_in_bin(&mut self, id: &str, deleted: &str) -> String {
        self.next += 1;
        let guid = format!("{:08x}-0000-4000-8000-{:012x}", self.next, self.next);
        let parent = self.items[id].parent.clone().unwrap_or_default();
        let dir = self.dir_name(&parent);
        self.bin.push(FBin { guid: guid.clone(), item: id.to_string(), dir, deleted: deleted.to_string(), stage: 1 });
        guid
    }

    fn drop_tree(&mut self, id: &str) {
        let kids: Vec<String> = self.items.values().filter(|i| i.parent.as_deref() == Some(id)).map(|i| i.id.clone()).collect();
        for k in kids {
            self.drop_tree(&k);
        }
        self.items.remove(id);
    }

    fn bump(&mut self) -> u64 {
        self.version += 1;
        self.version
    }

    pub fn add(&mut self, parent: &str, name: &str, is_dir: bool, data: &[u8]) -> String {
        self.next += 1;
        let id = format!("I{}", self.next);
        let version = self.bump();
        self.items.insert(id.clone(), FItem { id: id.clone(), parent: Some(parent.into()), name: name.into(), is_dir, data: data.to_vec(), version, deleted: false });
        id
    }

    pub fn touch(&mut self, id: &str, f: impl FnOnce(&mut FItem)) {
        let v = self.bump();
        let it = self.items.get_mut(id).expect("an item");
        f(it);
        it.version = v;
    }

    fn kids(&self, id: &str) -> Vec<&FItem> {
        let mut v: Vec<&FItem> = self.items.values().filter(|i| !i.deleted && i.parent.as_deref() == Some(id)).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    fn child(&self, id: &str, name: &str) -> Option<&FItem> {
        self.kids(id).into_iter().find(|i| i.name.to_lowercase() == name.to_lowercase())
    }

    fn json(&self, it: &FItem, full: bool, base: &str) -> Value {
        let mut v = json!({
            "id": it.id,
            "name": it.name,
            "size": if it.is_dir { self.kids(&it.id).iter().map(|k| k.data.len()).sum::<usize>() } else { it.data.len() },
            "lastModifiedDateTime": "2024-05-06T07:08:09Z",
        });
        if it.deleted {
            v["deleted"] = json!({ "state": "deleted" });
        }
        match &it.parent {
            Some(p) => v["parentReference"] = json!({ "driveId": DRIVE, "driveType": if self.business { "business" } else { "personal" }, "id": p }),
            None => v["root"] = json!({}),
        }
        if it.is_dir {
            v["folder"] = json!({ "childCount": self.kids(&it.id).len() });
        } else if full {
            let mut h = QuickXor::new();
            h.update(&it.data);
            v["file"] = json!({ "mimeType": "application/octet-stream", "hashes": { "quickXorHash": h.base64() } });
            v["@microsoft.graph.downloadUrl"] = format!("{base}/dl/{}", it.id).into();
        }
        if full {
            v["eTag"] = format!("\"{{{}}},{}\"", it.id, it.version).into();
            v["createdDateTime"] = "2024-05-06T07:08:09Z".into();
            v["webUrl"] = format!("https://onedrive.live.com/?id={}", it.id).into();
        }
        v
    }
}

#[derive(Default)]
pub struct Count {
    pub requests: AtomicUsize,
    pub bytes: AtomicUsize,
}

pub struct Fake {
    pub state: Arc<Mutex<State>>,
    pub count: Arc<Count>,
    pub base: String,
    stop: Arc<AtomicBool>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base.trim_start_matches("http://"));
    }
}

impl Fake {
    pub fn start() -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let mut st = State::default();
        st.items.insert(ROOT.into(), FItem { id: ROOT.into(), parent: None, name: "root".into(), is_dir: true, data: Vec::new(), version: 0, deleted: false });
        let state = Arc::new(Mutex::new(st));
        let count = Arc::new(Count::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (s, c, b, halt) = (state.clone(), count.clone(), base.clone(), stop.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if halt.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(conn) = conn else { continue };
                let (s, c, b) = (s.clone(), c.clone(), b.clone());
                std::thread::spawn(move || serve(conn, &s, &c, &b));
            }
        });
        Fake { state, count, base, stop }
    }

    pub fn ends(&self) -> Endpoints {
        Endpoints { graph: self.base.clone(), login: format!("{}/login", self.base) }
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    pub fn requests(&self) -> usize {
        self.count.requests.load(Ordering::SeqCst)
    }
}

struct Req {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn json(status: u16, v: Value) -> Resp {
        Resp { status, headers: vec![("Content-Type".into(), "application/json".into())], body: serde_json::to_vec(&v).unwrap() }
    }

    fn empty(status: u16) -> Resp {
        Resp { status, headers: Vec::new(), body: Vec::new() }
    }

    fn error(status: u16, code: &str) -> Resp {
        Resp::json(status, json!({ "error": { "code": code, "message": code } }))
    }
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(x) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(x);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn read(conn: &TcpStream) -> Option<Req> {
    let mut r = BufReader::new(conn);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        r.read_line(&mut h).ok()?;
        if h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers.get("content-length").and_then(|l| l.parse().ok()).unwrap_or(0);
    let mut body = vec![0; len];
    r.read_exact(&mut body).ok()?;
    let (path, q) = target.split_once('?').unwrap_or((&target, ""));
    let query = q.split('&').filter(|kv| !kv.is_empty()).map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (decode(k), decode(v))
    }).collect();
    Some(Req { method, path: path.to_string(), query, headers, body })
}

fn serve(mut conn: TcpStream, state: &Mutex<State>, count: &Count, base: &str) {
    let Some(req) = read(&conn) else { return };
    if req.path == "/$batch" {
        let mut st = state.lock().unwrap();
        match st.drop_batch {
            Some(0) => {
                st.drop_batch = None;
                return;
            }
            Some(n) => st.drop_batch = Some(n - 1),
            None => {}
        }
    }
    let resp = answer(&req, state, base);
    count.requests.fetch_add(1, Ordering::SeqCst);
    count.bytes.fetch_add(resp.body.len(), Ordering::SeqCst);
    let mut head = format!("HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n", resp.status, resp.body.len());
    for (k, v) in &resp.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = conn.write_all(head.as_bytes());
    // A cut download: the head says it all, the bytes stop short.
    let cut = if req.path.starts_with("/dl/") { state.lock().unwrap().cut_download.take() } else { None };
    let body = match cut {
        Some(n) => &resp.body[..n.min(resp.body.len())],
        None => &resp.body[..],
    };
    let _ = conn.write_all(body);
    let _ = conn.flush();
}

fn form(body: &[u8]) -> HashMap<String, String> {
    let s = String::from_utf8_lossy(body).replace('+', " ");
    s.split('&').filter(|kv| !kv.is_empty()).map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (decode(k), decode(v))
    }).collect()
}

fn answer(req: &Req, state: &Mutex<State>, base: &str) -> Resp {
    let mut st = state.lock().unwrap();
    if req.path.starts_with("/login/") {
        // A token for SharePoint (`{host}/.default`) only once its permission is granted; Graph's
        // otherwise. Each starts with its own letter, so a token sent to the wrong one fails.
        let scope = form(&req.body).get("scope").cloned().unwrap_or_default();
        let sharepoint = scope.contains("/.default");
        if sharepoint && !st.sp_granted {
            return Resp::json(400, json!({ "error": "invalid_grant", "error_description": "AADSTS65001: The user or administrator has not consented to use the application with ID '0b1c2d3e'. Send an interactive authorization request for this user and resource.\r\nTrace ID: x" }));
        }
        st.refreshes += 1;
        let letter = if sharepoint { "s" } else { "a" };
        return Resp::json(200, json!({ "access_token": format!("{letter}{}", st.refreshes), "refresh_token": format!("r{}", st.refreshes), "expires_in": 3600 }));
    }
    if let Some(rest) = req.path.strip_prefix(&format!("{SITE}/_api/")) {
        st.sp_requests += 1;
        if !req.headers.get("authorization").is_some_and(|a| a.starts_with("Bearer s")) {
            return Resp::json(401, json!({ "error_description": "Invalid audience." }));
        }
        if !req.headers.get("accept").is_some_and(|a| a.contains("application/json")) {
            return Resp { status: 200, headers: vec![("Content-Type".into(), "application/atom+xml".into())], body: b"<feed/>".to_vec() };
        }
        return sharepoint(req, &mut st, rest, base);
    }
    if !req.path.starts_with("/dl/") && !req.path.starts_with("/up/") && !req.path.starts_with("/monitor/") {
        if st.throttle > 0 {
            st.throttle -= 1;
            return Resp { status: 429, headers: vec![("Retry-After".into(), "0".into())], body: Vec::new() };
        }
        if !req.headers.get("authorization").is_some_and(|a| a.starts_with("Bearer a")) {
            return Resp::error(401, "InvalidAuthenticationToken");
        }
    }
    route(req, &mut st, base)
}

fn route(req: &Req, st: &mut State, base: &str) -> Resp {
    let full = !req.query.contains_key("$select");
    let p = req.path.as_str();
    if p == "/$batch" {
        let v: Value = serde_json::from_slice(&req.body).unwrap();
        let mut out = Vec::new();
        for r in v["requests"].as_array().unwrap() {
            let url = r["url"].as_str().unwrap();
            let (path, q) = url.split_once('?').unwrap_or((url, ""));
            let sub = Req {
                method: r["method"].as_str().unwrap().into(),
                path: path.into(),
                query: q.split('&').filter(|kv| !kv.is_empty()).map(|kv| { let (k, v) = kv.split_once('=').unwrap_or((kv, "")); (decode(k), decode(v)) }).collect(),
                headers: HashMap::new(),
                body: r.get("body").map(|b| serde_json::to_vec(b).unwrap()).unwrap_or_default(),
            };
            let resp = route(&sub, st, base);
            let body: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
            out.push(json!({ "id": r["id"], "status": resp.status, "body": body }));
        }
        out.reverse(); // answers may come in any order
        return Resp::json(200, json!({ "responses": out }));
    }
    if let Some(id) = p.strip_prefix("/dl/") {
        let Some(it) = st.items.get(id).filter(|i| !i.deleted) else { return Resp::error(404, "itemNotFound") };
        let from: usize = req.headers.get("range").and_then(|r| r.strip_prefix("bytes=")).and_then(|r| r.trim_end_matches('-').parse().ok()).unwrap_or(0);
        let status = if req.headers.contains_key("range") { 206 } else { 200 };
        return Resp { status, headers: Vec::new(), body: it.data[from.min(it.data.len())..].to_vec() };
    }
    if let Some(n) = p.strip_prefix("/monitor/") {
        let id = st.monitors.get(&n.parse().unwrap()).cloned().unwrap();
        return Resp::json(202, json!({ "status": "completed", "resourceId": id }));
    }
    if let Some(n) = p.strip_prefix("/up/") {
        let n: u64 = n.parse().unwrap();
        return upload_part(req, st, n, base);
    }
    if p == format!("/drives/{DRIVE}") {
        let used: usize = st.items.values().map(|i| i.data.len()).sum();
        return Resp::json(200, json!({ "quota": { "total": 5_368_709_120u64, "used": used, "remaining": 5_368_709_120u64 - used as u64, "deleted": 0, "state": "normal" } }));
    }
    let Some(rest) = p.strip_prefix(&format!("/drives/{DRIVE}/")) else { return Resp::error(400, "invalidRequest") };
    if rest == "root/delta" {
        return delta(req, st, base);
    }
    // The item: by id, then perhaps by path below it; then what's asked of it.
    let (id, mut tail) = if let Some(r) = rest.strip_prefix("root") {
        (ROOT.to_string(), r)
    } else if let Some(r) = rest.strip_prefix("items/") {
        let end = r.find([':', '/']).unwrap_or(r.len());
        (decode(&r[..end]), &r[end..])
    } else {
        return Resp::error(400, "invalidRequest");
    };
    let mut target = Some(id.clone());
    let mut pending: Option<(String, String)> = None;
    if let Some(r) = tail.strip_prefix(":/") {
        let end = r.find(':').unwrap_or(r.len());
        let segs: Vec<String> = r[..end].split('/').map(decode).collect();
        tail = &r[(end + 1).min(r.len())..];
        let mut at = id.clone();
        for (i, s) in segs.iter().enumerate() {
            match st.child(&at, s) {
                Some(c) => at = c.id.clone(),
                None if i == segs.len() - 1 => {
                    pending = Some((at.clone(), s.clone()));
                    target = None;
                    break;
                }
                None => return Resp::error(404, "itemNotFound"),
            }
        }
        if target.is_some() {
            target = Some(at);
        }
    }
    let alive = |st: &State, id: &str| st.items.get(id).is_some_and(|i| !i.deleted);
    match (req.method.as_str(), tail) {
        ("GET", "") => match target.filter(|t| alive(st, t)) {
            Some(t) => {
                let mut v = st.json(&st.items[&t], full, base);
                // A work account's library and site, as SharePoint has them.
                if st.business && t == ROOT && req.query.get("$select").is_some_and(|s| s.contains("sharepointIds")) {
                    v["webUrl"] = format!("{base}{SITE}/Documents").into();
                    v["sharepointIds"] = json!({ "siteUrl": format!("{base}{SITE}"), "listId": "L1" });
                }
                Resp::json(200, v)
            }
            None => Resp::error(404, "itemNotFound"),
        },
        ("GET", "/children") => {
            let Some(t) = target.filter(|t| alive(st, t)) else { return Resp::error(404, "itemNotFound") };
            let top: usize = req.query.get("$top").and_then(|t| t.parse().ok()).unwrap_or(200);
            let skip: usize = req.query.get("$skiptoken").and_then(|t| t.parse().ok()).unwrap_or(0);
            st.children_asked += 1;
            let mut kids = st.kids(&t);
            // By name (Graph's own order here); "name desc" from the other end.
            let order = req.query.get("$orderby").cloned();
            if order.as_deref() == Some("name desc") {
                kids.reverse();
            }
            // Only the facets asked for, as Graph: a listing asking `file` and not `folder`.
            let asked = |f: &str| req.query.get("$select").is_none_or(|s| s.split(',').any(|x| x == f));
            let page: Vec<Value> = kids
                .iter()
                .skip(skip)
                .take(top)
                .map(|k| {
                    let mut v = st.json(k, full, base);
                    if !asked("folder") && let Some(o) = v.as_object_mut() {
                        o.remove("folder");
                    }
                    if asked("file") && !k.is_dir && v.get("file").is_none() {
                        v["file"] = json!({ "mimeType": "application/octet-stream" });
                    }
                    v
                })
                .collect();
            let mut v = json!({ "value": page });
            if skip + top < kids.len() {
                let sel = req.query.get("$select").map(|s| format!("&$select={s}")).unwrap_or_default();
                let order = order.map(|o| format!("&$orderby={}", o.replace(' ', "%20"))).unwrap_or_default();
                v["@odata.nextLink"] = format!("{base}/drives/{DRIVE}/items/{t}/children?$top={top}&$skiptoken={}{sel}{order}", skip + top).into();
            }
            Resp::json(200, v)
        }
        ("POST", "/children") => {
            let Some(t) = target.filter(|t| alive(st, t)) else { return Resp::error(404, "itemNotFound") };
            let b: Value = serde_json::from_slice(&req.body).unwrap();
            let name = b["name"].as_str().unwrap();
            if st.child(&t, name).is_some() {
                return Resp::error(409, "nameAlreadyExists");
            }
            let id = st.add(&t, name, true, &[]);
            Resp::json(201, st.json(&st.items[&id], true, base))
        }
        ("PUT", "/content") => {
            let id = match (target, pending) {
                (Some(t), _) => {
                    st.touch(&t, |i| i.data = req.body.clone());
                    t
                }
                (None, Some((parent, name))) => st.add(&parent, &name, false, &req.body),
                _ => return Resp::error(404, "itemNotFound"),
            };
            Resp::json(201, st.json(&st.items[&id], true, base))
        }
        ("POST", "/createUploadSession") => {
            let (parent, name) = match (target, pending) {
                (Some(t), _) => {
                    let it = &st.items[&t];
                    (it.parent.clone().unwrap(), it.name.clone())
                }
                (None, Some(pn)) => pn,
                _ => return Resp::error(404, "itemNotFound"),
            };
            st.next += 1;
            let n = st.next;
            st.sessions.insert(n, Session { parent, name, len: 0, data: Vec::new() });
            Resp::json(200, json!({ "uploadUrl": format!("{base}/up/{n}"), "expirationDateTime": "2030-01-01T00:00:00Z", "nextExpectedRanges": ["0-"] }))
        }
        ("PATCH", "") => {
            let Some(t) = target.filter(|t| alive(st, t)) else { return Resp::error(404, "itemNotFound") };
            let b: Value = serde_json::from_slice(&req.body).unwrap();
            let parent = b.pointer("/parentReference/id").and_then(Value::as_str).map(str::to_string).or_else(|| st.items[&t].parent.clone()).unwrap();
            let name = b["name"].as_str().map(str::to_string).unwrap_or_else(|| st.items[&t].name.clone());
            if st.child(&parent, &name).is_some_and(|c| c.id != t) {
                return Resp::error(409, "nameAlreadyExists");
            }
            st.touch(&t, |i| {
                i.parent = Some(parent);
                i.name = name;
            });
            Resp::json(200, st.json(&st.items[&t], true, base))
        }
        ("DELETE", "") => {
            let Some(t) = target.filter(|t| alive(st, t)) else { return Resp::error(404, "itemNotFound") };
            st.touch(&t, |i| i.deleted = true);
            if st.business {
                st.put_in_bin(&t, &now());
            }
            Resp::empty(204)
        }
        // Graph restores only on personal accounts.
        ("POST", "/restore") if st.business => Resp::error(400, "invalidRequest"),
        ("POST", "/restore") => {
            let Some(t) = target.filter(|t| st.items.get(t).is_some_and(|i| i.deleted)) else { return Resp::error(404, "itemNotFound") };
            st.touch(&t, |i| i.deleted = false);
            Resp::json(200, st.json(&st.items[&t], true, base))
        }
        ("POST", "/copy") => {
            let Some(t) = target.filter(|t| alive(st, t)) else { return Resp::error(404, "itemNotFound") };
            let b: Value = serde_json::from_slice(&req.body).unwrap();
            let parent = b.pointer("/parentReference/id").and_then(Value::as_str).unwrap().to_string();
            let src = st.items[&t].clone();
            let name = b["name"].as_str().map(str::to_string).unwrap_or(src.name.clone());
            let id = copy_tree(st, &src, &parent, &name);
            st.next += 1;
            let n = st.next;
            st.monitors.insert(n, id);
            Resp { status: 202, headers: vec![("Location".into(), format!("{base}/monitor/{n}"))], body: Vec::new() }
        }
        ("GET", "/permissions") => {
            let Some(t) = target else { return Resp::error(404, "itemNotFound") };
            let v: Vec<Value> = st.links.get(&t).into_iter().flatten().map(|p| json!({ "id": p, "roles": ["read"], "link": { "type": "view", "scope": "anonymous", "webUrl": format!("https://1drv.ms/u/{p}") } })).collect();
            Resp::json(200, json!({ "value": v }))
        }
        ("POST", "/createLink") => {
            let Some(t) = target else { return Resp::error(404, "itemNotFound") };
            st.next += 1;
            let p = format!("P{}", st.next);
            st.links.entry(t).or_default().push(p.clone());
            Resp::json(201, json!({ "id": p, "link": { "type": "view", "scope": "anonymous", "webUrl": format!("https://1drv.ms/u/{p}") } }))
        }
        ("DELETE", perm) if perm.starts_with("/permissions/") => {
            let p = &perm["/permissions/".len()..];
            for v in st.links.values_mut() {
                v.retain(|x| x != p);
            }
            Resp::empty(204)
        }
        _ => Resp::error(400, "invalidRequest"),
    }
}

fn copy_tree(st: &mut State, src: &FItem, parent: &str, name: &str) -> String {
    let id = st.add(parent, name, src.is_dir, &src.data);
    let kids: Vec<FItem> = st.kids(&src.id).into_iter().cloned().collect();
    for k in kids {
        copy_tree(st, &k, &id, &k.name);
    }
    id
}

fn delta(req: &Req, st: &mut State, base: &str) -> Resp {
    let full = !req.query.contains_key("$select");
    if req.query.get("token").map(String::as_str) == Some("latest") {
        return Resp::json(200, json!({ "value": [], "@odata.deltaLink": format!("{base}/drives/{DRIVE}/root/delta?token={}", st.version) }));
    }
    let token: u64 = req.query.get("token").and_then(|t| t.parse().ok()).unwrap_or(0);
    if token < st.resync_before {
        return Resp::error(410, "resyncRequired");
    }
    let skip: usize = req.query.get("skip").and_then(|t| t.parse().ok()).unwrap_or(0);
    let mut changed: Vec<&FItem> = st.items.values().filter(|i| i.version > token).collect();
    changed.sort_by_key(|i| i.version);
    const TOP: usize = 1000;
    let page: Vec<Value> = changed.iter().skip(skip).take(TOP).map(|i| st.json(i, full, base)).collect();
    let mut v = json!({ "value": page });
    if skip + TOP < changed.len() {
        v["@odata.nextLink"] = format!("{base}/drives/{DRIVE}/root/delta?token={token}&skip={}", skip + TOP).into();
    } else {
        v["@odata.deltaLink"] = format!("{base}/drives/{DRIVE}/root/delta?token={}", st.version).into();
    }
    Resp::json(200, v)
}

fn upload_part(req: &Req, st: &mut State, n: u64, base: &str) -> Resp {
    if req.headers.contains_key("authorization") {
        return Resp::error(401, "unauthenticated");
    }
    match req.method.as_str() {
        "DELETE" => {
            st.sessions.remove(&n);
            Resp::empty(204)
        }
        "GET" => match st.sessions.get(&n) {
            Some(s) => Resp::json(200, json!({ "nextExpectedRanges": [format!("{}-", s.data.len())] })),
            None => Resp::error(404, "itemNotFound"),
        },
        "PUT" => {
            let range = req.headers.get("content-range").cloned().unwrap_or_default();
            let (span, total) = range.trim_start_matches("bytes ").split_once('/').unwrap();
            let (from, _) = span.split_once('-').unwrap();
            let (from, total): (usize, u64) = (from.parse().unwrap(), total.parse().unwrap());
            let cut = std::mem::take(&mut st.cut_upload);
            let Some(s) = st.sessions.get_mut(&n) else { return Resp::error(404, "itemNotFound") };
            if from != s.data.len() {
                return Resp::error(416, "invalidRange");
            }
            if !req.body.len().is_multiple_of(320 * 1024) &&(from + req.body.len()) as u64 != total {
                return Resp::error(400, "invalidChunkSize");
            }
            s.len = total;
            if cut {
                // Half of it arrived, then the session failed.
                s.data.extend_from_slice(&req.body[..req.body.len() / 2]);
                return Resp::error(500, "generalException");
            }
            s.data.extend_from_slice(&req.body);
            if (s.data.len() as u64) < total {
                return Resp::json(202, json!({ "nextExpectedRanges": [format!("{}-", s.data.len())] }));
            }
            let s = st.sessions.remove(&n).unwrap();
            let id = match st.child(&s.parent, &s.name).map(|c| c.id.clone()) {
                Some(id) => {
                    st.touch(&id, |i| i.data = s.data);
                    id
                }
                None => st.add(&s.parent, &s.name, false, &s.data),
            };
            Resp::json(201, st.json(&st.items[&id], true, base))
        }
        _ => Resp::error(400, "invalidRequest"),
    }
}

/// Now, as SharePoint writes `DeletedDate`.
pub fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn sp_error(status: u16, code: &str, message: &str) -> Resp {
    Resp::json(status, json!({ "odata.error": { "code": code, "message": { "lang": "en-US", "value": message } } }))
}

/// SharePoint's REST API for the work account's site: the recycle bin (both stages from the
/// site collection, the first from the site), restoring and deleting its items for good, and
/// `contextinfo`'s form digest, answering as with `odata=nometadata`.
fn sharepoint(req: &Req, st: &mut State, rest: &str, base: &str) -> Resp {
    if !st.business {
        return sp_error(404, "-1, Microsoft.SharePoint.Client.ResourceNotFoundException", "Not found.");
    }
    if req.method == "POST" && rest == "contextinfo" {
        return Resp::json(200, json!({ "FormDigestValue": "0x1234,19 Nov 2026 12:00:00 -0000", "FormDigestTimeoutSeconds": 1800 }));
    }
    if req.method == "POST" && st.sp_digest && !req.headers.contains_key("x-requestdigest") {
        return sp_error(403, "-2130575251, Microsoft.SharePoint.SPException", "The security validation for this page is invalid and might be corrupted. Please use your web browser's Back button to try your operation again.");
    }
    let (coll, after) = if let Some(a) = rest.strip_prefix("site/RecycleBin") {
        ("site", a)
    } else if let Some(a) = rest.strip_prefix("web/RecycleBin") {
        ("web", a)
    } else {
        return sp_error(404, "-1, Microsoft.SharePoint.Client.InvalidClientQueryException", "The expression is not valid.");
    };
    if (coll == "site" && st.sp_site_refused) || (coll == "web" && st.sp_web_refused) {
        return sp_error(403, "-2147024891, System.UnauthorizedAccessException", "Access denied.");
    }
    let visible = |b: &FBin| coll == "site" || b.stage == 1;
    if after.is_empty() && req.method == "GET" {
        let since = match req.query.get("$filter") {
            Some(_) if st.sp_no_filter => return sp_error(400, "-1, Microsoft.SharePoint.Client.InvalidClientQueryException", "The query is not valid."),
            Some(_) if st.sp_filter_status != 0 => return sp_error(st.sp_filter_status, "-1, System.NotSupportedException", "Not supported."),
            Some(f) => f.split("datetime'").nth(1).map(|s| s.trim_end_matches('\'').to_string()),
            None => None,
        };
        let ordered = req.query.contains_key("$orderby") || req.query.contains_key("$top");
        if ordered && st.sp_no_orderby {
            return sp_error(400, "-1, Microsoft.SharePoint.Client.InvalidClientQueryException", "The query is not valid.");
        }
        let mut all: Vec<&FBin> = st.bin.iter().filter(|b| visible(b) && since.as_ref().is_none_or(|s| b.deleted >= *s)).collect();
        if !st.sp_ignore_orderby && req.query.get("$orderby").is_some_and(|o| o == "DeletedDate desc") {
            // Newest first; of the same date, the one put in last first.
            all.reverse();
            all.sort_by(|a, b| b.deleted.cmp(&a.deleted));
        }
        if let Some(top) = req.query.get("$top").and_then(|t| t.parse::<usize>().ok()) {
            all.truncate(top);
        }
        let skip: usize = req.query.get("$skiptoken").and_then(|s| s.parse().ok()).unwrap_or(0);
        let take = if st.sp_page == 0 { usize::MAX } else { st.sp_page };
        let page: Vec<Value> = all
            .iter()
            .skip(skip)
            .take(take)
            .map(|b| {
                let it = &st.items[&b.item];
                json!({
                    "Id": b.guid, "LeafName": it.name, "DirName": b.dir, "DeletedDate": b.deleted,
                    "ItemState": b.stage, "ItemType": if it.is_dir { 5 } else { 1 }, "Size": it.data.len().to_string(),
                })
            })
            .collect();
        let mut v = json!({ "value": page });
        if skip.saturating_add(take) < all.len() {
            let q: String = req.query.iter().filter(|(k, _)| *k != "$skiptoken").map(|(k, v)| format!("{k}={}", v.replace(' ', "%20").replace('\'', "%27"))).collect::<Vec<_>>().join("&");
            v["odata.nextLink"] = format!("{base}{SITE}/_api/{coll}/RecycleBin?{q}&$skiptoken={}", skip + take).into();
        }
        return Resp::json(200, v);
    }
    // ('{guid}')/restore() or ('{guid}')/deleteObject()
    let Some((id, verb)) = after.strip_prefix("('").and_then(|a| a.split_once("')/")) else {
        return sp_error(400, "-1, Microsoft.SharePoint.Client.InvalidClientQueryException", "The expression is not valid.");
    };
    if req.method != "POST" {
        return sp_error(405, "-1, Microsoft.SharePoint.Client.ClientServiceException", "The HTTP method 'GET' cannot be used.");
    }
    let Some(at) = st.bin.iter().position(|b| b.guid == id && visible(b)) else {
        return sp_error(404, "-2147024809, System.ArgumentException", "Item does not exist. It may have been deleted by another user.");
    };
    let b = st.bin.remove(at);
    match verb {
        "restore()" => st.touch(&b.item, |i| i.deleted = false),
        "deleteObject()" => st.drop_tree(&b.item),
        _ => {
            st.bin.insert(at, b);
            return sp_error(400, "-1, Microsoft.SharePoint.Client.InvalidClientQueryException", "The expression is not valid.");
        }
    }
    Resp::json(200, json!({ "odata.null": true }))
}
