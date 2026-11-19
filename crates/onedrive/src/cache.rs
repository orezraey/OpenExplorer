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

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use openexplorer_cloud::sealed::{self, OnClear, Store};

pub use openexplorer_cloud::sealed::Refusal;

use crate::tree::{Snapshot, Tree};

/// Bumped when the body's shape changes: a file of another format is thrown away.
pub const FORMAT: u32 = 1;

/// The files, `<hash of the drive id>.tree`. A clear leaves the trees the open accounts hold:
/// each is the account's live state, which its delta link keeps current, not a copy of its
/// file; those accounts just never write it into the cleared folder again this run.
static STORE: Store = Store::new(b"OEODTRE\0", FORMAT, "tree", OnClear::Keep);

/// What seals and opens one account's file.
#[derive(Clone)]
pub struct Secret(sealed::Secret);

impl Secret {
    pub fn of(drive: &str, cache_key: &[u8; 32]) -> Secret {
        Secret(sealed::Secret::derive("OpenExplorer OneDrive cache", drive.as_bytes(), cache_key))
    }
}

/// The folder the trees are kept in; `None` keeps none (the default, and tests').
pub fn set_dir(dir: Option<PathBuf>) {
    STORE.set_dir(dir);
}

pub fn dir() -> Option<PathBuf> {
    STORE.dir()
}

/// The file of drive `drive` in `dir`, named by a hash.
pub fn file_in(dir: &Path, drive: &str) -> PathBuf {
    STORE.file_in(dir, drive)
}

pub fn file_of(drive: &str) -> Option<PathBuf> {
    dir().map(|d| file_in(&d, drive))
}

/// Noted by an account as it starts (see [`cleared_since`]).
pub fn mark() -> u64 {
    STORE.mark()
}

pub fn cleared_since(mark: u64, file: &Path) -> bool {
    STORE.cleared_since(mark, file)
}

/// Runs `clear` on the folder set, every write held off; accounts open now never write there
/// again this run. `None` with no folder set.
pub fn clear_with<R>(clear: impl FnOnce(&Path) -> R) -> Option<R> {
    STORE.clear_with(clear)
}

pub fn clear_in<R>(dir: &Path, clear: impl FnOnce(&Path) -> R) -> R {
    STORE.clear_in(dir, clear)
}

/// Deletes `file` (and temporary files a crash left beside it), after any write in flight.
pub fn forget_file(file: &Path) {
    STORE.forget_file(file);
}

/// Writes `tree` to `file` whole, unless the account was removed (`forgotten`) or the folder
/// cleared after `mark` by the time it's its turn. Whether it wrote.
pub fn write(file: &Path, secret: &Secret, snap: &Snapshot, forgotten: &AtomicBool, mark: u64) -> std::io::Result<bool> {
    let body = serde_json::to_vec(snap).map_err(std::io::Error::other)?;
    let sealed = STORE.seal_copy(&secret.0, &body);
    STORE.write_unless(file, &sealed, forgotten, mark)
}

/// The tree kept in `file`.
pub fn read(file: &Path, secret: &Secret) -> Result<Tree, Refusal> {
    let (data, body) = STORE.read(file, &secret.0)?;
    let snap: Snapshot = serde_json::from_slice(&data[body]).map_err(|_| Refusal::Malformed)?;
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

    /// A file sealed before the shared crate (`openexplorer-cloud`) took over the sealing, and
    /// its name: both must still be what they were.
    #[test]
    fn a_file_kept_before_still_opens() {
        let hex = "4f454f44545245000100000040e4fa4cf24b2cd67e75352c2c056d16b6672432ca0ad990f788d2122d0d0ec511ff2b0106a181ff2a60f21042805c7cc4a9b2e3087d0eb9e308872366f38bcf2e2ee5cc71813397";
        let mut old: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
        let body = STORE.open(&Secret::of("d1", &[7; 32]).0, &mut old).unwrap();
        assert_eq!(&old[body], b"kept before the shared crate");
        assert_eq!(file_in(Path::new("d"), "d1"), Path::new("d").join("8b53639f152c8fc6ef30802fde462ba0.tree"));
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
