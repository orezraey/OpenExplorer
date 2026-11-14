//! File operations on the local disk: open, rename, new folder, delete to the Recycle Bin,
//! copy and move. They block, so the app runs them off the UI thread.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

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
}

/// Characters Windows doesn't allow in a file name.
const FORBIDDEN: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

/// Why `name` can't be a file or folder name, or `None` if it can.
pub fn invalid_name(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return Some("O nome não pode ficar vazio.".into());
    }
    if name == "." || name == ".." {
        return Some("Esse nome não é permitido.".into());
    }
    if let Some(c) = name.chars().find(|c| FORBIDDEN.contains(c) || c.is_control()) {
        return Some(format!("O nome não pode conter o caractere {c:?}."));
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
        if code > 32 { Ok(()) } else { Err(format!("O Windows não conseguiu abrir {} (código {code}).", path.display())) }
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open").arg(path).spawn().map(|_| ()).map_err(|e| e.to_string())
    }
}

/// `name` in `dir`, or the first of `stem - Cópia`, `stem - Cópia (2)`... that doesn't exist
/// there yet, keeping the extension.
pub fn unique_in(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    (1..)
        .map(|n| if n == 1 { format!("{stem} - Cópia{ext}") } else { format!("{stem} - Cópia ({n}){ext}") })
        .map(|n| dir.join(n))
        .find(|p| !p.exists())
        .expect("an unused name exists")
}

pub fn rename(from: &Path, name: &str) -> Result<PathBuf, String> {
    if let Some(why) = invalid_name(name) {
        return Err(why);
    }
    let to = from.with_file_name(name.trim());
    if to.exists() && !same_file(from, &to) {
        return Err(format!("Já existe um item chamado \"{}\" aqui.", name.trim()));
    }
    std::fs::rename(from, &to).map(|_| to).map_err(|e| format!("Não consegui renomear: {e}"))
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
        return Err(format!("Já existe um item chamado \"{}\" aqui.", name.trim()));
    }
    std::fs::create_dir(&path).map(|_| path).map_err(|e| format!("Não consegui criar a pasta: {e}"))
}

/// Sends files and folders to the Recycle Bin, where they can be restored.
pub fn to_recycle_bin(paths: &[PathBuf]) -> Result<(), String> {
    trash::delete_all(paths).map_err(|e| format!("Não consegui mover para a Lixeira: {e}"))
}

/// Copies a file or a whole folder into `dir`, under a free name there.
pub fn copy_into(src: &Path, dir: &Path, prog: &Progress) -> Result<PathBuf, String> {
    let name = src.file_name().ok_or("Item sem nome.")?.to_string_lossy().into_owned();
    if src.is_dir() && dir.starts_with(src) {
        return Err(format!("Não dá para copiar a pasta \"{name}\" para dentro dela mesma."));
    }
    let to = unique_in(dir, &name);
    copy_tree(src, &to, prog).map(|_| to.clone()).map_err(|e| failed(e, &to, &format!("Não consegui copiar \"{name}\"")))
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
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{COPY_FILE_FAIL_IF_EXISTS, CopyFileExW, PROGRESS_CANCEL, PROGRESS_CONTINUE};
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
    let ok = unsafe { CopyFileExW(from.as_ptr(), into.as_ptr(), Some(chunk), (&raw mut sent).cast(), std::ptr::null_mut(), COPY_FILE_FAIL_IF_EXISTS) };
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

/// The bytes `path` holds, everything inside it included.
pub fn measure(path: &Path, stop: &dyn Fn() -> bool) -> u64 {
    totals(&[path.to_path_buf()], stop).bytes
}

/// Moves a file or folder into `dir`, keeping its name; a rename when both are on one disk,
/// a copy then delete across disks.
pub fn move_into(src: &Path, dir: &Path, prog: &Progress) -> Result<PathBuf, String> {
    let name = src.file_name().ok_or("Item sem nome.")?.to_string_lossy().into_owned();
    if src.parent() == Some(dir) {
        return Ok(src.to_path_buf());
    }
    if src.is_dir() && dir.starts_with(src) {
        return Err(format!("Não dá para mover a pasta \"{name}\" para dentro dela mesma."));
    }
    let to = dir.join(&name);
    if to.exists() {
        return Err(format!("Já existe \"{name}\" no destino."));
    }
    if std::fs::rename(src, &to).is_ok() {
        return Ok(to);
    }
    copy_tree(src, &to, prog).map_err(|e| failed(e, &to, &format!("Não consegui mover \"{name}\"")))?;
    let removed = if src.is_dir() { std::fs::remove_dir_all(src) } else { std::fs::remove_file(src) };
    removed.map(|_| to).map_err(|e| format!("\"{name}\" foi copiado, mas o original não pôde ser apagado: {e}"))
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
    let mut f = std::fs::File::open(path).map_err(|e| format!("Não consegui ler o arquivo: {e}"))?;
    let mut hash = Md5::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if stop() {
            return Err("Cancelado.".into());
        }
        let n = f.read(&mut buf).map_err(|e| format!("Não consegui ler o arquivo: {e}"))?;
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
        [(0x1, "Somente leitura"), (0x2, "Oculto"), (0x4, "Sistema"), (0x20, "Arquivo"), (0x800, "Compactado"), (0x4000, "Criptografado")]
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
