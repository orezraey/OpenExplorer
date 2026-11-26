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

use crate::conflict::{self, Decision, FileInfo, Resolver};
use crate::model::shown;
use crate::ops::{self, CANCELLED, Progress, Stage};
use crate::transfers::Batch;
use crate::work::{Queue, Slots};

/// One piece of work of a row.
enum Work {
    /// Read Drive folder `id` into local folder `dir` (already made).
    Folder { row: usize, id: String, dir: PathBuf, src_place: String },
    /// Fetch `file` into `dir` as `raw_name` (conflict checked before taking a download slot).
    File { row: usize, file: DriveFile, dir: PathBuf, raw_name: String, src_place: String, is_top: bool },
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
    /// The top-level item itself was skipped via conflict prompt.
    row_skipped: AtomicBool,
    /// Some inner file inside a folder was skipped (so a move keeps the Drive folder).
    kept: AtomicBool,
    /// Whether this row created its top-level destination item (false when merged or replaced).
    created: AtomicBool,
    error: Mutex<Option<String>>,
    /// Bytes of the files found so far, the row's size.
    total: AtomicU64,
    /// The file or folder the row made/targeted in the destination.
    made: Mutex<PathBuf>,
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

enum Claimed {
    Write { path: PathBuf, created: bool },
    Skip,
    Cancel,
}

impl Names {
    /// A free path in `dir` for Drive item `f` (auto-renaming without prompting).
    pub fn claim(&self, dir: &Path, f: &DriveFile) -> PathBuf {
        let name = ops::windows_name(&if f.is_dir { f.name.clone() } else { local_name(f) });
        let mut taken = self.0.lock().unwrap();
        let path = ops::unique_where(dir, &name, &|p| taken.contains(p));
        taken.insert(path.clone());
        path
    }

    fn claim_exact(&self, path: &Path) -> bool {
        let mut taken = self.0.lock().unwrap();
        if path.exists() || taken.contains(path) {
            return false;
        }
        taken.insert(path.to_path_buf());
        true
    }

    fn claim_numbered(&self, dir: &Path, raw_name: &str, is_dir: bool) -> PathBuf {
        let mut taken = self.0.lock().unwrap();
        let path = conflict::numbered_local(dir, raw_name, is_dir, &|p| taken.contains(p));
        taken.insert(path.clone());
        path
    }

    /// Resolves a destination file conflict (with `resolver`) and claims the target path
    /// without holding any lock while waiting for user input.
    fn resolve_file(
        &self,
        dir: &Path,
        raw_name: &str,
        file: &DriveFile,
        src_place: &str,
        resolver: Option<&Resolver>,
        is_cancelled: impl Fn() -> bool,
    ) -> Claimed {
        let desired = dir.join(raw_name);
        let Some(res) = resolver else {
            let mut taken = self.0.lock().unwrap();
            let path = ops::unique_where(dir, raw_name, &|p| taken.contains(p));
            taken.insert(path.clone());
            return Claimed::Write { path, created: true };
        };
        let (exists_on_disk, renamed_name) = {
            let mut taken = self.0.lock().unwrap();
            if !desired.exists() && !taken.contains(&desired) {
                taken.insert(desired.clone());
                return Claimed::Write { path: desired, created: true };
            }
            let renamed = conflict::numbered_local(dir, raw_name, false, &|p| taken.contains(p));
            let renamed_name = renamed.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| raw_name.to_string());
            (desired.exists(), renamed_name)
        };
        if !exists_on_disk {
            // Same-name sibling in the same batch already claimed `desired`: give this one a numbered name.
            let path = self.claim_numbered(dir, raw_name, false);
            return Claimed::Write { path, created: true };
        }
        let dst_info = FileInfo::local(&desired).unwrap_or_else(|| FileInfo {
            name: raw_name.to_string(),
            place: dir.display().to_string(),
            is_dir: false,
            size: None,
            modified: None,
            kind: conflict::kind_of(raw_name, false, None),
        });
        let src_info = FileInfo::drive(file, src_place.to_string());
        let decision = res.resolve(src_info, dst_info, renamed_name, is_cancelled);
        match decision {
            Decision::Cancel => Claimed::Cancel,
            Decision::Skip => Claimed::Skip,
            Decision::Replace if !desired.is_dir() => {
                let mut taken = self.0.lock().unwrap();
                if taken.contains(&desired) {
                    let path = conflict::numbered_local(dir, raw_name, false, &|p| taken.contains(p));
                    taken.insert(path.clone());
                    Claimed::Write { path, created: true }
                } else {
                    taken.insert(desired.clone());
                    Claimed::Write { path: desired, created: false }
                }
            }
            Decision::KeepBoth | Decision::Replace => {
                let path = self.claim_numbered(dir, raw_name, false);
                Claimed::Write { path, created: true }
            }
        }
    }
}

/// Joins a display subfolder path onto `place`.
fn child_place(place: &str, name: &str) -> String {
    let s = shown(name);
    if place.is_empty() { s } else { format!("{place}\\{s}") }
}

/// Downloads each of `items` (a row of `batch` each) into the folder `dst`, with `at_once` files
/// fetched at the same time, big ones in segments when `segmented`. A row whose files all
/// arrived then runs `finish` with `(row, made_path, created_new, kept_some_skipped)`.
#[allow(clippy::too_many_arguments)]
pub fn run(
    d: &Drive,
    batch: &Batch,
    items: &[DriveFile],
    dst: &Path,
    src_root: &str,
    at_once: usize,
    segmented: bool,
    slots: &Slots,
    resolver: Option<&Resolver>,
    finish: &(dyn Fn(usize, &Path, bool, bool) -> Result<(), String> + Sync),
) -> Result<String, String> {
    let rows = batch.rows();
    let names = Names::default();
    let mut first = Vec::new();
    let mut state = Vec::with_capacity(items.len());
    for (row, f) in items.iter().enumerate() {
        let total = known_size(f);
        if total > 0 || (!f.is_dir && !google(f)) {
            rows[row].size(total);
        }
        let raw_name = ops::windows_name(&if f.is_dir { f.name.clone() } else { local_name(f) });
        let desired = dst.join(&raw_name);
        if f.is_dir {
            let (made, created, skip_row, cancel_row) = if let Some(res) = resolver {
                if desired.is_dir() {
                    (desired, false, false, false)
                } else if desired.exists() {
                    let renamed = conflict::numbered_local(dst, &raw_name, true, &|_| false);
                    let renamed_name = renamed.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| raw_name.clone());
                    let dst_info = FileInfo::local(&desired).unwrap_or_else(|| FileInfo {
                        name: raw_name.clone(),
                        place: dst.display().to_string(),
                        is_dir: false,
                        size: None,
                        modified: None,
                        kind: conflict::kind_of(&raw_name, false, None),
                    });
                    match res.resolve(FileInfo::drive(f, src_root.to_string()), dst_info, renamed_name, || rows[row].cancelled()) {
                        Decision::Skip => (desired, false, true, false),
                        Decision::Cancel => (desired, false, false, true),
                        Decision::KeepBoth | Decision::Replace => (names.claim_numbered(dst, &raw_name, true), true, false, false),
                    }
                } else if names.claim_exact(&desired) {
                    (desired, true, false, false)
                } else {
                    (names.claim_numbered(dst, &raw_name, true), true, false, false)
                }
            } else {
                (names.claim(dst, f), true, false, false)
            };
            state.push(Row {
                left: AtomicUsize::new(1),
                skipped: AtomicBool::new(false),
                row_skipped: AtomicBool::new(skip_row),
                kept: AtomicBool::new(false),
                created: AtomicBool::new(created),
                error: Mutex::new(None),
                total: AtomicU64::new(total),
                made: Mutex::new(made.clone()),
            });
            if skip_row {
                rows[row].skip();
            } else if cancel_row {
                rows[row].end(Err(CANCELLED.into()));
            } else {
                rows[row].add_items(1);
                first.push(Work::Folder {
                    row,
                    id: f.target.clone().unwrap_or_else(|| f.id.clone()),
                    dir: made,
                    src_place: child_place(src_root, &f.name),
                });
            }
        } else {
            rows[row].add_items(1);
            state.push(Row {
                left: AtomicUsize::new(1),
                skipped: AtomicBool::new(false),
                row_skipped: AtomicBool::new(false),
                kept: AtomicBool::new(false),
                created: AtomicBool::new(true),
                error: Mutex::new(None),
                total: AtomicU64::new(total),
                made: Mutex::new(desired),
            });
            first.push(Work::File {
                row,
                file: f.clone(),
                dir: dst.to_path_buf(),
                raw_name,
                src_place: src_root.to_string(),
                is_top: true,
            });
        }
    }
    let queue = Queue::new(first);
    // Folders already read in each row: a shortcut back up the tree doesn't loop.
    let visited = Mutex::new(HashSet::<(usize, String)>::new());
    std::thread::scope(|scope| {
        for _ in 0..at_once.max(1) {
            scope.spawn(|| {
                while let Some((work, _finished)) = queue.next() {
                    let skip = |row: usize| rows[row].cancelled() || state[row].error.lock().map(|e| e.is_some()).unwrap_or(true);
                    // Resolve file conflicts BEFORE acquiring a download slot so waiting on user
                    // input never starves active downloads.
                    if let Work::File { row, file, dir, raw_name, src_place, is_top } = work {
                        let (p, st) = (&rows[row], &state[row]);
                        if skip(row) {
                            st.skipped.store(true, Ordering::Relaxed);
                        } else {
                            match names.resolve_file(&dir, &raw_name, &file, &src_place, resolver, || skip(row)) {
                                Claimed::Cancel => st.skipped.store(true, Ordering::Relaxed),
                                Claimed::Skip => {
                                    if is_top {
                                        st.row_skipped.store(true, Ordering::Relaxed);
                                    } else {
                                        st.kept.store(true, Ordering::Relaxed);
                                        p.unsize(known_size(&file));
                                    }
                                }
                                Claimed::Write { path: to, created } => {
                                    if is_top {
                                        st.created.store(created, Ordering::Relaxed);
                                        if let Ok(mut m) = st.made.lock() {
                                            *m = to.clone();
                                        }
                                    }
                                    let _slot = slots.acquire_unless(|| skip(row));
                                    if skip(row) {
                                        st.skipped.store(true, Ordering::Relaxed);
                                    } else {
                                        if p.stage() == Stage::Waiting {
                                            p.begin();
                                        }
                                        let result = fetch(d, &file, &to, segmented, p).map(|n| {
                                            // An export's size is known only now.
                                            if known_size(&file) == 0 && n > 0 {
                                                p.size(st.total.fetch_add(n, Ordering::Relaxed) + n);
                                            }
                                        });
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
                                }
                            }
                        }
                        p.item_done();
                        if st.left.fetch_sub(1, Ordering::Relaxed) == 1 {
                            finish_row(p, st, row, finish);
                        }
                        continue;
                    }

                    let _slot = slots.acquire_unless(|| skip(work.row()));
                    // Other folders waiting are read with this one, in one listing.
                    let mut group = vec![work];
                    if _slot.is_some() && matches!(group[0], Work::Folder { .. }) {
                        group.extend(queue.take(MANY - 1, |w| matches!(w, Work::Folder { .. })));
                    }
                    let mut to_read = Vec::new();
                    for (k, w) in group.iter().enumerate() {
                        if let Work::Folder { row, id, dir, src_place } = w
                            && !skip(*row)
                            && visited.lock().unwrap().insert((*row, id.clone()))
                        {
                            to_read.push((k, *row, id.as_str(), dir.as_path(), src_place.as_str()));
                        }
                    }
                    let mut read: Vec<Option<Result<Vec<Work>, String>>> = (0..group.len()).map(|_| None).collect();
                    for ((k, ..), r) in to_read.iter().zip(read_folders(d, &to_read, &names, resolver, &state, &|r| skip(r))) {
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
                                Work::File { .. } => Ok(()),
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
                            finish_row(p, st, row, finish);
                        }
                    }
                }
            });
        }
    });
    batch.summary()
}

fn finish_row(p: &Progress, st: &Row, row: usize, finish: &(dyn Fn(usize, &Path, bool, bool) -> Result<(), String> + Sync)) {
    if st.row_skipped.load(Ordering::Relaxed) {
        p.skip();
        return;
    }
    let error = st.error.lock().ok().and_then(|e| e.clone());
    let made = st.made.lock().map(|m| m.clone()).unwrap_or_default();
    let created = st.created.load(Ordering::Relaxed);
    let kept = st.kept.load(Ordering::Relaxed);
    p.end(match error {
        Some(e) => Err(e),
        None if st.skipped.load(Ordering::Relaxed) => {
            // A cancelled row takes away what it created, never an existing merged folder/file.
            if created {
                let _ = if made.is_dir() { std::fs::remove_dir_all(&made) } else { std::fs::remove_file(&made) };
            }
            Err(CANCELLED.into())
        }
        None => finish(row, &made, created, kept),
    });
}

/// Reads the Drive folders `list` (each `(_, row, id, local folder, src_place)`): one listing for
/// all of them, or one each if Drive refuses that one.
fn read_folders(
    d: &Drive,
    list: &[(usize, usize, &str, &Path, &str)],
    names: &Names,
    resolver: Option<&Resolver>,
    state: &[Row],
    stop: &dyn Fn(usize) -> bool,
) -> Vec<Result<Vec<Work>, String>> {
    if list.len() > 1 {
        let ids: Vec<&str> = list.iter().map(|(_, _, id, ..)| *id).collect();
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
            Ok(()) => {
                return list
                    .iter()
                    .map(|(_, row, id, dir, src_place)| works_in(*row, dir, src_place, children.get(*id).cloned().unwrap_or_default(), names, resolver, &state[*row], &|| stop(*row)))
                    .collect();
            }
            Err(e) => openexplorer_gdrive::log(&format!("download: {} pastas numa listagem só não deu ({e}); uma a uma", list.len())),
        }
    }
    list.iter().map(|(_, row, id, dir, src_place)| read_folder(d, *row, id, dir, src_place, names, resolver, &state[*row], &|| stop(*row))).collect()
}

/// Lists Drive folder `id`, makes its subfolders in `dir`, and gives the work for what's in it.
#[allow(clippy::too_many_arguments)]
fn read_folder(
    d: &Drive,
    row: usize,
    id: &str,
    dir: &Path,
    src_place: &str,
    names: &Names,
    resolver: Option<&Resolver>,
    st: &Row,
    stop: &dyn Fn() -> bool,
) -> Result<Vec<Work>, String> {
    let mut children = Vec::new();
    d.list(id, None, |page, _| {
        children.extend(page);
        true
    })?;
    works_in(row, dir, src_place, children, names, resolver, st, stop)
}

/// Makes local folder `dir` and the subfolders of `children` (a Drive folder's contents) in it,
/// and gives the work for what's in it. Shortcuts to files and Google's formats that can't be
/// exported (Forms, Sites) stay out: there's nothing to fetch.
#[allow(clippy::too_many_arguments)]
fn works_in(
    row: usize,
    dir: &Path,
    src_place: &str,
    children: Vec<DriveFile>,
    names: &Names,
    resolver: Option<&Resolver>,
    st: &Row,
    stop: &dyn Fn() -> bool,
) -> Result<Vec<Work>, String> {
    std::fs::create_dir_all(dir).map_err(|e| openexplorer_i18n::tr!("Não consegui criar a pasta \"{}\": {e}", "Couldn't create the folder \"{}\": {e}", dir.display()))?;
    let mut work = Vec::new();
    for f in children {
        if !f.is_dir && google(&f) && export_format(&f.mime).is_none() {
            openexplorer_gdrive::log(&format!("download: \"{}\" ({}) fica de fora: não tem o que baixar", f.name, f.mime));
            continue;
        }
        if f.is_dir {
            let raw_name = ops::windows_name(&f.name);
            let desired = dir.join(&raw_name);
            let to = if let Some(res) = resolver {
                if desired.is_dir() {
                    desired
                } else if desired.exists() {
                    let renamed = conflict::numbered_local(dir, &raw_name, true, &|_| false);
                    let renamed_name = renamed.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| raw_name.clone());
                    let dst_info = FileInfo::local(&desired).unwrap_or_else(|| FileInfo {
                        name: raw_name.clone(),
                        place: dir.display().to_string(),
                        is_dir: false,
                        size: None,
                        modified: None,
                        kind: conflict::kind_of(&raw_name, false, None),
                    });
                    match res.resolve(FileInfo::drive(&f, src_place.to_string()), dst_info, renamed_name, stop) {
                        Decision::Skip => {
                            st.kept.store(true, Ordering::Relaxed);
                            continue;
                        }
                        Decision::Cancel => return Err(CANCELLED.into()),
                        Decision::KeepBoth | Decision::Replace => names.claim_numbered(dir, &raw_name, true),
                    }
                } else if names.claim_exact(&desired) {
                    desired
                } else {
                    names.claim_numbered(dir, &raw_name, true)
                }
            } else {
                names.claim(dir, &f)
            };
            std::fs::create_dir_all(&to).map_err(|e| openexplorer_i18n::tr!("Não consegui criar a pasta \"{}\": {e}", "Couldn't create the folder \"{}\": {e}", to.display()))?;
            let id = f.target.clone().unwrap_or_else(|| f.id.clone());
            work.push(Work::Folder { row, id, dir: to, src_place: child_place(src_place, &f.name) });
        } else if f.mime != FOLDER_MIME {
            let raw_name = ops::windows_name(&local_name(&f));
            work.push(Work::File {
                row,
                file: f,
                dir: dir.to_path_buf(),
                raw_name,
                src_place: src_place.to_string(),
                is_top: false,
            });
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
        let out = run(&d, &batch, std::slice::from_ref(&folder), &dst, "", 4, true, &Slots::new(4), None, &|_, _, _, _| Ok(()));
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
