//! Logging in with e-mail and password (and a two-factor code), and resuming a session kept
//! from before. The password only ever derives the key that opens the master key here: what's
//! kept is the session (see [`Session::save`]), never the password.

use openexplorer_i18n::t;
use serde_json::{Value, json};

use crate::Result;
use crate::api::{Api, EFAILED, EMFAREQUIRED, ENOENT, Fail};
use crate::crypto::{RsaKey, b64, ecb_decrypt, ecb_encrypt, prepare_key_v1, prepare_key_v2, random, unb64, user_hash_v1};

/// A logged-in session's secrets.
#[derive(Clone)]
pub struct Session {
    /// The account's user handle.
    pub user: String,
    pub master: [u8; 16],
    /// The session key MEGA keeps for this session: the saved session holds the master key
    /// under it, so the saved text alone opens nothing once the session is closed.
    pub sek: [u8; 16],
    pub sid: String,
    pub rsa: Option<RsaKey>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").field("user", &self.user).finish_non_exhaustive()
    }
}

/// Why a login didn't work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The account has two-factor authentication: ask for the code and log in again.
    NeedsCode,
    /// The code given was wrong.
    WrongCode,
    Failed(String),
}

impl Refused {
    pub fn text(&self) -> String {
        match self {
            Refused::NeedsCode => crate::api::error_text(EMFAREQUIRED),
            Refused::WrongCode => t("O código de verificação está errado ou expirou. Digite o código atual do app autenticador.", "The verification code is wrong or expired. Enter the current code from the authenticator app.").into(),
            Refused::Failed(e) => e.clone(),
        }
    }
}

/// The e-mail as MEGA keys accounts by it.
pub fn normal_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// Logs in. `code` is the authenticator's six digits, for an account that asks for them.
pub fn login(api: &Api, email: &str, password: &str, code: Option<&str>) -> std::result::Result<Session, Refused> {
    let email = normal_email(email);
    let failed = |f: Fail| Refused::Failed(f.text());
    let pre = api.call(json!({"a": "us0", "user": email})).map_err(failed)?;
    let (pkey, uh) = match (pre.get("v").and_then(Value::as_i64), pre.get("s").and_then(Value::as_str)) {
        (Some(2), Some(salt)) => {
            let salt = unb64(salt).map_err(Refused::Failed)?;
            prepare_key_v2(password.as_bytes(), &salt)
        }
        (Some(1), _) => {
            let key = prepare_key_v1(password.as_bytes());
            (key, user_hash_v1(&email, &key))
        }
        _ => return Err(Refused::Failed(t("O MEGA respondeu ao login de um jeito desconhecido.", "MEGA answered the login in an unknown way.").into())),
    };
    let sek = random::<16>();
    let mut cmd = json!({"a": "us", "user": email, "uh": uh, "sek": b64(&sek)});
    let code = code.map(|c| c.chars().filter(char::is_ascii_digit).collect::<String>()).filter(|c| !c.is_empty());
    if let Some(c) = &code {
        cmd["mfa"] = c.clone().into();
    }
    let resp = match api.call(cmd) {
        Ok(r) => r,
        Err(Fail::Code(EMFAREQUIRED)) if code.is_none() => return Err(Refused::NeedsCode),
        Err(Fail::Code(EMFAREQUIRED | EFAILED)) if code.is_some() => return Err(Refused::WrongCode),
        Err(Fail::Code(ENOENT)) => return Err(Refused::Failed(t("E-mail ou senha incorretos.", "Wrong e-mail or password.").into())),
        Err(f) => return Err(failed(f)),
    };
    let session = open_login(&resp, &pkey, sek).map_err(Refused::Failed)?;
    api.set_sid(Some(session.sid.clone()));
    Ok(session)
}

/// The session in a login's answer: the master key under the password key, the RSA key under
/// the master key, and the session id under the RSA key.
pub fn open_login(resp: &Value, pkey: &[u8; 16], sek: [u8; 16]) -> Result<Session> {
    let field = |k: &str| resp.get(k).and_then(Value::as_str).ok_or_else(|| t("O MEGA não mandou a sessão completa.", "MEGA didn't send the whole session.").to_string());
    let mut master = unb64(field("k")?)?;
    if master.len() != 16 {
        return Err(t("A chave da conta veio num formato desconhecido.", "The account key came in an unknown format.").into());
    }
    ecb_decrypt(pkey, &mut master);
    let master: [u8; 16] = master.try_into().expect("16 bytes");
    let rsa = rsa_key(&master, field("privk")?)?;
    let Ok(csid) = field("csid") else {
        return Err(t("Esta conta do MEGA ainda não terminou de ser criada. Entre uma vez pelo site do MEGA e tente de novo.", "This MEGA account isn't fully set up yet. Sign in once on the MEGA website and try again.").into());
    };
    let plain = rsa.decrypt_padded(&unb64(csid)?)?;
    let sid = plain.get(..43).ok_or(t("O MEGA mandou uma sessão inválida (senha errada?).", "MEGA sent an invalid session (wrong password?)."))?;
    let sek = resp.get("sek").and_then(Value::as_str).and_then(|s| unb64(s).ok()).and_then(|s| s.try_into().ok()).unwrap_or(sek);
    Ok(Session { user: field("u")?.to_string(), master, sek, sid: b64(sid), rsa: Some(rsa) })
}

fn rsa_key(master: &[u8; 16], privk: &str) -> Result<RsaKey> {
    let mut raw = unb64(privk)?;
    ecb_decrypt(master, &mut raw);
    RsaKey::parse(&raw)
}

impl Session {
    /// The text kept to resume it: a version byte, the master key under the session key, the
    /// session id.
    pub fn save(&self) -> String {
        let mut key = self.master;
        ecb_encrypt(&self.sek, &mut key);
        let mut out = vec![1u8];
        out.extend(key);
        out.extend(unb64(&self.sid).unwrap_or_default());
        b64(&out)
    }
}

/// Resumes a session kept by [`Session::save`]: MEGA gives back its session key, which opens the
/// master key. Fails with MEGA's `ESID` once the session was closed (on the website, say).
pub fn resume(api: &Api, saved: &str) -> std::result::Result<Session, Fail> {
    let raw = unb64(saved).map_err(Fail::Other)?;
    let bad = || Fail::Other(t("A sessão guardada do MEGA está corrompida. Conecte a conta de novo.", "The saved MEGA session is corrupted. Connect the account again.").into());
    if raw.len() < 1 + 16 + 32 || raw[0] != 1 {
        return Err(bad());
    }
    let sid = b64(&raw[17..]);
    api.set_sid(Some(sid.clone()));
    let resp = api.call(json!({"a": "us", "sek": b64(&random::<16>())}))?;
    let sek: [u8; 16] = resp.get("sek").and_then(Value::as_str).and_then(|s| unb64(s).ok()).and_then(|s| s.try_into().ok()).ok_or_else(bad)?;
    let mut master: [u8; 16] = raw[1..17].try_into().expect("16 bytes");
    ecb_decrypt(&sek, &mut master);
    let privk = resp.get("privk").and_then(Value::as_str).ok_or_else(bad)?;
    let rsa = rsa_key(&master, privk).map_err(|_| bad())?;
    let user = resp.get("u").and_then(Value::as_str).ok_or_else(bad)?.to_string();
    Ok(Session { user, master, sek, sid, rsa: Some(rsa) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_bigint::BigUint;

    /// A small RSA key (two 64-bit primes) in MEGA's MPI form, with its public half.
    fn rsa_pair() -> (Vec<u8>, BigUint, BigUint) {
        let p = BigUint::from(18_446_744_073_709_551_557u64);
        let q = BigUint::from(18_446_744_073_709_551_533u64);
        let n = &p * &q;
        let phi = (&p - 1u32) * (&q - 1u32);
        let e = BigUint::from(65_537u32);
        let d = e.modinv(&phi).unwrap();
        let mpi = |v: &BigUint| {
            let bits = v.bits() as u16;
            let mut out = bits.to_be_bytes().to_vec();
            out.extend(v.to_bytes_be());
            out
        };
        let u = q.modinv(&p).unwrap();
        ([mpi(&p), mpi(&q), mpi(&d), mpi(&u)].concat(), n, e)
    }

    /// A login answer made the way MEGA makes one, opened again: the master key, the session id
    /// and the user come out, and the saved session holds the master key under the session key.
    #[test]
    fn a_login_answer_opens_and_saves() {
        let (privk, n, e) = rsa_pair();
        let pkey = random::<16>();
        let master = random::<16>();
        let mut k = master;
        ecb_encrypt(&pkey, &mut k);
        let mut pk = privk.clone();
        pk.resize(pk.len().div_ceil(16) * 16, 0);
        ecb_encrypt(&master, &mut pk);
        // The session id's 43 bytes don't fit a 128-bit key, so this test key gets a short one:
        // the RSA plaintext is padded to the key's length less 2 (14 bytes here).
        let sid_plain: Vec<u8> = (1..=14).collect();
        let c = BigUint::from_bytes_be(&sid_plain).modpow(&e, &n);
        let mut csid = (c.bits() as u16).to_be_bytes().to_vec();
        csid.extend(c.to_bytes_be());
        let sek = random::<16>();
        let resp = json!({"k": b64(&k), "privk": b64(&pk), "csid": b64(&csid), "u": "USERHANDLE1", "sek": b64(&sek)});
        // Too short for a real session id: refused, not cut short.
        assert!(open_login(&resp, &pkey, sek).is_err());
        let rsa = rsa_key(&master, &b64(&pk)).unwrap();
        assert_eq!(rsa.decrypt_padded(&csid).unwrap(), sid_plain);
        let s = Session { user: "USERHANDLE1".into(), master, sek, sid: b64(&[5u8; 43]), rsa: Some(rsa) };
        let saved = unb64(&s.save()).unwrap();
        assert_eq!(saved[0], 1);
        let mut back: [u8; 16] = saved[1..17].try_into().unwrap();
        ecb_decrypt(&sek, &mut back);
        assert_eq!(back, master);
        assert_eq!(b64(&saved[17..]), s.sid);
        assert!(!format!("{s:?}").contains(&format!("{:?}", master)));
    }

    #[test]
    fn emails_are_trimmed_and_lowercased() {
        assert_eq!(normal_email("  Ana@Exemplo.COM "), "ana@exemplo.com");
    }

    #[test]
    fn a_saved_session_too_short_is_refused_offline() {
        let api = Api::new("http://127.0.0.1:9");
        assert!(matches!(resume(&api, &b64(&[1, 2, 3])), Err(Fail::Other(_))));
    }
}

/// Against MEGA itself, no account needed (`cargo test -p openexplorer-mega -- --ignored`):
/// a login is asked for a proof of work first, and once it's solved MEGA says the e-mail and
/// password don't match.
#[cfg(test)]
mod online {
    use super::*;

    #[test]
    #[ignore = "talks to MEGA"]
    fn a_login_solves_the_proof_of_work_and_is_refused() {
        crate::api::set_log(|l| eprintln!("{l}"));
        let api = Api::default();
        let r = login(&api, "nobody-openexplorer-check@example.invalid", "not the password", None);
        assert_eq!(r.unwrap_err(), Refused::Failed(t("E-mail ou senha incorretos.", "Wrong e-mail or password.").into()));
    }
}
