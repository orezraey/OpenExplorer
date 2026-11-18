//! Dragging Google Drive items, and a server's, out of the app, as "virtual files".
//!
//! Rune's drag-out hands Windows paths of files that must already be on disk the moment the
//! drag starts, which for Drive (or a server) would mean downloading everything first. Instead this drag
//! carries a description of the files (`FileGroupDescriptorW`: names, folders, sizes, dates)
//! and offers each file's bytes as a stream (`FileContents`), the way browsers and mail apps
//! drag out attachments. Nothing is fetched until the drop: the target (Explorer) then reads
//! the streams, on its own background thread since the data object supports asynchronous
//! extraction, with its own copy dialog, while the app stays responsive. Folders go with their
//! whole tree, listed from Drive when the target asks for the description.
//!
//! The session runs as Rune's does: posted to the window and started from its message loop,
//! since `DoDragDrop` runs a modal loop of its own. When it ends over the app's own window,
//! the app is told which half (pane) it ended over, and copies there through the queue.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use openexplorer_gdrive::download::{export_format, local_name};
use openexplorer_gdrive::{Drive, DriveFile};
use windows::Win32::Foundation::{DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, E_FAIL, E_NOTIMPL, FILETIME, HGLOBAL, S_FALSE, S_OK};
use windows::Win32::System::Com::{
    CoTaskMemAlloc, DATADIR_GET, STATFLAG, STATFLAG_NONAME, STGTY_STREAM, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IBindCtx, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE, STATSTG, STGC, STGMEDIUM, STGMEDIUM_0, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize, ReleaseStgMedium};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::{CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, FD_ATTRIBUTES, FD_FILESIZE, FD_PROGRESSUI, FD_UNICODE, FD_WRITESTIME, FILEDESCRIPTORW, IDataObjectAsyncCapability, IDataObjectAsyncCapability_Impl, SHCreateStdEnumFmtEtc};
use windows::core::{BOOL, HRESULT, Ref, implement};

use crate::ops::CANCELLED;
use crate::work::Queue;

/// How a drag out of a Drive pane ended.
pub enum Ended {
    /// Let go over another app, which pulls the files itself.
    Outside,
    /// Let go over this app's window, over the left (0) or right (1) half.
    InApp(usize),
    Cancelled,
}

/// Where the dragged files come from: Google Drive or a server.
pub trait Files: Send + Sync + 'static {
    /// The description of what's dragged: each item, and for folders everything in them, a
    /// folder before what's in it, as the target makes them. Stops early once `stop` is set.
    fn list(&self, stop: &AtomicBool) -> Result<Vec<Virtual>, String>;
    /// File `file`'s bytes from byte `from` on.
    fn open(&self, file: &File, from: u64) -> Result<Box<dyn Read + Send>, String>;
}

/// The dragged items of a Drive account.
pub struct DriveSource {
    pub drive: Arc<Drive>,
    pub files: Vec<DriveFile>,
}

impl Files for DriveSource {
    fn list(&self, stop: &AtomicBool) -> Result<Vec<Virtual>, String> {
        list(&self.drive, &self.files, stop)
    }

    fn open(&self, file: &File, from: u64) -> Result<Box<dyn Read + Send>, String> {
        match &file.fetch {
            Fetch::Drive(f) => self.drive.open_media(f, from),
            Fetch::Server(_) => Err(openexplorer_i18n::t("Arquivo de outro lugar.", "File from another place.").into()),
        }
    }
}

/// Starts dragging what `source` describes out of the window `hwnd`; `done` hears how it ended,
/// on the UI thread, once the session is over.
pub fn start(hwnd: isize, source: Arc<dyn Files>, done: Box<dyn FnOnce(Ended)>) {
    let pending = Pending { hwnd, source, done };
    if let Some(old) = PENDING.with(|p| p.borrow_mut().replace(pending)) {
        (old.done)(Ended::Cancelled);
    }
    use windows_sys::Win32::UI::Shell::{RemoveWindowSubclass, SetWindowSubclass};
    // SAFETY: Win32 calls on a live window of this thread; the subclass keeps no pointer.
    let posted = unsafe {
        SetWindowSubclass(hwnd as _, Some(procedure), SUBCLASS, 0) != 0
            && (windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(hwnd as _, message(), 0, 0) != 0 || {
                RemoveWindowSubclass(hwnd as _, Some(procedure), SUBCLASS);
                false
            })
    };
    if !posted {
        run();
    }
}

struct Pending {
    hwnd: isize,
    source: Arc<dyn Files>,
    done: Box<dyn FnOnce(Ended)>,
}

thread_local! {
    static PENDING: std::cell::RefCell<Option<Pending>> = const { std::cell::RefCell::new(None) };
}

const SUBCLASS: usize = 0x4f45_4447;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn message() -> u32 {
    static MESSAGE: OnceLock<u32> = OnceLock::new();
    // SAFETY: a NUL-terminated name.
    *MESSAGE.get_or_init(|| unsafe { windows_sys::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW(wide("OpenExplorer::DriveDrag").as_ptr()) })
}

unsafe extern "system" fn procedure(hwnd: windows_sys::Win32::Foundation::HWND, msg: u32, w: usize, l: isize, _: usize, _: usize) -> isize {
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::WM_NCDESTROY;
    if msg == message() || msg == WM_NCDESTROY {
        // SAFETY: removes this subclass from the window it was set on.
        unsafe { RemoveWindowSubclass(hwnd, Some(procedure), SUBCLASS) };
        if msg == message() {
            run();
            return 0;
        }
        if let Some(p) = PENDING.with(|p| p.borrow_mut().take()) {
            (p.done)(Ended::Cancelled);
        }
    }
    // SAFETY: every other message goes on.
    unsafe { DefSubclassProc(hwnd, msg, w, l) }
}

fn run() {
    let Some(p) = PENDING.with(|p| p.borrow_mut().take()) else { return };
    let ended = session(&p).unwrap_or(Ended::Cancelled);
    // OLE's loop took the button's release; Rune still holds the press that started the drag,
    // so it's told the button is up.
    // SAFETY: posting a message to a live window of this thread.
    unsafe {
        let mut at = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
        windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut at);
        windows_sys::Win32::Graphics::Gdi::ScreenToClient(p.hwnd as _, &mut at);
        let l = ((at.y as u32 & 0xffff) << 16 | (at.x as u32 & 0xffff)) as isize;
        windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(p.hwnd as _, windows_sys::Win32::UI::WindowsAndMessaging::WM_LBUTTONUP, 0, l);
    }
    (p.done)(ended);
}

fn session(p: &Pending) -> Option<Ended> {
    // SAFETY: the UI thread is an OLE apartment already (the window takes drops); again is a
    // no-op. The objects made here are released when they drop.
    unsafe {
        let _ = OleInitialize(None);
        let data: IDataObject = Data::new(p.source.clone()).into();
        let escaped = Arc::new(AtomicBool::new(false));
        let source: IDropSource = Source(escaped.clone()).into();
        let mut effect = DROPEFFECT_NONE;
        let result = DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect);
        if escaped.load(Ordering::Relaxed) {
            return Some(Ended::Cancelled);
        }
        // Over this app's own window: which half.
        let mut at = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
        windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut at);
        let under = windows_sys::Win32::UI::WindowsAndMessaging::WindowFromPoint(at);
        let root = windows_sys::Win32::UI::WindowsAndMessaging::GetAncestor(under, windows_sys::Win32::UI::WindowsAndMessaging::GA_ROOT);
        if root == p.hwnd as _ {
            let mut rect = windows_sys::Win32::Foundation::RECT { left: 0, top: 0, right: 0, bottom: 0 };
            windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(p.hwnd as _, &mut rect);
            windows_sys::Win32::Graphics::Gdi::ScreenToClient(p.hwnd as _, &mut at);
            return Some(Ended::InApp(usize::from(at.x * 2 >= rect.right)));
        }
        Some(if result == DRAGDROP_S_DROP && effect != DROPEFFECT_NONE { Ended::Outside } else { Ended::Cancelled })
    }
}

/// Ends the drag when the left button comes up, cancels it on Escape (noted, so a cancel isn't
/// taken for a drop over the app); the system's cursors.
#[implement(IDropSource)]
struct Source(Arc<AtomicBool>);

impl IDropSource_Impl for Source_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            self.0.store(true, Ordering::Relaxed);
            return DRAGDROP_S_CANCEL;
        }
        if keys.0 & MK_LBUTTON.0 == 0 {
            return DRAGDROP_S_DROP;
        }
        S_OK
    }

    fn GiveFeedback(&self, _: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

// ---------------------------------------------------------------------------------------------
// The data object

fn format(name: windows::core::PCWSTR) -> u16 {
    // SAFETY: a NUL-terminated name.
    unsafe { RegisterClipboardFormatW(name) as u16 }
}

fn descriptor_format() -> u16 {
    static F: OnceLock<u16> = OnceLock::new();
    *F.get_or_init(|| format(CFSTR_FILEDESCRIPTORW))
}

fn contents_format() -> u16 {
    static F: OnceLock<u16> = OnceLock::new();
    *F.get_or_init(|| format(CFSTR_FILECONTENTS))
}

/// One entry of the description: a folder, or a file and where its bytes come from. `path` is
/// relative, with backslashes, as the target creates it.
pub struct Virtual {
    pub path: String,
    pub file: Option<File>,
}

/// A dragged file.
#[derive(Clone)]
pub struct File {
    /// Its name, as it lands.
    pub name: String,
    /// `None` when it isn't known before the file is made (a Google Docs export).
    pub size: Option<u64>,
    pub modified: Option<std::time::SystemTime>,
    pub fetch: Fetch,
}

/// Where a dragged file's bytes are.
#[derive(Clone)]
pub enum Fetch {
    Drive(DriveFile),
    /// Its path on the server.
    Server(String),
}

const DV_E_FORMATETC: HRESULT = HRESULT(0x8004_0064_u32 as i32);
const DV_E_LINDEX: HRESULT = HRESULT(0x8004_0068_u32 as i32);
const DV_E_TYMED: HRESULT = HRESULT(0x8004_0069_u32 as i32);
const OLE_E_ADVISENOTSUPPORTED: HRESULT = HRESULT(0x8004_0003_u32 as i32);
const DATA_S_SAMEFORMATETC: HRESULT = HRESULT(0x0004_0130);

#[implement(IDataObject, IDataObjectAsyncCapability)]
struct Data {
    source: Arc<dyn Files>,
    /// The description, being listed from the moment the drag started.
    listing: Arc<Listing>,
    /// Small files fetched ahead of the target.
    ahead: Arc<Ahead>,
    /// Formats other code stored (the drag image), as bytes.
    extra: Mutex<Vec<(u16, Vec<u8>)>>,
    asynchronous: AtomicBool,
    busy: AtomicBool,
}

impl Data {
    fn new(source: Arc<dyn Files>) -> Data {
        let listing = Listing::start(source.clone());
        Data { source, listing, ahead: Arc::new(Ahead::default()), extra: Mutex::new(Vec::new()), asynchronous: AtomicBool::new(true), busy: AtomicBool::new(false) }
    }

    fn listing(&self) -> Result<Arc<Vec<Virtual>>, String> {
        self.listing.wait()
    }
}

/// The drag is over and nothing holds the data any more: work for it stops.
impl Drop for Data {
    fn drop(&mut self) {
        self.listing.stop.store(true, Ordering::Relaxed);
        self.ahead.stop.store(true, Ordering::Relaxed);
        self.ahead.free.notify_all();
    }
}

/// The description of the dragged items, listed on a thread of its own from the moment the
/// drag starts, so by the drop most or all of it is known.
struct Listing {
    result: Mutex<Option<Result<Arc<Vec<Virtual>>, String>>>,
    ready: Condvar,
    stop: AtomicBool,
}

impl Listing {
    fn start(source: Arc<dyn Files>) -> Arc<Listing> {
        let l = Arc::new(Listing { result: Mutex::new(None), ready: Condvar::new(), stop: AtomicBool::new(false) });
        let me = l.clone();
        std::thread::spawn(move || {
            let r = source.list(&me.stop).map(Arc::new);
            // A drag that was let go of stops its listing: not an error.
            if let Err(e) = &r
                && e != CANCELLED
            {
                openexplorer_gdrive::log(&format!("arrastar para fora: {e}"));
            }
            *me.result.lock().unwrap() = Some(r);
            me.ready.notify_all();
        });
        l
    }

    fn wait(&self) -> Result<Arc<Vec<Virtual>>, String> {
        let mut r = self.result.lock().unwrap();
        while r.is_none() {
            r = self.ready.wait(r).unwrap();
        }
        r.clone().unwrap()
    }
}

/// Folders listed at the same time: each is a round trip to Google, so a tree of hundreds of
/// folders lists in a fraction of the time one by one takes.
const LISTERS: usize = 6;

/// The description of `top`: each item, and for folders everything in them, depth first (a
/// folder before what's in it, as the target makes them). Google formats that can't be
/// exported and shortcuts to files stay out: nothing to fetch. Folders are listed in parallel;
/// one reached again (a shortcut back up the tree) is listed and placed once.
fn list(d: &Drive, top: &[DriveFile], stop: &AtomicBool) -> Result<Vec<Virtual>, String> {
    let folder_id = |f: &DriveFile| f.target.clone().unwrap_or_else(|| f.id.clone());
    let kids: Mutex<HashMap<String, Vec<DriveFile>>> = Mutex::new(HashMap::new());
    let seen: Mutex<HashSet<String>> = Mutex::new(HashSet::new());
    let error: Mutex<Option<String>> = Mutex::new(None);
    let first: Vec<String> = top.iter().filter(|f| f.is_dir).map(folder_id).filter(|id| seen.lock().unwrap().insert(id.clone())).collect();
    let queue = Queue::new(first);
    std::thread::scope(|scope| {
        // Tests compare against one at a time with `OE_LISTERS=1`.
        let listers = if cfg!(test) { std::env::var("OE_LISTERS").ok().and_then(|v| v.parse().ok()).unwrap_or(LISTERS) } else { LISTERS };
        for _ in 0..listers {
            scope.spawn(|| {
                while let Some((id, _done)) = queue.next() {
                    if stop.load(Ordering::Relaxed) || error.lock().unwrap().is_some() {
                        continue;
                    }
                    let mut all = Vec::new();
                    let listed = d.list(&id, None, |page, _| {
                        all.extend(page);
                        !stop.load(Ordering::Relaxed)
                    });
                    match listed {
                        Ok(_) => {
                            let subs: Vec<String> = all.iter().filter(|f| f.is_dir).map(folder_id).filter(|s| seen.lock().unwrap().insert(s.clone())).collect();
                            kids.lock().unwrap().insert(id, all);
                            queue.add(subs);
                        }
                        Err(e) => {
                            error.lock().unwrap().get_or_insert(e);
                        }
                    }
                }
            });
        }
    });
    if stop.load(Ordering::Relaxed) {
        return Err(CANCELLED.into());
    }
    if let Some(e) = error.into_inner().unwrap() {
        return Err(e);
    }
    let kids = kids.into_inner().unwrap();
    fn add(f: &DriveFile, prefix: &str, kids: &HashMap<String, Vec<DriveFile>>, placed: &mut HashSet<String>, out: &mut Vec<Virtual>) {
        let google = f.mime.starts_with("application/vnd.google-apps.");
        if !f.is_dir && google && export_format(&f.mime).is_none() {
            return;
        }
        let name = crate::ops::windows_name(&if f.is_dir { f.name.clone() } else { local_name(f) });
        let path = if prefix.is_empty() { name } else { format!("{prefix}\\{name}") };
        // The description holds paths of up to 259 characters; a longer one (and what's under
        // a folder that long) stays out rather than arrive under a cut-off name.
        if path.encode_utf16().count() > 259 {
            openexplorer_gdrive::log(&format!("arrastar para fora: caminho longo demais, fica de fora: {path}"));
            return;
        }
        if !f.is_dir {
            // An export's size isn't known before it's made.
            let size = if export_format(&f.mime).is_some() { None } else { f.size };
            out.push(Virtual { path, file: Some(File { name: local_name(f), size, modified: f.modified, fetch: Fetch::Drive(f.clone()) }) });
            return;
        }
        let id = f.target.clone().unwrap_or_else(|| f.id.clone());
        if !placed.insert(id.clone()) {
            return;
        }
        out.push(Virtual { path: path.clone(), file: None });
        for c in kids.get(&id).map(Vec::as_slice).unwrap_or_default() {
            add(c, &path, kids, placed, out);
        }
    }
    let mut out = Vec::new();
    let mut placed = HashSet::new();
    for f in top {
        add(f, "", &kids, &mut placed, &mut out);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Files fetched ahead

/// Files ahead of the one the target is reading that are fetched already: Explorer pulls one
/// file at a time, and for small files the round trip to Google, not the bytes, is what takes
/// the time.
const AHEAD: usize = 8;
/// Fetched at the same time.
const AHEAD_AT_ONCE: usize = 4;
/// Only files up to this size are fetched ahead; bigger ones stream as they're read.
const AHEAD_MAX_FILE: u64 = 8 << 20;
/// Bytes held for files fetched ahead and not read yet, at most.
const AHEAD_MAX_HELD: u64 = 64 << 20;

/// One file fetched ahead: `None` while it's coming, then its bytes, or `None` inside when it
/// failed (the target's read then goes to Drive itself).
struct Slot {
    state: Mutex<Option<Option<Arc<Vec<u8>>>>>,
    ready: Condvar,
    /// What it counts against [`AHEAD_MAX_HELD`].
    weight: u64,
}

#[derive(Default)]
struct Ahead {
    slots: Mutex<HashMap<usize, Arc<Slot>>>,
    /// Files already planned, so one read once isn't fetched again.
    planned: Mutex<HashSet<usize>>,
    held: AtomicU64,
    /// Fetches running.
    running: Mutex<usize>,
    free: Condvar,
    stop: AtomicBool,
}

impl Ahead {
    /// The slot of file `i`, if it was fetched ahead; the stream reading it takes it.
    fn take(&self, i: usize) -> Option<Arc<Slot>> {
        self.slots.lock().unwrap().remove(&i)
    }

    /// The target opened file `i`: the next small files start coming, within the limits.
    fn plan(self: &Arc<Self>, source: &Arc<dyn Files>, items: &Arc<Vec<Virtual>>, i: usize) {
        self.planned.lock().unwrap().insert(i);
        // Files the target passed over (Explorer's Pular for names already there) won't be
        // read: their bytes go, and stop counting against the budget.
        self.slots.lock().unwrap().retain(|&j, slot| {
            if j < i {
                self.release(slot);
            }
            j >= i
        });
        // Tests compare against no fetching ahead with `OE_NO_AHEAD`.
        if cfg!(test) && std::env::var_os("OE_NO_AHEAD").is_some() {
            return;
        }
        for (j, v) in items.iter().enumerate().skip(i + 1).take(AHEAD * 2).filter(|(_, v)| v.file.is_some()).take(AHEAD) {
            let Some(file) = &v.file else { continue };
            // An export's size isn't known before it's made: counted as 1 MB.
            let weight = file.size.unwrap_or(1 << 20);
            if weight > AHEAD_MAX_FILE || !self.planned.lock().unwrap().insert(j) {
                continue;
            }
            if self.held.load(Ordering::Relaxed) + weight > AHEAD_MAX_HELD {
                self.planned.lock().unwrap().remove(&j);
                break;
            }
            self.held.fetch_add(weight, Ordering::Relaxed);
            let slot = Arc::new(Slot { state: Mutex::new(None), ready: Condvar::new(), weight });
            self.slots.lock().unwrap().insert(j, slot.clone());
            let (me, source, file) = (self.clone(), source.clone(), file.clone());
            std::thread::spawn(move || {
                // A turn among the fetches running.
                {
                    let mut running = me.running.lock().unwrap();
                    while *running >= AHEAD_AT_ONCE && !me.stop.load(Ordering::Relaxed) {
                        running = me.free.wait(running).unwrap();
                    }
                    *running += 1;
                }
                let got = if me.stop.load(Ordering::Relaxed) {
                    None
                } else {
                    source.open(&file, 0).ok().and_then(|mut r| {
                        let mut buf = Vec::with_capacity(file.size.unwrap_or(0) as usize);
                        r.read_to_end(&mut buf).ok().map(|_| Arc::new(buf))
                    })
                };
                *slot.state.lock().unwrap() = Some(got);
                slot.ready.notify_all();
                *me.running.lock().unwrap() -= 1;
                me.free.notify_all();
            });
        }
    }

    /// A fetched-ahead file was read (or let go): its bytes no longer count.
    fn release(&self, slot: &Slot) {
        self.held.fetch_sub(slot.weight, Ordering::Relaxed);
    }
}

fn filetime(t: std::time::SystemTime) -> FILETIME {
    // 100 ns ticks since 1601.
    let ticks = t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() / 100).unwrap_or(0) as u64 + 116_444_736_000_000_000;
    FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 }
}

/// `FILEGROUPDESCRIPTORW`: the count, then one `FILEDESCRIPTORW` per entry.
fn descriptor_bytes(items: &[Virtual]) -> Vec<u8> {
    let mut out = (items.len() as u32).to_le_bytes().to_vec();
    for v in items {
        let mut d = FILEDESCRIPTORW { dwFlags: (FD_ATTRIBUTES.0 | FD_UNICODE.0 | FD_PROGRESSUI.0) as u32, ..Default::default() };
        match &v.file {
            None => d.dwFileAttributes = 0x10, // FILE_ATTRIBUTE_DIRECTORY
            Some(f) => {
                d.dwFileAttributes = 0x80; // FILE_ATTRIBUTE_NORMAL
                if let Some(n) = f.size {
                    d.dwFlags |= FD_FILESIZE.0 as u32;
                    d.nFileSizeHigh = (n >> 32) as u32;
                    d.nFileSizeLow = n as u32;
                }
                if let Some(t) = f.modified {
                    d.dwFlags |= FD_WRITESTIME.0 as u32;
                    d.ftLastWriteTime = filetime(t);
                }
            }
        }
        let mut name = [0u16; 260];
        for (slot, unit) in name.iter_mut().zip(v.path.encode_utf16().take(259)) {
            *slot = unit;
        }
        d.cFileName = name;
        // SAFETY: the struct is plain data, read as its bytes.
        let bytes = unsafe { std::slice::from_raw_parts((&raw const d).cast::<u8>(), size_of::<FILEDESCRIPTORW>()) };
        out.extend_from_slice(bytes);
    }
    out
}

fn hglobal(bytes: &[u8]) -> windows::core::Result<HGLOBAL> {
    // SAFETY: a new movable block of the bytes' size, filled while locked.
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
        let p = GlobalLock(h);
        if p.is_null() {
            return Err(E_FAIL.into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast(), bytes.len());
        let _ = GlobalUnlock(h);
        Ok(h)
    }
}

fn global_medium(bytes: &[u8]) -> windows::core::Result<STGMEDIUM> {
    Ok(STGMEDIUM { tymed: TYMED_HGLOBAL.0 as u32, u: STGMEDIUM_0 { hGlobal: hglobal(bytes)? }, pUnkForRelease: std::mem::ManuallyDrop::new(None) })
}

impl IDataObject_Impl for Data_Impl {
    fn GetData(&self, fmt: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        // SAFETY: the caller's format.
        let fmt = unsafe { &*fmt };
        if fmt.cfFormat == descriptor_format() {
            if fmt.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
                return Err(DV_E_TYMED.into());
            }
            let items = self.listing().map_err(|_| windows::core::Error::from(E_FAIL))?;
            return global_medium(&descriptor_bytes(&items));
        }
        if fmt.cfFormat == contents_format() {
            if fmt.tymed & TYMED_ISTREAM.0 as u32 == 0 {
                return Err(DV_E_TYMED.into());
            }
            let items = self.listing().map_err(|_| windows::core::Error::from(E_FAIL))?;
            let Some((i, file)) = usize::try_from(fmt.lindex).ok().and_then(|i| Some((i, items.get(i)?.file.clone()?))) else {
                return Err(DV_E_LINDEX.into());
            };
            // This file's bytes if they came ahead, and the next ones start coming.
            let slot = self.ahead.take(i);
            self.ahead.plan(&self.source, &items, i);
            let stream: IStream = Stream { source: self.source.clone(), file, state: Mutex::new((0, None)), ahead: slot.map(|s| (s, self.ahead.clone())) }.into();
            return Ok(STGMEDIUM { tymed: TYMED_ISTREAM.0 as u32, u: STGMEDIUM_0 { pstm: std::mem::ManuallyDrop::new(Some(stream)) }, pUnkForRelease: std::mem::ManuallyDrop::new(None) });
        }
        let extra = self.extra.lock().unwrap();
        match extra.iter().find(|(f, _)| *f == fmt.cfFormat) {
            Some((_, bytes)) if fmt.tymed & TYMED_HGLOBAL.0 as u32 != 0 => global_medium(bytes),
            _ => Err(DV_E_FORMATETC.into()),
        }
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, fmt: *const FORMATETC) -> HRESULT {
        // SAFETY: the caller's format.
        let fmt = unsafe { &*fmt };
        let known = fmt.cfFormat == descriptor_format() || fmt.cfFormat == contents_format() || self.extra.lock().unwrap().iter().any(|(f, _)| *f == fmt.cfFormat);
        if known { S_OK } else { DV_E_FORMATETC }
    }

    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        if !out.is_null() {
            // SAFETY: the caller's out format.
            unsafe { (*out).ptd = std::ptr::null_mut() };
        }
        DATA_S_SAMEFORMATETC
    }

    fn SetData(&self, fmt: *const FORMATETC, medium: *const STGMEDIUM, release: BOOL) -> windows::core::Result<()> {
        // SAFETY: the caller's format and medium; an HGLOBAL medium's bytes are copied, and the
        // medium released when the call hands it over.
        unsafe {
            let (fmt, m) = (&*fmt, &*medium);
            if m.tymed != TYMED_HGLOBAL.0 as u32 {
                return Err(E_NOTIMPL.into());
            }
            let h = m.u.hGlobal;
            let size = GlobalSize(h);
            let p = GlobalLock(h);
            if p.is_null() {
                return Err(E_FAIL.into());
            }
            let bytes = std::slice::from_raw_parts(p.cast::<u8>(), size).to_vec();
            let _ = GlobalUnlock(h);
            let mut extra = self.extra.lock().unwrap();
            extra.retain(|(f, _)| *f != fmt.cfFormat);
            extra.push((fmt.cfFormat, bytes));
            if release.as_bool() {
                ReleaseStgMedium(medium as *mut _);
            }
        }
        Ok(())
    }

    fn EnumFormatEtc(&self, direction: u32) -> windows::core::Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        let f = |cf: u16, tymed: u32, lindex: i32| FORMATETC { cfFormat: cf, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex, tymed };
        let mut all = vec![f(descriptor_format(), TYMED_HGLOBAL.0 as u32, -1), f(contents_format(), TYMED_ISTREAM.0 as u32, -1)];
        all.extend(self.extra.lock().unwrap().iter().map(|(cf, _)| f(*cf, TYMED_HGLOBAL.0 as u32, -1)));
        // SAFETY: a list of plain formats, copied by the enumerator.
        unsafe { SHCreateStdEnumFmtEtc(&all) }
    }

    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<IAdviseSink>) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// Lets Explorer pull the files on a thread of its own after the drop, so the app's window
/// isn't held while they download.
impl IDataObjectAsyncCapability_Impl for Data_Impl {
    fn SetAsyncMode(&self, on: BOOL) -> windows::core::Result<()> {
        self.asynchronous.store(on.as_bool(), Ordering::Relaxed);
        Ok(())
    }

    fn GetAsyncMode(&self) -> windows::core::Result<BOOL> {
        Ok(self.asynchronous.load(Ordering::Relaxed).into())
    }

    fn StartOperation(&self, _: Ref<IBindCtx>) -> windows::core::Result<()> {
        self.busy.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn InOperation(&self) -> windows::core::Result<BOOL> {
        Ok(self.busy.load(Ordering::Relaxed).into())
    }

    fn EndOperation(&self, _: HRESULT, _: Ref<IBindCtx>, _: u32) -> windows::core::Result<()> {
        self.busy.store(false, Ordering::Relaxed);
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// One file's bytes

/// A file as the target reads it: from the bytes fetched ahead when there are some, else
/// straight from where it is, asking again from where it got to after a dropped connection.
#[implement(IStream)]
struct Stream {
    source: Arc<dyn Files>,
    file: File,
    state: Mutex<(u64, Option<Box<dyn Read + Send>>)>,
    /// The file fetched ahead, while it's still to be used.
    ahead: Option<(Arc<Slot>, Arc<Ahead>)>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        if let Some((slot, ahead)) = &self.ahead {
            ahead.release(slot);
        }
    }
}

impl Stream {
    /// The bytes fetched ahead, once they're here; `None` when there are none (or it failed).
    fn fetched(&self) -> Option<Arc<Vec<u8>>> {
        let (slot, _) = self.ahead.as_ref()?;
        let mut s = slot.state.lock().unwrap();
        while s.is_none() {
            s = slot.ready.wait(s).unwrap();
        }
        s.clone().flatten()
    }
}

const STG_E_READFAULT: HRESULT = HRESULT(0x8003_001E_u32 as i32);

impl ISequentialStream_Impl for Stream_Impl {
    fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, read: *mut u32) -> HRESULT {
        let mut st = self.state.lock().unwrap();
        // SAFETY: the caller's buffer of `cb` bytes.
        let buf = unsafe { std::slice::from_raw_parts_mut(pv.cast::<u8>(), cb as usize) };
        if let Some(bytes) = self.fetched() {
            let from = (st.0 as usize).min(bytes.len());
            let n = buf.len().min(bytes.len() - from);
            buf[..n].copy_from_slice(&bytes[from..from + n]);
            st.0 += n as u64;
            if !read.is_null() {
                // SAFETY: the caller's out count.
                unsafe { *read = n as u32 };
            }
            return if n < buf.len() { S_FALSE } else { S_OK };
        }
        let mut filled = 0usize;
        let mut failures = 0;
        while filled < buf.len() {
            if st.1.is_none() {
                match self.source.open(&self.file, st.0) {
                    Ok(r) => st.1 = Some(r),
                    Err(e) => {
                        failures += 1;
                        openexplorer_gdrive::log(&format!("arrastar para fora: {e}"));
                        if failures > 3 {
                            return STG_E_READFAULT;
                        }
                        std::thread::sleep(std::time::Duration::from_secs(1 << failures));
                        continue;
                    }
                }
            }
            match st.1.as_mut().unwrap().read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => {
                    filled += n;
                    st.0 += n as u64;
                }
                // Asked again from here on the next round.
                Err(_) => {
                    st.1 = None;
                    failures += 1;
                    if failures > 3 {
                        return STG_E_READFAULT;
                    }
                }
            }
        }
        if !read.is_null() {
            // SAFETY: the caller's out count.
            unsafe { *read = filled as u32 };
        }
        if filled < buf.len() { S_FALSE } else { S_OK }
    }

    fn Write(&self, _: *const core::ffi::c_void, _: u32, _: *mut u32) -> HRESULT {
        E_NOTIMPL
    }
}

impl IStream_Impl for Stream_Impl {
    fn Seek(&self, by: i64, origin: STREAM_SEEK, to: *mut u64) -> windows::core::Result<()> {
        let mut st = self.state.lock().unwrap();
        let base = match origin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => st.0 as i64,
            STREAM_SEEK_END => self.file.size.ok_or(windows::core::Error::from(E_NOTIMPL))? as i64,
            _ => return Err(E_FAIL.into()),
        };
        let at = u64::try_from(base + by).map_err(|_| windows::core::Error::from(E_FAIL))?;
        if at != st.0 {
            // Another place: asked again from there.
            *st = (at, None);
        }
        if !to.is_null() {
            // SAFETY: the caller's out position.
            unsafe { *to = at };
        }
        Ok(())
    }

    fn SetSize(&self, _: u64) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn CopyTo(&self, _: Ref<IStream>, _: u64, _: *mut u64, _: *mut u64) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Commit(&self, _: &STGC) -> windows::core::Result<()> {
        Ok(())
    }

    fn Revert(&self) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Stat(&self, out: *mut STATSTG, flags: &STATFLAG) -> windows::core::Result<()> {
        if out.is_null() {
            return Err(E_FAIL.into());
        }
        let mut s = STATSTG { r#type: STGTY_STREAM.0 as u32, cbSize: self.file.size.unwrap_or(0), ..Default::default() };
        if let Some(t) = self.file.modified {
            s.mtime = filetime(t);
        }
        if flags.0 & STATFLAG_NONAME.0 == 0 {
            let name: Vec<u16> = self.file.name.encode_utf16().chain([0]).collect();
            // SAFETY: a CoTaskMem block for the name, which the caller frees.
            unsafe {
                let p = CoTaskMemAlloc(name.len() * 2).cast::<u16>();
                if !p.is_null() {
                    std::ptr::copy_nonoverlapping(name.as_ptr(), p, name.len());
                    s.pwcsName = windows::core::PWSTR(p);
                }
            }
        }
        // SAFETY: the caller's out struct.
        unsafe { *out = s };
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, size: Option<u64>, mime: &str) -> File {
        let f = DriveFile { id: "x".into(), name: name.into(), is_dir: false, size, modified: None, mime: mime.into(), target: None, md5: None, starred: false };
        let size = if export_format(&f.mime).is_some() { None } else { f.size };
        File { name: local_name(&f), size, modified: None, fetch: Fetch::Drive(f) }
    }

    #[test]
    fn the_description_lists_names_folders_and_known_sizes() {
        let items = vec![
            Virtual { path: "Fotos".into(), file: None },
            Virtual { path: "Fotos\\praia.jpg".into(), file: Some(file("praia.jpg", Some(5_000_000_000), "image/jpeg")) },
            Virtual { path: "Ata.docx".into(), file: Some(file("Ata", Some(1024), "application/vnd.google-apps.document")) },
        ];
        let bytes = descriptor_bytes(&items);
        let one = size_of::<FILEDESCRIPTORW>();
        assert_eq!(bytes.len(), 4 + 3 * one);
        assert_eq!(u32::from_le_bytes(bytes[..4].try_into().unwrap()), 3);
        // SAFETY: reads back the structs just written.
        let at = |i: usize| unsafe { std::ptr::read_unaligned(bytes[4 + i * one..].as_ptr().cast::<FILEDESCRIPTORW>()) };
        let (folder, photo, doc) = (at(0), at(1), at(2));
        assert_eq!({ folder.dwFileAttributes }, 0x10);
        assert_eq!(String::from_utf16_lossy(&{ photo.cFileName }[..15]), "Fotos\\praia.jpg");
        assert_eq!(({ photo.nFileSizeHigh } as u64) << 32 | { photo.nFileSizeLow } as u64, 5_000_000_000);
        assert_ne!({ photo.dwFlags } & FD_FILESIZE.0 as u32, 0);
        // An export's size isn't known: the target reads the stream to its end.
        assert_eq!({ doc.dwFlags } & FD_FILESIZE.0 as u32, 0);
    }

    /// What Explorer does after a drop, against the first connected account: asks the data
    /// object for the description of a small real folder and reads every file's stream to its
    /// end, checking the bytes against Drive's sizes. Read-only for the account.
    /// `cargo test -p openexplorer real_drag -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_drag_streams_whole_files() {
        use openexplorer_gdrive::{ClientConfig, store};
        let cfg = ClientConfig::load().unwrap();
        let account = store::accounts().into_iter().next().expect("uma conta conectada");
        let d = Arc::new(Drive::new(cfg, store::refresh_token(&account.email).unwrap()));
        let mut root = Vec::new();
        d.list("root", None, |page, _| {
            root.extend(page);
            true
        })
        .unwrap();
        // A folder of a few small ordinary files, and one Google Doc if there is one.
        let pick = root.iter().filter(|f| f.is_dir && f.target.is_none()).take(40).find(|folder| {
            let mut inside = Vec::new();
            d.list(&folder.id, None, |page, _| {
                inside.extend(page);
                true
            })
            .unwrap();
            !inside.is_empty() && inside.len() <= 20 && inside.iter().all(|f| !f.is_dir && f.size.unwrap_or(0) < 20 << 20)
        });
        let mut top: Vec<DriveFile> = pick.cloned().into_iter().collect();
        top.extend(root.iter().find(|f| f.mime == "application/vnd.google-apps.document").cloned());
        assert!(!top.is_empty(), "nada pequeno para testar na raiz");
        let data: IDataObject = Data::new(Arc::new(DriveSource { drive: d.clone(), files: top })).into();
        let fmt = |cf: u16, tymed: u32, lindex: i32| FORMATETC { cfFormat: cf, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex, tymed };
        // SAFETY: COM calls on the object just made; media released after use.
        unsafe {
            let mut desc = data.GetData(&fmt(descriptor_format(), TYMED_HGLOBAL.0 as u32, -1)).unwrap();
            let h = desc.u.hGlobal;
            let bytes = std::slice::from_raw_parts(GlobalLock(h).cast::<u8>(), GlobalSize(h)).to_vec();
            let _ = GlobalUnlock(h);
            ReleaseStgMedium(&mut desc);
            let n = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
            let one = size_of::<FILEDESCRIPTORW>();
            for i in 0..n {
                let fd = std::ptr::read_unaligned(bytes[4 + i * one..].as_ptr().cast::<FILEDESCRIPTORW>());
                let name = { fd.cFileName };
                let name = String::from_utf16_lossy(&name[..name.iter().position(|c| *c == 0).unwrap_or(260)]);
                if { fd.dwFileAttributes } & 0x10 != 0 {
                    println!("pasta {name}");
                    continue;
                }
                let mut m = data.GetData(&fmt(contents_format(), TYMED_ISTREAM.0 as u32, i as i32)).unwrap();
                let stream = (*m.u.pstm).clone().unwrap();
                let mut total = 0u64;
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let mut got = 0u32;
                    let hr = stream.Read(buf.as_mut_ptr().cast(), buf.len() as u32, Some(&mut got));
                    assert!(hr.is_ok(), "{name}: {hr:?}");
                    total += got as u64;
                    if got == 0 {
                        break;
                    }
                }
                ReleaseStgMedium(&mut m);
                let declared = ({ fd.dwFlags } & FD_FILESIZE.0 as u32 != 0).then_some(({ fd.nFileSizeHigh } as u64) << 32 | { fd.nFileSizeLow } as u64);
                println!("{name}: {total} bytes (descrito: {declared:?})");
                if let Some(size) = declared {
                    assert_eq!(total, size, "{name}");
                } else {
                    assert!(total > 0, "{name}: exportação vazia");
                }
            }
        }
    }

    /// Times what Explorer does with a big tree: the description, then every file read in
    /// order. Takes the folder named in `OE_DRAG_FOLDER` at the root of the first account's My
    /// Drive. Read-only for the account.
    /// `OE_DRAG_FOLDER=ps5-probes cargo test -p openexplorer --release drag_tree_timing -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn drag_tree_timing() {
        use openexplorer_gdrive::{ClientConfig, store};
        let Ok(name) = std::env::var("OE_DRAG_FOLDER") else { return };
        let cfg = ClientConfig::load().unwrap();
        let account = store::accounts().into_iter().next().expect("uma conta conectada");
        let d = Arc::new(Drive::new(cfg, store::refresh_token(&account.email).unwrap()));
        let mut root = Vec::new();
        d.list("root", None, |page, _| {
            root.extend(page);
            true
        })
        .unwrap();
        let Some(folder) = root.into_iter().find(|f| f.is_dir && f.name == name) else {
            println!("sem a pasta {name} na raiz");
            return;
        };
        let t = std::time::Instant::now();
        let data: IDataObject = Data::new(Arc::new(DriveSource { drive: d.clone(), files: vec![folder] })).into();
        let fmt = |cf: u16, tymed: u32, lindex: i32| FORMATETC { cfFormat: cf, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex, tymed };
        // SAFETY: COM calls on the object just made; media released after use.
        unsafe {
            let mut desc = data.GetData(&fmt(descriptor_format(), TYMED_HGLOBAL.0 as u32, -1)).unwrap();
            let h = desc.u.hGlobal;
            let bytes = std::slice::from_raw_parts(GlobalLock(h).cast::<u8>(), GlobalSize(h)).to_vec();
            let _ = GlobalUnlock(h);
            ReleaseStgMedium(&mut desc);
            let n = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
            let listed = t.elapsed().as_secs_f64();
            let one = size_of::<FILEDESCRIPTORW>();
            let (mut files, mut total) = (0, 0u64);
            let mut buf = vec![0u8; 256 * 1024];
            for i in 0..n {
                let fd = std::ptr::read_unaligned(bytes[4 + i * one..].as_ptr().cast::<FILEDESCRIPTORW>());
                if { fd.dwFileAttributes } & 0x10 != 0 {
                    continue;
                }
                let mut m = data.GetData(&fmt(contents_format(), TYMED_ISTREAM.0 as u32, i as i32)).unwrap();
                let stream = (*m.u.pstm).clone().unwrap();
                loop {
                    let mut got = 0u32;
                    assert!(stream.Read(buf.as_mut_ptr().cast(), buf.len() as u32, Some(&mut got)).is_ok());
                    total += got as u64;
                    if got == 0 {
                        break;
                    }
                }
                drop(stream);
                ReleaseStgMedium(&mut m);
                files += 1;
            }
            let secs = t.elapsed().as_secs_f64();
            println!("{n} itens descritos em {listed:.1} s; {files} arquivos, {total} bytes lidos em {secs:.1} s ({:.1} arquivos/s)", files as f64 / (secs - listed).max(0.001));
        }
    }
}
