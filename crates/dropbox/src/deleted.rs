//! What was deleted, and bringing it back.
//!
//! Dropbox keeps a deleted file's revisions for a while (30 days to a year, by plan) and
//! restores a file at a revision (`files/restore`); it has no route that restores a folder. What
//! the API does give: `list_folder` with `include_deleted` lists the items deleted from a folder
//! that exists (files and folders alike, with no date and nothing telling one from the other),
//! and `list_revisions` of a deleted file says when it was deleted (`server_deleted`, Dropbox's
//! clock) and its last revision; of a folder it answers `not_file`. A file whose revisions are
//! gone (kept past the plan's days) can't come back, and isn't listed nor taken for a folder.
//!
//! So a folder comes back as the website brings it back: the folder made again, what was
//! deleted in it listed (all the way down), each file deleted in that one deletion restored at
//! its last revision, and the folders deleted with it made again, the empty ones too. "That one
//! deletion" is a window of Dropbox's own clock: around the delete when the app made it (the
//! `Date` of Dropbox's answers before and after it, so this computer's clock plays no part), or,
//! for a folder found among the deleted items, the latest deletion in it: the newest
//! `server_deleted` there and those just before it, no gap longer than [`GAP`] between them (a
//! big folder's files aren't all stamped the same second).
//!
//! A deleted subfolder has no date of its own. It's made again unless every file deleted from it
//! was deleted outside the window (then it went earlier, with them); one that held no file at all
//! can't be dated and is made again.
//!
//! There's no batch route for `list_revisions` or `restore`: they go a few at once, reads
//! ([`READS`]) more than writes ([`WRITES`]), which Dropbox serialises per account anyway.


use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use openexplorer_i18n::tr;
use serde_json::{Value, json};

use crate::account::{Account, about};
use crate::meta::{api_path, key, last, parent};
use crate::Result;

/// `list_revisions` asked at once.
const READS: usize = 4;
/// `restore` and new folders made at once.
const WRITES: usize = 2;
/// The longest pause between two files' `server_deleted` within one deletion found by its dates.
pub const GAP: i64 = 2;
/// Deleted items whose details (file or folder, when, size) the deleted items view asks for.
pub const DETAILS: usize = 300;

/// An item deleted from a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deleted {
    pub name: String,
    /// Its path as Dropbox shows it.
    pub path: String,
    /// A folder; `None` when not asked (past [`DETAILS`]).
    pub dir: Option<bool>,
    /// A file's last revision, size, and when it was deleted (seconds, Dropbox's clock).
    pub rev: Option<String>,
    pub size: Option<u64>,
    pub when: Option<i64>,
}

/// What `list_revisions` says of a deleted path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Found {
    File { rev: String, size: u64, when: i64 },
    /// A folder (`not_file`).
    Folder,
    /// Nothing left to bring back: no revision kept, or Dropbox doesn't know the path.
    Gone,
    /// A file that's there now.
    Live,
}

/// `work` on each of `items`, `at_once` at a time: the results, in order.
fn each<T: Sync, R: Send>(items: &[T], at_once: usize, work: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<R>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..at_once.min(items.len()) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(it) = items.get(i) else { break };
                    let r = work(it);
                    out.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(r);
                }
            });
        }
    });
    out.into_inner().unwrap_or_else(|e| e.into_inner()).into_iter().map(|r| r.expect("every item done")).collect()
}

impl Account {
    /// What `list_revisions` says of `path`.
    pub(crate) fn found(&self, path: &str) -> Result<Found> {
        let v = match self.api.rpc("files/list_revisions", &json!({"path": api_path(path), "mode": "path", "limit": 1})) {
            Ok(v) => v,
            Err(f) if f.is("not_file") => return Ok(Found::Folder),
            Err(f) if f.is("not_found") => return Ok(Found::Gone),
            Err(f) => return Err(about(path, f)),
        };
        if !v.get("is_deleted").and_then(Value::as_bool).unwrap_or(false) {
            return Ok(Found::Live);
        }
        let when = v.get("server_deleted").and_then(Value::as_str).and_then(crate::meta::seconds).unwrap_or(0);
        let rev = v.pointer("/entries/0/rev").and_then(Value::as_str).unwrap_or("").to_string();
        let size = v.pointer("/entries/0/size").and_then(Value::as_u64).unwrap_or(0);
        Ok(if rev.is_empty() { Found::Gone } else { Found::File { rev, size, when } })
    }

    /// The paths (as Dropbox shows them) deleted from folder `path`, all the way down with
    /// `recursive`.
    fn deleted_paths(&self, path: &str, recursive: bool) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        let mut page = self.api.rpc("files/list_folder", &json!({"path": api_path(path), "recursive": recursive, "include_deleted": true, "limit": 2000})).map_err(|f| about(path, f))?;
        loop {
            for it in page.get("entries").and_then(Value::as_array).into_iter().flatten() {
                if it.get(".tag").and_then(Value::as_str) == Some("deleted") {
                    let lower = it.get("path_lower").and_then(Value::as_str).unwrap_or("");
                    let shown = it.get("path_display").and_then(Value::as_str).unwrap_or(lower);
                    if !shown.is_empty() {
                        out.push((shown.to_string(), it.get("name").and_then(Value::as_str).unwrap_or(last(shown)).to_string()));
                    }
                }
            }
            if !page.get("has_more").and_then(Value::as_bool).unwrap_or(false) {
                return Ok(out);
            }
            let cursor = page.get("cursor").and_then(Value::as_str).unwrap_or("").to_string();
            page = self.api.rpc("files/list_folder/continue", &json!({"cursor": cursor})).map_err(String::from)?;
        }
    }

    /// The items deleted from folder `path` (its own, not further down): files with their
    /// revision, size and when, newest first, then folders, then those past [`DETAILS`] not asked
    /// about. Deleted files there again since aren't listed.
    pub fn deleted_in(&self, path: &str) -> Result<Vec<Deleted>> {
        let paths = self.deleted_paths(path, false)?;
        let asked = &paths[..paths.len().min(DETAILS)];
        let found = each(asked, READS, |(p, _)| self.found(p));
        let mut out = Vec::new();
        for (i, (p, name)) in paths.iter().enumerate() {
            let item = |dir, rev, size, when| Deleted { name: name.clone(), path: p.clone(), dir, rev, size, when };
            out.push(match found.get(i) {
                None => item(None, None, None, None),
                Some(Ok(Found::File { rev, size, when })) => item(Some(false), Some(rev.clone()), Some(*size), Some(*when)),
                Some(Ok(Found::Folder)) => item(Some(true), None, None, None),
                Some(Ok(Found::Live | Found::Gone)) => continue,
                Some(Err(e)) => return Err(e.clone()),
            });
        }
        out.sort_by(|a, b| {
            let rank = |d: &Deleted| match d.dir {
                Some(false) => 0,
                Some(true) => 1,
                None => 2,
            };
            rank(a).cmp(&rank(b)).then(b.when.cmp(&a.when)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(out)
    }

    /// Brings back an item [`Account::deleted_in`] listed: a file at its revision, a folder with
    /// what was deleted with it (see the module docs). How many files came back.
    pub fn restore_deleted(&self, item: &Deleted) -> Result<usize> {
        let found = match (&item.dir, &item.rev) {
            (Some(false), Some(rev)) => Found::File { rev: rev.clone(), size: 0, when: 0 },
            (Some(true), _) => Found::Folder,
            _ => self.found(&item.path)?,
        };
        match found {
            Found::File { rev, .. } => self.restore(&item.path, &rev).map(|_| 1),
            Found::Folder => self.restore_folder(&item.path, None),
            Found::Live => Err(tr!("“{}” já está de volta no Dropbox.", "“{}” is already back on Dropbox.", item.name)),
            Found::Gone => Err(tr!("O Dropbox não guarda mais “{}”.", "Dropbox no longer keeps “{}”.", item.name)),
        }
    }

    /// Brings back the folder deleted from `path`, with what was deleted in it in one deletion:
    /// the one within `window` (Dropbox's seconds, both ends in) when given, else the latest
    /// (see the module docs). How many files came back.
    pub fn restore_folder(&self, path: &str, window: Option<(i64, i64)>) -> Result<usize> {
        match self.mkdir(path) {
            Ok(_) => {}
            Err(_) if self.stat(path)?.is_some_and(|e| e.dir) => {}
            Err(e) => return Err(e),
        }
        let paths = self.deleted_paths(path, true)?;
        let found = each(&paths, READS, |(p, _)| self.found(p));
        let mut files: Vec<(String, String, i64)> = Vec::new();
        let mut folders: Vec<String> = Vec::new();
        for ((p, _), f) in paths.iter().zip(found) {
            match f? {
                Found::File { rev, when, .. } => files.push((p.clone(), rev, when)),
                Found::Folder => folders.push(p.clone()),
                Found::Live | Found::Gone => {}
            }
        }
        let (from, to) = match window {
            Some(w) => w,
            None => latest(&files.iter().map(|f| f.2).collect::<Vec<_>>()).unwrap_or((i64::MAX, i64::MIN)),
        };
        let inside = |when: i64| (from..=to).contains(&when);
        // A deleted folder is made again unless every file deleted from it went outside the window.
        let remade: Vec<String> = folders
            .into_iter()
            .filter(|f| {
                let below = format!("{}/", key(f));
                let mut its = files.iter().filter(|(p, ..)| key(p).starts_with(&below)).peekable();
                its.peek().is_none() || its.any(|(.., when)| inside(*when))
            })
            .collect();
        // Only the deepest: making one makes the folders on its way.
        let deepest: Vec<String> = remade.iter().filter(|f| !remade.iter().any(|o| parent(&key(o)) == key(f))).cloned().collect();
        let back: Vec<&(String, String, i64)> = files.iter().filter(|(.., when)| inside(*when)).collect();
        // Files deleted there, none within the window: say how far the nearest was, as a window
        // missed (two clocks not agreeing) would otherwise only show as an empty folder.
        if back.is_empty()
            && let Some((p, _, when)) = files.iter().min_by_key(|(.., w)| (w - from).abs().min((w - to).abs()))
        {
            crate::api::log(&format!("Dropbox restaurar {path}: nenhum dos {} arquivos excluídos foi excluído entre {from} e {to}; o mais perto, {p}, em {when}", files.len()));
        }
        let failed = Mutex::new(Vec::new());
        each(&deepest, WRITES, |f| {
            if let Err(e) = self.mkdir(f)
                && !self.stat(f).ok().flatten().is_some_and(|e| e.dir)
            {
                failed.lock().unwrap_or_else(|e| e.into_inner()).push(e);
            }
        });
        let restored = each(&back, WRITES, |(p, rev, _)| self.restore(p, rev));
        let mut failed = failed.into_inner().unwrap_or_else(|e| e.into_inner());
        failed.extend(restored.iter().filter_map(|r| r.as_ref().err().cloned()));
        let done = restored.iter().filter(|r| r.is_ok()).count();
        match failed.first() {
            None => Ok(done),
            Some(first) => Err(tr!("Restaurados {done} arquivos de “{}”; {} falharam ({first})", "Restored {done} files of “{}”; {} failed ({first})", last(&api_path(path)), failed.len())),
        }
    }
}

/// The latest deletion among `whens` (seconds): the newest and those before it with no gap over
/// [`GAP`] between them, as `(from, to)`.
fn latest(whens: &[i64]) -> Option<(i64, i64)> {
    let mut sorted: Vec<i64> = whens.to_vec();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    sorted.dedup();
    let to = *sorted.first()?;
    let mut from = to;
    for w in &sorted[1..] {
        if from - w > GAP {
            break;
        }
        from = *w;
    }
    Some((from, to))
}

/// The paths of `items` that are folders, deduplicated (used by the view's tests).
#[cfg(test)]
fn dirs(items: &[Deleted]) -> std::collections::HashSet<String> {
    items.iter().filter(|d| d.dir == Some(true)).map(|d| d.name.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::Fake;

    #[test]
    fn a_deletion_is_told_by_its_dates() {
        assert_eq!(latest(&[100, 101, 103, 90, 50]), Some((100, 103)));
        assert_eq!(latest(&[100, 96]), Some((100, 100)));
        assert_eq!(latest(&[]), None);
    }

    #[test]
    fn a_folder_comes_back_with_what_went_with_it_and_its_empty_folders() {
        let fake = Fake::start();
        fake.put_file("/Pasta/a.txt", b"um");
        fake.put_file("/Pasta/velho.txt", b"velho");
        fake.put_file("/Pasta/Sub/b.txt", b"dois");
        fake.put_file("/Pasta/Antiga/c.txt", b"tres");
        fake.make_dir("/Pasta/Vazia/Dentro");
        let a = fake.account();
        // Deleted long before: a file, and a folder with its file.
        a.delete("/Pasta/velho.txt").unwrap();
        a.delete("/Pasta/Antiga").unwrap();
        fake.backdate("/Pasta/velho.txt", 3600);
        fake.backdate("/Pasta/Antiga/c.txt", 3600);
        let from = a.api.server_now() - 2;
        let results = a.delete_many(&["/Pasta".into(), "/nada".into()]).unwrap();
        let to = a.api.server_now() + 1;
        assert!(results[0].is_ok() && results[1].as_ref().unwrap_err().contains("Não encontrado"));
        assert!(!fake.has("/Pasta"));
        assert_eq!(a.restore_folder("/Pasta", Some((from, to))).unwrap(), 2);
        assert_eq!(fake.content("/Pasta/Sub/b.txt"), b"dois");
        assert!(fake.has("/Pasta/a.txt") && fake.has("/Pasta/Vazia/Dentro"));
        assert!(!fake.has("/Pasta/velho.txt") && !fake.has("/Pasta/Antiga"));
    }

    #[test]
    fn a_file_whose_revisions_are_gone_is_neither_listed_nor_made_a_folder() {
        let fake = Fake::start();
        fake.put_file("/P/velho.txt", b"v");
        fake.put_file("/P/novo.txt", b"n");
        let a = fake.account();
        a.delete("/P/velho.txt").unwrap();
        fake.expire("/P/velho.txt");
        a.delete("/P/novo.txt").unwrap();
        let items = a.deleted_in("/P").unwrap();
        assert_eq!(items.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["novo.txt"]);
        a.delete("/P").unwrap();
        assert_eq!(a.restore_folder("/P", None).unwrap(), 1);
        assert!(fake.has("/P/novo.txt") && !fake.has("/P/velho.txt"));
    }

    #[test]
    fn the_deleted_items_view_lists_and_restores_them() {
        let fake = Fake::start();
        fake.put_file("/Docs/fica.txt", b"fica");
        fake.put_file("/Docs/nota.txt", b"nota");
        fake.put_file("/Docs/Fotos/f1.jpg", b"f1");
        fake.put_file("/Docs/Fotos/f2.jpg", b"f2");
        let a = fake.account();
        a.delete("/Docs/nota.txt").unwrap();
        a.delete("/Docs/Fotos/f1.jpg").unwrap();
        fake.backdate("/Docs/Fotos/f1.jpg", 3600);
        a.delete("/Docs/Fotos").unwrap();
        let items = a.deleted_in("/Docs").unwrap();
        assert_eq!(items.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["nota.txt", "Fotos"]);
        assert_eq!((items[0].size, items[0].dir), (Some(4), Some(false)));
        assert!(items[0].when.is_some() && dirs(&items).contains("Fotos"));
        assert_eq!(a.restore_deleted(&items[0]).unwrap(), 1);
        assert_eq!(fake.content("/Docs/nota.txt"), b"nota");
        // The folder with what its deletion took, not the file deleted an hour before.
        assert_eq!(a.restore_deleted(&items[1]).unwrap(), 1);
        assert!(fake.has("/Docs/Fotos/f2.jpg") && !fake.has("/Docs/Fotos/f1.jpg"));
        assert!(a.deleted_in("/Docs").unwrap().iter().all(|d| d.name != "nota.txt"));
    }
}
