//! The app's caches on disk, for Opções › Cache: how much room they take, and clearing them.
//!
//! Three caches live under `%LOCALAPPDATA%\OpenExplorer`: `cache\`, the last listing of each
//! Google Drive folder (see [`crate::cache`]), `mega\`, each MEGA account's sealed tree
//! (see `openexplorer_mega::cache`), and `dropbox\`, each Dropbox account's sealed listings
//! (see `openexplorer_dropbox::cache`). All only spare reading from the network: clearing them
//! leaves accounts, sessions and settings as they are.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use openexplorer_i18n::{t, tr};

use crate::actions::Dialog;
use crate::{Msg, State};

/// What each cache takes on disk, in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub drive: u64,
    pub mega: u64,
    pub dropbox: u64,
}

impl Usage {
    pub fn total(self) -> u64 {
        self.drive + self.mega + self.dropbox
    }

    /// Measured now. Walks the folders, so it runs off the UI thread.
    fn now() -> Usage {
        Usage {
            drive: dir_size(&crate::cache::root()),
            mega: openexplorer_mega::cache::dir().map_or(0, |d| dir_size(&d)),
            dropbox: openexplorer_dropbox::cache::dir().map_or(0, |d| dir_size(&d)),
        }
    }
}

/// The Cache page's state while Opções is open.
#[derive(Debug, Default)]
pub struct Page {
    /// What the caches take, once measured.
    pub usage: Option<Usage>,
    /// The answer awaited (0: none), a measure's or a clear's: a later one replaces it.
    pub waiting: u64,
    /// Clearing now.
    pub clearing: bool,
    /// Limpar cache was pressed: its confirmation shows.
    pub confirming: bool,
    /// What the last clear couldn't delete.
    pub error: Option<String>,
}

/// What a background measure or clear brings back.
pub enum Answer {
    Measured { id: u64, usage: Usage },
    Cleared { id: u64, result: Result<(), String>, usage: Usage },
}

/// Numbers the answers, so one for a page since closed or replaced is told apart.
static NEXT: AtomicU64 = AtomicU64::new(1);

fn page(s: &mut State) -> Option<&mut Page> {
    match &mut s.dialog {
        Some(Dialog::Options(opt)) => Some(&mut opt.cache),
        _ => None,
    }
}

/// Measures the caches for the page, on a thread (the page just opened).
pub fn measure(s: &mut State) {
    let Some(page) = page(s) else { return };
    if page.clearing {
        return;
    }
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    page.waiting = id;
    crate::background(s, move |emit| emit(Msg::Caches(Answer::Measured { id, usage: Usage::now() })));
}

/// Shows (or hides) the confirmation of Limpar cache.
pub fn confirm(s: &mut State, on: bool) {
    if let Some(page) = page(s) {
        page.confirming = on && !page.clearing;
    }
}

/// Clears the caches: the Drive listings in memory now, then on a thread the files of all
/// (after the Drive writes already asked for, and holding off MEGA's), and measures again.
pub fn clear(s: &mut State) {
    let Some(page) = page(s) else { return };
    if page.clearing {
        return;
    }
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    *page = Page { usage: page.usage, waiting: id, clearing: true, confirming: false, error: None };
    let drive = s.cache.clear();
    openexplorer_gdrive::log("cache: limpando as listagens do Google Drive e as árvores do MEGA");
    crate::background(s, move |emit| {
        // No answer only if the Drive cache's disk thread is gone: clear it from here.
        let drive = drive.recv().unwrap_or_else(|_| clear_dir(&crate::cache::root()));
        let mega = openexplorer_mega::cache::clear_with(clear_dir).unwrap_or(Ok(()));
        let dropbox = openexplorer_dropbox::cache::clear_with(clear_dir).unwrap_or(Ok(()));
        let errors: Vec<String> = [drive, mega, dropbox].into_iter().filter_map(Result::err).collect();
        let result = if errors.is_empty() { Ok(()) } else { Err(errors.join("\n")) };
        emit(Msg::Caches(Answer::Cleared { id, result, usage: Usage::now() }));
    });
}

pub fn arrived(s: &mut State, answer: Answer) {
    let (id, usage, cleared) = match answer {
        Answer::Measured { id, usage } => (id, usage, None),
        Answer::Cleared { id, result, usage } => (id, usage, Some(result)),
    };
    if let Some(Err(e)) = &cleared {
        openexplorer_gdrive::log(&format!("cache: erro ao limpar: {e}"));
    }
    let shown = match page(s) {
        Some(page) if page.waiting == id => {
            page.usage = Some(usage);
            page.waiting = 0;
            page.clearing = false;
            if let Some(Err(e)) = &cleared {
                page.error = Some(e.clone());
            }
            true
        }
        _ => false,
    };
    match cleared {
        Some(Ok(())) => s.status = t("Cache limpo. As pastas voltam a ser lidas da rede ao abrir.", "Cache cleared. Folders are read from the network again when opened.").into(),
        // Shown on the page; with the page gone, in the status bar.
        Some(Err(e)) if !shown => crate::fail(s, e),
        _ => {}
    }
}

/// The bytes the files under `dir` take, subfolders included (links aren't followed). What
/// can't be read counts as nothing.
pub fn dir_size(dir: &Path) -> u64 {
    let Ok(list) = std::fs::read_dir(dir) else { return 0 };
    list.flatten()
        .map(|entry| match entry.metadata() {
            Ok(m) if m.is_dir() => dir_size(&entry.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

/// Deletes everything inside `dir` (the folder itself stays), going on past what can't be
/// deleted (a file in use) and naming it in the error. A missing folder is already clear.
pub fn clear_dir(dir: &Path) -> Result<(), String> {
    let mut failed = Vec::new();
    empty(dir, &mut failed);
    let Some((path, e)) = failed.first() else { return Ok(()) };
    Err(match failed.len() {
        1 => tr!(
            "Não foi possível apagar {}: {e}. Feche o que estiver usando o arquivo e tente de novo.",
            "Couldn't delete {}: {e}. Close whatever is using the file and try again.",
            path.display()
        ),
        n => tr!(
            "Não foi possível apagar {n} itens do cache, entre eles {}: {e}. Feche o que estiver usando os arquivos e tente de novo.",
            "Couldn't delete {n} items of the cache, among them {}: {e}. Close whatever is using the files and try again.",
            path.display()
        ),
    })
}

fn empty(dir: &Path, failed: &mut Vec<(PathBuf, std::io::Error)>) {
    let list = match std::fs::read_dir(dir) {
        Ok(list) => list,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => return failed.push((dir.to_path_buf(), e)),
    };
    for entry in list {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                failed.push((dir.to_path_buf(), e));
                continue;
            }
        };
        let path = entry.path();
        // A link to a folder is removed as itself, never followed.
        let kind = entry.file_type();
        let is_dir = kind.as_ref().is_ok_and(|t| t.is_dir());
        let is_link = kind.as_ref().is_ok_and(|t| t.is_symlink());
        let removed = if is_dir {
            let before = failed.len();
            empty(&path, failed);
            if failed.len() > before {
                // What's left inside is already named; the folder can't go with it there.
                continue;
            }
            std::fs::remove_dir(&path)
        } else {
            // Windows removes a link to a folder (or a junction) as a folder.
            std::fs::remove_file(&path).or_else(|e| if is_link { std::fs::remove_dir(&path) } else { Err(e) })
        };
        if let Err(e) = removed {
            failed.push((path, e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oe-caches-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("conta").join("sub")).unwrap();
        std::fs::write(dir.join("a.json"), [0u8; 1000]).unwrap();
        std::fs::write(dir.join("conta").join("b.json"), [0u8; 234]).unwrap();
        std::fs::write(dir.join("conta").join("sub").join("c.pos.json"), [0u8; 6]).unwrap();
        dir
    }

    #[test]
    fn the_size_counts_every_file_in_every_subfolder() {
        let dir = temp("size");
        assert_eq!(dir_size(&dir), 1240);
        assert_eq!(dir_size(&dir.join("conta")), 240);
        assert_eq!(dir_size(&dir.join("nada")), 0);
        assert_eq!(Usage { drive: 1240, mega: 60, dropbox: 700 }.total(), 2000);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn clearing_empties_the_folder_and_keeps_it() {
        let dir = temp("clear");
        assert_eq!(clear_dir(&dir), Ok(()));
        assert!(dir.is_dir());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        assert_eq!(clear_dir(&dir.join("nada")), Ok(()), "a missing folder is already clear");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A file another program holds open (without letting it be deleted) stays and is named;
    /// everything else still goes.
    #[cfg(windows)]
    #[test]
    fn a_file_in_use_is_named_and_the_rest_still_goes() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = temp("in-use");
        let held = dir.join("conta").join("b.json");
        let _open = std::fs::OpenOptions::new().read(true).share_mode(0).open(&held).unwrap();
        let e = clear_dir(&dir).unwrap_err();
        assert!(e.contains("b.json") && e.contains("Não foi possível apagar"), "{e}");
        assert!(held.exists());
        assert!(!dir.join("a.json").exists() && !dir.join("conta").join("sub").exists());
        drop(_open);
        assert_eq!(clear_dir(&dir), Ok(()));
        let _ = std::fs::remove_dir_all(dir);
    }
}
