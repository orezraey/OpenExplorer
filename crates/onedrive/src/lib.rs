//! OneDrive for Open Explorer, personal and work or school accounts, through Microsoft Graph:
//! the login of a desktop app (OAuth 2.0 authorization code with PKCE, a public client with a
//! loopback redirect), the accounts and their refresh tokens in the OS credential store, and the
//! calls the panes need: listing, new folders, renaming, moving, deleting to the recycle bin and
//! restoring from it, copying on the server, resumable downloads, uploads in sessions, the
//! storage quota, sharing links and the changes made elsewhere. A work or school account's
//! recycle bin, which Graph doesn't reach, goes through SharePoint's REST API (see [`bin`]).
//!
//! Everything here blocks; the app runs it on background threads.
//!
//! # Why our own client
//!
//! - Microsoft publishes Graph SDKs for C#, Go, Java, JavaScript, PHP, PowerShell and Python,
//!   none for Rust, and no MSAL for Rust either; Kiota, its generator for other languages, has
//!   no Rust target.
//! - `graph-rs-sdk`, the community crate, is async on tokio and reqwest (its blocking client
//!   wraps a runtime), beside an app that has neither and talks HTTP through ureq.
//! - What the app needs is a few dozen REST calls; a blocking client of them on ureq, like the
//!   Google Drive and MEGA crates, built only from crate versions Cargo.lock already has.
//!
//! # Speed
//!
//! The drive's folders are kept as they were listed, by item id, in a tree (see [`tree`]); the
//! account's delta feed (`/root/delta`) brings what changed anywhere since, so a folder opened
//! again costs one small delta request instead of its whole listing, and a pane showing it is
//! told when it changes. The tree and the delta position are kept between runs, sealed (see
//! [`cache`]). Throttling (429, 503) pauses the whole account for as long as `Retry-After` says.

pub mod account;
pub mod bin;
pub mod cache;
pub mod graph;
pub mod oauth;
pub mod quickxor;
pub mod store;
pub mod transfer;
pub mod tree;
pub mod vault;

#[cfg(test)]
mod bench;
#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod scenarios;

use std::path::PathBuf;
use std::sync::RwLock;

use openexplorer_i18n::{t, tr};
use serde::{Deserialize, Serialize};

pub use account::{Account, Area, Changes, Item, Kind, Quota, cached, connect, disconnect, reach, save_caches};
pub use bin::{Access as BinAccess, BinItem};
pub use cache::set_dir as set_cache_dir;
pub use oauth::PendingLogin;
pub use store::{Store, StoredAccount};

/// Errors are shown to the user as they are, so they're plain sentences in the interface's
/// language (see `openexplorer_i18n`).
pub type Result<T> = std::result::Result<T, String>;

/// The error of a transfer stopped by Cancelar.
pub const CANCELLED: &str = "Cancelado.";

/// Open Explorer's own app registration in Microsoft Entra (personal and work or school
/// accounts, public client, no secret): what the login uses unless a client of one's own is set
/// (see [`ClientConfig::load`]). An app's client id isn't a secret; the login proves itself with
/// PKCE.
pub const BUILT_IN_CLIENT_ID: &str = "9f69a854-17ff-417a-a6a7-dda1351adcb7";

/// The OAuth client the app signs in with: Open Explorer's own registration
/// ([`BUILT_IN_CLIENT_ID`]), or one a person or organization registered in Microsoft Entra and
/// set instead. Public: it has no secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ClientConfig {
    pub client_id: String,
    /// Who may sign in: `common` (personal and work or school accounts), `consumers`
    /// (personal only), `organizations` (work or school only) or one directory's id.
    pub tenant: String,
}

/// What the person has to register in Microsoft Entra for the login to work, said wherever the
/// client is missing.
pub fn registration_help() -> String {
    tr!(
        "Registre um aplicativo no portal do Microsoft Entra (entra.microsoft.com › Registros de aplicativo): tipos de conta \"Contas em qualquer diretório organizacional e contas pessoais da Microsoft\"; plataforma \"Aplicativos móveis e da área de trabalho\" com o URI de redirecionamento http://localhost; \"Permitir fluxos de clientes públicos\" ligado; permissões delegadas do Microsoft Graph Files.ReadWrite.All, offline_access e User.Read (e, para a Lixeira de contas corporativas ou de estudante, a do SharePoint AllSites.Write). Depois salve {{\"client_id\": \"<ID do aplicativo>\"}} em {} ou defina OPENEXPLORER_ONEDRIVE_CLIENT_ID.",
        "Register an app in the Microsoft Entra portal (entra.microsoft.com › App registrations): account types \"Accounts in any organizational directory and personal Microsoft accounts\"; platform \"Mobile and desktop applications\" with the redirect URI http://localhost; \"Allow public client flows\" on; Microsoft Graph delegated permissions Files.ReadWrite.All, offline_access and User.Read (and, for work or school accounts' recycle bin, SharePoint's AllSites.Write). Then save {{\"client_id\": \"<Application ID>\"}} to {} or set OPENEXPLORER_ONEDRIVE_CLIENT_ID.",
        client_file().display()
    )
}

impl ClientConfig {
    /// Reads the client from [`client_file`] when saved, else from
    /// `OPENEXPLORER_ONEDRIVE_CLIENT_ID` (and `OPENEXPLORER_ONEDRIVE_TENANT`, `common` if unset),
    /// else Open Explorer's own ([`BUILT_IN_CLIENT_ID`]).
    pub fn load() -> Result<ClientConfig> {
        let path = client_file();
        if let Ok(text) = std::fs::read_to_string(&path) {
            return ClientConfig::parse(&text).map_err(|e| format!("{}: {e}", path.display()));
        }
        let env = |k| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        if let Some(id) = env("OPENEXPLORER_ONEDRIVE_CLIENT_ID") {
            let cfg = ClientConfig { client_id: id, tenant: env("OPENEXPLORER_ONEDRIVE_TENANT").unwrap_or_else(|| "common".into()) };
            cfg.check()?;
            return Ok(cfg);
        }
        Ok(ClientConfig::built_in())
    }

    /// Open Explorer's own registration, for personal and work or school accounts.
    pub fn built_in() -> ClientConfig {
        ClientConfig { client_id: BUILT_IN_CLIENT_ID.into(), tenant: "common".into() }
    }

    /// `{"client_id": "…", "tenant": "common"}`, the tenant optional; also the application
    /// (client) id alone.
    pub fn parse(text: &str) -> Result<ClientConfig> {
        #[derive(Deserialize)]
        struct File {
            client_id: Option<String>,
            #[serde(alias = "appId", alias = "application_id")]
            app_id: Option<String>,
            tenant: Option<String>,
        }
        let text = text.trim().trim_start_matches('\u{feff}').trim();
        let cfg = if text.starts_with('{') {
            let f: File = serde_json::from_str(text).map_err(|e| tr!("não é um JSON válido: {e}", "is not valid JSON: {e}"))?;
            let id = f.client_id.or(f.app_id).unwrap_or_default();
            ClientConfig { client_id: id.trim().to_string(), tenant: f.tenant.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).unwrap_or_else(|| "common".into()) }
        } else {
            ClientConfig { client_id: text.to_string(), tenant: "common".into() }
        };
        cfg.check()?;
        Ok(cfg)
    }

    fn check(&self) -> Result<()> {
        if !is_guid(&self.client_id) {
            return Err(t(
                "o client_id deve ser o \"ID do aplicativo (cliente)\" do registro, no formato 00000000-0000-0000-0000-000000000000.",
                "client_id must be the registration's \"Application (client) ID\", shaped 00000000-0000-0000-0000-000000000000.",
            )
            .into());
        }
        let tenant_ok = matches!(self.tenant.as_str(), "common" | "consumers" | "organizations") || is_guid(&self.tenant) || self.tenant.contains('.');
        if !tenant_ok || self.tenant.contains('/') {
            return Err(t("tenant deve ser common, consumers, organizations ou o ID (ou domínio) de um diretório.", "tenant must be common, consumers, organizations or a directory's ID (or domain).").into());
        }
        Ok(())
    }

    /// Saves the client to [`client_file`].
    pub fn save(&self) -> Result<()> {
        let dir = config_dir();
        std::fs::create_dir_all(&dir).map_err(|e| tr!("Não consegui criar {}: {e}", "Couldn't create {}: {e}", dir.display()))?;
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(client_file(), text).map_err(|e| tr!("Não consegui salvar {}: {e}", "Couldn't save {}: {e}", client_file().display()))
    }
}

fn is_guid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.iter().map(|p| p.len()).eq([8, 4, 4, 4, 12]) && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// `%APPDATA%\OpenExplorer` (or `~/.config/OpenExplorer` elsewhere), as the other crates.
pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("OpenExplorer")
}

pub fn client_file() -> PathBuf {
    config_dir().join("onedrive-client.json")
}

static LOG: RwLock<Option<fn(&str)>> = RwLock::new(None);

/// Where the crate's log lines go (the app's log file); nowhere until set.
pub fn set_log(f: fn(&str)) {
    *LOG.write().unwrap_or_else(|e| e.into_inner()) = Some(f);
}

pub(crate) fn log(line: &str) {
    if let Some(f) = *LOG.read().unwrap_or_else(|e| e.into_inner()) {
        f(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_registration_is_a_valid_client() {
        let c = ClientConfig::built_in();
        assert!(c.check().is_ok());
        assert_eq!((c.client_id.as_str(), c.tenant.as_str()), (BUILT_IN_CLIENT_ID, "common"));
    }

    #[test]
    fn the_client_reads_as_json_or_as_the_id_alone() {
        let id = "0b1c2d3e-4f50-6172-8394-a5b6c7d8e9f0";
        let c = ClientConfig::parse(&format!("\u{feff}{{\"client_id\": \"{id}\"}}")).unwrap();
        assert_eq!((c.client_id.as_str(), c.tenant.as_str()), (id, "common"));
        let c = ClientConfig::parse(&format!("{{\"appId\": \"{id}\", \"tenant\": \"consumers\"}}")).unwrap();
        assert_eq!(c.tenant, "consumers");
        assert_eq!(ClientConfig::parse(id).unwrap().client_id, id);
        assert!(ClientConfig::parse("{\"client_id\": \"abc\"}").unwrap_err().contains("client_id"));
        assert!(ClientConfig::parse(&format!("{{\"client_id\": \"{id}\", \"tenant\": \"a/b\"}}")).is_err());
        assert!(registration_help().contains("http://localhost"));
    }
}
