//! Searching Drive by name, as fast as Drive itself answers.
//!
//! Drive searches its own index (`files.list` with `name contains`), so the account's size
//! doesn't matter for finding things; what costs is bringing many results over: each page of up
//! to a thousand takes Drive one to two seconds, and a page token only gives the next one. So a
//! search runs as several queries at once, each over a stretch of modification dates, sorted
//! by that date: when a worker is free, the busy stretch with more pages is cut where its last
//! page ended and the rest is shared out. Each page is handed on as it lands, unsorted (the
//! app sorts), with the same file never handed twice.
//!
//! Each word typed must be in the name; Drive matches words by their start ("rel" finds
//! "Relatório 2025"). Where to search narrows the query on Google's side whenever Drive can:
//! one shared drive, or what the account owns for My Drive. Everything is one `allDrives`
//! query: searching each shared drive on its own (`corpora=drive`) is no faster once the
//! dates are split, and Drive's index there misses some files `allDrives` finds. A folder
//! can't be asked for with its subfolders, so its tree is walked for folder ids (only folders,
//! many parents per request, several requests at once) while the search runs, and results keep
//! to that tree: those whose folder is already known in it show at once; when the search ends
//! before the walk, the few left are placed by climbing from their folders up, the many wait
//! for the walk to end.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Condvar, Mutex};

use openexplorer_i18n::{t, tr};
use serde::Deserialize;

use crate::Result;
use crate::api::{Drive, DriveFile, FOLDER_MIME, valid_id};

/// Where a search looks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Within {
    /// Everything the account reaches: My Drive, shared with it, shared drives.
    Everything,
    /// My Drive: what the account owns.
    MyDrive,
    /// One shared drive, whole.
    SharedDrive(String),
    /// A folder and everything under it; `shared_drive` when it sits in one.
    Folder { id: String, shared_drive: Option<String> },
}

/// Queries running at once in a search.
const SEARCHERS: usize = 16;
/// Parents asked for in one folder-walk request.
const PARENTS_PER_REQUEST: usize = 30;
/// Folder-walk requests at the same time.
const WALKERS: usize = 6;
/// Page size when asking for the folders of several parents at once: Drive answers HTTP 500
/// ("Internal Error") to `'a' in parents or 'b' in parents ...` with pages over about 500
/// when the parents hold that many (measured: 500 works, 700 fails). One parent takes 1000.
const PAGE_OF_MANY_PARENTS: &str = "500";
/// The most folders of results still waiting on the walk that are placed by climbing from them
/// (a request per folder on the way up) rather than by walking on.
const CLIMB_AT_MOST: usize = 300;
/// How deep Drive nests folders: a climb any longer is going round in circles.
const DEEPEST_FOLDER: usize = 100;
/// The first results, asked for apart: a short unsorted page comes back in a fraction of the
/// time of a full sorted one.
const GLIMPSE: &str = "100";
/// The shortest stretch of dates worth cutting in two, in milliseconds.
const NARROWEST: i64 = 1000;

/// The `q` of a search: every word in the name, trash left out. `None` when nothing was typed.
/// Quotes and backslashes are escaped, so what's typed can't change the query.
pub fn query(text: &str) -> Option<String> {
    let words: Vec<String> = text.split_whitespace().map(|w| format!("name contains {}", crate::api::quoted(w))).collect();
    (!words.is_empty()).then(|| format!("{} and trashed = false", words.join(" and ")))
}

/// `'a' in parents or 'b' in parents ...`, for ids already checked by [`valid_id`] or from Drive.
fn in_parents(folders: &[String]) -> String {
    folders.iter().map(|p| format!("'{p}' in parents")).collect::<Vec<_>>().join(" or ")
}

/// A result, with the folders it sits in when a folder search asked for them.
#[derive(Deserialize)]
struct Hit {
    #[serde(flatten)]
    file: crate::api::RawFile,
    #[serde(default)]
    parents: Vec<String>,
}

#[derive(Deserialize)]
struct FolderId {
    id: String,
}

/// What a result needs to list. `parents` is asked for only when a folder search needs it:
/// with it, Drive sends at most 460 files a page instead of a thousand.
const FIELDS: &str = "nextPageToken,files(id,name,mimeType,size,modifiedTime,md5Checksum,starred,driveId,ownedByMe,shortcutDetails(targetId,targetMimeType))";
const FIELDS_WITH_PARENTS: &str = "nextPageToken,files(id,name,mimeType,size,modifiedTime,md5Checksum,starred,driveId,ownedByMe,parents,shortcutDetails(targetId,targetMimeType))";

/// Which of Drive's indexes a query runs on: `corpora=user` (the account's files and those
/// shared with it), `allDrives` (that and every shared drive), or one shared drive.
#[derive(Clone, Debug, PartialEq)]
enum Corpus {
    User,
    All,
    Drive(String),
}

impl Corpus {
    fn params<'a>(&'a self, query: &mut Vec<(&str, &'a str)>) {
        match self {
            Corpus::User => query.push(("corpora", "user")),
            Corpus::All => query.push(("corpora", "allDrives")),
            Corpus::Drive(d) => query.extend([("corpora", "drive"), ("driveId", d.as_str())]),
        }
    }
}

/// The results in a page of `files.list`.
fn hits(v: &mut serde_json::Value) -> Result<Vec<Hit>> {
    serde_json::from_value(v.get_mut("files").map(serde_json::Value::take).unwrap_or_default()).map_err(|e| tr!("Pesquisa: resposta inesperada: {e}", "Search: unexpected answer: {e}"))
}

/// A stretch of modification dates in one corpus, `from` included, `to` not; open ends unbounded.
#[derive(Clone, Debug, PartialEq)]
struct Stretch {
    corpus: Corpus,
    from: Option<String>,
    to: Option<String>,
}

/// The stretches waiting and how many workers hold one.
struct Pool {
    jobs: VecDeque<Stretch>,
    busy: usize,
}

/// Milliseconds since 1970 of a Drive time ("2024-05-01T12:00:00.000Z").
fn millis(t: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(t).ok().map(|d| d.timestamp_millis())
}

fn drive_time(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()).unwrap_or_default()
}

/// What's left of `s` past `last` (the date its last page ended on), cut in up to `parts`
/// stretches of equal dates; `None` when it's too narrow to cut. The first starts at `last`
/// itself, as files sharing that date may not all have come; the repeats are dropped later.
fn cut(s: &Stretch, last: &str, parts: usize, now: i64) -> Option<Vec<Stretch>> {
    let lo = millis(last)?;
    let hi = match &s.to {
        Some(t) => millis(t)?,
        None => now.max(lo),
    };
    let parts = (parts as i64).min((hi - lo) / NARROWEST);
    if parts < 2 {
        return None;
    }
    let mut bounds = vec![Some(last.to_string())];
    bounds.extend((1..parts).map(|i| Some(drive_time(lo + (hi - lo) * i / parts))));
    bounds.push(s.to.clone());
    Some(bounds.windows(2).map(|w| Stretch { corpus: s.corpus.clone(), from: w[0].clone(), to: w[1].clone() }).collect())
}

impl Drive {
    /// Files and folders whose name has every word of `text`, in `within`, handed to `each` a
    /// page at a time like [`Drive::list`]; `false` from `each` stops it.
    pub fn search(&self, text: &str, within: &Within, each: impl FnMut(Vec<DriveFile>, bool) -> bool) -> Result<()> {
        self.search_with_workers(text, within, SEARCHERS, each)
    }

    /// Like [`Drive::search`], capping parallel date-stretch workers at `max_workers`.
    pub fn search_with_workers(&self, text: &str, within: &Within, max_workers: usize, mut each: impl FnMut(Vec<DriveFile>, bool) -> bool) -> Result<()> {
        let Some(mut q) = query(text) else {
            each(Vec::new(), false);
            return Ok(());
        };
        let corpus = match within {
            Within::SharedDrive(d) | Within::Folder { shared_drive: Some(d), .. } => {
                if !valid_id(d) {
                    return Err(tr!("Id de drive compartilhado inválido: {d}", "Invalid shared drive id: {d}"));
                }
                Corpus::Drive(d.clone())
            }
            Within::MyDrive => {
                q.push_str(" and 'me' in owners");
                Corpus::User
            }
            // A folder outside shared drives can hold what others shared into it.
            Within::Folder { shared_drive: None, .. } | Within::Everything => Corpus::All,
        };
        let start = Stretch { corpus, from: None, to: None };
        let Within::Folder { id, shared_drive } = within else {
            let mut seen = HashSet::new();
            return self.search_all(&q, FIELDS, start, max_workers, &AtomicBool::new(false), |hits, more| each(hits.into_iter().filter(|h| seen.insert(h.file.id.clone())).map(|h| h.file.into()).collect(), more));
        };
        if !valid_id(id) {
            return Err(tr!("Id de pasta inválido: {id}", "Invalid folder id: {id}"));
        }
        // The folder's tree is walked while the search runs. (Asking for the words in the tree's
        // folders, `'id' in parents`, would spare paging through every match in Drive, but Drive
        // then matches only the start of the whole name: "a" finds "aio", not "Kit a".)
        let tree: Mutex<HashSet<String>> = Mutex::new(HashSet::from([id.clone()]));
        let walked = AtomicBool::new(false);
        // Stops the search and the walk: the caller left, or something failed.
        let stop = AtomicBool::new(false);
        let mut left = false;
        std::thread::scope(|scope| {
            let walk = scope.spawn(|| {
                let out = self.walk_folders(id, shared_drive.as_deref(), &tree, &stop);
                walked.store(true, Ordering::Release);
                // A failed walk stops the search too: its results couldn't be kept to the tree.
                if out.is_err() {
                    stop.store(true, Ordering::Relaxed);
                }
                out
            });
            let mut seen = HashSet::new();
            // Results whose folder isn't known to be in the tree yet.
            let mut waiting: Vec<Hit> = Vec::new();
            let searched = self.search_all(&q, FIELDS_WITH_PARENTS, start, max_workers, &stop, |hits, more| {
                let known = tree.lock().unwrap();
                let complete = walked.load(Ordering::Acquire);
                let mut now = Vec::new();
                for h in hits {
                    if !seen.insert(h.file.id.clone()) {
                        continue;
                    }
                    if h.parents.iter().any(|p| known.contains(p)) {
                        now.push(h.file.into());
                    } else if !complete {
                        waiting.push(h);
                    }
                }
                drop(known);
                // The last page waits for the walk, which says what's left.
                if !now.is_empty() && !each(now, true) {
                    left = true;
                    return false;
                }
                more
            });
            // `stop` set by now means the walk failed or the caller left: the search may have been
            // cut short, and only the walk's outcome tells.
            let whole = searched.is_ok() && !stop.load(Ordering::Relaxed);
            // Few results still waiting are placed by climbing from their folders instead: far
            // fewer requests than the rest of a big tree's walk.
            let mut climbed = None;
            if whole && !waiting.is_empty() && !walked.load(Ordering::Acquire) {
                let known = tree.lock().unwrap();
                let folders: HashSet<String> = waiting.iter().flat_map(|h| h.parents.iter()).filter(|p| !known.contains(*p)).cloned().collect();
                drop(known);
                if folders.len() <= CLIMB_AT_MOST {
                    climbed = self.climb(folders, shared_drive.as_deref(), &tree);
                }
            }
            let placed = whole && (waiting.is_empty() || climbed.is_some());
            // With every result placed, what's left of the walk would decide nothing.
            if placed {
                stop.store(true, Ordering::Relaxed);
            }
            let walk = walk.join().unwrap_or_else(|_| Err(t("Erro interno ao percorrer a pasta.", "Internal error going through the folder.").into()));
            stop.store(true, Ordering::Relaxed);
            if !placed {
                walk?;
            }
            searched?;
            if !left {
                let known = tree.lock().unwrap();
                let inside = |p: &String| known.contains(p) || climbed.as_ref().is_some_and(|c| c.contains(p));
                let rest: Vec<DriveFile> = waiting.into_iter().filter(|h| h.parents.iter().any(inside)).map(|h| h.file.into()).collect();
                each(rest, false);
            }
            Ok(())
        })
    }

    /// Runs `q` over the `start` stretch with up to `max_workers` workers, cutting busy stretches for idle
    /// ones (see [`cut`]), and hands every page to `each` on this thread as it lands, `more`
    /// until the last; `false` from `each`, or `stop` set from outside, stops it (with no last
    /// page). Files on a cut can come twice: `each` drops repeats.
    fn search_all(&self, q: &str, fields: &str, start: Stretch, max_workers: usize, stop: &AtomicBool, mut each: impl FnMut(Vec<Hit>, bool) -> bool) -> Result<()> {
        // OE_SEARCHERS: a test switch, to time other counts.
        let searchers = std::env::var("OE_SEARCHERS").ok().and_then(|v| v.parse().ok()).filter(|n| *n > 0).unwrap_or_else(|| max_workers.max(1));
        let pool = Mutex::new(Pool { jobs: VecDeque::from([start]), busy: 0 });
        let ready = Condvar::new();
        let (tx, rx) = channel::<Result<Vec<Hit>>>();
        std::thread::scope(|scope| {
            // A glimpse, while the full search starts: its files come again later and are
            // dropped then. A failure here is the full search's to report.
            let glimpse = tx.clone();
            let corpus = pool.lock().unwrap().jobs[0].corpus.clone();
            scope.spawn(move || {
                let mut query = vec![("q", q), ("fields", fields), ("pageSize", GLIMPSE), ("supportsAllDrives", "true"), ("includeItemsFromAllDrives", "true")];
                corpus.params(&mut query);
                if let Ok(page) = self.get("/files", &query, t("Pesquisa", "Search")).and_then(|mut v| hits(&mut v)) {
                    let _ = glimpse.send(Ok(page));
                }
            });
            for _ in 0..searchers {
                let tx = tx.clone();
                let (pool, ready) = (&pool, &ready);
                scope.spawn(move || {
                    loop {
                        let job = {
                            let mut p = pool.lock().unwrap();
                            loop {
                                if stop.load(Ordering::Relaxed) {
                                    return;
                                }
                                if let Some(j) = p.jobs.pop_front() {
                                    p.busy += 1;
                                    break j;
                                }
                                if p.busy == 0 {
                                    ready.notify_all();
                                    return;
                                }
                                p = ready.wait(p).unwrap();
                            }
                        };
                        if let Err(e) = self.search_stretch(q, fields, &job, searchers, pool, stop, &tx) {
                            let _ = tx.send(Err(e));
                            stop.store(true, Ordering::Relaxed);
                        }
                        pool.lock().unwrap().busy -= 1;
                        ready.notify_all();
                    }
                });
            }
            drop(tx);
            let mut error = None;
            // Pages go on as they land; an empty one ends it, as only then is it known which was last.
            for got in rx {
                match got {
                    Ok(page) => {
                        if !stop.load(Ordering::Relaxed) && !each(page, true) {
                            stop.store(true, Ordering::Relaxed);
                            ready.notify_all();
                        }
                    }
                    Err(e) => {
                        error.get_or_insert(e);
                        ready.notify_all();
                    }
                }
            }
            if let Some(e) = error {
                return Err(e);
            }
            if !stop.load(Ordering::Relaxed) {
                each(Vec::new(), false);
            }
            Ok(())
        })
    }

    /// Pages through one stretch, oldest first, sending each page on; when a worker is idle and
    /// more pages remain, cuts what's left for the pool and returns.
    #[allow(clippy::too_many_arguments)]
    fn search_stretch(&self, q: &str, fields: &str, s: &Stretch, searchers: usize, pool: &Mutex<Pool>, stop: &AtomicBool, tx: &Sender<Result<Vec<Hit>>>) -> Result<()> {
        let mut q = q.to_string();
        if let Some(f) = &s.from {
            q.push_str(&format!(" and modifiedTime >= '{f}'"));
        }
        if let Some(t) = &s.to {
            q.push_str(&format!(" and modifiedTime < '{t}'"));
        }
        let mut params = vec![("q", q.as_str()), ("fields", fields), ("pageSize", "1000"), ("orderBy", "modifiedTime"), ("supportsAllDrives", "true"), ("includeItemsFromAllDrives", "true")];
        s.corpus.params(&mut params);
        let mut token: Option<String> = None;
        while !stop.load(Ordering::Relaxed) {
            let mut query = params.clone();
            if let Some(t) = &token {
                query.push(("pageToken", t.as_str()));
            }
            let mut v = self.get("/files", &query, t("Pesquisa", "Search"))?;
            let page = hits(&mut v)?;
            token = v.get("nextPageToken").and_then(|t| t.as_str()).map(str::to_string);
            let (first, last) = (page.first().and_then(|h| h.file.modified_time.clone()), page.last().and_then(|h| h.file.modified_time.clone()));
            let _ = tx.send(Ok(page));
            if token.is_none() {
                return Ok(());
            }
            // A page all of one date can't be cut there: go on with the token.
            let (Some(first), Some(last)) = (first, last) else { continue };
            if first == last {
                continue;
            }
            let mut p = pool.lock().unwrap();
            let idle = searchers.saturating_sub(p.busy);
            if p.jobs.is_empty()
                && idle > 0
                && let Some(parts) = cut(s, &last, idle + 1, chrono::Utc::now().timestamp_millis())
            {
                p.jobs.extend(parts);
                return Ok(());
            }
        }
        Ok(())
    }

    /// The folders among `folders` and on their way up that are inside `tree`, told by climbing
    /// from each through its parents until a folder known to be inside or the top of the drive,
    /// several at once. `tree` is only read: the walk, still running, adds to it what it finds.
    /// `None` when climbing can't tell: a request failed, a folder has several parents (only the
    /// walk follows them all), or the way up is deeper than Drive nests folders.
    fn climb(&self, folders: HashSet<String>, shared_drive: Option<&str>, tree: &Mutex<HashSet<String>>) -> Option<HashSet<String>> {
        let folders: Vec<String> = folders.into_iter().collect();
        let inside = Mutex::new(HashSet::new());
        let outside: Mutex<HashSet<String>> = Mutex::new(shared_drive.map(str::to_string).into_iter().collect());
        let at = AtomicUsize::new(0);
        let failed = AtomicBool::new(false);
        std::thread::scope(|scope| {
            for _ in 0..SEARCHERS.min(folders.len()) {
                scope.spawn(|| {
                    while let Some(start) = folders.get(at.fetch_add(1, Ordering::Relaxed)) {
                        let mut way = Vec::new();
                        let mut folder = start.clone();
                        let is_inside = loop {
                            if failed.load(Ordering::Relaxed) {
                                return;
                            }
                            if tree.lock().unwrap().contains(&folder) || inside.lock().unwrap().contains(&folder) {
                                break true;
                            }
                            if outside.lock().unwrap().contains(&folder) || !valid_id(&folder) {
                                break false;
                            }
                            let got = self.get(&format!("/files/{folder}"), &[("fields", "parents"), ("supportsAllDrives", "true")], t("Pastas da pesquisa", "Search folders"));
                            let parents: Option<Vec<String>> = got.ok().map(|mut v| v.get_mut("parents").map(serde_json::Value::take).and_then(|p| serde_json::from_value(p).ok()).unwrap_or_default());
                            let Some(parents) = parents.filter(|p| p.len() <= 1 && way.len() < DEEPEST_FOLDER) else {
                                failed.store(true, Ordering::Relaxed);
                                return;
                            };
                            way.push(std::mem::replace(&mut folder, parents.into_iter().next().unwrap_or_default()));
                        };
                        if is_inside { inside.lock().unwrap().extend(way) } else { outside.lock().unwrap().extend(way) }
                    }
                });
            }
        });
        (!failed.into_inner()).then(|| inside.into_inner().unwrap())
    }

    /// Adds every folder under `root` to `tree`, a level at a time: each request asks for the
    /// folders in up to [`PARENTS_PER_REQUEST`] parents, [`WALKERS`] requests at once.
    fn walk_folders(&self, root: &str, shared_drive: Option<&str>, tree: &Mutex<HashSet<String>>, stop: &AtomicBool) -> Result<()> {
        let corpus = shared_drive.map_or(Corpus::All, |d| Corpus::Drive(d.to_string()));
        let corpus = &corpus;
        // The folders in `parents`, into `tree` and `next`.
        let folders_in = |parents: &[String], next: &Mutex<Vec<String>>| {
            let q = format!("({}) and mimeType = '{FOLDER_MIME}' and trashed = false", in_parents(parents));
            let size = if parents.len() == 1 { "1000" } else { PAGE_OF_MANY_PARENTS };
            let mut query = vec![("q", q.as_str()), ("fields", "nextPageToken,files(id)"), ("pageSize", size), ("supportsAllDrives", "true"), ("includeItemsFromAllDrives", "true")];
            corpus.params(&mut query);
            self.pages("/files", &query, "files", t("Pastas da pesquisa", "Search folders"), |found: Vec<FolderId>, _| {
                let mut tree = tree.lock().unwrap();
                let fresh: Vec<String> = found.into_iter().map(|f| f.id).filter(|id| tree.insert(id.clone())).collect();
                drop(tree);
                next.lock().unwrap().extend(fresh);
                !stop.load(Ordering::Relaxed)
            })
        };
        let mut level = vec![root.to_string()];
        while !level.is_empty() && !stop.load(Ordering::Relaxed) {
            // Few parents are spread over every walker rather than asked for together.
            let per = level.len().div_ceil(WALKERS).clamp(1, PARENTS_PER_REQUEST);
            let batches: Vec<&[String]> = level.chunks(per).collect();
            let next = Mutex::new(Vec::new());
            let error = Mutex::new(None);
            let at = AtomicUsize::new(0);
            std::thread::scope(|scope| {
                for _ in 0..WALKERS.min(batches.len()) {
                    scope.spawn(|| {
                        while let Some(batch) = batches.get(at.fetch_add(1, Ordering::Relaxed)) {
                            if stop.load(Ordering::Relaxed) || error.lock().unwrap().is_some() {
                                return;
                            }
                            let mut got = folders_in(batch, &next);
                            // Drive can fail a query on many parents with a server error even so:
                            // each parent is then asked for on its own, which is what fails for
                            // real (what came already is in `tree` and comes again only as
                            // repeats, which are dropped).
                            if batch.len() > 1 && got.is_err() {
                                crate::log(&format!("pastas de {} pais falharam, uma a uma", batch.len()));
                                got = batch.iter().try_for_each(|p| folders_in(std::slice::from_ref(p), &next));
                            }
                            if let Err(e) = got {
                                error.lock().unwrap().get_or_insert(e);
                            }
                        }
                    });
                }
            });
            if let Some(e) = error.into_inner().unwrap() {
                return Err(e);
            }
            level = next.into_inner().unwrap();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_word_must_be_in_the_name_and_quotes_cant_break_out() {
        assert_eq!(query("  "), None);
        assert_eq!(query("relatório 2025").unwrap(), "name contains 'relatório' and name contains '2025' and trashed = false");
        assert_eq!(query(r"d'água \x").unwrap(), r"name contains 'd\'água' and name contains '\\x' and trashed = false");
    }

    /// The parallel search finds what one plain `allDrives` query pages through.
    /// `OE_SEARCH=a cargo test -p openexplorer-gdrive --release same_results_as_one_query -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn same_results_as_one_query() {
        use crate::{ClientConfig, store};
        use std::collections::HashMap;
        let Ok(text) = std::env::var("OE_SEARCH") else { return };
        let account = store::accounts().into_iter().next().unwrap();
        let d = Drive::new(ClientConfig::load().unwrap(), store::refresh_token(&account.email).unwrap());
        let q = query(&text).unwrap();
        let fields = "nextPageToken,files(id,name,modifiedTime,driveId,owners(emailAddress),shared,ownedByMe)";
        let p = [("q", q.as_str()), ("fields", fields), ("pageSize", "1000"), ("supportsAllDrives", "true"), ("includeItemsFromAllDrives", "true"), ("corpora", "allDrives")];
        let mut plain: HashMap<String, serde_json::Value> = HashMap::new();
        d.pages("/files", &p, "files", "plain", |page: Vec<serde_json::Value>, _| {
            for f in page {
                plain.insert(f["id"].as_str().unwrap().to_string(), f);
            }
            true
        })
        .unwrap();
        let mut ours = HashSet::new();
        d.search(&text, &Within::Everything, |page, _| {
            ours.extend(page.into_iter().map(|f| f.id));
            true
        })
        .unwrap();
        println!("uma consulta: {}  paralela: {}", plain.len(), ours.len());
        assert!(plain.len() == ours.len() && plain.keys().all(|id| ours.contains(id)));
        for (id, f) in plain.iter().filter(|(id, _)| !ours.contains(*id)).take(20) {
            println!("  só na consulta: {id} {f}");
        }
        for id in ours.iter().filter(|id| !plain.contains_key(*id)).take(20) {
            println!("  só na paralela: {id}");
        }
    }

    /// What's left of a stretch is cut in equal dates from where its last page ended (that date
    /// included), keeping its open end; a stretch too narrow, or a bad date, isn't cut.
    #[test]
    fn a_busy_stretch_is_cut_from_its_last_page_on() {
        let open = Stretch { corpus: Corpus::User, from: None, to: None };
        let now = millis("2024-01-01T00:00:00.000Z").unwrap();
        let parts = cut(&open, "2020-01-01T00:00:00.000Z", 4, now).unwrap();
        let bounds: Vec<(Option<&str>, Option<&str>)> = parts.iter().map(|p| (p.from.as_deref(), p.to.as_deref())).collect();
        assert_eq!(
            bounds,
            [
                // 1461 days in four, 2020 being a leap year.
                (Some("2020-01-01T00:00:00.000Z"), Some("2020-12-31T06:00:00.000Z")),
                (Some("2020-12-31T06:00:00.000Z"), Some("2021-12-31T12:00:00.000Z")),
                (Some("2021-12-31T12:00:00.000Z"), Some("2022-12-31T18:00:00.000Z")),
                (Some("2022-12-31T18:00:00.000Z"), None),
            ]
        );
        let shut = Stretch { corpus: Corpus::Drive("0AA".into()), from: None, to: Some("2020-01-01T00:00:01.500Z".into()) };
        assert_eq!(cut(&shut, "2020-01-01T00:00:00.000Z", 8, now), None, "1.5 s holds one narrowest stretch");
        let shut = Stretch { to: Some("2020-01-01T00:00:03.000Z".into()), ..shut };
        let parts = cut(&shut, "2020-01-01T00:00:00.000Z", 8, now).unwrap();
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.corpus == shut.corpus));
        assert_eq!(parts.last().unwrap().to, shut.to);
        assert_eq!(cut(&open, "ontem", 4, now), None);
        // A last page dated after now leaves nothing to cut in an open stretch.
        assert_eq!(cut(&open, "2030-01-01T00:00:00.000Z", 4, now), None);
    }

    #[test]
    fn search_fields_include_drive_origin_without_slowing_general_pages_with_parents() {
        assert!(FIELDS.contains("driveId"));
        assert!(FIELDS.contains("ownedByMe"));
        assert!(!FIELDS.contains("parents"));
        assert!(FIELDS_WITH_PARENTS.contains("driveId"));
        assert!(FIELDS_WITH_PARENTS.contains("ownedByMe"));
        assert!(FIELDS_WITH_PARENTS.contains("parents"));

        let raw: crate::api::RawFile = serde_json::from_str(
            r#"{"id":"f1","name":"ep1.mkv","mimeType":"video/x-matroska","driveId":"0AA","ownedByMe":false}"#,
        )
        .unwrap();
        let file: DriveFile = raw.into();
        assert_eq!(file.drive_id.as_deref(), Some("0AA"));
        assert_eq!(file.owned_by_me, Some(false));
    }
}
