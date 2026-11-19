//! The OneDrive accounts connected: who they are in `onedrive-accounts.json` (a file of their
//! own, so an older Open Explorer reading `servers.json` never meets an entry it doesn't know),
//! their refresh tokens in the OS credential store (see [`crate::vault`]).
//!
//! An account is known by its drive's id, not its e-mail: one address can be a personal
//! Microsoft account and a work account at once, and a work account may have no `mail`.

use std::path::PathBuf;

use openexplorer_i18n::tr;
use serde::{Deserialize, Serialize};

use crate::Result;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredAccount {
    /// The drive's id: the key of everything kept for the account.
    pub id: String,
    /// The e-mail (or user principal name) shown for it.
    pub email: String,
    pub name: String,
    /// A work or school account (OneDrive for Business), not a personal one.
    #[serde(default)]
    pub business: bool,
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
        std::fs::write(&self.file, text).map_err(|e| tr!("Não consegui salvar a lista de contas do OneDrive: {e}", "Couldn't save the list of OneDrive accounts: {e}"))
    }

    /// Adds `account`, or updates the one with its id.
    pub fn add(&self, account: StoredAccount) -> Result<()> {
        let mut list = self.accounts();
        match list.iter_mut().find(|a| a.id == account.id) {
            Some(a) => *a = account,
            None => list.push(account),
        }
        self.write(&list)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        let mut list = self.accounts();
        list.retain(|a| a.id != id);
        self.write(&list)
    }

    pub fn find(&self, id: &str) -> Option<StoredAccount> {
        self.accounts().into_iter().find(|a| a.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_email_can_be_two_accounts() {
        let dir = std::env::temp_dir().join(format!("oe-onedrive-store-{}", std::process::id()));
        let store = Store::at(dir.join("onedrive-accounts.json"));
        assert!(store.accounts().is_empty());
        let a = |id: &str, business| StoredAccount { id: id.into(), email: "ana@exemplo.com".into(), name: "Ana".into(), business };
        store.add(a("d1", false)).unwrap();
        store.add(a("b!x", true)).unwrap();
        store.add(StoredAccount { name: "Ana S.".into(), ..a("d1", false) }).unwrap();
        let list = store.accounts();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "Ana S.");
        store.remove("d1").unwrap();
        assert_eq!(store.accounts(), [a("b!x", true)]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
