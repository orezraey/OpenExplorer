//! The last listing of each Drive folder, so a folder opened before shows at once while the
//! fresh listing loads behind it. Kept in memory for the session and on disk under
//! `%LOCALAPPDATA%\OpenExplorer\cache`, one folder per account, so it also survives a restart.
//!
//! A listing can carry the point of the account's Drive change log it's valid at (a [`Pos`]):
//! everything that changed since is in the log from there, so reading the log can prove the
//! folder unchanged and spare reading it whole (see [`crate::watch::verdict`]). The point is kept
//! beside the listing in a small file of its own, so moving it on doesn't rewrite a listing of
//! thousands.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};

use serde::{Deserialize, Serialize};

use crate::model::{Entry, Location, Space};

/// Bumped when [`Entry`]'s shape changes, so old files are ignored instead of misread.
/// 2: modified times as signed milliseconds.
const FORMAT: u32 = 2;

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
        // A trashed folder lists its trashed contents, not what a folder in My Drive shows.
        Space::Trash => folder.map_or_else(|| "view/trash".to_string(), |f| format!("trash/{f}")),
        // Results aren't kept: each search asks Drive again. A folder opened from them is.
        Space::Search(_) => format!("my/{}", folder?),
        // A folder opened from a view lists as it does anywhere else.
        Space::SharedWithMe | Space::Starred | Space::Recent => match folder {
            Some(f) => format!("my/{f}"),
            None => format!("view/{}", match space {
                Space::SharedWithMe => "shared-with-me",
                Space::Starred => "starred",
                _ => "recent",
            }),
        },
    };
    Some(Key { email: email.clone(), folder })
}

/// Where in the account's change log a listing is valid: no change after `token` is missing
/// from the listing's picture of the folder (it may already hold some made after).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pos {
    pub token: String,
    /// When the listing was last read whole from Drive, in seconds since 1970: past
    /// [`crate::watch::MAX_AGE`] it's read again whatever the log says.
    pub fetched: u64,
}

/// A folder's last listing, and where in the change log it's valid (`None`: unknown, so it's
/// read again on opening).
#[derive(Clone, Debug)]
pub struct Listing {
    pub entries: Arc<Vec<Entry>>,
    pub pos: Option<Pos>,
}

/// Disk work, done in order on one thread: a write never lands after the forget that should
/// have removed it, and two writes of one folder never interleave.
enum Job {
    /// The listing file and what to write there: the entries when they changed (else the
    /// listing already there, of `count` entries, stays), and the position (`None` removes it).
    Put { file: PathBuf, entries: Option<Arc<Vec<Entry>>>, count: usize, pos: Option<Pos> },
    Forget(PathBuf),
}

#[derive(Default)]
pub struct Cache {
    mem: HashMap<Key, Listing>,
    disk: Option<Sender<Job>>,
    /// Tests keep the cache in memory, away from the real cache folder.
    #[cfg(test)]
    memory_only: bool,
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

/// The position file beside a listing. `count` is the listing's length, a check that the two
/// files belong together.
#[derive(Serialize, Deserialize)]
struct PosFile {
    format: u32,
    count: usize,
    pos: Pos,
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

fn pos_path(listing: &Path) -> PathBuf {
    listing.with_extension("pos.json")
}

/// Writes `bytes` to `file` aside (as `ext`) and renames it, so a crash mid-write never leaves
/// half a file.
fn write(file: &Path, ext: &str, bytes: &[u8]) -> bool {
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = file.with_extension(ext);
    std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, file).is_ok()
}

fn run(job: Job) {
    match job {
        Job::Put { file, entries, count, pos } => {
            let pos_file = pos_path(&file);
            let listed = match &entries {
                Some(entries) => serde_json::to_vec(&FileOut { format: FORMAT, entries }).is_ok_and(|text| write(&file, "tmp", &text)),
                // The position belongs to the listing already on disk; with none there (the
                // account was disconnected meanwhile, say) there's none to keep.
                None => file.exists(),
            };
            if !listed {
                // Neither the old listing nor its position may outlive a failed write: the old
                // position would vouch for a listing that's no longer the one kept.
                if entries.is_some() {
                    let _ = std::fs::remove_file(&file);
                }
                let _ = std::fs::remove_file(&pos_file);
                return;
            }
            let kept = pos.is_some_and(|pos| serde_json::to_vec(&PosFile { format: FORMAT, count, pos }).is_ok_and(|text| write(&pos_file, "pos.tmp", &text)));
            if !kept {
                let _ = std::fs::remove_file(&pos_file);
            }
        }
        Job::Forget(dir) => {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

impl Cache {
    pub fn get(&self, k: &Key) -> Option<&Listing> {
        self.mem.get(k)
    }

    #[cfg(test)]
    pub fn memory_only() -> Cache {
        Cache { memory_only: true, ..Cache::default() }
    }

    fn disk(&mut self, job: Job) {
        #[cfg(test)]
        if self.memory_only {
            return;
        }
        let tx = self.disk.get_or_insert_with(|| {
            let (tx, rx) = channel();
            std::thread::spawn(move || rx.into_iter().for_each(run));
            tx
        });
        let _ = tx.send(job);
    }

    /// Keeps a complete listing valid at `pos`, and saves it to disk: the entries unless
    /// they're what was already kept, the position whenever it moved.
    pub fn put(&mut self, k: Key, entries: Arc<Vec<Entry>>, pos: Option<Pos>) {
        let (entries, changed) = match self.mem.get(&k) {
            Some(old) if old.entries == entries => {
                if old.pos == pos {
                    return;
                }
                (old.entries.clone(), false)
            }
            _ => (entries, true),
        };
        self.disk(Job::Put { file: path(&k), entries: changed.then(|| entries.clone()), count: entries.len(), pos: pos.clone() });
        self.mem.insert(k, Listing { entries, pos });
    }

    /// Keeps `listing`, which the change log proved still stands at its new position. Read from
    /// disk and not kept in memory yet, it's what the disk holds: only the position is written.
    pub fn keep(&mut self, k: Key, listing: Listing) {
        if self.mem.contains_key(&k) {
            return self.put(k, listing.entries, listing.pos);
        }
        self.disk(Job::Put { file: path(&k), entries: None, count: listing.entries.len(), pos: listing.pos.clone() });
        self.mem.insert(k, listing);
    }

    /// The listing of `k` is no longer known valid at its position (the app just changed the
    /// folder, and the change log may not show it yet): it's read again on opening.
    pub fn unverify(&mut self, k: &Key) {
        let count = match self.mem.get_mut(k) {
            Some(l) => {
                l.pos = None;
                l.entries.len()
            }
            None => 0,
        };
        self.disk(Job::Put { file: path(k), entries: None, count, pos: None });
    }

    /// Drops an account's listings, in memory and on disk (on disconnecting it).
    pub fn forget(&mut self, email: &str) {
        self.mem.retain(|k, _| k.email != email);
        self.disk(Job::Forget(account_dir(email)));
    }
}

/// A listing saved by an earlier run. Reads a file, so it runs off the UI thread.
pub fn load_disk(k: &Key) -> Option<Listing> {
    read(&path(k))
}

/// The listing saved at `file`, with its position when one was saved with it.
fn read(file: &Path) -> Option<Listing> {
    let bytes = std::fs::read(file).ok()?;
    let f: FileIn = serde_json::from_slice(&bytes).ok()?;
    if f.format != FORMAT {
        return None;
    }
    let pos = std::fs::read(pos_path(file))
        .ok()
        .and_then(|b| serde_json::from_slice::<PosFile>(&b).ok())
        .filter(|p| p.format == FORMAT && p.count == f.entries.len())
        .map(|p| p.pos);
    Some(Listing { entries: Arc::new(f.entries), pos })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(n: usize) -> Arc<Vec<Entry>> {
        Arc::new((0..n).map(|i| Entry { id: format!("f{i}"), name: format!("f{i}.txt"), is_dir: false, size: Some(1), modified: None, target: None, mime: None, md5: None, starred: false }).collect())
    }

    #[test]
    fn listings_with_times_before_1970_round_trip_through_the_disk_format() {
        use std::time::{Duration, UNIX_EPOCH};
        let at = |ms: i64| if ms >= 0 { UNIX_EPOCH + Duration::from_millis(ms as u64) } else { UNIX_EPOCH - Duration::from_millis(ms.unsigned_abs()) };
        let entries: Vec<Entry> = [Some(at(1_709_294_400_250)), Some(at(-86_400_000)), None]
            .into_iter()
            .enumerate()
            .map(|(i, modified)| Entry { id: format!("{i}"), name: format!("f{i}"), is_dir: false, size: None, modified, target: None, mime: None, md5: None, starred: false })
            .collect();
        let text = serde_json::to_vec(&FileOut { format: FORMAT, entries: &entries }).unwrap();
        let back: FileIn = serde_json::from_slice(&text).unwrap();
        assert_eq!(back.entries, entries);
    }

    #[test]
    fn a_listing_s_position_survives_a_restart_and_moves_without_rewriting_the_listing() {
        let dir = std::env::temp_dir().join(format!("oe-cache-test-{}", std::process::id()));
        let file = dir.join("folder.json");
        let at = |token: &str| Some(Pos { token: token.into(), fetched: 1_700_000_000 });
        let put = |entries: Option<Arc<Vec<Entry>>>, count: usize, pos: Option<Pos>| run(Job::Put { file: file.clone(), entries, count, pos });
        // A position with no listing beside it isn't written.
        put(None, 3, at("t0"));
        assert!(!pos_path(&file).exists());
        put(Some(entries(3)), 3, at("t1"));
        let back = read(&file).unwrap();
        assert_eq!((back.entries.len(), back.pos.clone()), (3, at("t1")));
        // After a restart the change log is caught up from the saved position before deciding.
        let folder = crate::watch::Provable { folder: Some("pasta".into()), above: Vec::new() };
        let verdict = crate::watch::verdict(Some(&back), &folder, 1_700_000_060, |token, _| if token == "t1" { Ok(Some((Vec::new(), "t2".into()))) } else { Err("HTTP 410".into()) }, || unreachable!());
        assert_eq!(verdict, crate::watch::Verdict::Unchanged(at("t2").unwrap()));
        // Proven unchanged later: only the position moves; the listing file isn't written again.
        let written = std::fs::metadata(&file).unwrap().modified().unwrap();
        put(None, 3, at("t2"));
        assert_eq!(read(&file).unwrap().pos, at("t2"));
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), written);
        // A position that doesn't belong to the listing beside it isn't trusted.
        put(Some(entries(4)), 4, None);
        assert_eq!(read(&file).unwrap().pos, None);
        std::fs::write(pos_path(&file), serde_json::to_vec(&PosFile { format: FORMAT, count: 3, pos: at("t3").unwrap() }).unwrap()).unwrap();
        assert_eq!(read(&file).unwrap().pos, None);
        // A listing from before positions existed still shows, with none.
        std::fs::remove_file(pos_path(&file)).unwrap();
        assert_eq!(read(&file).map(|l| (l.entries.len(), l.pos)), Some((4, None)));
        // Unverified: the position goes, the listing stays.
        put(Some(entries(4)), 4, at("t4"));
        put(None, 4, None);
        assert_eq!(read(&file).map(|l| (l.entries.len(), l.pos)), Some((4, None)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn put_moves_the_position_of_an_unchanged_listing_and_unverify_drops_it() {
        let mut c = Cache::memory_only();
        let k = key(&Location::drive("a@b", Space::MyDrive)).unwrap();
        let at = |token: &str| Some(Pos { token: token.into(), fetched: 1 });
        c.put(k.clone(), entries(2), at("t1"));
        c.put(k.clone(), entries(2), at("t2"));
        assert_eq!(c.get(&k).unwrap().pos, at("t2"));
        c.unverify(&k);
        assert_eq!(c.get(&k).unwrap().pos, None);
        assert_eq!(c.get(&k).unwrap().entries.len(), 2);
    }

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
        let view = |space: Space, path: Vec<(String, String)>| k(&Location::Drive { email: "a@b".into(), space, path });
        assert_eq!(view(Space::Starred, vec![]).as_deref(), Some("view/starred"));
        assert_eq!(view(Space::SharedWithMe, vec![]).as_deref(), Some("view/shared-with-me"));
        assert_eq!(view(Space::Recent, vec![]).as_deref(), Some("view/recent"));
        assert_eq!(view(Space::Trash, vec![]).as_deref(), Some("view/trash"));
        assert_eq!(view(Space::Starred, vec![("f1".into(), "Fotos".into())]).as_deref(), Some("my/f1"));
        assert_eq!(view(Space::Trash, vec![("f1".into(), "Fotos".into())]).as_deref(), Some("trash/f1"));
    }
}
