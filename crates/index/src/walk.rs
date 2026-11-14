//! A [`Tree`] from walking folders, as any user can: what the service does when it can't read a
//! volume's MFT (not NTFS), and how the index is tried without being an administrator
//! (`openexplorer-index console --walk`, the timing example).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::UNIX_EPOCH;

use crate::tree::{self, ROOT, Tree};

/// Folders listed at once.
const WALKERS: usize = 16;

struct Item {
    record: u32,
    parent: u32,
    name: String,
    flags: u8,
    size: u64,
    modified: u32,
}

#[cfg(windows)]
fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &std::fs::Metadata) -> bool {
    false
}

/// Every file and folder under `root` ("C:\"), listed [`WALKERS`] folders at a time. Folders
/// that can't be read are left out; links to folders aren't followed.
pub fn walk(root: &str) -> Tree {
    let next = AtomicU32::new(ROOT + 19);
    let queue = Mutex::new((VecDeque::from([(PathBuf::from(root), ROOT)]), 0usize));
    let ready = Condvar::new();
    let found = Mutex::new(Vec::<Vec<Item>>::new());
    std::thread::scope(|s| {
        for _ in 0..WALKERS {
            s.spawn(|| {
                let mut mine = Vec::new();
                loop {
                    let (dir, parent) = {
                        let mut q = queue.lock().unwrap();
                        loop {
                            if let Some(d) = q.0.pop_front() {
                                q.1 += 1;
                                break d;
                            }
                            if q.1 == 0 {
                                ready.notify_all();
                                found.lock().unwrap().push(mine);
                                return;
                            }
                            q = ready.wait(q).unwrap();
                        }
                    };
                    let mut sub = Vec::new();
                    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                        let Ok(meta) = e.metadata() else { continue };
                        let record = next.fetch_add(1, Ordering::Relaxed);
                        // Junctions and links to folders aren't followed: what they point to is
                        // listed where it really is (as the MFT has it).
                        let is_dir = meta.is_dir() && !meta.is_symlink() && !is_reparse_point(&meta);
                        if is_dir {
                            sub.push((e.path(), record));
                        }
                        let modified = meta.modified().ok().and_then(|m| m.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs().min(u32::MAX as u64) as u32);
                        mine.push(Item { record, parent, name: e.file_name().to_string_lossy().into_owned(), flags: if is_dir { tree::DIR } else { 0 }, size: if is_dir { 0 } else { meta.len() }, modified });
                    }
                    let mut q = queue.lock().unwrap();
                    q.0.extend(sub);
                    q.1 -= 1;
                    drop(q);
                    ready.notify_all();
                }
            });
        }
    });
    let mut t = Tree::new(root);
    let mut all = found.into_inner().unwrap().into_iter().flatten().collect::<Vec<_>>();
    // In record order, so the arenas are laid out as an MFT read lays them.
    all.sort_unstable_by_key(|i| i.record);
    t.reserve(next.into_inner() as usize, all.iter().map(|i| i.name.len()).sum());
    for i in all {
        t.set(i.record, i.parent, &i.name, i.flags, i.size, i.modified);
    }
    t.shrink();
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Query;

    #[test]
    fn a_walked_folder_is_searchable() {
        let dir = std::env::temp_dir().join(format!("oe-walk-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Sub Pasta")).unwrap();
        std::fs::write(dir.join("Sub Pasta").join("Relatório.txt"), "abc").unwrap();
        std::fs::write(dir.join("outro.bin"), "").unwrap();
        let sep = std::path::MAIN_SEPARATOR;
        let root = format!("{}{sep}", dir.display());
        let t = walk(&root);
        assert_eq!(t.len(), 3);
        let mut out = Vec::new();
        assert_eq!(t.search(&Query::new("relatorio").unwrap(), None, 10, 2, &mut out), 1);
        let row = t.row(out[0]).unwrap();
        assert_eq!(row.path, format!("{root}Sub Pasta{sep}Relatório.txt"));
        assert_eq!((row.is_dir, row.size), (false, 3));
        assert!(t.find(&format!("{root}Sub Pasta")).is_some());
        // Windows names a folder whatever the case; Linux only exactly.
        assert_eq!(t.find(&format!("{root}sub pasta")).is_some(), cfg!(windows));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
