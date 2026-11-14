//! Uploads from the computer to Google Drive, as rows of the transfer queue.
//!
//! A job first adds up each row's size, then makes each folder's tree on Drive, then sends
//! every file of every row through [`PARALLEL`] workers at once: a big file streams on one of
//! them while the others get through small ones, which spend most of their time waiting on
//! Google's answer rather than sending. A row's bar counts the bytes of all its files.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use openexplorer_gdrive::Drive;

use crate::ops::{self, CANCELLED, Progress, Stage};
use crate::transfers::Batch;

/// Files sent at the same time.
pub const PARALLEL: usize = 4;

/// One file to send: the row it belongs to, where it is, and the Drive folder it goes in.
struct Task {
    row: usize,
    path: PathBuf,
    parent: String,
    name: String,
}

/// What a row still has going: files not yet tried, files that arrived, the first error.
struct Row {
    left: AtomicUsize,
    arrived: AtomicUsize,
    error: Mutex<Option<String>>,
}

/// Uploads each of `sources` (a row of `batch` each) into the Drive folder `parent`. With
/// `remove`, a row whose files all arrived then goes to the Recycle Bin (Recortar and Colar).
pub fn run(d: &Drive, batch: &Batch, sources: &[PathBuf], parent: &str, remove: bool) -> Result<String, String> {
    let rows = batch.rows();
    // Single files first get their sizes, so their rows show them at once.
    for (p, path) in rows.iter().zip(sources).filter(|(_, path)| !path.is_dir()) {
        p.size(std::fs::metadata(path).map(|m| m.len()).unwrap_or(0));
    }
    // Then the folders' trees, so each file knows the folder it goes in; a folder's size adds
    // up as its tree is walked.
    let mut tasks = Vec::new();
    let state: Vec<Row> = rows.iter().map(|_| Row { left: AtomicUsize::new(0), arrived: AtomicUsize::new(0), error: Mutex::new(None) }).collect();
    for (i, (p, path)) in rows.iter().zip(sources).enumerate() {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if p.cancelled() {
            p.end(Err(CANCELLED.into()));
            continue;
        }
        if !path.is_dir() {
            tasks.push(Task { row: i, path: path.clone(), parent: parent.into(), name });
            continue;
        }
        p.begin();
        let before = tasks.len();
        let mut bytes = 0;
        match plan(d, i, path, &name, parent, p, &mut tasks, &mut bytes) {
            Ok(()) => p.size(bytes),
            Err(e) => {
                tasks.truncate(before);
                p.end(Err(e));
            }
        }
    }
    for t in &tasks {
        state[t.row].left.fetch_add(1, Ordering::Relaxed);
    }
    // Rows with nothing to send (an empty folder) are done once their folders exist.
    for (i, (p, path)) in rows.iter().zip(sources).enumerate() {
        if state[i].left.load(Ordering::Relaxed) == 0 && !p.over() {
            p.end(finish(path, remove));
        }
    }
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..PARALLEL.min(tasks.len()) {
            scope.spawn(|| {
                while let Some(t) = tasks.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let (p, row) = (&rows[t.row], &state[t.row]);
                    let skip = p.cancelled() || row.error.lock().map(|e| e.is_some()).unwrap_or(true);
                    if !skip {
                        if p.stage() == Stage::Waiting {
                            p.begin();
                        }
                        match send(d, t, p) {
                            Ok(()) => {
                                row.arrived.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) if e == CANCELLED => {}
                            Err(e) => {
                                if let Ok(mut first) = row.error.lock() {
                                    first.get_or_insert(e);
                                }
                            }
                        }
                    }
                    // The row's last file ends it: done when every file arrived, even if
                    // Cancelar came too late to stop them.
                    if row.left.fetch_sub(1, Ordering::Relaxed) == 1 {
                        let total = tasks.iter().filter(|o| o.row == t.row).count();
                        let error = row.error.lock().ok().and_then(|e| e.clone());
                        p.end(match error {
                            Some(e) => Err(e),
                            None if row.arrived.load(Ordering::Relaxed) == total => finish(&sources[t.row], remove),
                            None => Err(CANCELLED.into()),
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

/// Makes folder `name` (local `dir`) inside Drive folder `parent`, and the folders under it,
/// adding a task for each file and its size to `bytes`.
#[allow(clippy::too_many_arguments)]
fn plan(d: &Drive, row: usize, dir: &Path, name: &str, parent: &str, p: &Progress, tasks: &mut Vec<Task>, bytes: &mut u64) -> Result<(), String> {
    if p.cancelled() {
        return Err(CANCELLED.into());
    }
    let made = d.create_folder(name, parent)?;
    let items = std::fs::read_dir(dir).map_err(|e| format!("Não consegui ler \"{}\": {e}", dir.display()))?;
    for item in items.flatten() {
        let path = item.path();
        let name = item.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            plan(d, row, &path, &name, &made.id, p, tasks, bytes)?;
        } else {
            *bytes += item.metadata().map(|m| m.len()).unwrap_or(0);
            tasks.push(Task { row, path, parent: made.id.clone(), name });
        }
    }
    Ok(())
}

/// Sends one file, keeping its row's bar in step: bytes resent after a dropped connection
/// are taken back first.
fn send(d: &Drive, t: &Task, p: &Progress) -> Result<(), String> {
    let mut reported = 0u64;
    let result = d.upload(&t.path, &t.parent, &t.name, &mut |done| {
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
