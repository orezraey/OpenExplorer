//! Google Drive for Open Explorer: the OAuth login of a desktop app (PKCE with a loopback
//! redirect), the accounts and their refresh tokens, and the Drive v3 calls the panes need.
//!
//! Everything here blocks; the app runs it on background threads.

pub mod api;
pub mod oauth;
pub mod store;

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

pub use api::{Account, Drive, DriveFile, SharedDrive};

/// Errors are shown to the user as they are, so they're plain Portuguese sentences.
pub type Result<T> = std::result::Result<T, String>;

/// The OAuth client the app logs in with. Google only lets an app reach every file in a Drive
/// (the `drive` scope) once it passes a security review, so until then each person creates their
/// own client in Google Cloud and points the app at it.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub client_id: String,
    pub client_secret: String,
}

impl ClientConfig {
    /// Reads the client from `OPENEXPLORER_GOOGLE_CLIENT_ID` and `OPENEXPLORER_GOOGLE_CLIENT_SECRET`,
    /// else from [`client_file`]: either the JSON Google Cloud downloads for a desktop client
    /// (`{"installed": {...}}`) or `{"client_id": .., "client_secret": ..}`.
    pub fn load() -> Result<ClientConfig> {
        match (std::env::var("OPENEXPLORER_GOOGLE_CLIENT_ID"), std::env::var("OPENEXPLORER_GOOGLE_CLIENT_SECRET")) {
            (Ok(id), Ok(secret)) => return Ok(ClientConfig { client_id: id, client_secret: secret }),
            (Ok(_), Err(_)) => return Err("OPENEXPLORER_GOOGLE_CLIENT_ID está definida, mas falta OPENEXPLORER_GOOGLE_CLIENT_SECRET.".into()),
            (Err(_), Ok(_)) => return Err("OPENEXPLORER_GOOGLE_CLIENT_SECRET está definida, mas falta OPENEXPLORER_GOOGLE_CLIENT_ID.".into()),
            (Err(_), Err(_)) => {}
        }
        let path = client_file();
        let text = std::fs::read_to_string(&path).map_err(|_| {
            format!("Credenciais do Google não encontradas. Salve o JSON do cliente OAuth (tipo \"App para computador\") em {}", path.display())
        })?;
        ClientConfig::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn parse(text: &str) -> Result<ClientConfig> {
        #[derive(Deserialize)]
        struct Inner {
            client_id: String,
            client_secret: String,
        }
        #[derive(Deserialize)]
        struct File {
            installed: Option<Inner>,
            client_id: Option<String>,
            client_secret: Option<String>,
        }
        let f: File = serde_json::from_str(text).map_err(|e| format!("não é um JSON válido: {e}"))?;
        match (f.installed, f.client_id, f.client_secret) {
            (Some(i), _, _) => Ok(ClientConfig { client_id: i.client_id, client_secret: i.client_secret }),
            (None, Some(id), Some(secret)) => Ok(ClientConfig { client_id: id, client_secret: secret }),
            _ => Err("não tem client_id e client_secret de um app para computador (um cliente \"web\" não serve).".into()),
        }
    }
}

/// `%APPDATA%\OpenExplorer` (or `~/.config/OpenExplorer` elsewhere).
pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("OpenExplorer")
}

/// `%LOCALAPPDATA%\OpenExplorer`: files tied to this machine (cache, log), kept out of a roaming
/// profile. [`config_dir`] where there's no such folder.
pub fn local_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("OpenExplorer")).unwrap_or_else(config_dir)
}

/// Appends a timestamped line to `openexplorer.log` in [`local_dir`]: request timings, to tell a
/// slow network from a slow app. Starts over past 1 MB. Never fails the caller.
pub fn log(line: &str) {
    use std::io::Write;
    let path = local_dir().join("openexplorer.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1 << 20) {
        let _ = std::fs::remove_file(&path);
    }
    let _ = std::fs::create_dir_all(local_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} {line}", chrono::Local::now().format("%H:%M:%S%.3f"));
    }
}

pub fn client_file() -> PathBuf {
    config_dir().join("google-client.json")
}

/// An HTTP agent that hands back 4xx and 5xx responses instead of failing on them, so the
/// caller can read Google's error message.
pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(60))).build().into()
}

/// Reads a response as JSON, or turns an error status into a message with Google's reason.
pub(crate) fn json<T: serde::de::DeserializeOwned>(mut resp: ureq::http::Response<ureq::Body>, what: &str) -> Result<T> {
    let status = resp.status().as_u16();
    let body = resp.body_mut().read_to_string().map_err(|e| format!("{what}: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(format!("{what}: HTTP {status}: {}", google_reason(&body)));
    }
    serde_json::from_str(&body).map_err(|e| format!("{what}: resposta inesperada: {e}"))
}

/// The human part of a Google error body (`error_description`, `error.message`), else the body.
fn google_reason(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    v.get("error_description")
        .and_then(|d| d.as_str())
        .or_else(|| v.pointer("/error/message").and_then(|m| m.as_str()))
        .or_else(|| v.get("error").and_then(|e| e.as_str()))
        .map(str::to_string)
        .unwrap_or_else(|| body.chars().take(300).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_json_google_cloud_downloads_for_a_desktop_client() {
        let downloaded = r#"{"installed":{"client_id":"123.apps.googleusercontent.com","project_id":"p","auth_uri":"https://accounts.google.com/o/oauth2/auth","client_secret":"GOCSPX-x","redirect_uris":["http://localhost"]}}"#;
        let c = ClientConfig::parse(downloaded).unwrap();
        assert_eq!((c.client_id.as_str(), c.client_secret.as_str()), ("123.apps.googleusercontent.com", "GOCSPX-x"));
        let flat = ClientConfig::parse(r#"{"client_id":"a","client_secret":"b"}"#).unwrap();
        assert_eq!(flat.client_id, "a");
        assert!(ClientConfig::parse(r#"{"web":{"client_id":"a","client_secret":"b"}}"#).unwrap_err().contains("web"));
    }

    #[test]
    fn google_errors_read_as_their_message() {
        assert_eq!(google_reason(r#"{"error":"invalid_grant","error_description":"Bad Request"}"#), "Bad Request");
        assert_eq!(google_reason(r#"{"error":{"code":403,"message":"Drive API has not been used"}}"#), "Drive API has not been used");
    }
}
