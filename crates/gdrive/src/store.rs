//! Connected accounts: their e-mail and name in `accounts.json` under [`crate::config_dir`],
//! their refresh tokens in the OS credential store, never on disk in the clear.

use serde::{Deserialize, Serialize};

use crate::{Result, config_dir};

const SERVICE: &str = "OpenExplorer Google Drive";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredAccount {
    pub email: String,
    pub name: String,
}

fn accounts_file() -> std::path::PathBuf {
    config_dir().join("accounts.json")
}

pub fn accounts() -> Vec<StoredAccount> {
    std::fs::read_to_string(accounts_file()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn write_accounts(list: &[StoredAccount]) -> Result<()> {
    std::fs::create_dir_all(config_dir()).map_err(|e| format!("Não consegui criar {}: {e}", config_dir().display()))?;
    let text = serde_json::to_string_pretty(list).map_err(|e| e.to_string())?;
    std::fs::write(accounts_file(), text).map_err(|e| format!("Não consegui salvar a lista de contas: {e}"))
}

fn entry(email: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, email).map_err(|e| format!("Cofre de credenciais indisponível: {e}"))
}

/// Adds or updates an account and stores its refresh token.
pub fn save(account: StoredAccount, refresh_token: &str) -> Result<()> {
    entry(&account.email)?.set_password(refresh_token).map_err(|e| format!("Não consegui guardar o acesso no cofre do sistema: {e}"))?;
    let mut list = accounts();
    match list.iter_mut().find(|a| a.email == account.email) {
        Some(a) => *a = account,
        None => list.push(account),
    }
    write_accounts(&list)
}

pub fn refresh_token(email: &str) -> Result<String> {
    entry(email)?.get_password().map_err(|_| "O acesso desta conta não está no cofre do sistema. Conecte a conta de novo.".to_string())
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
