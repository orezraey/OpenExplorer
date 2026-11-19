//! Which file on this computer each large upload the app started is sending, so one that stopped
//! midway (no network, the app closed) can be taken up again from the Envios inacabados dialog.
//!
//! The path stays on this computer, in `b2-uploads.json` beside the keys' list: B2 only learns
//! the size and part size the upload notes in its `fileInfo` (`oe_size`, `oe_part`), which a
//! resume matches. A note goes when its upload ends or is cancelled, and after [`KEEP`].

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// How long a note of an upload that never ended is kept.
pub const KEEP: u64 = 60 * 24 * 3600;
/// The most notes kept: the oldest go first.
pub const MOST: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub key_id: String,
    pub bucket: String,
    /// The file's name in the bucket.
    pub name: String,
    pub size: u64,
    /// The file on this computer.
    pub local: PathBuf,
    /// When it was started, seconds since 1970.
    pub at: u64,
}

static FILE: Mutex<Option<PathBuf>> = Mutex::new(None);
/// Held by the tests that set [`FILE`], one at a time.
#[cfg(test)]
pub(crate) static TESTS: Mutex<()> = Mutex::new(());
/// Reads and writes of the file, one at a time.
static HELD: Mutex<()> = Mutex::new(());

/// Where the notes are kept; `None` keeps none (the default, and tests').
pub fn set_file(file: Option<PathBuf>) {
    *FILE.lock().unwrap_or_else(|e| e.into_inner()) = file;
}

fn file() -> Option<PathBuf> {
    FILE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn read(file: &Path) -> Vec<Note> {
    std::fs::read(file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write(file: &Path, notes: &[Note]) {
    let Ok(json) = serde_json::to_vec_pretty(notes) else { return };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = file.with_extension("json.tmp");
    if std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, file)).is_err() {
        let _ = std::fs::remove_file(&tmp);
        crate::api::log("B2: não consegui gravar as notas dos envios grandes");
    }
}

fn change(edit: impl FnOnce(&mut Vec<Note>)) {
    let Some(file) = file() else { return };
    let _held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    let mut notes = read(&file);
    edit(&mut notes);
    let old = crate::cache::now().saturating_sub(KEEP);
    notes.retain(|n| n.at >= old);
    if notes.len() > MOST {
        notes.sort_by_key(|n| std::cmp::Reverse(n.at));
        notes.truncate(MOST);
    }
    write(&file, &notes);
}

fn same(n: &Note, key_id: &str, bucket: &str, name: &str, size: u64) -> bool {
    n.key_id == key_id && n.bucket == bucket && n.name == name && n.size == size
}

/// Notes that the upload of `local` to `name` in `bucket` (`size` bytes) began.
pub fn begin(key_id: &str, bucket: &str, name: &str, size: u64, local: &Path) {
    let note = Note { key_id: key_id.into(), bucket: bucket.into(), name: name.into(), size, local: local.to_path_buf(), at: crate::cache::now() };
    change(|notes| {
        notes.retain(|n| !same(n, key_id, bucket, name, size));
        notes.push(note);
    });
}

/// The upload ended (or was cancelled): its note goes.
pub fn end(key_id: &str, bucket: &str, name: &str, size: u64) {
    change(|notes| notes.retain(|n| !same(n, key_id, bucket, name, size)));
}

/// The file on this computer an unfinished upload of `name` (`size` bytes) was sending, if a
/// note says and the file is still there with that size.
pub fn local_of(key_id: &str, bucket: &str, name: &str, size: u64) -> Option<PathBuf> {
    let file = file()?;
    let notes = {
        let _held = HELD.lock().unwrap_or_else(|e| e.into_inner());
        read(&file)
    };
    let n = notes.into_iter().rev().find(|n| same(n, key_id, bucket, name, size))?;
    (std::fs::metadata(&n.local).ok()?.len() == size).then_some(n.local)
}

/// Whether this computer started an upload of `name` (`size` bytes) that hasn't ended: a note
/// says so, wherever the file is now.
pub fn noted(key_id: &str, bucket: &str, name: &str, size: u64) -> bool {
    let Some(file) = file() else { return false };
    let _held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    read(&file).iter().any(|n| same(n, key_id, bucket, name, size))
}

/// Forgets every note of key `key_id` (the key was removed).
pub fn forget_key(key_id: &str) {
    change(|notes| notes.retain(|n| n.key_id != key_id));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_note_finds_the_file_while_it_has_the_same_size() {
        let dir = std::env::temp_dir().join(format!("oe-b2-notes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let local = dir.join("video.mp4");
        std::fs::write(&local, vec![7u8; 1000]).unwrap();
        let _one = TESTS.lock().unwrap_or_else(|e| e.into_inner());
        set_file(Some(dir.join("b2-uploads.json")));
        begin("k", "fotos", "2024/video.mp4", 1000, &local);
        assert_eq!(local_of("k", "fotos", "2024/video.mp4", 1000), Some(local.clone()));
        assert_eq!(local_of("k", "fotos", "2024/video.mp4", 999), None, "another size is another upload");
        assert_eq!(local_of("outra", "fotos", "2024/video.mp4", 1000), None);
        // The file changed on the computer: no resume from it.
        std::fs::write(&local, vec![7u8; 10]).unwrap();
        assert_eq!(local_of("k", "fotos", "2024/video.mp4", 1000), None);
        end("k", "fotos", "2024/video.mp4", 1000);
        assert!(read(&dir.join("b2-uploads.json")).is_empty());
        begin("k", "fotos", "a", 1, &local);
        forget_key("k");
        assert!(read(&dir.join("b2-uploads.json")).is_empty());
        set_file(None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
