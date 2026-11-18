//! Google Drive for Open Explorer: the OAuth login of a desktop app (PKCE with a loopback
//! redirect), the accounts and their refresh tokens, and the Drive v3 calls the panes need.
//!
//! Everything here blocks; the app runs it on background threads.

pub mod api;
pub mod changes;
pub mod download;
pub mod oauth;
pub mod quota;
pub mod share;
pub mod store;
pub mod search;
pub mod upload;

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use api::{Account, Details, Drive, DriveFile, Place, Revision, SharedDrive, Top, View, valid_id};
pub use changes::Change;
pub use quota::Quota;
pub use share::{Access, Permission, Role, Sharing};

/// Errors are shown to the user as they are, so they're plain Portuguese sentences.
pub type Result<T> = std::result::Result<T, String>;

/// The OAuth client the app logs in with. Google only lets an app reach every file in a Drive
/// (the `drive` scope) once it passes a security review, so until then each person creates their
/// own client in Google Cloud and points the app at it.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct ClientConfig {
    pub client_id: String,
    pub client_secret: String,
}

impl std::fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConfig").field("client_id", &self.client_id).field("client_secret", &"<redacted>").finish()
    }
}

impl ClientConfig {
    /// Reads the client from [`client_file`] when saved, else from `OPENEXPLORER_GOOGLE_CLIENT_ID`
    /// and `OPENEXPLORER_GOOGLE_CLIENT_SECRET`: either the JSON Google Cloud downloads for a desktop
    /// client (`{"installed": {...}}`) or `{"client_id": .., "client_secret": ..}`.
    pub fn load() -> Result<ClientConfig> {
        let path = client_file();
        if let Ok(text) = std::fs::read_to_string(&path) {
            return ClientConfig::parse(&text).map_err(|e| format!("{}: {e}", path.display()));
        }
        let env_non_empty = |k| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        match (env_non_empty("OPENEXPLORER_GOOGLE_CLIENT_ID"), env_non_empty("OPENEXPLORER_GOOGLE_CLIENT_SECRET")) {
            (Some(id), Some(secret)) => return Ok(ClientConfig { client_id: id, client_secret: secret }),
            (Some(_), None) => return Err("OPENEXPLORER_GOOGLE_CLIENT_ID está definida, mas falta OPENEXPLORER_GOOGLE_CLIENT_SECRET.".into()),
            (None, Some(_)) => return Err("OPENEXPLORER_GOOGLE_CLIENT_SECRET está definida, mas falta OPENEXPLORER_GOOGLE_CLIENT_ID.".into()),
            (None, None) => {}
        }
        Err(format!(
            "Credenciais do Google não encontradas. Salve o JSON do cliente OAuth (tipo \"App para computador\") em {}",
            path.display()
        ))
    }

    pub fn parse(text: &str) -> Result<ClientConfig> {
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
        let text = text.trim_start_matches('\u{feff}');
        let f: File = serde_json::from_str(text).map_err(|e| format!("não é um JSON válido: {e}"))?;
        let (id, secret) = match (f.installed, f.client_id, f.client_secret) {
            (Some(i), _, _) => (i.client_id, i.client_secret),
            (None, Some(id), Some(secret)) => (id, secret),
            _ => return Err("não tem client_id e client_secret de um app para computador (um cliente \"web\" não serve).".into()),
        };
        if id.trim().is_empty() || secret.trim().is_empty() {
            return Err("client_id e client_secret não podem estar vazios.".into());
        }
        Ok(ClientConfig { client_id: id.trim().to_string(), client_secret: secret.trim().to_string() })
    }

    /// Saves the OAuth client credentials to [`client_file`] (restricted to owner on Unix).
    pub fn save(&self) -> Result<()> {
        use std::io::Write;
        let dir = config_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("Não consegui criar {}: {e}", dir.display()))?;
        let path = client_file();
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&path).map_err(|e| format!("Não consegui salvar {}: {e}", path.display()))?;
        f.write_all(text.as_bytes()).map_err(|e| format!("Não consegui salvar {}: {e}", path.display()))
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

pub fn log_file() -> PathBuf {
    local_dir().join("openexplorer.log")
}

/// Appends a timestamped single-line entry to `openexplorer.log` in [`local_dir`]: request timings
/// and events, to tell a slow network from a slow app. Starts over past 1 MB. Never fails.
pub fn log(line: &str) {
    use std::io::Write;
    let path = log_file();
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1 << 20)
        && std::fs::OpenOptions::new().write(true).truncate(true).open(&path).is_err()
    {
        let _ = std::fs::remove_file(&path);
    }
    let clean = line.replace("\r\n", " ⏎ ").replace(['\r', '\n'], " ⏎ ");
    let _ = std::fs::create_dir_all(local_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} {clean}", chrono::Local::now().format("%H:%M:%S%.3f"));
    }
}

/// Reads the last `max_lines` lines of [`log_file`] (tolerating non-UTF-8 bytes), along with its
/// total size in bytes.
pub fn read_log(max_lines: usize) -> (Vec<String>, u64) {
    let path = log_file();
    let Ok(raw) = std::fs::read(&path) else { return (Vec::new(), 0) };
    let bytes = raw.len() as u64;
    let text = String::from_utf8_lossy(&raw);
    let all: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let skip = all.len().saturating_sub(max_lines);
    let tail = all.into_iter().skip(skip).map(str::to_string).collect();
    (tail, bytes)
}

/// Empties [`log_file`] by truncating in place so open readers on Windows don't block it.
pub fn clear_log() -> Result<()> {
    let path = log_file();
    match std::fs::OpenOptions::new().write(true).truncate(true).open(&path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("Não consegui limpar {}: {e}", path.display())),
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

/// The agent uploads go through: a big file takes as long as it takes, so there's no overall
/// limit, only on connecting and on a stalled connection.
pub(crate) fn upload_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        // Drive answers 308 Resume Incomplete mid-upload: a status to read, not a redirect.
        .max_redirects(0)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_per_write(Some(Duration::from_secs(120)))
        .timeout_recv_response(Some(Duration::from_secs(300)))
        .build()
        .into()
}

/// The agent downloads go through: like [`upload_agent`] no overall limit, but a read that
/// waits two minutes for a byte gives up, so a stalled connection is asked again from where
/// it stopped instead of hanging the download.
pub(crate) fn download_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(120)))
        .timeout_per_read(Some(Duration::from_secs(120)))
        .build()
        .into()
}

/// The error of an upload (or any transfer) stopped by Cancelar.
pub const CANCELLED: &str = "Cancelado.";

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
pub(crate) fn google_reason(body: &str) -> String {
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
        let flat = ClientConfig::parse("\u{feff}{\"client_id\":\"a\",\"client_secret\":\"segredo\"}").unwrap();
        assert_eq!(flat.client_id, "a");
        assert!(!format!("{flat:?}").contains("segredo"));
        assert!(ClientConfig::parse(r#"{"web":{"client_id":"a","client_secret":"b"}}"#).unwrap_err().contains("web"));
    }

    #[test]
    fn google_errors_read_as_their_message() {
        assert_eq!(google_reason(r#"{"error":"invalid_grant","error_description":"Bad Request"}"#), "Bad Request");
        assert_eq!(google_reason(r#"{"error":{"code":403,"message":"Drive API has not been used"}}"#), "Drive API has not been used");
    }
}
