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
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use aes_gcm::KeyInit;
use aes_gcm::aead::AeadInOut;
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::meta::{Change, Entry, parent};

const MAGIC: &[u8; 8] = b"OEDBXLS\0";
/// Bumped when the body's shape changes: a file of another format is thrown away.
pub const FORMAT: u32 = 1;
const HEADER: usize = 8 + 4 + 16 + 12;
const TAG: usize = 16;

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
pub struct Secret {
    key: [u8; 32],
    id: [u8; 16],
}

impl Secret {
    /// The secret of the login with refresh token `token`, of account `account_id`.
    pub fn of(account_id: &str, token: &str) -> Secret {
        let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(account_id.as_bytes()), token.as_bytes());
        let mut key = [0u8; 32];
        let mut id = [0u8; 16];
        hk.expand(b"OpenExplorer Dropbox listing cache: key", &mut key).expect("32 bytes is a valid length");
        hk.expand(b"OpenExplorer Dropbox listing cache: key id", &mut id).expect("16 bytes is a valid length");
        Secret { key, id }
    }
}

/// Why a kept file wasn't used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    Missing,
    Io(String),
    /// Not one of these files, or of another format.
    Format,
    /// Sealed by another login (or account).
    OtherLogin,
    /// Changed since it was written.
    Tampered,
    /// Opened, but not a body of listings.
    Malformed,
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the system's random generator");
    b
}

/// The whole file of `body`.
pub fn seal(secret: &Secret, body: &[u8]) -> Vec<u8> {
    let nonce = random::<12>();
    let mut file = Vec::with_capacity(HEADER + body.len() + TAG);
    file.extend_from_slice(MAGIC);
    file.extend_from_slice(&FORMAT.to_le_bytes());
    file.extend_from_slice(&secret.id);
    file.extend_from_slice(&nonce);
    file.extend_from_slice(body);
    let gcm = aes_gcm::Aes256Gcm::new_from_slice(&secret.key).expect("a 32-byte key");
    let (head, rest) = file.split_at_mut(HEADER);
    let tag = gcm.encrypt_inout_detached((&nonce[..]).try_into().expect("12 bytes"), &head[..28], rest.into()).expect("a body GCM takes");
    file.extend_from_slice(&tag);
    file
}

/// The body of a file [`seal`] made.
pub fn open(secret: &Secret, mut file: Vec<u8>) -> Result<Vec<u8>, Refusal> {
    if file.len() < HEADER + TAG || &file[..8] != MAGIC || file[8..12] != FORMAT.to_le_bytes() {
        return Err(Refusal::Format);
    }
    if file[12..28] != secret.id {
        return Err(Refusal::OtherLogin);
    }
    let end = file.len() - TAG;
    let tag: [u8; TAG] = file[end..].try_into().expect("16 bytes");
    let (head, rest) = file.split_at_mut(HEADER);
    let gcm = aes_gcm::Aes256Gcm::new_from_slice(&secret.key).expect("a 32-byte key");
    gcm.decrypt_inout_detached((&head[28..]).try_into().expect("12 bytes"), &head[..28], (&mut rest[..end - HEADER]).into(), (&tag[..]).try_into().expect("16 bytes")).map_err(|_| Refusal::Tampered)?;
    file.truncate(end);
    file.drain(..HEADER);
    Ok(file)
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

static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
/// One file is written at a time, so an older snapshot never lands over a newer one, and a
/// forget waits for a write in flight.
static WRITING: Mutex<()> = Mutex::new(());
/// How many clears ran, and the folder of each (see [`mark`]).
static CLEARS: Mutex<(u64, Vec<(u64, PathBuf)>)> = Mutex::new((0, Vec::new()));

/// The folder the listings are kept in; `None` keeps none (the default, and tests').
pub fn set_dir(dir: Option<PathBuf>) {
    *DIR.lock().unwrap_or_else(|e| e.into_inner()) = dir;
}

pub fn dir() -> Option<PathBuf> {
    DIR.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The file of account `email` in `dir`: named by a hash, so the folder doesn't list e-mails.
pub fn file_in(dir: &Path, email: &str) -> PathBuf {
    let digest = sha2::Sha256::digest(crate::store::normal_email(email).as_bytes());
    let name: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{name}.lists"))
}

pub fn file_of(email: &str) -> Option<PathBuf> {
    dir().map(|d| file_in(&d, email))
}

/// What an account notes as it opens: [`cleared_since`] tells whether its folder was cleared
/// after, and then it keeps nothing there for the rest of the run.
pub fn mark() -> u64 {
    CLEARS.lock().unwrap_or_else(|e| e.into_inner()).0
}

pub fn cleared_since(mark: u64, file: &Path) -> bool {
    CLEARS.lock().unwrap_or_else(|e| e.into_inner()).1.iter().any(|(n, dir)| *n > mark && file.starts_with(dir))
}

/// Runs `clear` on the folder set, with writes held off: a listing an account open now holds is
/// never written there again this run. `None` with no folder set.
pub fn clear_with<R>(clear: impl FnOnce(&Path) -> R) -> Option<R> {
    let dir = dir()?;
    {
        let mut clears = CLEARS.lock().unwrap_or_else(|e| e.into_inner());
        clears.0 += 1;
        let n = clears.0;
        clears.1.push((n, dir.clone()));
    }
    let _held = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    Some(clear(&dir))
}

/// Deletes `file`, after any write in flight, and temporary files a crash left beside it.
pub fn forget_file(file: &Path) {
    let _held = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let _ = std::fs::remove_file(file);
    let (Some(dir), Some(stem)) = (file.parent(), file.file_stem().and_then(|s| s.to_str())) else { return };
    let prefix = format!("{stem}.tmp");
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_name().to_str().is_some_and(|n| n.starts_with(&prefix)) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Writes `body` sealed to `file` whole, unless the account was forgotten or the folder cleared
/// after `mark` by the time it's its turn. Whether it wrote.
pub fn write_unless(file: &Path, secret: &Secret, body: &[u8], forgotten: &AtomicBool, mark: u64) -> std::io::Result<bool> {
    let sealed = seal(secret, body);
    let _held = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    if forgotten.load(Ordering::SeqCst) || cleared_since(mark, file) {
        return Ok(false);
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension(format!("tmp{}", u32::from_le_bytes(random::<4>())));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&sealed)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, file)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map(|()| true)
}

/// The listings kept in `file`.
pub fn read(file: &Path, secret: &Secret) -> Result<Lists, Refusal> {
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Refusal::Missing),
        Err(e) => return Err(Refusal::Io(e.to_string())),
    };
    decode(&open(secret, bytes)?)
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
        assert_eq!(read(&path, &Secret::of("dbid:AAA", "refresh-2")).err(), Some(Refusal::OtherLogin));
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
