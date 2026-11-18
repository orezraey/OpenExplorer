//! The servers the user added: their addresses in `servers.json`, their passwords in the OS
//! credential store, never on disk in the clear.

use std::path::PathBuf;

use openexplorer_i18n::tr;

use crate::{Result, Server};

const SERVICE: &str = "OpenExplorer servidor";

/// Where the list is kept: the app's config folder, a temporary one in tests.
#[derive(Clone, Debug)]
pub struct Store {
    pub file: PathBuf,
}

impl Store {
    pub fn at(file: PathBuf) -> Store {
        Store { file }
    }

    pub fn servers(&self) -> Vec<Server> {
        std::fs::read_to_string(&self.file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn write(&self, list: &[Server]) -> Result<()> {
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| tr!("Não consegui criar {}: {e}", "Couldn't create {}: {e}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(list).map_err(|e| e.to_string())?;
        std::fs::write(&self.file, text).map_err(|e| tr!("Não consegui salvar a lista de servidores: {e}", "Couldn't save the list of servers: {e}"))
    }

    /// Adds or replaces `server`, and its password: `None` keeps the one stored, `Some("")`
    /// forgets it.
    pub fn save(&self, server: Server, secret: Option<&str>) -> Result<()> {
        // MEGA accounts live in the MEGA crate's store; one here would make an older app read
        // the whole list as empty.
        if server.is_mega() {
            return Err(openexplorer_i18n::t("Contas MEGA não ficam na lista de servidores.", "MEGA accounts aren't kept in the list of servers.").into());
        }
        match secret {
            Some("") => forget_secret(&server.id),
            Some(s) => entry(&server.id)?.set_password(s).map_err(|e| tr!("Não consegui guardar a senha no cofre do sistema: {e}", "Couldn't keep the password in the system's credential store: {e}"))?,
            None => {}
        }
        let mut list = self.servers();
        match list.iter_mut().find(|s| s.id == server.id) {
            Some(s) => *s = server,
            None => list.push(server),
        }
        self.write(&list)
    }

    /// Forgets a server and its password.
    pub fn remove(&self, id: &str) -> Result<()> {
        forget_secret(id);
        let mut list = self.servers();
        list.retain(|s| s.id != id);
        self.write(&list)
    }
}

fn entry(id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, id).map_err(|e| tr!("Cofre de credenciais indisponível: {e}", "Credential store unavailable: {e}"))
}

fn forget_secret(id: &str) {
    if let Ok(e) = entry(id) {
        let _ = e.delete_credential();
    }
}

/// The password stored for server `id`, if any.
pub fn secret(id: &str) -> Option<String> {
    entry(id).ok()?.get_password().ok()
}

/// A new server id: the time and a random number, never reused.
pub fn new_id() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("s{nanos:x}")
}
