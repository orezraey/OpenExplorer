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
    /// (revoked access, clock skew): renew it and try once more.
    fn get(&self, path: &str, query: &[(&str, &str)], what: &str) -> Result<serde_json::Value> {
        for retry in [false, true] {
            let mut req = self.agent.get(format!("{API}{path}")).header("Authorization", format!("Bearer {}", self.token()?));
            for (k, v) in query {
                req = req.query(*k, *v);
            }
            let resp = req.call().map_err(|e| format!("{what}: sem conexão com o Google: {e}"))?;
            if resp.status().as_u16() == 401 && !retry {
                *self.access.lock().unwrap() = None;
                continue;
            }
            return json(resp, what);
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

    /// Every file and folder directly in `folder_id` (`"root"` is My Drive), trash excluded.
    pub fn list(&self, folder_id: &str) -> Result<Vec<DriveFile>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Raw {
            id: String,
            name: String,
            mime_type: String,
            size: Option<String>,
            modified_time: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Page {
            #[serde(default)]
            files: Vec<Raw>,
            next_page_token: Option<String>,
        }
        // Drive ids are [A-Za-z0-9_-]; anything else would break out of the query string.
        if !folder_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(format!("Id de pasta inválido: {folder_id}"));
        }
        let q = format!("'{folder_id}' in parents and trashed = false");
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![
                ("q", q.as_str()),
                ("fields", "nextPageToken,files(id,name,mimeType,size,modifiedTime)"),
                ("pageSize", "1000"),
                ("orderBy", "folder,name_natural"),
            ];
            if let Some(t) = &token {
                query.push(("pageToken", t.as_str()));
            }
            let v = self.get("/files", &query, "Listagem da pasta")?;
            let page: Page = serde_json::from_value(v).map_err(|e| format!("Listagem da pasta: resposta inesperada: {e}"))?;
            out.extend(page.files.into_iter().map(|r| DriveFile {
                is_dir: r.mime_type == FOLDER_MIME,
                size: r.size.and_then(|s| s.parse().ok()),
                modified: r.modified_time.and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok()).map(SystemTime::from),
                id: r.id,
                name: r.name,
                mime: r.mime_type,
            }));
            match page.next_page_token {
                Some(t) => token = Some(t),
                None => return Ok(out),
            }
        }
    }
}
