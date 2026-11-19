//! Dropbox for Open Explorer: the OAuth login of a desktop app (PKCE, no secret, a refresh
//! token), the accounts and their tokens, and the API v2 calls the panes need: listing, new
//! folders, renaming, moving, copying on Dropbox's side, deleting and restoring what was
//! deleted, downloads that resume, uploads (one request up to 150 MiB, an upload session in
//! blocks above), the storage quota, shared links and the changes made elsewhere.
//!
//! Everything here blocks; the app runs it on background threads.
//!
//! # Why our own client
//!
//! Dropbox maintains a Rust SDK, `dropbox-sdk` (0.21.0, Apache-2.0), generated from the API's
//! Stone spec. Its sync client is on ureq 3, like the app, and everything it pulls in was already
//! in Cargo.lock; trimmed to files, sharing and users it builds in about 5 s. It was weighed and
//! set aside because its HTTP side doesn't fit:
//!
//! - its ureq client has no timeouts at all (a stalled download or a dropped longpoll would hang
//!   the thread for good), is `https_only` and has the hosts written in, so neither a timeout nor
//!   a stand-in server for tests and benchmarks can be had without writing the HTTP client
//!   ourselves anyway (its `HttpClient` trait);
//! - a request's body is a `&[u8]`: an upload holds each block in memory and reports no
//!   progress within it;
//! - its `Dropbox-API-Arg` escaping writes a character past U+FFFF as `\u1f600`, which isn't a
//!   JSON escape: a file named with an emoji can't be reached;
//! - each route has its own error enum to turn into the user's words, where the API's
//!   `error_summary` already says it in one string.
//!
//! What's left once the HTTP client is ours is a few dozen JSON shapes, so this crate is a
//! blocking client of the HTTP API on ureq, like the Google Drive and MEGA crates; the SDK's
//! sources and the API reference were the reference for routes, arguments and errors.

pub mod account;
pub mod api;
pub mod cache;
pub mod content;
pub mod meta;
pub mod oauth;
pub mod store;
pub mod transfer;

#[cfg(test)]
mod bench;
#[cfg(test)]
pub(crate) mod fake;

use std::path::PathBuf;

use openexplorer_i18n::{t, tr};
use serde::{Deserialize, Serialize};

pub use account::{Account, Changes, Quota, cached, connect, disconnect, reach, save_caches};
pub use api::set_log;
pub use meta::Entry;

/// Errors are shown to the user as they are, so they're plain sentences in the interface's
/// language (see `openexplorer_i18n`).
pub type Result<T> = std::result::Result<T, String>;

/// The error of a transfer stopped by Cancelar.
pub const CANCELLED: &str = "Cancelado.";

/// The loopback port the login listens on unless the app's file says another: Dropbox matches
/// a redirect URI exactly, port included, so the app registered in the App Console must list
/// `http://localhost:53719`.
pub const DEFAULT_PORT: u16 = 53719;

/// The Dropbox app the login goes through. Each person registers their own in the Dropbox App
/// Console (a "Scoped access", "Full Dropbox" app) until Open Explorer has one of its own: only
/// its app key is needed, as the login uses PKCE and keeps no secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    pub app_key: String,
    /// The loopback port registered as `http://localhost:<port>`.
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

impl AppConfig {
    /// Reads [`app_file`] when there is one, else `OPENEXPLORER_DROPBOX_APP_KEY` (and
    /// `OPENEXPLORER_DROPBOX_PORT`).
    pub fn load() -> Result<AppConfig> {
        let path = app_file();
        if let Ok(text) = std::fs::read_to_string(&path) {
            return AppConfig::parse(&text).map_err(|e| format!("{}: {e}", path.display()));
        }
        let env = |k| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        if let Some(app_key) = env("OPENEXPLORER_DROPBOX_APP_KEY") {
            let port = match env("OPENEXPLORER_DROPBOX_PORT") {
                Some(p) => p.parse().map_err(|_| tr!("OPENEXPLORER_DROPBOX_PORT não é uma porta: {p}", "OPENEXPLORER_DROPBOX_PORT isn't a port: {p}"))?,
                None => DEFAULT_PORT,
            };
            return Ok(AppConfig { app_key, port });
        }
        Err(tr!(
            "App do Dropbox não configurado. Informe a App key do seu app (Dropbox App Console) ou salve {{\"app_key\": \"…\"}} em {}",
            "Dropbox app not set up. Enter your app's App key (Dropbox App Console) or save {{\"app_key\": \"…\"}} to {}",
            path.display()
        ))
    }

    /// `{"app_key": "…", "port": 53719}`, the port optional.
    pub fn parse(text: &str) -> Result<AppConfig> {
        let text = text.trim_start_matches('\u{feff}');
        let mut c: AppConfig = serde_json::from_str(text).map_err(|e| tr!("não é um JSON válido com app_key: {e}", "is not valid JSON with an app_key: {e}"))?;
        c.app_key = c.app_key.trim().to_string();
        if c.app_key.is_empty() || !c.app_key.chars().all(|ch| ch.is_ascii_alphanumeric()) {
            return Err(t("a App key deve ter só letras e números (está na aba Settings do app no Dropbox App Console).", "the App key must be letters and digits only (it's on the app's Settings tab in the Dropbox App Console).").into());
        }
        if c.port == 0 {
            return Err(t("a porta não pode ser 0.", "the port can't be 0.").into());
        }
        Ok(c)
    }

    /// Saves to [`app_file`].
    pub fn save(&self) -> Result<()> {
        let dir = config_dir();
        std::fs::create_dir_all(&dir).map_err(|e| tr!("Não consegui criar {}: {e}", "Couldn't create {}: {e}", dir.display()))?;
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(app_file(), text).map_err(|e| tr!("Não consegui salvar {}: {e}", "Couldn't save {}: {e}", app_file().display()))
    }

    /// The redirect URI to register: `http://localhost:<port>`.
    pub fn redirect(&self) -> String {
        format!("http://localhost:{}", self.port)
    }
}

/// `%APPDATA%\OpenExplorer` (or `~/.config/OpenExplorer` elsewhere), as the other accounts use.
pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("OpenExplorer")
}

/// `dropbox-client.json` in [`config_dir`].
pub fn app_file() -> PathBuf {
    config_dir().join("dropbox-client.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_file_needs_a_key_and_may_name_a_port() {
        let c = AppConfig::parse("\u{feff}{\"app_key\": \" abc123xyz \"}").unwrap();
        assert_eq!((c.app_key.as_str(), c.port), ("abc123xyz", DEFAULT_PORT));
        assert_eq!(c.redirect(), "http://localhost:53719");
        assert_eq!(AppConfig::parse(r#"{"app_key":"k1","port":8765}"#).unwrap().port, 8765);
        assert!(AppConfig::parse(r#"{"app_key":""}"#).is_err());
        assert!(AppConfig::parse(r#"{"app_key":"has space"}"#).is_err());
        assert!(AppConfig::parse("{}").is_err());
        assert!(AppConfig::parse(r#"{"app_key":"k","port":0}"#).is_err());
    }
}
