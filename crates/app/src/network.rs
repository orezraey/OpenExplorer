//! Places Windows reaches by path off this computer's disks: the shared folders of computers on
//! the network (`\\servidor\pasta`) and WSL's Linux distributions (`\\wsl.localhost\Ubuntu`).
//! Below a shared folder everything lists, copies and opens as on a disk. This module knows the
//! levels above one, which have no folder to read: a computer (`\\servidor`, which lists its
//! shared folders), the distributions, and the computers on the network.

use std::path::{Component, Path, PathBuf, Prefix};

/// The computer WSL's distributions show under: `\\wsl.localhost\Ubuntu`.
pub const WSL: &str = "wsl.localhost";

/// The computer and the shared folder of a `\\servidor\pasta\…` path; the shared folder is ""
/// for the computer's own path, `\\servidor`.
pub fn unc(path: &Path) -> Option<(String, String)> {
    if let Some(Component::Prefix(prefix)) = path.components().next() {
        return match prefix.kind() {
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => Some((server.to_string_lossy().into_owned(), share.to_string_lossy().into_owned())),
            _ => None,
        };
    }
    // A computer alone, which `Path` doesn't read as one: to it `\\nas` is the folder `nas` at
    // the top of the current disk.
    let name = path.to_str()?.strip_prefix(r"\\")?.trim_end_matches('\\');
    (!name.is_empty() && !name.contains(['\\', '/', '?']) && name != ".").then(|| (name.to_string(), String::new()))
}

/// `\\servidor`.
pub fn server_path(server: &str) -> PathBuf {
    PathBuf::from(format!(r"\\{server}"))
}

/// `\\servidor\pasta`.
pub fn share_path(server: &str, share: &str) -> PathBuf {
    PathBuf::from(format!(r"\\{server}\{share}"))
}

/// Whether `path` is a computer, `\\servidor`: it lists its shared folders, not a folder.
pub fn is_server(path: &Path) -> bool {
    unc(path).is_some_and(|(_, share)| share.is_empty())
}

/// Whether `path` is a shared folder itself, or a distribution (`\\nas\Fotos`,
/// `\\wsl.localhost\Ubuntu`): what's in it can change, not it.
pub fn is_share_root(path: &Path) -> bool {
    unc(path).is_some_and(|(_, share)| !share.is_empty()) && path.parent().is_none()
}

pub fn is_wsl(server: &str) -> bool {
    server.eq_ignore_ascii_case(WSL)
}

/// Whether reading `dir` may wait on the network (a shared folder, a distribution, a network
/// drive), so it's read off the window's thread; and whether it has no Lixeira to send to.
pub fn is_remote(dir: &Path) -> bool {
    unc(dir).is_some() || crate::model::drive_kind(dir) == crate::model::DriveKind::Network
}

/// The folder above `dir`. A shared folder's is its computer, which [`Path::parent`] doesn't
/// know (`\\servidor\pasta` has no parent to it).
pub fn parent(dir: &Path) -> Option<PathBuf> {
    match (unc(dir), dir.parent()) {
        // To `Path`, `\\nas`'s parent is `\`.
        (Some((_, share)), _) if share.is_empty() => None,
        (_, Some(up)) => Some(up.to_path_buf()),
        (Some((server, _)), None) => Some(server_path(&server)),
        (None, None) => None,
    }
}

/// The name of `dir`'s last step, a shared folder's or a computer's included (where
/// [`Path::file_name`] has none).
pub fn name(dir: &Path) -> Option<String> {
    match unc(dir) {
        Some((server, share)) if share.is_empty() => Some(server),
        Some((_, share)) if dir.parent().is_none() => Some(share),
        _ => dir.file_name().map(|n| n.to_string_lossy().into_owned()),
    }
}

/// `path` as the app spells a network path: `\\servidor\pasta` for `\\?\UNC\servidor\pasta`,
/// and WSL's old computer name, `\\wsl$`, as its current one, so each leads to one place in the
/// tree and the path bar.
pub fn tidy(path: PathBuf) -> PathBuf {
    let path = match path.to_str().and_then(|p| p.strip_prefix(r"\\?\UNC\")) {
        Some(rest) => PathBuf::from(format!(r"\\{rest}")),
        None => path,
    };
    match unc(&path) {
        Some((server, share)) if server.eq_ignore_ascii_case("wsl$") => {
            if share.is_empty() {
                return server_path(WSL);
            }
            let rest = path.strip_prefix(share_path(&server, &share)).map(Path::to_path_buf).unwrap_or_default();
            let top = share_path(WSL, &share);
            if rest.as_os_str().is_empty() { top } else { top.join(rest) }
        }
        _ => path,
    }
}

/// What a computer offers: its shared folders, or for WSL's the distributions.
pub fn shares(server: &str) -> std::io::Result<Vec<String>> {
    if is_wsl(server) {
        return Ok(distros());
    }
    shared_folders(server)
}

/// The computer's shared folders of files: not printers nor the hidden ones (`C$`, `ADMIN$`).
#[cfg(windows)]
fn shared_folders(server: &str) -> std::io::Result<Vec<String>> {
    use windows_sys::Win32::NetworkManagement::NetManagement::{MAX_PREFERRED_LENGTH, NetApiBufferFree};
    use windows_sys::Win32::Storage::FileSystem::{NetShareEnum, SHARE_INFO_1, STYPE_DISKTREE, STYPE_MASK, STYPE_SPECIAL};
    let wide: Vec<u16> = format!(r"\\{server}").encode_utf16().chain([0]).collect();
    let (mut buf, mut read, mut total) = (std::ptr::null_mut::<u8>(), 0u32, 0u32);
    // SAFETY: a NUL-terminated name and valid outs; Windows allocates `buf`, freed below.
    let code = unsafe { NetShareEnum(wide.as_ptr(), 1, &mut buf, MAX_PREFERRED_LENGTH, &mut read, &mut total, std::ptr::null_mut()) };
    let mut out = Vec::new();
    if code == 0 && !buf.is_null() {
        // SAFETY: `read` SHARE_INFO_1 records at `buf`, as the call says.
        let items = unsafe { std::slice::from_raw_parts(buf as *const SHARE_INFO_1, read as usize) };
        for item in items {
            if item.shi1_type & STYPE_MASK == STYPE_DISKTREE && item.shi1_type & STYPE_SPECIAL == 0 {
                // SAFETY: a NUL-terminated string in the same buffer.
                out.push(unsafe { wide_str(item.shi1_netname) });
            }
        }
    }
    // SAFETY: what NetShareEnum allocated (null is allowed).
    unsafe { NetApiBufferFree(buf as _) };
    if code != 0 {
        // The network's own codes (NERR_*, 2100–2999) have no text among Windows' messages.
        return Err(match code {
            2100..=2999 => std::io::Error::other(format!("o computador não entregou a lista de pastas compartilhadas (erro de rede {code})")),
            _ => std::io::Error::from_raw_os_error(code as i32),
        });
    }
    Ok(out)
}

#[cfg(not(windows))]
fn shared_folders(_server: &str) -> std::io::Result<Vec<String>> {
    Ok(Vec::new())
}

/// The characters of a NUL-terminated wide string Windows handed over (none for null).
///
/// # Safety
/// `p` is null or points at a NUL-terminated UTF-16 string that outlives the slice.
#[cfg(windows)]
pub(crate) unsafe fn wide<'a>(p: *const u16) -> &'a [u16] {
    if p.is_null() {
        return &[];
    }
    // SAFETY: as the caller promises.
    let len = unsafe { (0..).take_while(|&i| *p.add(i) != 0).count() };
    // SAFETY: `len` characters were just read.
    unsafe { std::slice::from_raw_parts(p, len) }
}

/// # Safety
/// As [`wide`].
#[cfg(windows)]
unsafe fn wide_str(p: *const u16) -> String {
    // SAFETY: as the caller promises.
    String::from_utf16_lossy(unsafe { wide(p) })
}

/// WSL's distributions, by name, from the registry: reading it doesn't start them (opening
/// `\\wsl.localhost\Ubuntu` does).
#[cfg(windows)]
pub fn distros() -> Vec<String> {
    use windows_sys::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, KEY_READ, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW};
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let mut key: HKEY = std::ptr::null_mut();
    let path = wide(r"Software\Microsoft\Windows\CurrentVersion\Lxss");
    // SAFETY: a NUL-terminated name and a valid out; the key is closed below.
    if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_READ, &mut key) } != 0 {
        return Vec::new();
    }
    let value = wide("DistributionName");
    let mut out = Vec::new();
    for i in 0.. {
        let mut sub = [0u16; 256];
        let mut sub_len = sub.len() as u32;
        // SAFETY: `sub` holds `sub_len` characters; the other outs may be null.
        let code = unsafe { RegEnumKeyExW(key, i, sub.as_mut_ptr(), &mut sub_len, std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
        if code != 0 {
            break;
        }
        let mut name = [0u16; 256];
        let mut size = std::mem::size_of_val(&name) as u32;
        // SAFETY: `sub` is NUL-terminated by the call, `name` holds `size` bytes.
        let ok = unsafe { RegGetValueW(key, sub.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), name.as_mut_ptr() as _, &mut size) } == 0;
        if ok {
            let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            if len > 0 {
                out.push(String::from_utf16_lossy(&name[..len]));
            }
        }
    }
    // SAFETY: the key opened above.
    unsafe { RegCloseKey(key) };
    out.sort_by_key(|n| n.to_lowercase());
    out
}

#[cfg(not(windows))]
pub fn distros() -> Vec<String> {
    Vec::new()
}

/// The computers of the network drives (`Z:` → `\\nas`), known without asking the network.
#[cfg(windows)]
pub fn mapped_servers(drives: &[(PathBuf, crate::model::DriveKind)]) -> Vec<String> {
    use windows_sys::Win32::NetworkManagement::WNet::WNetGetConnectionW;
    let mut out: Vec<String> = Vec::new();
    for (d, kind) in drives {
        if *kind != crate::model::DriveKind::Network {
            continue;
        }
        let letter: Vec<u16> = d.to_string_lossy().trim_end_matches('\\').encode_utf16().chain([0]).collect();
        let mut remote = [0u16; 1024];
        let mut len = remote.len() as u32;
        // SAFETY: a NUL-terminated name, and `remote` holds `len` characters.
        if unsafe { WNetGetConnectionW(letter.as_ptr(), remote.as_mut_ptr(), &mut len) } != 0 {
            continue;
        }
        let end = remote.iter().position(|&c| c == 0).unwrap_or(remote.len());
        if let Some((server, _)) = unc(Path::new(&String::from_utf16_lossy(&remote[..end])))
            && !out.iter().any(|s| s.eq_ignore_ascii_case(&server))
        {
            out.push(server);
        }
    }
    out
}

#[cfg(not(windows))]
pub fn mapped_servers(_drives: &[(PathBuf, crate::model::DriveKind)]) -> Vec<String> {
    Vec::new()
}

/// The computers Explorer's Rede finds on the network, by name. It can take half a minute
/// (Windows asks the network each way it knows), and finds none where network discovery is off.
#[cfg(windows)]
pub fn computers() -> Vec<String> {
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize};
    use windows::Win32::UI::Shell::{BHID_EnumItems, IEnumShellItems, IShellItem, SHCreateItemFromParsingName, SIGDN_DESKTOPABSOLUTEPARSING};
    use windows::core::w;
    // SAFETY: COM for this thread, undone below when this call set it up.
    let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let mut out: Vec<String> = Vec::new();
    // SAFETY: plain shell calls; each string Windows hands over is freed after it's read.
    unsafe {
        // Explorer's Rede.
        let found: windows::core::Result<()> = (|| {
            let network: IShellItem = SHCreateItemFromParsingName(w!("::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}"), None)?;
            let items: IEnumShellItems = network.BindToHandler(None, &BHID_EnumItems)?;
            loop {
                let mut one = [None];
                let mut got = 0u32;
                items.Next(&mut one, Some(&mut got))?;
                let Some(item) = one[0].take().filter(|_| got == 1) else { break };
                let Ok(raw) = item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING) else { continue };
                let parsed = wide_str(raw.0);
                CoTaskMemFree(Some(raw.0 as _));
                // Computers only: TVs and printers have no path.
                if let Some((server, share)) = unc(Path::new(&parsed))
                    && share.is_empty()
                    && !out.iter().any(|s| s.eq_ignore_ascii_case(&server))
                {
                    out.push(server);
                }
            }
            Ok(())
        })();
        let _ = found;
        if init.is_ok() {
            CoUninitialize();
        }
    }
    out
}

#[cfg(not(windows))]
pub fn computers() -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A computer, its shared folders and what's below them, with the steps `Path` doesn't take.
    #[test]
    fn the_levels_above_a_shared_folder() {
        let server = Path::new(r"\\nas");
        let share = Path::new(r"\\nas\Fotos");
        let deep = Path::new(r"\\nas\Fotos\2024");
        assert_eq!(unc(server), Some(("nas".into(), String::new())));
        assert_eq!(unc(deep), Some(("nas".into(), "Fotos".into())));
        assert_eq!(unc(Path::new(r"C:\Fotos")), None);
        assert!(is_server(server) && !is_server(share) && !is_server(Path::new(r"C:\")));
        assert_eq!(parent(deep).as_deref(), Some(share));
        assert_eq!(parent(share).as_deref(), Some(server));
        assert_eq!(parent(server), None);
        assert_eq!([name(deep), name(share), name(server)], [Some("2024".into()), Some("Fotos".into()), Some("nas".into())]);
        // What `open` and the tree build by joining a listed name.
        assert_eq!(server.join("Fotos"), share);
        assert_eq!(share.join("2024"), deep);
        assert_eq!(Path::new(r"\\nas\Fotos\"), share);
        assert_eq!(deep.strip_prefix(share).unwrap(), Path::new("2024"));
    }

    #[test]
    fn wsl_by_its_old_name_is_the_same_place() {
        assert_eq!(tidy(PathBuf::from(r"\\wsl$\Ubuntu\home")), Path::new(r"\\wsl.localhost\Ubuntu\home"));
        assert_eq!(tidy(PathBuf::from(r"\\WSL$\Ubuntu")), Path::new(r"\\wsl.localhost\Ubuntu"));
        assert_eq!(tidy(PathBuf::from(r"\\wsl$")), Path::new(r"\\wsl.localhost"));
        assert_eq!(tidy(PathBuf::from(r"\\nas\x")), Path::new(r"\\nas\x"));
        assert_eq!(tidy(PathBuf::from(r"\\?\UNC\nas\Fotos\2024")).to_str(), Some(r"\\nas\Fotos\2024"));
        assert_eq!(tidy(PathBuf::from(r"\\?\UNC\wsl$\Ubuntu")).to_str(), Some(r"\\wsl.localhost\Ubuntu"));
    }

    /// Typed in the address bar, a computer alone and WSL's old name lead to the network; the
    /// path bar steps through the computer and the shared folder; a computer's listing takes
    /// no new items.
    #[test]
    fn the_address_and_path_bars_reach_the_network() {
        use crate::address::{Target, parse};
        use crate::model::{Location, crumbs};
        let here = Location::Local(PathBuf::from(r"C:\Users"));
        assert_eq!(parse(r"\\nas", &here, &[]), Ok(Target::Local(PathBuf::from(r"\\nas"))));
        assert_eq!(parse(r"\\nas\", &here, &[]), Ok(Target::Local(PathBuf::from(r"\\nas"))));
        assert_eq!(parse(r"\\wsl$\Ubuntu\home", &here, &[]), Ok(Target::Local(PathBuf::from(r"\\wsl.localhost\Ubuntu\home"))));
        let steps = |p: &str| crumbs(&Location::Local(PathBuf::from(p))).into_iter().map(|c| (c.label, c.to)).collect::<Vec<_>>();
        let at = |p: &str| Location::Local(PathBuf::from(p));
        assert_eq!(steps(r"\\nas\Fotos\2024"), [(r"\\nas".to_string(), at(r"\\nas")), ("Fotos".into(), at(r"\\nas\Fotos")), ("2024".into(), at(r"\\nas\Fotos\2024"))]);
        assert_eq!(steps(r"\\wsl.localhost\Ubuntu"), [("Linux".to_string(), at(r"\\wsl.localhost")), ("Ubuntu".into(), at(r"\\wsl.localhost\Ubuntu"))]);
        assert!(!crate::actions::editable(&at(r"\\nas")) && !crate::actions::items_editable(&at(r"\\nas")));
        assert!(crate::actions::editable(&at(r"\\nas\Fotos")));
    }

    /// Deleting for good takes files, folders with what's in them, and a link without what it
    /// leads to; it says how far it got when one fails.
    #[test]
    fn deleting_for_good() {
        let root = std::env::temp_dir().join("openexplorer-network-test-delete");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("pasta/sub")).unwrap();
        std::fs::write(root.join("pasta/sub/a.txt"), "a").unwrap();
        std::fs::write(root.join("b.txt"), "b").unwrap();
        crate::ops::delete_forever(&[root.join("pasta"), root.join("b.txt")]).unwrap();
        assert!(!root.join("pasta").exists() && !root.join("b.txt").exists());
        std::fs::write(root.join("c.txt"), "c").unwrap();
        let err = crate::ops::delete_forever(&[root.join("c.txt"), root.join("não existe")]).unwrap_err();
        assert!(!root.join("c.txt").exists());
        assert!(err.contains("não existe") && err.contains("já excluídos para sempre: 1"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Only reads: this computer's own shared folders and the distributions, where there are any.
    #[test]
    #[ignore = "reads this computer's shares and WSL"]
    fn lists_this_computers_shares_and_distros() {
        println!("shares of localhost: {:?}", shares("localhost"));
        println!("distros: {:?}", distros());
        println!("mapped: {:?}", mapped_servers(&crate::model::local_drives()));
        let started = std::time::Instant::now();
        println!("computers: {:?} in {:?}", computers(), started.elapsed());
    }
}
