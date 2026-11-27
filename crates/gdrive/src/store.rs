//! Connected accounts: their e-mail and name in `accounts.json` under [`crate::config_dir`],
//! their refresh tokens in the OS credential store, never on disk in the clear.

use openexplorer_i18n::{t, tr};
use serde::{Deserialize, Serialize};

use crate::{Result, config_dir};

const SERVICE: &str = "OpenExplorer Google Drive";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredAccount {
    pub email: String,
    pub name: String,
    /// The OAuth client the account signed in with: its refresh token works only with that
    /// client, so the app's built-in one and a client of the user's own can both have accounts.
    /// Empty for accounts saved before it was kept (they use [`crate::ClientConfig::load`]).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_id: String,
}

/// The client account `email` signed in with ([`crate::ClientConfig::for_account`]).
pub fn client_of(email: &str) -> String {
    accounts().into_iter().find(|a| a.email == email).map(|a| a.client_id).unwrap_or_default()
}

fn accounts_file() -> std::path::PathBuf {
    config_dir().join("accounts.json")
}

pub fn accounts() -> Vec<StoredAccount> {
    std::fs::read_to_string(accounts_file()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn write_accounts(list: &[StoredAccount]) -> Result<()> {
    std::fs::create_dir_all(config_dir()).map_err(|e| tr!("Não consegui criar {}: {e}", "Couldn't create {}: {e}", config_dir().display()))?;
    let text = serde_json::to_string_pretty(list).map_err(|e| e.to_string())?;
    std::fs::write(accounts_file(), text).map_err(|e| tr!("Não consegui salvar a lista de contas: {e}", "Couldn't save the list of accounts: {e}"))
}

fn entry(email: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, email).map_err(|e| tr!("Cofre de credenciais indisponível: {e}", "Credential store unavailable: {e}"))
}

/// Adds or updates an account and stores its refresh token.
pub fn save(account: StoredAccount, refresh_token: &str) -> Result<()> {
    entry(&account.email)?.set_password(refresh_token).map_err(|e| tr!("Não consegui guardar o acesso no cofre do sistema: {e}", "Couldn't keep the access in the system's credential store: {e}"))?;
    let mut list = accounts();
    match list.iter_mut().find(|a| a.email == account.email) {
        Some(a) => *a = account,
        None => list.push(account),
    }
    write_accounts(&list)
}

pub fn refresh_token(email: &str) -> Result<String> {
    entry(email)?.get_password().map_err(|_| t("O acesso desta conta não está no cofre do sistema. Conecte a conta de novo.", "This account's access isn't in the system's credential store. Connect the account again.").to_string())
}

/// Forgets an account and deletes its refresh token.
pub fn remove(email: &str) -> Result<()> {
    if let Ok(e) = entry(email) {
        let _ = e.delete_credential();
    }
    let mut list = accounts();
    list.retain(|a| a.email != email);
    write_accounts(&list)
}
