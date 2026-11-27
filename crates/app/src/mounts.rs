//! The disks Este Computador lists, and the news that one was mounted or removed while the app
//! runs (a USB stick, a mapped network drive, an `rclone mount`), so it shows at once.
//!
//! Each system tells by itself, with no polling: Windows sends `WM_DEVICECHANGE` to its
//! top-level windows, Linux marks `/proc/self/mountinfo` for `poll`, macOS raises a kqueue
//! `EVFILT_FS` event. A mount that doesn't tell (`subst`) shows when the window gets the focus
//! back, as the app reads the disks again then.

use std::path::{Path, PathBuf};

use crate::model::DriveKind;

/// Whether `dir` is a disk's top (`C:\`, `/`), which goes up to Este Computador; not a
/// computer on the network (`\\servidor`).
pub fn is_disk_root(dir: &Path) -> bool {
    !dir.as_os_str().is_empty() && dir.parent().is_none() && crate::network::unc(dir).is_none()
}

/// The id of disk `root` in Este Computador's listing.
pub fn disk_id(root: &Path) -> String {
    root.display().to_string()
}

/// A disk's name until the system's comes (see [`names`]): on Windows by its kind and letter
/// ("Disco local (C:)"); elsewhere a disk is a folder ("/media/ana/PEN") and shows by its name,
/// `/` by its kind.
pub fn fallback_name(root: &Path, kind: DriveKind) -> String {
    let shown = root.display().to_string();
    let letter = match shown.trim_end_matches(['\\', '/']) {
        "" => shown.as_str(),
        l => l,
    };
    match root.file_name() {
        Some(name) if !cfg!(windows) => name.to_string_lossy().into_owned(),
        _ => kind.label(letter),
    }
}

/// The disks' names as the system shows them: Explorer's ("Disco Local (C:)", "Dados (D:)",
/// "AltMount (\\server) (X:)"). Asking the shell can wait on a slow disk, so this runs off the
/// window's thread. A disk the shell names nothing keeps its [`fallback_name`].
pub fn names(drives: &[(PathBuf, DriveKind)]) -> Vec<(PathBuf, String)> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
        use windows_sys::Win32::UI::Shell::{SHFILEINFOW, SHGFI_DISPLAYNAME, SHGetFileInfoW};
        // SAFETY: COM for this thread, undone below when this call set it up.
        let init = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
        let out = drives
            .iter()
            .map(|(root, kind)| {
                let wide: Vec<u16> = root.as_os_str().encode_wide().chain([0]).collect();
                // SAFETY: an all-zero SHFILEINFOW is valid; a NUL-terminated path and its size.
                let mut info: SHFILEINFOW = unsafe { std::mem::zeroed() };
                let ok = unsafe { SHGetFileInfoW(wide.as_ptr(), 0, &mut info, std::mem::size_of::<SHFILEINFOW>() as u32, SHGFI_DISPLAYNAME) } != 0;
                let end = info.szDisplayName.iter().position(|&c| c == 0).unwrap_or(info.szDisplayName.len());
                let name = String::from_utf16_lossy(&info.szDisplayName[..end]);
                (root.clone(), if ok && !name.trim().is_empty() { name } else { fallback_name(root, *kind) })
            })
            .collect();
        if init >= 0 {
            // SAFETY: pairs the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        out
    }
    #[cfg(not(windows))]
    {
        drives.iter().map(|(root, kind)| (root.clone(), fallback_name(root, *kind))).collect()
    }
}

/// The disks to show in Este Computador, with their kind.
pub fn list() -> Vec<(PathBuf, DriveKind)> {
    #[cfg(windows)]
    {
        // A bit per letter that has a drive, without touching the drives themselves (an empty
        // DVD drive would make `exists` wait).
        let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
        (0..26u8)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| PathBuf::from(format!("{}:\\", (b'A' + i) as char)))
            .map(|d| {
                let kind = crate::model::drive_kind(&d);
                (d, kind)
            })
            .collect()
    }
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        linux_mounts(&text, std::env::var_os("HOME").map(PathBuf::from).as_deref())
    }
    #[cfg(target_os = "macos")]
    {
        mac::list()
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        vec![(PathBuf::from("/"), DriveKind::Fixed)]
    }
}

/// Calls `changed` on a thread of its own each time the system says a disk was mounted or
/// removed. It may call it for other changes too: the caller compares with what it has.
pub fn watch(changed: impl Fn() + Send + 'static) {
    let spawned = std::thread::Builder::new().name("mounts".into()).spawn(move || {
        #[cfg(windows)]
        win::watch(changed);
        #[cfg(target_os = "linux")]
        linux::watch(changed);
        #[cfg(target_os = "macos")]
        mac::watch(changed);
        #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
        drop(changed);
    });
    if let Err(e) = spawned {
        openexplorer_gdrive::log(&format!("erro vigiar unidades: {e}"));
    }
}

// ---------------------------------------------------------------------------------------------
// Linux

/// File systems that are the system's own workings, never a disk to show.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const PSEUDO: &[&str] = &[
    "proc", "sysfs", "devtmpfs", "devpts", "tmpfs", "ramfs", "cgroup", "cgroup2", "securityfs", "pstore", "debugfs", "tracefs", "configfs", "fusectl", "mqueue", "hugetlbfs",
    "bpf", "autofs", "binfmt_misc", "rpc_pipefs", "nsfs", "efivarfs", "selinuxfs", "squashfs", "overlay", "fuse.gvfsd-fuse", "fuse.portal", "fuse.snapfuse", "fuse.lxcfs",
];

/// File systems reached over the network, rclone's included.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const NETWORK: &[&str] = &["nfs", "nfs4", "cifs", "smb3", "smbfs", "9p", "ceph", "glusterfs", "davfs", "fuse.sshfs", "fuse.rclone", "fuse.davfs2", "fuse.s3fs"];

/// The disks in `mountinfo` (the text of `/proc/self/mountinfo`) a person would look for, as
/// the file managers show them: `/`, and what's mounted under `/media`, `/run/media`, `/mnt` or
/// the home folder (not inside a hidden folder there), of a file system that holds files.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_mounts(mountinfo: &str, home: Option<&std::path::Path>) -> Vec<(PathBuf, DriveKind)> {
    let mut out: Vec<(PathBuf, DriveKind)> = vec![(PathBuf::from("/"), DriveKind::Fixed)];
    for line in mountinfo.lines() {
        // id parent major:minor root mount-point options [optional…] - type source super-options
        let Some((left, right)) = line.split_once(" - ") else { continue };
        let Some(at) = left.split(' ').nth(4).map(unescape) else { continue };
        let fs = right.split(' ').next().unwrap_or("");
        if PSEUDO.contains(&fs) {
            continue;
        }
        let path = PathBuf::from(&at);
        let in_home = home.is_some_and(|h| in_home(&path, h));
        // WSL keeps its own workings under /mnt/wslg; its Windows disks (/mnt/c) do show.
        let shown = (["/media/", "/run/media/", "/mnt/"].iter().any(|p| at.starts_with(p)) && !at.starts_with("/mnt/wslg/")) || in_home;
        if !shown {
            continue;
        }
        let kind = if NETWORK.contains(&fs) {
            DriveKind::Network
        } else if matches!(fs, "iso9660" | "udf") {
            DriveKind::Optical
        } else if at.starts_with("/media/") || at.starts_with("/run/media/") {
            DriveKind::Removable
        } else {
            DriveKind::Fixed
        };
        // Mounted again over the same folder: the last one is what shows there.
        out.retain(|(p, _)| *p != path);
        out.push((path, kind));
    }
    out[1..].sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Whether `path` is below the home folder `home`, and not inside a hidden folder there.
#[cfg_attr(windows, allow(dead_code))]
fn in_home(path: &Path, home: &Path) -> bool {
    path.strip_prefix(home).is_ok_and(|r| r.components().count() > 0 && !r.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')))
}

/// A mount point as mountinfo writes it, with `\040` for a space and the like.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && let Ok(n) = u8::from_str_radix(&field[i + 1..i + 4], 8)
        {
            out.push(n);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::os::fd::AsRawFd;

    /// The kernel marks an open `/proc/self/mountinfo` with `POLLPRI` each time a file system
    /// is mounted or unmounted (proc(5)); `poll` itself takes the mark off.
    pub fn watch(changed: impl Fn()) {
        let Ok(file) = std::fs::File::open("/proc/self/mountinfo") else { return };
        let mut fd = libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLPRI, revents: 0 };
        loop {
            // SAFETY: one valid pollfd, the file open for the whole loop.
            let n = unsafe { libc::poll(&mut fd, 1, -1) };
            if n < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            if fd.revents & (libc::POLLPRI | libc::POLLERR) != 0 {
                changed();
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// macOS

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::CStr;
    use std::path::PathBuf;

    use crate::model::DriveKind;

    /// `/` and the volumes Finder shows: those under `/Volumes` or the home folder that don't
    /// ask to stay hidden (`MNT_DONTBROWSE`, as the system's own volumes do).
    pub fn list() -> Vec<(PathBuf, DriveKind)> {
        let mut out = vec![(PathBuf::from("/"), DriveKind::Fixed)];
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut mounts: *mut libc::statfs = std::ptr::null_mut();
        // SAFETY: getmntinfo points `mounts` at a buffer of `n` entries it owns; MNT_NOWAIT keeps
        // it from asking a slow network server.
        let n = unsafe { libc::getmntinfo(&mut mounts, libc::MNT_NOWAIT) };
        if n <= 0 || mounts.is_null() {
            return out;
        }
        // SAFETY: `n` entries from getmntinfo, read before anything else calls it on this thread.
        let mounts = unsafe { std::slice::from_raw_parts(mounts, n as usize) };
        for m in mounts {
            // SAFETY: both are NUL-terminated within their arrays.
            let at = unsafe { CStr::from_ptr(m.f_mntonname.as_ptr()) }.to_string_lossy().into_owned();
            let fs = unsafe { CStr::from_ptr(m.f_fstypename.as_ptr()) }.to_string_lossy().into_owned();
            let flags = m.f_flags as i32;
            let path = PathBuf::from(&at);
            let in_home = home.as_ref().is_some_and(|h| super::in_home(&path, h));
            if flags & libc::MNT_DONTBROWSE != 0 || !(at.starts_with("/Volumes/") || in_home) {
                continue;
            }
            let kind = if flags & libc::MNT_LOCAL == 0 {
                DriveKind::Network
            } else if matches!(fs.as_str(), "cd9660" | "udf") {
                DriveKind::Optical
            } else {
                DriveKind::Fixed
            };
            out.retain(|(p, _)| *p != path);
            out.push((path, kind));
        }
        out[1..].sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// kqueue's `EVFILT_FS` fires on every mount and unmount (and on a disk running low, which
    /// the caller sees changed nothing).
    pub fn watch(changed: impl Fn()) {
        // SAFETY: a new kqueue, closed only with the process.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return;
        }
        // SAFETY: an all-zero kevent is valid; the fields set below are the ones EVFILT_FS reads.
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.filter = libc::EVFILT_FS;
        ev.flags = libc::EV_ADD | libc::EV_CLEAR;
        // SAFETY: one change in, no events out.
        if unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) } < 0 {
            return;
        }
        loop {
            // SAFETY: an all-zero kevent is valid; kevent fills it.
            let mut got: libc::kevent = unsafe { std::mem::zeroed() };
            // SAFETY: no changes in, room for one event out, no timeout.
            let n = unsafe { libc::kevent(kq, std::ptr::null(), 0, &mut got, 1, std::ptr::null()) };
            if n < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            if n > 0 {
                changed();
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Windows

#[cfg(windows)]
mod win {
    use std::cell::RefCell;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MSG, RegisterClassW, TranslateMessage, WM_DEVICECHANGE, WNDCLASSW};

    /// A device or volume arrived, and one was removed (dbt.h).
    const DBT_DEVICEARRIVAL: usize = 0x8000;
    const DBT_DEVICEREMOVECOMPLETE: usize = 0x8004;

    thread_local! {
        static CHANGED: RefCell<Option<Box<dyn Fn()>>> = RefCell::new(None);
    }

    /// Windows sends `WM_DEVICECHANGE` to every top-level window, hidden ones too, when a volume
    /// arrives or leaves: a USB disk, a mapped network drive (`net use`, Explorer's Map), a
    /// WinFsp mount (`rclone mount`). A message-only window would get none of them, so this is a
    /// plain window never shown.
    pub fn watch(changed: impl Fn() + 'static) {
        CHANGED.with(|c| *c.borrow_mut() = Some(Box::new(changed)));
        let class: Vec<u16> = "OpenExplorerMounts\0".encode_utf16().collect();
        // SAFETY: a class with a valid procedure and a NUL-terminated name that outlives it; a
        // window of it never shown; the usual message loop on the thread that made it.
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let wc = WNDCLASSW { lpfnWndProc: Some(procedure), hInstance: instance, lpszClassName: class.as_ptr(), ..std::mem::zeroed() };
            if RegisterClassW(&wc) == 0 {
                return;
            }
            let hwnd = CreateWindowExW(0, class.as_ptr(), class.as_ptr(), 0, 0, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null());
            if hwnd.is_null() {
                return;
            }
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    unsafe extern "system" fn procedure(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if message == WM_DEVICECHANGE {
            if matches!(wparam, DBT_DEVICEARRIVAL | DBT_DEVICEREMOVECOMPLETE) {
                CHANGED.with(|c| {
                    if let Some(f) = c.borrow().as_ref() {
                        f();
                    }
                });
            }
            return 1;
        }
        // SAFETY: forwarding the message this window received.
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// From a Linux mount table only `/`, removable disks, network and rclone mounts, and the
    /// home folder's mounts show; the system's own and hidden folders' don't.
    #[test]
    fn linux_shows_the_disks_a_person_looks_for() {
        let info = "\
22 1 0:21 / /proc rw,nosuid shared:5 - proc proc rw
25 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw
26 25 8:1 / /boot/efi rw shared:2 - vfat /dev/sda1 rw
30 25 0:25 / /run rw shared:6 - tmpfs tmpfs rw
40 30 8:17 / /run/media/ana/PEN\\040DRIVE rw shared:9 - vfat /dev/sdb1 rw
41 25 0:50 / /home/ana/gdrive rw shared:10 - fuse.rclone gdrive: rw
42 25 0:51 / /home/ana/.cache/doc rw shared:11 - fuse.portal portal rw
43 25 0:52 / /home/ana/.local/x rw shared:12 - fuse.sshfs h:/ rw
44 25 0:53 / /mnt/nas rw shared:13 - cifs //nas/dados rw
45 25 7:0 / /snap/core/1 ro shared:14 - squashfs /dev/loop0 ro
46 25 11:0 / /media/cdrom rw shared:15 - iso9660 /dev/sr0 ro
47 25 8:48 / /mnt/wslg/distro rw shared:16 - ext4 /dev/sdd rw
";
        let got = linux_mounts(info, Some(Path::new("/home/ana")));
        let want = [
            ("/", DriveKind::Fixed),
            ("/home/ana/gdrive", DriveKind::Network),
            ("/media/cdrom", DriveKind::Optical),
            ("/mnt/nas", DriveKind::Network),
            ("/run/media/ana/PEN DRIVE", DriveKind::Removable),
        ];
        assert_eq!(got, want.map(|(p, k)| (PathBuf::from(p), k)));
    }

    /// The disks' names are Explorer's. Ignored: it reads this computer's disks (here C: and
    /// the rclone mount on X:).
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn the_disks_are_named_as_explorer_names_them() {
        let names = names(&list());
        println!("{names:?}");
        let of = |l: &str| names.iter().find(|(p, _)| p.as_os_str() == format!("{l}:\\").as_str()).map(|(_, n)| n.clone());
        assert_eq!(of("C").as_deref(), Some("Disco Local (C:)"));
        if let Some(x) = of("X") {
            assert_eq!(x, r"AltMount (\\server) (X:)");
        }
    }

    /// An `rclone mount` on a free letter, as a local disk and as a network drive, reaches the
    /// watcher (WinFsp tells Windows' top-level windows) and is in the next reading; its
    /// unmount too. Ignored: it mounts drives with the rclone on PATH, which needs WinFsp.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn an_rclone_mount_is_told_and_read() {
        use std::process::Command;
        use std::sync::mpsc::channel;
        use std::time::Duration;
        let (tx, rx) = channel();
        // Read the instant the news comes, the names too, as the app does.
        watch(move || {
            let drives = list();
            let named = names(&drives);
            let _ = tx.send((drives, named));
        });
        std::thread::sleep(Duration::from_millis(300));
        let addr = "127.0.0.1:55721";
        let mut rcd = Command::new("rclone").args(["rcd", "--rc-no-auth", "--rc-addr", addr]).spawn().expect("rclone");
        std::thread::sleep(Duration::from_secs(2));
        let rc = |args: &[&str]| Command::new("rclone").args(["rc", "--url", &format!("http://{addr}/")]).args(args).status().is_ok_and(|s| s.success());
        let dir = std::env::temp_dir().join("openexplorer-rclone-mount-test");
        std::fs::create_dir_all(&dir).unwrap();
        let mut failed = Vec::new();
        for network in [false, true] {
            let before = list();
            let letter = (b'G'..=b'Z').rev().map(|c| c as char).find(|c| !before.iter().any(|(d, _)| d.to_string_lossy().starts_with(*c))).expect("no free letter");
            let root = PathBuf::from(format!("{letter}:\\"));
            while rx.try_recv().is_ok() {}
            let opt = format!("mountOpt={{\"NetworkMode\": {network}, \"VolumeName\": \"OeTeste\"}}");
            assert!(rc(&["mount/mount", &format!("fs={}", dir.display()), &format!("mountPoint={letter}:"), &opt]), "rclone didn't mount {letter}:");
            // Some reading told has the drive: the first may come before it's there.
            let (mut kind, mut named) = (None, None);
            while kind.is_none() {
                let Ok((drives, names_then)) = rx.recv_timeout(Duration::from_secs(20)) else { break };
                kind = drives.iter().find(|(d, _)| *d == root).map(|(_, k)| *k);
                named = names_then.into_iter().find(|(d, _)| *d == root).map(|(_, n)| n);
            }
            std::thread::sleep(Duration::from_secs(3));
            let named_later = names(&list()).into_iter().find(|(d, _)| *d == root).map(|(_, n)| n);
            println!("network={network} {letter}: named at the news {named:?}, 3 s later {named_later:?}");
            if named != named_later {
                failed.push(network);
            }
            while rx.try_recv().is_ok() {}
            assert!(rc(&["mount/unmount", &format!("mountPoint={letter}:")]), "rclone didn't unmount {letter}:");
            let mut gone = false;
            let mut tellings = 0;
            while !gone {
                let Ok((drives, _)) = rx.recv_timeout(Duration::from_secs(20)) else { break };
                tellings += 1;
                gone = !drives.iter().any(|(d, _)| *d == root);
            }
            println!("network={network} {letter}: kind={kind:?} gone={gone} after {tellings} tellings");
            if !(kind.is_some() && gone) {
                failed.push(network);
            }
        }
        let _ = rc(&["core/quit"]);
        std::thread::sleep(Duration::from_secs(1));
        let _ = rcd.kill();
        let _ = rcd.wait();
        assert!(failed.is_empty(), "failed with network mode {failed:?}");
    }
}
