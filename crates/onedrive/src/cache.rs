//! An account's listings and delta position kept on disk between runs, so a start reads only
//! what changed since (one delta request when nothing did) instead of listing folders again.
//!
//! One file per account, `<hash of the drive id>.tree`, in the folder the app sets with
//! [`set_dir`] (`%LOCALAPPDATA%\OpenExplorer\onedrive`; with none set, nothing is kept): the
//! tree's snapshot as JSON, written whole and atomically (a temporary file renamed over the
//! old one).
//!
//! ```text
//! magic "OEODTRE\0" | format (u32 LE) | key id (16) | nonce (12) | AES-256-GCM(JSON) | tag (16)
//! ```
//!
//! # Security
//!
//! The file holds names of files, so it's sealed with AES-256-GCM under a key derived
//! (HKDF-SHA256, the drive id as salt) from 32 random bytes kept only in the OS credential store
//! beside the refresh token (see [`crate::vault`]): the file alone opens nothing, and removing
//! the account makes it unreadable even if left behind. The magic, format and key id are
//! authenticated with the body; any changed byte is refused and the file thrown away.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use aes_gcm::KeyInit;
use aes_gcm::aead::AeadInOut;
use sha2::Digest;

use crate::tree::{Snapshot, Tree};

const MAGIC: &[u8; 8] = b"OEODTRE\0";
/// Bumped when the body's shape changes: a file of another format is thrown away.
pub const FORMAT: u32 = 1;
const HEADER: usize = 8 + 4 + 16 + 12;
const TAG: usize = 16;

/// Why a kept tree wasn't used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    Missing,
    Io(String),
    /// Not one of these files, or of another format.
    Format,
    /// Sealed with another key (the account was removed and added again).
    OtherKey,
    /// Changed since it was written.
    Tampered,
    /// Opened, but it isn't a tree.
    Malformed,
}

/// What seals and opens one account's file.
#[derive(Clone)]
pub struct Secret {
    key: [u8; 32],
    id: [u8; 16],
}

impl Secret {
    pub fn of(drive: &str, cache_key: &[u8; 32]) -> Secret {
        let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(drive.as_bytes()), cache_key);
        let mut key = [0u8; 32];
        let mut id = [0u8; 16];
        hk.expand(b"OpenExplorer OneDrive cache: key", &mut key).expect("32 bytes is a valid length");
        hk.expand(b"OpenExplorer OneDrive cache: key id", &mut id).expect("16 bytes is a valid length");
        Secret { key, id }
    }
}

static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
/// One file is written at a time, and a forget or a clear waits for a write in flight.
static WRITING: Mutex<()> = Mutex::new(());
/// How many clears ran, and each one's folder: no tree an account held before a clear is
/// written into its folder after it.
static CLEARS: Mutex<(u64, Vec<(u64, PathBuf)>)> = Mutex::new((0, Vec::new()));

/// The folder the trees are kept in; `None` keeps none (the default, and tests').
pub fn set_dir(dir: Option<PathBuf>) {
    *DIR.lock().unwrap_or_else(|e| e.into_inner()) = dir;
}

pub fn dir() -> Option<PathBuf> {
    DIR.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The file of drive `drive` in `dir`, named by a hash.
pub fn file_in(dir: &Path, drive: &str) -> PathBuf {
    let digest = sha2::Sha256::digest(drive.as_bytes());
    let name: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{name}.tree"))
}

pub fn file_of(drive: &str) -> Option<PathBuf> {
    dir().map(|d| file_in(&d, drive))
}

/// Noted by an account as it starts (see [`cleared_since`]).
pub fn mark() -> u64 {
    CLEARS.lock().unwrap_or_else(|e| e.into_inner()).0
}

pub fn cleared_since(mark: u64, file: &Path) -> bool {
    CLEARS.lock().unwrap_or_else(|e| e.into_inner()).1.iter().any(|(n, dir)| *n > mark && file.starts_with(dir))
}

/// Runs `clear` on the folder set, every write held off; accounts open now never write there
/// again this run. `None` with no folder set.
pub fn clear_with<R>(clear: impl FnOnce(&Path) -> R) -> Option<R> {
    let dir = dir()?;
    Some(clear_in(&dir, clear))
}

pub fn clear_in<R>(dir: &Path, clear: impl FnOnce(&Path) -> R) -> R {
    {
        let mut c = CLEARS.lock().unwrap_or_else(|e| e.into_inner());
        c.0 += 1;
        let n = c.0;
        c.1.push((n, dir.to_path_buf()));
    }
    let _held = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    clear(dir)
}

/// Deletes `file` (and temporary files a crash left beside it), after any write in flight.
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

pub fn seal(secret: &Secret, body: &[u8]) -> Vec<u8> {
    let nonce = crate::oauth::random::<12>();
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
        return Err(Refusal::OtherKey);
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

/// Writes `tree` to `file` whole, unless the account was removed (`forgotten`) or the folder
/// cleared after `mark` by the time it's its turn. Whether it wrote.
pub fn write(file: &Path, secret: &Secret, snap: &Snapshot, forgotten: &AtomicBool, mark: u64) -> std::io::Result<bool> {
    let body = serde_json::to_vec(snap).map_err(std::io::Error::other)?;
    let sealed = seal(secret, &body);
    let _held = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    if forgotten.load(Ordering::SeqCst) || cleared_since(mark, file) {
        return Ok(false);
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension(format!("tmp{}", u32::from_le_bytes(crate::oauth::random::<4>())));
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

/// The tree kept in `file`.
pub fn read(file: &Path, secret: &Secret) -> Result<Tree, Refusal> {
    let data = match std::fs::read(file) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Refusal::Missing),
        Err(e) => return Err(Refusal::Io(e.to_string())),
    };
    let body = open(secret, data)?;
    let snap: Snapshot = serde_json::from_slice(&body).map_err(|_| Refusal::Malformed)?;
    Tree::from_snapshot(snap).ok_or(Refusal::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::tests::{dir as folder, file};
    use crate::tree::Seen;

    fn tree() -> Tree {
        let mut t = Tree::default();
        let r = t.set_root(&Seen { root: true, ..folder("R", "", "") });
        t.set_listing(r, &[folder("D", "R", "Relatórios"), file("x", "R", "x.txt")]);
        t.delta = Some("https://graph/delta?token=1".into());
        t
    }

    #[test]
    fn a_sealed_tree_opens_only_with_its_key_and_unchanged() {
        let dir = std::env::temp_dir().join(format!("oe-onedrive-cache-{}", std::process::id()));
        let f = file_in(&dir, "d1");
        let secret = Secret::of("d1", &[7; 32]);
        assert!(write(&f, &secret, &tree().snapshot(), &AtomicBool::new(false), mark()).unwrap());
        let raw = std::fs::read(&f).unwrap();
        assert!(!raw.windows(5).any(|w| w == b"x.txt"), "names are sealed");
        assert_eq!(read(&f, &secret).unwrap().snapshot(), tree().snapshot());
        assert_eq!(read(&f, &Secret::of("d1", &[8; 32])).err(), Some(Refusal::OtherKey));
        assert_eq!(read(&f, &Secret::of("d2", &[7; 32])).err(), Some(Refusal::OtherKey));
        let mut bad = raw.clone();
        let n = bad.len();
        bad[n - 20] ^= 1;
        std::fs::write(&f, &bad).unwrap();
        assert_eq!(read(&f, &secret).err(), Some(Refusal::Tampered));
        // Removed meanwhile, or the folder cleared: nothing is written.
        assert!(!write(&f, &secret, &tree().snapshot(), &AtomicBool::new(true), mark()).unwrap());
        let m = mark();
        clear_in(&dir, |d| std::fs::remove_dir_all(d)).unwrap();
        assert!(!write(&f, &secret, &tree().snapshot(), &AtomicBool::new(false), m).unwrap());
        assert_eq!(read(&f, &secret).err(), Some(Refusal::Missing));
    }
}
