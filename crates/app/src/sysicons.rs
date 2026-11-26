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
    /// Windows' official "Este Computador" / "This PC" stock icon (`SIID_DESKTOPPC`).
    Computer,
    /// A drive, a special folder or a file whose icon is its own (`.exe`, `.lnk`, `.ico`).
    Path(PathBuf),
    /// A local folder that may carry an icon of its own (a `desktop.ini`). Checked on the path
    /// thread, as reading a folder's attributes can stall on an offline share; `None` when it
    /// has no icon of its own, and the folder shows [`IconKey::Folder`].
    Dir(PathBuf),
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
    if OWN_ICON.contains(&ext.as_str())
        && let Some(path) = loc.local_path(e)
    {
        return Some(IconKey::Path(path));
    }
    Some(IconKey::Ext(ext))
}

/// Straight-alpha RGBA pixels of an icon.
pub type Rgba = (u32, u32, Vec<u8>);

/// Windows Explorer icon sizes: `small` from `SHIL_SMALL` (16 px / DPI-scaled, Details & tree),
/// `large` from `SHIL_EXTRALARGE` (48 px, Medium icons & Propriedades), and `jumbo` from
/// `SHIL_JUMBO` (256 px, Large & Extra large icons) when the icon actually has artwork larger
/// than 48 px.
pub struct IconBitmaps {
    pub small: Rgba,
    pub large: Option<Rgba>,
    pub jumbo: Option<Rgba>,
}

struct StoredIcon {
    small: Image,
    large: Option<Image>,
    jumbo: Option<Image>,
}

#[derive(Default)]
pub struct SysIcons {
    images: Option<ImageCache>,
    /// `None` while the lookup runs or when Windows had no icon.
    known: HashMap<IconKey, Option<StoredIcon>>,
    /// Lookups by type (quick, from the registry) and by path (a drive or a file, which can
    /// stall on an empty DVD drive or an offline share), on separate threads so a slow path
    /// never holds up the types.
    by_type: Option<Sender<(IconKey, bool)>>,
    by_path: Option<Sender<(IconKey, bool)>>,
    /// Drives and the home folder, which the tree always shows.
    fixed: HashSet<IconKey>,
    /// Icons handed to the image cache and still decoding.
    decoding: Vec<IconKey>,
    /// Some pane shows Large or Extra large icons: lookups bring the 256 px icon too. It costs
    /// 256 KB a key, so the other views go without it.
    big: bool,
    /// The keys asked for with the 256 px icon.
    asked_big: HashSet<IconKey>,
}

/// A lookup thread; icons found land in `poll` as [`Msg::Icon`].
fn worker(outbox: Sender<Msg>, wake: rune::Waker) -> Sender<(IconKey, bool)> {
    let (tx, rx) = channel::<(IconKey, bool)>();
    std::thread::spawn(move || {
        #[cfg(windows)]
        // SAFETY: plain COM initialisation for this thread, which the shell's icon lookup needs.
        unsafe {
            windows_sys::Win32::System::Com::CoInitializeEx(std::ptr::null(), windows_sys::Win32::System::Com::COINIT_APARTMENTTHREADED as u32);
        }
        for (key, big) in rx {
            let rgba = lookup(&key, big);
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
        self.want(fixed.into_iter().chain([IconKey::Folder, IconKey::Computer]));
    }

    /// Asks for the icons of `keys` not seen yet.
    fn want(&mut self, keys: impl IntoIterator<Item = IconKey>) {
        for k in keys {
            if self.known.contains_key(&k) {
                continue;
            }
            if !self.ask(k.clone()) {
                return;
            }
            self.known.insert(k, None);
        }
    }

    /// Sends `key` to its lookup thread, with the 256 px icon while `big`. False with no thread.
    fn ask(&mut self, key: IconKey) -> bool {
        let worker = if matches!(key, IconKey::Path(_) | IconKey::Dir(_)) { &self.by_path } else { &self.by_type };
        let Some(worker) = worker else { return false };
        if self.big {
            self.asked_big.insert(key.clone());
        }
        let _ = worker.send((key, self.big));
        true
    }

    /// Whether some pane shows Large or Extra large icons. Turned on, the icons known without
    /// the 256 px one are asked for again with it; they keep showing meanwhile.
    pub fn set_big(&mut self, big: bool) {
        if big == self.big {
            return;
        }
        self.big = big;
        if big {
            let again: Vec<IconKey> = self.known.keys().filter(|k| !self.asked_big.contains(*k)).cloned().collect();
            for k in again {
                self.ask(k);
            }
        }
    }

    /// Asks for what the panes now list, and lets go of the icons of single files (programs,
    /// shortcuts) they no longer show, so browsing System32 doesn't keep thousands of them.
    pub fn show(&mut self, keys: Vec<IconKey>) {
        let listed: HashSet<&IconKey> = keys.iter().collect();
        let fixed = &self.fixed;
        self.known.retain(|k, _| !matches!(k, IconKey::Path(_) | IconKey::Dir(_)) || fixed.contains(k) || listed.contains(k));
        let known = &self.known;
        self.asked_big.retain(|k| known.contains_key(k));
        self.want(keys);
    }

    pub fn arrived(&mut self, key: IconKey, rgba: Option<IconBitmaps>) {
        // One let go of while it was being looked up stays gone.
        let (Some(slot), Some(images)) = (self.known.get_mut(&key), &self.images) else { return };
        *slot = rgba.map(|b| StoredIcon {
            small: images.rgba(b.small.0, b.small.1, b.small.2),
            large: b.large.map(|(w, h, px)| images.rgba(w, h, px)),
            jumbo: b.jumbo.map(|(w, h, px)| images.rgba(w, h, px)),
        });
        if slot.is_some() {
            self.decoding.push(key);
        }
    }

    /// Whether an icon finished decoding since the last call: the view then draws it in place
    /// of the app's own (decoding wakes the app, but nothing else would ask for a new view).
    pub fn settle(&mut self) -> bool {
        let before = self.decoding.len();
        let known = &self.known;
        self.decoding.retain(|k| {
            matches!(
                known.get(k),
                Some(Some(pair))
                    if !pair.small.is_ready()
                        || pair.large.as_ref().is_some_and(|l| !l.is_ready())
                        || pair.jumbo.as_ref().is_some_and(|j| !j.is_ready())
            )
        });
        self.decoding.len() != before
    }

    /// The best-resolution Windows shell icon for a display `size` in points: `SHIL_JUMBO`
    /// (256 px) above 64 pt, `SHIL_EXTRALARGE` (48 px) above 24 pt, and `SHIL_SMALL` otherwise.
    pub fn get_sized(&self, key: &IconKey, size: f32) -> Option<&Image> {
        let pair = self.known.get(key)?.as_ref()?;
        if size > 64.0 {
            pair.jumbo.as_ref().or(pair.large.as_ref()).or(Some(&pair.small))
        } else if size > 24.0 {
            pair.large.as_ref().or(Some(&pair.small))
        } else {
            Some(&pair.small)
        }
    }
}

/// The shell's icons for `key`: `SHIL_SMALL` (the exact 16 px / DPI-scaled icon Windows
/// Explorer draws in Details view and the Navigation Tree) and `SHIL_EXTRALARGE` (48 px, for
/// Propriedades).
#[cfg(windows)]
fn lookup(key: &IconKey, big: bool) -> Option<IconBitmaps> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Controls::{HIMAGELIST, ImageList_GetIcon};
    use windows_sys::Win32::UI::Shell::{
        SHFILEINFOW, SHGFI_ICON, SHGFI_SMALLICON, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES, SHGSI_SYSICONINDEX,
        SHGetFileInfoW, SHGetImageList, SHGetStockIconInfo, SHIL_EXTRALARGE, SHIL_JUMBO, SHIL_SMALL, SHSTOCKICONINFO, SIID_DESKTOPPC,
    };
    const NORMAL: u32 = 0x80;
    const DIRECTORY: u32 = 0x10;

    // System image lists, kept for the thread's life (they are the shell's own, shared).
    thread_local!(static LISTS: (HIMAGELIST, HIMAGELIST, HIMAGELIST) = {
        // IID_IImageList; the interface pointer works as an HIMAGELIST.
        let iid = windows_sys::core::GUID::from_u128(0x46eb5926_582e_4017_9fdf_e8998daa0950);
        let get = |shil: u32| -> HIMAGELIST {
            let mut list = std::ptr::null_mut();
            // SAFETY: a GUID and an out pointer.
            let hr = unsafe { SHGetImageList(shil as i32, &iid, &mut list) };
            if hr < 0 { 0 } else { list as HIMAGELIST }
        };
        (get(SHIL_SMALL), get(SHIL_EXTRALARGE), get(SHIL_JUMBO))
    });
    let from_index = |index: i32| -> Option<IconBitmaps> {
        let (small_list, large_list, jumbo_list) = LISTS.with(|l| *l);
        let from_list = |list: HIMAGELIST, trim: bool| -> Option<Rgba> {
            if list == 0 {
                return None;
            }
            // SAFETY: the system list and an index it gave; ILD_TRANSPARENT.
            let icon = unsafe { ImageList_GetIcon(list, index, 1) };
            if icon.is_null() {
                return None;
            }
            let px = hicon_rgba(icon)?;
            Some(if trim { trim_small(px) } else { px })
        };
        let small = from_list(small_list, false);
        let large = from_list(large_list, true);
        let jumbo = if big { from_list(jumbo_list, false).filter(has_jumbo_artwork) } else { None };
        if let Some(small) = small {
            return Some(IconBitmaps { small, large, jumbo });
        }
        large.map(|large| IconBitmaps { small: large, large: None, jumbo })
    };

    if matches!(key, IconKey::Computer) {
        let mut stock: SHSTOCKICONINFO = unsafe { std::mem::zeroed() };
        stock.cbSize = std::mem::size_of::<SHSTOCKICONINFO>() as u32;
        // SAFETY: `cbSize` is set to the struct's size.
        if unsafe { SHGetStockIconInfo(SIID_DESKTOPPC, SHGSI_SYSICONINDEX, &mut stock) } >= 0 {
            return from_index(stock.iSysImageIndex);
        }
        return None;
    }

    // By type (no file needs to exist) for extensions and folders; by path for the rest.
    let (name, attrs, by): (PathBuf, u32, u32) = match key {
        IconKey::Ext(ext) if ext.is_empty() => ("arquivo".into(), NORMAL, SHGFI_USEFILEATTRIBUTES),
        IconKey::Ext(ext) => (format!("arquivo.{ext}").into(), NORMAL, SHGFI_USEFILEATTRIBUTES),
        IconKey::Folder => ("pasta".into(), DIRECTORY, SHGFI_USEFILEATTRIBUTES),
        IconKey::Computer => unreachable!(),
        IconKey::Dir(p) if !has_custom_folder_icon(p) => return None,
        IconKey::Path(p) | IconKey::Dir(p) => (p.clone(), 0, 0),
    };
    let wide: Vec<u16> = name.as_os_str().encode_wide().chain([0]).collect();
    let size = std::mem::size_of::<SHFILEINFOW>() as u32;
    let mut info: SHFILEINFOW = unsafe { std::mem::zeroed() };
    // SAFETY: a NUL-terminated path and an out struct of the size given.
    if unsafe { SHGetFileInfoW(wide.as_ptr(), attrs, &mut info, size, SHGFI_SYSICONINDEX | by) } != 0
        && let Some(bitmaps) = from_index(info.iIcon)
    {
        return Some(bitmaps);
    }
    let mut info: SHFILEINFOW = unsafe { std::mem::zeroed() };
    // SAFETY: as above; SHGFI_ICON | SHGFI_SMALLICON hands over an icon this function destroys.
    let ok = unsafe { SHGetFileInfoW(wide.as_ptr(), attrs, &mut info, size, SHGFI_ICON | SHGFI_SMALLICON | by) };
    if ok == 0 || info.hIcon.is_null() {
        return None;
    }
    Some(IconBitmaps { small: hicon_rgba(info.hIcon)?, large: None, jumbo: None })
}

/// True when a `SHIL_JUMBO` (256 px) bitmap actually contains pixels beyond its top-left 48×48
/// corner; older icons without jumbo artwork are placed in that corner and should fall back to
/// `SHIL_EXTRALARGE` instead of drawing tiny and off-centre.
#[cfg(windows)]
fn has_jumbo_artwork((w, h, px): &Rgba) -> bool {
    let (w, h) = (*w as usize, *h as usize);
    if w <= 48 || h <= 48 {
        return true;
    }
    for y in 0..h {
        let start_x = if y < 48 { 48 } else { 0 };
        for x in start_x..w {
            if px[(y * w + x) * 4 + 3] != 0 {
                return true;
            }
        }
    }
    false
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
fn lookup(_: &IconKey, _: bool) -> Option<IconBitmaps> {
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

/// Whether a local directory has a Windows custom/special folder icon (`desktop.ini` is only
/// consulted by Windows Explorer on folders marked Read-Only or System).
#[cfg(windows)]
fn has_custom_folder_icon(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const READONLY_OR_SYSTEM: u32 = 0x1 | 0x4;
    let Ok(meta) = std::fs::metadata(path) else { return false };
    (meta.file_attributes() & READONLY_OR_SYSTEM) != 0 && path.join("desktop.ini").is_file()
}

#[cfg(not(windows))]
fn has_custom_folder_icon(_: &Path) -> bool {
    false
}

/// The icon keys a pane needs: one per type in its listing, plus each local folder, which may
/// carry an icon of its own (Documentos, Downloads, Imagens, OneDrive…). Nothing here touches
/// the disk: it runs on the UI thread, and the path thread checks the folders.
pub fn keys_of(entries: &[Entry], loc: &Location) -> Vec<IconKey> {
    let mut seen = std::collections::HashSet::new();
    let mut keys = Vec::new();
    for e in entries {
        if e.is_dir
            && let Some(path) = loc.local_path(e)
        {
            keys.push(IconKey::Dir(path));
        }
        if let Some(k) = key_for(e, loc)
            && seen.insert(k.clone())
        {
            keys.push(k);
        }
    }
    keys
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
        Entry { id: name.into(), name: name.into(), is_dir: false, size: None, modified: None, target: None, mime: None, md5: None, starred: false, drive_origin: None }
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

    /// Benchmark: building a listing's icon keys, which `poll` does on the UI thread each time a
    /// pane's listing changes.
    /// `cargo test --release -p openexplorer keys_of_cost -- --ignored --nocapture`
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn keys_of_cost() {
        for dir in [r"C:\Windows\WinSxS", r"C:\Windows\System32"] {
            let dir = PathBuf::from(dir);
            let entries: Vec<Entry> = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| Entry { is_dir: e.file_type().is_ok_and(|t| t.is_dir()), ..file(&e.file_name().to_string_lossy()) })
                .collect();
            let folders = entries.iter().filter(|e| e.is_dir).count();
            let loc = Location::Local(dir.clone());
            // Once to warm the file system's cache, as a listing just read has.
            std::hint::black_box(keys_of(&entries, &loc));
            let rounds = 20;
            let started = std::time::Instant::now();
            for _ in 0..rounds {
                std::hint::black_box(keys_of(&entries, &loc));
            }
            let ms = started.elapsed().as_secs_f64() * 1000.0 / rounds as f64;
            println!("{}: {} entries, {folders} folders: {ms:.3} ms per keys_of", dir.display(), entries.len());
        }
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
