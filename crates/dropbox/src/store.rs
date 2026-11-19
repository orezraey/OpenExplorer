//! The Dropbox accounts connected: their e-mail, name and account id in `dropbox-accounts.json`
//! (a file of their own, so an older Open Explorer reading `servers.json` never meets an entry
//! it doesn't know), their refresh tokens in the OS credential store, never on disk.

use std::path::PathBuf;

use openexplorer_i18n::{t, tr};
use serde::{Deserialize, Serialize};

use crate::Result;

const SERVICE: &str = "OpenExplorer Dropbox";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredAccount {
    pub email: String,
    #[serde(default)]
    pub name: String,
    /// Dropbox's id of the account (`dbid:…`), which an e-mail change doesn't change.
    #[serde(default)]
    pub account_id: String,
}

/// An e-mail as accounts are kept by: trimmed, lowercase.
pub fn normal_email(email: &str) -> String {
    email.trim().to_lowercase()
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
        std::fs::write(&self.file, text).map_err(|e| tr!("Não consegui salvar a lista de contas do Dropbox: {e}", "Couldn't save the list of Dropbox accounts: {e}"))
    }

    /// Adds `account`, or updates the one with its e-mail.
    pub fn add(&self, mut account: StoredAccount) -> Result<()> {
        account.email = normal_email(&account.email);
        let mut list = self.accounts();
        match list.iter_mut().find(|a| a.email == account.email) {
            Some(a) => *a = account,
            None => list.push(account),
        }
        self.write(&list)
    }

    pub fn remove(&self, email: &str) -> Result<()> {
        let email = normal_email(email);
        let mut list = self.accounts();
        list.retain(|a| a.email != email);
        self.write(&list)
    }
}

fn entry(email: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, &normal_email(email)).map_err(|e| tr!("Cofre de credenciais indisponível: {e}", "Credential store unavailable: {e}"))
}

/// The refresh token kept for `email`.
pub fn refresh_token(email: &str) -> Result<String> {
    entry(email)?.get_password().map_err(|_| t("O acesso desta conta do Dropbox não está no cofre do sistema. Remova a conta e conecte de novo.", "This Dropbox account's access isn't in the system's credential store. Remove the account and connect it again.").to_string())
}

pub fn set_refresh_token(email: &str, token: &str) -> Result<()> {
    entry(email)?.set_password(token).map_err(|e| tr!("Não consegui guardar o acesso no cofre do sistema: {e}", "Couldn't keep the access in the system's credential store: {e}"))
}

pub fn forget_refresh_token(email: &str) {
    if let Ok(e) = entry(email) {
        let _ = e.delete_credential();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_keeps_each_email_once() {
        let dir = std::env::temp_dir().join(format!("oe-dropbox-store-{}", std::process::id()));
        let store = Store::at(dir.join("dropbox-accounts.json"));
        assert!(store.accounts().is_empty());
        let a = |email: &str, name: &str| StoredAccount { email: email.into(), name: name.into(), account_id: "dbid:1".into() };
        store.add(a("Ana@Exemplo.com", "Ana")).unwrap();
        store.add(a("ana@exemplo.com ", "Ana Souza")).unwrap();
        store.add(a("bia@exemplo.com", "Bia")).unwrap();
        let list = store.accounts();
        assert_eq!(list.iter().map(|a| a.email.as_str()).collect::<Vec<_>>(), ["ana@exemplo.com", "bia@exemplo.com"]);
        assert_eq!(list[0].name, "Ana Souza");
        store.remove("ANA@exemplo.com").unwrap();
        assert_eq!(store.accounts().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
