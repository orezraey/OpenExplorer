//! File operations on the local disk: open, rename, new folder, delete to the Recycle Bin,
//! copy and move. They block, so the app runs them off the UI thread.

use std::path::{Path, PathBuf};

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
pub fn copy_into(src: &Path, dir: &Path) -> Result<PathBuf, String> {
    let name = src.file_name().ok_or("Item sem nome.")?.to_string_lossy().into_owned();
    if src.is_dir() && dir.starts_with(src) {
        return Err(format!("Não dá para copiar a pasta \"{name}\" para dentro dela mesma."));
    }
    let to = unique_in(dir, &name);
    copy_tree(src, &to).map(|_| to).map_err(|e| format!("Não consegui copiar \"{name}\": {e}"))
}

fn copy_tree(src: &Path, to: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir(to)?;
        for item in std::fs::read_dir(src)? {
            let item = item?;
            copy_tree(&item.path(), &to.join(item.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, to).map(|_| ())
    }
}

/// Moves a file or folder into `dir`, keeping its name; a rename when both are on one disk,
/// a copy then delete across disks.
pub fn move_into(src: &Path, dir: &Path) -> Result<PathBuf, String> {
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
    copy_tree(src, &to).map_err(|e| format!("Não consegui mover \"{name}\": {e}"))?;
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
        assert_eq!(copy_into(&d.join("nota.txt"), &d).unwrap(), d.join("nota - Cópia.txt"));
        assert_eq!(copy_into(&d.join("nota.txt"), &d).unwrap(), d.join("nota - Cópia (2).txt"));
        std::fs::create_dir_all(d.join("pasta/sub")).unwrap();
        std::fs::write(d.join("pasta/sub/a.txt"), "y").unwrap();
        let out = scratch("copy-out");
        let copied = copy_into(&d.join("pasta"), &out).unwrap();
        assert_eq!(std::fs::read_to_string(copied.join("sub/a.txt")).unwrap(), "y");
        assert!(copy_into(&d.join("pasta"), &d.join("pasta/sub")).is_err());
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
        let moved = move_into(&a, &d.join("dest")).unwrap();
        assert_eq!(std::fs::read_to_string(moved).unwrap(), "1");
        assert!(new_folder(&d, "dest").is_err());
        assert!(new_folder(&d, "Nova pasta").unwrap().is_dir());
        let _ = std::fs::remove_dir_all(d);
    }
}
