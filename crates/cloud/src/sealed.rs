//! A cloud account's tree or listings kept on disk between runs, sealed: what the MEGA,
//! Dropbox, B2 and OneDrive crates' `cache` modules have in common.
//!
//! # Format
//!
//! One file per account in the folder the app sets ([`Store::set_dir`]; with none set, nothing
//! is kept), named by a hash of the account's id so the folder doesn't list e-mails or keys, and
//! written whole and atomically (a temporary file renamed over the old one): a crash midway
//! leaves the old file or the new, never half of one.
//!
//! ```text
//! magic (8) | format (u32 LE) | key id (16) | nonce (12) | AES-256-GCM(body) | tag (16)
//! ```
//!
//! Each crate has its own magic, format number and body. The key is derived (HKDF-SHA256) from
//! something only the OS credential store gives back, so the file alone opens nothing; the key
//! id, derived apart from the key, tells a file sealed under another key (a new login, another
//! account) from a damaged one without decrypting it. The magic, the format and the key id are
//! authenticated with the body, so a changed byte anywhere is refused.
//!
//! # Clearing and forgetting
//!
//! Opções › Cache clears every crate's folder ([`Store::clear_with`]) with writes held off. No
//! account open then writes into the folder again this run: the crates that keep a file from an
//! account's start note [`Store::mark`] as it starts, B2 as it takes each snapshot, and
//! [`Store::write_unless`] writes nothing for a mark older than the last clear. What the open
//! accounts hold in memory follows the crate's [`OnClear`]. Removing an account deletes its file
//! after any write in flight ([`Store::forget_file`]).

use std::io::{Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use aes_gcm::KeyInit;
use aes_gcm::aead::AeadInOut;
use sha2::Digest;

/// The bytes before the body: magic, format, key id and nonce.
pub const HEADER: usize = 8 + 4 + 16 + 12;
/// The bytes after it: GCM's tag.
pub const TAG: usize = 16;
/// What the tag authenticates besides the body: the header but the nonce.
const AUTHENTICATED: usize = 8 + 4 + 16;

/// Why a kept file wasn't used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No file.
    Missing,
    /// It couldn't be read.
    Io(String),
    /// Not one of these files, or of another format.
    Format,
    /// Sealed under another key: another login or session, another account, an account removed
    /// and added again.
    OtherKey,
    /// Changed since it was written: the authentication failed.
    Tampered,
    /// Opened, but its body doesn't read.
    Malformed,
}

/// What seals and opens one account's file.
#[derive(Clone)]
pub struct Secret {
    pub key: [u8; 32],
    pub id: [u8; 16],
}

impl Secret {
    /// The key and the key id HKDF-SHA256 derives from `ikm` with `salt`, under the infos
    /// "`label`: key" and "`label`: key id".
    pub fn derive(label: &str, salt: &[u8], ikm: &[u8]) -> Secret {
        let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(salt), ikm);
        let mut key = [0u8; 32];
        let mut id = [0u8; 16];
        hk.expand(format!("{label}: key").as_bytes(), &mut key).expect("32 bytes is a valid length");
        hk.expand(format!("{label}: key id").as_bytes(), &mut id).expect("16 bytes is a valid length");
        Secret { key, id }
    }
}

/// What a clear ([`Store::clear_with`]) does to what the accounts open now hold in memory.
#[derive(Clone, Copy)]
pub enum OnClear {
    /// It goes too, by this function: each folder is read from the service again when it opens.
    Forget(fn()),
    /// It stays: it isn't a copy of the file but the account's own state, which its change log
    /// keeps current, and dropping it would only read the whole account again. Those accounts
    /// just never write into the cleared folder again this run.
    Keep,
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the system's random generator");
    b
}

/// One crate's kept files: their folder and format, and the locks that order writes, forgets
/// and clears. Each crate has one, a `static`.
pub struct Store {
    magic: &'static [u8; 8],
    format: u32,
    /// The files' extension, without the dot.
    ext: &'static str,
    on_clear: OnClear,
    dir: Mutex<Option<PathBuf>>,
    /// One file is written at a time, so an older snapshot never lands over a newer one, and a
    /// forget or a clear waits for a write in flight (then the file it renamed goes too).
    writing: Mutex<()>,
    /// How many clears ran, and the folder of each.
    clears: Mutex<(u64, Vec<(u64, PathBuf)>)>,
}

impl Store {
    pub const fn new(magic: &'static [u8; 8], format: u32, ext: &'static str, on_clear: OnClear) -> Store {
        Store { magic, format, ext, on_clear, dir: Mutex::new(None), writing: Mutex::new(()), clears: Mutex::new((0, Vec::new())) }
    }

    pub fn format(&self) -> u32 {
        self.format
    }

    // -----------------------------------------------------------------------------------------
    // Where

    /// The folder the files are kept in; `None` keeps none (the default, and tests').
    pub fn set_dir(&self, dir: Option<PathBuf>) {
        *self.dir.lock().unwrap_or_else(|e| e.into_inner()) = dir;
    }

    pub fn dir(&self) -> Option<PathBuf> {
        self.dir.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The file of the account whose id is `name` (already in its one spelling) in `dir`.
    pub fn file_in(&self, dir: &Path, name: &str) -> PathBuf {
        let digest = sha2::Sha256::digest(name.as_bytes());
        let hash: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
        dir.join(format!("{hash}.{}", self.ext))
    }

    /// [`Store::file_in`] the folder set, if one is.
    pub fn file_of(&self, name: &str) -> Option<PathBuf> {
        self.dir().map(|d| self.file_in(&d, name))
    }

    // -----------------------------------------------------------------------------------------
    // Clearing and forgetting

    /// What an account notes before reading its file (or B2, before a snapshot):
    /// [`Store::cleared_since`] tells whether the file's folder was cleared after.
    pub fn mark(&self) -> u64 {
        self.clears.lock().unwrap_or_else(|e| e.into_inner()).0
    }

    /// Whether the folder `file` is in was cleared after [`Store::mark`] gave `mark`.
    pub fn cleared_since(&self, mark: u64, file: &Path) -> bool {
        self.clears.lock().unwrap_or_else(|e| e.into_inner()).1.iter().any(|(n, dir)| *n > mark && file.starts_with(dir))
    }

    /// Runs `clear` on the folder set, with every write held off and after what [`OnClear`]
    /// says of memory: what an account open now holds is never written there again this run, so
    /// the folder stays clear. `None` with no folder set.
    pub fn clear_with<R>(&self, clear: impl FnOnce(&Path) -> R) -> Option<R> {
        let dir = self.dir()?;
        Some(self.clear_in(&dir, clear))
    }

    /// [`Store::clear_with`] on folder `dir`.
    pub fn clear_in<R>(&self, dir: &Path, clear: impl FnOnce(&Path) -> R) -> R {
        // A write in flight lands first and is deleted with the rest; one after sees the clear
        // (it checks under the same lock). Memory goes before the clear is noted, so a snapshot
        // marked after it holds nothing from before.
        let _held = self.writing.lock().unwrap_or_else(|e| e.into_inner());
        if let OnClear::Forget(forget) = self.on_clear {
            forget();
        }
        {
            let mut clears = self.clears.lock().unwrap_or_else(|e| e.into_inner());
            clears.0 += 1;
            let n = clears.0;
            clears.1.push((n, dir.to_path_buf()));
        }
        clear(dir)
    }

    /// Deletes `file`, after any write in flight, and any temporary file a write cut short (a
    /// crash) left beside it.
    pub fn forget_file(&self, file: &Path) {
        let _held = self.writing.lock().unwrap_or_else(|e| e.into_inner());
        let _ = std::fs::remove_file(file);
        let (Some(dir), Some(stem)) = (file.parent(), file.file_stem().and_then(|s| s.to_str())) else { return };
        let prefix = format!("{stem}.tmp");
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if entry.file_name().to_str().is_some_and(|n| n.starts_with(&prefix)) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Sealing

    /// Seals in place a body whose first [`HEADER`] bytes are room for the header: the file's
    /// whole contents. A body built that way (MEGA's tree, over 100 MB on a big account) isn't
    /// copied.
    pub fn seal(&self, secret: &Secret, mut file: Vec<u8>) -> Vec<u8> {
        let nonce = random::<12>();
        file[..8].copy_from_slice(self.magic);
        file[8..12].copy_from_slice(&self.format.to_le_bytes());
        file[12..AUTHENTICATED].copy_from_slice(&secret.id);
        file[AUTHENTICATED..HEADER].copy_from_slice(&nonce);
        let gcm = aes_gcm::Aes256Gcm::new_from_slice(&secret.key).expect("a 32-byte key");
        let (head, body) = file.split_at_mut(HEADER);
        let tag = gcm.encrypt_inout_detached((&nonce[..]).try_into().expect("12 bytes"), &head[..AUTHENTICATED], body.into()).expect("a body GCM takes");
        file.extend_from_slice(&tag);
        file
    }

    /// [`Store::seal`] of `body`, copied after room for the header.
    pub fn seal_copy(&self, secret: &Secret, body: &[u8]) -> Vec<u8> {
        let mut file = Vec::with_capacity(HEADER + body.len() + TAG);
        file.resize(HEADER, 0);
        file.extend_from_slice(body);
        self.seal(secret, file)
    }

    /// Whether a file's first bytes are of this format and this secret's.
    fn check_header(&self, secret: &Secret, head: &[u8]) -> Result<(), Refusal> {
        if head.len() < HEADER || &head[..8] != self.magic || head[8..12] != self.format.to_le_bytes() {
            return Err(Refusal::Format);
        }
        if head[12..AUTHENTICATED] != secret.id {
            return Err(Refusal::OtherKey);
        }
        Ok(())
    }

    /// Opens a file [`Store::seal`] made, in place: where the body is.
    pub fn open(&self, secret: &Secret, file: &mut [u8]) -> Result<Range<usize>, Refusal> {
        if file.len() < HEADER + TAG {
            return Err(Refusal::Format);
        }
        self.check_header(secret, &file[..HEADER])?;
        let end = file.len() - TAG;
        let tag: [u8; TAG] = file[end..].try_into().expect("16 bytes");
        let (head, rest) = file.split_at_mut(HEADER);
        let gcm = aes_gcm::Aes256Gcm::new_from_slice(&secret.key).expect("a 32-byte key");
        gcm.decrypt_inout_detached((&head[AUTHENTICATED..]).try_into().expect("12 bytes"), &head[..AUTHENTICATED], (&mut rest[..end - HEADER]).into(), (&tag[..]).try_into().expect("16 bytes"))
            .map_err(|_| Refusal::Tampered)?;
        Ok(HEADER..end)
    }

    // -----------------------------------------------------------------------------------------
    // Files

    /// Writes `sealed` to `file` whole (a temporary file renamed over the old one), unless by
    /// the time it's its turn `forgotten` is on (the account was removed meanwhile) or the
    /// folder was cleared after `mark`. Whether it wrote.
    pub fn write_unless(&self, file: &Path, sealed: &[u8], forgotten: &AtomicBool, mark: u64) -> std::io::Result<bool> {
        let _held = self.writing.lock().unwrap_or_else(|e| e.into_inner());
        if forgotten.load(Ordering::SeqCst) || self.cleared_since(mark, file) {
            return Ok(false);
        }
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = file.with_extension(format!("tmp{}", u32::from_le_bytes(random::<4>())));
        let result = (|| {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(sealed)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, file)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.map(|()| true)
    }

    /// The file `file` opened: its bytes, and where the body is in them. The header is read
    /// first, so a file of another key or format is refused without reading the rest.
    pub fn read(&self, file: &Path, secret: &Secret) -> Result<(Vec<u8>, Range<usize>), Refusal> {
        let io = |e: std::io::Error| Refusal::Io(e.to_string());
        let mut f = match std::fs::File::open(file) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Refusal::Missing),
            Err(e) => return Err(io(e)),
        };
        let mut data = vec![0; HEADER];
        match f.read_exact(&mut data) {
            Ok(()) => self.check_header(secret, &data)?,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(Refusal::Format),
            Err(e) => return Err(io(e)),
        }
        let len = f.metadata().map_err(io)?.len();
        data.reserve(usize::try_from(len).unwrap_or(0).saturating_sub(HEADER));
        f.read_to_end(&mut data).map_err(io)?;
        let body = self.open(secret, &mut data)?;
        Ok((data, body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static STORE: Store = Store::new(b"OETEST0\0", 3, "kept", OnClear::Keep);

    fn secret() -> Secret {
        Secret::derive("OpenExplorer test cache", b"salt", b"ikm")
    }

    #[test]
    fn a_changed_byte_anywhere_or_another_key_is_refused() {
        let sealed = STORE.seal_copy(&secret(), b"some body, long enough to sample");
        for i in (0..sealed.len()).step_by(3).chain([sealed.len() - 1]) {
            let mut bad = sealed.clone();
            bad[i] ^= 0x01;
            let want = match i {
                0..12 => Refusal::Format,
                12..28 => Refusal::OtherKey,
                _ => Refusal::Tampered,
            };
            assert_eq!(STORE.open(&secret(), &mut bad).err(), Some(want), "byte {i}");
        }
        let other = Secret::derive("OpenExplorer test cache", b"salt", b"other");
        assert_eq!(STORE.open(&other, &mut sealed.clone()).err(), Some(Refusal::OtherKey));
        let forged = Secret { key: other.key, id: secret().id };
        assert_eq!(STORE.open(&forged, &mut sealed.clone()).err(), Some(Refusal::Tampered));
        assert_eq!(STORE.open(&secret(), &mut sealed[..HEADER + TAG - 1].to_vec()).err(), Some(Refusal::Format));
        let mut good = sealed;
        let body = STORE.open(&secret(), &mut good).unwrap();
        assert_eq!(&good[body], b"some body, long enough to sample");
    }

    #[test]
    fn writes_stop_for_a_forgotten_account_and_after_a_clear() {
        let dir = std::env::temp_dir().join(format!("oe-sealed-{}", std::process::id()));
        let file = STORE.file_in(&dir, "conta");
        assert!(file.to_string_lossy().ends_with(".kept"));
        let sealed = STORE.seal_copy(&secret(), b"corpo");
        assert!(STORE.write_unless(&file, &sealed, &AtomicBool::new(false), STORE.mark()).unwrap());
        let (data, body) = STORE.read(&file, &secret()).unwrap();
        assert_eq!(&data[body], b"corpo");
        assert!(!STORE.write_unless(&file, &sealed, &AtomicBool::new(true), STORE.mark()).unwrap());
        let before = STORE.mark();
        STORE.clear_in(&dir, |d| std::fs::remove_dir_all(d).unwrap());
        assert_eq!(STORE.read(&file, &secret()).err(), Some(Refusal::Missing));
        assert!(!STORE.write_unless(&file, &sealed, &AtomicBool::new(false), before).unwrap());
        assert!(STORE.write_unless(&file, &sealed, &AtomicBool::new(false), STORE.mark()).unwrap());
        let stray = file.with_extension("tmp42");
        std::fs::write(&stray, b"half").unwrap();
        STORE.forget_file(&file);
        assert!(!file.exists() && !stray.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
