//! B2's native API, v4: `b2_authorize_account` with the key ID and the application key, then a
//! JSON POST per call to the `apiUrl` it gives, with its authorization token.
//!
//! Following Backblaze's integration checklist:
//! - a token that expired (or that B2 no longer takes) is renewed once, by authorizing again, and
//!   the call is sent again (`401 expired_auth_token` or `bad_auth_token`);
//! - `429 too_many_requests`, `503 service_unavailable`, the other 5xx and `408` are sent again
//!   after `Retry-After` when B2 gives one, else after a growing pause (1, 2, 4… s, at most 32);
//!   so is a request that got no answer; a 429 or a 503 also holds the reads the app makes on
//!   its own for that long (see [`crate::watch`]), and those are never sent again by themselves;
//! - `403` caps (`transaction_cap_exceeded` and the like) stop at once, saying where to raise them;
//! - every request says who sends it in `User-Agent`.
//!
//! Uploads go to URLs of their own (see [`crate::upload`]), whose failures ask for a new URL.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use base64::Engine;
use openexplorer_i18n::{t, tr};
use serde_json::Value;

/// Where an application key is authorized.
pub const AUTH_URL: &str = "https://api.backblazeb2.com";
/// The API version every call goes to.
pub const V: &str = "b2api/v4";

/// What a key reaches: its buckets (`None`: all of the account's), what it may do, and the
/// prefix every file name it touches must start with.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Allowed {
    /// Bucket ids and names; a name is `None` for a bucket deleted since the key was made.
    pub buckets: Option<Vec<(String, Option<String>)>>,
    pub capabilities: Vec<String>,
    pub name_prefix: Option<String>,
}

impl Allowed {
    pub fn can(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }
}

/// An authorization: the token and where to send calls.
#[derive(Clone, Debug)]
pub struct Auth {
    pub account_id: String,
    pub token: String,
    pub api_url: String,
    pub download_url: String,
    pub recommended_part: u64,
    pub minimum_part: u64,
    pub allowed: Allowed,
    /// Counts authorizations: a 401 seen under an older one doesn't renew again.
    pub generation: u64,
}

/// A call that failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fail {
    /// B2 answered with an error.
    Status { status: u16, code: String, message: String, retry_after: Option<u64> },
    /// No answer: the network, a timeout.
    Network(String),
    /// An answer that didn't read.
    Other(String),
}

impl Fail {
    pub fn code(&self) -> &str {
        match self {
            Fail::Status { code, .. } => code,
            _ => "",
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            Fail::Status { status, .. } => *status,
            _ => 0,
        }
    }

    /// The token is no longer taken: authorize again.
    pub fn expired(&self) -> bool {
        self.status() == 401 && matches!(self.code(), "expired_auth_token" | "bad_auth_token")
    }

    /// A cap the account set itself (Caps & Alerts) was reached: nothing more of the kind goes
    /// until it's raised or the day turns.
    pub fn capped(&self) -> bool {
        self.status() == 403 && matches!(self.code(), "transaction_cap_exceeded" | "cap_exceeded" | "download_cap_exceeded" | "storage_cap_exceeded")
    }

    /// What `Retry-After` said, in seconds.
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            Fail::Status { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// B2 asked to slow down, or is busy (429, 503).
    pub fn busy(&self) -> bool {
        matches!(self.status(), 429 | 503)
    }

    /// Worth sending again after a pause.
    pub fn transient(&self) -> bool {
        match self {
            Fail::Status { status, .. } => matches!(status, 408 | 429 | 500..=599),
            Fail::Network(_) => true,
            Fail::Other(_) => false,
        }
    }

    pub fn text(&self) -> String {
        match self {
            Fail::Status { status, code, message, .. } => error_text(*status, code, message),
            Fail::Network(e) => tr!("Sem conexão com o Backblaze B2: {e}", "No connection to Backblaze B2: {e}"),
            Fail::Other(e) => e.clone(),
        }
    }
}

impl From<Fail> for String {
    fn from(f: Fail) -> String {
        f.text()
    }
}

/// An error B2 answered, in the user's words.
pub fn error_text(status: u16, code: &str, message: &str) -> String {
    match code {
        "unauthorized" | "access_denied" => tr!(
            "A chave de aplicação não permite isso no B2 ({message}). Veja as permissões da chave no site da Backblaze.",
            "The application key doesn't allow that on B2 ({message}). Check the key's capabilities on the Backblaze website."
        ),
        "unsupported" => t("Esta chave só funciona com uma versão mais nova da API do B2.", "This key only works with a newer version of the B2 API.").into(),
        "transaction_cap_exceeded" | "cap_exceeded" | "storage_cap_exceeded" | "download_cap_exceeded" => tr!(
            "A conta B2 chegou a um limite que ela mesma definiu ({code}). Ajuste em Caps & Alerts, no site da Backblaze.",
            "The B2 account reached a cap it set itself ({code}). Change it under Caps & Alerts on the Backblaze website."
        ),
        "not_found" | "file_not_present" => t("Não está mais no B2 (foi apagado ou renomeado).", "It's no longer on B2 (deleted or renamed).").into(),
        "too_many_requests" => t("Pedidos demais ao B2. Espere um pouco e tente de novo.", "Too many requests to B2. Wait a little and try again.").into(),
        "service_unavailable" => t("O B2 está indisponível agora. Tente de novo em instantes.", "B2 is unavailable right now. Try again in a moment.").into(),
        "bad_bucket_id" | "invalid_bucket_id" => t("Esse bucket não existe mais no B2.", "That bucket no longer exists on B2.").into(),
        "bad_request" => tr!("O B2 recusou o pedido: {message}", "B2 turned the request down: {message}"),
        "expired_auth_token" | "bad_auth_token" => t("A autorização do B2 expirou e não pôde ser renovada.", "The B2 authorization expired and couldn't be renewed.").into(),
        _ => tr!("O B2 respondeu {status} {code}: {message}", "B2 answered {status} {code}: {message}"),
    }
}

/// Where request timings and events go: the app points it at its log.
static LOG: RwLock<Option<fn(&str)>> = RwLock::new(None);

pub fn set_log(f: fn(&str)) {
    *LOG.write().unwrap_or_else(|e| e.into_inner()) = Some(f);
}

pub(crate) fn log(line: &str) {
    if let Some(f) = *LOG.read().unwrap_or_else(|e| e.into_inner()) {
        f(line);
    }
}

/// What every request says it is: the product and its version, then the HTTP client.
pub fn user_agent() -> String {
    format!("OpenExplorer/{}+ureq/3 ({})", env!("CARGO_PKG_VERSION"), std::env::consts::OS)
}

/// An agent that hands back error statuses instead of failing, so B2's error bodies can be read.
pub(crate) fn agent(read: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(read))
        .timeout_per_read(Some(read))
        .user_agent(user_agent())
        .build()
        .into()
}

/// One application key's connection to the API, shared by every pane and transfer.
pub struct Api {
    pub(crate) agent: ureq::Agent,
    auth_base: String,
    key_id: String,
    key: String,
    auth: RwLock<Option<Arc<Auth>>>,
    /// Held while authorizing, with the count of authorizations so far.
    renewing: Mutex<u64>,
    /// The first pause before sending again; doubles each time.
    pub(crate) backoff: Duration,
    /// How many times a call is sent in all, at most, before its error stands.
    pub(crate) attempts: u32,
    /// `X-Bz-Test-Mode`, for tests against B2 itself.
    pub(crate) test_mode: Option<String>,
    /// Until when (seconds since 1970) B2 asked for a pause, by a 429 or a 503 to any call: the
    /// reads the app makes on its own wait for it (see [`crate::watch`]).
    pause_until: AtomicU64,
}

/// The pause after a 429 or a 503 that gave no `Retry-After`, in seconds.
pub const PAUSE: u64 = 60;

impl Api {
    pub fn new(key_id: &str, key: &str) -> Api {
        Api::at(AUTH_URL, key_id, key)
    }

    /// An API that authorizes at `base` (a stand-in for B2, in tests).
    pub fn at(base: &str, key_id: &str, key: &str) -> Api {
        Api {
            agent: agent(Duration::from_secs(120)),
            auth_base: base.trim_end_matches('/').to_string(),
            key_id: key_id.trim().to_string(),
            key: key.trim().to_string(),
            auth: RwLock::new(None),
            renewing: Mutex::new(0),
            backoff: Duration::from_secs(1),
            attempts: 6,
            test_mode: None,
            pause_until: AtomicU64::new(0),
        }
    }

    /// How long B2 asked to be left alone still (by a 429 or a 503 to any call), if it did.
    pub fn paused(&self, now: u64) -> Option<u64> {
        let until = self.pause_until.load(Ordering::SeqCst);
        (until > now).then(|| until - now)
    }

    fn note_busy(&self, f: &Fail) {
        if f.busy() {
            let until = crate::cache::now() + f.retry_after().unwrap_or(PAUSE).max(1);
            self.pause_until.fetch_max(until, Ordering::SeqCst);
        }
    }

    /// Sends B2's `X-Bz-Test-Mode` header (`fail_some_uploads`,
    /// `expire_some_account_authorization_tokens`…) with every request, for tests against B2.
    pub fn set_test_mode(&mut self, mode: Option<String>) {
        self.test_mode = mode.filter(|m| !m.is_empty());
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    /// The authorization in use, authorizing first if there's none.
    pub fn auth(&self) -> std::result::Result<Arc<Auth>, Fail> {
        if let Some(a) = self.auth.read().unwrap_or_else(|e| e.into_inner()).clone() {
            return Ok(a);
        }
        self.renew(0)
    }

    /// The authorization, if one was made, without asking B2.
    pub fn known_auth(&self) -> Option<Arc<Auth>> {
        self.auth.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Authorizes again, unless another thread already did since authorization `seen` (then
    /// its authorization is used).
    pub fn renew(&self, seen: u64) -> std::result::Result<Arc<Auth>, Fail> {
        let mut count = self.renewing.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(a) = self.auth.read().unwrap_or_else(|e| e.into_inner()).clone()
            && a.generation > seen
        {
            return Ok(a);
        }
        *count += 1;
        let auth = Arc::new(self.authorize(*count)?);
        *self.auth.write().unwrap_or_else(|e| e.into_inner()) = Some(auth.clone());
        Ok(auth)
    }

    fn authorize(&self, generation: u64) -> std::result::Result<Auth, Fail> {
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", self.key_id, self.key));
        let url = format!("{}/{V}/b2_authorize_account", self.auth_base);
        let v = self.retrying("b2_authorize_account", self.attempts, || {
            let mut req = self.agent.get(&url).header("Authorization", &format!("Basic {basic}"));
            if let Some(m) = &self.test_mode {
                req = req.header("X-Bz-Test-Mode", m);
            }
            answer(req.call())
        });
        let v = v.map_err(|f| match f.code() {
            // At authorization, "unauthorized" is the key itself.
            "unauthorized" | "bad_auth_token" => Fail::Other(t("O ID da chave ou a chave de aplicação estão errados (ou a chave foi apagada).", "The key ID or the application key is wrong (or the key was deleted).").into()),
            _ => f,
        })?;
        parse_auth(&v, generation).ok_or_else(|| Fail::Other(t("Resposta inesperada do B2 ao autorizar a chave.", "Unexpected answer from B2 authorizing the key.").into()))
    }

    /// Runs `send` until it succeeds, its error isn't worth another try, or `attempts` are up.
    pub(crate) fn retrying<T>(&self, what: &str, attempts: u32, mut send: impl FnMut() -> std::result::Result<T, Fail>) -> std::result::Result<T, Fail> {
        let mut delay = self.backoff;
        let mut attempt = 1;
        loop {
            let result = send();
            if let Err(f) = &result {
                self.note_busy(f);
            }
            match result {
                Ok(v) => return Ok(v),
                Err(f) if f.transient() && attempt < attempts => {
                    let wait = match &f {
                        Fail::Status { retry_after: Some(s), .. } => Duration::from_secs(*s).min(Duration::from_secs(120)),
                        _ => delay,
                    };
                    log(&format!("B2 {what}: {} — de novo em {} ms", f.text(), wait.as_millis()));
                    std::thread::sleep(wait);
                    delay = (delay * 2).min(Duration::from_secs(32));
                    attempt += 1;
                }
                Err(f) => return Err(f),
            }
        }
    }

    /// Calls `name` with the JSON `body`: sent again when B2 is busy, and with a new token when
    /// the one in use expired.
    pub fn call(&self, name: &str, body: &Value) -> std::result::Result<Value, Fail> {
        self.call_with(name, body, true)
    }

    /// [`Api::call`]; unless `patient`, a busy answer (429, 503, no answer) isn't sent again but
    /// comes back at once, for reads the app makes on its own, which wait for their next turn
    /// instead (an expired token is still renewed).
    pub fn call_with(&self, name: &str, body: &Value, patient: bool) -> std::result::Result<Value, Fail> {
        let bytes = serde_json::to_vec(body).expect("a JSON body");
        let attempts = if patient { self.attempts } else { 1 };
        let mut renewed = 0;
        loop {
            let auth = self.auth()?;
            let url = format!("{}/{V}/{name}", auth.api_url);
            let started = Instant::now();
            let result = self.retrying(name, attempts, || {
                let mut req = self.agent.post(&url).header("Authorization", &auth.token).header("Content-Type", "application/json");
                if let Some(m) = &self.test_mode {
                    req = req.header("X-Bz-Test-Mode", m);
                }
                answer(req.send(&bytes[..]))
            });
            log(&format!("B2 {name} {} em {} ms", result.as_ref().map_or_else(|f| f.code().to_string(), |_| "ok".into()), started.elapsed().as_millis()));
            match result {
                Err(f) if f.expired() && renewed < 2 => {
                    renewed += 1;
                    self.renew(auth.generation)?;
                }
                other => return other,
            }
        }
    }

    /// [`Api::call`] with the error in words.
    pub fn run(&self, name: &str, body: &Value) -> crate::Result<Value> {
        self.call(name, body).map_err(String::from)
    }
}

/// An answer's JSON, or the error it says.
pub(crate) fn answer(r: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> std::result::Result<Value, Fail> {
    let mut resp = r.map_err(|e| Fail::Network(e.to_string()))?;
    let status = resp.status().as_u16();
    if status == 200 {
        let text = resp.body_mut().with_config().limit(256 * 1024 * 1024).read_to_string().map_err(|e| Fail::Network(e.to_string()))?;
        return serde_json::from_str(&text).map_err(|e| Fail::Other(tr!("Resposta inesperada do B2: {e}", "Unexpected answer from B2: {e}")));
    }
    Err(failure(&mut resp))
}

/// The error a response other than 200 carries: B2's `{status, code, message}`.
pub(crate) fn failure(resp: &mut ureq::http::Response<ureq::Body>) -> Fail {
    let status = resp.status().as_u16();
    let retry_after = resp.headers().get("Retry-After").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let code = v.get("code").and_then(Value::as_str).unwrap_or("").to_string();
    let message = v.get("message").and_then(Value::as_str).unwrap_or("").to_string();
    Fail::Status { status, code, message, retry_after }
}

fn parse_auth(v: &Value, generation: u64) -> Option<Auth> {
    let api = v.get("apiInfo")?.get("storageApi")?;
    let s = |o: &Value, k: &str| o.get(k).and_then(Value::as_str).map(|x| x.trim_end_matches('/').to_string());
    let allowed = api.get("allowed").cloned().unwrap_or(Value::Null);
    let buckets = allowed.get("buckets").and_then(Value::as_array).map(|list| {
        list.iter().filter_map(|b| Some((b.get("id").and_then(Value::as_str)?.to_string(), b.get("name").and_then(Value::as_str).map(str::to_string)))).collect()
    });
    let capabilities = allowed.get("capabilities").and_then(Value::as_array).map(|c| c.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
    let name_prefix = allowed.get("namePrefix").and_then(Value::as_str).filter(|p| !p.is_empty()).map(str::to_string);
    Some(Auth {
        account_id: v.get("accountId")?.as_str()?.to_string(),
        token: v.get("authorizationToken")?.as_str()?.to_string(),
        api_url: s(api, "apiUrl")?,
        download_url: s(api, "downloadUrl")?,
        recommended_part: api.get("recommendedPartSize").and_then(Value::as_u64).unwrap_or(100_000_000),
        minimum_part: api.get("absoluteMinimumPartSize").and_then(Value::as_u64).unwrap_or(5_000_000),
        allowed: Allowed { buckets, capabilities, name_prefix },
        generation,
    })
}

/// A file name as B2 takes it in `X-Bz-File-Name` and in download URLs: UTF-8, every byte but
/// letters, digits and ``._-/~!$'()*;=:@`` percent-encoded (`/` never).
pub fn encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"._-/~!$'()*;=:@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The SHA-1 of `data` in hex, as B2 writes it.
pub fn sha1_hex(data: &[u8]) -> String {
    use sha1::Digest;
    hex(&sha1::Sha1::digest(data))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_encoded_as_b2_asks() {
        // The examples of B2's "String Encoding" page.
        assert_eq!(encode("日本語"), "%E6%97%A5%E6%9C%AC%E8%AA%9E");
        assert_eq!(encode("a b+c&d"), "a%20b%2Bc%26d");
        assert_eq!(encode("fotos/2024/praia (1).jpg"), "fotos/2024/praia%20(1).jpg");
        assert_eq!(encode("~!$'()*;=:@._-"), "~!$'()*;=:@._-");
        assert_eq!(encode("100%"), "100%25");
    }

    #[test]
    fn sha1_is_hex() {
        // FIPS 180's "abc".
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn an_authorization_reads_with_its_restrictions() {
        let v: Value = serde_json::from_str(
            r#"{"accountId":"acc","authorizationToken":"tok","applicationKeyExpirationTimestamp":null,
            "apiInfo":{"storageApi":{"absoluteMinimumPartSize":5000000,"apiUrl":"https://api001.backblazeb2.com/","downloadUrl":"https://f001.backblazeb2.com",
            "recommendedPartSize":100000000,"s3ApiUrl":"https://s3.us-west-001.backblazeb2.com",
            "allowed":{"buckets":[{"id":"b1","name":"fotos"},{"id":"b2","name":null}],"capabilities":["listBuckets","listFiles","readFiles"],"namePrefix":"ana/"}}}}"#,
        )
        .unwrap();
        let a = parse_auth(&v, 3).unwrap();
        assert_eq!((a.account_id.as_str(), a.token.as_str(), a.api_url.as_str(), a.generation), ("acc", "tok", "https://api001.backblazeb2.com", 3));
        assert_eq!((a.recommended_part, a.minimum_part), (100_000_000, 5_000_000));
        assert_eq!(a.allowed.buckets, Some(vec![("b1".into(), Some("fotos".into())), ("b2".into(), None)]));
        assert_eq!(a.allowed.name_prefix.as_deref(), Some("ana/"));
        assert!(a.allowed.can("readFiles") && !a.allowed.can("writeFiles"));
        // A key of the whole account.
        let all: Value = serde_json::from_str(r#"{"accountId":"a","authorizationToken":"t","apiInfo":{"storageApi":{"apiUrl":"u","downloadUrl":"d","allowed":{"buckets":null,"capabilities":[],"namePrefix":null}}}}"#).unwrap();
        let a = parse_auth(&all, 1).unwrap();
        assert_eq!((a.allowed.buckets, a.allowed.name_prefix), (None, None));
    }

    #[test]
    fn errors_read_in_words() {
        let f = Fail::Status { status: 403, code: "transaction_cap_exceeded".into(), message: "x".into(), retry_after: None };
        assert!(f.text().contains("Caps & Alerts") && !f.transient());
        let f = Fail::Status { status: 401, code: "expired_auth_token".into(), message: String::new(), retry_after: None };
        assert!(f.expired() && !f.transient());
        assert!(Fail::Status { status: 503, code: "service_unavailable".into(), message: String::new(), retry_after: Some(1) }.transient());
        assert!(Fail::Status { status: 429, code: "too_many_requests".into(), message: String::new(), retry_after: None }.transient());
        assert!(Fail::Network("x".into()).transient());
        assert!(!Fail::Status { status: 400, code: "bad_request".into(), message: "m".into(), retry_after: None }.transient());
    }
}
