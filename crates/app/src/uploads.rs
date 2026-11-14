//! Uploads from the computer to Google Drive, as rows of the transfer queue.
//!
//! A job adds up each row's size (a quick local walk), then workers (Uploads simultâneos in
//! Opções, 4 unless changed) share one
//! list of work: making a Drive folder, which adds the folder's subfolders and files to the
//! list, and sending a file. Files start going up as soon as their folder exists, so a tree of
//! hundreds of folders shows progress from the first seconds instead of after all of them are
//! made. A row's bar counts the bytes of all its files, and its state counts items done.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use openexplorer_gdrive::Drive;

use crate::ops::{self, CANCELLED, Progress, Stage};
use crate::transfers::Batch;

/// One piece of work of a row.
enum Work {
    /// Make folder `name` (local `dir`) in Drive folder `parent`, then queue what's in it.
    Folder { row: usize, dir: PathBuf, name: String, parent: String },
    /// Send the file at `path` into Drive folder `parent` as `name`.
    File { row: usize, path: PathBuf, name: String, parent: String },
}

impl Work {
    fn row(&self) -> usize {
        match self {
            Work::Folder { row, .. } | Work::File { row, .. } => *row,
        }
    }
}

/// What a row still has going.
struct Row {
    /// Work queued or running.
    left: AtomicUsize,
    /// Some work was skipped (Cancelar, or after an error).
    skipped: AtomicBool,
    error: Mutex<Option<String>>,
}

/// The shared list of work, and how many workers are busy (more work may still come from them).
struct Queue {
    state: Mutex<(VecDeque<Work>, usize)>,
    ready: Condvar,
}

impl Queue {
    /// The next piece of work, waiting while busy workers may still add some; `None` when all
    /// is done.
    fn next(&self) -> Option<Work> {
        let mut s = self.state.lock().unwrap();
        loop {
            if let Some(w) = s.0.pop_front() {
                s.1 += 1;
                return Some(w);
            }
            if s.1 == 0 {
                return None;
            }
            s = self.ready.wait(s).unwrap();
        }
    }

    fn add(&self, work: impl IntoIterator<Item = Work>) {
        self.state.lock().unwrap().0.extend(work);
        self.ready.notify_all();
    }

    fn finished_one(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.1 -= 1;
        }
        self.ready.notify_all();
    }
}

/// Marks a worker's piece of work finished however it ends, a panic included, so the other
/// workers don't wait forever for work it would have added.
struct Finished<'a>(&'a Queue);

impl Drop for Finished<'_> {
    fn drop(&mut self) {
        self.0.finished_one();
    }
}

/// Uploads each of `sources` (a row of `batch` each) into the Drive folder `parent`, with
/// `at_once` folders made and files sent at the same time. With `remove`, a row whose files all
/// arrived then goes to the Recycle Bin (Recortar and Colar).
pub fn run(d: &Drive, batch: &Batch, sources: &[PathBuf], parent: &str, remove: bool, at_once: usize) -> Result<String, String> {
    let rows = batch.rows();
    for (p, path) in rows.iter().zip(sources) {
        p.size(ops::measure(path, &|| p.cancelled()));
    }
    let state: Vec<Row> = rows.iter().map(|_| Row { left: AtomicUsize::new(1), skipped: AtomicBool::new(false), error: Mutex::new(None) }).collect();
    let first: VecDeque<Work> = sources
        .iter()
        .enumerate()
        .map(|(row, path)| {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let (path, parent) = (path.clone(), parent.to_string());
            rows[row].add_items(1);
            if path.is_dir() { Work::Folder { row, dir: path, name, parent } } else { Work::File { row, path, name, parent } }
        })
        .collect();
    let queue = Queue { state: Mutex::new((first, 0)), ready: Condvar::new() };
    std::thread::scope(|scope| {
        for _ in 0..at_once.max(1) {
            scope.spawn(|| {
                while let Some(work) = queue.next() {
                    let _finished = Finished(&queue);
                    let row = work.row();
                    let (p, st) = (&rows[row], &state[row]);
                    if p.cancelled() || st.error.lock().map(|e| e.is_some()).unwrap_or(true) {
                        st.skipped.store(true, Ordering::Relaxed);
                    } else {
                        if p.stage() == Stage::Waiting {
                            p.begin();
                        }
                        let result = match &work {
                            Work::Folder { row, dir, name, parent } => open_folder(d, *row, dir, name, parent).map(|more| {
                                // The folder's contents join the row before this item counts
                                // as done, so the row can't end in between.
                                st.left.fetch_add(more.len(), Ordering::Relaxed);
                                p.add_items(more.len() as u64);
                                queue.add(more);
                            }),
                            Work::File { path, name, parent, .. } => send(d, path, name, parent, p),
                        };
                        match result {
                            Ok(()) => {}
                            Err(e) if e == CANCELLED => st.skipped.store(true, Ordering::Relaxed),
                            Err(e) => {
                                if let Ok(mut first) = st.error.lock() {
                                    first.get_or_insert(e);
                                }
                            }
                        }
                    }
                    p.item_done();
                    // The row's last piece of work ends it: done when nothing was skipped,
                    // even if Cancelar came too late to stop anything.
                    if st.left.fetch_sub(1, Ordering::Relaxed) == 1 {
                        let error = st.error.lock().ok().and_then(|e| e.clone());
                        p.end(match error {
                            Some(e) => Err(e),
                            None if st.skipped.load(Ordering::Relaxed) => Err(CANCELLED.into()),
                            None => finish(&sources[row], remove),
                        });
                    }
                }
            });
        }
    });
    batch.summary()
}

/// After a row's files all arrived: with `remove` (a move), the original goes to the
/// Recycle Bin.
fn finish(path: &Path, remove: bool) -> Result<(), String> {
    if remove { ops::to_recycle_bin(&[path.to_path_buf()]) } else { Ok(()) }
}

/// Makes folder `name` (local `dir`) inside Drive folder `parent`, and lists what goes in it.
fn open_folder(d: &Drive, row: usize, dir: &Path, name: &str, parent: &str) -> Result<Vec<Work>, String> {
    let made = d.create_folder(name, parent)?;
    let items = std::fs::read_dir(dir).map_err(|e| format!("Não consegui ler \"{}\": {e}", dir.display()))?;
    Ok(items
        .flatten()
        .map(|item| {
            let (path, name, parent) = (item.path(), item.file_name().to_string_lossy().into_owned(), made.id.clone());
            if path.is_dir() { Work::Folder { row, dir: path, name, parent } } else { Work::File { row, path, name, parent } }
        })
        .collect())
}

/// Sends one file, keeping its row's bar in step: bytes resent after a dropped connection
/// are taken back first.
fn send(d: &Drive, path: &Path, name: &str, parent: &str, p: &Progress) -> Result<(), String> {
    let mut reported = 0u64;
    let result = d.upload(path, parent, name, &mut |done| {
        if done > reported {
            p.add_bytes(done - reported);
        } else {
            p.sub_bytes(reported - done);
        }
        reported = done;
        !p.cancelled()
    });
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(row: usize) -> Work {
        Work::File { row, path: PathBuf::new(), name: String::new(), parent: String::new() }
    }

    /// Workers keep going while a busy one may still add work, and all stop once nothing is
    /// queued or running: every piece, nested ones included, is taken exactly once.
    #[test]
    fn the_work_queue_ends_only_when_all_work_is_done() {
        let queue = Queue { state: Mutex::new((VecDeque::from([Work::Folder { row: 0, dir: PathBuf::new(), name: String::new(), parent: String::new() }]), 0)), ready: Condvar::new() };
        let taken = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    while let Some(work) = queue.next() {
                        let _finished = Finished(&queue);
                        taken.fetch_add(1, Ordering::Relaxed);
                        // The root folder (row 0) adds three folders and five files; each of
                        // those folders (row 1) adds five files. A pause first, so the other
                        // workers find the list empty while this one is still busy.
                        if let Work::Folder { row, .. } = work {
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            if row == 0 {
                                queue.add((0..3).map(|_| Work::Folder { row: 1, dir: PathBuf::new(), name: String::new(), parent: String::new() }));
                            }
                            queue.add((0..5).map(|_| file(1)));
                        }
                    }
                });
            }
        });
        // The root, its 3 folders, and 5 files from each of the 4 folders.
        assert_eq!(taken.load(Ordering::Relaxed), 1 + 3 + 4 * 5);
    }
}
