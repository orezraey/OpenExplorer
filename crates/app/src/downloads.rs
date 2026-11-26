//! Downloads from Google Drive to the computer, as rows of the transfer queue.
//!
//! Each row is one selected item. Workers (Downloads simultâneos in Opções, 4 unless changed)
//! share one list of work: reading a Drive folder, which makes its subfolders on the computer
//! and adds them and its files to the list, and fetching a file. A worker reads up to
//! [`openexplorer_gdrive::api::MANY`] queued folders at once, in one listing. Files start arriving as soon
//! as their folder is read, so a big tree shows progress at once. A folder row's size grows as
//! its folders are read; its state counts items done.
//!
//! Names come out as Windows takes them (see [`ops::windows_name`]), Google's own formats gain
//! their export's extension, and a name already in use, on the disk or by a download under way,
//! becomes "nome - Cópia", as copies do. Nothing is overwritten.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use openexplorer_gdrive::api::{FOLDER_MIME, MANY};
use openexplorer_gdrive::download::{export_format, local_name};
use openexplorer_gdrive::{Drive, DriveFile};

use crate::ops::{self, CANCELLED, Progress, Stage};
use crate::transfers::Batch;
use crate::work::{Queue, Slots};

/// One piece of work of a row.
enum Work {
    /// Read Drive folder `id` into local folder `dir` (already made).
    Folder { row: usize, id: String, dir: PathBuf },
    /// Fetch `file` to `to`, a name claimed for it.
    File { row: usize, file: DriveFile, to: PathBuf },
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
    /// Bytes of the files found so far, the row's size.
    total: AtomicU64,
    /// The file or folder the row made in the destination.
    made: PathBuf,
}

/// Whether Drive keeps `f` in a format of Google's own, with no bytes of its own: its listed
/// size is the quota it uses, not what its export weighs.
fn google(f: &DriveFile) -> bool {
    f.mime.starts_with("application/vnd.google-apps.")
}

/// The bytes a file adds to its row's size, when known before fetching it.
fn known_size(f: &DriveFile) -> u64 {
    if google(f) { 0 } else { f.size.unwrap_or(0) }
}

/// Names claimed by downloads that haven't made their file yet, so two of them never pick the
/// same one.
#[derive(Default)]
pub struct Names(Mutex<HashSet<PathBuf>>);

impl Names {
    /// A free path in `dir` for Drive item `f`.
    pub fn claim(&self, dir: &Path, f: &DriveFile) -> PathBuf {
        let name = ops::windows_name(&if f.is_dir { f.name.clone() } else { local_name(f) });
        let mut taken = self.0.lock().unwrap();
        let path = ops::unique_where(dir, &name, &|p| taken.contains(p));
        taken.insert(path.clone());
        path
    }
}

/// Downloads each of `items` (a row of `batch` each) into the folder `dst`, with `at_once` files
/// fetched at the same time, big ones in segments when `segmented`. A row whose files all
/// arrived then runs `finish` with its index and the path it made (the original goes to the
/// Drive trash after Recortar, the file opens after Abrir).
#[allow(clippy::too_many_arguments)]
pub fn run(
    d: &Drive,
    batch: &Batch,
    items: &[DriveFile],
    dst: &Path,
    at_once: usize,
    segmented: bool,
    slots: &Slots,
    finish: &(dyn Fn(usize, &Path) -> Result<(), String> + Sync),
) -> Result<String, String> {
    let rows = batch.rows();
    let names = Names::default();
    let mut first = Vec::new();
    let mut state = Vec::new();
    for (row, f) in items.iter().enumerate() {
        let made = names.claim(dst, f);
        rows[row].add_items(1);
        let total = known_size(f);
        if total > 0 || !f.is_dir && !google(f) {
            rows[row].size(total);
        }
        state.push(Row { left: AtomicUsize::new(1), skipped: AtomicBool::new(false), error: Mutex::new(None), total: AtomicU64::new(total), made: made.clone() });
        first.push(if f.is_dir {
            Work::Folder { row, id: f.target.clone().unwrap_or_else(|| f.id.clone()), dir: made }
        } else {
            Work::File { row, file: f.clone(), to: made }
        });
    }
    let queue = Queue::new(first);
    // Folders already read in each row: a shortcut back up the tree doesn't loop.
    let visited = Mutex::new(HashSet::<(usize, String)>::new());
    std::thread::scope(|scope| {
        for _ in 0..at_once.max(1) {
            scope.spawn(|| {
                while let Some((work, _finished)) = queue.next() {
                    let skip = |row: usize| rows[row].cancelled() || state[row].error.lock().map(|e| e.is_some()).unwrap_or(true);
                    let _slot = (!skip(work.row())).then(|| slots.acquire());
                    // Other folders waiting are read with this one, in one listing.
                    let mut group = vec![work];
                    if _slot.is_some() && matches!(group[0], Work::Folder { .. }) {
                        group.extend(queue.take(MANY - 1, |w| matches!(w, Work::Folder { .. })));
                    }
                    let mut to_read = Vec::new();
                    for (k, w) in group.iter().enumerate() {
                        if let Work::Folder { row, id, dir } = w
                            && !skip(*row)
                            && visited.lock().unwrap().insert((*row, id.clone()))
                        {
                            to_read.push((k, *row, id.as_str(), dir.as_path()));
                        }
                    }
                    let mut read: Vec<Option<Result<Vec<Work>, String>>> = (0..group.len()).map(|_| None).collect();
                    for ((k, ..), r) in to_read.iter().zip(read_folders(d, &to_read, &names)) {
                        read[*k] = Some(r);
                    }
                    for (work, listed) in group.into_iter().zip(read) {
                        let row = work.row();
                        let (p, st) = (&rows[row], &state[row]);
                        if skip(row) {
                            st.skipped.store(true, Ordering::Relaxed);
                        } else {
                            if p.stage() == Stage::Waiting {
                                p.begin();
                            }
                            let result = match &work {
                                Work::Folder { .. } => {
                                    // Not read: it was read already in this row (a shortcut back up).
                                    listed.unwrap_or(Ok(Vec::new())).map(|more| {
                                        // The folder's contents join the row before this item counts
                                        // as done, so the row can't end in between.
                                        let bytes: u64 = more.iter().map(|w| if let Work::File { file, .. } = w { known_size(file) } else { 0 }).sum();
                                        p.size(st.total.fetch_add(bytes, Ordering::Relaxed) + bytes);
                                        st.left.fetch_add(more.len(), Ordering::Relaxed);
                                        p.add_items(more.len() as u64);
                                        queue.add(more);
                                    })
                                }
                                Work::File { file, to, .. } => fetch(d, file, to, segmented, p).map(|n| {
                                    // An export's size is known only now.
                                    if known_size(file) == 0 && n > 0 {
                                        p.size(st.total.fetch_add(n, Ordering::Relaxed) + n);
                                    }
                                }),
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
                        // The row's last piece of work ends it.
                        if st.left.fetch_sub(1, Ordering::Relaxed) == 1 {
                            let error = st.error.lock().ok().and_then(|e| e.clone());
                            p.end(match error {
                                Some(e) => Err(e),
                                None if st.skipped.load(Ordering::Relaxed) => {
                                    // A cancelled row takes away what it made, as a cancelled copy
                                    // does: what's left in the destination is only whole items.
                                    let _ = if st.made.is_dir() { std::fs::remove_dir_all(&st.made) } else { std::fs::remove_file(&st.made) };
                                    Err(CANCELLED.into())
                                }
                                None => finish(row, &st.made),
                            });
                        }
                    }
                }
            });
        }
    });
    batch.summary()
}

/// Reads the Drive folders `list` (each `(_, row, id, local folder)`): one listing for all of
/// them, or one each if Drive refuses that one. Gives each folder's work, in `list`'s order.
fn read_folders(d: &Drive, list: &[(usize, usize, &str, &Path)], names: &Names) -> Vec<Result<Vec<Work>, String>> {
    if list.len() > 1 {
        let ids: Vec<&str> = list.iter().map(|(_, _, id, _)| *id).collect();
        let mut children: std::collections::HashMap<String, Vec<DriveFile>> = ids.iter().map(|id| (id.to_string(), Vec::new())).collect();
        let together = d.list_many(&ids, |page| {
            for (parent, f) in page {
                children.entry(parent).or_default().push(f);
            }
            true
        });
        match together {
            // Cloned, not taken: one folder may be here twice (a folder and a shortcut to it,
            // both selected), and each of its rows gets its contents.
            Ok(()) => return list.iter().map(|(_, row, id, dir)| works_in(*row, dir, children.get(*id).cloned().unwrap_or_default(), names)).collect(),
            Err(e) => openexplorer_gdrive::log(&format!("download: {} pastas numa listagem só não deu ({e}); uma a uma", list.len())),
        }
    }
    list.iter().map(|(_, row, id, dir)| read_folder(d, *row, id, dir, names)).collect()
}

/// Lists Drive folder `id`, makes its subfolders in `dir`, and gives the work for what's in it.
fn read_folder(d: &Drive, row: usize, id: &str, dir: &Path, names: &Names) -> Result<Vec<Work>, String> {
    let mut children = Vec::new();
    d.list(id, None, |page, _| {
        children.extend(page);
        true
    })?;
    works_in(row, dir, children, names)
}

/// Makes local folder `dir` and the subfolders of `children` (a Drive folder's contents) in it,
/// and gives the work for what's in it. Shortcuts to files and Google's formats that can't be
/// exported (Forms, Sites) stay out: there's nothing to fetch.
fn works_in(row: usize, dir: &Path, children: Vec<DriveFile>, names: &Names) -> Result<Vec<Work>, String> {
    std::fs::create_dir_all(dir).map_err(|e| openexplorer_i18n::tr!("Não consegui criar a pasta \"{}\": {e}", "Couldn't create the folder \"{}\": {e}", dir.display()))?;
    let mut work = Vec::new();
    for f in children {
        if !f.is_dir && google(&f) && export_format(&f.mime).is_none() {
            openexplorer_gdrive::log(&format!("download: \"{}\" ({}) fica de fora: não tem o que baixar", f.name, f.mime));
            continue;
        }
        let to = names.claim(dir, &f);
        if f.is_dir {
            std::fs::create_dir_all(&to).map_err(|e| openexplorer_i18n::tr!("Não consegui criar a pasta \"{}\": {e}", "Couldn't create the folder \"{}\": {e}", to.display()))?;
            let id = f.target.clone().unwrap_or_else(|| f.id.clone());
            work.push(Work::Folder { row, id, dir: to });
        } else if f.mime != FOLDER_MIME {
            work.push(Work::File { row, file: f, to });
        }
    }
    Ok(work)
}

/// Fetches one file, keeping its row's bar in step. Returns the bytes written.
fn fetch(d: &Drive, file: &DriveFile, to: &Path, segmented: bool, p: &Progress) -> Result<u64, String> {
    let mut reported = 0u64;
    d.download(file, to, segmented, &mut |done| {
        if done > reported {
            p.add_bytes(done - reported);
        } else {
            p.sub_bytes(reported - done);
        }
        reported = done;
        !p.cancelled()
    })
    .inspect_err(|_| p.sub_bytes(reported))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, mime: &str, is_dir: bool) -> DriveFile {
        DriveFile { id: "x".into(), name: name.into(), is_dir, size: Some(1024), modified: None, mime: mime.into(), target: None, md5: None, starred: false }
    }

    #[test]
    fn names_are_claimed_once_and_follow_windows_rules() {
        let dir = std::env::temp_dir().join(format!("openexplorer-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ata.txt"), "").unwrap();
        let names = Names::default();
        let txt = item("ata.txt", "text/plain", false);
        // On the disk already, then claimed by a download under way.
        assert_eq!(names.claim(&dir, &txt), dir.join("ata - Cópia.txt"));
        assert_eq!(names.claim(&dir, &txt), dir.join("ata - Cópia (2).txt"));
        assert_eq!(names.claim(&dir, &item("Plano: 2027", "application/vnd.google-apps.document", false)), dir.join("Plano_ 2027.docx"));
        assert_eq!(names.claim(&dir, &item("Fotos.", FOLDER_MIME, true)), dir.join("Fotos"));
        // A Doc's listed size is its quota, not its export's.
        assert_eq!(known_size(&item("Plano", "application/vnd.google-apps.document", false)), 0);
        assert_eq!(known_size(&txt), 1024);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Downloads a small real folder (two levels, up to 50 MB) of the first connected account
    /// into a temp folder through the queue, checks every file arrived at its Drive size, and
    /// deletes the copy. Read-only for the account.
    /// `cargo test -p openexplorer real_folder -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_folder_downloads_whole() {
        use openexplorer_gdrive::{ClientConfig, store};
        let cfg = ClientConfig::load().unwrap();
        let account = store::accounts().into_iter().next().expect("uma conta conectada");
        let d = Drive::new(cfg, store::refresh_token(&account.email).unwrap());
        let list = |id: &str| {
            let mut out = Vec::new();
            d.list(id, None, |page, _| {
                out.extend(page);
                true
            })
            .unwrap();
            out
        };
        // A folder with a subfolder, nothing deeper, and little in it.
        let pick = list("root").into_iter().filter(|f| f.is_dir && f.target.is_none()).take(40).find_map(|folder| {
            let children = list(&folder.id);
            let subs: Vec<_> = children.iter().filter(|c| c.is_dir).collect();
            if subs.is_empty() || subs.len() > 3 || children.len() > 30 {
                return None;
            }
            let mut files: Vec<DriveFile> = children.iter().filter(|c| !c.is_dir).cloned().collect();
            for s in subs {
                let inner = list(&s.id);
                if inner.iter().any(|c| c.is_dir) || inner.len() > 30 {
                    return None;
                }
                files.extend(inner);
            }
            let bytes: u64 = files.iter().map(known_size).sum();
            (bytes <= 50 << 20).then_some((folder, files))
        });
        let Some((folder, files)) = pick else {
            println!("nenhuma pasta pequena com subpasta na raiz");
            return;
        };
        let dst = std::env::temp_dir().join(format!("openexplorer-real-folder-{}", std::process::id()));
        std::fs::create_dir_all(&dst).unwrap();
        let mut rows = Vec::new();
        let end = crate::transfers::End::local(&dst);
        let batch = crate::transfers::queue(&mut rows, None, crate::transfers::Op::Download, [(end.clone(), end)]);
        let out = run(&d, &batch, std::slice::from_ref(&folder), &dst, 4, true, &Slots::new(4), &|_, _| Ok(()));
        println!("{out:?}");
        let made: Vec<PathBuf> = walk(&dst);
        let p = &rows[0].progress;
        println!("{} arquivos no Drive, {} baixados, {} bytes", files.len(), made.len(), p.bytes.load(Ordering::Relaxed));
        assert_eq!(p.stage(), Stage::Done, "{}", p.error());
        let expected = files.iter().filter(|f| !google(f) || export_format(&f.mime).is_some()).count();
        assert_eq!(made.len(), expected);
        let ordinary: u64 = files.iter().map(known_size).sum();
        let on_disk: u64 = made.iter().map(|m| std::fs::metadata(m).unwrap().len()).sum();
        assert!(on_disk >= ordinary, "{on_disk} < {ordinary}");
        std::fs::remove_dir_all(&dst).unwrap();
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir).unwrap().flatten().flat_map(|e| if e.path().is_dir() { walk(&e.path()) } else { vec![e.path()] }).collect()
    }
}
