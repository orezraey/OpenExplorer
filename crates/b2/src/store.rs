//! The B2 keys connected: their key IDs and the names they show by in `b2-accounts.json` (a file
//! of their own, so an older Open Explorer reading `servers.json` never meets an entry it doesn't
//! know), their application keys in the OS credential store, never on disk in the clear.

use std::path::PathBuf;

use openexplorer_i18n::{t, tr};
use serde::{Deserialize, Serialize};

use crate::Result;

const SERVICE: &str = "OpenExplorer B2";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredAccount {
    pub key_id: String,
    /// What the tree shows: "B2 · fotos" for a key of one bucket, else the key ID's start.
    pub name: String,
}

/// Where the list is kept: the app's config folder, a temporary one in tests.
#[derive(Clone, Debug)]
pub struct Store {
    pub file: PathBuf,
}

impl Store {
    pub fn at(file: PathBuf) -> Store {
        Store { file }
    }

    pub fn accounts(&self) -> Vec<StoredAccount> {
        std::fs::read_to_string(&self.file).ok().and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok()).unwrap_or_default()
    }

    fn write(&self, list: &[StoredAccount]) -> Result<()> {
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| tr!("Não consegui criar {}: {e}", "Couldn't create {}: {e}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(list).map_err(|e| e.to_string())?;
        std::fs::write(&self.file, text).map_err(|e| tr!("Não consegui salvar a lista de contas do B2: {e}", "Couldn't save the list of B2 accounts: {e}"))
    }

    /// Adds key `key_id` (once), or gives it its new name.
    pub fn add(&self, key_id: &str, name: &str) -> Result<()> {
        let mut list = self.accounts();
        match list.iter_mut().find(|a| a.key_id == key_id) {
            Some(a) => a.name = name.to_string(),
            None => list.push(StoredAccount { key_id: key_id.to_string(), name: name.to_string() }),
        }
        self.write(&list)
    }

    pub fn remove(&self, key_id: &str) -> Result<()> {
        let mut list = self.accounts();
        list.retain(|a| a.key_id != key_id);
        self.write(&list)
    }
}

fn entry(key_id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, key_id).map_err(|e| tr!("Cofre de credenciais indisponível: {e}", "Credential store unavailable: {e}"))
}

/// The application key kept for `key_id`.
pub fn secret(key_id: &str) -> Result<String> {
    entry(key_id)?.get_password().map_err(|_| {
        t("A chave de aplicação desta conta B2 não está no cofre do sistema. Remova a conta e conecte de novo.", "This B2 account's application key isn't in the system's credential store. Remove the account and connect it again.").to_string()
    })
}

pub fn set_secret(key_id: &str, key: &str) -> Result<()> {
    entry(key_id)?.set_password(key).map_err(|e| tr!("Não consegui guardar a chave no cofre do sistema: {e}", "Couldn't keep the key in the system's credential store: {e}"))
}

pub fn forget_secret(key_id: &str) {
    if let Ok(e) = entry(key_id) {
        let _ = e.delete_credential();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_keeps_each_key_once() {
        let dir = std::env::temp_dir().join(format!("oe-b2-store-{}", std::process::id()));
        let store = Store::at(dir.join("b2-accounts.json"));
        assert!(store.accounts().is_empty());
        store.add("0012ab", "B2 · fotos").unwrap();
        store.add("0012ab", "B2 · fotos2").unwrap();
        store.add("0034cd", "B2 · 0034cd").unwrap();
        assert_eq!(store.accounts().into_iter().map(|a| a.name).collect::<Vec<_>>(), ["B2 · fotos2", "B2 · 0034cd"]);
        store.remove("0012ab").unwrap();
        assert_eq!(store.accounts().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
