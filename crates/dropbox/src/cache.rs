//! The folders' listings an account read, kept current from the account's change log, in
//! memory and sealed on disk between runs.
//!
//! # Why a reopened folder costs no listing
//!
//! Each account follows one cursor of `list_folder` over its whole tree (`recursive`, taken
//! with `get_latest_cursor`, which lists nothing): `list_folder/continue` on it answers with
//! every item changed since, wherever it is. Every kept listing is valid at that cursor's
//! position, so reading the log and applying what it says (an item that is, or isn't, at a path)
//! keeps them all current at once; a folder whose listing is kept opens with the one small
//! `continue` that brings the log up to date, and none when the longpoll watcher just did. The
//! cursor is taken before the first listing, so nothing that happens while a folder is being
//! read is missed: applying a change twice leaves the same listing.
//!
//! The log is dropped, and with it every listing (each is read whole again when next opened),
//! when Dropbox says the cursor is no longer good (`reset`), when it has more pages of changes
//! than a few listings would take, and when a listing is over a day old.
//!
//! # On disk
//!
//! One file per account, `<hash of the e-mail>.lists`, in the folder the app sets with
//! [`set_dir`] (`%LOCALAPPDATA%\OpenExplorer\dropbox`; with none set, nothing is kept). Written
//! whole and atomically (a temporary file renamed over the old one), from time to time and as the
//! app closes:
//!
//! ```text
//! magic "OEDBXLS\0" | format (u32 LE) | key id (16) | nonce (12) | AES-256-GCM(JSON body) | tag (16)
//! ```
//!
//! The body holds the log's cursor, the root namespace it was taken in and each folder's
//! listing. Names are private, so the file is sealed with a key derived (HKDF-SHA256) from the
//! account's refresh token, salted with its account id: the token only comes back from the OS
//! credential store, so the file alone opens nothing, and a new login (a new token) can't open an
//! old file: it's thrown away. The header is authenticated with the body.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use openexplorer_cloud::sealed::{self, OnClear, Store};
use serde::{Deserialize, Serialize};

pub use openexplorer_cloud::sealed::Refusal;

use crate::meta::{Change, Entry, parent};

/// Bumped when the body's shape changes: a file of another format is thrown away.
pub const FORMAT: u32 = 1;

/// The files, `<hash of the e-mail>.lists`. A clear empties the listings the open accounts hold
/// too (`account::forget_kept`): each folder is read whole again when it opens.
static STORE: Store = Store::new(b"OEDBXLS\0", FORMAT, "lists", OnClear::Forget(crate::account::forget_kept));

/// At most this many folders' listings are kept, and this many items in all: past either, the
/// least recently opened go.
pub const MAX_FOLDERS: usize = 4000;
pub const MAX_ENTRIES: usize = 400_000;
/// A listing older than this is read whole again rather than trusted.
pub const MAX_AGE: i64 = 24 * 3600;

/// One folder's items, and when they were read whole (seconds since 1970).
#[derive(Clone, Debug, PartialEq)]
pub struct Folder {
    pub entries: Vec<Entry>,
    pub fetched: i64,
    used: u64,
}

/// An account's kept listings and the change log they're valid at.
#[derive(Debug, Default)]
pub struct Lists {
    folders: HashMap<String, Folder>,
    /// The cursor of the account's change log.
    pub log: Option<String>,
    /// The root namespace the cursor and listings belong to.
    pub namespace: String,
    tick: u64,
    total: usize,
    /// Changed since last kept on disk.
    pub dirty: bool,
}

impl Lists {
    pub fn new(namespace: &str) -> Lists {
        Lists { namespace: namespace.into(), ..Lists::default() }
    }

    pub fn len(&self) -> usize {
        self.folders.len()
    }

    pub fn is_empty(&self) -> bool {
        self.folders.is_empty()
    }

    /// Folder `key`'s listing, if kept and not too old at `now`.
    pub fn get(&mut self, key: &str, now: i64) -> Option<&Folder> {
        self.tick += 1;
        let tick = self.tick;
        let f = self.folders.get_mut(key)?;
        if now - f.fetched > MAX_AGE {
            return None;
        }
        f.used = tick;
        Some(f)
    }

    /// Keeps folder `key`'s listing, read whole at `now`.
    pub fn put(&mut self, key: &str, entries: Vec<Entry>, now: i64) {
        self.tick += 1;
        if let Some(old) = self.folders.remove(key) {
            self.total -= old.entries.len();
        }
        self.total += entries.len();
        self.folders.insert(key.to_string(), Folder { entries, fetched: now, used: self.tick });
        self.dirty = true;
        self.evict();
    }

    fn evict(&mut self) {
        while self.folders.len() > MAX_FOLDERS || self.total > MAX_ENTRIES && self.folders.len() > 1 {
            let Some(oldest) = self.folders.iter().min_by_key(|(_, f)| f.used).map(|(k, _)| k.clone()) else { return };
            if let Some(f) = self.folders.remove(&oldest) {
                self.total -= f.entries.len();
            }
        }
    }

    /// Forgets every listing and the log: they'll be read again.
    pub fn clear(&mut self) {
        self.folders.clear();
        self.total = 0;
        self.log = None;
        self.dirty = true;
    }

    /// Forgets folder `key`'s listing alone.
    pub fn drop_folder(&mut self, key: &str) {
        if let Some(f) = self.folders.remove(key) {
            self.total -= f.entries.len();
            self.dirty = true;
        }
    }

    /// Forgets the listings of `lower` and of every folder in it.
    fn drop_under(&mut self, lower: &str) {
        let inside = format!("{lower}/");
        let gone: Vec<String> = self.folders.keys().filter(|k| k.as_str() == lower || k.starts_with(&inside)).cloned().collect();
        for k in gone {
            if let Some(f) = self.folders.remove(&k) {
                self.total -= f.entries.len();
            }
        }
    }

    /// Applies one change of the log (or of the app's own work) to the listings, noting in
    /// `touched` the folders whose items it changed.
    pub fn apply(&mut self, change: &Change, touched: &mut HashSet<String>) {
        let lower = change.lower().to_string();
        let up = parent(&lower).to_string();
        touched.insert(up.clone());
        self.dirty = true;
        match change {
            Change::Is(e) => {
                if !e.dir {
                    // A folder that was here is gone, a file in its place.
                    self.drop_under(&lower);
                }
                if let Some(f) = self.folders.get_mut(&up) {
                    match f.entries.iter_mut().find(|x| x.lower == lower) {
                        Some(x) => *x = e.clone(),
                        None => {
                            f.entries.push(e.clone());
                            self.total += 1;
                        }
                    }
                }
            }
            Change::Gone(_) => {
                touched.insert(lower.clone());
                self.drop_under(&lower);
                if let Some(f) = self.folders.get_mut(&up) {
                    let before = f.entries.len();
                    f.entries.retain(|x| x.lower != lower);
                    self.total -= before - f.entries.len();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Sealing

/// What seals and opens one login's file.
#[derive(Clone)]
pub struct Secret(sealed::Secret);

impl Secret {
    /// The secret of the login with refresh token `token`, of account `account_id`.
    pub fn of(account_id: &str, token: &str) -> Secret {
        Secret(sealed::Secret::derive("OpenExplorer Dropbox listing cache", account_id.as_bytes(), token.as_bytes()))
    }
}

#[derive(Serialize, Deserialize)]
struct Body {
    ns: String,
    log: Option<String>,
    folders: Vec<KeptFolder>,
}

#[derive(Serialize, Deserialize)]
struct KeptFolder {
    k: String,
    t: i64,
    e: Vec<Entry>,
}

/// The body of `lists`, as JSON.
pub fn encode(lists: &Lists) -> Vec<u8> {
    let mut folders: Vec<KeptFolder> = lists.folders.iter().map(|(k, f)| KeptFolder { k: k.clone(), t: f.fetched, e: f.entries.clone() }).collect();
    // The most recently opened last, so they come back as the most recent.
    folders.sort_by_key(|f| lists.folders[&f.k].used);
    serde_json::to_vec(&Body { ns: lists.namespace.clone(), log: lists.log.clone(), folders }).expect("listings encode")
}

/// The listings a body holds.
pub fn decode(body: &[u8]) -> Result<Lists, Refusal> {
    let b: Body = serde_json::from_slice(body).map_err(|_| Refusal::Malformed)?;
    let mut lists = Lists::new(&b.ns);
    lists.log = b.log;
    for f in b.folders {
        lists.put(&f.k, f.e, f.t);
    }
    lists.dirty = false;
    Ok(lists)
}

// ---------------------------------------------------------------------------------------------
// Where

/// The folder the listings are kept in; `None` keeps none (the default, and tests').
pub fn set_dir(dir: Option<PathBuf>) {
    STORE.set_dir(dir);
}

pub fn dir() -> Option<PathBuf> {
    STORE.dir()
}

/// The file of account `email` in `dir`: named by a hash, so the folder doesn't list e-mails.
pub fn file_in(dir: &Path, email: &str) -> PathBuf {
    STORE.file_in(dir, &crate::store::normal_email(email))
}

pub fn file_of(email: &str) -> Option<PathBuf> {
    dir().map(|d| file_in(&d, email))
}

/// What an account notes as it opens: [`cleared_since`] tells whether its folder was cleared
/// after, and then it keeps nothing there for the rest of the run.
pub fn mark() -> u64 {
    STORE.mark()
}

pub fn cleared_since(mark: u64, file: &Path) -> bool {
    STORE.cleared_since(mark, file)
}

/// Runs `clear` on the folder set, with writes held off, after emptying the listings the open
/// accounts hold: a listing an account open now held is never written there again this run.
/// `None` with no folder set.
pub fn clear_with<R>(clear: impl FnOnce(&Path) -> R) -> Option<R> {
    STORE.clear_with(clear)
}

/// Deletes `file`, after any write in flight, and temporary files a crash left beside it.
pub fn forget_file(file: &Path) {
    STORE.forget_file(file);
}

/// Writes `body` sealed to `file` whole, unless the account was forgotten or the folder cleared
/// after `mark` by the time it's its turn. Whether it wrote.
pub fn write_unless(file: &Path, secret: &Secret, body: &[u8], forgotten: &AtomicBool, mark: u64) -> std::io::Result<bool> {
    let sealed = STORE.seal_copy(&secret.0, body);
    STORE.write_unless(file, &sealed, forgotten, mark)
}

/// The listings kept in `file`.
pub fn read(file: &Path, secret: &Secret) -> Result<Lists, Refusal> {
    let (data, body) = STORE.read(file, &secret.0)?;
    decode(&data[body])
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn file(lower: &str, size: u64) -> Entry {
        Entry { name: crate::meta::last(lower).into(), lower: lower.into(), dir: false, size, modified: Some(1), rev: "r1".into(), hash: String::new() }
    }

    pub(crate) fn folder(lower: &str) -> Entry {
        Entry { name: crate::meta::last(lower).into(), lower: lower.into(), dir: true, size: 0, modified: None, rev: String::new(), hash: String::new() }
    }

    #[test]
    fn changes_patch_the_listings_they_touch() {
        let mut l = Lists::new("ns1");
        l.put("", vec![folder("/fotos"), file("/a.txt", 1)], 100);
        l.put("/fotos", vec![file("/fotos/x.jpg", 5), folder("/fotos/2024")], 100);
        l.put("/fotos/2024", vec![file("/fotos/2024/y.jpg", 7)], 100);
        let mut touched = HashSet::new();
        // A new file, a changed one, one gone.
        l.apply(&Change::Is(file("/fotos/z.jpg", 9)), &mut touched);
        l.apply(&Change::Is(file("/a.txt", 2)), &mut touched);
        l.apply(&Change::Gone("/fotos/x.jpg".into()), &mut touched);
        let names = |l: &mut Lists, k: &str| l.get(k, 100).map(|f| f.entries.iter().map(|e| (e.lower.clone(), e.size)).collect::<Vec<_>>());
        assert_eq!(names(&mut l, "/fotos").unwrap(), [("/fotos/2024".to_string(), 0), ("/fotos/z.jpg".into(), 9)]);
        assert_eq!(names(&mut l, "").unwrap(), [("/fotos".to_string(), 0), ("/a.txt".into(), 2)]);
        assert_eq!(touched, HashSet::from(["".to_string(), "/fotos".into(), "/fotos/x.jpg".into()]));
        // A folder deleted takes the listings in it along.
        l.apply(&Change::Gone("/fotos".into()), &mut touched);
        assert!(l.get("/fotos", 100).is_none() && l.get("/fotos/2024", 100).is_none());
        assert_eq!(names(&mut l, "").unwrap(), [("/a.txt".to_string(), 2)]);
        assert_eq!(l.total, 1);
        // Too old: read again.
        assert!(l.get("", 100 + MAX_AGE + 1).is_none());
    }

    #[test]
    fn the_least_recently_opened_listings_go_first() {
        let mut l = Lists::new("ns");
        for i in 0..MAX_FOLDERS {
            l.put(&format!("/p{i}"), vec![], 1);
        }
        l.get("/p0", 1);
        l.put("/nova", vec![], 1);
        assert_eq!(l.len(), MAX_FOLDERS);
        assert!(l.get("/p0", 1).is_some() && l.get("/p1", 1).is_none());
    }

    /// A file sealed before the shared crate (`openexplorer-cloud`) took over the sealing, and
    /// its name: both must still be what they were.
    #[test]
    fn a_file_kept_before_still_opens() {
        let hex = "4f454442584c5300010000006566f828e2d1074a25afe54bb65d52278d2250b0f6a9e716cb3e29a152bfd75b4b709f25bb9f2e69dd4f41c88281f960641c7188e66a8375e06c01dcc1760df71e5fd5cd9d1771b1";
        let mut old: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
        let body = STORE.open(&Secret::of("dbid:AAA", "refresh-1").0, &mut old).unwrap();
        assert_eq!(&old[body], b"kept before the shared crate");
        assert_eq!(file_in(Path::new("d"), "Ana@Exemplo.com"), Path::new("d").join("840d9902bc6853e59cd9585cd0ecb3ee.lists"));
    }

    #[test]
    fn a_sealed_file_opens_only_with_its_login_and_unchanged() {
        let dir = std::env::temp_dir().join(format!("oe-dropbox-cache-{}", std::process::id()));
        let path = file_in(&dir, "Ana@Exemplo.com");
        assert_eq!(path, file_in(&dir, "ana@exemplo.com"));
        let secret = Secret::of("dbid:AAA", "refresh-1");
        let mut l = Lists::new("ns1");
        l.log = Some("cursor-1".into());
        l.put("/ação", vec![file("/ação/😀.txt", 3)], 50);
        assert!(write_unless(&path, &secret, &encode(&l), &AtomicBool::new(false), mark()).unwrap());
        let mut back = read(&path, &secret).unwrap();
        assert_eq!((back.namespace.as_str(), back.log.as_deref()), ("ns1", Some("cursor-1")));
        assert_eq!(back.get("/ação", 50).unwrap().entries, l.get("/ação", 50).unwrap().entries);
        // The bytes don't show the names.
        let raw = std::fs::read(&path).unwrap();
        assert!(!raw.windows(4).any(|w| w == "ação".as_bytes()[..4].as_ref()));
        // Another login's key, or a changed byte: refused.
        assert_eq!(read(&path, &Secret::of("dbid:AAA", "refresh-2")).err(), Some(Refusal::OtherKey));
        let mut bad = raw.clone();
        let n = bad.len() - 20;
        bad[n] ^= 1;
        std::fs::write(&path, bad).unwrap();
        assert_eq!(read(&path, &secret).err(), Some(Refusal::Tampered));
        // A clear keeps writes out for accounts opened before it.
        set_dir(Some(dir.clone()));
        let before = mark();
        clear_with(|d| std::fs::remove_dir_all(d)).unwrap().unwrap();
        assert!(!write_unless(&path, &secret, &encode(&l), &AtomicBool::new(false), before).unwrap());
        assert!(write_unless(&path, &secret, &encode(&l), &AtomicBool::new(false), mark()).unwrap());
        forget_file(&path);
        assert_eq!(read(&path, &secret).err(), Some(Refusal::Missing));
        set_dir(None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
