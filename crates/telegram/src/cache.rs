//! An account's tree kept on disk between runs, sealed, so a reopened account shows its folders
//! at once and asks the channel only for what changed (one small request) instead of reading its
//! whole history again (about 450 messages a second).
//!
//! # On disk
//!
//! One file per account, `<hash of the user id>.tree`, in the folder the app sets with
//! [`set_dir`] (with none set, nothing is kept), written whole and atomically by
//! `openexplorer_cloud::sealed`:
//!
//! ```text
//! magic "OETGTRE\0" | format (u32 LE) | key id (16) | nonce (12) | AES-256-GCM(JSON body) | tag (16)
//! ```
//!
//! The body is the change log's position and the messages the tree is made of. Names are
//! private, so the key is derived from the session's authorization key (in the OS credential
//! store), salted with the user id: the file alone opens nothing, and a new login can't open an
//! old file, which is then read from the channel again.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use openexplorer_cloud::sealed::{OnClear, Secret, Store};
use serde::{Deserialize, Serialize};

use crate::tree::Msg;

/// Bumped when the body's shape changes: a file of another format is thrown away.
pub const FORMAT: u32 = 1;

/// The tree is the account's own state, kept current by the change log: a clear leaves it in
/// memory and only keeps it off the disk for the rest of the run.
static STORE: Store = Store::new(b"OETGTRE\0", FORMAT, "tree", OnClear::Keep);

#[derive(Serialize, Deserialize)]
struct Body {
    pts: i32,
    msgs: Vec<Msg>,
}

/// Keeps the files in `dir` from now on (`%LOCALAPPDATA%\OpenExplorer\telegram` in the app).
pub fn set_dir(dir: Option<PathBuf>) {
    STORE.set_dir(dir);
}

pub fn dir() -> Option<PathBuf> {
    STORE.dir()
}

pub fn file_of(user_id: i64) -> Option<PathBuf> {
    STORE.file_of(&user_id.to_string())
}

pub fn secret(user_id: i64, auth_key: &[u8]) -> Secret {
    Secret::derive("OpenExplorer Telegram tree cache", &user_id.to_le_bytes(), auth_key)
}

pub fn mark() -> u64 {
    STORE.mark()
}

/// The change log's position and the messages kept in `file`, if it opens.
pub fn load(file: &Path, secret: &Secret) -> Option<(i32, Vec<Msg>)> {
    let (data, body) = STORE.read(file, secret).ok()?;
    let b: Body = serde_json::from_slice(&data[body]).ok()?;
    Some((b.pts, b.msgs))
}

/// Writes the tree, unless the account was removed meanwhile or the folder was cleared after
/// `mark`. Whether it wrote.
pub fn save<'a>(file: &Path, secret: &Secret, pts: i32, msgs: impl Iterator<Item = &'a Msg>, forgotten: &AtomicBool, mark: u64) -> bool {
    let body = Body { pts, msgs: msgs.cloned().collect() };
    let Ok(json) = serde_json::to_vec(&body) else { return false };
    let sealed = STORE.seal_copy(secret, &json);
    STORE.write_unless(file, &sealed, forgotten, mark).unwrap_or(false)
}

pub fn forget_file(file: &Path) {
    STORE.forget_file(file);
}

/// Runs `clear` on the folder (Opções › Cache), with writes held off; the open accounts keep
/// their trees in memory and write none there again this run. `None` with no folder set.
pub fn clear_with<R>(clear: impl FnOnce(&Path) -> R) -> Option<R> {
    STORE.clear_with(clear)
}
