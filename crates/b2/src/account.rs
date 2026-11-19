//! One application key: the buckets it reaches, their folders, and what the panes do to them.
//!
//! One `Account` per key ID is open at a time, shared by every pane and transfer (see
//! [`reach`]); it holds the key's authorization and its listings ([`crate::cache`]).
//!
//! Paths are `/bucket/folder/file`: `/` lists the buckets, `/bucket` a bucket's top.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use openexplorer_i18n::{t, tr};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::api::Api;
use crate::cache::{self, Listing, Listings};
use crate::upload::Uploads;
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
    if items.len() <= 1 {
        return items.iter().map(&work).collect();
    }
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let out: Mutex<Vec<Option<Result<R>>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..AT_ONCE.min(items.len()) {
            scope.spawn(|| {
                while !failed.load(Ordering::SeqCst) {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(item) = items.get(i) else { break };
                    let r = work(item);
                    failed.fetch_or(r.is_err(), Ordering::SeqCst);
                    out.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(r);
                }
            });
        }
    });
    let results = out.into_inner().unwrap_or_else(|e| e.into_inner());
    let done = results.iter().filter(|r| matches!(r, Some(Ok(_)))).count();
    let mut ok = Vec::with_capacity(items.len());
    for r in results.into_iter().flatten() {
        match r {
            Ok(v) => ok.push(v),
            Err(e) if done == 0 => return Err(e),
            Err(e) => return Err(tr!("{e} (feitos antes do erro: {done})", "{e} (done before the error: {done})")),
        }
    }
    Ok(ok)
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
        Account { key_id: api.key_id().to_string(), api, lists, buckets: Mutex::new(buckets), parallel: crate::upload::PARTS_AT_ONCE }
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
        buckets.iter().find(|b| b.name == s.bucket)?.hidden_kept(&s.key)
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

    /// The folder at `path`: kept if younger than [`cache::FRESH`], else read from B2.
    pub fn list(&self, path: &str) -> Result<Vec<Item>> {
        match self.cached(path) {
            Some(l) if l.fresh() => Ok(l.items.clone()),
            _ => self.list_fresh(path),
        }
    }

    /// The folder at `path`, read from B2 now, and kept.
    pub fn list_fresh(&self, path: &str) -> Result<Vec<Item>> {
        let Some(s) = Spot::of(path) else {
            return Ok(self.buckets_fresh()?.iter().map(Bucket::item).collect());
        };
        let bucket = self.bucket(&s.bucket)?;
        let prefix = s.prefix();
        let fetched = cache::now();
        let items = self.read_folder(&bucket, &prefix)?;
        self.lists.put(&bucket.name, &prefix, items.clone(), fetched);
        Ok(items)
    }

    fn read_folder(&self, bucket: &Bucket, prefix: &str) -> Result<Vec<Item>> {
        let auth = self.api.auth()?;
        match within(auth.allowed.name_prefix.as_deref(), prefix) {
            Within::Inside => self.names(bucket, prefix, prefix),
            Within::Toward(seg) => Ok(vec![Item::dir(&seg)]),
            Within::Partial(np) => self.names(bucket, &np, prefix),
            Within::Outside => Err(t("Esta chave de aplicação só alcança outra pasta deste bucket.", "This application key only reaches another folder of this bucket.").into()),
        }
    }

    /// One folder level of names starting with `list_prefix`, named from after `rel_to`.
    fn names(&self, bucket: &Bucket, list_prefix: &str, rel_to: &str) -> Result<Vec<Item>> {
        let mut out = Vec::new();
        let mut start: Option<String> = None;
        loop {
            let mut body = json!({"bucketId": bucket.id, "prefix": list_prefix, "delimiter": "/", "maxFileCount": PAGE});
            if let Some(s) = &start {
                body["startFileName"] = s.clone().into();
            }
            let v = self.api.run("b2_list_file_names", &body)?;
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

    /// The item at `path`, or `None` when nothing's there: from the folder's kept listing when
    /// it's young, else asked of B2.
    pub fn stat(&self, path: &str) -> Result<Option<Item>> {
        let Some(s) = Spot::of(path) else { return Ok(Some(Item::dir(""))) };
        if s.key.is_empty() {
            return Ok(self.bucket(&s.bucket).ok().map(|b| b.item()));
        }
        let (parent, name) = s.parent();
        if let Some(l) = self.lists.get(&parent.bucket, &parent.prefix())
            && l.fresh()
        {
            return Ok(l.items.iter().find(|i| i.name == name).cloned());
        }
        let bucket = self.bucket(&s.bucket)?;
        let auth = self.api.auth()?;
        match within(auth.allowed.name_prefix.as_deref(), &s.prefix()) {
            Within::Inside => {}
            // A folder on the way down to the key's prefix.
            Within::Toward(_) | Within::Partial(_) => return Ok(Some(Item::dir(name))),
            Within::Outside => return Ok(None),
        }
        if let Some(v) = self.file(&bucket, &s.key)? {
            return Ok(Some(v.item(name)));
        }
        let v = self.api.run("b2_list_file_names", &json!({"bucketId": bucket.id, "prefix": s.prefix(), "maxFileCount": 1}))?;
        let any = v.get("files").and_then(Value::as_array).is_some_and(|f| !f.is_empty());
        Ok(any.then(|| Item::dir(name)))
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
    pub(crate) fn changed(&self, s: &Spot, tree: bool) {
        self.lists.forget(&s.bucket, &s.parent().0.prefix());
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
        match self.api.call("b2_delete_file_version", &json!({"fileName": v.name, "fileId": v.id})) {
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
    pub fn hide(&self, path: &str, is_dir: bool) -> Result<i64> {
        let s = Self::spot_for_change(path)?;
        let bucket = self.bucket(&s.bucket)?;
        let since = if is_dir {
            let files = self.files_under(&bucket, &s.prefix())?;
            let stamps = each(&files, |f| self.hide_name(&bucket, &f.name));
            self.changed(&s, true);
            stamps?.into_iter().min().unwrap_or_else(|| cache::now() as i64 * 1000)
        } else {
            let stamp = self.hide_name(&bucket, &s.key);
            self.changed(&s, false);
            stamp?
        };
        Ok(since)
    }

    /// The hide markers at `path` (or under it) that are their name's newest version and were
    /// made at `since` or after: what [`Account::unhide`] would delete.
    pub fn hidden_since(&self, path: &str, is_dir: bool, since: i64) -> Result<Vec<Version>> {
        let s = Self::spot_for_change(path)?;
        let bucket = self.bucket(&s.bucket)?;
        let all = if is_dir { self.versions(&bucket, &s.prefix(), true)? } else { self.versions(&bucket, &s.key, false)? };
        let mut out = Vec::new();
        let mut seen = String::new();
        for v in all {
            if v.name == seen {
                continue;
            }
            seen = v.name.clone();
            // Each name's first version is its newest.
            if v.action == "hide" && v.uploaded >= since {
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
        done.map(|d| d.len())
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
    fn source_and_target(&self, from: &str, to: &str) -> Result<(Spot, Spot, Item)> {
        let (src, dst) = (Self::spot_for_change(from)?, Self::spot_for_change(to)?);
        let item = self.stat(from)?.ok_or_else(|| not_there(&last(from)))?;
        if item.is_dir && src.bucket == dst.bucket && (dst.key == src.key || dst.key.starts_with(&src.prefix())) {
            return Err(tr!("Não dá para pôr a pasta “{}” dentro dela mesma.", "Can't put the folder “{}” inside itself.", item.name));
        }
        if self.stat(to)?.is_some() {
            return Err(tr!("Já existe “{}” nessa pasta do B2.", "“{}” already exists in that B2 folder.", dst.parent().1));
        }
        Ok((src, dst, item))
    }

    /// Copies what's at `src` to `dst` on B2's side: a file, or each file of a folder. Gives the
    /// versions copied.
    fn copy_spot(&self, src: &Spot, dst: &Spot, is_dir: bool) -> Result<Vec<Version>> {
        let (from, into) = (self.bucket(&src.bucket)?, self.bucket(&dst.bucket)?);
        let files = if is_dir {
            self.files_under(&from, &src.prefix())?
        } else {
            vec![self.file(&from, &src.key)?.ok_or_else(|| not_there(src.parent().1))?]
        };
        let names: Vec<(Version, String)> = files
            .into_iter()
            .map(|f| {
                let name = if is_dir { format!("{}{}", dst.prefix(), &f.name[src.prefix().len()..]) } else { dst.key.clone() };
                (f, name)
            })
            .collect();
        let copied = each(&names, |(f, name)| self.copy_version(f, &into, name).map(|_| f.clone()));
        self.changed(dst, is_dir);
        copied
    }

    /// Copies version `v` to `name` in `into`, on B2's side.
    pub(crate) fn copy_version(&self, v: &Version, into: &Bucket, name: &str) -> Result<()> {
        if v.size >= COPY_WHOLE_BELOW {
            return self.copy_large(v, into, name);
        }
        self.api.run("b2_copy_file", &json!({"sourceFileId": v.id, "destinationBucketId": into.id, "fileName": name, "metadataDirective": "COPY"}))?;
        Ok(())
    }

    /// Copies the file or folder at `from` to `to` on B2's side: no bytes go through the app.
    pub fn copy(&self, from: &str, to: &str) -> Result<()> {
        let (src, dst, item) = self.source_and_target(from, to)?;
        self.copy_spot(&src, &dst, item.is_dir).map(|_| ())
    }

    /// Renames or moves the item at `from` to `to`, within the account: a copy on B2's side,
    /// then the old name retired (see the crate docs).
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (src, dst, item) = self.source_and_target(from, to)?;
        let copied = self.copy_spot(&src, &dst, item.is_dir)?;
        let result = self.retire(&src, item.is_dir, &copied);
        self.changed(&src, item.is_dir);
        result
    }

    /// The names of `copied` (just copied from `src`) leave: the copied version is deleted when
    /// it was its name's only version, else the name is hidden, its history kept.
    fn retire(&self, src: &Spot, is_dir: bool, copied: &[Version]) -> Result<()> {
        let bucket = self.bucket(&src.bucket)?;
        let all = if is_dir { self.versions(&bucket, &src.prefix(), true)? } else { self.versions(&bucket, &src.key, false)? };
        let mut count: HashMap<&str, usize> = HashMap::new();
        for v in &all {
            *count.entry(v.name.as_str()).or_default() += 1;
        }
        let only = |v: &Version| count.get(v.name.as_str()) == Some(&1) && all.iter().any(|a| a.id == v.id);
        each(copied, |v| {
            if only(v) {
                match self.delete_version(v) {
                    Ok(()) => return Ok(()),
                    // A key that may write but not delete hides instead.
                    Err(e) => crate::api::log(&format!("B2: não apaguei {} ({e}), escondendo", v.name)),
                }
            }
            self.hide_name(&bucket, &v.name).map(|_| ())
        })
        .map(|_| ())
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
