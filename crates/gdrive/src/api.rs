//! The Drive v3 calls: who the account is, and a folder's listing.
//! <https://developers.google.com/workspace/drive/api/reference/rest/v3>

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;

use crate::oauth::{self, Tokens};
use crate::{ClientConfig, Result, agent, json};

const API: &str = "https://www.googleapis.com/drive/v3";
pub const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
pub const SHORTCUT_MIME: &str = "application/vnd.google-apps.shortcut";
/// The fields of a file Drive answers with after a change or an upload.
pub(crate) const FILE_FIELDS: &str = "id,name,mimeType,size,modifiedTime,md5Checksum,starred,shortcutDetails(targetId,targetMimeType)";
/// What a listing asks of each file: [`FILE_FIELDS`] and the next page.
const LIST_FIELDS: &str = "nextPageToken,files(id,name,mimeType,size,modifiedTime,md5Checksum,starred,shortcutDetails(targetId,targetMimeType))";
/// What the trash listing asks of each file, to keep only what this account can restore.
const TRASH_FIELDS: &str =
    "nextPageToken,files(id,name,mimeType,size,modifiedTime,md5Checksum,starred,shortcutDetails(targetId,targetMimeType),explicitlyTrashed,ownedByMe,driveId,capabilities(canUntrash))";
/// How many files Recentes shows: Drive's own Recent view also stops after a while.
pub const RECENT_LIMIT: usize = 200;

/// Google Drive's lists that aren't folders: files gathered by a property rather than by
/// where they sit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    /// Compartilhados comigo: shared with this account by someone else.
    SharedWithMe,
    /// Com estrela.
    Starred,
    /// Recentes: the files this account opened last, newest first.
    Recent,
    /// Lixeira: what this account trashed in My Drive, and what it can restore in shared drives.
    Trash,
}

impl View {
    /// The `files.list` parameters of the view, besides paging and fields.
    pub(crate) fn query(self) -> Vec<(&'static str, &'static str)> {
        let mut q = vec![("supportsAllDrives", "true"), ("includeItemsFromAllDrives", "true")];
        match self {
            View::SharedWithMe => q.extend([("q", "sharedWithMe = true and trashed = false"), ("corpora", "user"), ("pageSize", "1000")]),
            // Starred files can sit in shared drives, which `corpora=user` leaves out.
            View::Starred => q.extend([("q", "starred = true and trashed = false"), ("corpora", "allDrives"), ("pageSize", "1000")]),
            // Folders have no "opened" time worth showing; Drive's Recent lists files only.
            View::Recent => q.extend([
                ("q", "trashed = false and mimeType != 'application/vnd.google-apps.folder'"),
                ("corpora", "user"),
                ("orderBy", "viewedByMeTime desc"),
                ("pageSize", "200"),
            ]),
            // `explicitlyTrashed` can't be asked for in `q`; `in_trash_view` filters on it.
            View::Trash => q.extend([("q", "trashed = true"), ("corpora", "allDrives"), ("pageSize", "1000")]),
        }
        q
    }

    fn what(self) -> &'static str {
        match self {
            View::SharedWithMe => "Compartilhados comigo",
            View::Starred => "Com estrela",
            View::Recent => "Recentes",
            View::Trash => "Lixeira",
        }
    }
}

/// Whether a trashed file shows in Lixeira, as on the Drive website: only what was trashed
/// itself (not the contents of a trashed folder, which open with it), and only what this
/// account can restore: its own files in My Drive, and in a shared drive what its role allows.
fn in_trash_view(r: &RawFile) -> bool {
    if !r.explicitly_trashed.unwrap_or(false) {
        return false;
    }
    match r.drive_id {
        Some(_) => r.capabilities.as_ref().is_some_and(|c| c.can_untrash.unwrap_or(false)),
        None => r.owned_by_me.unwrap_or(false),
    }
}

#[derive(Clone, Debug)]
pub struct Account {
    pub email: String,
    pub name: String,
}

/// What [`Drive::status`] says of a file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Status {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default)]
    pub trashed: bool,
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
    pub starred: bool,
    /// Drive's MD5 of the content, sent with the listing; Google's own formats have none.
    pub md5: Option<String>,
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

/// Where a file sits: the top it hangs from, and the folders from just under that top down to
/// the one holding it (empty when it sits at the top itself). A shared drive's own root sits
/// at its top with no folders.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    pub top: Top,
    pub folders: Vec<(String, String)>,
    /// The file's own name, and whether it's a folder.
    pub name: String,
    pub is_folder: bool,
}

/// What a file's folders hang from.
#[derive(Clone, Debug, PartialEq)]
pub enum Top {
    MyDrive,
    SharedDrive { id: String, name: String },
    /// Someone else's Drive: the account sees no folder above the first of `folders` (or above
    /// the file, which then sits in Compartilhados comigo).
    SharedWithMe,
}

/// One connected account. Cheap to share across threads; it renews its access token as needed.
pub struct Drive {
    cfg: ClientConfig,
    refresh_token: String,
    access: Mutex<Option<(String, Instant)>>,
    agent: ureq::Agent,
    /// For uploads, which run as long as the file takes: no overall time limit.
    pub(crate) uploads: ureq::Agent,
    /// Where uploads go: Google's upload endpoint (a local stand-in in tests).
    pub(crate) upload_url: String,
    /// For downloads: no overall time limit either, but a stalled read gives up.
    pub(crate) downloads: ureq::Agent,
    /// Where Drive calls and downloads go: Google's API (a local stand-in in tests).
    pub(crate) api_url: String,
    /// My Drive's top folder's id, asked for once.
    root: std::sync::OnceLock<String>,
}

impl Drive {
    pub fn new(cfg: ClientConfig, refresh_token: String) -> Drive {
        Drive {
            cfg,
            refresh_token,
            access: Mutex::new(None),
            agent: agent(),
            uploads: crate::upload_agent(),
            upload_url: crate::upload::UPLOAD.into(),
            downloads: crate::download_agent(),
            api_url: API.into(),
            root: std::sync::OnceLock::new(),
        }
    }

    /// My Drive's top folder's id ("root" is only its alias), asked for the first time. A
    /// change names a file's folders by their real ids, this one included.
    pub fn root_id(&self) -> Result<String> {
        if let Some(id) = self.root.get() {
            return Ok(id.clone());
        }
        let v = self.get("/files/root", &[("fields", "id")], "Meu Drive")?;
        let id = v.get("id").and_then(|v| v.as_str()).filter(|id| !id.is_empty()).ok_or("Meu Drive: o Google não informou a pasta raiz.")?;
        Ok(self.root.get_or_init(|| id.to_string()).clone())
    }

    /// Starts from the tokens a login just returned, saving the first refresh.
    pub fn from_login(cfg: ClientConfig, tokens: &Tokens) -> Result<Drive> {
        let refresh = tokens.refresh_token.clone().ok_or("O Google não devolveu um token de renovação; tente conectar de novo.")?;
        let d = Drive::new(cfg, refresh);
        *d.access.lock().unwrap() = Some((tokens.access_token.clone(), Instant::now() + Duration::from_secs(tokens.expires_in)));
        Ok(d)
    }

    /// A Drive whose uploads and downloads go to `upload_url`, with a token that needs no
    /// renewing.
    #[cfg(test)]
    pub(crate) fn for_tests(upload_url: String) -> Drive {
        let cfg = ClientConfig { client_id: "x".into(), client_secret: "x".into() };
        let d = Drive { api_url: upload_url.clone(), upload_url, ..Drive::new(cfg, "x".into()) };
        *d.access.lock().unwrap() = Some(("token".into(), Instant::now() + Duration::from_secs(3600)));
        d
    }

    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }

    /// A valid access token, renewed a minute before it runs out.
    pub(crate) fn token(&self) -> Result<String> {
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

    /// Drops the access token, for one Google stopped taking before its expiry.
    pub(crate) fn forget_token(&self) {
        *self.access.lock().unwrap() = None;
    }

    /// A call with the access token: GET when `body` is `None`, else `method` with a JSON body.
    /// A 401 means Google dropped the token before its expiry (revoked access, clock skew):
    /// renew it and try once more. A 429 or a 403 for Drive's rate limit means the request was
    /// turned away undone (making hundreds of folders hits it): wait and send it again, up to
    /// five times. DELETE takes no body and answers none (204): that gives `Null`. Each call's
    /// time goes to [`crate::log`].
    pub(crate) fn call(&self, method: &str, path: &str, query: &[(&str, &str)], body: Option<&serde_json::Value>, what: &str) -> Result<serde_json::Value> {
        let mut renewed = false;
        let mut limited = 0u32;
        loop {
            let started = Instant::now();
            let url = format!("{}{path}", self.api_url);
            let auth = format!("Bearer {}", self.token()?);
            let sent = match (method, body) {
                ("DELETE", _) => {
                    let mut req = self.agent.delete(&url).header("Authorization", &auth);
                    for (k, v) in query {
                        req = req.query(*k, *v);
                    }
                    req.call()
                }
                ("GET", _) | (_, None) => {
                    let mut req = self.agent.get(&url).header("Authorization", &auth);
                    for (k, v) in query {
                        req = req.query(*k, *v);
                    }
                    req.call()
                }
                ("POST", Some(b)) => {
                    let mut req = self.agent.post(&url).header("Authorization", &auth);
                    for (k, v) in query {
                        req = req.query(*k, *v);
                    }
                    req.send_json(b)
                }
                (_, Some(b)) => {
                    let mut req = self.agent.patch(&url).header("Authorization", &auth);
                    for (k, v) in query {
                        req = req.query(*k, *v);
                    }
                    req.send_json(b)
                }
            };
            let resp = sent.map_err(|e| {
                crate::log(&format!("{method} {path} falhou em {} ms: {e}", started.elapsed().as_millis()));
                format!("{what}: sem conexão com o Google: {e}")
            })?;
            let mut resp = resp;
            let status = resp.status().as_u16();
            if status == 401 && !renewed {
                renewed = true;
                *self.access.lock().unwrap() = None;
                continue;
            }
            if status == 429 || status == 403 {
                let text = resp.body_mut().read_to_string().unwrap_or_default();
                let rate = status == 429 || text.contains("rateLimitExceeded") || text.contains("userRateLimitExceeded");
                if rate && limited < 5 {
                    crate::log(&format!("{method} {path} {status}: limite de taxa do Drive, tentando de novo"));
                    std::thread::sleep(Duration::from_secs(1 << limited));
                    limited += 1;
                    continue;
                }
                crate::log(&format!("{method} {path} {status} em {} ms", started.elapsed().as_millis()));
                return Err(format!("{what}: HTTP {status}: {}", crate::google_reason(&text)));
            }
            // A delete answers 204 with no body at all.
            let out = if status == 204 || (method == "DELETE" && (200..300).contains(&status)) { Ok(serde_json::Value::Null) } else { json(resp, what) };
            crate::log(&format!("{method} {path} {status} em {} ms", started.elapsed().as_millis()));
            return out;
        }
    }

    pub(crate) fn get(&self, path: &str, query: &[(&str, &str)], what: &str) -> Result<serde_json::Value> {
        self.call("GET", path, query, None, what)
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
    pub(crate) fn pages<T: serde::de::DeserializeOwned>(
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
            ("fields", LIST_FIELDS),
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

    /// The folders in folder `folder_id` (and shortcuts to folders), for the tree: only those
    /// are asked for, so a folder of thousands of files costs no more than its folders.
    pub fn folders(&self, folder_id: &str, shared_drive: Option<&str>) -> Result<Vec<DriveFile>> {
        let q = format!("'{}' in parents and trashed = false and (mimeType = '{FOLDER_MIME}' or mimeType = '{SHORTCUT_MIME}')", checked(folder_id)?);
        let mut query = vec![("q", q.as_str()), ("fields", LIST_FIELDS), ("pageSize", "1000"), ("supportsAllDrives", "true"), ("includeItemsFromAllDrives", "true")];
        match shared_drive {
            Some(d) => query.extend([("corpora", "drive"), ("driveId", checked(d)?)]),
            None => query.push(("corpora", "allDrives")),
        }
        let mut out = Vec::new();
        self.pages("/files", &query, "files", "Pastas", |raw: Vec<RawFile>, _| {
            // A shortcut to a file isn't a folder.
            out.extend(raw.into_iter().map(DriveFile::from).filter(|f| f.is_dir));
            true
        })?;
        Ok(out)
    }

    /// The files of a [`View`], handed to `each` a page at a time like [`Drive::list`].
    /// Recentes stops after [`RECENT_LIMIT`] files.
    pub fn view(&self, view: View, mut each: impl FnMut(Vec<DriveFile>, bool) -> bool) -> Result<()> {
        let mut query = view.query();
        query.push(("fields", if view == View::Trash { TRASH_FIELDS } else { LIST_FIELDS }));
        let mut seen = 0;
        self.pages("/files", &query, "files", view.what(), |raw: Vec<RawFile>, more| {
            let files: Vec<DriveFile> = raw.into_iter().filter(|r| view != View::Trash || in_trash_view(r)).map(DriveFile::from).collect();
            if view != View::Recent {
                return each(files, more);
            }
            let room = RECENT_LIMIT - seen;
            let full = files.len() >= room;
            seen += files.len().min(room);
            // The last page shown says so, or the pane would wait for one that never comes.
            each(files.into_iter().take(room).collect(), more && !full) && !full
        })
    }

    /// The contents of a trashed folder, which went to the trash with it and come back when
    /// it's restored. No `trashed` filter: Drive marks them trashed only some time after the
    /// folder, and everything in a trashed folder is in the trash anyway.
    pub fn list_trashed(&self, folder_id: &str, mut each: impl FnMut(Vec<DriveFile>, bool) -> bool) -> Result<()> {
        let q = format!("'{}' in parents", checked(folder_id)?);
        let query = [
            ("q", q.as_str()),
            ("fields", LIST_FIELDS),
            ("pageSize", "1000"),
            ("supportsAllDrives", "true"),
            ("includeItemsFromAllDrives", "true"),
            ("corpora", "allDrives"),
        ];
        self.pages("/files", &query, "files", "Listagem da pasta na lixeira", |raw: Vec<RawFile>, more| each(raw.into_iter().map(DriveFile::from).collect(), more))
    }

    /// Takes a file or folder out of the trash, back where it was.
    pub fn restore(&self, id: &str) -> Result<DriveFile> {
        self.file_call("PATCH", &format!("/files/{}", checked(id)?), &[], &serde_json::json!({ "trashed": false }), "Restaurar")
    }

    /// Deletes a file or folder for good, skipping the trash: it can't be undone.
    pub fn delete_forever(&self, id: &str) -> Result<()> {
        self.call("DELETE", &format!("/files/{}", checked(id)?), &[("supportsAllDrives", "true")], None, "Excluir permanentemente").map(|_| ())
    }

    /// Deletes for good everything in this account's My Drive trash; shared drives keep theirs.
    pub fn empty_trash(&self) -> Result<()> {
        self.call("DELETE", "/files/trash", &[], None, "Esvaziar lixeira").map(|_| ())
    }

    /// Adds or removes the star.
    pub fn set_starred(&self, id: &str, starred: bool) -> Result<DriveFile> {
        let what = if starred { "Adicionar estrela" } else { "Remover estrela" };
        self.file_call("PATCH", &format!("/files/{}", checked(id)?), &[], &serde_json::json!({ "starred": starred }), what)
    }

    /// A new folder named `name` in folder `parent` (`"root"` is My Drive).
    pub fn create_folder(&self, name: &str, parent: &str) -> Result<DriveFile> {
        let body = serde_json::json!({ "name": name, "mimeType": FOLDER_MIME, "parents": [parent] });
        self.file_call("POST", "/files", &[], &body, "Criar pasta")
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<DriveFile> {
        self.file_call("PATCH", &format!("/files/{}", checked(id)?), &[], &serde_json::json!({ "name": name }), "Renomear")
    }

    /// Moves a file or folder to the Drive trash, from where it can be restored for 30 days.
    pub fn trash(&self, id: &str) -> Result<DriveFile> {
        self.file_call("PATCH", &format!("/files/{}", checked(id)?), &[], &serde_json::json!({ "trashed": true }), "Mover para a lixeira")
    }

    /// Moves `id` out of folder `from` into folder `to`.
    pub fn move_to(&self, id: &str, from: &str, to: &str) -> Result<DriveFile> {
        let path = format!("/files/{}", checked(id)?);
        self.file_call("PATCH", &path, &[("addParents", checked(to)?), ("removeParents", checked(from)?)], &serde_json::json!({}), "Mover")
    }

    /// Where file `id` stands now: its name, its folders and whether it's in the trash, which
    /// Desfazer checks before changing anything back.
    pub fn status(&self, id: &str) -> Result<Status> {
        let v = self.get(&format!("/files/{}", checked(id)?), &[("fields", "name,parents,trashed"), ("supportsAllDrives", "true")], "Desfazer")?;
        serde_json::from_value(v).map_err(|e| format!("Desfazer: resposta inesperada: {e}"))
    }

    /// Whether folder `id` holds anything (trashed items aside).
    pub fn has_children(&self, id: &str) -> Result<bool> {
        let mut any = false;
        self.list(id, None, |page, _| {
            any = !page.is_empty();
            false
        })?;
        Ok(any)
    }

    /// A copy of file or folder `src` in folder `to`, named `name`. Drive copies files only, so
    /// a folder is copied as a new folder with copies of everything in it.
    pub fn copy(&self, src: &DriveFile, to: &str, name: &str) -> Result<DriveFile> {
        if !src.is_dir {
            let body = serde_json::json!({ "name": name, "parents": [to] });
            return self.file_call("POST", &format!("/files/{}/copy", checked(&src.id)?), &[], &body, "Copiar");
        }
        let folder = self.create_folder(name, to)?;
        let mut children = Vec::new();
        self.list(&src.id, None, |page, _| {
            children.extend(page);
            true
        })?;
        for c in children {
            // Shortcuts are copied as shortcuts, not followed into their target.
            let c = DriveFile { is_dir: c.mime == FOLDER_MIME, ..c };
            self.copy(&c, &folder.id, &c.name.clone())?;
        }
        Ok(folder)
    }

    /// Where file `id` sits, for Mostrar na pasta and Drive links: climbs through its parents to
    /// the top of its shared drive or My Drive, or to the highest folder the account can see.
    pub fn place(&self, id: &str) -> Result<Place> {
        const WHAT: &str = "Achar a pasta";
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Up {
            #[serde(default)]
            name: String,
            #[serde(default)]
            mime_type: String,
            #[serde(default)]
            parents: Vec<String>,
            drive_id: Option<String>,
        }
        let up = |id: &str, fields: &str| -> Result<Up> {
            let v = self.get(&format!("/files/{}", checked(id)?), &[("fields", fields), ("supportsAllDrives", "true")], WHAT)?;
            serde_json::from_value(v).map_err(|e| format!("{WHAT}: resposta inesperada: {e}"))
        };
        let drive_name = |drive: &str| -> Result<String> {
            let d = self.get(&format!("/drives/{}", checked(drive)?), &[("fields", "name")], WHAT)?;
            Ok(d.get("name").and_then(|n| n.as_str()).unwrap_or_default().to_string())
        };
        let file = up(id, "name,mimeType,parents,driveId")?;
        let (name, is_folder) = (file.name, file.mime_type == FOLDER_MIME);
        // A shared drive's root is a folder with the drive's id.
        if file.drive_id.as_deref() == Some(id) {
            let top = Top::SharedDrive { id: id.to_string(), name: drive_name(id)? };
            return Ok(Place { top, folders: Vec::new(), name, is_folder });
        }
        let root = match &file.drive_id {
            Some(_) => None,
            None => Some(self.root_id()?),
        };
        let mut folders = Vec::new();
        let mut next = file.parents.into_iter().next();
        let top = loop {
            let Some(at) = next else { break Top::SharedWithMe };
            if file.drive_id.as_deref() == Some(at.as_str()) {
                let name = drive_name(&at)?;
                break Top::SharedDrive { id: at, name };
            }
            if root.as_deref() == Some(at.as_str()) {
                break Top::MyDrive;
            }
            // Deeper than Drive nests folders: going round in circles.
            if folders.len() >= 100 {
                return Err(format!("{WHAT}: a pasta de {id} não tem fim."));
            }
            let folder = up(&at, "name,parents")?;
            folders.push((at, folder.name));
            next = folder.parents.into_iter().next();
        };
        folders.reverse();
        Ok(Place { top, folders, name, is_folder })
    }

    /// The item named `name` in folder `parent` (in shared drive `shared_drive`, when it's in
    /// one; `"root"` is My Drive), a folder first when a file has the same name. Drive lets
    /// names repeat: the first one Drive lists wins.
    pub fn child_named(&self, parent: &str, shared_drive: Option<&str>, name: &str) -> Result<Option<DriveFile>> {
        let q = format!("'{}' in parents and name = {} and trashed = false", checked(parent)?, quoted(name));
        let mut query = vec![("q", q.as_str()), ("fields", LIST_FIELDS), ("pageSize", "100"), ("supportsAllDrives", "true"), ("includeItemsFromAllDrives", "true")];
        match shared_drive {
            Some(d) => query.extend([("corpora", "drive"), ("driveId", checked(d)?)]),
            None => query.push(("corpora", "allDrives")),
        }
        self.first_named(&query)
    }

    /// The item named `name` in Compartilhados comigo or Com estrela, a folder first; `None`
    /// there or in a view that lists no such thing (Recentes has no folders; Lixeira opens only
    /// as a list).
    pub fn view_child_named(&self, view: View, name: &str) -> Result<Option<DriveFile>> {
        let (filter, corpus) = match view {
            View::SharedWithMe => ("sharedWithMe = true", "user"),
            View::Starred => ("starred = true", "allDrives"),
            View::Recent | View::Trash => return Ok(None),
        };
        let q = format!("{filter} and name = {} and trashed = false", quoted(name));
        let query = [("q", q.as_str()), ("fields", LIST_FIELDS), ("pageSize", "100"), ("supportsAllDrives", "true"), ("includeItemsFromAllDrives", "true"), ("corpora", corpus)];
        self.first_named(&query)
    }

    /// The first folder `query` finds, else its first file.
    fn first_named(&self, query: &[(&str, &str)]) -> Result<Option<DriveFile>> {
        let mut v = self.get("/files", query, "Achar pelo nome")?;
        let found: Vec<RawFile> = serde_json::from_value(v.get_mut("files").map(serde_json::Value::take).unwrap_or_default()).map_err(|e| format!("Achar pelo nome: resposta inesperada: {e}"))?;
        let mut found: Vec<DriveFile> = found.into_iter().map(DriveFile::from).collect();
        let at = found.iter().position(|f| f.is_dir).unwrap_or(0);
        Ok((!found.is_empty()).then(|| found.swap_remove(at)))
    }

    /// One file's details, for Propriedades.
    pub fn details(&self, id: &str) -> Result<Details> {
        let fields = "id,description,mimeType,md5Checksum,createdTime,viewedByMeTime,webViewLink,shared,starred,                      owners(displayName,emailAddress),lastModifyingUser(displayName,emailAddress),sharingUser(displayName)";
        let v = self.get(&format!("/files/{}", checked(id)?), &[("fields", fields), ("supportsAllDrives", "true")], "Propriedades")?;
        let s = |p: &str| v.pointer(p).and_then(|x| x.as_str()).filter(|x| !x.is_empty()).map(str::to_string);
        let person = |p: &str| s(&format!("{p}/displayName")).or_else(|| s(&format!("{p}/emailAddress")));
        Ok(Details {
            created: s("/createdTime").as_deref().and_then(parse_time),
            viewed: s("/viewedByMeTime").as_deref().and_then(parse_time),
            link: s("/webViewLink"),
            owner: person("/owners/0"),
            modified_by: person("/lastModifyingUser"),
            shared_by: person("/sharingUser"),
            description: s("/description"),
            md5: s("/md5Checksum"),
            mime: s("/mimeType"),
            shared: v.get("shared").and_then(|x| x.as_bool()).unwrap_or(false),
            starred: v.get("starred").and_then(|x| x.as_bool()).unwrap_or(false),
        })
    }

    /// The stored versions of a file, oldest first. Google Docs, Sheets and Slides list their
    /// revisions without sizes; folders have none.
    pub fn revisions(&self, id: &str) -> Result<Vec<Revision>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Raw {
            id: String,
            modified_time: Option<String>,
            size: Option<String>,
            keep_forever: Option<bool>,
            original_filename: Option<String>,
            last_modifying_user: Option<serde_json::Value>,
        }
        let path = format!("/files/{}/revisions", checked(id)?);
        let fields = "nextPageToken,revisions(id,modifiedTime,size,keepForever,originalFilename,lastModifyingUser(displayName,emailAddress))";
        let mut out = Vec::new();
        self.pages(&path, &[("pageSize", "200"), ("fields", fields)], "revisions", "Histórico de versões", |page: Vec<Raw>, _| {
            out.extend(page.into_iter().map(|r| Revision {
                modified: r.modified_time.as_deref().and_then(parse_time),
                size: r.size.and_then(|s| s.parse().ok()),
                keep_forever: r.keep_forever.unwrap_or(false),
                name: r.original_filename,
                by: r.last_modifying_user.and_then(|u| {
                    u.get("displayName").or_else(|| u.get("emailAddress")).and_then(|x| x.as_str()).map(str::to_string)
                }),
                id: r.id,
            }));
            true
        })?;
        Ok(out)
    }

    /// A call that answers with one file record.
    fn file_call(&self, method: &str, path: &str, query: &[(&str, &str)], body: &serde_json::Value, what: &str) -> Result<DriveFile> {
        let mut q = vec![("supportsAllDrives", "true"), ("fields", FILE_FIELDS)];
        q.extend_from_slice(query);
        let v = self.call(method, path, &q, Some(body), what)?;
        let raw: RawFile = serde_json::from_value(v).map_err(|e| format!("{what}: resposta inesperada: {e}"))?;
        Ok(raw.into())
    }
}

/// What Propriedades shows of a Drive file beyond its listing.
#[derive(Clone, Debug, Default)]
pub struct Details {
    pub created: Option<SystemTime>,
    /// When this account last opened it.
    pub viewed: Option<SystemTime>,
    pub link: Option<String>,
    pub owner: Option<String>,
    pub modified_by: Option<String>,
    pub shared_by: Option<String>,
    pub description: Option<String>,
    /// Drive's checksum of the content; Google Docs and folders have none.
    pub md5: Option<String>,
    pub mime: Option<String>,
    pub shared: bool,
    pub starred: bool,
}

/// One stored version of a Drive file.
#[derive(Clone, Debug)]
pub struct Revision {
    pub id: String,
    pub modified: Option<SystemTime>,
    pub size: Option<u64>,
    pub by: Option<String>,
    /// Kept for good rather than dropped after 30 days or 100 versions.
    pub keep_forever: bool,
    pub name: Option<String>,
}

/// `text` as a quoted string in a `files.list` query, its quotes and backslashes escaped so
/// it can't change the query.
pub(crate) fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Whether `id` can be a Drive id: `[A-Za-z0-9_-]` only, so it can't reach into a request's
/// path or query.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A Drive id, refused unless [`valid_id`].
pub(crate) fn checked(id: &str) -> Result<&str> {
    if valid_id(id) { Ok(id) } else { Err(format!("Id inválido: {id}")) }
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
pub(crate) struct RawFile {
    pub(crate) id: String,
    name: String,
    mime_type: String,
    size: Option<String>,
    pub(crate) modified_time: Option<String>,
    md5_checksum: Option<String>,
    shortcut_details: Option<Shortcut>,
    starred: Option<bool>,
    /// The trash listing's fields, see `in_trash_view`.
    explicitly_trashed: Option<bool>,
    owned_by_me: Option<bool>,
    drive_id: Option<String>,
    capabilities: Option<Capabilities>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Capabilities {
    can_untrash: Option<bool>,
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
            md5: r.md5_checksum,
            starred: r.starred.unwrap_or(false),
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
pub(crate) mod tests {
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

    fn param<'a>(q: &'a [(&str, &str)], k: &str) -> Option<&'a str> {
        q.iter().find(|(n, _)| *n == k).map(|(_, v)| *v)
    }

    #[test]
    fn each_view_asks_drive_for_its_own_files() {
        let q = View::SharedWithMe.query();
        assert_eq!(param(&q, "q"), Some("sharedWithMe = true and trashed = false"));
        assert_eq!(param(&q, "corpora"), Some("user"));
        let q = View::Starred.query();
        assert_eq!(param(&q, "q"), Some("starred = true and trashed = false"));
        assert_eq!(param(&q, "corpora"), Some("allDrives"));
        let q = View::Recent.query();
        assert_eq!(param(&q, "orderBy"), Some("viewedByMeTime desc"));
        assert_eq!(param(&q, "q"), Some("trashed = false and mimeType != 'application/vnd.google-apps.folder'"));
        let q = View::Trash.query();
        assert_eq!(param(&q, "q"), Some("trashed = true"));
        for v in [View::SharedWithMe, View::Starred, View::Recent, View::Trash] {
            let q = v.query();
            assert_eq!((param(&q, "supportsAllDrives"), param(&q, "includeItemsFromAllDrives")), (Some("true"), Some("true")), "{v:?}");
            assert!(param(&q, "pageSize").is_some(), "{v:?}");
        }
    }

    #[test]
    fn the_trash_shows_what_was_trashed_itself_and_can_come_back() {
        let raw = |json: &str| serde_json::from_str::<RawFile>(&format!(r#"{{"id":"a","name":"n","mimeType":"text/plain",{json}}}"#)).unwrap();
        // Trashed by its owner, this account.
        assert!(in_trash_view(&raw(r#""explicitlyTrashed":true,"ownedByMe":true"#)));
        // Inside a trashed folder: it shows inside that folder, not on its own.
        assert!(!in_trash_view(&raw(r#""explicitlyTrashed":false,"ownedByMe":true"#)));
        // Someone else's file they trashed, still shared with this account.
        assert!(!in_trash_view(&raw(r#""explicitlyTrashed":true,"ownedByMe":false"#)));
        // In a shared drive: as the account's role allows.
        assert!(in_trash_view(&raw(r#""explicitlyTrashed":true,"driveId":"0A","capabilities":{"canUntrash":true}"#)));
        assert!(!in_trash_view(&raw(r#""explicitlyTrashed":true,"driveId":"0A","capabilities":{"canUntrash":false}"#)));
        assert!(!in_trash_view(&raw(r#""explicitlyTrashed":true,"driveId":"0A""#)));
        let starred = DriveFile::from(raw(r#""starred":true"#));
        assert!(starred.starred);
        assert!(!DriveFile::from(raw(r#""size":"1""#)).starred);
    }

    /// A stand-in for the Drive API that answers each request in turn with the next of
    /// `answers` (status line, body), and hands back the first line of every request it got.
    pub(crate) fn stand_in(answers: Vec<(&'static str, &'static str)>) -> (Drive, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.local_addr().unwrap());
        let seen = std::thread::spawn(move || {
            let mut lines = Vec::new();
            for (status, body) in answers {
                let mut s = BufReader::new(server.accept().unwrap().0);
                let mut line = String::new();
                s.read_line(&mut line).unwrap();
                let mut len = 0;
                loop {
                    let mut h = String::new();
                    s.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut sent = vec![0; len];
                s.read_exact(&mut sent).unwrap();
                // The body as compact JSON, however it was sent.
                let got = serde_json::from_slice::<serde_json::Value>(&sent).map(|v| v.to_string()).unwrap_or_default();
                lines.push(format!("{} {got}", line.trim_end()).trim_end().to_string());
                let out = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                s.get_mut().write_all(out.as_bytes()).unwrap();
            }
            lines
        });
        (Drive { api_url: base, ..Drive::for_tests(String::new()) }, seen)
    }

    #[test]
    fn trash_actions_send_the_calls_drive_documents() {
        let file = r#"{"id":"f1","name":"a.txt","mimeType":"text/plain"}"#;
        let (d, seen) = stand_in(vec![("200 OK", file), ("204 No Content", ""), ("204 No Content", ""), ("200 OK", file), ("403 Forbidden", r#"{"error":{"code":403,"message":"The user does not have sufficient permissions for this file."}}"#)]);
        d.restore("f1").unwrap();
        d.delete_forever("f1").unwrap();
        d.empty_trash().unwrap();
        d.set_starred("f1", true).unwrap();
        let err = d.delete_forever("f2").unwrap_err();
        assert!(err.contains("sufficient permissions"), "{err}");
        let seen = seen.join().unwrap();
        assert!(seen[0].starts_with("PATCH /files/f1?supportsAllDrives=true") && seen[0].ends_with(r#"{"trashed":false}"#), "{}", seen[0]);
        assert!(seen[1].starts_with("DELETE /files/f1?supportsAllDrives=true "), "{}", seen[1]);
        // Esvaziar lixeira is the one call on the whole trash, with nothing that could aim it
        // at anything else.
        assert_eq!(seen[2], "DELETE /files/trash HTTP/1.1");
        assert!(seen[3].ends_with(r#"{"starred":true}"#), "{}", seen[3]);
        assert!(d.delete_forever("../trash").is_err(), "ids can't reach other paths");
    }

    #[test]
    fn the_trash_view_keeps_only_what_can_be_restored_and_recent_stops_at_its_limit() {
        let page = r#"{"files":[
            {"id":"mine","name":"meu.txt","mimeType":"text/plain","explicitlyTrashed":true,"ownedByMe":true},
            {"id":"inside","name":"dentro.txt","mimeType":"text/plain","explicitlyTrashed":false,"ownedByMe":true},
            {"id":"theirs","name":"deles.txt","mimeType":"text/plain","explicitlyTrashed":true,"ownedByMe":false}]}"#;
        let (d, seen) = stand_in(vec![("200 OK", page)]);
        let mut got = Vec::new();
        d.view(View::Trash, |files, more| {
            got.extend(files.into_iter().map(|f| f.id));
            assert!(!more);
            true
        })
        .unwrap();
        assert_eq!(got, ["mine"]);
        let line = &seen.join().unwrap()[0];
        assert!(line.contains("q=trashed+%3D+true") || line.contains("q=trashed%20%3D%20true"), "{line}");

        let many: String = (0..150).map(|i| format!(r#"{{"id":"r{i}","name":"r{i}","mimeType":"text/plain"}}"#)).collect::<Vec<_>>().join(",");
        let first: &'static str = format!(r#"{{"nextPageToken":"p2","files":[{many}]}}"#).leak();
        let (d, seen) = stand_in(vec![("200 OK", first), ("200 OK", first)]);
        let mut pages = Vec::new();
        d.view(View::Recent, |files, more| {
            pages.push((files.len(), more));
            true
        })
        .unwrap();
        assert_eq!(pages, [(150, true), (RECENT_LIMIT - 150, false)]);
        assert_eq!(seen.join().unwrap().len(), 2);
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
