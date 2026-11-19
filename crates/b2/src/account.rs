//! One application key: the buckets it reaches, their folders, and what the panes do to them.
//!
//! One `Account` per key ID is open at a time, shared by every pane and transfer (see
//! [`reach`]); it holds the key's authorization and its listings ([`crate::cache`]).
//!
//! Paths are `/bucket/folder/file`: `/` lists the buckets, `/bucket` a bucket's top.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use openexplorer_i18n::{t, tr};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::api::{Api, Fail};
use crate::cache::{self, Listing, Listings};
use crate::upload::Uploads;
use crate::watch::{self, Polled};
use crate::{Result, store};

/// What B2's web interface puts in a folder made empty, and hides; so does the app.
pub const EMPTY_MARKER: &str = ".bzEmpty";
/// The most names one listing call asks for. B2 bills listings by the thousand names returned
/// (a call returning 3,123 counts as 4), so a high count only saves round trips.
pub const PAGE: u32 = 10_000;
/// `b2_copy_file` copies files under 5 GB; bigger ones are copied in parts.
pub const COPY_WHOLE_BELOW: u64 = 5_000_000_000;
/// How many calls a batch over many files (deleting a folder, copying one) has out at once.
pub const AT_ONCE: usize = 8;
/// Folders read ahead at most after one shows (see [`Account::prefetch`]).
pub const AHEAD: usize = 24;

/// An item of a listing.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    /// Milliseconds since 1970: the file's own time (`src_last_modified_millis`) when the upload
    /// said it, else when it reached B2.
    pub modified: Option<i64>,
    /// A bucket's id.
    pub id: Option<String>,
    /// A bucket's lifecycle rules, as JSON (see [`Rule`]); `None` when the key can't read them.
    pub info: Option<String>,
}

impl Item {
    fn dir(name: &str) -> Item {
        Item { name: name.to_string(), is_dir: true, size: None, modified: None, id: None, info: None }
    }
}

/// A lifecycle rule of a bucket: files under `prefix` are hidden `upload_to_hide` days after
/// their upload, and hidden versions deleted `hide_to_delete` days after they were hidden.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub prefix: String,
    pub hide_to_delete: Option<u64>,
    pub upload_to_hide: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Bucket {
    pub id: String,
    pub name: String,
    /// `None` when the key may not read them (no `listBuckets`).
    pub rules: Option<Vec<Rule>>,
}

impl Bucket {
    fn item(&self) -> Item {
        Item { name: self.name.clone(), is_dir: true, size: None, modified: None, id: Some(self.id.clone()), info: self.rules.as_ref().and_then(|r| serde_json::to_string(r).ok()) }
    }

    fn of_item(i: &Item) -> Option<Bucket> {
        Some(Bucket { id: i.id.clone()?, name: i.name.clone(), rules: i.info.as_deref().and_then(|j| serde_json::from_str(j).ok()) })
    }

    fn of_json(v: &Value) -> Option<Bucket> {
        let rules = v.get("lifecycleRules").and_then(Value::as_array).map(|rules| {
            rules
                .iter()
                .map(|r| Rule {
                    prefix: r.get("fileNamePrefix").and_then(Value::as_str).unwrap_or("").to_string(),
                    hide_to_delete: r.get("daysFromHidingToDeleting").and_then(Value::as_u64),
                    upload_to_hide: r.get("daysFromUploadingToHiding").and_then(Value::as_u64),
                })
                .collect()
        });
        Some(Bucket { id: v.get("bucketId")?.as_str()?.to_string(), name: v.get("bucketName")?.as_str()?.to_string(), rules })
    }

    /// How long a hidden version of `name` stays before the lifecycle rules delete it: `Some(None)`
    /// when no rule does (B2's default, "keep all versions"), `None` when the rules are unknown.
    pub fn hidden_kept(&self, name: &str) -> Option<Option<u64>> {
        let rules = self.rules.as_ref()?;
        Some(rules.iter().filter(|r| name.starts_with(&r.prefix)).filter_map(|r| r.hide_to_delete).min())
    }
}

/// One version of a file, as B2's listings give it.
#[derive(Clone, Debug, PartialEq)]
pub struct Version {
    pub id: String,
    pub name: String,
    /// `upload`, `hide`, `start` (an unfinished large file) or `folder`.
    pub action: String,
    pub size: u64,
    pub sha1: Option<String>,
    pub content_type: Option<String>,
    pub info: Map<String, Value>,
    /// Milliseconds since 1970.
    pub uploaded: i64,
}

impl Version {
    pub(crate) fn of(v: &Value) -> Option<Version> {
        Some(Version {
            id: v.get("fileId").and_then(Value::as_str).unwrap_or("").to_string(),
            name: v.get("fileName")?.as_str()?.to_string(),
            action: v.get("action").and_then(Value::as_str).unwrap_or("upload").to_string(),
            size: v.get("contentLength").and_then(Value::as_u64).unwrap_or(0),
            sha1: v.get("contentSha1").and_then(Value::as_str).filter(|s| *s != "none").map(str::to_string),
            content_type: v.get("contentType").and_then(Value::as_str).map(str::to_string),
            info: v.get("fileInfo").and_then(Value::as_object).cloned().unwrap_or_default(),
            uploaded: v.get("uploadTimestamp").and_then(Value::as_i64).unwrap_or(0),
        })
    }

    /// The file's own time when the upload said it, else when it reached B2.
    pub fn modified(&self) -> i64 {
        self.info.get("src_last_modified_millis").and_then(Value::as_str).and_then(|s| s.parse().ok()).unwrap_or(self.uploaded)
    }

    fn item(&self, name: &str) -> Item {
        Item { name: name.to_string(), is_dir: false, size: Some(self.size), modified: Some(self.modified()).filter(|m| *m > 0), id: None, info: None }
    }
}

/// Where a path is: a bucket and the name in it, without slashes at either end ("" at the
/// bucket's top). `None` is the top, the list of buckets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spot {
    pub bucket: String,
    pub key: String,
}

impl Spot {
    pub fn of(path: &str) -> Option<Spot> {
        let mut segs = path.split('/').filter(|s| !s.is_empty());
        let bucket = segs.next()?.to_string();
        Some(Spot { bucket, key: segs.collect::<Vec<_>>().join("/") })
    }

    /// The prefix of what's in it as a folder: "" at the bucket's top, else `key/`.
    pub fn prefix(&self) -> String {
        if self.key.is_empty() { String::new() } else { format!("{}/", self.key) }
    }

    /// The folder it's in, and its name.
    pub fn parent(&self) -> (Spot, &str) {
        match self.key.rsplit_once('/') {
            Some((dir, name)) => (Spot { bucket: self.bucket.clone(), key: dir.to_string() }, name),
            None => (Spot { bucket: self.bucket.clone(), key: String::new() }, &self.key),
        }
    }
}

/// How a folder stands with a key limited to names under `namePrefix`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Within {
    /// All of it is the key's.
    Inside,
    /// Only the folder `name` in it leads to the key's names.
    Toward(String),
    /// Only its names that start with the key's prefix are the key's.
    Partial(String),
    /// None of it.
    Outside,
}

pub(crate) fn within(name_prefix: Option<&str>, prefix: &str) -> Within {
    let Some(np) = name_prefix else { return Within::Inside };
    if prefix.starts_with(np) {
        return Within::Inside;
    }
    match np.strip_prefix(prefix) {
        Some(rest) => match rest.split_once('/') {
            Some((seg, _)) if !seg.is_empty() => Within::Toward(seg.to_string()),
            _ => Within::Partial(np.to_string()),
        },
        None => Within::Outside,
    }
}

/// Names B2 refuses, or the panes couldn't tell apart.
pub fn check_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name == "." || name == ".." {
        return Err(t("Esse nome não pode ser usado.", "That name can't be used.").into());
    }
    if name.contains('/') {
        return Err(t("O nome não pode ter /.", "The name can't have /.").into());
    }
    if name.chars().any(|c| (c as u32) < 32 || c == '\u{7f}') {
        return Err(t("O nome não pode ter caracteres de controle.", "The name can't have control characters.").into());
    }
    if name == EMPTY_MARKER {
        return Err(tr!("“{EMPTY_MARKER}” é o nome que o B2 usa para pastas vazias.", "“{EMPTY_MARKER}” is the name B2 uses for empty folders."));
    }
    Ok(())
}

/// Runs `work` on each of `items`, [`AT_ONCE`] at a time; stops starting new ones after the
/// first error, which is what comes back (with how many were done before it).
pub(crate) fn each<T: Sync, R: Send>(items: &[T], work: impl Fn(&T) -> Result<R> + Sync) -> Result<Vec<R>> {
    match each_done(items, work) {
        (done, None) => Ok(done),
        (done, Some(e)) => Err(with_done(e, done.len())),
    }
}

/// An error that stopped a batch, with how many were done before it.
pub(crate) fn with_done(e: String, done: usize) -> String {
    if done == 0 { e } else { tr!("{e} (feitos antes do erro: {done})", "{e} (done before the error: {done})") }
}

/// Like [`each`], but what was done before the error comes back with it (in the items'
/// order): a batch that changes B2 must know what it changed even when it stopped midway.
pub(crate) fn each_done<T: Sync, R: Send>(items: &[T], work: impl Fn(&T) -> Result<R> + Sync) -> (Vec<R>, Option<String>) {
    let mut done = Vec::with_capacity(items.len());
    let mut error = None;
    for r in openexplorer_cloud::each_until(items, AT_ONCE, work, Result::is_err).into_iter().flatten() {
        match r {
            Ok(v) => done.push(v),
            Err(e) => {
                error.get_or_insert(e);
            }
        }
    }
    (done, error)
}

/// A delete ([`Account::hide`]) that stopped midway: `since` is the time of the earliest hide
/// marker it made before stopping, from which [`Account::unhide`] brings back what it hid
/// (`None`: it hid nothing), and `error` why it stopped.
#[derive(Clone, Debug, PartialEq)]
pub struct HideStopped {
    pub since: Option<i64>,
    pub error: String,
}

impl HideStopped {
    fn before_any(error: String) -> HideStopped {
        HideStopped { since: None, error }
    }
}

impl From<HideStopped> for String {
    fn from(h: HideStopped) -> String {
        h.error
    }
}

pub struct Account {
    pub key_id: String,
    pub(crate) api: Api,
    pub(crate) lists: Arc<Listings>,
    /// The buckets last seen, from B2 or the kept listing of the top.
    buckets: Mutex<Vec<Bucket>>,
    /// How many parts of a large upload go at once ([`crate::upload::PARTS_AT_ONCE`]; the
    /// benchmark compares).
    pub(crate) parallel: usize,
    /// Counts the app's own changes, so a listing read across one isn't kept.
    changes: AtomicU64,
    /// What the app's own readings may still ask (see [`crate::watch`]).
    budget: Mutex<watch::Budget>,
    /// Each bucket's sums of sizes (see [`crate::sizes`]), and one summing at a time.
    pub(crate) sizes: Mutex<HashMap<String, crate::sizes::Kept>>,
    pub(crate) sizing: Mutex<()>,
}

/// What B2 has at a name: a file, or a folder (names under it).
#[derive(Clone, Debug, PartialEq)]
pub enum Found {
    File(Version),
    Dir,
}

/// A folder read again, against what was kept of it.
#[derive(Clone, Debug, PartialEq)]
pub enum Fresh {
    /// As kept: nothing on screen needs to change.
    Same,
    Changed(Vec<Item>),
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account").field("key_id", &self.key_id).finish_non_exhaustive()
    }
}

fn not_there(name: &str) -> String {
    tr!("“{name}” não está mais no B2.", "“{name}” is no longer on B2.")
}

fn last(path: &str) -> String {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string()
}

impl Account {
    /// An account over `api`, its listings kept in `file` (read here) or in memory only. Asks
    /// B2 nothing: the key is authorized on its first call.
    pub fn new(api: Api, file: Option<PathBuf>) -> Account {
        let secret = cache::Secret::of(api.key_id(), api.key());
        let lists = Listings::open(file.map(|f| (f, secret)));
        let buckets = lists.get("", "").map(|l| l.items.iter().filter_map(Bucket::of_item).collect()).unwrap_or_default();
        Account { key_id: api.key_id().to_string(), api, lists, buckets: Mutex::new(buckets), parallel: crate::upload::PARTS_AT_ONCE, changes: AtomicU64::new(0), budget: Mutex::default(), sizes: Mutex::default(), sizing: Mutex::new(()) }
    }

    pub fn api(&self) -> &Api {
        &self.api
    }

    /// What the tree shows the account as: "B2 · bucket" for a key of one bucket, else the
    /// key ID's start. Authorizes the key if it isn't yet.
    pub fn label(&self) -> Result<String> {
        let auth = self.api.auth()?;
        Ok(match auth.allowed.buckets.as_deref() {
            Some([(_, Some(name))]) => format!("B2 · {name}"),
            _ => format!("B2 · {}", self.key_id.chars().take(12).collect::<String>()),
        })
    }

    // -----------------------------------------------------------------------------------------
    // Buckets

    /// The buckets the key reaches, asked of B2 now.
    pub fn buckets_fresh(&self) -> Result<Vec<Bucket>> {
        let auth = self.api.auth()?;
        let mut list = match &auth.allowed.buckets {
            // A key of some buckets lists only those, and may be refused a listing of all.
            Some(allowed) => {
                let mut out = Vec::new();
                for (id, name) in allowed {
                    let Some(name) = name else { continue };
                    let read = if auth.allowed.can("listBuckets") {
                        self.api.call("b2_list_buckets", &json!({"accountId": auth.account_id, "bucketId": id})).ok().and_then(|v| v.get("buckets").and_then(Value::as_array).and_then(|b| b.first()).and_then(Bucket::of_json))
                    } else {
                        None
                    };
                    out.push(read.unwrap_or(Bucket { id: id.clone(), name: name.clone(), rules: None }));
                }
                out
            }
            None => {
                let v = self.api.run("b2_list_buckets", &json!({"accountId": auth.account_id, "bucketTypes": ["all"]}))?;
                v.get("buckets").and_then(Value::as_array).into_iter().flatten().filter_map(Bucket::of_json).collect()
            }
        };
        list.sort_by(|a, b| a.name.cmp(&b.name));
        self.lists.put("", "", list.iter().map(Bucket::item).collect(), cache::now());
        *self.buckets.lock().unwrap_or_else(|e| e.into_inner()) = list.clone();
        Ok(list)
    }

    /// The bucket named `name`, as last seen; asked of B2 when it hasn't been.
    pub fn bucket(&self, name: &str) -> Result<Bucket> {
        let known = |list: &[Bucket]| list.iter().find(|b| b.name == name).cloned();
        if let Some(b) = known(&self.buckets.lock().unwrap_or_else(|e| e.into_inner())) {
            return Ok(b);
        }
        known(&self.buckets_fresh()?).ok_or_else(|| tr!("O bucket “{name}” não existe ou esta chave não o alcança.", "The bucket “{name}” doesn't exist or this key doesn't reach it."))
    }

    /// The buckets as last seen, without asking B2 (the address bar picks an account by them).
    pub fn known_buckets(&self) -> Vec<Bucket> {
        self.buckets.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// How long the bucket of `path` keeps a hidden version (see [`Bucket::hidden_kept`]), as
    /// last seen; `None` when unknown.
    pub fn hidden_kept(&self, path: &str) -> Option<Option<u64>> {
        let s = Spot::of(path)?;
        let buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        buckets.iter().find(|b| b.name == s.bucket)?.hidden_kept(&s.prefix())
    }

    // -----------------------------------------------------------------------------------------
    // Listing

    /// The listing of the folder at `path` kept from before (this run or an earlier one), however
    /// old.
    pub fn cached(&self, path: &str) -> Option<Arc<Listing>> {
        match Spot::of(path) {
            None => self.lists.get("", ""),
            Some(s) => self.lists.get(&s.bucket, &s.prefix()),
        }
    }

    /// Reads ahead the folders inside folder `path` that have no kept listing (up to
    /// [`AHEAD`], [`AT_ONCE`] at a time), so expanding one in the tree or opening it shows at
    /// once. Listing is free on B2; what fails is left for the opening to read.
    pub fn prefetch(&self, path: &str) {
        let Some(kept) = self.cached(path) else { return };
        let base = path.trim_end_matches('/');
        let dirs: Vec<String> = kept.items.iter().filter(|i| i.is_dir).map(|i| format!("{base}/{}", i.name)).filter(|d| self.cached(d).is_none()).take(AHEAD).collect();
        openexplorer_cloud::each(&dirs, AT_ONCE, |d| {
            let _ = self.read(d, false);
        });
    }

    /// The folder at `path`, read from B2 now (a copy's or an upload's check of the names taken
    /// must see what others did a moment ago), and kept.
    pub fn list(&self, path: &str) -> Result<Vec<Item>> {
        self.list_fresh(path)
    }

    /// The folder at `path`, read from B2 now, and kept.
    pub fn list_fresh(&self, path: &str) -> Result<Vec<Item>> {
        self.read(path, true).map(|(items, _)| items).map_err(String::from)
    }

    /// The folder at `path` read again from B2, against what was kept of it: the pane shows the
    /// kept listing at once and this behind it, changing nothing on screen when it's the same.
    pub fn reread(&self, path: &str) -> Result<Fresh> {
        self.read(path, true).map(|(items, same)| if same { Fresh::Same } else { Fresh::Changed(items) }).map_err(String::from)
    }

    /// The folder at `path` read again on the app's own (no one asked): within the key's
    /// [`watch::Budget`], tried once, held while B2 asks for a pause. See [`crate::watch`].
    pub fn poll(&self, path: &str) -> Polled {
        let now = cache::now();
        // The buckets change on the website only, and opening the top reads them.
        if Spot::of(path).is_none() {
            return Polled::Same;
        }
        if let Some(wait) = self.api.paused(now) {
            return Polled::Wait(wait);
        }
        let cost = watch::cost(self.cached(path).map_or(0, |l| l.items.len()));
        if let Err(hold) = self.budget.lock().unwrap_or_else(|e| e.into_inner()).take(now, cost) {
            return hold;
        }
        match self.read(path, false) {
            Ok((_, true)) => Polled::Same,
            Ok((items, false)) => Polled::Changed(items),
            Err(f) if f.capped() => {
                self.budget.lock().unwrap_or_else(|e| e.into_inner()).cap(f.text());
                Polled::Stopped(f.text())
            }
            Err(f) if f.transient() => Polled::Wait(self.api.paused(cache::now()).unwrap_or(watch::EVERY.as_secs())),
            Err(f) => Polled::Failed(f.text()),
        }
    }

    /// How many requests the key's own readings made this hour and this day.
    pub fn polls_spent(&self) -> (u32, u32) {
        self.budget.lock().unwrap_or_else(|e| e.into_inner()).spent()
    }

    /// Reads the folder at `path` and keeps it, unless one of the app's own changes came while
    /// it was read (it may predate it). Gives the items and whether they're those kept.
    fn read(&self, path: &str, patient: bool) -> std::result::Result<(Vec<Item>, bool), Fail> {
        let Some(s) = Spot::of(path) else {
            let kept = self.lists.get("", "");
            let items: Vec<Item> = self.buckets_fresh().map_err(Fail::Other)?.iter().map(Bucket::item).collect();
            let same = kept.is_some_and(|k| k.items == items);
            return Ok((items, same));
        };
        let bucket = self.bucket(&s.bucket).map_err(Fail::Other)?;
        let prefix = s.prefix();
        let kept = self.lists.get(&bucket.name, &prefix);
        let changes = self.changes.load(Ordering::SeqCst);
        let fetched = cache::now();
        let items = match self.read_folder(&bucket, &prefix, patient) {
            Ok(items) => items,
            Err(f) => {
                if f.capped() {
                    self.budget.lock().unwrap_or_else(|e| e.into_inner()).cap(f.text());
                }
                return Err(f);
            }
        };
        if patient {
            self.budget.lock().unwrap_or_else(|e| e.into_inner()).uncap();
        }
        let same = kept.as_ref().is_some_and(|k| k.items == items);
        if self.changes.load(Ordering::SeqCst) == changes {
            if same {
                self.lists.touch(&bucket.name, &prefix, fetched);
            } else {
                self.lists.put(&bucket.name, &prefix, items.clone(), fetched);
            }
        }
        Ok((items, same))
    }

    fn read_folder(&self, bucket: &Bucket, prefix: &str, patient: bool) -> std::result::Result<Vec<Item>, Fail> {
        let auth = self.api.auth()?;
        match within(auth.allowed.name_prefix.as_deref(), prefix) {
            Within::Inside => self.names(bucket, prefix, prefix, patient),
            Within::Toward(seg) => Ok(vec![Item::dir(&seg)]),
            Within::Partial(np) => self.names(bucket, &np, prefix, patient),
            Within::Outside => Err(Fail::Other(t("Esta chave de aplicação só alcança outra pasta deste bucket.", "This application key only reaches another folder of this bucket.").into())),
        }
    }

    /// One folder level of names starting with `list_prefix`, named from after `rel_to`.
    fn names(&self, bucket: &Bucket, list_prefix: &str, rel_to: &str, patient: bool) -> std::result::Result<Vec<Item>, Fail> {
        let mut out = Vec::new();
        let mut start: Option<String> = None;
        loop {
            let mut body = json!({"bucketId": bucket.id, "prefix": list_prefix, "delimiter": "/", "maxFileCount": PAGE});
            if let Some(s) = &start {
                body["startFileName"] = s.clone().into();
            }
            let v = self.api.call_with("b2_list_file_names", &body, patient)?;
            for f in v.get("files").and_then(Value::as_array).into_iter().flatten() {
                let Some(ver) = Version::of(f) else { continue };
                let Some(rel) = ver.name.strip_prefix(rel_to) else { continue };
                match ver.action.as_str() {
                    "folder" => {
                        let name = rel.trim_end_matches('/');
                        if !name.is_empty() && !name.contains('/') {
                            out.push(Item::dir(name));
                        }
                    }
                    "upload" if !rel.is_empty() && !rel.contains('/') && rel != EMPTY_MARKER => out.push(ver.item(rel)),
                    _ => {}
                }
            }
            match v.get("nextFileName").and_then(Value::as_str) {
                Some(n) => start = Some(n.to_string()),
                None => return Ok(out),
            }
        }
    }

    /// Every file under the folder at `path`, at any depth, page by page: 10,000 names a request,
    /// folders not folded (a search reads a whole tree this way in a request or a few, where
    /// folder by folder took one a folder). Each comes as its path below the folder and its
    /// item; `page` returns `false` to stop.
    pub fn walk(&self, path: &str, page: &mut dyn FnMut(Vec<(String, Item)>) -> bool) -> Result<()> {
        let s = Spot::of(path).ok_or_else(|| t("Pesquise dentro de um bucket.", "Search inside a bucket.").to_string())?;
        let bucket = self.bucket(&s.bucket)?;
        let prefix = s.prefix();
        let auth = self.api.auth()?;
        // A key limited to a prefix lists within it, wherever above it the search starts.
        let list_prefix = match (within(auth.allowed.name_prefix.as_deref(), &prefix), auth.allowed.name_prefix.as_deref()) {
            (Within::Inside, _) => prefix.clone(),
            (Within::Outside, _) => return Err(t("Esta chave de aplicação só alcança outra pasta deste bucket.", "This application key only reaches another folder of this bucket.").into()),
            (_, np) => np.unwrap_or_default().to_string(),
        };
        let mut start: Option<String> = None;
        loop {
            let mut body = json!({"bucketId": bucket.id, "prefix": list_prefix, "maxFileCount": PAGE});
            if let Some(s) = &start {
                body["startFileName"] = s.clone().into();
            }
            let v = self.api.run("b2_list_file_names", &body)?;
            let items = v
                .get("files")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Version::of)
                .filter(|v| v.action == "upload")
                .filter_map(|v| {
                    let rel = v.name.strip_prefix(&prefix).filter(|r| !r.is_empty())?.to_string();
                    let item = v.item(rel.rsplit('/').next().unwrap_or(&rel));
                    Some((rel, item))
                })
                .collect();
            if !page(items) {
                return Ok(());
            }
            match v.get("nextFileName").and_then(Value::as_str) {
                Some(n) => start = Some(n.to_string()),
                None => return Ok(()),
            }
        }
    }

    /// Every current file under `prefix` (no folder levels), as versions.
    pub(crate) fn files_under(&self, bucket: &Bucket, prefix: &str) -> Result<Vec<Version>> {
        let mut out = Vec::new();
        let mut start: Option<String> = None;
        loop {
            let mut body = json!({"bucketId": bucket.id, "prefix": prefix, "maxFileCount": PAGE});
            if let Some(s) = &start {
                body["startFileName"] = s.clone().into();
            }
            let v = self.api.run("b2_list_file_names", &body)?;
            out.extend(v.get("files").and_then(Value::as_array).into_iter().flatten().filter_map(Version::of).filter(|v| v.action == "upload"));
            match v.get("nextFileName").and_then(Value::as_str) {
                Some(n) => start = Some(n.to_string()),
                None => return Ok(out),
            }
        }
    }

    /// Every version (hide markers and unfinished uploads included) of the file named `name`
    /// exactly, or with `under` of every name starting with it: names in order, each newest first.
    pub(crate) fn versions(&self, bucket: &Bucket, name: &str, under: bool) -> Result<Vec<Version>> {
        let mut out = Vec::new();
        let mut start: Option<(String, Option<String>)> = Some((name.to_string(), None));
        while let Some((from, from_id)) = start.take() {
            let mut body = json!({"bucketId": bucket.id, "prefix": name, "startFileName": from, "maxFileCount": if under { PAGE } else { 100 }});
            if let Some(id) = from_id {
                body["startFileId"] = id.into();
            }
            let v = self.api.run("b2_list_file_versions", &body)?;
            let mut past = false;
            for ver in v.get("files").and_then(Value::as_array).into_iter().flatten().filter_map(Version::of) {
                if !under && ver.name != name {
                    // Names come in order: past the exact name, nothing more is its.
                    past |= ver.name.as_str() > name;
                    continue;
                }
                out.push(ver);
            }
            if past {
                break;
            }
            if let Some(n) = v.get("nextFileName").and_then(Value::as_str) {
                start = Some((n.to_string(), v.get("nextFileId").and_then(Value::as_str).map(str::to_string)));
            }
        }
        Ok(out)
    }

    /// The current version of the file named `key`, if there's one.
    pub fn file(&self, bucket: &Bucket, key: &str) -> Result<Option<Version>> {
        let v = self.api.run("b2_list_file_names", &json!({"bucketId": bucket.id, "prefix": key, "startFileName": key, "maxFileCount": 1}))?;
        Ok(v.get("files").and_then(Value::as_array).and_then(|f| f.first()).and_then(Version::of).filter(|v| v.name == key && v.action == "upload"))
    }

    /// The item at `path`, or `None` when nothing's there, asked of B2 now: it's what a new
    /// folder, a copy or a move checks before writing, and a name taken elsewhere a moment ago
    /// must count. One request, mostly.
    pub fn stat(&self, path: &str) -> Result<Option<Item>> {
        let Some(s) = Spot::of(path) else { return Ok(Some(Item::dir(""))) };
        if s.key.is_empty() {
            return Ok(self.bucket(&s.bucket).ok().map(|b| b.item()));
        }
        let name = s.parent().1;
        Ok(self.lookup(&s)?.map(|found| match found {
            Found::File(v) => v.item(name),
            Found::Dir => Item::dir(name),
        }))
    }

    /// What B2 has at `s` now: a file (its current version), a folder, or nothing.
    pub(crate) fn lookup(&self, s: &Spot) -> Result<Option<Found>> {
        let bucket = self.bucket(&s.bucket)?;
        let auth = self.api.auth()?;
        // A name above the key's prefix can only be a folder on the way down to it (asking B2
        // about it would be refused).
        if let Some(np) = auth.allowed.name_prefix.as_deref()
            && !s.key.starts_with(np)
        {
            return Ok(match within(Some(np), &s.prefix()) {
                Within::Outside => None,
                _ => Some(Found::Dir),
            });
        }
        // Names from `key` on, folders folded: the file `key` comes first, the folder `key/`
        // after the names between ("key.txt", "key (2)"), so one page mostly holds the answer.
        let folder = s.prefix();
        let v = self.api.run("b2_list_file_names", &json!({"bucketId": bucket.id, "prefix": s.key, "startFileName": s.key, "delimiter": "/", "maxFileCount": 100}))?;
        for f in v.get("files").and_then(Value::as_array).into_iter().flatten().filter_map(Version::of) {
            if f.name == s.key && f.action == "upload" {
                return Ok(Some(Found::File(f)));
            }
            if f.name == folder {
                return Ok(Some(Found::Dir));
            }
            if f.name > folder {
                return Ok(None);
            }
        }
        if v.get("nextFileName").and_then(Value::as_str).is_none() {
            return Ok(None);
        }
        // The names between filled the page: ask about the folder alone.
        let v = self.api.run("b2_list_file_names", &json!({"bucketId": bucket.id, "prefix": folder, "maxFileCount": 1}))?;
        let any = v.get("files").and_then(Value::as_array).is_some_and(|f| !f.is_empty());
        Ok(any.then_some(Found::Dir))
    }

    // -----------------------------------------------------------------------------------------
    // Changes

    /// The folder and the name a new item at `path` gets, refused at the top and a bucket's.
    fn spot_for_change(path: &str) -> Result<Spot> {
        match Spot::of(path) {
            Some(s) if !s.key.is_empty() => {
                check_name(s.parent().1)?;
                Ok(s)
            }
            Some(_) => Err(t("Buckets são criados, renomeados e apagados no site da Backblaze.", "Buckets are made, renamed and deleted on the Backblaze website.").into()),
            None => Err(t("O topo da conta B2 só lista os buckets: abra um deles.", "The B2 account's top only lists the buckets: open one of them.").into()),
        }
    }

    /// Drops the listings a change at `s` touches: its folder's, and with `tree` its own and
    /// every one under it.
    /// Every folder above may gain or lose a folder too (a folder exists only while a name is
    /// under it), so their listings go as well.
    pub(crate) fn changed(&self, s: &Spot, tree: bool) {
        // A listing being read meanwhile may predate the change: it isn't kept.
        self.changes.fetch_add(1, Ordering::SeqCst);
        self.sizes_changed(&s.bucket);
        let parent = s.parent().0.prefix();
        self.lists.forget(&s.bucket, "");
        for (i, _) in parent.match_indices('/') {
            self.lists.forget(&s.bucket, &parent[..=i]);
        }
        if tree {
            self.lists.forget_under(&s.bucket, &s.prefix());
        }
    }

    /// A new folder at `path`: its `.bzEmpty` marker.
    pub fn mkdir(&self, up: &mut Uploads, path: &str) -> Result<()> {
        let s = Self::spot_for_change(path)?;
        if self.stat(path)?.is_some() {
            return Err(tr!("Já existe “{}” nessa pasta do B2.", "“{}” already exists in that B2 folder.", s.parent().1));
        }
        let bucket = self.bucket(&s.bucket)?;
        self.put_bytes(up, &bucket, &format!("{}{EMPTY_MARKER}", s.prefix()), Vec::new())?;
        self.changed(&s, true);
        Ok(())
    }

    /// Deletes the empty folder at `path` (undoing a new folder): its marker, every version.
    pub fn remove_dir(&self, path: &str) -> Result<()> {
        let s = Self::spot_for_change(path)?;
        let bucket = self.bucket(&s.bucket)?;
        let marker = format!("{}{EMPTY_MARKER}", s.prefix());
        for v in self.versions(&bucket, &marker, false)? {
            self.delete_version(&v)?;
        }
        self.changed(&s, true);
        Ok(())
    }

    /// Deletes one version for good; one already gone is fine.
    pub(crate) fn delete_version(&self, v: &Version) -> Result<()> {
        self.delete_id(&v.name, &v.id)
    }

    /// Deletes version `id` of the file `name` for good; one already gone is fine.
    fn delete_id(&self, name: &str, id: &str) -> Result<()> {
        match self.api.call("b2_delete_file_version", &json!({"fileName": name, "fileId": id})) {
            Ok(_) => Ok(()),
            Err(f) if f.code() == "file_not_present" || f.status() == 404 => Ok(()),
            Err(f) => Err(f.text()),
        }
    }

    fn hide_name(&self, bucket: &Bucket, name: &str) -> Result<i64> {
        let v = self.api.run("b2_hide_file", &json!({"bucketId": bucket.id, "fileName": name}))?;
        Ok(v.get("uploadTimestamp").and_then(Value::as_i64).unwrap_or(0))
    }

    /// Deletes the file (or every file of the folder) at `path` by hiding it: it leaves the
    /// listings, and what it held stays as a hidden version for as long as the bucket's
    /// lifecycle rules keep it. Gives the time of the earliest hide marker (milliseconds since
    /// 1970, B2's clock), from which [`Account::unhide`] brings it back.
    ///
    /// A folder's files are hidden several at once; one refused stops the rest, and the error
    /// then says from when what was hidden before it can be brought back, so it isn't left
    /// hidden with no Desfazer.
    pub fn hide(&self, path: &str, is_dir: bool) -> std::result::Result<i64, HideStopped> {
        let s = Self::spot_for_change(path).map_err(HideStopped::before_any)?;
        let bucket = self.bucket(&s.bucket).map_err(HideStopped::before_any)?;
        if is_dir {
            let files = self.files_under(&bucket, &s.prefix()).map_err(HideStopped::before_any)?;
            let (stamps, error) = each_done(&files, |f| self.hide_name(&bucket, &f.name));
            self.changed(&s, true);
            let since = stamps.iter().copied().min();
            match error {
                None => Ok(since.unwrap_or_else(|| cache::now() as i64 * 1000)),
                Some(e) => Err(HideStopped { since, error: with_done(e, stamps.len()) }),
            }
        } else {
            let stamp = self.hide_name(&bucket, &s.key);
            self.changed(&s, false);
            stamp.map_err(HideStopped::before_any)
        }
    }

    /// The hide markers at `path` (or under it) made at `since` or after that are their name's
    /// newest versions (a hide sent twice, its first answer lost, leaves two): what
    /// [`Account::unhide`] would delete.
    pub fn hidden_since(&self, path: &str, is_dir: bool, since: i64) -> Result<Vec<Version>> {
        let s = Self::spot_for_change(path)?;
        let bucket = self.bucket(&s.bucket)?;
        let all = if is_dir { self.versions(&bucket, &s.prefix(), true)? } else { self.versions(&bucket, &s.key, false)? };
        let mut out = Vec::new();
        let mut name: Option<String> = None;
        let mut leading = false;
        for v in all {
            // Each name's versions come newest first.
            if name.as_deref() != Some(&v.name) {
                name = Some(v.name.clone());
                leading = true;
            }
            leading &= v.action == "hide" && v.uploaded >= since;
            if leading {
                out.push(v);
            }
        }
        Ok(out)
    }

    /// Brings back what [`Account::hide`] hid at `since`: deletes each name's hide marker, so
    /// its previous version is the current one again. How many came back.
    pub fn unhide(&self, path: &str, is_dir: bool, since: i64) -> Result<usize> {
        let markers = self.hidden_since(path, is_dir, since)?;
        let s = Self::spot_for_change(path)?;
        let done = each(&markers, |m| self.delete_version(m));
        self.changed(&s, is_dir);
        done?;
        let mut names: Vec<&str> = markers.iter().map(|m| m.name.as_str()).collect();
        names.dedup();
        Ok(names.len())
    }

    /// Deletes every version of the file (or of every file in the folder) at `path` for good,
    /// hide markers and unfinished uploads too. How many versions went.
    pub fn purge(&self, path: &str, is_dir: bool) -> Result<usize> {
        let s = Self::spot_for_change(path)?;
        let bucket = self.bucket(&s.bucket)?;
        let all = if is_dir { self.versions(&bucket, &s.prefix(), true)? } else { self.versions(&bucket, &s.key, false)? };
        let done = each(&all, |v| self.delete_version(v));
        self.changed(&s, is_dir);
        done.map(|d| d.len())
    }

    /// The source of a rename or a copy, and the place it goes, checked: the place free, and a
    /// folder not into itself.
    fn source_and_target(&self, from: &str, to: &str) -> Result<(Spot, Spot, Found)> {
        let (src, dst) = (Self::spot_for_change(from)?, Self::spot_for_change(to)?);
        let found = self.lookup(&src)?.ok_or_else(|| not_there(&last(from)))?;
        if matches!(found, Found::Dir) && src.bucket == dst.bucket && (dst.key == src.key || dst.key.starts_with(&src.prefix())) {
            return Err(tr!("Não dá para pôr a pasta “{}” dentro dela mesma.", "Can't put the folder “{}” inside itself.", last(from)));
        }
        if self.lookup(&dst)?.is_some() {
            return Err(tr!("Já existe “{}” nessa pasta do B2.", "“{}” already exists in that B2 folder.", dst.parent().1));
        }
        Ok((src, dst, found))
    }

    /// Copies what's at `src` (`found` there) to `dst` on B2's side: a file, or each file of a
    /// folder. Gives the versions copied.
    ///
    /// A copy that stops midway takes back what it made: the place was free, so each copy made
    /// there is deleted (hidden, by a key that may not delete) and nothing half copied stays,
    /// neither on screen nor billed as a hidden version.
    fn copy_spot(&self, src: &Spot, dst: &Spot, found: Found) -> Result<Vec<Version>> {
        let (from, into) = (self.bucket(&src.bucket)?, self.bucket(&dst.bucket)?);
        let is_dir = matches!(found, Found::Dir);
        let files = match found {
            Found::Dir => self.files_under(&from, &src.prefix())?,
            Found::File(v) => vec![v],
        };
        let names: Vec<(Version, String)> = files
            .into_iter()
            .map(|f| {
                let name = if is_dir { format!("{}{}", dst.prefix(), &f.name[src.prefix().len()..]) } else { dst.key.clone() };
                (f, name)
            })
            .collect();
        let (made, error) = each_done(&names, |(f, name)| self.copy_version(f, &into, name).map(|id| (f.clone(), name.clone(), id)));
        let result = match error {
            None => Ok(made.into_iter().map(|(f, ..)| f).collect()),
            Some(e) => {
                let (_, left) = each_done(&made, |(_, name, id)| match self.delete_id(name, id) {
                    Ok(()) => Ok(()),
                    Err(d) => {
                        crate::api::log(&format!("B2: não apaguei a cópia {name} ({d}), escondendo"));
                        self.hide_name(&into, name).map(|_| ())
                    }
                });
                Err(match left {
                    None => e,
                    Some(l) => tr!("{e} (e parte da cópia ficou em “{}”: {l})", "{e} (and part of the copy stayed in “{}”: {l})", last(&dst.key)),
                })
            }
        };
        self.changed(dst, is_dir);
        result
    }

    /// Copies version `v` to `name` in `into`, on B2's side. Gives the new version's id.
    pub(crate) fn copy_version(&self, v: &Version, into: &Bucket, name: &str) -> Result<String> {
        if v.size >= COPY_WHOLE_BELOW {
            return self.copy_large(v, into, name);
        }
        let made = self.api.run("b2_copy_file", &json!({"sourceFileId": v.id, "destinationBucketId": into.id, "fileName": name, "metadataDirective": "COPY"}))?;
        made.get("fileId").and_then(Value::as_str).map(str::to_string).ok_or_else(|| t("Resposta inesperada do B2 ao copiar.", "Unexpected answer from B2 copying.").into())
    }

    /// Copies the file or folder at `from` to `to` on B2's side: no bytes go through the app.
    pub fn copy(&self, from: &str, to: &str) -> Result<()> {
        let (src, dst, found) = self.source_and_target(from, to)?;
        self.copy_spot(&src, &dst, found).map(|_| ())
    }

    /// Renames or moves the item at `from` to `to`, within the account: a copy on B2's side,
    /// then the old name retired (see the crate docs). A copy that fails takes itself back
    /// (nothing changed); old names that fail to leave after the whole copy went are said, as
    /// what's at `to` is then the only copy of those already gone.
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (src, dst, found) = self.source_and_target(from, to)?;
        let is_dir = matches!(found, Found::Dir);
        let copied = self.copy_spot(&src, &dst, found)?;
        let result = self.retire(&src, is_dir, &copied);
        self.changed(&src, is_dir);
        result.map_err(|(left, e)| {
            tr!(
                "Tudo foi copiado para “{}”, mas {left} de {} item(ns) ficaram também em “{}”: {e}",
                "Everything was copied to “{}”, but {left} of {} item(s) also stayed in “{}”: {e}",
                last(to),
                copied.len(),
                last(from)
            )
        })
    }

    /// The names of `copied` (just copied from `src`) leave: the copied version is deleted when
    /// it was its name's only version, else the name is hidden, its history kept. Stopped
    /// midway, how many are left, and why.
    fn retire(&self, src: &Spot, is_dir: bool, copied: &[Version]) -> std::result::Result<(), (usize, String)> {
        let bucket = self.bucket(&src.bucket).map_err(|e| (copied.len(), e))?;
        let all = if is_dir { self.versions(&bucket, &src.prefix(), true) } else { self.versions(&bucket, &src.key, false) }.map_err(|e| (copied.len(), e))?;
        let mut count: HashMap<&str, usize> = HashMap::new();
        for v in &all {
            *count.entry(v.name.as_str()).or_default() += 1;
        }
        let only = |v: &Version| count.get(v.name.as_str()) == Some(&1) && all.iter().any(|a| a.id == v.id);
        let (done, error) = each_done(copied, |v| {
            if only(v) {
                match self.delete_version(v) {
                    Ok(()) => return Ok(()),
                    // A key that may write but not delete hides instead.
                    Err(e) => crate::api::log(&format!("B2: não apaguei {} ({e}), escondendo", v.name)),
                }
            }
            self.hide_name(&bucket, &v.name).map(|_| ())
        });
        match error {
            None => Ok(()),
            Some(e) => Err((copied.len() - done.len(), e)),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The keys open now

static OPEN: LazyLock<Mutex<HashMap<String, Arc<Account>>>> = LazyLock::new(Mutex::default);

/// The account open for `key_id`, if it is.
pub fn cached(key_id: &str) -> Option<Arc<Account>> {
    OPEN.lock().unwrap_or_else(|e| e.into_inner()).get(key_id.trim()).cloned()
}

/// The account for `key_id`: open already, or opened with the application key kept in the
/// credential store. Asks B2 nothing yet.
pub fn reach(key_id: &str) -> Result<Arc<Account>> {
    let key_id = key_id.trim();
    let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(a) = open.get(key_id) {
        return Ok(a.clone());
    }
    let key = store::secret(key_id)?;
    let account = Arc::new(Account::new(Api::new(key_id, &key), cache::file_of(key_id)));
    open.insert(key_id.to_string(), account.clone());
    Ok(account)
}

/// Authorizes key `key_id` with `key`, and keeps the key in the credential store.
pub fn connect(key_id: &str, key: &str) -> Result<Arc<Account>> {
    let key_id = key_id.trim();
    let api = Api::new(key_id, key);
    api.auth().map_err(String::from)?;
    // A key made again under an id kept before: its old listings don't open with it.
    let account = Arc::new(Account::new(api, cache::file_of(key_id)));
    store::set_secret(key_id, key.trim())?;
    OPEN.lock().unwrap_or_else(|e| e.into_inner()).insert(key_id.to_string(), account.clone());
    Ok(account)
}

/// Forgets key `key_id`: its application key leaves the credential store and its listings the
/// disk. The key itself stays valid on B2 (delete it on the website).
pub fn disconnect(key_id: &str) {
    let key_id = key_id.trim();
    let open = OPEN.lock().unwrap_or_else(|e| e.into_inner()).remove(key_id);
    match open {
        Some(a) => a.lists.forget_all(),
        None => {
            if let Some(f) = cache::file_of(key_id) {
                cache::forget_file(&f);
            }
        }
    }
    crate::notes::forget_key(key_id);
    store::forget_secret(key_id);
}

/// Writes the listings of each key open whose listings changed (the app calls it as it closes).
pub fn save_caches() {
    let open: Vec<Arc<Account>> = OPEN.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
    for a in open {
        a.lists.save_if_changed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_split_into_bucket_and_name() {
        assert_eq!(Spot::of("/"), None);
        let s = Spot::of("/fotos/2024/praia.jpg").unwrap();
        assert_eq!((s.bucket.as_str(), s.key.as_str(), s.prefix().as_str()), ("fotos", "2024/praia.jpg", "2024/praia.jpg/"));
        assert_eq!(s.parent(), (Spot { bucket: "fotos".into(), key: "2024".into() }, "praia.jpg"));
        let top = Spot::of("/fotos/").unwrap();
        assert_eq!((top.key.as_str(), top.prefix().as_str()), ("", ""));
        assert_eq!(Spot::of("/fotos/a").unwrap().parent().0.prefix(), "");
    }

    #[test]
    fn a_key_limited_to_a_prefix_sees_the_way_down_to_it() {
        assert_eq!(within(None, ""), Within::Inside);
        assert_eq!(within(Some("time/ana/"), ""), Within::Toward("time".into()));
        assert_eq!(within(Some("time/ana/"), "time/"), Within::Toward("ana".into()));
        assert_eq!(within(Some("time/ana/"), "time/ana/"), Within::Inside);
        assert_eq!(within(Some("time/ana/"), "time/ana/fotos/"), Within::Inside);
        assert_eq!(within(Some("time/an"), "time/"), Within::Partial("time/an".into()));
        assert_eq!(within(Some("time/ana/"), "outro/"), Within::Outside);
    }

    #[test]
    fn hidden_versions_last_as_the_rules_say() {
        let rules = |r: Vec<Rule>| Bucket { id: "b".into(), name: "n".into(), rules: Some(r) };
        let rule = |prefix: &str, days: Option<u64>| Rule { prefix: prefix.into(), hide_to_delete: days, upload_to_hide: None };
        assert_eq!(rules(vec![]).hidden_kept("a.txt"), Some(None));
        assert_eq!(rules(vec![rule("", Some(1))]).hidden_kept("a.txt"), Some(Some(1)));
        assert_eq!(rules(vec![rule("", Some(30)), rule("tmp/", Some(2))]).hidden_kept("tmp/a"), Some(Some(2)));
        assert_eq!(rules(vec![rule("tmp/", Some(2))]).hidden_kept("a"), Some(None));
        assert_eq!(Bucket { id: "b".into(), name: "n".into(), rules: None }.hidden_kept("a"), None);
        let back = Bucket::of_item(&rules(vec![rule("", Some(7))]).item()).unwrap();
        assert_eq!(back.hidden_kept("x"), Some(Some(7)));
    }

    #[test]
    fn names_b2_or_the_panes_cant_take_are_refused() {
        for bad in ["", " ", ".", "..", "a/b", "a\u{1}b", EMPTY_MARKER] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_name("relatório final (2).pdf").is_ok());
    }

    #[test]
    fn batches_stop_at_the_first_error_and_say_how_far_they_got() {
        let items: Vec<usize> = (0..50).collect();
        assert_eq!(each(&items, |i| Ok(i * 2)).unwrap().len(), 50);
        let e = each(&[1], |_| Err::<(), _>("ruim".to_string())).unwrap_err();
        assert_eq!(e, "ruim");
        let e = each(&items, |i| if *i == 30 { Err("ruim".to_string()) } else { Ok(()) }).unwrap_err();
        assert!(e.starts_with("ruim (feitos antes do erro:"), "{e}");
    }
}
