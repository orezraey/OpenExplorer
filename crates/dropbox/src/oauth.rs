//! OAuth 2.0 code flow with PKCE, the way the Dropbox OAuth guide describes it for an app that
//! can't keep a secret: the browser signs in at `www.dropbox.com/oauth2/authorize` and comes back
//! to a one-shot listener on this machine, and the code it carries is exchanged with the PKCE
//! verifier for a short-lived access token and, with `token_access_type=offline`, a refresh
//! token. Refreshing sends only the app key and the refresh token.
//!
//! Dropbox matches the redirect URI exactly, port included, so the listener takes the fixed port
//! of [`crate::AppConfig`] and the app must have `http://localhost:<port>` registered. `localhost`
//! may reach the listener over IPv4 or IPv6, so it listens on both loopbacks.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use openexplorer_i18n::{t, tr};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::api::Fail;
use crate::{AppConfig, Result};

const AUTHORIZE: &str = "https://www.dropbox.com/oauth2/authorize";
/// How long the listener waits for the browser.
const WAIT: Duration = Duration::from_secs(5 * 60);
/// What the app asks for: browsing and changing every file, shared links, and the account's
/// e-mail and storage. Each must be ticked on the app's Permissions tab too.
pub const SCOPES: &str = "account_info.read files.metadata.read files.metadata.write files.content.read files.content.write sharing.read sharing.write";

#[derive(Clone, Debug, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub expires_in: u64,
    /// Sent on the login (offline access), not on refreshes.
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
}

/// A login waiting for the browser: open [`PendingLogin::url`], then call
/// [`PendingLogin::finish`] on a background thread.
pub struct PendingLogin {
    listeners: Vec<TcpListener>,
    redirect: String,
    verifier: String,
    state: String,
    pub url: String,
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 48];
    getrandom::fill(&mut bytes).map_err(|e| tr!("Sem gerador aleatório do sistema: {e}", "No random generator from the system: {e}"))?;
    // 64 characters of [A-Za-z0-9-_]: within PKCE's 43 to 128.
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Opens the loopback listener on the app's port and builds the authorization URL.
pub fn begin(cfg: &AppConfig) -> Result<PendingLogin> {
    let v4 = TcpListener::bind(("127.0.0.1", cfg.port)).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            tr!(
                "A porta {} do login já está em uso (outro login aberto?). Feche o outro programa ou escolha outra porta em dropbox-client.json, registrando-a no app do Dropbox.",
                "The login's port {} is already in use (another login open?). Close the other program or pick another port in dropbox-client.json, registering it in the Dropbox app.",
                cfg.port
            )
        } else {
            tr!("Não consegui abrir a porta local do login: {e}", "Couldn't open the login's local port: {e}")
        }
    })?;
    let mut listeners = vec![v4];
    // Where the system has IPv6, `localhost` may well go there first.
    if let Ok(v6) = TcpListener::bind(("::1", cfg.port)) {
        listeners.push(v6);
    }
    let redirect = cfg.redirect();
    let verifier = random_token()?;
    let state = random_token()?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let url = url::Url::parse_with_params(
        AUTHORIZE,
        &[
            ("client_id", cfg.app_key.as_str()),
            ("response_type", "code"),
            ("redirect_uri", redirect.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", state.as_str()),
            ("token_access_type", "offline"),
            ("scope", SCOPES),
            // Another account may be signed in on the website: ask which.
            ("force_reapprove", "true"),
        ],
    )
    .map_err(|e| e.to_string())?
    .to_string();
    Ok(PendingLogin { listeners, redirect, verifier, state, url })
}

impl PendingLogin {
    /// Waits for the browser's redirect (up to five minutes) and exchanges its code for tokens
    /// at `api` (the api host: `https://api.dropboxapi.com`).
    pub fn finish(self, cfg: &AppConfig, api: &str) -> Result<Tokens> {
        self.finish_until(cfg, api, &|| false)
    }

    /// [`PendingLogin::finish`], given up (and the port freed) once `stop` says so: a newer
    /// login began.
    pub fn finish_until(self, cfg: &AppConfig, api: &str, stop: &dyn Fn() -> bool) -> Result<Tokens> {
        let code = self.wait_for_code(stop)?;
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(60))).build().into();
        let resp = agent
            .post(&format!("{api}/oauth2/token"))
            .send_form([
                ("code", code.as_str()),
                ("grant_type", "authorization_code"),
                ("client_id", cfg.app_key.as_str()),
                ("redirect_uri", self.redirect.as_str()),
                ("code_verifier", self.verifier.as_str()),
            ])
            .map_err(|e| crate::api::net_text(&e))?;
        let tokens: Tokens = token_answer(resp).map_err(String::from)?;
        if tokens.refresh_token.is_none() {
            return Err(t("O Dropbox não mandou um token de renovação (acesso offline).", "Dropbox didn't send a refresh token (offline access).").into());
        }
        Ok(tokens)
    }

    fn wait_for_code(&self, stop: &dyn Fn() -> bool) -> Result<String> {
        for l in &self.listeners {
            l.set_nonblocking(true).map_err(|e| e.to_string())?;
        }
        let deadline = Instant::now() + WAIT;
        loop {
            let mut idle = true;
            for l in &self.listeners {
                match l.accept() {
                    Ok((stream, _)) => {
                        idle = false;
                        // The browser may also ask for /favicon.ico; only the redirect has a query.
                        if let Some(answer) = self.answer(stream)? {
                            return answer;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(tr!("Erro na porta local do login: {e}", "Error on the login's local port: {e}")),
                }
            }
            if idle {
                if stop() {
                    return Err(t("Login substituído por um novo.", "Login replaced by a new one.").into());
                }
                if Instant::now() > deadline {
                    return Err(t("O login expirou: o navegador não voltou em 5 minutos.", "The login expired: the browser didn't come back within 5 minutes.").into());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    /// Reads one request. `None` when it isn't the redirect.
    fn answer(&self, mut stream: TcpStream) -> Result<Option<Result<String>>> {
        stream.set_nonblocking(false).map_err(|e| e.to_string())?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut line = String::new();
        if BufReader::new(&stream).read_line(&mut line).is_err() {
            return Ok(None);
        }
        // "GET /?code=..&state=.. HTTP/1.1"
        let target = line.split_whitespace().nth(1).unwrap_or("");
        let Ok(url) = url::Url::parse(&format!("http://localhost{target}")) else { return Ok(None) };
        let q = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
        let (code, error, state) = (q("code"), q("error"), q("state"));
        if code.is_none() && error.is_none() {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            return Ok(None);
        }
        let result = if state.as_deref() != Some(self.state.as_str()) {
            Err(t("O login voltou com um estado diferente do pedido; tente de novo.", "The login came back with a different state than asked; try again.").to_string())
        } else if let Some(e) = error {
            Err(if e == "access_denied" {
                t("Login cancelado no navegador.", "Login cancelled in the browser.").to_string()
            } else {
                tr!("O Dropbox recusou o login: {}", "Dropbox refused the login: {}", q("error_description").unwrap_or(e))
            })
        } else {
            Ok(code.unwrap_or_default())
        };
        let (title, text) = match &result {
            Ok(_) => (t("Conectado", "Connected"), t("Pronto! Pode fechar esta aba e voltar ao Open Explorer.", "Done! You can close this tab and go back to Open Explorer.")),
            Err(_) => (t("Login não concluído", "Login not finished"), t("O login não foi concluído. Volte ao Open Explorer para ver o motivo.", "The login wasn't finished. Go back to Open Explorer to see why.")),
        };
        let html = format!(
            "<!doctype html><meta charset=utf-8><title>Open Explorer</title>\
             <body style=\"font-family:system-ui;display:grid;place-items:center;height:90vh\">\
             <div><h2>{title}</h2><p>{text}</p></div>"
        );
        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}", html.len());
        Ok(Some(result))
    }
}

/// Reads the token endpoint's answer.
fn token_answer(mut resp: ureq::http::Response<ureq::Body>) -> std::result::Result<Tokens, Fail> {
    let status = resp.status().as_u16();
    let body = resp.body_mut().read_to_string().map_err(|e| Fail::Other(crate::api::net_text(&e)))?;
    if !(200..300).contains(&status) {
        let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        let error = v.get("error").and_then(|e| e.as_str()).unwrap_or("");
        let said = v.get("error_description").and_then(|e| e.as_str()).unwrap_or(error);
        if error == "invalid_grant" {
            return Err(Fail::Auth(t("O acesso a esta conta do Dropbox expirou ou foi revogado. Remova a conta e conecte de novo.", "Access to this Dropbox account expired or was revoked. Remove the account and connect it again.").into()));
        }
        return Err(Fail::Other(tr!("O Dropbox recusou o login (HTTP {status}): {said}", "Dropbox refused the login (HTTP {status}): {said}")));
    }
    serde_json::from_str(&body).map_err(|e| Fail::Other(tr!("Resposta inesperada do Dropbox: {e}", "Unexpected answer from Dropbox: {e}")))
}

/// A new access token from a refresh token, at `api`.
pub fn refresh(agent: &ureq::Agent, api: &str, app_key: &str, refresh_token: &str) -> std::result::Result<Tokens, Fail> {
    let resp = agent
        .post(&format!("{api}/oauth2/token"))
        .send_form([("grant_type", "refresh_token"), ("refresh_token", refresh_token), ("client_id", app_key)])
        .map_err(|e| Fail::Other(crate::api::net_text(&e)))?;
    token_answer(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// A free port for one test's listener.
    fn cfg() -> AppConfig {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        AppConfig { app_key: "chave123".into(), port }
    }

    fn browse(port: u16, target: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn the_authorize_url_asks_offline_access_with_pkce_and_the_fixed_redirect() {
        let c = cfg();
        let l = begin(&c).unwrap();
        let url = url::Url::parse(&l.url).unwrap();
        let q = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned()).unwrap();
        assert_eq!(url.host_str(), Some("www.dropbox.com"));
        assert_eq!(q("client_id"), "chave123");
        assert_eq!(q("redirect_uri"), format!("http://localhost:{}", c.port));
        assert_eq!(q("token_access_type"), "offline");
        assert_eq!(q("code_challenge_method"), "S256");
        assert_eq!(q("code_challenge"), URL_SAFE_NO_PAD.encode(Sha256::digest(l.verifier.as_bytes())));
        assert!((43..=128).contains(&l.verifier.len()));
        assert!(q("scope").contains("files.content.write") && q("scope").contains("sharing.write"));
        // A second login on the same port is refused in words, not with an OS error.
        let busy = begin(&c).err().unwrap();
        assert!(busy.contains("já está em uso"), "{busy}");
    }

    #[test]
    fn the_redirect_hands_back_its_code_and_a_forged_state_is_refused() {
        let l = begin(&cfg()).unwrap();
        let (p, state) = (l.listeners[0].local_addr().unwrap().port(), l.state.clone());
        let browser = std::thread::spawn(move || (browse(p, "/favicon.ico"), browse(p, &format!("/?code=abc%2Fdef&state={state}"))));
        assert_eq!(l.wait_for_code(&|| false).unwrap(), "abc/def");
        let (fav, page) = browser.join().unwrap();
        assert!(fav.starts_with("HTTP/1.1 404") && page.contains("Pode fechar esta aba"));

        let l = begin(&cfg()).unwrap();
        let p = l.listeners[0].local_addr().unwrap().port();
        let browser = std::thread::spawn(move || browse(p, "/?code=abc&state=forjado"));
        assert!(l.wait_for_code(&|| false).unwrap_err().contains("estado diferente"));
        browser.join().unwrap();
    }
}
