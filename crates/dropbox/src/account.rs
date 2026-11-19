//! A connected account: its connection, where its paths start (a team space's root), the
//! listings it keeps (see [`crate::cache`]) and the calls the panes make, each applied to the
//! kept listings as soon as Dropbox confirms it.
//!
//! One `Account` per e-mail is open at a time, shared by every pane and transfer (see
//! [`reach`]): the pool's "connections" to it cost no login of their own.
//!
//! # Team accounts
//!
//! A member of a team with a team space has a root namespace (the team space: their own folder
//! and the team folders) apart from their home namespace. Paths are relative to the home unless
//! a call carries `Dropbox-API-Path-Root`, so an account whose root isn't its home sends its root
//! with every call: the panes see what the Dropbox website shows, team folders included.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use openexplorer_i18n::{t, tr};
use serde_json::{Value, json};

use crate::api::{Api, Fail, Hosts};
use crate::cache::{self, Lists, Secret};
use crate::meta::{Change, Entry, api_path, change, entry, key, last, parent};
use crate::store::{self, normal_email};
use crate::{AppConfig, Result};

/// How long after reading the log to its end a reopened folder is taken as current without
/// reading it again.
const FRESH: Duration = Duration::from_secs(2);
/// Pages of changes read at once before giving up on the log (each up to 2,000 changes): past
/// it, re-reading the folders as they open costs less.
const MAX_LOG_PAGES: usize = 5;
/// How often, at most, the listings are kept on disk while they change.
const SAVE_EVERY: Duration = Duration::from_secs(60);
/// Folders noted as changed for the watcher, past which it's told "everything".
const MAX_PENDING: usize = 10_000;

/// The account's storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quota {
    pub used: u64,
    /// What the account may use: its own allocation, or what the team allows it.
    pub total: u64,
    /// The space is the team's, shared with its members.
    pub team: bool,
}

/// What changed since the watcher last asked.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Changes {
    /// The folders whose items changed (lowercased API paths, `""` for the top).
    pub folders: HashSet<String>,
    /// Too much to name: every folder may have changed.
    pub all: bool,
    /// Seconds Dropbox asked to wait before the next longpoll.
    pub backoff: Option<u64>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.folders.is_empty() && !self.all
    }

    /// Whether the folder at app path `path` may have changed.
    pub fn touches(&self, path: &str) -> bool {
        self.all || self.folders.contains(&key(path))
    }
}

pub struct Account {
    pub email: String,
    pub name: String,
    pub account_id: String,
    /// The account's root is a team space (see the module docs).
    pub team: bool,
    pub api: Api,
    pub(crate) lists: Mutex<Lists>,
    /// One reader of the log at a time.
    log_gate: Mutex<()>,
    /// One batch job (moves, copies, an upload batch's commit) at a time: Dropbox asks that an
    /// account's batch commits go one after another, and jobs at once only make it busier.
    pub(crate) batch_gate: Mutex<()>,
    /// When the log was last read to its end.
    caught_up: Mutex<Option<Instant>>,
    /// The cursor a longpoll is waiting on now, since when and for how long: while it's the
    /// log's and the wait hasn't overrun, nothing changed since.
    watching: Mutex<Option<(String, Instant, u64)>>,
    /// Folders the log changed since the watcher last asked.
    pending: Mutex<(HashSet<String>, bool)>,
    /// Where the listings are kept between runs and what seals them; `None` keeps nothing.
    kept: Option<(PathBuf, Secret)>,
    saved_at: Mutex<Option<Instant>>,
    /// The account was removed: a save on its way writes nothing.
    forgotten: AtomicBool,
    /// [`cache::mark`] as it opened.
    mark: u64,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account").field("email", &self.email).field("team", &self.team).finish_non_exhaustive()
    }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// The words of a failure about the item at `path`.
pub(crate) fn about(path: &str, f: Fail) -> String {
    match f {
        Fail::Route { .. } => format!("“{}”: {}", last(&api_path(path)), f.text()),
        f => f.text(),
    }
}

impl Account {
    /// Opens the account `api` reaches: who it is and where its paths start (one call), and the
    /// listings kept in `file`, if they're of this login and this root.
    pub fn open(api: Api, file: Option<PathBuf>) -> Result<Account> {
        let mark = cache::mark();
        let me = api.rpc("users/get_current_account", &Value::Null).map_err(String::from)?;
        let s = |p: &str| me.pointer(p).and_then(Value::as_str).unwrap_or("").to_string();
        let (root, home) = (s("/root_info/root_namespace_id"), s("/root_info/home_namespace_id"));
        let team = !root.is_empty() && root != home;
        if team {
            api.set_root(Some(&root));
        }
        let account_id = s("/account_id");
        let kept = match (file, api.refresh_token()) {
            (Some(f), Some(token)) => Some((f, Secret::of(&account_id, token))),
            _ => None,
        };
        let lists = match &kept {
            Some((f, secret)) => match cache::read(f, secret) {
                Ok(l) if l.namespace == root => l,
                Ok(_) => {
                    crate::api::log("Dropbox cache: de outra raiz (a conta entrou ou saiu de um time), descartado");
                    cache::forget_file(f);
                    Lists::new(&root)
                }
                Err(cache::Refusal::Missing) => Lists::new(&root),
                Err(why) => {
                    crate::api::log(&format!("Dropbox cache: {why:?}, descartado"));
                    cache::forget_file(f);
                    Lists::new(&root)
                }
            },
            None => Lists::new(&root),
        };
        if !lists.is_empty() {
            crate::api::log(&format!("Dropbox cache: {} pastas guardadas", lists.len()));
        }
        Ok(Account {
            email: normal_email(&s("/email")),
            name: s("/name/display_name"),
            account_id,
            team,
            api,
            lists: Mutex::new(lists),
            log_gate: Mutex::new(()),
            batch_gate: Mutex::new(()),
            caught_up: Mutex::new(None),
            watching: Mutex::new(None),
            pending: Mutex::default(),
            kept,
            saved_at: Mutex::new(None),
            forgotten: AtomicBool::new(false),
            mark,
        })
    }

    /// As if the log had been read long ago (tests, benchmarks).
    #[cfg(test)]
    pub(crate) fn forget_fresh(&self) {
        *self.caught_up.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn lists(&self) -> MutexGuard<'_, Lists> {
        self.lists.lock().unwrap_or_else(|e| e.into_inner())
    }

    // -----------------------------------------------------------------------------------------
    // The log and the kept listings

    /// Takes the log's cursor, if there's none: before any listing it covers.
    fn ensure_log(&self) -> Result<()> {
        if self.lists().log.is_some() {
            return Ok(());
        }
        let _gate = self.log_gate.lock().unwrap_or_else(|e| e.into_inner());
        if self.lists().log.is_some() {
            return Ok(());
        }
        let v = self.api.rpc("files/list_folder/get_latest_cursor", &json!({"path": "", "recursive": true, "include_deleted": false})).map_err(String::from)?;
        let cursor = v.get("cursor").and_then(Value::as_str).ok_or(t("O Dropbox não mandou o cursor das mudanças.", "Dropbox didn't send the changes' cursor."))?;
        self.lists().log = Some(cursor.to_string());
        *self.caught_up.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        Ok(())
    }

    /// Whether the kept listings are known current without asking: the log was just read to
    /// its end, or a longpoll is waiting on its cursor (it would have answered on a change).
    /// A wait past its timeout and the up to 90 s Dropbox may add is no proof: its connection
    /// may be dead.
    fn fresh(&self) -> bool {
        if self.caught_up.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|at| at.elapsed() < FRESH) {
            return true;
        }
        let watching = self.watching.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match watching {
            Some((cursor, since, timeout)) => since.elapsed() < Duration::from_secs(timeout + 90) && Some(cursor) == self.lists().log,
            None => false,
        }
    }

    /// Forgets folder `path`'s kept listing: the next opening reads it whole (Atualizar).
    pub fn forget(&self, path: &str) {
        let mut lists = self.lists();
        lists.drop_folder(&key(path));
    }

    /// Applies `changes` to the listings, noting the folders they touch for the watcher.
    pub(crate) fn note(&self, changes: &[Change]) {
        if changes.is_empty() {
            return;
        }
        let mut touched = HashSet::new();
        {
            let mut lists = self.lists();
            for c in changes {
                lists.apply(c, &mut touched);
            }
        }
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.0.extend(touched);
        if pending.0.len() > MAX_PENDING {
            pending.0.clear();
            pending.1 = true;
        }
    }

    /// Drops every listing and the log: each folder is read whole again when it opens.
    fn start_over(&self, why: &str) {
        crate::api::log(&format!("Dropbox {}: {why}; as pastas serão lidas de novo", self.email));
        self.lists().clear();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
    }

    /// Reads the log to its end and applies it.
    pub fn catch_up(&self) -> Result<()> {
        let _gate = self.log_gate.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut cursor) = self.lists().log.clone() else { return Ok(()) };
        for _ in 0..MAX_LOG_PAGES {
            let v = match self.api.rpc("files/list_folder/continue", &json!({"cursor": cursor})) {
                Ok(v) => v,
                Err(f) if f.is("reset") => {
                    self.start_over("o cursor das mudanças não vale mais (reset)");
                    return Ok(());
                }
                Err(f) => return Err(f.text()),
            };
            let changes: Vec<Change> = v.get("entries").and_then(Value::as_array).map(|a| a.iter().filter_map(change).collect()).unwrap_or_default();
            self.note(&changes);
            cursor = v.get("cursor").and_then(Value::as_str).unwrap_or(&cursor).to_string();
            self.lists().log = Some(cursor.clone());
            if !v.get("has_more").and_then(Value::as_bool).unwrap_or(false) {
                *self.caught_up.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                return Ok(());
            }
        }
        self.start_over("mudanças demais desde a última vez");
        Ok(())
    }

    /// Every item of folder `path`, read whole (`list_folder` and its pages).
    fn read_folder(&self, path: &str) -> Result<Vec<Entry>> {
        let first = self.api.rpc("files/list_folder", &json!({"path": api_path(path), "recursive": false, "include_deleted": false, "limit": 2000})).map_err(|f| {
            if f.is("not_found") { tr!("A pasta “{}” não está mais no Dropbox.", "The folder “{}” is no longer on Dropbox.", last(&api_path(path))) } else { about(path, f) }
        })?;
        let mut entries = Vec::new();
        let mut page = first;
        loop {
            entries.extend(page.get("entries").and_then(Value::as_array).into_iter().flatten().filter_map(entry));
            if !page.get("has_more").and_then(Value::as_bool).unwrap_or(false) {
                return Ok(entries);
            }
            let cursor = page.get("cursor").and_then(Value::as_str).unwrap_or("").to_string();
            page = self.api.rpc("files/list_folder/continue", &json!({"cursor": cursor})).map_err(String::from)?;
        }
    }

    /// A folder's items: its kept listing brought up to date (no request when it's known
    /// current, one small one otherwise), else read whole.
    pub fn list(&self, path: &str) -> Result<Vec<Entry>> {
        let k = key(path);
        self.ensure_log()?;
        if self.lists().get(&k, now()).is_some() {
            if !self.fresh() {
                self.catch_up()?;
            }
            if let Some(f) = self.lists().get(&k, now()) {
                return Ok(f.entries.clone());
            }
            // The log said start over: the log again, then the folder.
            self.ensure_log()?;
        }
        let entries = self.read_folder(path)?;
        if let Some(k) = kept_key(&k, &entries) {
            self.lists().put(&k, entries.clone(), now());
            self.maybe_save();
        }
        Ok(entries)
    }

    /// Reads folder `path` whole and keeps it, whatever is kept (Atualizar).
    pub fn reload(&self, path: &str) -> Result<Vec<Entry>> {
        self.ensure_log()?;
        let entries = self.read_folder(path)?;
        if let Some(k) = kept_key(&key(path), &entries) {
            self.lists().put(&k, entries.clone(), now());
        }
        Ok(entries)
    }

    /// The item at `path`, `None` when there's nothing there. From the kept listing of its
    /// folder when that's known current, else asked.
    pub fn stat(&self, path: &str) -> Result<Option<Entry>> {
        let k = key(path);
        if k.is_empty() {
            return Ok(Some(Entry { name: String::new(), lower: String::new(), dir: true, size: 0, modified: None, rev: String::new(), hash: String::new() }));
        }
        if self.fresh()
            && let Some(f) = self.lists().get(parent(&k), now())
        {
            return Ok(f.entries.iter().find(|e| e.lower == k).cloned());
        }
        match self.api.rpc("files/get_metadata", &json!({"path": api_path(path)})) {
            Ok(v) => Ok(entry(&v)),
            Err(f) if f.is("not_found") => Ok(None),
            Err(f) => Err(about(path, f)),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Changes the panes make

    pub(crate) fn metadata_of(v: &Value) -> Result<Entry> {
        entry(v.get("metadata").unwrap_or(v)).ok_or_else(|| t("O Dropbox respondeu sem os dados do item.", "Dropbox answered without the item's details.").into())
    }

    pub fn mkdir(&self, path: &str) -> Result<Entry> {
        let v = self.api.rpc("files/create_folder_v2", &json!({"path": api_path(path), "autorename": false})).map_err(|f| about(path, f))?;
        let e = Account::metadata_of(&v)?;
        self.note(&[Change::Is(e.clone())]);
        Ok(e)
    }

    /// Renames or moves `from` to `to`. Fails if something is at `to` (but a change of case
    /// alone goes through).
    pub fn rename(&self, from: &str, to: &str) -> Result<Entry> {
        let arg = |a: &str, b: &str| json!({"from_path": api_path(a), "to_path": api_path(b), "autorename": false, "allow_ownership_transfer": false});
        let v = match self.api.rpc("files/move_v2", &arg(from, to)) {
            Ok(v) => v,
            // Only the case changes: by a name of its own on the way.
            Err(f) if f.is("conflict") && key(from) == key(to) => {
                let aside = format!("{}.oe-{}", api_path(from), now());
                self.api.rpc("files/move_v2", &arg(from, &aside)).map_err(|f| about(from, f))?;
                self.api.rpc("files/move_v2", &arg(&aside, to)).map_err(|f| about(to, f))?
            }
            Err(f) => return Err(about(from, f)),
        };
        let e = Account::metadata_of(&v)?;
        self.note(&[Change::Gone(key(from)), Change::Is(e.clone())]);
        Ok(e)
    }

    /// Copies a file or a folder and all it holds, on Dropbox's side.
    pub fn copy(&self, from: &str, to: &str) -> Result<Entry> {
        let v = self.api.rpc("files/copy_v2", &json!({"from_path": api_path(from), "to_path": api_path(to), "autorename": false})).map_err(|f| about(from, f))?;
        let e = Account::metadata_of(&v)?;
        self.note(&[Change::Is(e.clone())]);
        Ok(e)
    }

    /// Deletes a file or a folder and all it holds. Dropbox keeps what's deleted for a while
    /// (see [`Account::restore`]). The item as it was, with its revision.
    pub fn delete(&self, path: &str) -> Result<Entry> {
        let v = self.api.rpc("files/delete_v2", &json!({"path": api_path(path)})).map_err(|f| about(path, f))?;
        let e = Account::metadata_of(&v)?;
        self.note(&[Change::Gone(e.lower.clone())]);
        Ok(e)
    }

    /// Deletes several items in one batch job (`delete_batch`), waiting for it to end: each
    /// item's result, in order.
    pub fn delete_many(&self, paths: &[String]) -> Result<Vec<Result<Entry>>> {
        let mut results = Vec::with_capacity(paths.len());
        // Up to 1,000 entries a job.
        for chunk in paths.chunks(1000) {
            let entries: Vec<Value> = chunk.iter().map(|p| json!({"path": api_path(p)})).collect();
            let v = {
                let _gate = self.batch_gate.lock().unwrap_or_else(|e| e.into_inner());
                let v = self.api.rpc("files/delete_batch", &json!({"entries": entries})).map_err(String::from)?;
                self.wait_job("files/delete_batch/check", v)?
            };
            let answered: Vec<Value> = v.get("entries").and_then(Value::as_array).cloned().unwrap_or_default();
            let done: Vec<Result<Entry>> = chunk
                .iter()
                .enumerate()
                .map(|(i, p)| match answered.get(i).map(|r| (r.get(".tag").and_then(Value::as_str), r)) {
                    Some((Some("success"), r)) => Account::metadata_of(r),
                    Some((_, r)) => {
                        let summary = r.get("failure").map(failure_summary).unwrap_or_default();
                        Err(format!("“{}”: {}", last(&api_path(p)), crate::api::route_text(&summary)))
                    }
                    None => Err(tr!("“{}”: o Dropbox não disse se excluiu.", "“{}”: Dropbox didn't say whether it was deleted.", last(&api_path(p)))),
                })
                .collect();
            let gone: Vec<Change> = done.iter().filter_map(|r| r.as_ref().ok()).map(|e| Change::Gone(e.lower.clone())).collect();
            self.note(&gone);
            results.extend(done);
        }
        Ok(results)
    }

    /// Waits for a batch job that answered `first`: `complete` at once, or an `async_job_id`
    /// asked about at `check` until it ends.
    pub(crate) fn wait_job(&self, check: &str, first: Value) -> Result<Value> {
        let mut v = first;
        let mut pause = Duration::from_millis(100);
        let started = Instant::now();
        let mut id = String::new();
        loop {
            match v.get(".tag").and_then(Value::as_str) {
                Some("complete") => return Ok(v),
                Some("async_job_id") | Some("in_progress") => {}
                Some("failed") => return Err(crate::api::route_text(&v.get("failed").map(failure_summary).unwrap_or_default())),
                _ => return Err(t("O Dropbox respondeu ao lote de um jeito inesperado.", "Dropbox answered the batch in an unexpected way.").into()),
            }
            if let Some(job) = v.get("async_job_id").and_then(Value::as_str) {
                id = job.to_string();
            }
            if started.elapsed() > Duration::from_secs(600) {
                return Err(t("O lote no Dropbox não terminou em 10 minutos.", "The batch on Dropbox didn't end within 10 minutes.").into());
            }
            std::thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_secs(2));
            v = self.api.rpc(check, &json!({"async_job_id": id})).map_err(String::from)?;
        }
    }

    // -----------------------------------------------------------------------------------------
    // What was deleted

    /// Brings back revision `rev` of the file at `path`.
    pub fn restore(&self, path: &str, rev: &str) -> Result<Entry> {
        let v = self.api.rpc("files/restore", &json!({"path": api_path(path), "rev": rev})).map_err(|f| about(path, f))?;
        let e = Account::metadata_of(&v)?;
        self.note(&[Change::Is(e.clone())]);
        Ok(e)
    }

    // -----------------------------------------------------------------------------------------
    // Storage, links, changes

    pub fn quota(&self) -> Result<Quota> {
        let v = self.api.rpc("users/get_space_usage", &Value::Null).map_err(String::from)?;
        let used = v.get("used").and_then(Value::as_u64).unwrap_or(0);
        let a = v.get("allocation").cloned().unwrap_or(Value::Null);
        let team = a.get(".tag").and_then(Value::as_str) == Some("team");
        let total = if team {
            // A cap on this member, when the team sets one; else the team's whole space.
            match a.get("user_within_team_space_allocated").and_then(Value::as_u64) {
                Some(cap) if cap > 0 => cap,
                _ => a.get("allocated").and_then(Value::as_u64).unwrap_or(0),
            }
        } else {
            a.get("allocated").and_then(Value::as_u64).unwrap_or(0)
        };
        Ok(Quota { used, total, team })
    }

    /// The item's shared link, `None` when it has none yet.
    pub fn link_of(&self, path: &str) -> Result<Option<String>> {
        let v = self.api.rpc("sharing/list_shared_links", &json!({"path": api_path(path), "direct_only": true})).map_err(|f| about(path, f))?;
        Ok(v.pointer("/links/0/url").and_then(Value::as_str).map(str::to_string))
    }

    /// Makes a shared link anyone who has it can open (or the one that's there).
    pub fn create_link(&self, path: &str) -> Result<String> {
        match self.api.rpc("sharing/create_shared_link_with_settings", &json!({"path": api_path(path), "settings": {"audience": "public", "access": "viewer"}})) {
            Ok(v) => v.get("url").and_then(Value::as_str).map(str::to_string).ok_or_else(|| t("O Dropbox não mandou o link.", "Dropbox didn't send the link.").into()),
            Err(f) if f.is("shared_link_already_exists") => match f {
                Fail::Route { error, .. } if error.pointer("/shared_link_already_exists/metadata/url").is_some() => Ok(error.pointer("/shared_link_already_exists/metadata/url").and_then(Value::as_str).unwrap_or("").to_string()),
                _ => self.link_of(path)?.ok_or_else(|| t("O Dropbox diz que o link já existe, mas não o mostrou.", "Dropbox says the link exists but didn't show it.").into()),
            },
            Err(f) => Err(about(path, f)),
        }
    }

    /// Takes the item's shared links away. How many there were.
    pub fn remove_link(&self, path: &str) -> Result<usize> {
        let v = self.api.rpc("sharing/list_shared_links", &json!({"path": api_path(path), "direct_only": true})).map_err(|f| about(path, f))?;
        let urls: Vec<String> = v.get("links").and_then(Value::as_array).into_iter().flatten().filter_map(|l| l.get("url").and_then(Value::as_str).map(str::to_string)).collect();
        for url in &urls {
            match self.api.rpc("sharing/revoke_shared_link", &json!({"url": url})) {
                Ok(_) => {}
                Err(f) if f.is("shared_link_not_found") => {}
                Err(f) => return Err(about(path, f)),
            }
        }
        Ok(urls.len())
    }

    /// Waits up to `timeout` seconds (30 to 480) for something to change in the account, without
    /// spending any of its quota of calls (`list_folder/longpoll`), then reads the log: what
    /// changed since the last ask, by this wait or by a folder opening meanwhile.
    pub fn watch(&self, timeout: u64) -> Result<Changes> {
        self.ensure_log()?;
        let cursor = self.lists().log.clone().unwrap_or_default();
        let timeout = timeout.clamp(30, 480);
        *self.watching.lock().unwrap_or_else(|e| e.into_inner()) = Some((cursor.clone(), Instant::now(), timeout));
        let polled = self.api.longpoll(&cursor, timeout);
        *self.watching.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let polled = match polled {
            Ok(v) => v,
            Err(f) if f.is("reset") => {
                self.start_over("o cursor das mudanças não vale mais (reset)");
                Value::Null
            }
            Err(f) => return Err(f.text()),
        };
        if polled.get("changes").and_then(Value::as_bool).unwrap_or(false) {
            self.catch_up()?;
        }
        let (folders, all) = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        self.maybe_save();
        Ok(Changes { folders, all, backoff: polled.get("backoff").and_then(Value::as_u64) })
    }

    // -----------------------------------------------------------------------------------------
    // Keeping the listings

    /// Keeps the listings on disk if they changed and it's been a while.
    fn maybe_save(&self) {
        let due = self.lists().dirty && self.saved_at.lock().unwrap_or_else(|e| e.into_inner()).is_none_or(|at| at.elapsed() >= SAVE_EVERY);
        if due {
            self.save_cache();
        }
    }

    /// Keeps the listings on disk now, if this account keeps them.
    pub fn save_cache(&self) {
        let Some((file, secret)) = &self.kept else { return };
        if self.forgotten.load(Ordering::SeqCst) || cache::cleared_since(self.mark, file) {
            self.lists().dirty = false;
            return;
        }
        let started = Instant::now();
        let body = {
            let mut lists = self.lists();
            lists.dirty = false;
            cache::encode(&lists)
        };
        *self.saved_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        match cache::write_unless(file, secret, &body, &self.forgotten, self.mark) {
            Ok(true) => crate::api::log(&format!("Dropbox cache: {} KB guardados em {} ms", body.len() / 1024, started.elapsed().as_millis())),
            Ok(false) => {}
            Err(e) => {
                crate::api::log(&format!("Dropbox cache: não consegui guardar: {e}"));
                self.lists().dirty = true;
            }
        }
    }

    /// Keeps the listings if they changed since last kept (the app calls it before closing).
    pub fn save_cache_if_changed(&self) {
        if self.lists().dirty {
            self.save_cache();
        }
    }

    /// Ends the login on Dropbox's side: the refresh token stops working.
    pub fn logout(&self) {
        let _ = self.api.rpc("auth/token/revoke", &Value::Null);
    }
}

/// The key a folder's listing is kept under, `None` when it can't be kept: the log names items
/// by Dropbox's own lowercase (`path_lower`), so the key must be that. An ASCII path lowercases
/// the same anywhere; any other is checked against the items' `path_lower`, and a folder with
/// none to check against isn't kept (it's read whole each time it opens, never shown stale).
pub(crate) fn kept_key(asked: &str, entries: &[Entry]) -> Option<String> {
    match entries.first() {
        Some(e) => Some(parent(&e.lower).to_string()),
        None if asked.is_ascii() => Some(asked.to_string()),
        None => None,
    }
}

/// Forgets the listings every open account holds in memory (Limpar cache): each folder is read
/// whole again when it opens.
pub fn forget_kept() {
    let open: Vec<Arc<Account>> = OPEN.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
    for a in open {
        a.lists().clear();
    }
}

/// The summary of a failure object of a batch (`{".tag": "path_lookup", "path_lookup": {...}}`)
/// as the API writes `error_summary`: the tags, joined with `/`.
pub(crate) fn failure_summary(v: &Value) -> String {
    let mut out = String::new();
    let mut at = v;
    while let Some(tag) = at.get(".tag").and_then(Value::as_str) {
        out.push_str(tag);
        out.push('/');
        match at.get(tag) {
            Some(inner) if inner.is_object() => at = inner,
            _ => break,
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The accounts open now

static OPEN: LazyLock<Mutex<HashMap<String, Arc<Account>>>> = LazyLock::new(Mutex::default);
/// One lock per e-mail, so two panes reaching the same account at once open it once.
static OPENING: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = LazyLock::new(Mutex::default);

/// The account open for `email`, if it is.
pub fn cached(email: &str) -> Option<Arc<Account>> {
    OPEN.lock().unwrap_or_else(|e| e.into_inner()).get(&normal_email(email)).cloned()
}

/// The account for `email`: open already, or opened from the refresh token kept in the
/// credential store.
pub fn reach(email: &str) -> Result<Arc<Account>> {
    let email = normal_email(email);
    if let Some(a) = cached(&email) {
        return Ok(a);
    }
    let gate = OPENING.lock().unwrap_or_else(|e| e.into_inner()).entry(email.clone()).or_default().clone();
    let _held = gate.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(a) = cached(&email) {
        return Ok(a);
    }
    let token = store::refresh_token(&email)?;
    let cfg = AppConfig::load()?;
    let account = Arc::new(Account::open(Api::new(Hosts::default(), &cfg.app_key, &token), cache::file_of(&email))?);
    if account.email != email {
        crate::api::log(&format!("Dropbox: a conta {email} agora tem o e-mail {}", account.email));
    }
    OPEN.lock().unwrap_or_else(|e| e.into_inner()).insert(email, account.clone());
    Ok(account)
}

/// Makes `account` the one open for `email` (the app's tests, on the stand-in).
#[cfg(any(test, feature = "fake"))]
pub fn adopt(email: &str, account: Arc<Account>) {
    OPEN.lock().unwrap_or_else(|e| e.into_inner()).insert(normal_email(email), account);
}

/// Opens the account a login just got `tokens` for and keeps its refresh token in the
/// credential store.
pub fn connect(cfg: &AppConfig, tokens: &crate::oauth::Tokens) -> Result<Arc<Account>> {
    let refresh = tokens.refresh_token.as_deref().ok_or(t("O Dropbox não mandou um token de renovação.", "Dropbox didn't send a refresh token."))?;
    let api = Api::logged_in(Hosts::default(), &cfg.app_key, refresh, &tokens.access_token, tokens.expires_in);
    // Opened first: its e-mail names the file and the credential.
    let probe = Account::open(api, None)?;
    let email = probe.email.clone();
    store::set_refresh_token(&email, refresh)?;
    let Account { api, .. } = probe;
    let account = Arc::new(Account::open(api, cache::file_of(&email))?);
    OPEN.lock().unwrap_or_else(|e| e.into_inner()).insert(email, account.clone());
    Ok(account)
}

/// Keeps the listings of each account open that changed since last kept: the app calls it as
/// it closes.
pub fn save_caches() {
    let open: Vec<Arc<Account>> = OPEN.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
    for a in open {
        a.save_cache_if_changed();
    }
}

/// Forgets account `email`: its refresh token leaves the credential store and its listings the
/// disk at once, and the token is revoked on Dropbox's side on a thread of its own.
pub fn disconnect(email: &str) {
    let email = normal_email(email);
    let open = OPEN.lock().unwrap_or_else(|e| e.into_inner()).remove(&email);
    if let Some(a) = &open {
        a.forgotten.store(true, Ordering::SeqCst);
    }
    if let Some(f) = cache::file_of(&email) {
        cache::forget_file(&f);
    }
    let token = match &open {
        Some(_) => None,
        None => store::refresh_token(&email).ok(),
    };
    store::forget_refresh_token(&email);
    std::thread::spawn(move || match (open, token) {
        (Some(a), _) => a.logout(),
        (None, Some(token)) => {
            if let Ok(cfg) = AppConfig::load() {
                let api = Api::new(Hosts::default(), &cfg.app_key, &token);
                let _ = api.rpc("auth/token/revoke", &Value::Null);
            }
        }
        (None, None) => {}
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::Fake;

    #[test]
    fn a_reopened_folder_costs_one_small_request_and_shows_what_changed() {
        let fake = Fake::start();
        fake.put_file("/Fotos/a.jpg", b"aaa");
        fake.put_file("/Fotos/b.jpg", b"bbbb");
        let a = fake.account();
        let names = |v: Vec<Entry>| {
            let mut n: Vec<String> = v.into_iter().map(|e| e.name).collect();
            n.sort();
            n
        };
        assert_eq!(names(a.list("/Fotos").unwrap()), ["a.jpg", "b.jpg"]);
        // Changes made elsewhere reach the kept listing through the log.
        fake.put_file("/Fotos/c.jpg", b"c");
        fake.remove("/Fotos/a.jpg");
        std::thread::sleep(FRESH);
        let before = fake.calls();
        assert_eq!(names(a.list("/Fotos").unwrap()), ["b.jpg", "c.jpg"]);
        assert_eq!(fake.calls_since(&before), ["files/list_folder/continue"]);
        // Just after: known current, no request at all.
        let before = fake.calls();
        a.list("/Fotos").unwrap();
        assert!(fake.calls_since(&before).is_empty());
        // The app's own changes show at once.
        a.mkdir("/Fotos/Nova").unwrap();
        a.rename("/Fotos/b.jpg", "/Fotos/B2.jpg").unwrap();
        assert_eq!(names(a.list("/Fotos").unwrap()), ["B2.jpg", "Nova", "c.jpg"]);
        assert!(a.stat("/Fotos/Nova").unwrap().unwrap().dir);
        assert!(a.stat("/Fotos/b.jpg").unwrap().is_none());
    }

    #[test]
    fn a_reset_cursor_or_a_long_backlog_reads_the_folders_again() {
        let fake = Fake::start();
        fake.put_file("/a.txt", b"1");
        let a = fake.account();
        a.list("/").unwrap();
        fake.reset_cursors();
        fake.put_file("/b.txt", b"2");
        std::thread::sleep(FRESH);
        let names: Vec<String> = a.list("/").unwrap().into_iter().map(|e| e.name).collect();
        assert!(names.contains(&"b.txt".to_string()), "{names:?}");
        // More changes than a few pages: start over rather than read them all.
        a.list("/").unwrap();
        for i in 0..(MAX_LOG_PAGES * 2000 + 10) / 500 {
            fake.put_many(&format!("/lote{i}"), 500);
        }
        std::thread::sleep(FRESH);
        let before = fake.calls();
        let top = a.list("/").unwrap();
        assert!(top.iter().any(|e| e.name == "lote0"));
        let calls = fake.calls_since(&before);
        assert!(calls.contains(&"files/list_folder".to_string()), "{calls:?}");
    }

    #[test]
    fn the_watcher_hears_changes_without_polling_them() {
        let fake = Fake::start();
        fake.put_file("/Docs/x.txt", b"x");
        let a = Arc::new(fake.account());
        a.list("/Docs").unwrap();
        let w = a.clone();
        let waiting = std::thread::spawn(move || w.watch(30).unwrap());
        std::thread::sleep(Duration::from_millis(300));
        // While the longpoll waits, a reopen needs nothing.
        std::thread::sleep(FRESH);
        let before = fake.calls();
        a.list("/Docs").unwrap();
        assert!(fake.calls_since(&before).is_empty());
        fake.put_file("/Docs/y.txt", b"y");
        let changes = waiting.join().unwrap();
        assert!(changes.touches("/Docs") && !changes.touches("/Outra"), "{changes:?}");
        assert!(a.list("/Docs").unwrap().iter().any(|e| e.name == "y.txt"));
    }

    #[test]
    fn listings_are_kept_under_dropbox_s_own_lowercase() {
        let e = |lower: &str| Entry { name: last(lower).into(), lower: lower.into(), dir: false, size: 0, modified: None, rev: String::new(), hash: String::new() };
        // Dropbox's lowercase of "İ" may not be Rust's: the items say which it is.
        assert_eq!(kept_key(&key("/İzmir"), &[e("/izmir/a.txt")]).as_deref(), Some("/izmir"));
        assert_eq!(kept_key("/fotos", &[]).as_deref(), Some("/fotos"));
        assert_eq!(kept_key(&key("/İzmir"), &[]), None);
        assert_eq!(kept_key("", &[e("/a.txt")]).as_deref(), Some(""));
    }

    #[test]
    fn atualizar_reads_the_folder_whole_again() {
        let fake = Fake::start();
        fake.put_file("/a.txt", b"1");
        let a = fake.account();
        a.list("/").unwrap();
        a.forget("/");
        let before = fake.calls();
        a.list("/").unwrap();
        assert!(fake.calls_since(&before).contains(&"files/list_folder".to_string()));
    }

    #[test]
    fn team_accounts_send_their_root() {
        let fake = Fake::start();
        fake.set_team("ns-root", "ns-home");
        let a = fake.account();
        assert!(a.team);
        a.list("/").unwrap();
        assert_eq!(fake.last_root().as_deref(), Some(r#"{".tag":"root","root":"ns-root"}"#));
        let q = a.quota().unwrap();
        assert!(q.team && q.total > 0);
    }

    #[test]
    fn links_are_made_found_and_taken_away() {
        let fake = Fake::start();
        fake.put_file("/f.pdf", b"pdf");
        let a = fake.account();
        assert_eq!(a.link_of("/f.pdf").unwrap(), None);
        let url = a.create_link("/f.pdf").unwrap();
        assert_eq!(a.create_link("/f.pdf").unwrap(), url);
        assert_eq!(a.link_of("/f.pdf").unwrap(), Some(url));
        assert_eq!(a.remove_link("/f.pdf").unwrap(), 1);
        assert_eq!(a.link_of("/f.pdf").unwrap(), None);
    }

    #[test]
    fn waits_as_long_as_dropbox_asks_when_it_says_too_many() {
        let fake = Fake::start();
        fake.throttle(2);
        let a = fake.account();
        assert!(a.list("/").is_ok());
        assert_eq!(fake.throttled(), 2);
    }

    #[test]
    fn listings_survive_a_restart_sealed_and_catch_up_from_their_cursor() {
        let fake = Fake::start();
        fake.put_file("/Fotos/a.jpg", b"a");
        let dir = std::env::temp_dir().join(format!("oe-dropbox-restart-{}", std::process::id()));
        let file = cache::file_in(&dir, "ana@exemplo.com");
        {
            let a = fake.account_kept(&file);
            a.list("/Fotos").unwrap();
            a.save_cache();
        }
        fake.put_file("/Fotos/b.jpg", b"b");
        let a = fake.account_kept(&file);
        let before = fake.calls();
        let names: Vec<String> = a.list("/Fotos").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names.len(), 2);
        assert_eq!(fake.calls_since(&before), ["files/list_folder/continue"]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
