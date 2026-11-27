//! File operations on the local disk: open, rename, new folder, delete to the Recycle Bin,
//! copy and move. They block, so the app runs them off the UI thread.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use openexplorer_i18n::{t, tr};

/// The error a copy, move or upload gives when Cancelar stopped it.
pub const CANCELLED: &str = openexplorer_gdrive::CANCELLED;

/// Where one transfer stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Waiting,
    Running,
    Done,
    Cancelled,
    Failed,
    Skipped,
}

/// How far one item's copy, move, upload or download has got, shared by the thread doing it
/// and the window showing it.
pub struct Progress {
    /// Bytes through so far, and in all once known (`sized`).
    pub bytes: AtomicU64,
    pub total: AtomicU64,
    pub sized: AtomicBool,
    /// Items (files and folders) done, and found so far: a folder's grow as it's walked.
    pub items_done: AtomicU64,
    pub items: AtomicU64,
    stage: AtomicU8,
    /// Why it failed.
    error: Mutex<String>,
    /// When it started running, for its speed.
    began: Mutex<Option<Instant>>,
    /// Set by Cancelar: the copy stops within a chunk.
    cancel: AtomicBool,
    made: Instant,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Milliseconds after `made` of the last wake: the window redraws at most ten times a
    /// second, not once per 64 KB chunk.
    woke: AtomicU64,
}

impl Progress {
    pub fn new(wake: Option<Arc<dyn Fn() + Send + Sync>>) -> Progress {
        Progress {
            bytes: AtomicU64::new(0),
            total: AtomicU64::new(0),
            sized: AtomicBool::new(false),
            items_done: AtomicU64::new(0),
            items: AtomicU64::new(0),
            stage: AtomicU8::new(Stage::Waiting as u8),
            error: Mutex::new(String::new()),
            began: Mutex::new(None),
            cancel: AtomicBool::new(false),
            made: Instant::now(),
            wake,
            woke: AtomicU64::new(0),
        }
    }

    fn tick(&self, now: bool) {
        let at = self.made.elapsed().as_millis() as u64;
        let last = self.woke.load(Ordering::Relaxed);
        if (now || at >= last + 100)
            && self.woke.compare_exchange(last, at, Ordering::Relaxed, Ordering::Relaxed).is_ok()
            && let Some(wake) = &self.wake
        {
            wake();
        }
    }

    pub fn stage(&self) -> Stage {
        match self.stage.load(Ordering::Relaxed) {
            0 => Stage::Waiting,
            1 => Stage::Running,
            2 => Stage::Done,
            3 => Stage::Cancelled,
            5 => Stage::Skipped,
            _ => Stage::Failed,
        }
    }

    /// Whether it has ended, however.
    pub fn over(&self) -> bool {
        !matches!(self.stage(), Stage::Waiting | Stage::Running)
    }

    pub fn error(&self) -> String {
        self.error.lock().map(|e| e.clone()).unwrap_or_default()
    }

    /// Why it failed, without what was being done to which item: the queue's row already
    /// names it. `Enviar "foto.jpg": HTTP 403: …` reads `HTTP 403: …`.
    pub fn reason(&self) -> String {
        reason(&self.error()).to_string()
    }

    /// Marks this row skipped due to a destination conflict (leaves `bytes` untouched).
    pub fn skip(&self) {
        self.stage.store(Stage::Skipped as u8, Ordering::Relaxed);
        self.tick(true);
    }

    /// Asks it to stop: one still waiting is cancelled at once and won't start; one running
    /// stops within a chunk.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        let _ = self.stage.compare_exchange(Stage::Waiting as u8, Stage::Cancelled as u8, Ordering::Relaxed, Ordering::Relaxed);
        self.tick(true);
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Its size, once worked out.
    pub fn size(&self, bytes: u64) {
        self.total.store(bytes, Ordering::Relaxed);
        self.sized.store(true, Ordering::Relaxed);
        self.tick(false);
    }

    /// Takes `bytes` off its size: part of it left out (an upload's skipped files).
    pub fn unsize(&self, bytes: u64) {
        let _ = self.total.try_update(Ordering::Relaxed, Ordering::Relaxed, |t| Some(t.saturating_sub(bytes)));
        self.tick(false);
    }

    /// Starts it running, unless it was cancelled while waiting.
    pub fn begin(&self) -> bool {
        if self.stage.compare_exchange(Stage::Waiting as u8, Stage::Running as u8, Ordering::Relaxed, Ordering::Relaxed).is_err() {
            return false;
        }
        if let Ok(mut b) = self.began.lock() {
            *b = Some(Instant::now());
        }
        self.tick(true);
        true
    }

    /// Seconds since it started running.
    pub fn running_for(&self) -> Option<f64> {
        self.began.lock().ok()?.map(|b| b.elapsed().as_secs_f64())
    }

    /// How it ended: done (its bar full), cancelled, or failed with why.
    pub fn end(&self, result: Result<(), String>) {
        let stage = match result {
            Ok(()) => {
                self.bytes.store(self.total.load(Ordering::Relaxed), Ordering::Relaxed);
                Stage::Done
            }
            Err(e) if e == CANCELLED || self.cancelled() => Stage::Cancelled,
            Err(e) => {
                if let Ok(mut m) = self.error.lock() {
                    *m = e;
                }
                Stage::Failed
            }
        };
        self.stage.store(stage as u8, Ordering::Relaxed);
        self.tick(true);
    }

    /// Takes back bytes reported for data that has to go again (an upload resumed from
    /// further back).
    pub fn sub_bytes(&self, n: u64) {
        let _ = self.bytes.try_update(Ordering::Relaxed, Ordering::Relaxed, |b| Some(b.saturating_sub(n)));
        self.tick(false);
    }

    /// More items found to do.
    pub fn add_items(&self, n: u64) {
        self.items.fetch_add(n, Ordering::Relaxed);
        self.tick(false);
    }

    pub fn item_done(&self) {
        self.items_done.fetch_add(1, Ordering::Relaxed);
        self.tick(false);
    }

    pub fn add_bytes(&self, n: u64) {
        self.bytes.fetch_add(n, Ordering::Relaxed);
        self.tick(false);
    }

    /// A transfer that had reached `*reached` bytes says it's at `now`: what's new is added,
    /// and a count that goes back (a part sent again) adds nothing until it passes `*reached`
    /// again, so the bar neither wraps around nor counts the part twice.
    pub fn reached(&self, reached: &mut u64, now: u64) {
        self.add_bytes(now.saturating_sub(*reached));
        *reached = (*reached).max(now);
    }
}

/// Characters Windows doesn't allow in a file name.
const FORBIDDEN: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

/// Why `name` can't be a file or folder name, or `None` if it can.
pub fn invalid_name(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return Some(t("O nome não pode ficar vazio.", "The name can't be empty.").into());
    }
    if name == "." || name == ".." {
        return Some(t("Esse nome não é permitido.", "That name isn't allowed.").into());
    }
    if let Some(c) = name.chars().find(|c| FORBIDDEN.contains(c) || c.is_control()) {
        return Some(tr!("O nome não pode conter o caractere {c:?}.", "The name can't contain the character {c:?}."));
    }
    None
}

/// Opens a file or folder with the program Windows associates with it.
pub fn open(path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide = |s: &std::ffi::OsStr| s.encode_wide().chain([0]).collect::<Vec<u16>>();
        let (verb, file) = (wide("open".as_ref()), wide(path.as_os_str()));
        // SAFETY: NUL-terminated strings that outlive the call; the window and directory may be
        // null. ShellExecuteW returns a value above 32 on success. 1 is SW_SHOWNORMAL.
        let code = unsafe {
            windows_sys::Win32::UI::Shell::ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), 1)
        } as isize;
        if code > 32 { Ok(()) } else { Err(tr!("O Windows não conseguiu abrir {} (código {code}).", "Windows couldn't open {} (code {code}).", path.display())) }
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open").arg(path).spawn().map(|_| ()).map_err(|e| e.to_string())
    }
}

/// `name` in `dir`, or the first of `stem - Cópia`, `stem - Cópia (2)`... that doesn't exist
/// there yet, keeping the extension.
pub fn unique_in(dir: &Path, name: &str) -> PathBuf {
    unique_where(dir, name, &|_| false)
}

/// A temporary path next to `to` for atomic overwrite-safe transfers.
pub fn part_path(to: &Path) -> PathBuf {
    static PARTS: AtomicU64 = AtomicU64::new(0);
    let n = PARTS.fetch_add(1, Ordering::Relaxed);
    let mut name = to.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}-{n}.openexplorer-part", std::process::id()));
    to.with_file_name(name)
}

/// [`unique_in`], also passing over the paths `taken` says are spoken for (by downloads under
/// way that haven't made their file yet).
pub fn unique_where(dir: &Path, name: &str, taken: &dyn Fn(&Path) -> bool) -> PathBuf {
    let free = |p: &PathBuf| !p.exists() && !taken(p);
    let first = dir.join(name);
    if free(&first) {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    (1..)
        .map(|n| if n == 1 { tr!("{stem} - Cópia{ext}", "{stem} - Copy{ext}") } else { tr!("{stem} - Cópia ({n}){ext}", "{stem} - Copy ({n}){ext}") })
        .map(|n| dir.join(n))
        .find(free)
        .expect("an unused name exists")
}

/// A name Windows takes for one Google Drive allows: a `/` becomes the `／` the app shows for it
/// (see [`crate::model::NAME_SLASH`]), other characters Windows refuses become `_`, trailing dots
/// and spaces go, and a device name (`CON`, `aux.txt`) gets a `_` after it.
pub fn windows_name(name: &str) -> String {
    let mut out: String = crate::model::shown(name).chars().map(|c| if FORBIDDEN.contains(&c) || c.is_control() { '_' } else { c }).collect();
    out.truncate(out.trim_end_matches(['.', ' ']).len());
    let stem = out.split('.').next().unwrap_or_default().trim_end().to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit() && stem.as_bytes()[3] != b'0');
    if device {
        out.insert(stem.len(), '_');
    }
    if out.is_empty() { "_".into() } else { out }
}

pub fn rename(from: &Path, name: &str) -> Result<PathBuf, String> {
    if let Some(why) = invalid_name(name) {
        return Err(why);
    }
    let to = from.with_file_name(name.trim());
    if to.exists() && !same_file(from, &to) {
        return Err(tr!("Já existe um item chamado \"{}\" aqui.", "There's already an item named \"{}\" here.", name.trim()));
    }
    std::fs::rename(from, &to).map(|_| to).map_err(|e| tr!("Não consegui renomear: {e}", "Couldn't rename: {e}"))
}

/// Renames each `(from, to)` together, through temporary names, so they can swap or chain
/// (`a` → `b` and `b` → `a`, or `a` → `b` and `b` → `c`). Each `to` must be free or the `from` of
/// another pair; nothing is touched until that holds. When a rename fails, every item goes back
/// to its first name, and the error says which (if any) couldn't, under their temporary name.
pub fn rename_together(moves: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    for (from, to) in moves {
        let name = to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some(why) = invalid_name(&name) {
            return Err(why);
        }
        if to.exists() && !moves.iter().any(|(other, _)| same_file(other, to)) {
            return Err(tr!("Já existe um item chamado \"{}\" aqui.", "There's already an item named \"{}\" here.", name));
        }
        if !from.exists() {
            return Err(tr!("\"{}\" não está mais aqui.", "\"{}\" is no longer here.", from.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
        }
    }
    let pid = std::process::id();
    // Phase 1: every item to a free temporary name in its own folder.
    let mut staged: Vec<PathBuf> = Vec::with_capacity(moves.len());
    for (idx, (from, _)) in moves.iter().enumerate() {
        let tmp = (0usize..).map(|n| from.with_file_name(format!(".oe-batch-tmp-{pid}-{idx}-{n}"))).find(|p| !p.exists()).expect("a free name");
        if let Err(e) = std::fs::rename(from, &tmp) {
            let stuck = put_back(moves, &staged, 0);
            return Err(with_stuck(tr!("Não consegui renomear \"{}\": {e}", "Couldn't rename \"{}\": {e}", from.display()), &stuck));
        }
        staged.push(tmp);
    }
    // Phase 2: each to its new name. `std::fs::rename` would replace a file that appeared there
    // meanwhile, so a taken name stops the batch.
    for (k, ((_, to), tmp)) in moves.iter().zip(&staged).enumerate() {
        let result = if to.exists() {
            Err(tr!("Já existe um item chamado \"{}\" aqui.", "There's already an item named \"{}\" here.", to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()))
        } else {
            std::fs::rename(tmp, to).map_err(|e| tr!("Não consegui renomear: {e}", "Couldn't rename: {e}"))
        };
        if let Err(e) = result {
            let stuck = put_back(moves, &staged, k);
            return Err(with_stuck(e, &stuck));
        }
    }
    Ok(())
}

/// Undoes [`rename_together`] after a failure: the first `placed` items leave their new names
/// for their temporary ones, then every temporary name goes back to the item's first name. The
/// temporary names that couldn't go back.
fn put_back(moves: &[(PathBuf, PathBuf)], staged: &[PathBuf], placed: usize) -> Vec<PathBuf> {
    for ((_, to), tmp) in moves.iter().zip(staged).take(placed).rev() {
        let _ = std::fs::rename(to, tmp);
    }
    let mut stuck = Vec::new();
    for ((from, _), tmp) in moves.iter().zip(staged).rev() {
        if !tmp.exists() {
            continue;
        }
        if from.exists() || std::fs::rename(tmp, from).is_err() {
            stuck.push(tmp.clone());
        }
    }
    stuck
}

fn with_stuck(err: String, stuck: &[PathBuf]) -> String {
    if stuck.is_empty() {
        return tr!("{err} Nada foi renomeado.", "{err} Nothing was renamed.");
    }
    let names: Vec<String> = stuck.iter().map(|p| p.display().to_string()).collect();
    tr!("{err} Ficaram com nome temporário: {}", "{err} Left under a temporary name: {}", names.join(", "))
}

/// Same path ignoring case, as Windows compares names: renaming `a.txt` to `A.txt` is allowed.
fn same_file(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

pub fn new_folder(dir: &Path, name: &str) -> Result<PathBuf, String> {
    if let Some(why) = invalid_name(name) {
        return Err(why);
    }
    let path = dir.join(name.trim());
    if path.exists() {
        return Err(tr!("Já existe um item chamado \"{}\" aqui.", "There's already an item named \"{}\" here.", name.trim()));
    }
    std::fs::create_dir(&path).map(|_| path).map_err(|e| tr!("Não consegui criar a pasta: {e}", "Couldn't create the folder: {e}"))
}

/// Sends files and folders to the Recycle Bin, where they can be restored.
pub fn to_recycle_bin(paths: &[PathBuf]) -> Result<(), String> {
    trash::delete_all(paths).map_err(|e| tr!("Não consegui mover para a Lixeira: {e}", "Couldn't move to the Recycle Bin: {e}"))
}

/// Deletes files and folders for good, where there's no Recycle Bin (a shared folder, WSL).
/// A link to a folder goes, not what it leads to.
pub fn delete_forever(paths: &[PathBuf]) -> Result<(), String> {
    for (i, path) in paths.iter().enumerate() {
        // A link to a folder isn't `is_dir` here; Windows removes it as a folder.
        let gone = std::fs::symlink_metadata(path).and_then(|meta| match meta.is_dir() {
            true => std::fs::remove_dir_all(path),
            false => std::fs::remove_file(path).or_else(|e| if meta.is_symlink() { std::fs::remove_dir(path) } else { Err(e) }),
        });
        // Say how far it got: what went before an error is gone for good.
        gone.map_err(|e| {
            let done = if i == 0 { String::new() } else { tr!(" (já excluídos para sempre: {i})", " (already deleted for good: {i})") };
            tr!("Não consegui excluir {}: {e}{done}", "Couldn't delete {}: {e}{done}", path.display())
        })?;
    }
    Ok(())
}

/// Copies a file or a whole folder into `dir`, under a free name there.
pub fn copy_into(src: &Path, dir: &Path, prog: &Progress) -> Result<PathBuf, String> {
    let name = src.file_name().ok_or(t("Item sem nome.", "Item has no name."))?.to_string_lossy().into_owned();
    if src.is_dir() && dir.starts_with(src) {
        return Err(tr!("Não dá para copiar a pasta \"{name}\" para dentro dela mesma.", "Can't copy the folder \"{name}\" into itself."));
    }
    let to = unique_in(dir, &name);
    copy_tree(src, &to, prog).map(|_| to.clone()).map_err(|e| failed(e, &to, &tr!("Não consegui copiar \"{name}\"", "Couldn't copy \"{name}\"")))
}

/// The message for a copy that stopped. A cancelled one takes away the item it was in the
/// middle of, so what's left in the destination is only whole items.
fn failed(e: std::io::Error, to: &Path, what: &str) -> String {
    if e.kind() != std::io::ErrorKind::Interrupted {
        return format!("{what}: {e}");
    }
    let _ = if to.is_dir() { std::fs::remove_dir_all(to) } else { std::fs::remove_file(to) };
    CANCELLED.into()
}

fn copy_tree(src: &Path, to: &Path, prog: &Progress) -> std::io::Result<()> {
    if prog.cancelled() {
        return Err(std::io::ErrorKind::Interrupted.into());
    }
    if src.is_dir() {
        std::fs::create_dir(to)?;
        for item in std::fs::read_dir(src)? {
            let item = item?;
            copy_tree(&item.path(), &to.join(item.file_name()), prog)?;
        }
        Ok(())
    } else {
        copy_file(src, to, prog)
    }
}

/// One file, through Windows' own copy (as Explorer does: attributes and dates come along),
/// reporting each chunk and stopping, with the partial file removed, on Cancelar.
#[cfg(windows)]
fn copy_file(src: &Path, to: &Path, prog: &Progress) -> std::io::Result<()> {
    copy_file_flags(src, to, prog, windows_sys::Win32::Storage::FileSystem::COPY_FILE_FAIL_IF_EXISTS)
}

#[cfg(windows)]
fn copy_file_flags(src: &Path, to: &Path, prog: &Progress, flags: u32) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{CopyFileExW, PROGRESS_CANCEL, PROGRESS_CONTINUE};
    struct Sent<'a> {
        prog: &'a Progress,
        bytes: u64,
    }
    /// Called by CopyFileExW between chunks, on this thread.
    unsafe extern "system" fn chunk(_size: i64, done: i64, _: i64, _: i64, _: u32, _: u32, _: HANDLE, _: HANDLE, data: *const core::ffi::c_void) -> u32 {
        // SAFETY: `data` is the `Sent` below, alive for the whole call.
        let sent = unsafe { &mut *(data as *mut Sent) };
        let done = done.max(0) as u64;
        if done > sent.bytes {
            sent.prog.add_bytes(done - sent.bytes);
            sent.bytes = done;
        }
        if sent.prog.cancelled() { PROGRESS_CANCEL } else { PROGRESS_CONTINUE }
    }
    let wide = |p: &Path| p.as_os_str().encode_wide().chain([0]).collect::<Vec<u16>>();
    let (from, into) = (wide(src), wide(to));
    let mut sent = Sent { prog, bytes: 0 };
    // SAFETY: NUL-terminated paths, and a callback whose data outlives the call.
    let ok = unsafe { CopyFileExW(from.as_ptr(), into.as_ptr(), Some(chunk), (&raw mut sent).cast(), std::ptr::null_mut(), flags) };
    if ok != 0 {
        Ok(())
    } else if prog.cancelled() {
        Err(std::io::ErrorKind::Interrupted.into())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(windows))]
fn copy_file(src: &Path, to: &Path, prog: &Progress) -> std::io::Result<()> {
    prog.add_bytes(std::fs::copy(src, to)?);
    Ok(())
}

/// Copies a single file `src` to `to`, atomically replacing an existing destination via a `.part`
/// file when `overwrite` is true so a cancelled copy never destroys the previous destination file.
pub fn copy_file_to(src: &Path, to: &Path, overwrite: bool, prog: &Progress) -> std::io::Result<()> {
    if !overwrite {
        return copy_file(src, to, prog);
    }
    let part = part_path(to);
    #[cfg(windows)]
    let res = copy_file_flags(src, &part, prog, 0);
    #[cfg(not(windows))]
    let res = copy_file(src, &part, prog);
    if let Err(e) = res {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&part, to) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    Ok(())
}

/// Result of a conflict-aware local transfer.
#[derive(Debug, PartialEq, Eq)]
pub enum TransferOutcome {
    Skipped,
    Copied { made: PathBuf, replaced: bool },
    Moved { from: PathBuf, to: PathBuf, replaced: bool },
    Merged,
}

/// Copies or moves `src` into `dst`, resolving destination collisions with `resolver`.
/// Same-folder copies auto-rename without prompting; folders meeting folders merge.
pub fn transfer_local(src: &Path, dst: &Path, move_item: bool, prog: &Progress, resolver: Option<&crate::conflict::Resolver>) -> Result<TransferOutcome, String> {
    use crate::conflict::{self, Decision, FileInfo};
    let name = src.file_name().ok_or(t("Item sem nome.", "Item has no name."))?.to_string_lossy().into_owned();
    if !move_item && src.parent() == Some(dst) {
        return copy_into(src, dst, prog).map(|made| TransferOutcome::Copied { made, replaced: false });
    }
    if move_item && src.parent() == Some(dst) {
        return Ok(TransferOutcome::Moved { from: src.to_path_buf(), to: src.to_path_buf(), replaced: false });
    }
    if src.is_dir() && dst.starts_with(src) {
        return Err(if move_item {
            tr!("Não dá para mover a pasta \"{name}\" para dentro dela mesma.", "Can't move the folder \"{name}\" into itself.")
        } else {
            tr!("Não dá para copiar a pasta \"{name}\" para dentro dela mesma.", "Can't copy the folder \"{name}\" into itself.")
        });
    }
    let to = dst.join(&name);
    if !to.exists() {
        return if move_item {
            move_into(src, dst, prog).map(|to| TransferOutcome::Moved { from: src.to_path_buf(), to, replaced: false })
        } else {
            copy_tree(src, &to, prog)
                .map(|_| TransferOutcome::Copied { made: to.clone(), replaced: false })
                .map_err(|e| failed(e, &to, &tr!("Não consegui copiar \"{name}\"", "Couldn't copy \"{name}\"")))
        };
    }
    if src.is_dir() && to.is_dir() {
        let mut kept = false;
        merge_local_dir(src, &to, move_item, prog, resolver, &mut kept)?;
        if move_item && !kept {
            let _ = std::fs::remove_dir(src);
        }
        return Ok(TransferOutcome::Merged);
    }
    let Some(res) = resolver else {
        return if move_item {
            move_into(src, dst, prog).map(|to| TransferOutcome::Moved { from: src.to_path_buf(), to, replaced: false })
        } else {
            copy_into(src, dst, prog).map(|made| TransferOutcome::Copied { made, replaced: false })
        };
    };
    let src_info = FileInfo::local(src).unwrap_or_else(|| FileInfo {
        name: name.clone(),
        place: src.parent().map(|p| p.display().to_string()).unwrap_or_default(),
        is_dir: src.is_dir(),
        size: None,
        modified: None,
        kind: conflict::kind_of(&name, src.is_dir(), None),
    });
    let dst_info = FileInfo::local(&to).unwrap_or_else(|| FileInfo {
        name: name.clone(),
        place: dst.display().to_string(),
        is_dir: to.is_dir(),
        size: None,
        modified: None,
        kind: conflict::kind_of(&name, to.is_dir(), None),
    });
    let free = conflict::numbered_local(dst, &name, src.is_dir(), &|_| false);
    let renamed = free.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| name.clone());
    match res.resolve(src_info, dst_info, renamed, || prog.cancelled()) {
        Decision::Cancel => Err(CANCELLED.into()),
        Decision::Skip => {
            prog.skip();
            Ok(TransferOutcome::Skipped)
        }
        Decision::Replace if !src.is_dir() && !to.is_dir() => transfer_local_file(src, &to, &name, move_item, true, prog).map(|(target, replaced)| {
            if move_item {
                TransferOutcome::Moved { from: src.to_path_buf(), to: target, replaced }
            } else {
                TransferOutcome::Copied { made: target, replaced }
            }
        }),
        Decision::KeepBoth | Decision::Replace => {
            if move_item {
                if std::fs::rename(src, &free).is_ok() {
                    return Ok(TransferOutcome::Moved { from: src.to_path_buf(), to: free, replaced: false });
                }
                copy_tree(src, &free, prog).map_err(|e| failed(e, &free, &tr!("Não consegui mover \"{name}\"", "Couldn't move \"{name}\"")))?;
                let removed = if src.is_dir() { std::fs::remove_dir_all(src) } else { std::fs::remove_file(src) };
                removed
                    .map(|_| TransferOutcome::Moved { from: src.to_path_buf(), to: free, replaced: false })
                    .map_err(|e| tr!("\"{name}\" foi copiado, mas o original não pôde ser apagado: {e}", "\"{name}\" was copied, but the original couldn't be deleted: {e}"))
            } else {
                copy_tree(src, &free, prog)
                    .map(|_| TransferOutcome::Copied { made: free.clone(), replaced: false })
                    .map_err(|e| failed(e, &free, &tr!("Não consegui copiar \"{name}\"", "Couldn't copy \"{name}\"")))
            }
        }
    }
}

fn transfer_local_file(src: &Path, to: &Path, name: &str, move_item: bool, overwrite: bool, prog: &Progress) -> Result<(PathBuf, bool), String> {
    if move_item {
        if std::fs::rename(src, to).is_ok() {
            return Ok((to.to_path_buf(), overwrite));
        }
        copy_file_to(src, to, overwrite, prog).map_err(|e| {
            if e.kind() == std::io::ErrorKind::Interrupted {
                CANCELLED.into()
            } else {
                tr!("Não consegui mover \"{name}\": {e}", "Couldn't move \"{name}\": {e}")
            }
        })?;
        std::fs::remove_file(src).map_err(|e| tr!("\"{name}\" foi copiado, mas o original não pôde ser apagado: {e}", "\"{name}\" was copied, but the original couldn't be deleted: {e}"))?;
        Ok((to.to_path_buf(), overwrite))
    } else {
        copy_file_to(src, to, overwrite, prog).map_err(|e| {
            if e.kind() == std::io::ErrorKind::Interrupted {
                CANCELLED.into()
            } else {
                tr!("Não consegui copiar \"{name}\": {e}", "Couldn't copy \"{name}\": {e}")
            }
        })?;
        Ok((to.to_path_buf(), overwrite))
    }
}

fn merge_local_dir(src: &Path, to: &Path, move_item: bool, prog: &Progress, resolver: Option<&crate::conflict::Resolver>, kept: &mut bool) -> Result<(), String> {
    use crate::conflict::{self, Decision, FileInfo};
    let entries = std::fs::read_dir(src).map_err(|e| tr!("Não consegui ler \"{}\": {e}", "Couldn't read \"{}\": {e}", src.display()))?;
    for entry in entries {
        if prog.cancelled() {
            return Err(CANCELLED.into());
        }
        let entry = entry.map_err(|e| tr!("Não consegui ler \"{}\": {e}", "Couldn't read \"{}\": {e}", src.display()))?;
        let child_src = entry.path();
        let child_name = entry.file_name().to_string_lossy().into_owned();
        let child_to = to.join(&child_name);
        if !child_to.exists() {
            if move_item {
                if std::fs::rename(&child_src, &child_to).is_err() {
                    copy_tree(&child_src, &child_to, prog).map_err(|e| failed(e, &child_to, &tr!("Não consegui mover \"{child_name}\"", "Couldn't move \"{child_name}\"")))?;
                    let removed = if child_src.is_dir() { std::fs::remove_dir_all(&child_src) } else { std::fs::remove_file(&child_src) };
                    removed.map_err(|e| tr!("\"{child_name}\" foi copiado, mas o original não pôde ser apagado: {e}", "\"{child_name}\" was copied, but the original couldn't be deleted: {e}"))?;
                }
            } else {
                copy_tree(&child_src, &child_to, prog).map_err(|e| failed(e, &child_to, &tr!("Não consegui copiar \"{child_name}\"", "Couldn't copy \"{child_name}\"")))?;
            }
            continue;
        }
        if child_src.is_dir() && child_to.is_dir() {
            let mut sub_kept = false;
            merge_local_dir(&child_src, &child_to, move_item, prog, resolver, &mut sub_kept)?;
            if sub_kept {
                *kept = true;
            } else if move_item {
                let _ = std::fs::remove_dir(&child_src);
            }
            continue;
        }
        let Some(res) = resolver else {
            let free = unique_in(to, &child_name);
            if move_item {
                if std::fs::rename(&child_src, &free).is_err() {
                    copy_tree(&child_src, &free, prog).map_err(|e| failed(e, &free, &tr!("Não consegui mover \"{child_name}\"", "Couldn't move \"{child_name}\"")))?;
                    let removed = if child_src.is_dir() { std::fs::remove_dir_all(&child_src) } else { std::fs::remove_file(&child_src) };
                    removed.map_err(|e| tr!("\"{child_name}\" foi copiado, mas o original não pôde ser apagado: {e}", "\"{child_name}\" was copied, but the original couldn't be deleted: {e}"))?;
                }
            } else {
                copy_tree(&child_src, &free, prog).map_err(|e| failed(e, &free, &tr!("Não consegui copiar \"{child_name}\"", "Couldn't copy \"{child_name}\"")))?;
            }
            continue;
        };
        let src_info = FileInfo::local(&child_src).unwrap_or_else(|| FileInfo {
            name: child_name.clone(),
            place: src.display().to_string(),
            is_dir: child_src.is_dir(),
            size: None,
            modified: None,
            kind: conflict::kind_of(&child_name, child_src.is_dir(), None),
        });
        let dst_info = FileInfo::local(&child_to).unwrap_or_else(|| FileInfo {
            name: child_name.clone(),
            place: to.display().to_string(),
            is_dir: child_to.is_dir(),
            size: None,
            modified: None,
            kind: conflict::kind_of(&child_name, child_to.is_dir(), None),
        });
        let free = conflict::numbered_local(to, &child_name, child_src.is_dir(), &|_| false);
        let renamed = free.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| child_name.clone());
        match res.resolve(src_info, dst_info, renamed, || prog.cancelled()) {
            Decision::Cancel => return Err(CANCELLED.into()),
            Decision::Skip => {
                *kept = true;
                prog.unsize(measure(&child_src, &|| prog.cancelled()));
            }
            Decision::Replace if !child_src.is_dir() && !child_to.is_dir() => {
                transfer_local_file(&child_src, &child_to, &child_name, move_item, true, prog)?;
            }
            Decision::KeepBoth | Decision::Replace => {
                if move_item {
                    if std::fs::rename(&child_src, &free).is_err() {
                        copy_tree(&child_src, &free, prog).map_err(|e| failed(e, &free, &tr!("Não consegui mover \"{child_name}\"", "Couldn't move \"{child_name}\"")))?;
                        let removed = if child_src.is_dir() { std::fs::remove_dir_all(&child_src) } else { std::fs::remove_file(&child_src) };
                        removed.map_err(|e| tr!("\"{child_name}\" foi copiado, mas o original não pôde ser apagado: {e}", "\"{child_name}\" was copied, but the original couldn't be deleted: {e}"))?;
                    }
                } else {
                    copy_tree(&child_src, &free, prog).map_err(|e| failed(e, &free, &tr!("Não consegui copiar \"{child_name}\"", "Couldn't copy \"{child_name}\"")))?;
                }
            }
        }
    }
    Ok(())
}

/// The bytes `path` holds, everything inside it included.
pub fn measure(path: &Path, stop: &dyn Fn() -> bool) -> u64 {
    totals(&[path.to_path_buf()], stop).bytes
}

/// Moves a file or folder into `dir`, keeping its name; a rename when both are on one disk,
/// a copy then delete across disks.
pub fn move_into(src: &Path, dir: &Path, prog: &Progress) -> Result<PathBuf, String> {
    let name = src.file_name().ok_or(t("Item sem nome.", "Item has no name."))?.to_string_lossy().into_owned();
    if src.parent() == Some(dir) {
        return Ok(src.to_path_buf());
    }
    if src.is_dir() && dir.starts_with(src) {
        return Err(tr!("Não dá para mover a pasta \"{name}\" para dentro dela mesma.", "Can't move the folder \"{name}\" into itself."));
    }
    let to = dir.join(&name);
    if to.exists() {
        return Err(tr!("Já existe \"{name}\" no destino.", "\"{name}\" already exists in the destination."));
    }
    if std::fs::rename(src, &to).is_ok() {
        return Ok(to);
    }
    copy_tree(src, &to, prog).map_err(|e| failed(e, &to, &tr!("Não consegui mover \"{name}\"", "Couldn't move \"{name}\"")))?;
    let removed = if src.is_dir() { std::fs::remove_dir_all(src) } else { std::fs::remove_file(src) };
    removed.map(|_| to).map_err(|e| tr!("\"{name}\" foi copiado, mas o original não pôde ser apagado: {e}", "\"{name}\" was copied, but the original couldn't be deleted: {e}"))
}

/// What a set of files and folders adds up to: bytes and how many files and folders, counting
/// everything inside the folders.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub bytes: u64,
    pub files: u64,
    pub dirs: u64,
}

/// Walks `paths` (and every folder inside them) adding up sizes. Folders it can't read are
/// skipped; `stop` is checked between entries, to give up when the dialog closes.
pub fn totals(paths: &[PathBuf], stop: &dyn Fn() -> bool) -> Totals {
    let mut t = Totals::default();
    let mut todo: Vec<PathBuf> = paths.to_vec();
    while let Some(p) = todo.pop() {
        if stop() {
            break;
        }
        // symlink_metadata: a link to a folder counts as itself, not what it points to, so a
        // link loop can't make this walk forever.
        let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
        // A single item doesn't count itself ("Contém" is what's inside it); among several,
        // each one counts, as in Explorer.
        let counts = !paths.contains(&p) || paths.len() > 1;
        if meta.is_dir() {
            if counts {
                t.dirs += 1;
            }
            if let Ok(items) = std::fs::read_dir(&p) {
                todo.extend(items.flatten().map(|i| i.path()));
            }
        } else {
            if counts {
                t.files += 1;
            }
            t.bytes += meta.len();
        }
    }
    t
}

/// The MD5 of a file's content, in lowercase hex, read in 1 MB chunks.
pub fn md5_file(path: &Path, stop: &dyn Fn() -> bool) -> Result<String, String> {
    use md5::{Digest, Md5};
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| tr!("Não consegui ler o arquivo: {e}", "Couldn't read the file: {e}"))?;
    let mut hash = Md5::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if stop() {
            return Err("Cancelado.".into());
        }
        let n = f.read(&mut buf).map_err(|e| tr!("Não consegui ler o arquivo: {e}", "Couldn't read the file: {e}"))?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// The Windows attributes set in `meta`: Somente leitura, Oculto, Sistema, Arquivo...
pub fn attributes(meta: &std::fs::Metadata) -> Vec<&'static str> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let a = meta.file_attributes();
        // FILE_ATTRIBUTE_* bits.
        [(0x1, t("Somente leitura", "Read-only")), (0x2, t("Oculto", "Hidden")), (0x4, t("Sistema", "System")), (0x20, t("Arquivo", "Archive")), (0x800, t("Compactado", "Compressed")), (0x4000, t("Criptografado", "Encrypted"))]
            .into_iter()
            .filter(|(bit, _)| a & bit != 0)
            .map(|(_, name)| name)
            .collect()
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        Vec::new()
    }
}

/// `error` without its leading `Verb "item": ` contexts, however many are stacked.
fn reason(mut error: &str) -> &str {
    loop {
        let Some((head, tail)) = error.split_once("\": ") else { return error };
        // The context is words then one quoted name: `Enviar "foto.jpg"`.
        if head.matches('"').count() != 1 || head.starts_with('"') || tail.trim().is_empty() {
            return error;
        }
        error = tail;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reason_drops_what_was_being_done() {
        assert_eq!(
            reason("Enviar \"WhatsApp Image 2026-11-18 at 11.49.49.jpeg\": HTTP 403: The user's Drive storage quota has been exceeded."),
            "HTTP 403: The user's Drive storage quota has been exceeded."
        );
        assert_eq!(reason("Não consegui criar a pasta \"C:\\x\": Acesso negado. (os error 5)"), "Acesso negado. (os error 5)");
        assert_eq!(reason("Copiar \"a\": Enviar \"b\": HTTP 500"), "HTTP 500");
        assert_eq!(reason("HTTP 404: File not found"), "HTTP 404: File not found");
        assert_eq!(reason("Enviar \"a\"b\": HTTP 500"), "Enviar \"a\"b\": HTTP 500");
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("openexplorer-ops-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn totals_count_what_is_inside_folders_and_md5_matches() {
        let d = scratch("totals");
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::write(d.join("a/um.txt"), "12345").unwrap();
        std::fs::write(d.join("a/b/dois.txt"), "abc").unwrap();
        let t = totals(&[d.join("a")], &|| false);
        assert_eq!(t, Totals { bytes: 8, files: 2, dirs: 1 });
        std::fs::write(d.join("solto.txt"), "xy").unwrap();
        assert_eq!(totals(&[d.join("a"), d.join("solto.txt")], &|| false), Totals { bytes: 10, files: 3, dirs: 2 });
        // The MD5 of "abc".
        assert_eq!(md5_file(&d.join("a/b/dois.txt"), &|| false).unwrap(), "900150983cd24fb0d6963f7d28e17f72");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn drive_names_become_names_windows_takes() {
        assert_eq!(windows_name("Reunião: pauta?.docx"), "Reunião_ pauta_.docx");
        assert_eq!(windows_name(r"a/b\c"), "a／b_c");
        assert_eq!(windows_name("fim. . "), "fim");
        assert_eq!(windows_name("con.txt"), "con_.txt");
        assert_eq!(windows_name("COM1"), "COM1_");
        assert_eq!(windows_name("Console.txt"), "Console.txt");
        assert_eq!(windows_name("..."), "_");
        let d = scratch("unique");
        std::fs::write(d.join("a.txt"), "").unwrap();
        let taken = d.join("a - Cópia.txt");
        assert_eq!(unique_where(&d, "a.txt", &|p| p == taken), d.join("a - Cópia (2).txt"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn names_windows_refuses_are_caught() {
        assert!(invalid_name("Relatório 2026.pdf").is_none());
        assert!(invalid_name("   ").is_some());
        assert!(invalid_name("a:b").is_some());
        assert!(invalid_name("..").is_some());
    }

    #[test]
    fn copies_get_a_free_name_and_folders_copy_whole() {
        let d = scratch("copy");
        std::fs::write(d.join("nota.txt"), "x").unwrap();
        let prog = Progress::new(None);
        assert_eq!(copy_into(&d.join("nota.txt"), &d, &prog).unwrap(), d.join("nota - Cópia.txt"));
        assert_eq!(copy_into(&d.join("nota.txt"), &d, &prog).unwrap(), d.join("nota - Cópia (2).txt"));
        assert_eq!(prog.bytes.load(Ordering::Relaxed), 2);
        std::fs::create_dir_all(d.join("pasta/sub")).unwrap();
        std::fs::write(d.join("pasta/sub/a.txt"), "y").unwrap();
        let out = scratch("copy-out");
        assert_eq!(measure(&d.join("pasta"), &|| false), 1);
        assert_eq!(measure(&d.join("nota.txt"), &|| false), 1);
        let copied = copy_into(&d.join("pasta"), &out, &prog).unwrap();
        assert_eq!(std::fs::read_to_string(copied.join("sub/a.txt")).unwrap(), "y");
        assert!(copy_into(&d.join("pasta"), &d.join("pasta/sub"), &prog).is_err());
        // Cancelar stops it, and the half-copied item doesn't stay behind.
        prog.cancel();
        assert_eq!(copy_into(&d.join("pasta"), &out, &prog).unwrap_err(), CANCELLED);
        assert!(!out.join("pasta - Cópia").exists());
        let _ = std::fs::remove_dir_all(d);
        let _ = std::fs::remove_dir_all(out);
    }

    #[test]
    fn renames_and_moves_keep_names_and_refuse_clashes() {
        let d = scratch("move");
        std::fs::write(d.join("a.txt"), "1").unwrap();
        std::fs::write(d.join("b.txt"), "2").unwrap();
        assert!(rename(&d.join("a.txt"), "b.txt").is_err());
        let a = rename(&d.join("a.txt"), "A.txt").unwrap();
        std::fs::create_dir(d.join("dest")).unwrap();
        let moved = move_into(&a, &d.join("dest"), &Progress::new(None)).unwrap();
        assert_eq!(std::fs::read_to_string(moved).unwrap(), "1");
        assert!(new_folder(&d, "dest").is_err());
        assert!(new_folder(&d, "Nova pasta").unwrap().is_dir());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn transfer_local_honours_replace_skip_keep_both_and_newer() {
        use crate::conflict::{Choice, Resolver};
        use std::time::{Duration, SystemTime};

        let root = scratch("conflict");
        let src = root.join("src");
        let dst = root.join("dst");
        std::fs::create_dir_all(src.join("pasta")).unwrap();
        std::fs::create_dir_all(dst.join("pasta")).unwrap();

        let older = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let newer = older + Duration::from_secs(60);
        let set_time = |p: &Path, t: SystemTime| {
            let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
            f.set_modified(t).unwrap();
        };

        // Replace overwrites destination content.
        std::fs::write(src.join("r.txt"), "novo").unwrap();
        std::fs::write(dst.join("r.txt"), "velho").unwrap();
        let out = transfer_local(&src.join("r.txt"), &dst, false, &Progress::new(None), Some(&Resolver::fixed(Choice::Replace))).unwrap();
        assert_eq!(out, TransferOutcome::Copied { made: dst.join("r.txt"), replaced: true });
        assert_eq!(std::fs::read_to_string(dst.join("r.txt")).unwrap(), "novo");

        // Skip keeps destination intact and marks Progress as Skipped.
        std::fs::write(src.join("s.txt"), "novo").unwrap();
        std::fs::write(dst.join("s.txt"), "velho").unwrap();
        let prog = Progress::new(None);
        let out = transfer_local(&src.join("s.txt"), &dst, true, &prog, Some(&Resolver::fixed(Choice::Skip))).unwrap();
        assert_eq!(out, TransferOutcome::Skipped);
        assert_eq!(prog.stage(), Stage::Skipped);
        assert_eq!(std::fs::read_to_string(dst.join("s.txt")).unwrap(), "velho");
        assert!(src.join("s.txt").exists(), "skipped move must keep source");

        // KeepBoth creates numbered copy without touching existing destination.
        std::fs::write(src.join("k.txt"), "novo").unwrap();
        std::fs::write(dst.join("k.txt"), "velho").unwrap();
        let out = transfer_local(&src.join("k.txt"), &dst, false, &Progress::new(None), Some(&Resolver::fixed(Choice::KeepBoth))).unwrap();
        assert_eq!(out, TransferOutcome::Copied { made: dst.join("k (2).txt"), replaced: false });
        assert_eq!(std::fs::read_to_string(dst.join("k.txt")).unwrap(), "velho");
        assert_eq!(std::fs::read_to_string(dst.join("k (2).txt")).unwrap(), "novo");

        // ReplaceNewer replaces only when source is newer.
        std::fs::write(src.join("n1.txt"), "mais-novo").unwrap();
        std::fs::write(dst.join("n1.txt"), "antigo").unwrap();
        set_time(&src.join("n1.txt"), newer);
        set_time(&dst.join("n1.txt"), older);
        let out = transfer_local(&src.join("n1.txt"), &dst, false, &Progress::new(None), Some(&Resolver::fixed(Choice::ReplaceNewer))).unwrap();
        assert_eq!(out, TransferOutcome::Copied { made: dst.join("n1.txt"), replaced: true });
        assert_eq!(std::fs::read_to_string(dst.join("n1.txt")).unwrap(), "mais-novo");

        std::fs::write(src.join("n2.txt"), "antigo-src").unwrap();
        std::fs::write(dst.join("n2.txt"), "mais-novo-dst").unwrap();
        set_time(&src.join("n2.txt"), older);
        set_time(&dst.join("n2.txt"), newer);
        let out = transfer_local(&src.join("n2.txt"), &dst, false, &Progress::new(None), Some(&Resolver::fixed(Choice::ReplaceNewer))).unwrap();
        assert_eq!(out, TransferOutcome::Skipped);
        assert_eq!(std::fs::read_to_string(dst.join("n2.txt")).unwrap(), "mais-novo-dst");

        // Folder merge keeps existing untouched files and resolves inner collisions.
        std::fs::write(dst.join("pasta/fica.txt"), "intacto").unwrap();
        std::fs::write(dst.join("pasta/colide.txt"), "destino").unwrap();
        std::fs::write(src.join("pasta/colide.txt"), "origem").unwrap();
        std::fs::write(src.join("pasta/novo.txt"), "novo").unwrap();
        let out = transfer_local(&src.join("pasta"), &dst, false, &Progress::new(None), Some(&Resolver::fixed(Choice::Skip))).unwrap();
        assert_eq!(out, TransferOutcome::Merged);
        assert_eq!(std::fs::read_to_string(dst.join("pasta/fica.txt")).unwrap(), "intacto");
        assert_eq!(std::fs::read_to_string(dst.join("pasta/colide.txt")).unwrap(), "destino");
        assert_eq!(std::fs::read_to_string(dst.join("pasta/novo.txt")).unwrap(), "novo");

        // Same-folder copy auto-renames without prompting.
        let out = transfer_local(&dst.join("r.txt"), &dst, false, &Progress::new(None), Some(&Resolver::fixed(Choice::Skip))).unwrap();
        assert_eq!(out, TransferOutcome::Copied { made: dst.join("r - Cópia.txt"), replaced: false });

        let _ = std::fs::remove_dir_all(root);
    }
}
