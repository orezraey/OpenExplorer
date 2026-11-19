//! The Dropbox API v2 over HTTP: RPC routes (a JSON argument, a JSON answer) on the api host,
//! content routes (the argument in the `Dropbox-API-Arg` header, the file in the body) on the
//! content host, and the longpoll on the notify host.
//!
//! What every call shares lives here: the access token, renewed from the refresh token when it
//! is about to expire or Dropbox says it did (`expired_access_token`, HTTP 401); waiting as long
//! as Dropbox asks on HTTP 429 and 503 (`Retry-After`, or `retry_after` in the body, which
//! `too_many_write_operations` leaves out); and the `Dropbox-API-Path-Root` header of a team
//! account (see [`crate::account`]).
//!
//! A request whose answer never came (a connection dropped once it was sent, a timeout, a 5xx)
//! may or may not have done its work. Only routes that are safe to send twice go again by
//! themselves (see [`repeatable`]): reads, the longpoll, the upload sessions' blocks (their
//! offset says what Dropbox holds), a whole-file upload that overwrites. Any other route (a move,
//! a copy, a deletion, a commit) fails with [`Fail::Lost`], and its caller asks Dropbox how
//! things stand before sending it again or saying it failed (see `Account::write`). A request
//! that never left (no address, no connection) goes again whatever its route.

use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use openexplorer_i18n::{t, tr};
use serde_json::Value;

/// Where the API answers. Tests and benchmarks point it at a stand-in on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hosts {
    pub api: String,
    pub content: String,
    pub notify: String,
}

impl Default for Hosts {
    fn default() -> Hosts {
        Hosts { api: "https://api.dropboxapi.com".into(), content: "https://content.dropboxapi.com".into(), notify: "https://notify.dropboxapi.com".into() }
    }
}

impl Hosts {
    /// All three on one base address (a stand-in server).
    pub fn all(base: &str) -> Hosts {
        Hosts { api: base.into(), content: base.into(), notify: base.into() }
    }
}

/// A call that failed.
#[derive(Clone, Debug, PartialEq)]
pub enum Fail {
    /// The route's own error (HTTP 409): its `error_summary` ("path/not_found/..") and `error`.
    Route { summary: String, error: Value },
    /// The access ended: revoked, or the refresh token no longer works.
    Auth(String),
    /// The request may have reached Dropbox but its answer didn't come back (the connection
    /// dropped, a timeout, a 5xx): whether it did its work is unknown. In the user's words.
    Lost(String),
    /// Anything else, already in the user's words.
    Other(String),
}

impl Fail {
    /// Whether the route's error names `what` ("not_found", "conflict", "reset"...).
    pub fn is(&self, what: &str) -> bool {
        matches!(self, Fail::Route { summary, .. } if summary.split(['/', '.']).any(|part| part == what))
    }

    pub fn text(&self) -> String {
        match self {
            Fail::Route { summary, .. } => route_text(summary),
            Fail::Auth(e) | Fail::Lost(e) | Fail::Other(e) => e.clone(),
        }
    }
}

/// Whether route `route` may go again by itself after a try whose answer was lost: sending it
/// twice ends as sending it once. Reads and job checks; the longpoll; an upload session's start
/// (a second one only leaves an unused session) and its blocks, each sent at an offset Dropbox
/// checks (`incorrect_offset` says what it holds); a whole-file upload, which overwrites with the
/// same bytes; a shared link made or taken away, whose second try's `shared_link_already_exists`
/// / `shared_link_not_found` are taken as done. Anything else (a new folder, a move, a copy, a
/// deletion, a restore, a commit of upload sessions, a batch job) would fail
/// the second time where the first succeeded, or do the work twice.
pub fn repeatable(route: &str) -> bool {
    route.ends_with("/check")
        || route.ends_with("/check_v2")
        || matches!(
            route,
            "users/get_current_account"
                | "users/get_space_usage"
                | "auth/token/revoke"
                | "files/list_folder"
                | "files/list_folder/continue"
                | "files/list_folder/get_latest_cursor"
                | "files/list_folder/longpoll"
                | "files/get_metadata"
                | "files/list_revisions"
                | "files/download"
                | "files/upload"
                | "files/upload_session/start"
                | "files/upload_session/start_batch"
                | "files/upload_session/append_v2"
                | "files/upload_session/append_batch"
                | "sharing/list_shared_links"
                | "sharing/create_shared_link_with_settings"
                | "sharing/revoke_shared_link"
        )
}

/// How long a commit of `entries` upload sessions in one `finish_batch_v2` may take to answer:
/// it answers only once every file is committed, and Dropbox gives no figure, so the RPC's 90 s
/// and two seconds more per file (250 files: about 10 minutes).
pub fn commit_timeout(entries: usize) -> Duration {
    Duration::from_secs(90 + 2 * entries as u64)
}

impl From<Fail> for String {
    fn from(f: Fail) -> String {
        f.text()
    }
}

/// A route's error in the user's words.
pub fn route_text(summary: &str) -> String {
    let has = |w: &str| summary.split(['/', '.']).any(|p| p == w);
    if has("not_found") || has("not_file") && has("from_lookup") {
        t("Não encontrado no Dropbox (o item foi apagado ou movido?).", "Not found on Dropbox (was the item deleted or moved?).").into()
    } else if has("conflict") {
        t("Já existe um item com esse nome nesse lugar do Dropbox.", "An item with that name already exists there on Dropbox.").into()
    } else if has("insufficient_space") || has("insufficient_quota") {
        t("Sem espaço livre no Dropbox.", "Out of space on Dropbox.").into()
    } else if has("no_write_permission") || has("no_permission") || has("access_denied") {
        t("Sem permissão para alterar isso no Dropbox.", "No permission to change that on Dropbox.").into()
    } else if has("disallowed_name") || has("malformed_path") {
        t("O Dropbox não aceita esse nome.", "Dropbox doesn't accept that name.").into()
    } else if has("cant_move_folder_into_itself") || has("cant_copy_shared_folder") || has("cant_nest_shared_folder") || has("cant_move_shared_folder") || has("cant_transfer_ownership") {
        t("O Dropbox não permite mover ou copiar isso para lá.", "Dropbox doesn't allow moving or copying that there.").into()
    } else if has("too_many_files") || has("too_many_write_operations") {
        t("Itens demais de uma vez para o Dropbox. Tente com menos.", "Too many items at once for Dropbox. Try fewer.").into()
    } else if has("restricted_content") {
        t("O Dropbox bloqueou esse conteúdo.", "Dropbox blocked that content.").into()
    } else if has("email_not_verified") {
        t("Confirme o e-mail da conta Dropbox no site antes.", "Confirm the Dropbox account's e-mail on the website first.").into()
    } else if has("not_folder") {
        t("Isso não é uma pasta no Dropbox.", "That isn't a folder on Dropbox.").into()
    } else if has("is_folder") || has("not_file") {
        t("Isso é uma pasta, não um arquivo, no Dropbox.", "That's a folder, not a file, on Dropbox.").into()
    } else {
        tr!("O Dropbox recusou: {}", "Dropbox refused: {}", summary.trim_end_matches(['/', '.']))
    }
}

// ---------------------------------------------------------------------------------------------
// Log

static LOG: RwLock<Option<fn(&str)>> = RwLock::new(None);

/// Where the crate's log lines go (the app's log file).
pub fn set_log(f: fn(&str)) {
    *LOG.write().unwrap_or_else(|e| e.into_inner()) = Some(f);
}

pub(crate) fn log(line: &str) {
    if let Some(f) = *LOG.read().unwrap_or_else(|e| e.into_inner()) {
        f(line);
    }
}

// ---------------------------------------------------------------------------------------------
// The connection

/// How a call proves who it is.
enum Auth {
    /// The account's refresh token, and the access token it gave, with when it stops working.
    Refresh { app_key: String, refresh: String, access: Mutex<Option<(String, Instant)>> },
    /// A token as it is (tests, benchmarks).
    Fixed(String),
}

/// The body a call sends.
pub(crate) enum Body<'a> {
    Empty,
    Json(String),
    /// Bytes in memory, sent again when Dropbox asks to wait or the connection drops; the
    /// callback hears how many went in this try, and stops the call with [`crate::CANCELLED`]
    /// by answering `false`.
    Bytes(&'a [u8], Option<&'a mut dyn FnMut(usize) -> bool>),
}

/// Reads a [`Body::Bytes`] out for one try, telling its callback how far it got.
struct Sending<'a, 'b> {
    data: &'a [u8],
    at: usize,
    told: Option<&'a mut (dyn FnMut(usize) -> bool + 'b)>,
    stopped: bool,
}

impl Read for Sending<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = (self.data.len() - self.at).min(buf.len()).min(256 << 10);
        buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
        self.at += n;
        if n > 0 && let Some(told) = self.told.as_mut() && !told(self.at) {
            self.stopped = true;
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, crate::CANCELLED));
        }
        Ok(n)
    }
}

/// The connection to the API of one account.
pub struct Api {
    pub hosts: Hosts,
    auth: Auth,
    /// RPC calls: a minute at most.
    rpc: ureq::Agent,
    /// Downloads and uploads: no overall limit, only on connecting and on a stall.
    transfer: ureq::Agent,
    /// The longpoll, which holds the answer back up to its timeout.
    poll: ureq::Agent,
    /// `Dropbox-API-Path-Root`'s value, for an account whose root isn't its home.
    root: RwLock<Option<String>>,
    /// Dropbox's clock as its last answer gave it (`Date`, seconds since 1970), and when that
    /// answer came: see [`Api::server_now`].
    clock: Mutex<Option<(i64, Instant)>>,
    /// Requests sent and bytes of JSON received, for the benchmarks.
    pub requests: AtomicU64,
    pub received: AtomicU64,
    /// The longest `Retry-After` honoured, so a test isn't kept waiting.
    max_wait: Duration,
}

/// How many times a call is tried when Dropbox asks to wait (429, 503).
const WAITS: u32 = 6;
/// How many times a call is tried on a dropped connection or a 5xx.
const TRIES: u32 = 3;

fn agent(global: Option<Duration>, per_read: Option<Duration>) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(global)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_per_read(per_read)
        .timeout_recv_response(Some(Duration::from_secs(if global.is_some() { 120 } else { 300 })))
        .build()
        .into()
}

impl Api {
    /// An account's connection, from its refresh token.
    pub fn new(hosts: Hosts, app_key: &str, refresh: &str) -> Api {
        Api::with(hosts, Auth::Refresh { app_key: app_key.into(), refresh: refresh.into(), access: Mutex::new(None) })
    }

    /// One with a fixed access token (tests, benchmarks).
    pub fn fixed(hosts: Hosts, token: &str) -> Api {
        Api::with(hosts, Auth::Fixed(token.into()))
    }

    /// One with an access token already in hand (the one a login just got), renewed from
    /// `refresh` once it expires.
    pub fn logged_in(hosts: Hosts, app_key: &str, refresh: &str, access: &str, expires_in: u64) -> Api {
        let until = Instant::now() + Duration::from_secs(expires_in);
        Api::with(hosts, Auth::Refresh { app_key: app_key.into(), refresh: refresh.into(), access: Mutex::new(Some((access.into(), until))) })
    }

    fn with(hosts: Hosts, auth: Auth) -> Api {
        Api {
            hosts,
            auth,
            rpc: agent(Some(Duration::from_secs(90)), None),
            transfer: agent(None, Some(Duration::from_secs(120))),
            // The longest longpoll is 480 s, and Dropbox adds up to 90 s of jitter.
            poll: agent(Some(Duration::from_secs(480 + 120)), None),
            root: RwLock::new(None),
            clock: Mutex::new(None),
            requests: AtomicU64::new(0),
            received: AtomicU64::new(0),
            max_wait: Duration::from_secs(60),
        }
    }

    #[cfg(any(test, feature = "fake"))]
    pub(crate) fn short_waits(mut self) -> Api {
        self.max_wait = Duration::from_millis(50);
        self
    }

    /// The refresh token, to keep in the credential store.
    pub fn refresh_token(&self) -> Option<&str> {
        match &self.auth {
            Auth::Refresh { refresh, .. } => Some(refresh),
            Auth::Fixed(_) => None,
        }
    }

    /// Sets the namespace paths start from (`None`: the account's home).
    pub fn set_root(&self, namespace: Option<&str>) {
        *self.root.write().unwrap_or_else(|e| e.into_inner()) = namespace.map(|ns| serde_json::json!({".tag": "root", "root": ns}).to_string());
    }

    /// A valid access token: the one held, else a new one from the refresh token.
    fn token(&self, stale: Option<&str>) -> Result<String, Fail> {
        match &self.auth {
            Auth::Fixed(t) => Ok(t.clone()),
            Auth::Refresh { app_key, refresh, access } => {
                // Held while renewing, so calls that all found it expired renew it once.
                let mut held = access.lock().unwrap_or_else(|e| e.into_inner());
                if let Some((token, until)) = &*held
                    && Some(token.as_str()) != stale
                    && Instant::now() + Duration::from_secs(60) < *until
                {
                    return Ok(token.clone());
                }
                let fresh = crate::oauth::refresh(&self.rpc, &self.hosts.api, app_key, refresh)?;
                let token = fresh.access_token.clone();
                *held = Some((fresh.access_token, Instant::now() + Duration::from_secs(fresh.expires_in.max(60))));
                Ok(token)
            }
        }
    }

    /// Sends one call, trying again as the module docs say: `repeat` when a try whose answer
    /// was lost may go again (see [`repeatable`]). `auth` adds the token (and the path root);
    /// the longpoll goes without.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn exec(&self, agent: Agent, url: &str, arg: Option<&Value>, range: Option<u64>, body: &mut Body, auth: bool, repeat: bool) -> Result<ureq::http::Response<ureq::Body>, Fail> {
        let (agent, limit) = match agent {
            Agent::Rpc => (&self.rpc, None),
            Agent::Waiting(d) => (&self.rpc, Some(d)),
            Agent::Transfer => (&self.transfer, None),
            Agent::Poll => (&self.poll, None),
        };
        let arg = arg.map(|a| header_json(&a.to_string()));
        let mut refreshed: Option<String> = None;
        let (mut waits, mut tries) = (0u32, 0u32);
        // Whether a try may have reached Dropbox (its answer lost).
        let mut reached = false;
        loop {
            let mut req = agent.post(url);
            if let Some(d) = limit {
                req = req.config().timeout_global(Some(d)).timeout_recv_response(Some(d)).build();
            }
            let mut token = None;
            if auth {
                let tk = self.token(refreshed.as_deref())?;
                req = req.header("Authorization", &format!("Bearer {tk}"));
                if let Some(root) = &*self.root.read().unwrap_or_else(|e| e.into_inner()) {
                    req = req.header("Dropbox-API-Path-Root", root);
                }
                token = Some(tk);
            }
            if let Some(a) = &arg {
                req = req.header("Dropbox-API-Arg", a);
            }
            if let Some(from) = range {
                req = req.header("Range", &format!("bytes={from}-"));
            }
            self.requests.fetch_add(1, Ordering::Relaxed);
            let mut stopped = false;
            let sent = match body {
                Body::Empty => req.send_empty(),
                Body::Json(text) => req.header("Content-Type", "application/json").send(text.as_bytes()),
                Body::Bytes([], _) => req.header("Content-Type", "application/octet-stream").send(&[][..]),
                Body::Bytes(data, told) => {
                    let mut sending = Sending { data, at: 0, told: told.as_deref_mut(), stopped: false };
                    let sent = req.header("Content-Type", "application/octet-stream").header("Content-Length", &data.len().to_string()).send(ureq::SendBody::from_reader(&mut sending));
                    stopped = sending.stopped;
                    sent
                }
            };
            let mut resp = match sent {
                Ok(r) => r,
                Err(_) if stopped => return Err(Fail::Other(crate::CANCELLED.into())),
                Err(e) => {
                    tries += 1;
                    reached |= !never_sent(&e);
                    if !reached && tries >= TRIES {
                        // It never reached Dropbox: nothing to find out.
                        return Err(Fail::Other(net_text(&e)));
                    }
                    if tries >= TRIES || !(repeat || never_sent(&e)) {
                        log(&format!("Dropbox {url}: {e}, sem resposta"));
                        return Err(Fail::Lost(net_text(&e)));
                    }
                    log(&format!("Dropbox {url}: {e}, tentando de novo"));
                    std::thread::sleep(Duration::from_millis(500 << tries));
                    continue;
                }
            };
            if let Some(date) = resp.headers().get("Date").and_then(|v| v.to_str().ok()).and_then(http_date) {
                *self.clock.lock().unwrap_or_else(|e| e.into_inner()) = Some((date, Instant::now()));
            }
            let status = resp.status().as_u16();
            if (200..300).contains(&status) {
                return Ok(resp);
            }
            let header_wait = resp.headers().get("Retry-After").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let summary = json.get("error_summary").and_then(Value::as_str).unwrap_or("").to_string();
            match status {
                401 if summary.starts_with("expired_access_token") && refreshed.is_none() && matches!(self.auth, Auth::Refresh { .. }) => {
                    refreshed = token;
                }
                401 => {
                    log(&format!("Dropbox 401: {summary}"));
                    return Err(Fail::Auth(t("O acesso a esta conta do Dropbox foi revogado ou expirou. Remova a conta e conecte de novo.", "Access to this Dropbox account was revoked or expired. Remove the account and connect it again.").into()));
                }
                429 | 503 if waits + 1 < WAITS => {
                    waits += 1;
                    let asked = header_wait.or_else(|| json.pointer("/error/retry_after").and_then(Value::as_u64)).unwrap_or(1 << waits.min(5));
                    let wait = Duration::from_secs(asked.max(1)).min(self.max_wait);
                    log(&format!("Dropbox {status} ({summary}): esperando {} s", wait.as_secs_f32()));
                    std::thread::sleep(wait);
                }
                429 => return Err(Fail::Other(t("O Dropbox pediu para esperar: muitas operações ao mesmo tempo. Tente de novo em instantes.", "Dropbox asked to wait: too many operations at once. Try again in a moment.").into())),
                409 => return Err(Fail::Route { summary, error: json.get("error").cloned().unwrap_or(Value::Null) }),
                500..=599 if repeat && tries + 1 < TRIES => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(500 << tries));
                }
                // Dropbox's own failure midway (`internal_error`): whether the work was done is
                // for the caller to find out.
                s @ 500..=599 => {
                    log(&format!("Dropbox {url}: HTTP {s} ({summary})"));
                    return Err(Fail::Lost(tr!("O Dropbox respondeu com o erro HTTP {s}.", "Dropbox answered with HTTP error {s}.")));
                }
                400 => return Err(Fail::Other(tr!("O Dropbox recusou o pedido: {}", "Dropbox turned the request down: {}", text.chars().take(300).collect::<String>()))),
                403 => return Err(Fail::Other(tr!("O Dropbox negou o acesso: {}", "Dropbox denied access: {}", if summary.is_empty() { text.chars().take(300).collect::<String>() } else { summary }))),
                s => return Err(Fail::Other(tr!("O Dropbox respondeu com o erro HTTP {s}.", "Dropbox answered with HTTP error {s}."))),
            }
        }
    }

    /// Dropbox's clock now, in seconds since 1970: the `Date` of its latest answer plus the time
    /// since, else this computer's (before any answer). Whole seconds both, so up to a second
    /// behind: times Dropbox writes (`server_deleted`) compare with it without trusting this
    /// computer's clock.
    pub fn server_now(&self) -> i64 {
        match *self.clock.lock().unwrap_or_else(|e| e.into_inner()) {
            Some((date, at)) => date + at.elapsed().as_secs() as i64,
            None => std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
        }
    }

    /// An RPC route on the api host: `route` like "files/list_folder".
    pub fn rpc(&self, route: &str, arg: &Value) -> Result<Value, Fail> {
        let url = format!("{}/2/{route}", self.hosts.api);
        let resp = self.exec(Agent::Rpc, &url, None, None, &mut Body::Json(arg.to_string()), true, repeatable(route))?;
        self.read_json(resp)
    }

    /// [`Api::rpc`] of a route that answers only once its work is done, given `limit` to.
    pub fn rpc_waiting(&self, route: &str, arg: &Value, limit: Duration) -> Result<Value, Fail> {
        let url = format!("{}/2/{route}", self.hosts.api);
        let resp = self.exec(Agent::Waiting(limit), &url, None, None, &mut Body::Json(arg.to_string()), true, repeatable(route))?;
        self.read_json(resp)
    }

    /// The longpoll (no token: the cursor says which account).
    pub fn longpoll(&self, cursor: &str, timeout: u64) -> Result<Value, Fail> {
        let url = format!("{}/2/files/list_folder/longpoll", self.hosts.notify);
        let arg = serde_json::json!({"cursor": cursor, "timeout": timeout});
        let resp = self.exec(Agent::Poll, &url, None, None, &mut Body::Json(arg.to_string()), false, true)?;
        self.read_json(resp)
    }

    /// A content-download route: what `Dropbox-API-Result` says, and the body to read.
    pub fn download(&self, route: &str, arg: &Value, from: u64) -> Result<(Value, ureq::http::Response<ureq::Body>), Fail> {
        let url = format!("{}/2/{route}", self.hosts.content);
        let resp = self.exec(Agent::Transfer, &url, Some(arg), (from > 0).then_some(from), &mut Body::Empty, true, true)?;
        let result = resp.headers().get("Dropbox-API-Result").and_then(|v| v.to_str().ok()).and_then(|v| serde_json::from_str(v).ok()).unwrap_or(Value::Null);
        Ok((result, resp))
    }

    /// A content-upload route.
    pub(crate) fn upload(&self, route: &str, arg: &Value, body: &mut Body) -> Result<Value, Fail> {
        let url = format!("{}/2/{route}", self.hosts.content);
        let resp = self.exec(Agent::Transfer, &url, Some(arg), None, body, true, repeatable(route))?;
        self.read_json(resp)
    }

    fn read_json(&self, mut resp: ureq::http::Response<ureq::Body>) -> Result<Value, Fail> {
        // The work is done, but the answer saying how was cut short.
        let text = resp.body_mut().with_config().limit(64 << 20).read_to_string().map_err(|e| Fail::Lost(net_text(&e)))?;
        self.received.fetch_add(text.len() as u64, Ordering::Relaxed);
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| Fail::Other(tr!("Resposta inesperada do Dropbox: {e}", "Unexpected answer from Dropbox: {e}")))
    }
}

/// Which of the connection's agents a call goes through.
#[derive(Clone, Copy)]
pub(crate) enum Agent {
    Rpc,
    /// An RPC given this long to answer instead.
    Waiting(Duration),
    Transfer,
    Poll,
}

/// Whether a failed try never reached Dropbox: no address for it, no connection made.
fn never_sent(e: &ureq::Error) -> bool {
    matches!(e, ureq::Error::HostNotFound | ureq::Error::ConnectionFailed | ureq::Error::Timeout(ureq::Timeout::Resolve | ureq::Timeout::Connect))
        || matches!(e, ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::ConnectionRefused)
}

/// A network failure in the user's words.
pub(crate) fn net_text(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Timeout(_) => t("O Dropbox não respondeu a tempo.", "Dropbox didn't answer in time.").into(),
        ureq::Error::HostNotFound => t("Não encontrei o Dropbox: sem conexão com a internet?", "Couldn't find Dropbox: no internet connection?").into(),
        e => tr!("Falha ao falar com o Dropbox: {e}", "Couldn't talk to Dropbox: {e}"),
    }
}

/// Seconds since 1970 of an HTTP date, `Thu, 19 Nov 2026 17:50:38 GMT` (the only form HTTP/1.1
/// servers send).
pub fn http_date(text: &str) -> Option<i64> {
    let mut parts = text.split_whitespace().skip(1);
    let (day, month, year, time) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let m = MONTHS.iter().position(|x| *x == month)? + 1;
    let day: u32 = day.parse().ok()?;
    crate::meta::seconds(&format!("{year}-{m:02}-{day:02}T{time}Z"))
}

/// JSON text fit for an HTTP header: everything outside printable ASCII as a `\uXXXX` escape,
/// a character past U+FFFF as its two UTF-16 halves (what JSON calls for).
pub fn header_json(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if ('\x20'..'\x7f').contains(&c) {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for u in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_arguments_escape_all_but_printable_ascii_in_utf16() {
        let arg = serde_json::json!({"path": "/ação/😀.txt"}).to_string();
        let h = header_json(&arg);
        assert!(h.is_ascii());
        // A backslash, then u and four hex digits: UTF-16 halves for the emoji.
        let b = char::from(92);
        assert_eq!(h, format!("{{\"path\":\"/a{b}u00e7{b}u00e3o/{b}ud83d{b}ude00.txt\"}}"));
        // Read back as JSON, it's the same text.
        let back: Value = serde_json::from_str(&h).unwrap();
        assert_eq!(back["path"], "/ação/😀.txt");
    }

    #[test]
    fn http_dates_read_as_seconds() {
        assert_eq!(http_date("Thu, 12 May 2015 15:50:38 GMT"), Some(1_431_445_838));
        assert_eq!(http_date(&crate::fake::http_date(1_431_445_838)), Some(1_431_445_838));
        assert_eq!(http_date("nada"), None);
    }

    #[test]
    fn route_errors_read_as_sentences() {
        let f = Fail::Route { summary: "path/not_found/..".into(), error: Value::Null };
        assert!(f.is("not_found") && f.is("path") && !f.is("conflict"));
        assert!(f.text().contains("Não encontrado"));
        assert!(route_text("to/conflict/folder/...").contains("Já existe"));
        assert!(route_text("path/insufficient_space/").contains("espaço"));
        assert!(route_text("other_thing/").contains("other_thing"));
    }
}
