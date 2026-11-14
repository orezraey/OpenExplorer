//! Linux: the index as the user sees the disks. There is no MFT to read: the folders are listed
//! (in parallel, as the user, so only what the user can read), each one watched with inotify
//! before it's listed, so what changes while the service runs is applied as it happens. What
//! changes while it's stopped isn't recorded anywhere, so each start lists everything again; the
//! index saved at the last run answers meanwhile.
//!
//! The service is the user's own (no root): started by the app when it isn't running, or by
//! systemd at login once `openexplorer-index install` set that up. It listens on a Unix socket
//! in the user's runtime folder, which only the user can open.
//!
//! inotify needs one watch per folder, and the kernel caps them (`max_user_watches`, which grows
//! with the memory). Past the cap the remaining folders are listed but not watched, and the whole
//! index is listed again every [`REWALK_UNWATCHED`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::CString;
use std::io::{BufReader, BufWriter};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::index::{Index, Vol};
use crate::proto::{self, Answer, Request};
use crate::tree::{self, ROOT, Tree};

/// Folders listed at once.
const WALKERS: usize = 16;
/// Connections answered at once.
const MAX_CLIENTS: usize = 32;
/// How long a connection may take to send its request, and to take in the answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);
/// With folders left unwatched (the kernel's cap), how often everything is listed again.
const REWALK_UNWATCHED: Duration = Duration::from_secs(20 * 60);
/// How often the index is saved, when it changed.
const SAVE_EVERY: Duration = Duration::from_secs(10 * 60);
/// The systemd user unit `install` writes.
const UNIT: &str = "openexplorer-index.service";
const EXE: &str = "openexplorer-index";

const MASK: u32 = libc::IN_CREATE | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE | libc::IN_ATTRIB | libc::IN_ONLYDIR | libc::IN_DONT_FOLLOW | libc::IN_EXCL_UNLINK;

/// File systems not indexed: the kernel's own, memory, network and packaged images, as Windows
/// leaves out network drives.
const SKIPPED_FS: &[&str] = &[
    "proc", "sysfs", "devtmpfs", "devpts", "tmpfs", "ramfs", "cgroup", "cgroup2", "securityfs", "debugfs", "tracefs", "configfs", "fusectl", "mqueue", "hugetlbfs", "bpf", "pstore", "autofs", "binfmt_misc",
    "efivarfs", "nsfs", "rpc_pipefs", "squashfs", "overlay", "nfs", "nfs4", "cifs", "smb3", "smbfs", "9p", "drvfs", "virtiofs", "afs", "ceph", "fuse.sshfs", "fuse.rclone", "fuse.gvfsd-fuse", "fuse.portal",
    "fuse.snapfuse", "fuse.lxcfs", "fuse.kio-fuse",
];

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Where the service listens: the user's runtime folder (`/run/user/<uid>`, only the user's),
/// else a folder of the user's cache made private.
pub fn socket_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_dir()) {
        return dir.join("openexplorer-index.sock");
    }
    cache_dir().join("index.sock")
}

/// `~/.cache/openexplorer`, made (private) the first time it's asked for.
fn cache_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".cache")).join("openexplorer");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        dir
    })
    .clone()
}

fn saved_path() -> PathBuf {
    cache_dir().join("index-linux.bin")
}

// ---------------------------------------------------------------------------------------------
// inotify

struct Inotify(OwnedFd);

impl Inotify {
    fn new() -> std::io::Result<Inotify> {
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor we own.
        Ok(Inotify(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    fn watch(&self, path: &Path) -> std::io::Result<i32> {
        let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        let wd = unsafe { libc::inotify_add_watch(self.0.as_raw_fd(), c.as_ptr(), MASK) };
        if wd < 0 { Err(std::io::Error::last_os_error()) } else { Ok(wd) }
    }

    fn unwatch(&self, wd: i32) {
        unsafe { libc::inotify_rm_watch(self.0.as_raw_fd(), wd) };
    }

    /// Waits up to `ms` for events, then reads what's there.
    fn read(&self, buf: &mut [u8], ms: i32) -> std::io::Result<usize> {
        let mut p = libc::pollfd { fd: self.0.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut p, 1, ms) } <= 0 {
            return Ok(0);
        }
        let n = unsafe { libc::read(self.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            return if e.kind() == std::io::ErrorKind::WouldBlock { Ok(0) } else { Err(e) };
        }
        Ok(n as usize)
    }
}

#[derive(Debug, PartialEq)]
struct Event {
    wd: i32,
    mask: u32,
    cookie: u32,
    name: String,
}

/// The events in what inotify wrote: each a header and a name padded with zeros.
fn parse(buf: &[u8]) -> Vec<Event> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 16 <= buf.len() {
        let u = |at: usize| u32::from_ne_bytes(buf[i + at..i + at + 4].try_into().unwrap());
        let len = u(12) as usize;
        let Some(raw) = buf.get(i + 16..i + 16 + len) else { break };
        let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
        out.push(Event { wd: u(0) as i32, mask: u(4), cookie: u(8), name: String::from_utf8_lossy(&raw[..end]).into_owned() });
        i += 16 + len;
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Listing

/// Record numbers: past the highest given, or one given back.
struct Alloc {
    next: u32,
    free: Vec<u32>,
}

impl Alloc {
    fn new() -> Mutex<Alloc> {
        Mutex::new(Alloc { next: ROOT + 1, free: Vec::new() })
    }
}

fn take(a: &Mutex<Alloc>) -> u32 {
    let mut a = a.lock().unwrap();
    a.free.pop().unwrap_or_else(|| {
        a.next += 1;
        a.next - 1
    })
}

struct Item {
    record: u32,
    parent: u32,
    depth: u32,
    name: String,
    flags: u8,
    size: u64,
    modified: u32,
}

fn seconds(m: &std::fs::Metadata) -> u32 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs().min(u32::MAX as u64) as u32)
}

fn flags_of(name: &str, is_dir: bool) -> u8 {
    let mut f = if is_dir { tree::DIR } else { 0 };
    if name.starts_with('.') {
        f |= tree::HIDDEN;
    }
    f
}

/// What a listing found: every entry under the folders given, outermost first, and the watch of
/// each folder listed.
struct Listed {
    items: Vec<Item>,
    watches: Vec<(i32, u32)>,
}

/// Lists the folders `dirs` (each a path and its record) and everything under them, [`WALKERS`]
/// at a time, watching each folder before listing it so nothing made meanwhile is missed. Folders
/// in `skip` (mount points of skipped file systems) are left out; links aren't followed.
fn list(dirs: Vec<(PathBuf, u32, u32)>, alloc: &Mutex<Alloc>, ino: &Inotify, skip: &HashSet<PathBuf>, full: &AtomicBool) -> Listed {
    let queue = Mutex::new((VecDeque::from(dirs), 0usize));
    let ready = Condvar::new();
    let found = Mutex::new(Listed { items: Vec::new(), watches: Vec::new() });
    std::thread::scope(|s| {
        for _ in 0..WALKERS {
            s.spawn(|| {
                let (mut items, mut watches) = (Vec::new(), Vec::new());
                loop {
                    let (dir, record, depth) = {
                        let mut q = queue.lock().unwrap();
                        loop {
                            if let Some(d) = q.0.pop_front() {
                                q.1 += 1;
                                break d;
                            }
                            if q.1 == 0 {
                                ready.notify_all();
                                let mut f = found.lock().unwrap();
                                f.items.append(&mut items);
                                f.watches.append(&mut watches);
                                return;
                            }
                            q = ready.wait(q).unwrap();
                        }
                    };
                    if !full.load(Ordering::Relaxed) {
                        match ino.watch(&dir) {
                            Ok(wd) => watches.push((wd, record)),
                            Err(e) if e.raw_os_error() == Some(libc::ENOSPC) => full.store(true, Ordering::Relaxed),
                            Err(_) => {}
                        }
                    }
                    let mut sub = Vec::new();
                    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                        let Ok(meta) = e.metadata() else { continue };
                        let name = e.file_name().to_string_lossy().into_owned();
                        let is_dir = meta.is_dir();
                        let child = take(alloc);
                        if is_dir {
                            let path = e.path();
                            if !skip.contains(&path) {
                                sub.push((path, child, depth + 1));
                            }
                        }
                        items.push(Item { record: child, parent: record, depth: depth + 1, flags: flags_of(&name, is_dir), size: if is_dir { 0 } else { meta.len() }, modified: seconds(&meta), name });
                    }
                    let mut q = queue.lock().unwrap();
                    q.0.extend(sub);
                    q.1 -= 1;
                    drop(q);
                    ready.notify_all();
                }
            });
        }
    });
    let mut out = found.into_inner().unwrap();
    out.items.sort_unstable_by_key(|i| (i.depth, i.record));
    out
}

/// The mount points of file systems not indexed ([`SKIPPED_FS`]), from `/proc/self/mountinfo`.
fn skipped_mounts() -> HashSet<PathBuf> {
    let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    mounts_to_skip(&info)
}

fn mounts_to_skip(info: &str) -> HashSet<PathBuf> {
    // "36 35 98:0 /mnt1 /mnt/parent rw,noatime master:1 - ext3 /dev/root rw,errors=continue":
    // the mount point is the 5th field, the type the first after " - ".
    let unescape = |s: &str| s.replace("\\040", " ").replace("\\011", "\t").replace("\\012", "\n").replace("\\134", "\\");
    info.lines()
        .filter_map(|l| {
            let (left, right) = l.split_once(" - ")?;
            let point = left.split(' ').nth(4)?;
            let fs = right.split(' ').next()?;
            (point != "/" && SKIPPED_FS.contains(&fs)).then(|| PathBuf::from(unescape(point)))
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Following

/// The watches of one index, and what they say.
struct Live {
    ino: Arc<Inotify>,
    wds: HashMap<i32, u32>,
    by_record: HashMap<u32, i32>,
    alloc: Mutex<Alloc>,
    /// The kernel's cap on watches was reached.
    full: AtomicBool,
    stop: Arc<AtomicBool>,
    events: Receiver<Vec<Event>>,
}

impl Live {
    fn new() -> std::io::Result<Live> {
        let ino = Arc::new(Inotify::new()?);
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, events) = channel();
        // Read on a thread of its own, from the first watch on, so a long listing doesn't leave
        // the kernel's queue to overflow.
        let (reader, quit) = (ino.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 256 << 10];
            while !quit.load(Ordering::Relaxed) {
                match reader.read(&mut buf, 500) {
                    Ok(0) => {}
                    Ok(n) => {
                        if tx.send(parse(&buf[..n])).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        Ok(Live { ino, wds: HashMap::new(), by_record: HashMap::new(), alloc: Alloc::new(), full: AtomicBool::new(false), stop, events })
    }

    fn keep(&mut self, watches: Vec<(i32, u32)>, remap: &HashMap<u32, u32>) {
        for (wd, r) in watches {
            let r = remap.get(&r).copied().unwrap_or(r);
            self.wds.insert(wd, r);
            self.by_record.insert(r, wd);
        }
    }

    /// Lets go of records taken out of the tree: their watches and their numbers.
    fn forget(&mut self, records: Vec<u32>) {
        let mut a = self.alloc.lock().unwrap();
        for r in records {
            if let Some(wd) = self.by_record.remove(&r) {
                self.wds.remove(&wd);
                self.ino.unwatch(wd);
            }
            a.free.push(r);
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Lists everything under `/` into a new tree, watching each folder.
fn build(skip: &HashSet<PathBuf>) -> std::io::Result<(Tree, Live)> {
    let live = Live::new()?;
    let listed = list(vec![(PathBuf::from("/"), ROOT, 0)], &live.alloc, &live.ino, skip, &live.full);
    let mut t = Tree::new("/");
    let records = live.alloc.lock().unwrap().next as usize;
    t.reserve(records, listed.items.iter().map(|i| i.name.len()).sum());
    for i in &listed.items {
        t.set(i.record, i.parent, &i.name, i.flags, i.size, i.modified);
    }
    t.link();
    t.shrink();
    let mut live = live;
    live.keep(listed.watches, &HashMap::new());
    Ok((t, live))
}

/// Puts what [`list`] found under folders already in the tree. A name the tree already holds
/// (an event got there first) keeps its record, and what the listing found under it goes there.
fn absorb(t: &mut Tree, live: &mut Live, listed: Listed) {
    let mut remap: HashMap<u32, u32> = HashMap::new();
    let mut spare = Vec::new();
    for i in listed.items {
        let parent = remap.get(&i.parent).copied().unwrap_or(i.parent);
        if !t.is_dir(parent) {
            spare.push(i.record);
            continue;
        }
        let record = match t.child(parent, &i.name) {
            Some(r) if r != i.record => {
                remap.insert(i.record, r);
                spare.push(i.record);
                r
            }
            _ => i.record,
        };
        t.set(record, parent, &i.name, i.flags, i.size, i.modified);
    }
    live.alloc.lock().unwrap().free.extend(spare);
    live.keep(listed.watches, &remap);
}

/// What applying a batch of events left to do.
enum After {
    Fine,
    /// The kernel's queue overflowed: changes were lost, list everything again.
    Lost,
}

/// Applies inotify's events to the tree: names come and go, are moved (a pair of events with
/// the same cookie), and files change size or date. A folder that appears is listed, after the
/// tree is let go of.
fn apply(vol: &Vol, live: &mut Live, skip: &HashSet<PathBuf>, events: Vec<Event>) -> After {
    let mut new_dirs = Vec::new();
    {
        let mut t = vol.tree.write().unwrap();
        // Moved away and not (yet) seen arriving: removed at the end if they don't.
        let mut moving: HashMap<u32, u32> = HashMap::new();
        for ev in events {
            if ev.mask & libc::IN_Q_OVERFLOW != 0 {
                return After::Lost;
            }
            if ev.mask & libc::IN_IGNORED != 0 {
                if let Some(r) = live.wds.remove(&ev.wd) {
                    live.by_record.remove(&r);
                }
                continue;
            }
            let Some(&dir) = live.wds.get(&ev.wd) else { continue };
            if ev.name.is_empty() || !t.is_dir(dir) {
                continue;
            }
            let here = t.child(dir, &ev.name);
            if ev.mask & libc::IN_DELETE != 0 {
                if let Some(r) = here {
                    let gone = t.remove_tree(r);
                    live.forget(gone);
                }
            } else if ev.mask & libc::IN_MOVED_FROM != 0 {
                if let Some(r) = here {
                    moving.insert(ev.cookie, r);
                }
            } else if ev.mask & libc::IN_MOVED_TO != 0
                && let Some(r) = moving.remove(&ev.cookie).filter(|r| t.contains(*r))
            {
                // Moved over another file, which goes.
                if let Some(old) = here.filter(|o| *o != r) {
                    let gone = t.remove_tree(old);
                    live.forget(gone);
                }
                let (size, modified) = t.meta(r).unwrap_or((0, 0));
                let is_dir = t.is_dir(r);
                t.set(r, dir, &ev.name, flags_of(&ev.name, is_dir), size, modified);
            } else if ev.mask & (libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE | libc::IN_ATTRIB) != 0 {
                let Some(path) = t.path(dir).map(|p| Path::new(&p).join(&ev.name)) else { continue };
                let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
                let record = here.unwrap_or_else(|| take(&live.alloc));
                let is_dir = meta.is_dir();
                t.set(record, dir, &ev.name, flags_of(&ev.name, is_dir), if is_dir { 0 } else { meta.len() }, seconds(&meta));
                // A folder made or moved in: what it holds is listed (and watched).
                if is_dir && here.is_none() && !skip.contains(&path) {
                    new_dirs.push((path, record, 0));
                }
            }
        }
        for (_, r) in moving {
            let gone = t.remove_tree(r);
            live.forget(gone);
        }
        t.maybe_compact();
    }
    if !new_dirs.is_empty() {
        let listed = list(new_dirs, &live.alloc, &live.ino, skip, &live.full);
        let mut t = vol.tree.write().unwrap();
        absorb(&mut t, live, listed);
    }
    After::Fine
}

fn save(vol: &Vol) {
    let path = saved_path();
    let tmp = path.with_extension("tmp");
    let written = std::fs::File::create(&tmp).and_then(|f| vol.tree.read().unwrap().save(&mut BufWriter::with_capacity(1 << 20, f)));
    if written.is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Builds and follows the index of `/` into `vol`, for as long as the service runs.
pub fn follow(vol: Arc<Vol>) {
    let skip = skipped_mounts();
    // The last run's index answers while the disks are listed again.
    // It's only searched: changes go to the tree the walk builds, so it needs no child links.
    if let Some(t) = std::fs::File::open(saved_path()).ok().and_then(|f| Tree::load(&mut BufReader::with_capacity(1 << 20, f)).ok()) {
        vol.loaded(t);
    }
    loop {
        let t0 = Instant::now();
        let (tree, mut live) = match build(&skip) {
            Ok(b) => b,
            Err(e) => {
                vol.failed(format!("inotify: {e}"));
                std::thread::sleep(Duration::from_secs(60));
                continue;
            }
        };
        vol.ready(tree, t0.elapsed().as_millis() as u64);
        if live.full.load(Ordering::Relaxed) {
            vol.note(Some("Pastas demais para o inotify vigiar: mudanças em parte delas aparecem só na próxima releitura.".into()));
        }
        save(&vol);
        let (mut saved, mut dirty) = (Instant::now(), false);
        loop {
            let first = match live.events.recv_timeout(Duration::from_secs(30)) {
                Ok(e) => e,
                Err(RecvTimeoutError::Timeout) => {
                    if dirty && saved.elapsed() > SAVE_EVERY {
                        save(&vol);
                        (saved, dirty) = (Instant::now(), false);
                    }
                    if live.full.load(Ordering::Relaxed) && t0.elapsed() > REWALK_UNWATCHED {
                        break;
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            };
            // Whatever else is waiting goes in the same batch, so a move's two halves meet.
            let mut events = first;
            while let Ok(more) = live.events.try_recv() {
                events.extend(more);
            }
            dirty = true;
            if let After::Lost = apply(&vol, &mut live, &skip, events) {
                break;
            }
        }
        vol.reindexing();
    }
}

// ---------------------------------------------------------------------------------------------
// Serving, asking, installing

/// Answers searches from `index` on [`socket_path`]; fails when another copy already does.
pub fn serve(index: Arc<Index>) -> Result<(), String> {
    let path = socket_path();
    if UnixStream::connect(&path).is_ok() {
        return Err("O serviço de pesquisa já está rodando.".into());
    }
    let _ = std::fs::remove_file(&path);
    // Made private before anyone can connect: the umask covers the moment between bind and chmod.
    let old = unsafe { libc::umask(0o177) };
    let listener = UnixListener::bind(&path);
    unsafe { libc::umask(old) };
    let listener = listener.map_err(|e| format!("Abrir {}: {e}", path.display()))?;
    let clients = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if clients.load(Ordering::Relaxed) >= MAX_CLIENTS {
            continue;
        }
        clients.fetch_add(1, Ordering::Relaxed);
        let (index, clients) = (index.clone(), clients.clone());
        std::thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
            let _ = stream.set_write_timeout(Some(ANSWER_TIMEOUT));
            let answer = match proto::read_request(&mut BufReader::new(&stream)) {
                Ok(req) => index.answer(&req),
                Err(e) => Answer { header: proto::Header { error: Some(e), ..Default::default() }, rows: Vec::new() },
            };
            let _ = proto::write_answer(&mut BufWriter::with_capacity(1 << 16, &stream), &answer);
            clients.fetch_sub(1, Ordering::Relaxed);
        });
    }
    Ok(())
}

/// Holds the user's one service: a lock on a file in the cache folder, kept while the process
/// runs. Several searches can start the service at once (typing on first use); the copies that
/// come second end here, before touching the socket.
fn only_copy() -> Result<std::fs::File, String> {
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(cache_dir().join("index.lock")).map_err(|e| format!("Trava do serviço: {e}"))?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("O serviço de pesquisa já está rodando.".into());
    }
    Ok(f)
}

/// Lists `/` and serves searches, until stopped.
pub fn run() -> Result<(), String> {
    let _only = only_copy()?;
    let vol = Vol::new("/");
    let follower = vol.clone();
    std::thread::spawn(move || follow(follower));
    serve(Arc::new(Index::new(vec![vol])))
}

/// Asks the service, starting it first when it isn't running.
pub fn ask(req: &Request) -> Result<Answer, String> {
    let stream = match UnixStream::connect(socket_path()) {
        Ok(s) => s,
        Err(_) => {
            start()?;
            let t = Instant::now();
            loop {
                match UnixStream::connect(socket_path()) {
                    Ok(s) => break s,
                    Err(_) if t.elapsed() < Duration::from_secs(3) => std::thread::sleep(Duration::from_millis(50)),
                    Err(e) => return Err(format!("O serviço de pesquisa não respondeu ({e}).")),
                }
            }
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    proto::write_request(&mut &stream, req).map_err(|e| format!("Falar com o serviço: {e}"))?;
    proto::read_answer(&mut BufReader::with_capacity(1 << 16, &stream))
}

/// Where `install` copies the service: `~/.local/lib/openexplorer`.
fn installed_exe() -> PathBuf {
    home().join(".local/lib/openexplorer").join(EXE)
}

fn unit_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config")).join("systemd/user").join(UNIT)
}

fn systemctl(args: &[&str]) -> bool {
    std::process::Command::new("systemctl").arg("--user").args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
}

/// Starts the service: through systemd when installed there, else as a process of its own that
/// outlives the app.
pub fn start() -> Result<(), String> {
    if unit_path().exists() && systemctl(&["start", UNIT]) {
        return Ok(());
    }
    // Next to the app, the folder above (cargo's examples), or the copy `install` made.
    let here = std::env::current_exe().map_err(|e| e.to_string())?;
    let candidates = [here.with_file_name(EXE), here.parent().and_then(Path::parent).map_or_else(PathBuf::new, |d| d.join(EXE)), installed_exe()];
    let exe = candidates.iter().find(|p| p.is_file()).ok_or_else(|| format!("{EXE} não foi encontrado ao lado do Open Explorer."))?;
    std::process::Command::new(exe)
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Iniciar {}: {e}", exe.display()))
}

/// Whether the service answers.
pub fn running() -> bool {
    UnixStream::connect(socket_path()).is_ok()
}

/// Makes systemd start the service at login: this program copied to `~/.local/lib/openexplorer`
/// and a user unit for it. No root needed.
pub fn install() -> Result<(), String> {
    let dst = installed_exe();
    let dir = dst.parent().unwrap();
    std::fs::create_dir_all(dir).map_err(|e| format!("Criar {}: {e}", dir.display()))?;
    let src = std::env::current_exe().map_err(|e| e.to_string())?;
    if src != dst {
        // Copied to a new name and renamed over, so a running copy keeps its file.
        let tmp = dst.with_extension("new");
        std::fs::copy(&src, &tmp).and_then(|_| std::fs::rename(&tmp, &dst)).map_err(|e| format!("Copiar o serviço: {e}"))?;
    }
    let unit = unit_path();
    std::fs::create_dir_all(unit.parent().unwrap()).map_err(|e| e.to_string())?;
    let text = format!(
        "[Unit]\nDescription=Open Explorer — índice de arquivos\n\n[Service]\nExecStart=\"{}\" serve\nRestart=on-failure\nNice=10\nIOSchedulingClass=idle\n\n[Install]\nWantedBy=default.target\n",
        dst.display()
    );
    std::fs::write(&unit, text).map_err(|e| format!("Escrever {}: {e}", unit.display()))?;
    if !systemctl(&["daemon-reload"]) || !systemctl(&["enable", "--now", UNIT]) {
        return Err("O systemd do usuário não respondeu: o serviço foi copiado, mas não inicia com o sistema. O app o inicia quando pesquisar.".into());
    }
    Ok(())
}

pub fn uninstall() -> Result<(), String> {
    let _ = systemctl(&["disable", "--now", UNIT]);
    let _ = std::fs::remove_file(unit_path());
    let _ = systemctl(&["daemon-reload"]);
    let _ = std::fs::remove_file(saved_path());
    let exe = installed_exe();
    let _ = std::fs::remove_file(&exe);
    let _ = std::fs::remove_dir(exe.parent().unwrap());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_parse_from_what_inotify_writes() {
        let mut buf = Vec::new();
        for (wd, mask, cookie, name) in [(1i32, libc::IN_CREATE, 0u32, "novo.txt"), (2, libc::IN_MOVED_FROM, 7, "a b"), (2, libc::IN_IGNORED, 0, "")] {
            let padded = if name.is_empty() { 0 } else { (name.len() / 16 + 1) * 16 };
            buf.extend_from_slice(&wd.to_ne_bytes());
            buf.extend_from_slice(&mask.to_ne_bytes());
            buf.extend_from_slice(&cookie.to_ne_bytes());
            buf.extend_from_slice(&(padded as u32).to_ne_bytes());
            let mut n = name.as_bytes().to_vec();
            n.resize(padded, 0);
            buf.extend_from_slice(&n);
        }
        let evs = parse(&buf);
        assert_eq!(evs.len(), 3);
        assert_eq!((evs[0].wd, evs[0].name.as_str()), (1, "novo.txt"));
        assert_eq!((evs[1].cookie, evs[1].name.as_str()), (7, "a b"));
        assert_eq!(evs[2].name, "");
        // A cut buffer stops at the last whole event.
        assert_eq!(parse(&buf[..20]).len(), 0);
    }

    #[test]
    fn kernel_network_and_image_mounts_are_skipped() {
        let info = "\
21 1 0:20 / /proc rw - proc proc rw
25 1 8:1 / / rw - ext4 /dev/sda1 rw
26 25 8:2 / /home rw - ext4 /dev/sda2 rw
27 25 0:30 / /mnt/Meus\\040Arquivos rw - cifs //nas/x rw
28 25 7:0 / /snap/core/1 ro - squashfs /dev/loop0 ro
29 25 0:40 / /tmp rw - tmpfs tmpfs rw
";
        let skip = mounts_to_skip(info);
        for p in ["/proc", "/mnt/Meus Arquivos", "/snap/core/1", "/tmp"] {
            assert!(skip.contains(Path::new(p)), "{p}");
        }
        assert!(!skip.contains(Path::new("/home")) && !skip.contains(Path::new("/")));
    }

    /// The whole loop on a scratch folder: listed, then files made, moved, renamed and
    /// deleted, a folder made with files in it, each seen through inotify.
    #[test]
    fn changes_under_a_watched_folder_reach_the_index() {
        let root = std::env::temp_dir().join(format!("oe-unix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/relatorio.txt"), "x").unwrap();
        let mut live = Live::new().unwrap();
        let listed = list(vec![(root.clone(), ROOT, 0)], &live.alloc, &live.ino, &HashSet::new(), &live.full);
        let mut t = Tree::new(&format!("{}/", root.display()));
        for i in &listed.items {
            t.set(i.record, i.parent, &i.name, i.flags, i.size, i.modified);
        }
        t.link();
        live.keep(listed.watches, &HashMap::new());
        let vol = Vol::new("/");
        vol.ready(t, 0);
        // Every path the index holds, under the scratch folder.
        let names = |vol: &Vol| {
            let t = vol.tree.read().unwrap();
            let mut v: Vec<String> = (0..t.records() as u32).filter_map(|r| t.path(r)).filter(|p| p.len() > root.to_str().unwrap().len() + 1).map(|p| p[root.to_str().unwrap().len()..].to_string()).collect();
            v.sort();
            v
        };
        let settle = |live: &mut Live, vol: &Vol| {
            let t = Instant::now();
            while t.elapsed() < Duration::from_millis(600) {
                if let Ok(evs) = live.events.recv_timeout(Duration::from_millis(100)) {
                    let mut all = evs;
                    while let Ok(more) = live.events.try_recv() {
                        all.extend(more);
                    }
                    let _ = apply(vol, live, &HashSet::new(), all);
                }
            }
        };
        assert_eq!(names(&vol), ["/docs", "/docs/relatorio.txt"]);
        std::fs::write(root.join("docs/nota.txt"), "abc").unwrap();
        std::fs::rename(root.join("docs/relatorio.txt"), root.join("relatorio-final.txt")).unwrap();
        std::fs::create_dir_all(root.join("fotos/2026")).unwrap();
        std::fs::write(root.join("fotos/2026/viagem.txt"), "").unwrap();
        settle(&mut live, &vol);
        assert_eq!(names(&vol), ["/docs", "/docs/nota.txt", "/fotos", "/fotos/2026", "/fotos/2026/viagem.txt", "/relatorio-final.txt"]);
        let t = vol.tree.read().unwrap();
        let nota = t.find(&format!("{}/docs", root.display())).and_then(|d| t.child(d, "nota.txt")).unwrap();
        assert_eq!(t.meta(nota).map(|m| m.0), Some(3));
        drop(t);
        std::fs::rename(root.join("fotos"), root.join("docs/fotos")).unwrap();
        std::fs::remove_file(root.join("docs/nota.txt")).unwrap();
        settle(&mut live, &vol);
        assert_eq!(names(&vol), ["/docs", "/docs/fotos", "/docs/fotos/2026", "/docs/fotos/2026/viagem.txt", "/relatorio-final.txt"]);
        std::fs::remove_dir_all(root.join("docs")).unwrap();
        settle(&mut live, &vol);
        assert_eq!(names(&vol), ["/relatorio-final.txt"]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
