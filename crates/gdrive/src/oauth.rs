//! OAuth 2.0 for installed apps: the browser signs in and redirects to a one-shot HTTP listener
//! on 127.0.0.1, and the code it carries is exchanged with a PKCE verifier.
//! <https://developers.google.com/identity/protocols/oauth2/native-app>

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{ClientConfig, Result, agent, json};

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub(crate) const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// Every file in the Drive: browsing, renaming, uploading and downloading anything.
pub const SCOPE: &str = "https://www.googleapis.com/auth/drive";
/// How long the listener waits for the browser.
const WAIT: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    /// Sent on the first consent (we ask with `prompt=consent`), not on refreshes.
    pub refresh_token: Option<String>,
    pub expires_in: u64,
}

/// A login waiting for the browser: open [`PendingLogin::url`], then call
/// [`PendingLogin::finish`] on a background thread.
pub struct PendingLogin {
    listener: TcpListener,
    redirect: String,
    verifier: String,
    state: String,
    pub url: String,
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| format!("Sem gerador aleatório do sistema: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Opens the loopback listener and builds the consent URL.
pub fn begin(cfg: &ClientConfig) -> Result<PendingLogin> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("Não consegui abrir a porta local do login: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect = format!("http://127.0.0.1:{port}");
    let verifier = random_token()?;
    let state = random_token()?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let url = url::Url::parse_with_params(
        AUTH_URL,
        &[
            ("client_id", cfg.client_id.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("response_type", "code"),
            ("scope", SCOPE),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", state.as_str()),
            // A refresh token, so the account stays connected across runs.
            ("access_type", "offline"),
            ("prompt", "consent select_account"),
        ],
    )
    .map_err(|e| e.to_string())?
    .to_string();
    Ok(PendingLogin { listener, redirect, verifier, state, url })
}

impl PendingLogin {
    /// Waits for the browser's redirect (up to five minutes) and exchanges its code for tokens.
    pub fn finish(self, cfg: &ClientConfig) -> Result<Tokens> {
        let code = self.wait_for_code()?;
        let resp = agent()
            .post(TOKEN_URL)
            .send_form([
                ("code", code.as_str()),
                ("client_id", cfg.client_id.as_str()),
                ("client_secret", cfg.client_secret.as_str()),
                ("redirect_uri", self.redirect.as_str()),
                ("grant_type", "authorization_code"),
                ("code_verifier", self.verifier.as_str()),
            ])
            .map_err(|e| format!("Falha ao falar com o Google: {e}"))?;
        json(resp, "Troca do código de login")
    }

    fn wait_for_code(&self) -> Result<String> {
        self.listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + WAIT;
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    // The browser may also ask for /favicon.ico; only the redirect has a query.
                    if let Some(answer) = self.answer(stream)? {
                        return answer;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() > deadline {
                        return Err("O login expirou: o navegador não voltou em 5 minutos.".into());
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(format!("Erro na porta local do login: {e}")),
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
        // "GET /?state=..&code=.. HTTP/1.1"
        let target = line.split_whitespace().nth(1).unwrap_or("");
        let Ok(url) = url::Url::parse(&format!("http://127.0.0.1{target}")) else { return Ok(None) };
        let q = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
        let (code, error, state) = (q("code"), q("error"), q("state"));
        if code.is_none() && error.is_none() {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            return Ok(None);
        }
        let result = if state.as_deref() != Some(self.state.as_str()) {
            Err("O login voltou com um estado diferente do pedido; tente de novo.".to_string())
        } else if let Some(e) = error {
            Err(if e == "access_denied" { "Login cancelado no navegador.".to_string() } else { format!("O Google recusou o login: {e}") })
        } else {
            Ok(code.unwrap_or_default())
        };
        let (title, text) = match &result {
            Ok(_) => ("Conectado", "Pronto! Pode fechar esta aba e voltar ao Open Explorer."),
            Err(_) => ("Login não concluído", "O login não foi concluído. Volte ao Open Explorer para ver o motivo."),
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

/// A new access token from a refresh token.
pub fn refresh(agent: &ureq::Agent, cfg: &ClientConfig, refresh_token: &str) -> Result<Tokens> {
    let resp = agent
        .post(TOKEN_URL)
        .send_form([
            ("client_id", cfg.client_id.as_str()),
            ("client_secret", cfg.client_secret.as_str()),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .map_err(|e| format!("Falha ao falar com o Google: {e}"))?;
    json(resp, "Renovação do acesso").map_err(|e| {
        if e.contains("invalid_grant") || e.contains("expired or revoked") {
            "O acesso a esta conta expirou ou foi revogado. Conecte a conta de novo.".into()
        } else {
            e
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn cfg() -> ClientConfig {
        ClientConfig { client_id: "id.apps.googleusercontent.com".into(), client_secret: "s".into() }
    }

    /// Plays the browser: one GET to the listener, returning the page it got back.
    fn browse(port: u16, target: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    fn port(l: &PendingLogin) -> u16 {
        l.listener.local_addr().unwrap().port()
    }

    #[test]
    fn the_consent_url_asks_for_drive_offline_with_pkce_and_our_redirect() {
        let l = begin(&cfg()).unwrap();
        let url = url::Url::parse(&l.url).unwrap();
        let q = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned()).unwrap();
        assert_eq!(q("redirect_uri"), format!("http://127.0.0.1:{}", port(&l)));
        assert_eq!(q("scope"), SCOPE);
        assert_eq!(q("access_type"), "offline");
        assert_eq!(q("code_challenge_method"), "S256");
        assert_eq!(q("code_challenge"), URL_SAFE_NO_PAD.encode(Sha256::digest(l.verifier.as_bytes())));
        assert_eq!(q("state"), l.state);
    }

    #[test]
    fn the_redirect_hands_back_its_code_after_ignoring_a_favicon_request() {
        let l = begin(&cfg()).unwrap();
        let (p, state) = (port(&l), l.state.clone());
        let browser = std::thread::spawn(move || {
            let fav = browse(p, "/favicon.ico");
            let page = browse(p, &format!("/?state={state}&code=4%2F0Abc&scope=x"));
            (fav, page)
        });
        assert_eq!(l.wait_for_code().unwrap(), "4/0Abc");
        let (fav, page) = browser.join().unwrap();
        assert!(fav.starts_with("HTTP/1.1 404"));
        assert!(page.contains("Pode fechar esta aba"));
    }

    #[test]
    fn a_wrong_state_or_a_refusal_is_an_error() {
        let l = begin(&cfg()).unwrap();
        let p = port(&l);
        let browser = std::thread::spawn(move || browse(p, "/?state=forged&code=abc"));
        assert!(l.wait_for_code().unwrap_err().contains("estado diferente"));
        browser.join().unwrap();

        let l = begin(&cfg()).unwrap();
        let (p, state) = (port(&l), l.state.clone());
        let browser = std::thread::spawn(move || browse(p, &format!("/?state={state}&error=access_denied")));
        assert_eq!(l.wait_for_code().unwrap_err(), "Login cancelado no navegador.");
        browser.join().unwrap();
    }
}
