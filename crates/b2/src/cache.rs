//! Folder listings kept between openings and between runs, so a folder seen before shows at
//! once, without waiting for B2.
//!
//! # When a listing is read again
//!
//! B2 has no change log nor any way to ask "what changed since": a listing can't be proved
//! current, and listing is free (see [`crate::watch`]). So a kept listing is only ever shown
//! while B2 is asked again:
//! - opening a folder shows its kept listing at once and always reads it again behind it; the
//!   pane changes only if B2's answer differs (`Account::reread`);
//! - while a pane shows a folder and the window has the focus, the folder is read again every
//!   45 seconds or so, within a budget (`Account::poll`, [`crate::watch`]);
//! - anything that checks before writing (a name taken, a folder's names before an upload) asks
//!   B2, never the kept listing (`Account::list`, `Account::stat`);
//! - the app's own changes drop the listings they touch at once (the folder an item left or
//!   entered, every folder above it, and every folder under one moved, copied or deleted), and
//!   a listing read across one of them isn't kept.
//!
//! Listings older than [`KEEP`] (30 days) are dropped from the disk.
//!
//! # Format
//!
//! One file per key, `<hash of the key ID>.lists`, in the folder the app sets with [`set_dir`]
//! (`%LOCALAPPDATA%\OpenExplorer\b2`; with none set, nothing is kept). It holds every listing the
//! key keeps, written whole and atomically (a temporary file renamed over the old one) a few
//! seconds after the listings change, and at closing.
//!
//! ```text
//! magic "OEB2LST\0" | format (u32 LE) | key id (16) | nonce (12) | AES-256-GCM(body) | tag (16)
//! ```
//!
//! # Security
//!
//! File names and sizes are private, so the file is sealed with AES-256-GCM under a key derived
//! (HKDF-SHA256) from the application key, the key ID as salt. The application key only comes
//! back from the OS credential store, so the file alone opens nothing; the header's key id tells
//! another key's file from a damaged one without decrypting it, and is authenticated with the body.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use openexplorer_cloud::sealed::{self, OnClear, Store};
use openexplorer_cloud::varint::{Reader, put_bytes, put_len, put_signed};

pub use openexplorer_cloud::sealed::Refusal;

use crate::account::Item;

/// How long a listing stays on disk at all.
pub const KEEP: Duration = Duration::from_secs(30 * 24 * 3600);
/// The items a key keeps in memory at most, over all its listings: the oldest listings go first.
pub const MOST_ITEMS: usize = 400_000;
/// How long after a change the file is written.
const SAVE_AFTER: Duration = Duration::from_secs(3);

/// Bumped when the body's shape changes: a file of another format is thrown away.
pub const FORMAT: u32 = 1;
const HEADER: usize = sealed::HEADER;
const TAG: usize = sealed::TAG;

/// The files, `<hash of the key ID>.lists`. A clear empties the listings the open keys hold
/// too ([`forget_memory`]): every folder is read from B2 again.
static STORE: Store = Store::new(b"OEB2LST\0", FORMAT, "lists", OnClear::Forget(forget_memory));

/// Seconds since 1970.
pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A folder's items as B2 listed them, and when (seconds since 1970).
#[derive(Clone, Debug, PartialEq)]
pub struct Listing {
    pub items: Vec<Item>,
    pub fetched: u64,
}

impl Listing {
    pub fn age(&self) -> Duration {
        Duration::from_secs(now().saturating_sub(self.fetched))
    }
}

/// What seals and opens one key's file.
#[derive(Clone)]
pub struct Secret(sealed::Secret);

impl Secret {
    pub fn of(key_id: &str, application_key: &str) -> Secret {
        Secret(sealed::Secret::derive("OpenExplorer B2 listing cache", key_id.as_bytes(), application_key.as_bytes()))
    }
}

// ---------------------------------------------------------------------------------------------
// Where

/// The keys' listings open now, emptied by a clear.
static OPEN: Mutex<Vec<Weak<Listings>>> = Mutex::new(Vec::new());

/// The folder the listings are kept in; `None` keeps none (the default, and tests').
pub fn set_dir(dir: Option<PathBuf>) {
    STORE.set_dir(dir);
}

pub fn dir() -> Option<PathBuf> {
    STORE.dir()
}

/// The file of key `key_id` in `dir`: named by a hash, so the folder doesn't list key IDs.
pub fn file_in(dir: &Path, key_id: &str) -> PathBuf {
    STORE.file_in(dir, key_id.trim())
}

pub fn file_of(key_id: &str) -> Option<PathBuf> {
    dir().map(|d| file_in(&d, key_id))
}

/// Runs `clear` on the folder set (see [`set_dir`]) with every write held off, after emptying the
/// listings in memory: every folder is read from B2 again. `None` with no folder set.
pub fn clear_with<R>(clear: impl FnOnce(&Path) -> R) -> Option<R> {
    STORE.clear_with(clear)
}

/// [`clear_with`] on folder `dir`.
pub fn clear_in<R>(dir: &Path, clear: impl FnOnce(&Path) -> R) -> R {
    STORE.clear_in(dir, clear)
}

/// Empties the listings of every key open now (a clear's [`OnClear`]).
fn forget_memory() {
    let open: Vec<Arc<Listings>> = OPEN.lock().unwrap_or_else(|e| e.into_inner()).iter().filter_map(Weak::upgrade).collect();
    for l in open {
        l.clear_memory();
    }
}

/// Deletes `file`, and any temporary file a write cut short left beside it, after any write in
/// flight.
pub fn forget_file(file: &Path) {
    STORE.forget_file(file);
}

// ---------------------------------------------------------------------------------------------
// The listings of one key

/// A folder: its bucket's name and its prefix ("" at the bucket's top, else ending in `/`); the
/// bucket list itself is `("", "")`.
pub type Key = (String, String);

#[derive(Default)]
struct Inner {
    map: HashMap<Key, Arc<Listing>>,
    items: usize,
    dirty: bool,
}

/// One key's listings, in memory and in its file.
pub struct Listings {
    inner: Mutex<Inner>,
    file: Option<(PathBuf, Secret)>,
    /// A write is on its way.
    saving: AtomicBool,
    /// The key was removed: nothing is written any more.
    forgotten: AtomicBool,
}

impl Listings {
    /// The listings of a key whose file is `file` (read here), or kept in memory only.
    pub fn open(file: Option<(PathBuf, Secret)>) -> Arc<Listings> {
        let mut inner = Inner::default();
        if let Some((path, secret)) = &file {
            match read(path, secret) {
                Ok(map) => {
                    let old = now().saturating_sub(KEEP.as_secs());
                    for (k, l) in map {
                        if l.fetched >= old {
                            inner.items += l.items.len();
                            inner.map.insert(k, Arc::new(l));
                        } else {
                            inner.dirty = true;
                        }
                    }
                    crate::api::log(&format!("B2 cache: {} listagens lidas do disco", inner.map.len()));
                }
                Err(Refusal::Missing) => {}
                Err(why) => {
                    crate::api::log(&format!("B2 cache: {why:?}, descartado"));
                    forget_file(path);
                }
            }
        }
        let l = Arc::new(Listings { inner: Mutex::new(inner), file, saving: AtomicBool::new(false), forgotten: AtomicBool::new(false) });
        let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
        open.retain(|w| w.strong_count() > 0);
        open.push(Arc::downgrade(&l));
        l
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get(&self, bucket: &str, prefix: &str) -> Option<Arc<Listing>> {
        self.lock().map.get(&(bucket.to_string(), prefix.to_string())).cloned()
    }

    /// How many listings are kept.
    pub fn len(&self) -> usize {
        self.lock().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Keeps `items` as the listing of `prefix` in `bucket`, read at `fetched`.
    pub fn put(self: &Arc<Self>, bucket: &str, prefix: &str, items: Vec<Item>, fetched: u64) -> Arc<Listing> {
        let listing = Arc::new(Listing { items, fetched });
        {
            let mut inner = self.lock();
            inner.items += listing.items.len();
            if let Some(old) = inner.map.insert((bucket.to_string(), prefix.to_string()), listing.clone()) {
                inner.items -= old.items.len();
            }
            // The oldest listings go once there are too many items.
            while inner.items > MOST_ITEMS && inner.map.len() > 1 {
                let Some(oldest) = inner.map.iter().filter(|(_, l)| !Arc::ptr_eq(l, &listing)).min_by_key(|(_, l)| l.fetched).map(|(k, _)| k.clone()) else { break };
                let gone = inner.map.remove(&oldest).expect("a listing just found");
                inner.items -= gone.items.len();
            }
            inner.dirty = true;
        }
        self.save_soon();
        listing
    }

    /// B2 listed `prefix` in `bucket` at `fetched` just as kept. The listing's time moves in
    /// memory only (the file isn't written again for it), and at most once an hour, as it only
    /// orders which listings go first past [`MOST_ITEMS`] and which leave the disk past [`KEEP`].
    pub fn touch(&self, bucket: &str, prefix: &str, fetched: u64) {
        let mut inner = self.lock();
        let key = (bucket.to_string(), prefix.to_string());
        if let Some(old) = inner.map.get(&key)
            && fetched.saturating_sub(old.fetched) >= 3600
        {
            let moved = Arc::new(Listing { items: old.items.clone(), fetched });
            inner.map.insert(key, moved);
        }
    }

    /// Drops the listings `drop` picks.
    fn drop_where(self: &Arc<Self>, drop: impl Fn(&Key) -> bool) {
        let gone = {
            let mut inner = self.lock();
            let keys: Vec<Key> = inner.map.keys().filter(|k| drop(k)).cloned().collect();
            for k in &keys {
                let l = inner.map.remove(k).expect("a key just listed");
                inner.items -= l.items.len();
            }
            inner.dirty |= !keys.is_empty();
            !keys.is_empty()
        };
        if gone {
            self.save_soon();
        }
    }

    /// Drops the listing of `prefix` in `bucket`.
    pub fn forget(self: &Arc<Self>, bucket: &str, prefix: &str) {
        self.drop_where(|(b, p)| b == bucket && p == prefix);
    }

    /// Drops the listing of `prefix` in `bucket` and of every folder under it.
    pub fn forget_under(self: &Arc<Self>, bucket: &str, prefix: &str) {
        self.drop_where(|(b, p)| b == bucket && p.starts_with(prefix));
    }

    /// Drops every listing of `bucket`, or all with `None`.
    pub fn forget_bucket(self: &Arc<Self>, bucket: Option<&str>) {
        self.drop_where(|(b, _)| bucket.is_none_or(|x| b == x));
    }

    fn clear_memory(&self) {
        let mut inner = self.lock();
        inner.map.clear();
        inner.items = 0;
        inner.dirty = false;
    }

    /// Stops writing (the key was removed) and deletes the file.
    pub fn forget_all(&self) {
        self.forgotten.store(true, Ordering::SeqCst);
        self.clear_memory();
        if let Some((file, _)) = &self.file {
            forget_file(file);
        }
    }

    /// Writes the file in a few seconds, on a thread of its own, unless a write is already due.
    fn save_soon(self: &Arc<Self>) {
        if self.file.is_none() || self.saving.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(SAVE_AFTER);
            me.saving.store(false, Ordering::SeqCst);
            me.save_if_changed();
        });
    }

    /// Writes the file now if the listings changed since it was last written (the app calls it
    /// as it closes).
    pub fn save_if_changed(&self) {
        let Some((file, secret)) = &self.file else { return };
        if self.forgotten.load(Ordering::SeqCst) {
            return;
        }
        // A clear after this (see [`clear_in`]) makes the snapshot one of listings no longer kept.
        let mark = STORE.mark();
        let body = {
            let mut inner = self.lock();
            if !inner.dirty {
                return;
            }
            inner.dirty = false;
            encode(&inner.map)
        };
        let sealed = seal(secret, body);
        if let Err(e) = STORE.write_unless(file, &sealed, &self.forgotten, mark) {
            crate::api::log(&format!("B2 cache: não consegui gravar as listagens: {e}"));
            self.lock().dirty = true;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Body

const IS_DIR: u8 = 1;
const HAS_SIZE: u8 = 2;
const HAS_MODIFIED: u8 = 4;
const HAS_ID: u8 = 8;
const HAS_INFO: u8 = 16;

/// The listings as the file's body (after room for the header).
pub fn encode(map: &HashMap<Key, Arc<Listing>>) -> Vec<u8> {
    let items: usize = map.values().map(|l| l.items.len()).sum();
    let mut out = Vec::with_capacity(HEADER + items * 48 + TAG);
    out.resize(HEADER, 0);
    put_len(&mut out, map.len() as u64);
    for ((bucket, prefix), l) in map {
        put_bytes(&mut out, bucket.as_bytes());
        put_bytes(&mut out, prefix.as_bytes());
        put_len(&mut out, l.fetched);
        put_len(&mut out, l.items.len() as u64);
        for it in &l.items {
            let flag = |on: bool, f: u8| if on { f } else { 0 };
            out.push(flag(it.is_dir, IS_DIR) | flag(it.size.is_some(), HAS_SIZE) | flag(it.modified.is_some(), HAS_MODIFIED) | flag(it.id.is_some(), HAS_ID) | flag(it.info.is_some(), HAS_INFO));
            put_bytes(&mut out, it.name.as_bytes());
            if let Some(s) = it.size {
                put_len(&mut out, s);
            }
            if let Some(m) = it.modified {
                put_signed(&mut out, m);
            }
            if let Some(id) = &it.id {
                put_bytes(&mut out, id.as_bytes());
            }
            if let Some(info) = &it.info {
                put_bytes(&mut out, info.as_bytes());
            }
        }
    }
    out
}

/// The listings [`encode`] wrote (`body` without the header).
pub fn decode(body: &[u8]) -> Result<HashMap<Key, Listing>, Refusal> {
    let mut r = Reader::new(body);
    let count = r.count(4)?;
    let mut map = HashMap::with_capacity(count);
    for _ in 0..count {
        let key = (r.text()?.to_string(), r.text()?.to_string());
        let fetched = r.unsigned()?;
        let n = r.count(2)?;
        let mut items = Vec::with_capacity(n);
        for _ in 0..n {
            let flags = r.byte()?;
            if flags & !(IS_DIR | HAS_SIZE | HAS_MODIFIED | HAS_ID | HAS_INFO) != 0 {
                return Err(Refusal::Malformed);
            }
            let name = r.text()?.to_string();
            let size = if flags & HAS_SIZE != 0 { Some(r.unsigned()?) } else { None };
            let modified = if flags & HAS_MODIFIED != 0 { Some(r.signed()?) } else { None };
            let id = if flags & HAS_ID != 0 { Some(r.text()?.to_string()) } else { None };
            let info = if flags & HAS_INFO != 0 { Some(r.text()?.to_string()) } else { None };
            items.push(Item { name, is_dir: flags & IS_DIR != 0, size, modified, id, info });
        }
        if map.insert(key, Listing { items, fetched }).is_some() {
            return Err(Refusal::Malformed);
        }
    }
    if !r.done() {
        return Err(Refusal::Malformed);
    }
    Ok(map)
}

// ---------------------------------------------------------------------------------------------
// Sealing

/// Seals a body [`encode`] made, in place: the file's whole contents.
pub fn seal(secret: &Secret, file: Vec<u8>) -> Vec<u8> {
    STORE.seal(&secret.0, file)
}

/// Opens a file [`seal`] made, in place: the body's range.
pub fn open(secret: &Secret, file: &mut [u8]) -> Result<std::ops::Range<usize>, Refusal> {
    STORE.open(&secret.0, file)
}

/// Writes `sealed` to `file` whole: a temporary file renamed over the old one.
#[cfg(test)]
fn write(file: &Path, sealed: &[u8]) -> std::io::Result<()> {
    STORE.write_unless(file, sealed, &AtomicBool::new(false), STORE.mark()).map(|_| ())
}

/// The listings kept in `file`.
pub fn read(file: &Path, secret: &Secret) -> Result<HashMap<Key, Listing>, Refusal> {
    let (data, body) = STORE.read(file, &secret.0)?;
    decode(&data[body])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, dir: bool) -> Item {
        Item { name: name.into(), is_dir: dir, size: (!dir).then_some(12_345), modified: (!dir).then_some(1_700_000_000_123), id: None, info: None }
    }

    fn secret() -> Secret {
        Secret::of("000ab12cd", "K000secretsecret")
    }

    fn sample() -> HashMap<Key, Arc<Listing>> {
        let mut m = HashMap::new();
        m.insert(("fotos".into(), "".into()), Arc::new(Listing { items: vec![item("2024", true), item("praia.jpg", false)], fetched: now() }));
        m.insert(("fotos".into(), "2024/".into()), Arc::new(Listing { items: vec![item("açaí ñ 日本.png", false)], fetched: now() - 10 }));
        let bucket = Item { name: "fotos".into(), is_dir: true, size: None, modified: None, id: Some("4a5b6c".into()), info: Some("[]".into()) };
        m.insert(("".into(), "".into()), Arc::new(Listing { items: vec![bucket], fetched: now() }));
        m
    }

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("oe-b2-cache-{}-{name}", std::process::id()))
    }

    /// A file sealed before the shared crate (`openexplorer-cloud`) took over the sealing and
    /// the body's encoding, and its name: all must still be what they were.
    #[test]
    fn a_file_kept_before_still_opens() {
        let bytes = |hex: &str| (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect::<Vec<u8>>();
        let mut old = bytes("4f4542324c53540001000000001923d7f32c42d3c0644e0f44824708f6de7c87c7c71b87f0b8bab3300eac22734ce199d5afa54f2b14b8baef1abba8354fb421ee5a0c3c7ddcec6c2d7d01758878d9e70cdf576b");
        let body = open(&secret(), &mut old).unwrap();
        assert_eq!(&old[body], b"kept before the shared crate");
        let listing = Listing { items: vec![Item { name: "açaí.png".into(), is_dir: false, size: Some(12345), modified: Some(-5), id: Some("i".into()), info: Some("{}".into()) }, Item { name: "d".into(), is_dir: true, size: None, modified: None, id: None, info: None }], fetched: 1_700_000_000 };
        let key = ("fotos".to_string(), "2024/".to_string());
        let old_body = bytes("0105666f746f7305323032342f80e2cfaa06021e0a61c3a761c3ad2e706e67b960090169027b7d010164");
        assert_eq!(decode(&old_body).unwrap(), HashMap::from([(key.clone(), listing.clone())]));
        assert_eq!(encode(&HashMap::from([(key, Arc::new(listing))]))[HEADER..], old_body[..]);
        assert_eq!(file_in(Path::new("d"), " 000ab12cd "), Path::new("d").join("c09e8609c7ee349cbc2d7238c113b9a7.lists"));
    }

    #[test]
    fn listings_go_to_disk_and_come_back_whole() {
        let map = sample();
        let dir = temp("roundtrip");
        let file = file_in(&dir, "000ab12cd");
        assert_eq!(file, file_in(&dir, " 000ab12cd "));
        write(&file, &seal(&secret(), encode(&map))).unwrap();
        let back = read(&file, &secret()).unwrap();
        assert_eq!(back.len(), 3);
        for (k, l) in &map {
            assert_eq!(&back[k], &**l);
        }
        // The names aren't readable in the file.
        let raw = std::fs::read(&file).unwrap();
        assert!(!raw.windows(5).any(|w| w == b"praia") && !raw.windows(5).any(|w| w == b"fotos"));
        let stray = file.with_extension("tmp7");
        std::fs::write(&stray, b"half").unwrap();
        forget_file(&file);
        assert_eq!(read(&file, &secret()).err(), Some(Refusal::Missing));
        assert!(!stray.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_changed_byte_or_another_key_opens_nothing() {
        let sealed = seal(&secret(), encode(&sample()));
        for i in (0..HEADER).chain((HEADER..sealed.len()).step_by(29)).chain([sealed.len() - 1]) {
            let mut bad = sealed.clone();
            bad[i] ^= 0x01;
            let want = match i {
                0..12 => Refusal::Format,
                12..28 => Refusal::OtherKey,
                _ => Refusal::Tampered,
            };
            assert_eq!(open(&secret(), &mut bad).err(), Some(want), "byte {i}");
        }
        // Another application key under the same key ID (a key can't be read back, but a file
        // of a deleted key must not open with a new one).
        assert_eq!(open(&Secret::of("000ab12cd", "K000other"), &mut sealed.clone()).err(), Some(Refusal::OtherKey));
        let forged = Secret(sealed::Secret { key: Secret::of("000ab12cd", "K000other").0.key, id: secret().0.id });
        assert_eq!(open(&forged, &mut sealed.clone()).err(), Some(Refusal::Tampered));
        let mut good = sealed;
        assert!(open(&secret(), &mut good).is_ok());
    }

    #[test]
    fn a_body_that_doesnt_read_is_refused() {
        let body = encode(&sample())[HEADER..].to_vec();
        assert!(decode(&body).is_ok());
        for cut in [0, 1, 5, body.len() / 2, body.len() - 1] {
            assert_eq!(decode(&body[..cut]).err(), Some(Refusal::Malformed), "cut at {cut}");
        }
        let mut longer = body.clone();
        longer.push(0);
        assert_eq!(decode(&longer).err(), Some(Refusal::Malformed));
        for i in 0..body.len() {
            let mut b = body.clone();
            b[i] = b[i].wrapping_add(0x41);
            let _ = decode(&b);
        }
    }

    #[test]
    fn own_changes_drop_what_they_touch_and_memory_stays_bounded() {
        let l = Listings::open(None);
        l.put("b", "", vec![item("a", true)], now());
        l.put("b", "a/", vec![item("x", false)], now());
        l.put("b", "a/c/", vec![item("y", false)], now());
        l.put("b", "ab/", vec![item("z", false)], now());
        l.put("o", "a/", vec![item("z", false)], now());
        l.forget_under("b", "a/");
        assert!(l.get("b", "a/").is_none() && l.get("b", "a/c/").is_none());
        assert!(l.get("b", "").is_some() && l.get("b", "ab/").is_some() && l.get("o", "a/").is_some());
        l.forget("b", "");
        assert!(l.get("b", "").is_none());
        l.forget_bucket(Some("o"));
        assert_eq!(l.len(), 1);
        // Over the bound, the oldest listings go.
        let big = |n: usize| (0..n).map(|i| item(&i.to_string(), false)).collect::<Vec<_>>();
        l.put("b", "old/", big(MOST_ITEMS / 2), now() - 100);
        l.put("b", "new/", big(MOST_ITEMS / 2 + 10), now());
        assert!(l.get("b", "old/").is_none() && l.get("b", "new/").is_some());
    }

    #[test]
    fn listings_older_than_kept_are_dropped_on_reading() {
        let dir = temp("keep");
        let file = file_in(&dir, "k");
        let s = Secret::of("k", "secret");
        let mut map = sample();
        map.insert(("fotos".into(), "velho/".into()), Arc::new(Listing { items: vec![item("a", false)], fetched: now() - KEEP.as_secs() - 60 }));
        write(&file, &seal(&s, encode(&map))).unwrap();
        let l = Listings::open(Some((file.clone(), s)));
        assert_eq!(l.len(), 3);
        assert!(l.get("fotos", "velho/").is_none());
        l.forget_all();
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
