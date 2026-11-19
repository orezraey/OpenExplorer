//! Talking to Microsoft Graph: the access token (renewed from the refresh token, which Microsoft
//! replaces at every renewal), throttling, errors read as sentences, and `$batch`.
//!
//! A 429 or 503 pauses every request of the account for as long as `Retry-After` says (OneDrive
//! asks for that: going on meanwhile gets the app blocked), then the request is sent again, a few
//! times at most.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use openexplorer_i18n::{t, tr};
use serde_json::{Value, json};

use crate::{ClientConfig, log};

pub const GRAPH: &str = "https://graph.microsoft.com/v1.0";
pub const LOGIN: &str = "https://login.microsoftonline.com";

/// Where Graph and the login are: Microsoft's, or a stand-in in tests.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub graph: String,
    pub login: String,
}

impl Default for Endpoints {
    fn default() -> Endpoints {
        Endpoints { graph: GRAPH.into(), login: LOGIN.into() }
    }
}

/// An answer Graph (or the login) gave that isn't a success: its status and error code, and
/// the sentence the user reads.
#[derive(Clone, Debug, PartialEq)]
pub struct Fail {
    pub status: u16,
    pub code: String,
    pub text: String,
}

impl Fail {
    pub fn new(text: impl Into<String>) -> Fail {
        Fail { status: 0, code: String::new(), text: text.into() }
    }

    pub fn is(&self, code: &str) -> bool {
        self.code.eq_ignore_ascii_case(code)
    }

    pub fn not_found(&self) -> bool {
        self.status == 404 || self.is("itemNotFound")
    }

    pub fn exists(&self) -> bool {
        self.status == 409 || self.is("nameAlreadyExists")
    }
}

impl From<Fail> for String {
    fn from(f: Fail) -> String {
        f.text
    }
}

impl From<String> for Fail {
    fn from(text: String) -> Fail {
        Fail::new(text)
    }
}

pub type Res<T> = std::result::Result<T, Fail>;

/// The sentence for an error body Graph sent (`{"error": {"code", "message"}}`).
pub fn failure(status: u16, body: &str, what: &str) -> Fail {
    let v: Value = serde_json::from_str(body).unwrap_or_default();
    let code = v.pointer("/error/code").and_then(Value::as_str).unwrap_or_default().to_string();
    let message = v.pointer("/error/message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| body.chars().take(300).collect());
    let text = match (status, code.as_str()) {
        (_, "itemNotFound") | (404, _) => tr!("{what}: o item não existe (mais) no OneDrive.", "{what}: the item doesn't exist (any more) on OneDrive."),
        (_, "nameAlreadyExists") | (409, _) => tr!("{what}: já existe um item com esse nome.", "{what}: an item with that name already exists."),
        (_, "accessDenied") | (403, _) => tr!("{what}: o OneDrive negou o acesso ({message}).", "{what}: OneDrive denied access ({message})."),
        (_, "quotaLimitReached") | (507, _) => tr!("{what}: não há espaço livre no OneDrive.", "{what}: there's no free space on OneDrive."),
        (423, _) => tr!("{what}: o item está bloqueado (aberto em outro lugar?).", "{what}: the item is locked (open elsewhere?)."),
        _ => tr!("{what}: o OneDrive respondeu {status}: {message}", "{what}: OneDrive answered {status}: {message}"),
    };
    Fail { status, code, text }
}

/// The sentence for a login error (`error`, `error_description` with an AADSTS code).
pub fn login_failure(status: u16, body: &str) -> Fail {
    let v: Value = serde_json::from_str(body).unwrap_or_default();
    let code = v.get("error").and_then(Value::as_str).unwrap_or_default().to_string();
    let desc = v.get("error_description").and_then(Value::as_str).unwrap_or(body);
    let first = desc.lines().next().unwrap_or_default().to_string();
    let has = |c: &str| desc.contains(c);
    let text = if has("AADSTS7000218") || has("AADSTS700025") {
        t(
            "O registro do aplicativo não está como cliente público: em Autenticação, no portal do Microsoft Entra, ligue \"Permitir fluxos de clientes públicos\" e confira que http://localhost está na plataforma \"Aplicativos móveis e da área de trabalho\" (não em \"Web\").",
            "The app registration isn't a public client: under Authentication, in the Microsoft Entra portal, turn on \"Allow public client flows\" and check that http://localhost is under the platform \"Mobile and desktop applications\" (not \"Web\").",
        )
        .into()
    } else if has("AADSTS50011") {
        t("O registro do aplicativo não tem o URI de redirecionamento http://localhost (plataforma \"Aplicativos móveis e da área de trabalho\").", "The app registration lacks the redirect URI http://localhost (platform \"Mobile and desktop applications\").").into()
    } else if has("AADSTS700016") {
        t("Esse client_id não existe no diretório da conta: confira o ID e os tipos de conta do registro (pessoais e organizacionais).", "That client_id doesn't exist in the account's directory: check the ID and the registration's account types (personal and organizational).").into()
    } else if has("AADSTS65001") || has("AADSTS90094") || has("AADSTS90095") {
        t("A organização da conta exige que um administrador aprove o aplicativo antes do uso.", "The account's organization requires an administrator to approve the app first.").into()
    } else if code == "invalid_grant" || has("AADSTS70008") || has("AADSTS700082") || has("AADSTS50173") {
        t("O acesso a esta conta do OneDrive expirou ou foi revogado. Remova a conta e conecte de novo.", "Access to this OneDrive account expired or was revoked. Remove the account and connect it again.").into()
    } else {
        tr!("A Microsoft recusou o login ({status}): {first}", "Microsoft refused the login ({status}): {first}")
    };
    Fail { status, code, text }
}

/// An answer as it came: status, the headers that matter, the body.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub location: Option<String>,
    pub retry_after: Option<u64>,
    pub body: String,
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn json(&self, what: &str) -> Res<Value> {
        if !self.ok() {
            return Err(failure(self.status, &self.body, what));
        }
        if self.body.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&self.body).map_err(|e| Fail::new(tr!("{what}: resposta inesperada: {e}", "{what}: unexpected answer: {e}")))
    }
}

pub(crate) fn retry_after(resp: &ureq::http::Response<ureq::Body>) -> Option<u64> {
    resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse().ok())
}

/// The message for a request that never got an answer.
pub(crate) fn unreachable(e: &ureq::Error) -> Fail {
    Fail::new(tr!("Não consegui falar com o OneDrive: {e}", "Couldn't talk to OneDrive: {e}"))
}

struct Tokens {
    access: String,
    expires: Instant,
    refresh: String,
}

/// How many times a throttled or failed request is sent again.
const TRIES: u32 = 6;
/// The longest one pause lasts, whatever `Retry-After` says.
const LONGEST_PAUSE: u64 = 120;

/// One account's way to Graph.
pub struct Graph {
    pub ends: Endpoints,
    cfg: ClientConfig,
    agent: ureq::Agent,
    tokens: Mutex<Tokens>,
    /// Until when every request waits (throttled).
    pause: Mutex<Option<Instant>>,
    /// Called with each new refresh token, under the token lock, to keep it.
    rotated: Box<dyn Fn(&str) + Send + Sync>,
}

/// The agent API calls go through: errors are answers to read, redirects are read by hand
/// (a download's address), and nothing waits past a minute.
pub(crate) fn api_agent() -> ureq::Agent {
    ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0).timeout_global(Some(Duration::from_secs(60))).build().into()
}

/// The agent the pre-authenticated addresses (downloads, upload sessions, copy monitors) go
/// through: no overall limit, but a stalled connection gives up.
pub(crate) fn transfer_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_per_write(Some(Duration::from_secs(120)))
        .timeout_recv_response(Some(Duration::from_secs(300)))
        .timeout_per_read(Some(Duration::from_secs(120)))
        .build()
        .into()
}

/// What a login's tokens answer holds.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct TokenAnswer {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: u64,
}

/// Posts a form to the token endpoint.
pub(crate) fn token_call(agent: &ureq::Agent, ends: &Endpoints, cfg: &ClientConfig, form: &[(&str, &str)]) -> Res<TokenAnswer> {
    let url = format!("{}/{}/oauth2/v2.0/token", ends.login, cfg.tenant);
    let mut resp = agent.post(&url).send_form(form.iter().copied()).map_err(|e| Fail::new(tr!("Não consegui falar com a Microsoft: {e}", "Couldn't talk to Microsoft: {e}")))?;
    let status = resp.status().as_u16();
    let body = resp.body_mut().read_to_string().map_err(|e| Fail::new(e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(login_failure(status, &body));
    }
    serde_json::from_str(&body).map_err(|e| Fail::new(tr!("Resposta inesperada do login: {e}", "Unexpected answer from the login: {e}")))
}

impl Graph {
    /// A connection that gets its first access token from `refresh` when first used.
    pub fn new(cfg: ClientConfig, ends: Endpoints, refresh: String, rotated: Box<dyn Fn(&str) + Send + Sync>) -> Graph {
        let tokens = Tokens { access: String::new(), expires: Instant::now(), refresh };
        Graph { ends, cfg, agent: api_agent(), tokens: Mutex::new(tokens), pause: Mutex::new(None), rotated }
    }

    /// One fresh from a login: its access token used until it expires.
    pub fn from_login(cfg: ClientConfig, ends: Endpoints, answer: &TokenAnswer, rotated: Box<dyn Fn(&str) + Send + Sync>) -> Res<Graph> {
        let refresh = answer.refresh_token.clone().ok_or_else(|| Fail::new(t("A Microsoft não deu acesso duradouro (offline_access).", "Microsoft gave no lasting access (offline_access).")))?;
        let g = Graph::new(cfg, ends, refresh, rotated);
        {
            let mut tk = g.tokens.lock().unwrap_or_else(|e| e.into_inner());
            tk.access = answer.access_token.clone();
            tk.expires = Instant::now() + Duration::from_secs(answer.expires_in.max(60));
        }
        Ok(g)
    }

    pub fn refresh_token(&self) -> String {
        self.tokens.lock().unwrap_or_else(|e| e.into_inner()).refresh.clone()
    }

    /// The access token: the one held while it has a minute left, else a new one. `stale` is
    /// one Graph turned down: renewed unless another thread renewed it meanwhile.
    fn access(&self, stale: Option<&str>) -> Res<String> {
        let mut tk = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        let turned_down = stale.is_some_and(|s| s == tk.access);
        if !turned_down && !tk.access.is_empty() && tk.expires > Instant::now() + Duration::from_secs(60) {
            return Ok(tk.access.clone());
        }
        let refresh = tk.refresh.clone();
        let answer = token_call(&self.agent, &self.ends, &self.cfg, &[("client_id", self.cfg.client_id.as_str()), ("grant_type", "refresh_token"), ("refresh_token", refresh.as_str()), ("scope", crate::oauth::SCOPES)])?;
        tk.access = answer.access_token;
        tk.expires = Instant::now() + Duration::from_secs(answer.expires_in.max(60));
        if let Some(new) = answer.refresh_token.filter(|r| *r != tk.refresh) {
            // Kept before the old one is let go: Microsoft replaces it at every renewal.
            (self.rotated)(&new);
            tk.refresh = new;
        }
        Ok(tk.access.clone())
    }

    /// Waits out a pause another request was told to make.
    fn wait_pause(&self) {
        let until = *self.pause.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(until) = until {
            let now = Instant::now();
            if until > now {
                std::thread::sleep(until - now);
            }
        }
    }

    /// Pauses every request for `secs`.
    pub(crate) fn throttled(&self, secs: u64) {
        let until = Instant::now() + Duration::from_secs(secs.min(LONGEST_PAUSE));
        let mut p = self.pause.lock().unwrap_or_else(|e| e.into_inner());
        if p.is_none_or(|old| old < until) {
            *p = Some(until);
        }
        log(&format!("OneDrive pediu pausa de {secs} s"));
    }

    pub fn url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") { path.to_string() } else { format!("{}{path}", self.ends.graph) }
    }

    /// Sends a request with the access token, again after a renewal (401) or as long as it's
    /// throttled (429, 503) or Graph stumbled (502, 504), a few times at most. A request with
    /// no answer at all is sent again only when it reads (`GET`).
    pub fn send(&self, method: &str, path: &str, body: Body<'_>, headers: &[(&str, &str)]) -> Res<Reply> {
        let url = self.url(path);
        let bytes: Option<(std::borrow::Cow<'_, [u8]>, &str)> = match body {
            Body::None => None,
            Body::Json(v) => Some((serde_json::to_vec(v).expect("JSON values serialize").into(), "application/json")),
            Body::Raw(b, kind) => Some((b.into(), kind)),
        };
        let mut stale: Option<String> = None;
        let mut renewed = false;
        for attempt in 0..TRIES {
            self.wait_pause();
            let token = self.access(stale.as_deref())?;
            let started = Instant::now();
            let mut req = ureq::http::Request::builder().method(method).uri(&url).header("Authorization", format!("Bearer {token}"));
            for (k, v) in headers {
                req = req.header(*k, *v);
            }
            let resp = match &bytes {
                Some((b, kind)) => self.agent.run(req.header("Content-Type", *kind).body(&b[..]).map_err(|e| Fail::new(e.to_string()))?),
                None if method == "GET" || method == "DELETE" => self.agent.run(req.body(()).map_err(|e| Fail::new(e.to_string()))?),
                None => self.agent.run(req.body(Vec::new()).map_err(|e| Fail::new(e.to_string()))?),
            };
            let mut resp = match resp {
                Ok(r) => r,
                Err(e) if method == "GET" && attempt + 1 < TRIES => {
                    log(&format!("OneDrive {method} sem resposta ({e}); de novo"));
                    std::thread::sleep(Duration::from_millis(500 << attempt.min(4)));
                    continue;
                }
                Err(e) => return Err(unreachable(&e)),
            };
            let status = resp.status().as_u16();
            let reply = Reply {
                status,
                location: resp.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_string),
                retry_after: retry_after(&resp),
                body: resp.body_mut().read_to_string().unwrap_or_default(),
            };
            log(&format!("OneDrive {method} {} → {status} em {} ms", short(&url), started.elapsed().as_millis()));
            match status {
                401 if !renewed => {
                    renewed = true;
                    stale = Some(token);
                }
                429 | 503 | 502 | 504 if attempt + 1 < TRIES => {
                    let secs = reply.retry_after.unwrap_or(1 << attempt.min(5));
                    self.throttled(secs);
                }
                _ => return Ok(reply),
            }
        }
        Err(Fail::new(t("O OneDrive está ocupado demais agora; tente de novo em alguns minutos.", "OneDrive is too busy right now; try again in a few minutes.")))
    }

    /// [`Graph::send`], the answer read as JSON, an error status as its sentence.
    pub fn call(&self, method: &str, path: &str, body: Option<&Value>, what: &str) -> Res<Value> {
        self.send(method, path, body.map_or(Body::None, Body::Json), &[])?.json(what)
    }

    pub fn get(&self, path: &str, what: &str) -> Res<Value> {
        self.call("GET", path, None, what)
    }

    /// Several small requests in one (`$batch`, 20 at most each): the answers in the order
    /// asked, each `(status, body)`. Those throttled are asked again after the longest
    /// `Retry-After` among them.
    pub fn batch(&self, subs: &[Sub], what: &str) -> Res<Vec<(u16, Value)>> {
        let mut out: Vec<Option<(u16, Value)>> = vec![None; subs.len()];
        let mut todo: Vec<usize> = (0..subs.len()).collect();
        for round in 0..TRIES {
            if todo.is_empty() {
                break;
            }
            let mut again = Vec::new();
            let mut wait = 0;
            for chunk in todo.chunks(20) {
                let requests: Vec<Value> = chunk
                    .iter()
                    .map(|&i| {
                        let s = &subs[i];
                        let mut r = json!({ "id": i.to_string(), "method": s.method, "url": s.url });
                        if let Some(b) = &s.body {
                            r["body"] = b.clone();
                            r["headers"] = json!({ "Content-Type": "application/json" });
                        }
                        r
                    })
                    .collect();
                let answer = self.call("POST", "/$batch", Some(&json!({ "requests": requests })), what)?;
                for r in answer.get("responses").and_then(Value::as_array).into_iter().flatten() {
                    let Some(i) = r.get("id").and_then(Value::as_str).and_then(|s| s.parse::<usize>().ok()).filter(|i| *i < subs.len()) else { continue };
                    let status = r.get("status").and_then(Value::as_u64).unwrap_or(0) as u16;
                    if matches!(status, 429 | 503) && round + 1 < TRIES {
                        let secs = r.pointer("/headers/Retry-After").or_else(|| r.pointer("/headers/retry-after")).and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or(v.as_u64())).unwrap_or(1 << round.min(5));
                        wait = wait.max(secs);
                        again.push(i);
                        continue;
                    }
                    out[i] = Some((status, r.get("body").cloned().unwrap_or(Value::Null)));
                }
            }
            if wait > 0 {
                self.throttled(wait);
            }
            todo = again;
        }
        Ok(out.into_iter().map(|o| o.unwrap_or((0, Value::Null))).collect())
    }
}

/// What a request carries.
#[derive(Clone, Copy)]
pub enum Body<'a> {
    None,
    Json(&'a Value),
    /// Bytes of the given content type.
    Raw(&'a [u8], &'a str),
}

/// One request of a `$batch`: its address below the version (`/me/drive/items/…`).
#[derive(Clone, Debug)]
pub struct Sub {
    pub method: &'static str,
    pub url: String,
    pub body: Option<Value>,
}

/// An address for the log, without the query (it can carry tokens).
fn short(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

/// `name` as one segment of a Graph path (`root:/a/b:`): percent-encoded but for unreserved
/// characters, so `#`, `%`, `?` and spaces can't be read as something else.
pub fn segment(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_read_as_sentences() {
        let f = failure(404, r#"{"error":{"code":"itemNotFound","message":"The resource could not be found."}}"#, "Abrir");
        assert!(f.not_found() && f.text.starts_with("Abrir: o item não existe"));
        let f = failure(409, r#"{"error":{"code":"nameAlreadyExists","message":"x"}}"#, "Criar");
        assert!(f.exists());
        let f = failure(500, "oops", "Listar");
        assert!(f.text.contains("500") && f.text.contains("oops"));
        let l = login_failure(400, r#"{"error":"invalid_client","error_description":"AADSTS7000218: The request body must contain the following parameter: 'client_assertion' or 'client_secret'."}"#);
        assert!(l.text.contains("Permitir fluxos de clientes públicos"));
        let l = login_failure(400, r#"{"error":"invalid_grant","error_description":"AADSTS70008: expired"}"#);
        assert!(l.text.contains("expirou"));
    }

    #[test]
    fn segments_escape_what_paths_would_misread() {
        assert_eq!(segment("Relatório #2 (final)?.docx"), "Relat%C3%B3rio%20%232%20(final)%3F.docx");
        assert_eq!(segment("100%"), "100%25");
        assert_eq!(segment("a/b"), "a%2Fb");
    }
}
