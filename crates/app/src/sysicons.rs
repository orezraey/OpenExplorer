//! The icons Windows Explorer shows: a file type's icon comes from the program set to open it
//! (VLC's for `.mkv` when VLC plays them), plus the folder, drive and home folder icons. They're
//! looked up on a worker thread through `SHGetFileInfoW`, kept one per type, and drawn as
//! images; until one arrives, or where Windows has none (Google Docs), the app's own icon shows.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};

use rune::{Image, ImageCache};

use crate::Msg;
use crate::model::{Entry, Location};

/// What an icon is looked up by.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum IconKey {
    /// Any file with this extension, lowercased ("" for none).
    Ext(String),
    Folder,
    /// A drive, a special folder or a file whose icon is its own (`.exe`, `.lnk`, `.ico`).
    Path(PathBuf),
}

/// Extensions whose files each carry their own icon, so they're looked up by path.
const OWN_ICON: &[&str] = &["exe", "lnk", "ico", "url", "msc", "cpl", "scr"];

/// The key for an entry of `loc`, or `None` for Google's own formats, which have no Windows icon.
pub fn key_for(e: &Entry, loc: &Location) -> Option<IconKey> {
    if e.is_dir {
        return Some(IconKey::Folder);
    }
    if e.mime.as_deref().is_some_and(|m| m.starts_with("application/vnd.google-apps.")) {
        return None;
    }
    let ext = e.ext().unwrap_or_default();
    if let Location::Local(dir) = loc
        && OWN_ICON.contains(&ext.as_str())
    {
        return Some(IconKey::Path(dir.join(&e.name)));
    }
    Some(IconKey::Ext(ext))
}

/// Straight-alpha RGBA pixels of an icon.
pub type Rgba = (u32, u32, Vec<u8>);

#[derive(Default)]
pub struct SysIcons {
    images: Option<ImageCache>,
    /// `None` while the lookup runs or when Windows had no icon.
    known: HashMap<IconKey, Option<Image>>,
    /// Lookups by type (quick, from the registry) and by path (a drive or a file, which can
    /// stall on an empty DVD drive or an offline share), on separate threads so a slow path
    /// never holds up the types.
    by_type: Option<Sender<IconKey>>,
    by_path: Option<Sender<IconKey>>,
    /// Drives and the home folder, which the tree always shows.
    fixed: HashSet<IconKey>,
    /// Icons handed to the image cache and still decoding.
    decoding: Vec<IconKey>,
}

/// A lookup thread; icons found land in `poll` as [`Msg::Icon`].
fn worker(outbox: Sender<Msg>, wake: rune::Waker) -> Sender<IconKey> {
    let (tx, rx) = channel::<IconKey>();
    std::thread::spawn(move || {
        #[cfg(windows)]
        // SAFETY: plain COM initialisation for this thread, which the shell's icon lookup needs.
        unsafe {
            windows_sys::Win32::System::Com::CoInitializeEx(std::ptr::null(), windows_sys::Win32::System::Com::COINIT_APARTMENTTHREADED as u32);
        }
        for key in rx {
            let rgba = lookup(&key);
            if outbox.send(Msg::Icon { key, rgba }).is_err() {
                break;
            }
            wake();
        }
    });
    tx
}

impl SysIcons {
    /// Starts the lookup threads and asks for the icons the tree always shows.
    pub fn start(&mut self, images: ImageCache, outbox: Sender<Msg>, wake: rune::Waker, fixed: impl IntoIterator<Item = IconKey>) {
        self.by_type = Some(worker(outbox.clone(), wake.clone()));
        self.by_path = Some(worker(outbox, wake));
        self.images = Some(images);
        self.fixed = fixed.into_iter().collect();
        let fixed: Vec<IconKey> = self.fixed.iter().cloned().collect();
        self.want(fixed.into_iter().chain([IconKey::Folder]));
    }

    /// Asks for the icons of `keys` not seen yet.
    fn want(&mut self, keys: impl IntoIterator<Item = IconKey>) {
        for k in keys {
            if self.known.contains_key(&k) {
                continue;
            }
            let worker = if matches!(k, IconKey::Path(_)) { &self.by_path } else { &self.by_type };
            let Some(worker) = worker else { return };
            let _ = worker.send(k.clone());
            self.known.insert(k, None);
        }
    }

    /// Asks for what the panes now list, and lets go of the icons of single files (programs,
    /// shortcuts) they no longer show, so browsing System32 doesn't keep thousands of them.
    pub fn show(&mut self, keys: Vec<IconKey>) {
        let listed: HashSet<&IconKey> = keys.iter().collect();
        let fixed = &self.fixed;
        self.known.retain(|k, _| !matches!(k, IconKey::Path(_)) || fixed.contains(k) || listed.contains(k));
        self.want(keys);
    }

    pub fn arrived(&mut self, key: IconKey, rgba: Option<Rgba>) {
        // One let go of while it was being looked up stays gone.
        let (Some(slot), Some(images)) = (self.known.get_mut(&key), &self.images) else { return };
        *slot = rgba.map(|(w, h, px)| images.rgba(w, h, px));
        if slot.is_some() {
            self.decoding.push(key);
        }
    }

    /// Whether an icon finished decoding since the last call: the view then draws it in place
    /// of the app's own (decoding wakes the app, but nothing else would ask for a new view).
    pub fn settle(&mut self) -> bool {
        let before = self.decoding.len();
        let known = &self.known;
        self.decoding.retain(|k| matches!(known.get(k), Some(Some(image)) if !image.is_ready()));
        self.decoding.len() != before
    }

    /// The icon for `key`, once Windows gave one.
    pub fn get(&self, key: &IconKey) -> Option<&Image> {
        self.known.get(key)?.as_ref()
    }
}

/// The shell's icon for `key`, from its 48 px list so it stays sharp on high-DPI screens and in
/// Propriedades; the 32 px one if that list isn't there.
#[cfg(windows)]
fn lookup(key: &IconKey) -> Option<Rgba> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Controls::{HIMAGELIST, ImageList_GetIcon};
    use windows_sys::Win32::UI::Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW, SHGetImageList, SHIL_EXTRALARGE};
    const NORMAL: u32 = 0x80;
    const DIRECTORY: u32 = 0x10;
    // By type (no file needs to exist) for extensions and folders; by path for the rest.
    let (name, attrs, by): (PathBuf, u32, u32) = match key {
        IconKey::Ext(ext) if ext.is_empty() => ("arquivo".into(), NORMAL, SHGFI_USEFILEATTRIBUTES),
        IconKey::Ext(ext) => (format!("arquivo.{ext}").into(), NORMAL, SHGFI_USEFILEATTRIBUTES),
        IconKey::Folder => ("pasta".into(), DIRECTORY, SHGFI_USEFILEATTRIBUTES),
        IconKey::Path(p) => (p.clone(), 0, 0),
    };
    let wide: Vec<u16> = name.as_os_str().encode_wide().chain([0]).collect();
    let size = std::mem::size_of::<SHFILEINFOW>() as u32;
    let mut info: SHFILEINFOW = unsafe { std::mem::zeroed() };
    // SAFETY: a NUL-terminated path and an out struct of the size given.
    if unsafe { SHGetFileInfoW(wide.as_ptr(), attrs, &mut info, size, SHGFI_SYSICONINDEX | by) } != 0 {
        // The 48 px system list, kept for the thread's life (it's the shell's own, shared).
        thread_local!(static LIST: HIMAGELIST = {
            // IID_IImageList; the interface pointer works as an HIMAGELIST.
            let iid = windows_sys::core::GUID::from_u128(0x46eb5926_582e_4017_9fdf_e8998daa0950);
            let mut list = std::ptr::null_mut();
            // SAFETY: a GUID and an out pointer.
            let hr = unsafe { SHGetImageList(SHIL_EXTRALARGE as i32, &iid, &mut list) };
            if hr < 0 { 0 } else { list as HIMAGELIST }
        });
        let list = LIST.with(|l| *l);
        if list != 0 {
            // SAFETY: the system list and an index it gave; ILD_TRANSPARENT.
            let icon = unsafe { ImageList_GetIcon(list, info.iIcon, 1) };
            if !icon.is_null()
                && let Some(px) = hicon_rgba(icon)
            {
                return Some(trim_small(px));
            }
        }
    }
    let mut info: SHFILEINFOW = unsafe { std::mem::zeroed() };
    // SAFETY: as above; SHGFI_ICON hands over an icon this function destroys.
    let ok = unsafe { SHGetFileInfoW(wide.as_ptr(), attrs, &mut info, size, SHGFI_ICON | by) };
    if ok == 0 || info.hIcon.is_null() {
        return None;
    }
    hicon_rgba(info.hIcon)
}

/// An icon that only has a 32 px image comes out of the 48 px list drawn in its top-left
/// corner; it's cut to that corner so it centres like the others.
#[cfg(windows)]
fn trim_small((w, h, px): Rgba) -> Rgba {
    if w != 48 || h != 48 {
        return (w, h, px);
    }
    let outside = (0..48usize).flat_map(|y| (0..48usize).map(move |x| (x, y))).filter(|&(x, y)| x >= 32 || y >= 32);
    if outside.into_iter().any(|(x, y)| px[(y * 48 + x) * 4 + 3] != 0) {
        return (w, h, px);
    }
    let mut cut = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32 {
        cut.extend_from_slice(&px[y * 48 * 4..(y * 48 + 32) * 4]);
    }
    (32, 32, cut)
}

#[cfg(not(windows))]
fn lookup(_: &IconKey) -> Option<Rgba> {
    None
}

/// An icon handle's pixels as straight-alpha RGBA; the handle is destroyed.
#[cfg(windows)]
fn hicon_rgba(icon: windows_sys::Win32::UI::WindowsAndMessaging::HICON) -> Option<Rgba> {
    use windows_sys::Win32::Graphics::Gdi::{BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits, GetObjectW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};
    let mut info: ICONINFO = unsafe { std::mem::zeroed() };
    // SAFETY: a live icon handle and an out struct.
    if unsafe { GetIconInfo(icon, &mut info) } == 0 {
        unsafe { DestroyIcon(icon) };
        return None;
    }
    let result = (|| {
        if info.hbmColor.is_null() {
            return None;
        }
        let mut bitmap: BITMAP = unsafe { std::mem::zeroed() };
        // SAFETY: the icon's colour bitmap and a BITMAP-sized out buffer.
        if unsafe { GetObjectW(info.hbmColor, std::mem::size_of::<BITMAP>() as i32, (&raw mut bitmap).cast()) } == 0 {
            return None;
        }
        let (w, h) = (bitmap.bmWidth, bitmap.bmHeight);
        if w <= 0 || h <= 0 || w > 512 || h > 512 {
            return None;
        }
        let mut header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // rows top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..unsafe { std::mem::zeroed() }
            },
            ..unsafe { std::mem::zeroed() }
        };
        let mut px = vec![0u8; w as usize * h as usize * 4];
        // SAFETY: a memory DC for GetDIBits, a buffer of w×h×4 bytes for 32-bit rows.
        let dc = unsafe { CreateCompatibleDC(std::ptr::null_mut()) };
        let rows = unsafe { GetDIBits(dc, info.hbmColor, 0, h as u32, px.as_mut_ptr().cast(), &mut header, DIB_RGB_COLORS) };
        unsafe { DeleteDC(dc) };
        if rows == 0 {
            return None;
        }
        // Old icons have no alpha (all zero): their mask says what's transparent instead, white
        // there. It's read the same way, 32 bits a pixel; without it they draw opaque.
        let mask = px.as_chunks::<4>().0.iter().all(|p| p[3] == 0).then(|| {
            if info.hbmMask.is_null() {
                return None;
            }
            let mut m = vec![0u8; px.len()];
            // SAFETY: as above, for the mask bitmap of the same size.
            let dc = unsafe { CreateCompatibleDC(std::ptr::null_mut()) };
            let rows = unsafe { GetDIBits(dc, info.hbmMask, 0, h as u32, m.as_mut_ptr().cast(), &mut header, DIB_RGB_COLORS) };
            unsafe { DeleteDC(dc) };
            (rows != 0).then_some(m)
        });
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            p.swap(0, 2);
            match &mask {
                Some(Some(m)) => p[3] = if m[i * 4] == 0 { 255 } else { 0 },
                Some(None) => p[3] = 255,
                None => {}
            }
        }
        Some((w as u32, h as u32, px))
    })();
    // SAFETY: the bitmaps GetIconInfo made and the icon SHGetFileInfoW handed over are ours.
    unsafe {
        if !info.hbmColor.is_null() {
            DeleteObject(info.hbmColor);
        }
        if !info.hbmMask.is_null() {
            DeleteObject(info.hbmMask);
        }
        DestroyIcon(icon);
    }
    result
}

/// The icon keys a pane needs: one per type in its listing.
pub fn keys_of(entries: &[Entry], loc: &Location) -> Vec<IconKey> {
    let mut seen = std::collections::HashSet::new();
    entries.iter().filter_map(|e| key_for(e, loc)).filter(|k| seen.insert(k.clone())).collect()
}

/// The home folder's key, for the tree.
pub fn home_key() -> Option<IconKey> {
    match Location::home() {
        Location::Local(p) => Some(IconKey::Path(p)),
        _ => None,
    }
}

pub fn drive_key(p: &Path) -> IconKey {
    IconKey::Path(p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> Entry {
        Entry { id: name.into(), name: name.into(), is_dir: false, size: None, modified: None, target: None, mime: None, md5: None, starred: false }
    }

    #[test]
    fn icons_are_asked_for_by_type() {
        let drive = Location::Mock(Vec::new());
        assert_eq!(key_for(&file("Filme.MKV"), &drive), Some(IconKey::Ext("mkv".into())));
        assert_eq!(key_for(&file("LEIAME"), &drive), Some(IconKey::Ext(String::new())));
        assert_eq!(key_for(&file(".bashrc"), &drive), Some(IconKey::Ext(String::new())));
        assert_eq!(key_for(&Entry { is_dir: true, ..file("Fotos.2024") }, &drive), Some(IconKey::Folder));
        let doc = Entry { mime: Some("application/vnd.google-apps.document".into()), ..file("Ata") };
        assert_eq!(key_for(&doc, &drive), None);
        // A program carries its own icon, so a local one is asked for by path; on Drive it can't be.
        let local = Location::Local(r"C:\Apps".into());
        assert_eq!(key_for(&file("setup.exe"), &local), Some(IconKey::Path(r"C:\Apps\setup.exe".into())));
        assert_eq!(key_for(&file("setup.exe"), &drive), Some(IconKey::Ext("exe".into())));
        let keys = keys_of(&[file("a.mp4"), file("b.MP4"), file("c.mkv")], &drive);
        assert_eq!(keys, vec![IconKey::Ext("mp4".into()), IconKey::Ext("mkv".into())]);
    }

    #[cfg(windows)]
    #[test]
    fn a_32_px_icon_in_the_48_px_list_is_cut_to_its_corner() {
        let mut px = vec![0u8; 48 * 48 * 4];
        px[(31 * 48 + 31) * 4 + 3] = 255;
        let (w, h, cut) = trim_small((48, 48, px.clone()));
        assert_eq!((w, h, cut.len()), (32, 32, 32 * 32 * 4));
        assert_eq!(cut[(31 * 32 + 31) * 4 + 3], 255);
        px[(40 * 48 + 40) * 4 + 3] = 255;
        assert_eq!(trim_small((48, 48, px)).0, 48);
    }
}
