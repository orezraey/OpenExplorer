//! What an account keeps in the OS credential store: its refresh token and the key its cache
//! file is sealed with. Never on disk in the clear.
//!
//! Windows keeps at most 2,560 bytes per credential, and a work account's refresh token can
//! come close to that alone; so the secret is written as raw bytes (not UTF-16) and split over
//! as many entries as it takes: `<account>`, `<account>#1`, `<account>#2`, … The first part
//! starts with the number of parts.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use openexplorer_i18n::{t, tr};
use serde::{Deserialize, Serialize};

use crate::Result;

const SERVICE: &str = "OpenExplorer OneDrive";
/// Bytes per entry, under Windows' 2,560 with room for the header.
const PART: usize = 2000;

#[derive(Clone, Serialize, Deserialize)]
pub struct Secret {
    /// The refresh token, replaced at every renewal.
    #[serde(rename = "r")]
    pub refresh: String,
    /// The cache file's key (32 random bytes, base64), made once per account.
    #[serde(rename = "k")]
    pub cache_key: String,
    /// The app registration it was signed in with: a refresh token only works with it.
    #[serde(rename = "c")]
    pub client_id: String,
    #[serde(rename = "t")]
    pub tenant: String,
    /// Who the account is, so it opens with no request of its own.
    #[serde(rename = "e", default)]
    pub email: String,
    #[serde(rename = "n", default)]
    pub name: String,
    #[serde(rename = "b", default)]
    pub business: bool,
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl Secret {
    /// A new account's: its cache key made now.
    pub fn new(refresh: String, client: &crate::ClientConfig, email: &str, name: &str, business: bool) -> Secret {
        Secret {
            refresh,
            cache_key: STANDARD.encode(crate::oauth::random::<32>()),
            client_id: client.client_id.clone(),
            tenant: client.tenant.clone(),
            email: email.into(),
            name: name.into(),
            business,
        }
    }

    pub fn client(&self) -> crate::ClientConfig {
        crate::ClientConfig { client_id: self.client_id.clone(), tenant: self.tenant.clone() }
    }

    pub fn key(&self) -> Option<[u8; 32]> {
        STANDARD.decode(&self.cache_key).ok()?.try_into().ok()
    }
}

/// The parts `data` is written as: the first starts with `<count>\n`.
pub fn split(data: &[u8]) -> Vec<Vec<u8>> {
    let chunks: Vec<&[u8]> = if data.is_empty() { vec![&[][..]] } else { data.chunks(PART).collect() };
    chunks
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut part = if i == 0 { format!("{}\n", chunks.len()).into_bytes() } else { Vec::new() };
            part.extend_from_slice(c);
            part
        })
        .collect()
}

/// The count the first part announces, and its data.
pub fn head(first: &[u8]) -> Option<(usize, &[u8])> {
    let nl = first.iter().position(|b| *b == b'\n')?;
    let n = std::str::from_utf8(&first[..nl]).ok()?.parse().ok()?;
    Some((n, &first[nl + 1..]))
}

fn entry(name: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, name).map_err(|e| tr!("Cofre de credenciais indisponível: {e}", "Credential store unavailable: {e}"))
}

fn part_name(account: &str, i: usize) -> String {
    if i == 0 { account.to_string() } else { format!("{account}#{i}") }
}

/// Keeps `secret` for `account`, replacing what was there.
pub fn put(account: &str, secret: &Secret) -> Result<()> {
    let data = serde_json::to_vec(secret).map_err(|e| e.to_string())?;
    let parts = split(&data);
    let before = get_count(account).unwrap_or(0);
    // The later parts first: a crash midway leaves the old first part announcing parts that
    // may be new, which reads as damaged rather than as another token.
    for (i, p) in parts.iter().enumerate().rev() {
        entry(&part_name(account, i))?
            .set_secret(p)
            .map_err(|e| tr!("Não consegui guardar o acesso no cofre do sistema: {e}", "Couldn't keep the access in the system's credential store: {e}"))?;
    }
    for i in parts.len()..before {
        if let Ok(e) = entry(&part_name(account, i)) {
            let _ = e.delete_credential();
        }
    }
    Ok(())
}

fn get_count(account: &str) -> Option<usize> {
    let first = entry(account).ok()?.get_secret().ok()?;
    head(&first).map(|(n, _)| n)
}

/// The secret kept for `account`.
pub fn get(account: &str) -> Result<Secret> {
    let missing = || t("O acesso desta conta do OneDrive não está no cofre do sistema. Remova a conta e conecte de novo.", "This OneDrive account's access isn't in the system's credential store. Remove the account and connect it again.").to_string();
    let first = entry(account)?.get_secret().map_err(|_| missing())?;
    let (n, data) = head(&first).ok_or_else(missing)?;
    let mut all = data.to_vec();
    for i in 1..n {
        all.extend(entry(&part_name(account, i))?.get_secret().map_err(|_| missing())?);
    }
    serde_json::from_slice(&all).map_err(|_| missing())
}

/// Forgets `account`'s secret, every part of it.
pub fn forget(account: &str) {
    let n = get_count(account).unwrap_or(1).max(1);
    for i in 0..n {
        if let Ok(e) = entry(&part_name(account, i)) {
            let _ = e.delete_credential();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_secret_splits_into_parts_that_join_back() {
        let token = "M.C5_BAY.".to_string() + &"x".repeat(4321);
        let client = crate::ClientConfig { client_id: "0b1c2d3e-4f50-6172-8394-a5b6c7d8e9f0".into(), tenant: "common".into() };
        let data = serde_json::to_vec(&Secret::new(token.clone(), &client, "ana@exemplo.com", "Ana", true)).unwrap();
        let parts = split(&data);
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.len() <= 2560));
        let (n, first) = head(&parts[0]).unwrap();
        assert_eq!(n, 3);
        let mut all = first.to_vec();
        for p in &parts[1..] {
            all.extend_from_slice(p);
        }
        let back: Secret = serde_json::from_slice(&all).unwrap();
        assert_eq!(back.refresh, token);
        assert_eq!(back.key().unwrap().len(), 32);
        assert!(!format!("{back:?}").contains("M.C5"));
        assert_eq!(split(b"").len(), 1);
    }
}
