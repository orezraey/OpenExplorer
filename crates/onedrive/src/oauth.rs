//! Signing in: OAuth 2.0 authorization code with PKCE for a public client (no secret), the
//! browser redirected to a one-shot listener on this machine. Microsoft matches
//! `http://localhost` registered for a desktop app whatever the port, so the redirect is
//! `http://localhost:<port>`, and the listener answers on 127.0.0.1 and on ::1, whichever the
//! browser picks for `localhost`.
//! <https://learn.microsoft.com/entra/identity-platform/v2-oauth2-auth-code-flow>

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use openexplorer_i18n::{t, tr};
use sha2::{Digest, Sha256};

use crate::ClientConfig;
use crate::graph::{Endpoints, Fail, Res, TokenAnswer, api_agent, token_call};

/// Every file the account can reach, kept across runs, and who the account is.
pub const SCOPES: &str = "Files.ReadWrite.All offline_access User.Read";
/// How long the listener waits for the browser.
const WAIT: Duration = Duration::from_secs(5 * 60);

/// A login waiting for the browser: open [`PendingLogin::url`], then call
/// [`PendingLogin::finish`] on a background thread.
pub struct PendingLogin {
    listeners: Vec<TcpListener>,
    redirect: String,
    verifier: String,
    state: String,
    pub url: String,
    pub cfg: ClientConfig,
    pub ends: Endpoints,
    /// What it asks for: [`SCOPES`] for an account, or another resource's (see [`grant`]).
    scopes: String,
    /// A grant for another resource: a refusal reads as [`crate::graph::resource_failure`].
    resource: bool,
}

pub(crate) fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("the system's random generator");
    bytes
}

/// Opens the loopback listener and builds the sign-in address.
pub fn begin(cfg: &ClientConfig, ends: &Endpoints) -> Res<PendingLogin> {
    open(cfg, ends, SCOPES, &[("prompt", "select_account")], false)
}

/// A sign-in that only grants the app another resource for an account already connected
/// (`scope` like `https://contoso-my.sharepoint.com/.default`), the account picked by its e-mail:
/// Microsoft asks the person to consent to what the registration lists for that resource.
pub fn grant(cfg: &ClientConfig, ends: &Endpoints, scope: &str, email: &str) -> Res<PendingLogin> {
    let scopes = format!("{scope} offline_access");
    open(cfg, ends, &scopes, &[("login_hint", email)], true)
}

fn open(cfg: &ClientConfig, ends: &Endpoints, scopes: &str, extra: &[(&str, &str)], resource: bool) -> Res<PendingLogin> {
    let v4 = TcpListener::bind("127.0.0.1:0").map_err(|e| Fail::new(tr!("Não consegui abrir a porta local do login: {e}", "Couldn't open the login's local port: {e}")))?;
    let port = v4.local_addr().map_err(|e| Fail::new(e.to_string()))?.port();
    let mut listeners = vec![v4];
    // `localhost` may be ::1 to the browser; the same port there too, when free.
    if let Ok(v6) = TcpListener::bind(("::1", port)) {
        listeners.push(v6);
    }
    let redirect = format!("http://localhost:{port}");
    let verifier = URL_SAFE_NO_PAD.encode(random::<32>());
    let state = URL_SAFE_NO_PAD.encode(random::<16>());
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut params = vec![
        ("client_id", cfg.client_id.as_str()),
        ("response_type", "code"),
        ("redirect_uri", redirect.as_str()),
        ("response_mode", "query"),
        ("scope", scopes),
        ("state", state.as_str()),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
    ];
    params.extend_from_slice(extra);
    let url = url::Url::parse_with_params(&format!("{}/{}/oauth2/v2.0/authorize", ends.login, cfg.tenant), &params).map_err(|e| Fail::new(e.to_string()))?.to_string();
    Ok(PendingLogin { listeners, redirect, verifier, state, url, cfg: cfg.clone(), ends: ends.clone(), scopes: scopes.to_string(), resource })
}

impl PendingLogin {
    /// Waits for the browser's redirect (up to five minutes) and exchanges its code for tokens.
    pub fn finish(&self) -> Res<TokenAnswer> {
        let code = self.wait_for_code()?;
        let form = [
            ("client_id", self.cfg.client_id.as_str()),
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", self.redirect.as_str()),
            ("code_verifier", self.verifier.as_str()),
            ("scope", self.scopes.as_str()),
        ];
        let read = if self.resource { crate::graph::resource_failure } else { crate::graph::login_failure };
        token_call(&api_agent(), &self.ends, &self.cfg, &form, read)
    }

    fn wait_for_code(&self) -> Res<String> {
        for l in &self.listeners {
            l.set_nonblocking(true).map_err(|e| Fail::new(e.to_string()))?;
        }
        let deadline = Instant::now() + WAIT;
        loop {
            for l in &self.listeners {
                match l.accept() {
                    Ok((stream, _)) => {
                        // The browser may also ask for /favicon.ico; only the redirect has a query.
                        if let Some(answer) = self.answer(stream) {
                            return answer;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(Fail::new(tr!("Erro na porta local do login: {e}", "Error on the login's local port: {e}"))),
                }
            }
            if Instant::now() > deadline {
                return Err(Fail::new(t("O login expirou: o navegador não voltou em 5 minutos.", "The login expired: the browser didn't come back within 5 minutes.")));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Reads one request. `None` when it isn't the redirect.
    fn answer(&self, mut stream: TcpStream) -> Option<Res<String>> {
        stream.set_nonblocking(false).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).ok()?;
        let target = line.split_whitespace().nth(1).unwrap_or("");
        let url = url::Url::parse(&format!("http://localhost{target}")).ok()?;
        let q = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
        let (code, error, state) = (q("code"), q("error"), q("state"));
        if code.is_none() && error.is_none() {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            return None;
        }
        let result = if state.as_deref() != Some(self.state.as_str()) {
            Err(Fail::new(t("O login voltou com um estado diferente do pedido; tente de novo.", "The login came back with a different state than asked; try again.")))
        } else if let Some(e) = error {
            let why = q("error_description").unwrap_or_default();
            // A plain refusal on the consent page carries no AADSTS code; an organization's
            // wall (admin consent, conditional access) does.
            Err(if e == "access_denied" && !why.contains("AADSTS") {
                Fail::new(t("Login cancelado no navegador.", "Login cancelled in the browser."))
            } else {
                // A grant for SharePoint refused reads as what to add, not as a failed sign-in.
                let read = if self.resource { crate::graph::resource_failure } else { crate::graph::login_failure };
                read(400, &serde_json::json!({ "error": e, "error_description": why }).to_string())
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
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn cfg() -> ClientConfig {
        ClientConfig { client_id: "0b1c2d3e-4f50-6172-8394-a5b6c7d8e9f0".into(), tenant: "common".into() }
    }

    fn browse(port: u16, target: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    fn port(l: &PendingLogin) -> u16 {
        l.listeners[0].local_addr().unwrap().port()
    }

    #[test]
    fn the_sign_in_address_asks_with_pkce_a_localhost_redirect_and_no_secret() {
        let l = begin(&cfg(), &Endpoints::default()).unwrap();
        let url = url::Url::parse(&l.url).unwrap();
        assert!(l.url.starts_with("https://login.microsoftonline.com/common/oauth2/v2.0/authorize?"));
        let q = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
        assert_eq!(q("redirect_uri").unwrap(), format!("http://localhost:{}", port(&l)));
        assert_eq!(q("scope").unwrap(), SCOPES);
        assert!(q("scope").unwrap().contains("offline_access"));
        assert_eq!(q("code_challenge_method").unwrap(), "S256");
        assert_eq!(q("code_challenge").unwrap(), URL_SAFE_NO_PAD.encode(Sha256::digest(l.verifier.as_bytes())));
        assert!(q("client_secret").is_none());
    }

    #[test]
    fn the_redirect_hands_back_its_code_after_ignoring_a_favicon_request() {
        let l = begin(&cfg(), &Endpoints::default()).unwrap();
        let (p, state) = (port(&l), l.state.clone());
        let browser = std::thread::spawn(move || (browse(p, "/favicon.ico"), browse(p, &format!("/?code=M.C5_abc%2Fx&state={state}"))));
        assert_eq!(l.wait_for_code().unwrap(), "M.C5_abc/x");
        let (fav, page) = browser.join().unwrap();
        assert!(fav.starts_with("HTTP/1.1 404") && page.contains("Pode fechar esta aba"));
    }

    #[test]
    fn a_forged_state_or_an_admin_wall_is_an_error() {
        let l = begin(&cfg(), &Endpoints::default()).unwrap();
        let p = port(&l);
        let b = std::thread::spawn(move || browse(p, "/?state=forjado&code=abc"));
        assert!(l.wait_for_code().unwrap_err().text.contains("estado diferente"));
        b.join().unwrap();

        let l = begin(&cfg(), &Endpoints::default()).unwrap();
        let (p, state) = (port(&l), l.state.clone());
        let b = std::thread::spawn(move || browse(p, &format!("/?state={state}&error=access_denied&error_description=AADSTS90094%3A+admin")));
        assert!(l.wait_for_code().unwrap_err().text.contains("administrador"));
        b.join().unwrap();
    }
}
