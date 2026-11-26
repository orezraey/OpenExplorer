//! Google Drive for Open Explorer: the OAuth login of a desktop app (PKCE with a loopback
//! redirect), the accounts and their refresh tokens, and the Drive v3 calls the panes need.
//!
//! Everything here blocks; the app runs it on background threads.

pub mod api;
#[cfg(test)]
mod bench;
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

use openexplorer_i18n::{t, tr};
use serde::{Deserialize, Serialize};

pub use api::{Account, Details, Drive, DriveFile, Place, Revision, SharedDrive, Status, Top, View, valid_id};
pub use changes::Change;
pub use quota::Quota;
pub use share::{Access, Permission, Role, Sharing};

/// Errors are shown to the user as they are, so they're plain sentences in the interface's
/// language (see `openexplorer_i18n`).
pub type Result<T> = std::result::Result<T, String>;

/// The OAuth client the app logs in with: a saved file, runtime environment variables, or the
/// client baked into the binary at build time (via `OPENEXPLORER_GOOGLE_CLIENT_ID` and
/// `OPENEXPLORER_GOOGLE_CLIENT_SECRET`).
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
    /// and `OPENEXPLORER_GOOGLE_CLIENT_SECRET` at runtime, else from the credentials baked into the
    /// binary at build time ([`Self::built_in`]).
    ///
    /// Either the JSON Google Cloud downloads for a desktop client (`{"installed": {...}}`) or
    /// `{"client_id": .., "client_secret": ..}`.
    pub fn load() -> Result<ClientConfig> {
        let path = client_file();
        let from_file = std::fs::read_to_string(&path).ok().map(|text| ClientConfig::parse(&text).map_err(|e| format!("{}: {e}", path.display())));
        let env_non_empty = |k| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let from_env = (env_non_empty("OPENEXPLORER_GOOGLE_CLIENT_ID"), env_non_empty("OPENEXPLORER_GOOGLE_CLIENT_SECRET"));
        Self::resolve(from_file, from_env, Self::built_in(), &path)
    }

    /// OAuth client baked into the binary at compile time when `OPENEXPLORER_GOOGLE_CLIENT_ID` and
    /// `OPENEXPLORER_GOOGLE_CLIENT_SECRET` were both set during the build.
    pub fn built_in() -> Option<ClientConfig> {
        let id = option_env!("OPENEXPLORER_GOOGLE_CLIENT_ID").map(str::trim).filter(|v| !v.is_empty())?;
        let secret = option_env!("OPENEXPLORER_GOOGLE_CLIENT_SECRET").map(str::trim).filter(|v| !v.is_empty())?;
        Some(ClientConfig { client_id: id.into(), client_secret: secret.into() })
    }

    fn resolve(
        from_file: Option<Result<ClientConfig>>,
        from_env: (Option<String>, Option<String>),
        built_in: Option<ClientConfig>,
        path: &std::path::Path,
    ) -> Result<ClientConfig> {
        if let Some(res) = from_file {
            return res;
        }
        match from_env {
            (Some(id), Some(secret)) => return Ok(ClientConfig { client_id: id, client_secret: secret }),
            (Some(_), None) => return Err(t("OPENEXPLORER_GOOGLE_CLIENT_ID está definida, mas falta OPENEXPLORER_GOOGLE_CLIENT_SECRET.", "OPENEXPLORER_GOOGLE_CLIENT_ID is set, but OPENEXPLORER_GOOGLE_CLIENT_SECRET is missing.").into()),
            (None, Some(_)) => return Err(t("OPENEXPLORER_GOOGLE_CLIENT_SECRET está definida, mas falta OPENEXPLORER_GOOGLE_CLIENT_ID.", "OPENEXPLORER_GOOGLE_CLIENT_SECRET is set, but OPENEXPLORER_GOOGLE_CLIENT_ID is missing.").into()),
            (None, None) => {}
        }
        if let Some(cfg) = built_in {
            return Ok(cfg);
        }
        Err(tr!(
            "Credenciais do Google não encontradas. Salve o JSON do cliente OAuth (tipo \"App para computador\") em {}",
            "Google credentials not found. Save the OAuth client's JSON (type \"Desktop app\") to {}",
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
        let f: File = serde_json::from_str(text).map_err(|e| tr!("não é um JSON válido: {e}", "is not valid JSON: {e}"))?;
        let (id, secret) = match (f.installed, f.client_id, f.client_secret) {
            (Some(i), _, _) => (i.client_id, i.client_secret),
            (None, Some(id), Some(secret)) => (id, secret),
            _ => return Err(t("não tem client_id e client_secret de um app para computador (um cliente \"web\" não serve).", "has no client_id and client_secret of a desktop app (a \"web\" client won't do).").into()),
        };
        if id.trim().is_empty() || secret.trim().is_empty() {
            return Err(t("client_id e client_secret não podem estar vazios.", "client_id and client_secret can't be empty.").into());
        }
        Ok(ClientConfig { client_id: id.trim().to_string(), client_secret: secret.trim().to_string() })
    }

    /// Saves the OAuth client credentials to [`client_file`] (restricted to owner on Unix).
    pub fn save(&self) -> Result<()> {
        use std::io::Write;
        let dir = config_dir();
        std::fs::create_dir_all(&dir).map_err(|e| tr!("Não consegui criar {}: {e}", "Couldn't create {}: {e}", dir.display()))?;
        let path = client_file();
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&path).map_err(|e| tr!("Não consegui salvar {}: {e}", "Couldn't save {}: {e}", path.display()))?;
        f.write_all(text.as_bytes()).map_err(|e| tr!("Não consegui salvar {}: {e}", "Couldn't save {}: {e}", path.display()))
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
        Err(e) => Err(tr!("Não consegui limpar {}: {e}", "Couldn't clear {}: {e}", path.display())),
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
    serde_json::from_str(&body).map_err(|e| tr!("{what}: resposta inesperada: {e}", "{what}: unexpected answer: {e}"))
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

    #[test]
    fn client_resolution_prefers_file_then_env_then_built_in() {
        let path = std::path::Path::new("google-client.json");
        let file_cfg = ClientConfig { client_id: "file-id".into(), client_secret: "file-sec".into() };
        let built_in = ClientConfig { client_id: "built-id".into(), client_secret: "built-sec".into() };

        // File beats env and built-in.
        let r = ClientConfig::resolve(
            Some(Ok(file_cfg.clone())),
            (Some("env-id".into()), Some("env-sec".into())),
            Some(built_in.clone()),
            path,
        );
        assert_eq!(r.unwrap(), file_cfg);

        // Runtime env beats built-in.
        let r = ClientConfig::resolve(
            None,
            (Some("env-id".into()), Some("env-sec".into())),
            Some(built_in.clone()),
            path,
        );
        assert_eq!(r.unwrap().client_id, "env-id");

        // Half-configured runtime env still reports an error instead of silently falling back.
        let r = ClientConfig::resolve(None, (Some("env-id".into()), None), Some(built_in.clone()), path);
        assert!(r.is_err());

        // Built-in is used when neither file nor runtime env is set.
        let r = ClientConfig::resolve(None, (None, None), Some(built_in.clone()), path);
        assert_eq!(r.unwrap(), built_in);

        // Missing everywhere returns an error pointing to the file.
        let r = ClientConfig::resolve(None, (None, None), None, path);
        assert!(r.unwrap_err().contains("google-client.json"));
    }
}
