//! MEGA's API: a POST of a JSON array of commands to `/cs`, answered by an array of results
//! where a negative number is that command's error. The whole answer can also be a single
//! number (`-3`: busy, ask again), and since 2024 a login can be refused with HTTP 402 and an
//! `X-Hashcash` challenge to solve first (see [`crate::hashcash`]).

use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use openexplorer_i18n::{t, tr};
use serde_json::Value;

use crate::Result;
use crate::hashcash::Challenge;

/// Where the API answers.
pub const API: &str = "https://g.api.mega.co.nz";

/// A command that failed: MEGA's error code, or what went wrong on the way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fail {
    Code(i64),
    Other(String),
}

impl Fail {
    pub fn text(&self) -> String {
        match self {
            Fail::Code(c) => error_text(*c),
            Fail::Other(e) => e.clone(),
        }
    }
}

impl From<Fail> for String {
    fn from(f: Fail) -> String {
        f.text()
    }
}

// MEGA's error codes (the SDK's `error` enum).
pub const EAGAIN: i64 = -3;
pub const ERATELIMIT: i64 = -4;
pub const EFAILED: i64 = -5;
pub const ETOOMANY: i64 = -6;
pub const ENOENT: i64 = -9;
pub const EACCESS: i64 = -11;
pub const EEXIST: i64 = -12;
pub const EKEY: i64 = -14;
pub const ESID: i64 = -15;
pub const EBLOCKED: i64 = -16;
pub const EOVERQUOTA: i64 = -17;
pub const ETEMPUNAVAIL: i64 = -18;
pub const EMFAREQUIRED: i64 = -26;

/// An error code in the user's words.
pub fn error_text(code: i64) -> String {
    match code {
        -1 => t("O MEGA teve um erro interno. Tente de novo.", "MEGA had an internal error. Try again.").into(),
        -2 => t("O MEGA recusou o pedido (argumentos inválidos).", "MEGA turned the request down (invalid arguments).").into(),
        EAGAIN | ETEMPUNAVAIL => t("O MEGA está ocupado agora. Tente de novo em instantes.", "MEGA is busy right now. Try again in a moment.").into(),
        ERATELIMIT | ETOOMANY => t("Pedidos demais ao MEGA. Espere um pouco e tente de novo.", "Too many requests to MEGA. Wait a little and try again.").into(),
        EFAILED => t("O MEGA não conseguiu fazer isso.", "MEGA couldn't do that.").into(),
        -8 => t("O link ou a sessão do MEGA expirou.", "The MEGA link or session expired.").into(),
        ENOENT => t("Não encontrado no MEGA (o item foi apagado, ou e-mail ou senha estão errados).", "Not found on MEGA (the item was deleted, or the e-mail or password is wrong).").into(),
        -10 => t("O MEGA recusou: isso criaria um ciclo (uma pasta dentro dela mesma).", "MEGA refused: that would make a loop (a folder inside itself).").into(),
        EACCESS => t("Sem permissão para isso no MEGA.", "No permission for that on MEGA.").into(),
        EEXIST => t("Isso já existe no MEGA.", "That already exists on MEGA.").into(),
        -13 => t("O pedido ao MEGA ficou incompleto.", "The request to MEGA was incomplete.").into(),
        EKEY => t("A chave de criptografia não confere (dados corrompidos ou chave errada).", "The encryption key doesn't match (corrupted data or a wrong key).").into(),
        ESID => t("A sessão do MEGA não vale mais. Conecte a conta de novo.", "The MEGA session is no longer valid. Connect the account again.").into(),
        EBLOCKED => t("A conta ou o arquivo foi bloqueado pelo MEGA.", "The account or the file was blocked by MEGA.").into(),
        EOVERQUOTA => t("Sem espaço ou cota de transferência no MEGA.", "Out of storage or transfer quota on MEGA.").into(),
        -19 => t("Conexões demais ao MEGA.", "Too many connections to MEGA.").into(),
        -24 => t("O envio passaria da cota de espaço do MEGA.", "The upload would go over the MEGA storage quota.").into(),
        EMFAREQUIRED => t("Esta conta usa verificação em duas etapas: digite o código do app autenticador.", "This account uses two-factor authentication: enter the code from the authenticator app.").into(),
        -29 => t("A conta do MEGA está bloqueada por excesso de uso (ODQ). Libere espaço no site do MEGA.", "The MEGA account is locked for being over its storage (ODQ). Free up space on the MEGA website.").into(),
        c => tr!("O MEGA respondeu com o erro {c}.", "MEGA answered with error {c}."),
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

/// The connection to the API of one session.
pub struct Api {
    agent: ureq::Agent,
    base: String,
    seq: AtomicU64,
    sid: RwLock<Option<String>>,
    /// Turned on to stop a hashcash search midway (the dialog was closed).
    pub stop: AtomicBool,
}

impl Default for Api {
    fn default() -> Api {
        Api::new(API)
    }
}

/// An agent that hands back error statuses instead of failing, so the 402 challenge and MEGA's
/// bodies can be read.
pub(crate) fn agent(read: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(read))
        .timeout_per_read(Some(read))
        .user_agent("OpenExplorer")
        .build()
        .into()
}

impl Api {
    pub fn new(base: &str) -> Api {
        let seq = u64::from_le_bytes(crate::crypto::random::<8>()) >> 34;
        Api { agent: agent(Duration::from_secs(180)), base: base.trim_end_matches('/').to_string(), seq: AtomicU64::new(seq), sid: RwLock::new(None), stop: AtomicBool::new(false) }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn set_sid(&self, sid: Option<String>) {
        *self.sid.write().unwrap_or_else(|e| e.into_inner()) = sid;
    }

    pub fn sid(&self) -> Option<String> {
        self.sid.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Sends `cmds` and gives each command's result, its error as a negative number in place.
    /// Sends again when MEGA is busy, and solves a login's hashcash challenge.
    pub fn batch(&self, cmds: &[Value]) -> std::result::Result<Vec<Value>, Fail> {
        self.batch_with(cmds, &[])
    }

    pub fn batch_with(&self, cmds: &[Value], query: &[(&str, &str)]) -> std::result::Result<Vec<Value>, Fail> {
        let what = cmds.iter().filter_map(|c| c.get("a").and_then(Value::as_str)).collect::<Vec<_>>().join(",");
        let body = serde_json::to_vec(cmds).expect("commands are JSON");
        let mut delay = Duration::from_millis(250);
        let mut hashcash: Option<String> = None;
        let mut solved = 0;
        // The same id for every try: MEGA ties a retry (and a solved challenge) to it.
        let id = self.seq.fetch_add(1, Ordering::Relaxed).to_string();
        for attempt in 0..8 {
            if attempt > 0 && hashcash.is_none() {
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(16));
            }
            let started = Instant::now();
            let sid = self.sid();
            let mut req = self.agent.post(format!("{}/cs", self.base)).query("id", &id).header("Content-Type", "application/json");
            if let Some(sid) = &sid {
                req = req.query("sid", sid);
            }
            for (k, v) in query {
                req = req.query(*k, *v);
            }
            if let Some(h) = hashcash.take() {
                req = req.header("X-Hashcash", &h);
            }
            let mut resp = match req.send(&body[..]) {
                Ok(r) => r,
                Err(e) => {
                    log(&format!("MEGA {what} falhou em {} ms: {e}", started.elapsed().as_millis()));
                    if attempt < 3 {
                        continue;
                    }
                    return Err(Fail::Other(tr!("Sem conexão com o MEGA: {e}", "No connection to MEGA: {e}")));
                }
            };
            let status = resp.status().as_u16();
            if status == 402
                && let Some(c) = resp.headers().get("X-Hashcash").and_then(|v| v.to_str().ok()).and_then(Challenge::parse)
                && solved < 3
            {
                solved += 1;
                let t0 = Instant::now();
                let Some(h) = c.solve(&self.stop) else {
                    return Err(Fail::Other(t("O MEGA pediu uma verificação de segurança que não foi concluída.", "MEGA asked for a security check that didn't finish.").into()));
                };
                log(&format!("MEGA {what}: hashcash (facilidade {}) resolvido em {} ms", c.easiness, t0.elapsed().as_millis()));
                hashcash = Some(h);
                continue;
            }
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            log(&format!("MEGA {what} {status} em {} ms", started.elapsed().as_millis()));
            if status == 500 || status == 503 {
                continue;
            }
            if status != 200 {
                return Err(Fail::Other(tr!("O MEGA respondeu HTTP {status}.", "MEGA answered HTTP {status}.")));
            }
            let value: Value = serde_json::from_str(&text).map_err(|e| Fail::Other(tr!("Resposta inesperada do MEGA: {e}", "Unexpected answer from MEGA: {e}")))?;
            match value {
                Value::Number(n) => {
                    let code = n.as_i64().unwrap_or(-1);
                    if matches!(code, EAGAIN | ERATELIMIT | ETOOMANY) {
                        continue;
                    }
                    if code == 0 {
                        return Ok(vec![Value::from(0); cmds.len()]);
                    }
                    return Err(Fail::Code(code));
                }
                Value::Array(results) => {
                    // A command turned away for being busy goes again, with the others (each
                    // carries its own id, so what went through isn't done twice).
                    if results.iter().any(|r| r.as_i64() == Some(EAGAIN)) {
                        continue;
                    }
                    return Ok(results);
                }
                _ => return Err(Fail::Other(t("Resposta inesperada do MEGA.", "Unexpected answer from MEGA.").into())),
            }
        }
        Err(Fail::Code(EAGAIN))
    }

    /// Sends one command; its error comes back as `Err`.
    pub fn call(&self, cmd: Value) -> std::result::Result<Value, Fail> {
        let mut out = self.batch(std::slice::from_ref(&cmd))?;
        let v = if out.is_empty() { Value::from(0) } else { out.swap_remove(0) };
        match v.as_i64() {
            Some(code) if code < 0 => Err(Fail::Code(code)),
            _ => Ok(v),
        }
    }

    /// [`Api::call`] with the error in words.
    pub fn run(&self, cmd: Value) -> Result<Value> {
        self.call(cmd).map_err(String::from)
    }

    /// A GET whose answer is JSON (the `sc` channel).
    pub fn get_json(&self, url: &str) -> std::result::Result<Value, Fail> {
        let started = Instant::now();
        let mut resp = self.agent.get(url).call().map_err(|e| Fail::Other(tr!("Sem conexão com o MEGA: {e}", "No connection to MEGA: {e}")))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        log(&format!("MEGA sc {status} em {} ms", started.elapsed().as_millis()));
        if status != 200 {
            return Err(Fail::Other(tr!("O MEGA respondeu HTTP {status}.", "MEGA answered HTTP {status}.")));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| Fail::Other(tr!("Resposta inesperada do MEGA: {e}", "Unexpected answer from MEGA: {e}")))?;
        match v.as_i64() {
            Some(code) if code < 0 => Err(Fail::Code(code)),
            _ => Ok(v),
        }
    }
}

/// A request id that makes a change safe to send twice: MEGA does it once.
pub fn request_id() -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    crate::crypto::random::<10>().iter().map(|b| CHARS[usize::from(*b) % CHARS.len()] as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_read_in_words() {
        assert!(error_text(ENOENT).contains("senha"));
        assert!(error_text(-1234).contains("-1234"));
        assert_eq!(String::from(Fail::Other("x".into())), "x");
    }

    #[test]
    fn request_ids_are_ten_letters() {
        let a = request_id();
        assert_eq!(a.len(), 10);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a, request_id());
    }
}
