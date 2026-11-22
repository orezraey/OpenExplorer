//! The Telegram accounts connected: their user id, name and storage channel in
//! `telegram-accounts.json` (a file of their own, so an older Open Explorer reading `servers.json`
//! never meets an entry it doesn't know). Their sessions and api keys are in the OS credential
//! store (`session`), never on disk.

use std::path::PathBuf;

use openexplorer_i18n::tr;
use serde::{Deserialize, Serialize};

use crate::Result;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredAccount {
    pub user_id: i64,
    /// The person's name on Telegram, to show.
    #[serde(default)]
    pub name: String,
    /// The storage channel: its id and access hash.
    pub channel_id: i64,
    pub channel_hash: i64,
    #[serde(default)]
    pub premium: bool,
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

    pub fn get(&self, user_id: i64) -> Option<StoredAccount> {
        self.accounts().into_iter().find(|a| a.user_id == user_id)
    }

    fn write(&self, list: &[StoredAccount]) -> Result<()> {
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| tr!("Não consegui criar {}: {e}", "Couldn't create {}: {e}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(list).map_err(|e| e.to_string())?;
        std::fs::write(&self.file, text).map_err(|e| tr!("Não consegui salvar a lista de contas do Telegram: {e}", "Couldn't save the list of Telegram accounts: {e}"))
    }

    /// Adds `account`, or updates the one with its user id.
    pub fn add(&self, account: StoredAccount) -> Result<()> {
        let mut list = self.accounts();
        match list.iter_mut().find(|a| a.user_id == account.user_id) {
            Some(a) => *a = account,
            None => list.push(account),
        }
        self.write(&list)
    }

    pub fn remove(&self, user_id: i64) -> Result<()> {
        let mut list = self.accounts();
        list.retain(|a| a.user_id != user_id);
        self.write(&list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_keeps_each_account_once() {
        let dir = std::env::temp_dir().join(format!("oe-telegram-store-{}", std::process::id()));
        let store = Store::at(dir.join("telegram-accounts.json"));
        assert!(store.accounts().is_empty());
        let a = |id: i64, name: &str| StoredAccount { user_id: id, name: name.into(), channel_id: 5, channel_hash: 6, premium: false };
        store.add(a(1, "Ana")).unwrap();
        store.add(a(1, "Ana Souza")).unwrap();
        store.add(a(2, "Bia")).unwrap();
        assert_eq!(store.accounts().len(), 2);
        assert_eq!(store.get(1).unwrap().name, "Ana Souza");
        store.remove(1).unwrap();
        assert_eq!(store.accounts(), vec![a(2, "Bia")]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
