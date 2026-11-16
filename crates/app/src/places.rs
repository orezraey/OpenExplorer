//! The computer's special folders, which the tree shows under Área de Trabalho as Explorer's
//! navigation pane does: the desktop itself (the user's and the one every user shares), the
//! user's folders (Documentos, Downloads, Imagens, Músicas, Vídeos) and the home folder, with
//! the names Windows gives them; and WSL's distributions, which the tree shows under Linux.

use std::path::{Path, PathBuf};

/// The special folders, read once at start.
#[derive(Clone, Debug, Default)]
pub struct Places {
    /// The user's desktop and its name ("Área de Trabalho"); `None` when there's none.
    pub desktop: Option<(PathBuf, String)>,
    /// Other folders whose folders show on the desktop: the shared Public Desktop.
    pub desktop_also: Vec<PathBuf>,
    /// The user's folders and the home folder, each with its name.
    pub user: Vec<(PathBuf, String)>,
    /// WSL's Linux distributions, by name (see [`crate::network::distros`]).
    pub wsl: Vec<String>,
}

impl Places {
    /// The name Windows gives `path` when it's one of the user's folders.
    pub fn name_of(&self, path: &Path) -> Option<&str> {
        self.user.iter().find(|(p, _)| p == path).map(|(_, n)| n.as_str())
    }
}

#[cfg(windows)]
pub fn places() -> Places {
    use windows_sys::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music, FOLDERID_Pictures, FOLDERID_Profile, FOLDERID_PublicDesktop, FOLDERID_Videos};
    let named = |p: PathBuf| {
        let name = display_name(&p).unwrap_or_else(|| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        (p, name)
    };
    let user = [&FOLDERID_Documents, &FOLDERID_Downloads, &FOLDERID_Pictures, &FOLDERID_Music, &FOLDERID_Videos, &FOLDERID_Profile]
        .into_iter()
        .filter_map(known_folder)
        .filter(|p| p.is_dir())
        .map(named)
        .collect();
    Places {
        desktop: known_folder(&FOLDERID_Desktop).filter(|p| p.is_dir()).map(named),
        desktop_also: known_folder(&FOLDERID_PublicDesktop).filter(|p| p.is_dir()).into_iter().collect(),
        user,
        wsl: crate::network::distros(),
    }
}

/// Where Windows keeps known folder `id` (moved to OneDrive or another disk included).
#[cfg(windows)]
fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;
    let mut raw: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: Windows writes a string it allocated to `raw`, freed below whatever the outcome.
    let ok = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut raw) } == 0;
    // SAFETY: a NUL-terminated string from Windows, freed only below.
    let path = (ok && !raw.is_null()).then(|| PathBuf::from(std::ffi::OsString::from_wide(unsafe { crate::network::wide(raw) })));
    // SAFETY: freeing what SHGetKnownFolderPath allocated (null is allowed).
    unsafe { CoTaskMemFree(raw as _) };
    path
}

/// The name Explorer shows for `path` ("Documentos" for Documents).
#[cfg(windows)]
fn display_name(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::{SHFILEINFOW, SHGFI_DISPLAYNAME, SHGetFileInfoW};
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: zeroed is a valid SHFILEINFOW, filled in by the call.
    let mut info: SHFILEINFOW = unsafe { std::mem::zeroed() };
    // SAFETY: `wide` is NUL-terminated and `info` is as big as said.
    let ok = unsafe { SHGetFileInfoW(wide.as_ptr(), 0, &mut info, std::mem::size_of::<SHFILEINFOW>() as u32, SHGFI_DISPLAYNAME) } != 0;
    let len = info.szDisplayName.iter().position(|&c| c == 0).unwrap_or(info.szDisplayName.len());
    (ok && len > 0).then(|| String::from_utf16_lossy(&info.szDisplayName[..len]))
}

/// Elsewhere: the usual folders under the home folder, by their own names.
#[cfg(not(windows))]
pub fn places() -> Places {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return Places::default() };
    let named = |p: PathBuf| {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        (p, name)
    };
    let user = ["Documents", "Downloads", "Pictures", "Music", "Videos"].iter().map(|n| home.join(n)).chain([home.clone()]).filter(|p| p.is_dir()).map(named).collect();
    Places { desktop: Some(home.join("Desktop")).filter(|p| p.is_dir()).map(named), desktop_also: Vec::new(), user, wsl: Vec::new() }
}
