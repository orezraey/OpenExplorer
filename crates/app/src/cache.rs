//! The last listing of each Drive folder, so a folder opened before shows at once while the
//! fresh listing loads behind it. Kept in memory for the session and on disk under
//! `%LOCALAPPDATA%\OpenExplorer\cache`, one folder per account, so it also survives a restart.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};

use serde::{Deserialize, Serialize};

use crate::model::{Entry, Location, Space};

/// Bumped when [`Entry`]'s shape changes, so old files are ignored instead of misread.
const FORMAT: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    email: String,
    /// The space and folder, e.g. `my/root`, `sd/<drive>/<folder>`, `sd-list`.
    folder: String,
}

/// The cache key of a Drive location; local and simulated folders aren't cached.
pub fn key(loc: &Location) -> Option<Key> {
    let Location::Drive { email, space, path } = loc else { return None };
    let folder = path.last().map(|(id, _)| id.as_str());
    let folder = match space {
        Space::MyDrive => format!("my/{}", folder.unwrap_or("root")),
        Space::SharedDrives => "sd-list".to_string(),
        Space::Shared { id, .. } => format!("sd/{id}/{}", folder.unwrap_or(id)),
    };
    Some(Key { email: email.clone(), folder })
}

/// Disk work, done in order on one thread: a write never lands after the forget that should
/// have removed it, and two writes of one folder never interleave.
enum Job {
    Put(PathBuf, Arc<Vec<Entry>>),
    Forget(PathBuf),
}

#[derive(Default)]
pub struct Cache {
    mem: HashMap<Key, Arc<Vec<Entry>>>,
    disk: Option<Sender<Job>>,
}

#[derive(Serialize)]
struct FileOut<'a> {
    format: u32,
    entries: &'a [Entry],
}

#[derive(Deserialize)]
struct FileIn {
    format: u32,
    entries: Vec<Entry>,
}

fn hash(s: &str) -> String {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn account_dir(email: &str) -> PathBuf {
    openexplorer_gdrive::local_dir().join("cache").join(hash(email))
}

fn path(k: &Key) -> PathBuf {
    account_dir(&k.email).join(format!("{}.json", hash(&k.folder)))
}

fn run(job: Job) {
    match job {
        Job::Put(file, entries) => {
            let Ok(text) = serde_json::to_vec(&FileOut { format: FORMAT, entries: &entries }) else { return };
            if let Some(dir) = file.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            // Written aside and renamed, so a crash mid-write never leaves half a file.
            let tmp = file.with_extension("tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &file);
            }
        }
        Job::Forget(dir) => {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

impl Cache {
    pub fn get(&self, k: &Key) -> Option<&Arc<Vec<Entry>>> {
        self.mem.get(k)
    }

    fn disk(&mut self, job: Job) {
        let tx = self.disk.get_or_insert_with(|| {
            let (tx, rx) = channel();
            std::thread::spawn(move || rx.into_iter().for_each(run));
            tx
        });
        let _ = tx.send(job);
    }

    /// Keeps a complete listing, and saves it to disk unless it's what was already kept.
    pub fn put(&mut self, k: Key, entries: Vec<Entry>) {
        if self.mem.get(&k).is_some_and(|old| **old == entries) {
            return;
        }
        let entries = Arc::new(entries);
        self.disk(Job::Put(path(&k), entries.clone()));
        self.mem.insert(k, entries);
    }

    /// Drops an account's listings, in memory and on disk (on disconnecting it).
    pub fn forget(&mut self, email: &str) {
        self.mem.retain(|k, _| k.email != email);
        self.disk(Job::Forget(account_dir(email)));
    }
}

/// A listing saved by an earlier run. Reads a file, so it runs off the UI thread.
pub fn load_disk(k: &Key) -> Option<Vec<Entry>> {
    let bytes = std::fs::read(path(k)).ok()?;
    let f: FileIn = serde_json::from_slice(&bytes).ok()?;
    (f.format == FORMAT).then_some(f.entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_drive_folder_has_its_own_key_and_local_folders_none() {
        let my = |path: Vec<(String, String)>| Location::Drive { email: "a@b".into(), space: Space::MyDrive, path };
        let shared = Space::Shared { id: "0AAx".into(), name: "Equipe".into() };
        let k = |l: &Location| key(l).map(|k| k.folder);
        assert_eq!(k(&my(vec![])).as_deref(), Some("my/root"));
        assert_eq!(k(&my(vec![("f1".into(), "Fotos".into())])).as_deref(), Some("my/f1"));
        assert_eq!(k(&Location::drive("a@b", Space::SharedDrives)).as_deref(), Some("sd-list"));
        assert_eq!(k(&Location::drive("a@b", shared.clone())).as_deref(), Some("sd/0AAx/0AAx"));
        assert_eq!(k(&Location::Drive { email: "a@b".into(), space: shared, path: vec![("f2".into(), "Docs".into())] }).as_deref(), Some("sd/0AAx/f2"));
        assert_ne!(key(&my(vec![])), key(&Location::Drive { email: "c@d".into(), space: Space::MyDrive, path: vec![] }));
        assert!(key(&Location::Mock(vec![])).is_none());
    }
}
