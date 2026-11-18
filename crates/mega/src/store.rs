//! The MEGA accounts connected: their e-mails in `mega-accounts.json` (a file of their own, so
//! an older Open Explorer reading `servers.json` never meets an entry it doesn't know), their
//! sessions in the OS credential store. The password is never kept.

use std::path::PathBuf;

use openexplorer_i18n::{t, tr};
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::session::normal_email;

const SERVICE: &str = "OpenExplorer MEGA";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredAccount {
    pub email: String,
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
        std::fs::write(&self.file, text).map_err(|e| tr!("Não consegui salvar a lista de contas do MEGA: {e}", "Couldn't save the list of MEGA accounts: {e}"))
    }

    /// Adds `email` to the list (once).
    pub fn add(&self, email: &str) -> Result<()> {
        let email = normal_email(email);
        let mut list = self.accounts();
        if !list.iter().any(|a| a.email == email) {
            list.push(StoredAccount { email });
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

/// The session kept for `email`.
pub fn session(email: &str) -> Result<String> {
    entry(email)?.get_password().map_err(|_| t("A sessão desta conta do MEGA não está no cofre do sistema. Remova a conta e conecte de novo.", "This MEGA account's session isn't in the system's credential store. Remove the account and connect it again.").to_string())
}

pub fn set_session(email: &str, saved: &str) -> Result<()> {
    entry(email)?.set_password(saved).map_err(|e| tr!("Não consegui guardar a sessão no cofre do sistema: {e}", "Couldn't keep the session in the system's credential store: {e}"))
}

pub fn forget_session(email: &str) {
    if let Ok(e) = entry(email) {
        let _ = e.delete_credential();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_keeps_each_email_once() {
        let dir = std::env::temp_dir().join(format!("oe-mega-store-{}", std::process::id()));
        let store = Store::at(dir.join("mega-accounts.json"));
        assert!(store.accounts().is_empty());
        store.add("Ana@Exemplo.com").unwrap();
        store.add("ana@exemplo.com ").unwrap();
        store.add("bia@exemplo.com").unwrap();
        assert_eq!(store.accounts().into_iter().map(|a| a.email).collect::<Vec<_>>(), ["ana@exemplo.com", "bia@exemplo.com"]);
        store.remove("ANA@exemplo.com").unwrap();
        assert_eq!(store.accounts().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
