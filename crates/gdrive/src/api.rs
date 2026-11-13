//! The Drive v3 calls: who the account is, and a folder's listing.
//! <https://developers.google.com/workspace/drive/api/reference/rest/v3>

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;

use crate::oauth::{self, Tokens};
use crate::{ClientConfig, Result, agent, json};

const API: &str = "https://www.googleapis.com/drive/v3";
pub const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

#[derive(Clone, Debug)]
pub struct Account {
    pub email: String,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct DriveFile {
    pub id: String,
    pub name: String,
    pub is_dir: bool,
    /// Google Docs, Sheets and Slides have none.
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub mime: String,
    /// For a shortcut to a folder: the folder it opens.
    pub target: Option<String>,
}

/// A shared drive (formerly Team Drive): files owned by a group rather than by a person.
#[derive(Clone, Debug, Deserialize)]
pub struct SharedDrive {
    pub id: String,
    pub name: String,
    /// Hidden by the user on the Drive website.
    #[serde(default)]
    pub hidden: bool,
}

/// One connected account. Cheap to share across threads; it renews its access token as needed.
pub struct Drive {
    cfg: ClientConfig,
    refresh_token: String,
    access: Mutex<Option<(String, Instant)>>,
    agent: ureq::Agent,
}

impl Drive {
    pub fn new(cfg: ClientConfig, refresh_token: String) -> Drive {
        Drive { cfg, refresh_token, access: Mutex::new(None), agent: agent() }
    }

    /// Starts from the tokens a login just returned, saving the first refresh.
    pub fn from_login(cfg: ClientConfig, tokens: &Tokens) -> Result<Drive> {
        let refresh = tokens.refresh_token.clone().ok_or("O Google não devolveu um token de renovação; tente conectar de novo.")?;
        let d = Drive::new(cfg, refresh);
        *d.access.lock().unwrap() = Some((tokens.access_token.clone(), Instant::now() + Duration::from_secs(tokens.expires_in)));
        Ok(d)
    }

    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }

    /// A valid access token, renewed a minute before it runs out.
    fn token(&self) -> Result<String> {
        let mut access = self.access.lock().unwrap();
        if let Some((t, until)) = access.as_ref()
            && Instant::now() + Duration::from_secs(60) < *until
        {
            return Ok(t.clone());
        }
        let fresh = oauth::refresh(&self.agent, &self.cfg, &self.refresh_token)?;
        *access = Some((fresh.access_token.clone(), Instant::now() + Duration::from_secs(fresh.expires_in)));
        Ok(fresh.access_token)
    }

    /// A GET with the access token. A 401 means Google dropped the token before its expiry
    /// (revoked access, clock skew): renew it and try once more. Each call's time goes to
    /// [`crate::log`].
    fn get(&self, path: &str, query: &[(&str, &str)], what: &str) -> Result<serde_json::Value> {
        for retry in [false, true] {
            let started = Instant::now();
            let mut req = self.agent.get(format!("{API}{path}")).header("Authorization", format!("Bearer {}", self.token()?));
            for (k, v) in query {
                req = req.query(*k, *v);
            }
            let resp = req.call().map_err(|e| {
                crate::log(&format!("GET {path} falhou em {} ms: {e}", started.elapsed().as_millis()));
                format!("{what}: sem conexão com o Google: {e}")
            })?;
            let status = resp.status().as_u16();
            if status == 401 && !retry {
                *self.access.lock().unwrap() = None;
                continue;
            }
            let out = json(resp, what);
            crate::log(&format!("GET {path} {status} em {} ms", started.elapsed().as_millis()));
            return out;
        }
        unreachable!("the second attempt always returns")
    }

    pub fn about(&self) -> Result<Account> {
        let v = self.get("/about", &[("fields", "user(emailAddress,displayName)")], "Dados da conta")?;
        let s = |p: &str| v.pointer(p).and_then(|x| x.as_str()).unwrap_or_default().to_string();
        let email = s("/user/emailAddress");
        if email.is_empty() {
            return Err("O Google não informou o e-mail da conta.".into());
        }
        Ok(Account { name: s("/user/displayName"), email })
    }

    /// A paged Drive listing (`files`, `drives`, ...), one page at a time: `each` gets a page's
    /// items and whether more pages follow, and returns `false` to stop early (the reader left
    /// the folder).
    fn pages<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
        items: &str,
        what: &str,
        mut each: impl FnMut(Vec<T>, bool) -> bool,
    ) -> Result<()> {
        let mut token: Option<String> = None;
        loop {
            let mut q = query.to_vec();
            if let Some(t) = &token {
                q.push(("pageToken", t.as_str()));
            }
            let mut v = self.get(path, &q, what)?;
            let page: Vec<T> = serde_json::from_value(v.get_mut(items).map(serde_json::Value::take).unwrap_or_default())
                .map_err(|e| format!("{what}: resposta inesperada: {e}"))?;
            token = v.get("nextPageToken").and_then(|t| t.as_str()).map(str::to_string);
            crate::log(&format!("  {} itens nesta página{}", page.len(), if token.is_some() { ", há mais" } else { ", última" }));
            if !each(page, token.is_some()) || token.is_none() {
                return Ok(());
            }
        }
    }

    /// The shared drives the account is a member of, including the ones hidden on the Drive
    /// website (marked in `hidden`), which would otherwise be out of reach here.
    pub fn shared_drives(&self) -> Result<Vec<SharedDrive>> {
        let mut out = Vec::new();
        self.pages("/drives", &[("pageSize", "100"), ("fields", "nextPageToken,drives(id,name,hidden)")], "drives", "Lista de drives compartilhados", |page, _| {
            out.extend(page);
            true
        })?;
        Ok(out)
    }

    /// Every file and folder directly in `folder_id`, trash excluded, handed to `each` a page at
    /// a time as Drive sends them (see [`Drive::pages`]); unsorted, since the app sorts anyway.
    /// `"root"` is My Drive; in a shared drive pass its id as `shared_drive` (its own id is also
    /// its top folder's).
    ///
    /// A shortcut to a folder lists as a folder whose [`DriveFile::target`] is the folder to
    /// open. That folder can sit in a shared drive, so without `shared_drive` the listing
    /// covers every drive the account reaches (`corpora=allDrives`); the parent filter keeps
    /// it as cheap as a My Drive listing.
    pub fn list(&self, folder_id: &str, shared_drive: Option<&str>, mut each: impl FnMut(Vec<DriveFile>, bool) -> bool) -> Result<()> {
        // Drive ids are [A-Za-z0-9_-]; anything else would break out of the query string.
        let valid = |id: &str| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid(folder_id) {
            return Err(format!("Id de pasta inválido: {folder_id}"));
        }
        if let Some(d) = shared_drive.filter(|d| !valid(d)) {
            return Err(format!("Id de drive compartilhado inválido: {d}"));
        }
        let q = format!("'{folder_id}' in parents and trashed = false");
        let mut query = vec![
            ("q", q.as_str()),
            ("fields", "nextPageToken,files(id,name,mimeType,size,modifiedTime,shortcutDetails(targetId,targetMimeType))"),
            // No orderBy: sorting a big folder on Google's side slows every page down, and the
            // app sorts locally.
            ("pageSize", "1000"),
            // Without these two, Drive answers nothing for a folder in a shared drive.
            ("supportsAllDrives", "true"),
            ("includeItemsFromAllDrives", "true"),
        ];
        match shared_drive {
            Some(d) => query.extend([("corpora", "drive"), ("driveId", d)]),
            None => query.push(("corpora", "allDrives")),
        }
        self.pages("/files", &query, "files", "Listagem da pasta", |raw: Vec<RawFile>, more| each(raw.into_iter().map(DriveFile::from).collect(), more))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Shortcut {
    target_id: String,
    target_mime_type: Option<String>,
}

/// A file as Drive's `files.list` sends it, with the fields [`Drive::list`] asks for.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawFile {
    id: String,
    name: String,
    mime_type: String,
    size: Option<String>,
    modified_time: Option<String>,
    shortcut_details: Option<Shortcut>,
}

impl From<RawFile> for DriveFile {
    fn from(r: RawFile) -> DriveFile {
        let target = r.shortcut_details.filter(|s| s.target_mime_type.as_deref() == Some(FOLDER_MIME)).map(|s| s.target_id);
        DriveFile {
            is_dir: r.mime_type == FOLDER_MIME || target.is_some(),
            target,
            size: r.size.and_then(|s| s.parse().ok()),
            modified: r.modified_time.as_deref().and_then(parse_time),
            id: r.id,
            name: r.name,
            mime: r.mime_type,
        }
    }
}

/// An RFC 3339 time from Drive, or `None` when it's malformed or out of the platform's range.
/// Drive keeps whatever a client sent, years 0001 and 9999 included, and Windows can't hold a
/// time before 1601: chrono's `SystemTime::from` panics there, which took down listings.
fn parse_time(t: &str) -> Option<SystemTime> {
    let dt = chrono::DateTime::parse_from_rfc3339(t).ok()?;
    let (secs, nanos) = (dt.timestamp(), dt.timestamp_subsec_nanos());
    if secs >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(Duration::new(secs.unsigned_abs(), nanos))
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(Duration::new(secs.unsigned_abs(), 0))?.checked_add(Duration::new(0, nanos))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_times_out_of_range_are_dropped_instead_of_panicking() {
        let t = parse_time("2024-03-01T12:00:00.250Z").unwrap();
        assert_eq!(t.duration_since(SystemTime::UNIX_EPOCH).unwrap(), Duration::new(1_709_294_400, 250_000_000));
        assert!(parse_time("1969-12-31T23:59:59Z").is_some());
        // Before 1601 Windows has no SystemTime; elsewhere it may, so only "no panic" is asserted.
        let _ = parse_time("0001-01-01T00:00:00Z");
        let _ = parse_time("1600-12-31T23:59:59Z");
        assert!(parse_time("not a date").is_none());
        #[cfg(windows)]
        assert!(parse_time("1600-12-31T23:59:59Z").is_none());
    }

    #[test]
    fn a_listed_file_from_year_one_maps_without_panicking() {
        let raw: RawFile = serde_json::from_str(
            r#"{"id":"a","name":"velho.txt","mimeType":"text/plain","size":"12","modifiedTime":"0001-01-01T00:00:00.000Z"}"#,
        )
        .unwrap();
        let f = DriveFile::from(raw);
        assert_eq!((f.name.as_str(), f.size, f.is_dir), ("velho.txt", Some(12), false));
        let shortcut: RawFile = serde_json::from_str(
            r#"{"id":"s","name":"Atalho","mimeType":"application/vnd.google-apps.shortcut","shortcutDetails":{"targetId":"t","targetMimeType":"application/vnd.google-apps.folder"}}"#,
        )
        .unwrap();
        let s = DriveFile::from(shortcut);
        assert!(s.is_dir);
        assert_eq!(s.target.as_deref(), Some("t"));
    }
}
