//! FTP, SFTP and WebDAV servers in the panes. Each server the user added shows in the tree under
//! Servidores and opens like a folder: listing, opening a file, copying to and from the
//! computer, moving within the server, renaming, new folders, deleting and searching. Every call
//! runs on a background thread with a connection from the pool.
//!
//! A server has no recycle bin: Excluir deletes for good, after asking.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use openexplorer_remote::{self as remote, Conn, Item, Pool, Protocol, Refusal, Rejected, Server, Store};
use rune::TextEdit;

use crate::actions::{Dialog, Then};
use crate::model::{Entry, Location};
use crate::ops::{self, CANCELLED, Progress};
use crate::transfers::{self, End, Op};
use crate::{Msg, Part, State, background, catch_panic};

/// The servers added, and their idle connections.
pub struct Servers {
    pub list: Vec<Server>,
    pub pool: Arc<Pool>,
    pub store: Store,
    /// Counts the add dialog's connection checks, to drop a stale answer.
    pub attempts: u64,
}

impl Servers {
    pub fn load() -> Servers {
        let store = Store::at(openexplorer_gdrive::config_dir().join("servers.json"));
        Servers { list: store.servers(), pool: Arc::default(), store, attempts: 0 }
    }
}

pub fn entry(item: Item) -> Entry {
    Entry { id: item.name.clone(), name: item.name, is_dir: item.is_dir, size: item.size, modified: item.modified, target: None, mime: None, md5: None, starred: false }
}

/// A server's folder: the location at `path` under its root.
pub fn folder(server: &Server, path: Vec<String>) -> Location {
    Location::Server { server: Box::new(server.clone()), path, find: None }
}

/// The folders from the server's root down to where `e`, listed in `loc`, is: in the folder
/// shown, or for a search's result where it was found.
pub fn segs_of(loc: &Location, e: &Entry) -> Vec<String> {
    let Location::Server { path, find, .. } = loc else { return Vec::new() };
    let mut segs = path.clone();
    match find {
        Some(_) => segs.extend(e.id.split('/').map(str::to_string)),
        None => segs.push(e.name.clone()),
    }
    segs
}

/// Where `e`, listed in `loc`, is on its server.
pub fn path_of(loc: &Location, e: &Entry) -> Option<String> {
    let Location::Server { server, .. } = loc else { return None };
    Some(server.path(&segs_of(loc, e)))
}

/// The server and the folder a server location shows, to put things in: not a search's results.
pub fn target(loc: &Location) -> Option<(&Server, String)> {
    match loc {
        Location::Server { server, path, find: None } => Some((server, server.path(path))),
        _ => None,
    }
}

/// The location a row of the queue names: "Servidor\Pasta\item [sftp://…]".
pub fn end(loc: &Location, name: Option<&str>) -> End {
    let Location::Server { server, path, .. } = loc else { return End { text: String::new(), disk: None } };
    let parts: Vec<&str> = std::iter::once(server.name.as_str()).chain(path.iter().map(String::as_str)).chain(name).collect();
    End { text: format!("{} [{}]", parts.join("\\"), server.address()), disk: None }
}

/// Runs `work` with a connection to `server`, given back to the pool afterwards.
pub fn with<T>(pool: &Pool, server: &Server, work: impl FnOnce(&mut dyn Conn) -> Result<T, String>) -> Result<T, String> {
    // The password is read from the credential store only for a new login.
    let mut conn = pool.take(server, || remote::store::secret(&server.id))?;
    let out = work(&mut *conn);
    conn.done();
    out
}

/// A cancelled transfer says so in the app's words.
fn cancelled(e: String) -> String {
    if e == remote::CANCELLED { CANCELLED.into() } else { e }
}

pub(crate) fn child(dir: &str, name: &str) -> String {
    format!("{}/{name}", dir.trim_end_matches('/'))
}

// ---------------------------------------------------------------------------------------------
// Listing and searching

/// Lists pane `p`'s server folder, or searches it, on a thread; search results come in pages.
pub fn load(s: &mut State, p: usize, request: u64) {
    let Location::Server { server, path, find } = s.panes[p].loc.clone() else { return };
    let pool = s.servers.pool.clone();
    let live = s.live[p].clone();
    let pane = &mut s.panes[p];
    pane.loading = true;
    pane.error = None;
    pane.free = None;
    background(s, move |emit| {
        let listed = |part, result| Msg::Listed { pane: p, request, part, result };
        let dir = server.path(&path);
        let Some(text) = find else {
            let result = catch_panic("listar", || with(&pool, &server, |c| c.list(&dir)).map(|v| v.into_iter().map(entry).collect()));
            return emit(listed(Part::WHOLE, result));
        };
        let alive = || live.load(Ordering::Relaxed) == request;
        let mut first = true;
        let result = catch_panic("pesquisar", || {
            with(&pool, &server, |c| {
                search(c, &dir, &text, &alive, &mut |entries, more| {
                    emit(listed(Part::Page { first, more }, Ok(entries)));
                    first = false;
                })
            })
        });
        if let Err(e) = result {
            emit(listed(Part::Page { first, more: false }, Err(e)));
        }
    });
}

/// Searches `dir` and every folder below it for names holding all of `text`'s words, folder by
/// folder, the nearest first. Each result's id is its path from `dir`.
fn search(c: &mut dyn Conn, dir: &str, text: &str, alive: &dyn Fn() -> bool, page: &mut dyn FnMut(Vec<Entry>, bool)) -> Result<(), String> {
    let words = crate::finder::words(text);
    let mut pager = crate::finder::Pager::new(page);
    let mut todo = VecDeque::from([String::new()]);
    let mut buf = Vec::new();
    while let Some(rel) = todo.pop_front() {
        if !alive() || pager.full() {
            break;
        }
        let items = match c.list(&if rel.is_empty() { dir.to_string() } else { child(dir, &rel) }) {
            Ok(v) => v,
            Err(e) if rel.is_empty() => return Err(e),
            // A folder that can't be read is passed over.
            Err(_) => continue,
        };
        for it in items {
            let id = if rel.is_empty() { it.name.clone() } else { format!("{rel}/{}", it.name) };
            // Links aren't followed: one back up would go round forever.
            if it.is_dir && !it.link {
                todo.push_back(id.clone());
            }
            if crate::finder::matches_in(&words, &it.name, &mut buf) {
                pager.push(Entry { id, ..entry(it) });
            }
        }
    }
    pager.flush(false);
    Ok(())
}

/// The folders in `path` of `server`, for the tree.
pub fn folders(pool: &Pool, server: &Server, path: &[String]) -> Result<Vec<String>, String> {
    let dir = server.path(path);
    with(pool, server, |c| c.list(&dir)).map(|v| v.into_iter().filter(|i| i.is_dir).map(|i| i.name).collect())
}

// ---------------------------------------------------------------------------------------------
// Opening, copying, moving

/// Opens a file of a server: downloaded to a temp folder first, as a row of the queue, and
/// opened when it arrives. A copy there from an earlier Abrir, as big and as new, opens at once.
pub fn open_file(s: &mut State, p: usize, item: &Entry) {
    let loc = s.panes[p].loc.clone();
    let (Location::Server { server, .. }, Some(path)) = (&loc, path_of(&loc, item)) else { return };
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (&server.id, &path).hash(&mut h);
    let dir = std::env::temp_dir().join("OpenExplorer-abrir").join(format!("{:016x}", h.finish()));
    let ready = dir.join(ops::windows_name(&item.name));
    let fresh = std::fs::metadata(&ready).ok().is_some_and(|m| Some(m.len()) == item.size && item.modified.is_some() && m.modified().ok() == item.modified);
    if fresh {
        if let Err(e) = ops::open(&ready) {
            s.status = e;
        }
        return;
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        s.status = format!("Não consegui criar a pasta temporária: {e}");
        return;
    }
    let _ = std::fs::remove_file(&ready);
    download(s, &loc, vec![item.clone()], dir, Then::Open, Vec::new());
}

/// Runs a server job on a thread, then reloads `reload` and says how it went.
fn job(s: &State, server: &Server, reload: Vec<Location>, work: impl FnOnce(&mut dyn Conn) -> Result<String, String> + Send + 'static) {
    let (pool, server) = (s.servers.pool.clone(), server.clone());
    background(s, move |emit| emit(Msg::Done { reload, result: catch_panic("operar no servidor", || with(&pool, &server, work)) }));
}

/// How deep a download goes into folders (links followed, as the copy follows them): a link back
/// up would otherwise go round forever.
const DEEPEST: usize = 64;

/// The bytes the file or folder at `path` holds, as far down as a download goes.
pub(crate) fn measure(c: &mut dyn Conn, path: &str, stop: &dyn Fn() -> bool) -> u64 {
    let mut total = 0;
    let mut todo = vec![(path.to_string(), 0)];
    while let Some((dir, depth)) = todo.pop() {
        if stop() {
            break;
        }
        for it in c.list(&dir).unwrap_or_default() {
            match it.is_dir {
                true if depth < DEEPEST => todo.push((child(&dir, &it.name), depth + 1)),
                true => {}
                false => total += it.size.unwrap_or(0),
            }
        }
    }
    total
}

/// Runs `batch`'s rows on `server`, up to `at_once` at a time, each worker with a connection of
/// its own, one connection left over for the panes' listings; then reloads `reload` and says how
/// it went. `prepare` runs first, on one connection, and gives what the rows share.
#[allow(clippy::too_many_arguments)]
fn transfer<P: Send + Sync + 'static>(
    s: &State,
    server: &Server,
    reload: Vec<Location>,
    at_once: usize,
    batch: transfers::Batch,
    prepare: impl FnOnce(&mut dyn Conn, &transfers::Batch) -> Result<P, String> + Send + 'static,
    step: impl Fn(&mut dyn Conn, &P, usize, &Progress) -> Result<(), String> + Send + Sync + 'static,
) {
    let workers = at_once.min(server.limit().saturating_sub(1)).min(batch.rows().len()).max(1);
    let (pool, server) = (s.servers.pool.clone(), server.clone());
    background(s, move |emit| {
        let result = catch_panic("transferir", || {
            let shared = with(&pool, &server, |c| prepare(c, &batch))?;
            let next = std::sync::atomic::AtomicUsize::new(0);
            let failed = std::sync::Mutex::new(None);
            std::thread::scope(|scope| {
                for _ in 0..workers {
                    scope.spawn(|| {
                        let ran = with(&pool, &server, |c| {
                            while let Some((i, p)) = batch.next(&next) {
                                p.end(step(c, &shared, i, p));
                            }
                            Ok(())
                        });
                        // A worker that couldn't log in leaves its rows to the others.
                        if let Err(e) = ran {
                            failed.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(e);
                        }
                    });
                }
            });
            if let Some(e) = failed.into_inner().unwrap_or_else(|e| e.into_inner()) {
                batch.abandon(&e);
            }
            batch.summary()
        });
        emit(Msg::Done { reload, result });
    });
}

/// Downloads `items` of the server folder `from` into the folder `dst` on the computer, a row
/// each in the queue, several at once; then `then` (Recortar: deleting them from the server).
pub fn download(s: &mut State, from: &Location, items: Vec<Entry>, dst: PathBuf, then: Then, reload: Vec<Location>) {
    let Location::Server { server, .. } = from else { return };
    let paths: Vec<String> = items.iter().filter_map(|e| path_of(from, e)).collect();
    let to = End::local(&dst);
    let batch = transfers::queue(&mut s.transfers, s.waker.clone(), Op::Download, items.iter().map(|e| (end(from, Some(&e.name)), to.clone())));
    let at_once = usize::from(s.settings.downloads);
    let sizes: Vec<(bool, Option<u64>, String)> = items.iter().zip(&paths).map(|(e, p)| (e.is_dir, e.size, p.clone())).collect();
    let prepare = move |c: &mut dyn Conn, batch: &transfers::Batch| {
        for (p, (is_dir, size, path)) in batch.rows().iter().zip(&sizes) {
            p.size(if *is_dir { measure(c, path, &|| p.cancelled()) } else { size.unwrap_or(0) });
        }
        // The names the rows took in `dst`, before their files are made.
        Ok(std::sync::Mutex::new(HashSet::<PathBuf>::new()))
    };
    let step = move |c: &mut dyn Conn, claimed: &std::sync::Mutex<HashSet<PathBuf>>, i: usize, p: &Progress| {
        let e = &items[i];
        let local = {
            let mut claimed = claimed.lock().unwrap_or_else(|e| e.into_inner());
            let local = ops::unique_where(&dst, &ops::windows_name(&e.name), &|p| claimed.contains(p));
            claimed.insert(local.clone());
            local
        };
        if let Err(err) = fetch(c, &paths[i], e.is_dir, e.modified, &local, p, 0) {
            // What arrived of an item cancelled midway is no use.
            if err == CANCELLED {
                let _ = if e.is_dir { std::fs::remove_dir_all(&local) } else { std::fs::remove_file(&local) };
            }
            return Err(err);
        }
        match then {
            Then::Nothing => Ok(()),
            Then::Trash => c.remove(&paths[i], e.is_dir),
            Then::Open => ops::open(&local),
        }
    };
    transfer(s, server, reload, at_once, batch, prepare, step);
}

/// A file or a folder and all it holds, from `path` on the server to `local`, `depth` folders
/// down from where the download started. Two names the server tells apart but Windows doesn't
/// ("Nota" and "nota", "a:b" and "a_b") each get their own file.
fn fetch(c: &mut dyn Conn, path: &str, is_dir: bool, modified: Option<SystemTime>, local: &Path, p: &Progress, depth: usize) -> Result<(), String> {
    if p.cancelled() {
        return Err(CANCELLED.into());
    }
    if is_dir {
        if depth > DEEPEST {
            return Err(format!("{path}: pastas demais umas dentro das outras (um link que volta para cima?)."));
        }
        std::fs::create_dir(local).map_err(|e| format!("Não consegui criar {}: {e}", local.display()))?;
        for it in c.list(path)? {
            let to = ops::unique_in(local, &ops::windows_name(&it.name));
            fetch(c, &child(path, &it.name), it.is_dir, it.modified, &to, p, depth + 1)?;
        }
        return Ok(());
    }
    let mut file = std::fs::File::create_new(local).map_err(|e| format!("Não consegui criar {}: {e}", local.display()))?;
    let mut last = 0;
    let got = c.download(path, &mut file, &mut |n| {
        p.add_bytes(n - last);
        last = n;
        !p.cancelled()
    });
    if let Err(e) = got {
        drop(file);
        let _ = std::fs::remove_file(local);
        return Err(cancelled(e));
    }
    // Dated as on the server, as a copy on the disk is.
    if let Some(m) = modified {
        let _ = file.set_modified(m);
    }
    Ok(())
}

/// Uploads `sources` (files and folders on the computer) into the server folder `to` shows, a
/// row each in the queue, several at once, a taken name getting a free one ("nome (2).ext").
/// With `remove` (Recortar), each original goes to the Recycle Bin once all of it arrived.
pub fn upload(s: &mut State, sources: Vec<PathBuf>, to: &Location, remove: bool, reload: Vec<Location>) {
    let Some((server, dir)) = target(to) else {
        s.status = "Abra uma pasta do servidor para enviar para ela.".into();
        return;
    };
    let server = server.clone();
    let dest = end(to, None);
    let batch = transfers::queue(&mut s.transfers, s.waker.clone(), Op::Upload, sources.iter().map(|p| (End::local(p), dest.clone())));
    let at_once = usize::from(s.settings.uploads);
    let measured = sources.clone();
    let listed = dir.clone();
    let prepare = move |c: &mut dyn Conn, batch: &transfers::Batch| {
        for (p, src) in batch.rows().iter().zip(&measured) {
            p.size(ops::measure(src, &|| p.cancelled()));
        }
        // Lowercased: a server on Windows (or many NAS) takes "Nota.txt" and "nota.txt" for one
        // file, and an upload under the other spelling would replace it.
        let taken: HashSet<String> = c.list(&listed)?.into_iter().map(|i| i.name.to_lowercase()).collect();
        Ok(std::sync::Mutex::new(taken))
    };
    let step = move |c: &mut dyn Conn, taken: &std::sync::Mutex<HashSet<String>>, i: usize, p: &Progress| {
        let src = &sources[i];
        let is_dir = src.is_dir();
        let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let name = {
            let mut taken = taken.lock().unwrap_or_else(|e| e.into_inner());
            let name = free_name(&name, is_dir, &taken);
            taken.insert(name.to_lowercase());
            name
        };
        let target = child(&dir, &name);
        if let Err(e) = send(c, src, &target, p) {
            if e == CANCELLED {
                let _ = c.remove(&target, is_dir);
            }
            return Err(e);
        }
        if remove {
            trash::delete(src).map_err(|e| format!("Enviado, mas não consegui mover o original para a Lixeira: {e}"))?;
        }
        Ok(())
    };
    transfer(s, &server, reload, at_once, batch, prepare, step);
}

/// `name`, or the first "nome (2).ext" whose lowercase isn't in `taken` (lowercased names).
pub(crate) fn free_name(name: &str, is_dir: bool, taken: &HashSet<String>) -> String {
    if !taken.contains(&name.to_lowercase()) {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && !is_dir => name.split_at(i),
        _ => (name, ""),
    };
    (2..).map(|k| format!("{stem} ({k}){ext}")).find(|n| !taken.contains(&n.to_lowercase())).expect("some number is free")
}

/// A file or a folder and all it holds, from the computer to `target` on the server.
fn send(c: &mut dyn Conn, src: &Path, target: &str, p: &Progress) -> Result<(), String> {
    if p.cancelled() {
        return Err(CANCELLED.into());
    }
    if src.is_dir() {
        c.mkdir(target)?;
        let items = std::fs::read_dir(src).map_err(|e| format!("Não consegui ler {}: {e}", src.display()))?;
        for item in items {
            let item = item.map_err(|e| format!("Não consegui ler {}: {e}", src.display()))?;
            send(c, &item.path(), &child(target, &item.file_name().to_string_lossy()), p)?;
        }
        return Ok(());
    }
    let mut file = std::fs::File::open(src).map_err(|e| format!("Não consegui abrir {}: {e}", src.display()))?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut last = 0;
    c.upload(target, &mut file, len, &mut |n| {
        p.add_bytes(n - last);
        last = n;
        !p.cancelled()
    })
    .map_err(cancelled)
}

/// Recortar and Colar within one server: each item moves into the folder `to` shows.
pub fn move_within(s: &mut State, from: &Location, items: Vec<Entry>, to: &Location, reload: Vec<Location>) {
    let Some((server, dst)) = target(to).map(|(sv, d)| (sv.clone(), d)) else { return };
    let paths: Vec<String> = items.iter().filter_map(|e| path_of(from, e)).collect();
    let batch = transfers::queue(&mut s.transfers, s.waker.clone(), Op::Move, items.iter().map(|e| (end(from, Some(&e.name)), end(to, None))));
    job(s, &server, reload, move |c| {
        batch.run(|i, _| {
            let (e, path) = (&items[i], &paths[i]);
            let into = child(&dst, &e.name);
            if *path == into {
                return Ok(());
            }
            if e.is_dir && (dst == *path || dst.starts_with(&format!("{path}/"))) {
                return Err(format!("Não dá para mover a pasta \"{}\" para dentro dela mesma.", e.name));
            }
            c.rename(path, &into)
        })
    });
}

/// Deletes `items` of `loc` from the server, for good.
pub fn delete(s: &mut State, loc: &Location, items: Vec<Entry>, reload: Vec<Location>) {
    let Location::Server { server, .. } = loc else { return };
    let paths: Vec<(String, bool)> = items.iter().filter_map(|e| path_of(loc, e).map(|p| (p, e.is_dir))).collect();
    let n = paths.len();
    job(s, server, reload, move |c| {
        for (i, (path, is_dir)) in paths.iter().enumerate() {
            c.remove(path, *is_dir).map_err(|e| {
                let done = if i == 0 { String::new() } else { format!(" (já excluídos para sempre: {i})") };
                format!("Não consegui excluir {path}: {e}{done}")
            })?;
        }
        Ok(format!("Excluído para sempre: {}.", if n == 1 { "1 item".into() } else { format!("{n} itens") }))
    });
}

/// Renames `item` of `loc` to `name`, or with no item makes folder `name` in the folder shown.
pub fn name(s: &mut State, loc: &Location, item: Option<Entry>, name: String) {
    let Location::Server { server, path, .. } = loc else { return };
    let reload = vec![loc.clone()];
    match item {
        Some(item) => {
            let Some(from) = path_of(loc, &item) else { return };
            let to = child(from.rsplit_once('/').map_or("", |(d, _)| d), &name);
            job(s, server, reload, move |c| c.rename(&from, &to).map(|_| format!("Renomeado para \"{name}\".")));
        }
        None => {
            let to = child(&server.path(path), &name);
            job(s, server, reload, move |c| c.mkdir(&to).map(|_| format!("Pasta \"{name}\" criada.")));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Adding, changing and removing servers

/// The Adicionar servidor dialog (or Editar servidor, with `editing`).
pub struct Form {
    /// The id of the server being changed.
    pub editing: Option<String>,
    pub protocol: Protocol,
    pub name: TextEdit,
    pub host: TextEdit,
    pub port: TextEdit,
    pub user: TextEdit,
    pub password: TextEdit,
    pub folder: TextEdit,
    pub key: TextEdit,
    /// Conexões simultâneas; empty for the usual number.
    pub connections: TextEdit,
    /// The fingerprint of the certificate the user trusts for this server (FTPS, https).
    pub trusted: Option<String>,
    /// A certificate the last check turned down, waiting for the user to trust it or not.
    pub untrusted: Option<Rejected>,
    pub error: Option<String>,
    /// The connection check under way, by number.
    pub checking: Option<u64>,
}

impl Form {
    pub fn new() -> Form {
        let blank = || TextEdit::new("");
        let mut host = blank();
        host.focused = true;
        Form {
            editing: None,
            protocol: Protocol::Sftp,
            name: blank(),
            host,
            port: blank(),
            user: blank(),
            password: TextEdit::new("").secure(),
            folder: blank(),
            key: blank(),
            connections: blank(),
            trusted: None,
            untrusted: None,
            error: None,
            checking: None,
        }
    }

    /// The form filled with `server`, to change it; the password stays as stored unless typed.
    pub fn of(server: &Server) -> Form {
        let mut f = Form::new();
        f.editing = Some(server.id.clone());
        f.protocol = server.protocol;
        f.name = TextEdit::new(server.name.clone());
        f.host = TextEdit::new(server.host.clone());
        if server.port != server.protocol.default_port() {
            f.port = TextEdit::new(server.port.to_string());
        }
        f.user = TextEdit::new(server.user.clone());
        f.folder = TextEdit::new(server.root.clone());
        f.key = TextEdit::new(server.key.clone().unwrap_or_default());
        if server.connections != remote::USUAL_CONNECTIONS {
            f.connections = TextEdit::new(server.connections.to_string());
        }
        f.trusted = server.cert.clone();
        f
    }

    /// The form filled with what an address typed says (`sftp://ana@nas:2222/home`).
    pub fn of_address(a: &Address) -> Form {
        let mut f = Form::new();
        f.protocol = a.protocol;
        f.host = TextEdit::new(a.host.clone());
        if let Some(port) = a.port {
            f.port = TextEdit::new(port.to_string());
        }
        f.user = TextEdit::new(a.user.clone());
        if let Some(p) = &a.password {
            f.password = TextEdit::new(p.clone()).secure();
        }
        if a.path != "/" {
            f.folder = TextEdit::new(a.path.clone());
        }
        f
    }

    /// The server the form describes, and the password a pasted address held; or what's
    /// missing.
    fn server(&self, s: &State) -> Result<(Server, Option<String>), String> {
        let mut protocol = self.protocol;
        let mut host = self.host.text.trim().to_string();
        let mut user = self.user.text.trim().to_string();
        let mut root = self.folder.text.trim().to_string();
        let mut port = None;
        let mut pasted = None;
        // A whole address pasted in the Servidor field fills the rest.
        if host.contains("://")
            && let Some(a) = parse_address(&host)
        {
            protocol = a.protocol;
            host = a.host;
            port = a.port;
            if user.is_empty() {
                user = a.user;
            }
            if root.is_empty() && a.path != "/" {
                root = a.path;
            }
            pasted = a.password;
        }
        if host.is_empty() {
            return Err("Escreva o endereço do servidor.".into());
        }
        if host.contains('/') || host.contains(' ') {
            return Err("O servidor é só o nome ou o IP, sem pastas: a pasta vai em Pasta inicial.".into());
        }
        let port = match (self.port.text.trim(), port) {
            ("", Some(p)) => p,
            ("", None) => protocol.default_port(),
            (t, _) => t.parse::<u16>().ok().filter(|p| *p > 0).ok_or("A porta é um número de 1 a 65535.")?,
        };
        if !root.is_empty() && !root.starts_with('/') {
            root.insert(0, '/');
        }
        let name = match self.name.text.trim() {
            "" => host.clone(),
            n => n.to_string(),
        };
        let key = match self.key.text.trim() {
            "" => None,
            k if protocol == Protocol::Sftp => Some(k.trim_matches('"').to_string()),
            _ => None,
        };
        if let Some(k) = &key
            && !Path::new(k).is_file()
        {
            return Err(format!("A chave {k} não existe."));
        }
        let connections = match self.connections.text.trim() {
            "" => remote::USUAL_CONNECTIONS,
            t => t.parse::<u8>().ok().filter(|n| (1..=remote::MOST_CONNECTIONS).contains(n)).ok_or(format!("Conexões simultâneas é um número de 1 a {}.", remote::MOST_CONNECTIONS))?,
        };
        // A certificate trusted is only for the protocols over TLS.
        let cert = self.trusted.clone().filter(|_| matches!(protocol, Protocol::Ftps | Protocol::Davs));
        let id = self.editing.clone().unwrap_or_else(remote::store::new_id);
        // The host key seen before stays, unless the server is another one now.
        let host_key = self.editing.as_ref().and_then(|id| s.servers.list.iter().find(|sv| sv.id == *id)).filter(|old| old.host == host && old.port == port && old.protocol == protocol).and_then(|old| old.host_key.clone());
        Ok((Server { id, name, protocol, host, port, user, root, key, host_key, cert, connections, implicit: false }, pasted))
    }
}

pub fn ask_add(s: &mut State) {
    s.dialog = Some(Dialog::Server(Box::new(Form::new())));
}

pub fn ask_edit(s: &mut State, id: &str) {
    if let Some(server) = s.servers.list.iter().find(|sv| sv.id == id) {
        s.dialog = Some(Dialog::Server(Box::new(Form::of(server))));
    }
}

/// Salvar in the dialog: checks the login on a thread; the server is kept once it worked.
pub fn submit(s: &mut State) {
    let Some(Dialog::Server(form)) = &s.dialog else { return };
    let (server, pasted) = match form.server(s) {
        Ok(found) => found,
        Err(e) => {
            if let Some(Dialog::Server(f)) = &mut s.dialog {
                f.error = Some(e);
            }
            return;
        }
    };
    // The Senha field, else a password the pasted address held.
    let typed = if form.password.text.is_empty() { pasted.unwrap_or_default() } else { form.password.text.clone() };
    // Changing a server without typing the password keeps the one stored.
    let secret = match (&form.editing, typed.is_empty()) {
        (Some(id), true) => remote::store::secret(id),
        _ => Some(typed.clone()),
    };
    let keep = if form.editing.is_some() && typed.is_empty() { None } else { Some(typed) };
    s.servers.attempts += 1;
    let attempt = s.servers.attempts;
    if let Some(Dialog::Server(f)) = &mut s.dialog {
        f.checking = Some(attempt);
        f.error = None;
        f.untrusted = None;
    }
    background(s, move |emit| {
        let checked = catch_panic("conectar ao servidor", || {
            let mut server = server;
            Ok(remote::probe(&mut server, secret.as_deref()).map(|_| server))
        });
        let result = checked.unwrap_or_else(|e| Err(Refusal::Failed(e)));
        emit(Msg::ServerChecked { attempt, result, secret: keep });
    });
}

/// Confiar e salvar: trusts the certificate the check turned down, by its fingerprint, and
/// checks again.
pub fn trust(s: &mut State) {
    let Some(Dialog::Server(f)) = &mut s.dialog else { return };
    let Some(r) = f.untrusted.take() else { return };
    f.trusted = Some(r.fingerprint);
    submit(s);
}

/// The add dialog's connection check ended: the server is saved and opens in the active pane,
/// or the dialog says what went wrong.
pub fn checked(s: &mut State, attempt: u64, result: Result<Server, Refusal>, secret: Option<String>) {
    let Some(Dialog::Server(form)) = &mut s.dialog else { return };
    if form.checking != Some(attempt) {
        return;
    }
    form.checking = None;
    let server = match result {
        Ok(sv) => sv,
        // The dialog shows the certificate and asks whether to trust it.
        Err(Refusal::Untrusted(r)) => {
            form.untrusted = Some(r);
            return;
        }
        Err(Refusal::Failed(e)) => {
            form.error = Some(e);
            return;
        }
    };
    if let Err(e) = s.servers.store.save(server.clone(), secret.as_deref()) {
        form.error = Some(e);
        return;
    }
    let edited = form.editing.is_some();
    s.dialog = None;
    s.servers.pool.forget(&server.id);
    let old_root = s.servers.list.iter().find(|sv| sv.id == server.id).map(|sv| sv.root.clone());
    match s.servers.list.iter_mut().find(|sv| sv.id == server.id) {
        Some(old) => *old = server.clone(),
        None => s.servers.list.push(server.clone()),
    }
    // Panes (and what Copiar picked up) on the server as it was go on with it as it is now;
    // with another folder to start in, the folders under the old one mean nothing there.
    let moved = old_root.is_some_and(|r| r != server.root);
    let renew = |l: &mut Location| {
        if let Location::Server { server: sv, path, find } = l
            && sv.id == server.id
        {
            **sv = server.clone();
            if moved {
                path.clear();
                *find = None;
            }
        }
    };
    for p in 0..2 {
        let pane = &mut s.panes[p];
        pane.back.iter_mut().chain(pane.forward.iter_mut()).for_each(renew);
    }
    if let Some(clip) = &mut s.clip {
        renew(&mut clip.from);
    }
    s.tree.server_changed(&server.id);
    // The panes on it show it again, read with its new settings.
    for p in 0..2 {
        let mut here = s.panes[p].loc.clone();
        let before = here.clone();
        renew(&mut here);
        if here != before {
            crate::switch(s, p, here);
        }
    }
    let key = match &server.host_key {
        Some(k) if !edited => format!(" Chave do servidor: {k}."),
        _ => String::new(),
    };
    s.status = format!("{} {}: {}.{key}", if edited { "Servidor alterado" } else { "Servidor adicionado" }, server.name, server.address());
    if !edited {
        let p = s.active;
        crate::navigate(s, p, folder(&server, Vec::new()));
    }
}

/// Forgets a server and its password; panes on it go home.
pub fn remove(s: &mut State, id: &str) {
    if let Err(e) = s.servers.store.remove(id) {
        s.status = e;
        return;
    }
    s.servers.pool.forget(id);
    let name = s.servers.list.iter().find(|sv| sv.id == id).map(|sv| sv.name.clone()).unwrap_or_default();
    s.servers.list.retain(|sv| sv.id != id);
    s.tree.server_changed(id);
    let on = |l: &Location| matches!(l, Location::Server { server, .. } if server.id == id);
    if s.clip.as_ref().is_some_and(|c| on(&c.from)) {
        s.clip = None;
    }
    for p in 0..2 {
        s.panes[p].back.retain(|l| !on(l));
        s.panes[p].forward.retain(|l| !on(l));
        if on(&s.panes[p].loc) {
            crate::switch(s, p, Location::home());
        }
    }
    s.status = format!("Servidor removido: {name}");
}

// ---------------------------------------------------------------------------------------------
// Addresses

/// What an address like `sftp://ana@nas:2222/home/ana` says.
#[derive(Clone, Debug, PartialEq)]
pub struct Address {
    pub protocol: Protocol,
    pub user: String,
    pub password: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    /// Decoded, from `/`.
    pub path: String,
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(b) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Reads `scheme://[user[:password]@]host[:port][/path]`; `None` for any other text.
pub fn parse_address(text: &str) -> Option<Address> {
    let (scheme, rest) = text.trim().split_once("://")?;
    let protocol = Protocol::from_scheme(scheme)?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (login, hostport) = match authority.rfind('@') {
        Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
        None => (None, authority),
    };
    let (user, password) = match login {
        Some(l) => match l.split_once(':') {
            Some((u, p)) => (decode(u), Some(decode(p))),
            None => (decode(l), None),
        },
        None => (String::new(), None),
    };
    let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
        let (h, after) = v6.split_once(']')?;
        (h.to_string(), after.strip_prefix(':').and_then(|p| p.parse().ok()))
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p.parse().ok()?)),
            None => (hostport.to_string(), None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let path = decode(path.split(['?', '#']).next().unwrap_or("/"));
    Some(Address { protocol, user, password, host, port, path })
}

/// The address bar's text for a server location: the server's address and the full path.
pub fn address_text(loc: &Location) -> String {
    let Location::Server { server, path, .. } = loc else { return String::new() };
    format!("{}{}", server.address(), server.path(path))
}

/// The server location an address leads to, among the servers added: the same protocol, host
/// and port (and user, when the address names one), and a path under the server's folder.
pub fn reach(servers: &[Server], a: &Address) -> Result<Location, String> {
    let port = a.port.unwrap_or(a.protocol.default_port());
    let same = |sv: &&Server| sv.protocol == a.protocol && sv.host.eq_ignore_ascii_case(&a.host) && sv.port == port && (a.user.is_empty() || sv.user == a.user);
    let candidates: Vec<&Server> = servers.iter().filter(same).collect();
    if candidates.is_empty() {
        return Err(String::new());
    }
    let wanted = a.path.trim_end_matches('/');
    // The server whose folder holds the path most closely.
    let best = candidates
        .iter()
        .filter(|sv| {
            let root = sv.root.trim_end_matches('/');
            wanted == root || wanted.starts_with(&format!("{root}/")) || root.is_empty()
        })
        .max_by_key(|sv| sv.root.trim_end_matches('/').len());
    let Some(sv) = best else {
        return Err(format!("{} fica fora da pasta inicial do servidor ({}).", a.path, candidates[0].root));
    };
    let rest = wanted[sv.root.trim_end_matches('/').len()..].trim_start_matches('/');
    let path = rest.split('/').filter(|s| !s.is_empty()).map(str::to_string).collect();
    Ok(folder(sv, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(protocol: Protocol, host: &str, root: &str) -> Server {
        Server { id: format!("{host}{root}"), name: host.into(), protocol, host: host.into(), port: protocol.default_port(), user: "ana".into(), root: root.into(), key: None, host_key: None, cert: None, connections: 4, implicit: false }
    }

    #[test]
    fn addresses_are_read_whole() {
        let a = parse_address("sftp://ana%40casa:s3nh%40@nas.local:2222/home/ana/F%C3%A9rias").unwrap();
        assert_eq!(a, Address { protocol: Protocol::Sftp, user: "ana@casa".into(), password: Some("s3nh@".into()), host: "nas.local".into(), port: Some(2222), path: "/home/ana/Férias".into() });
        let a = parse_address("https://cloud.example.com").unwrap();
        assert_eq!((a.protocol, a.port, a.path.as_str(), a.user.as_str()), (Protocol::Davs, None, "/", ""));
        assert_eq!(parse_address("ftp://[::1]:21/x").map(|a| (a.host, a.port)), Some(("::1".into(), Some(21))));
        assert!(parse_address(r"C:\Users").is_none());
        assert!(parse_address("gopher://x").is_none());
        assert!(parse_address("ftp://host:porta").is_none());
    }

    #[test]
    fn an_address_finds_its_server_and_folder() {
        let list = vec![server(Protocol::Sftp, "nas", "/home/ana"), server(Protocol::Sftp, "nas", "/"), server(Protocol::Davs, "cloud", "/dav/files/ana")];
        let at = |text: &str| reach(&list, &parse_address(text).unwrap());
        let Ok(Location::Server { server: sv, path, .. }) = at("sftp://nas/home/ana/Fotos/2024") else { panic!() };
        assert_eq!((sv.root.as_str(), path), ("/home/ana", vec!["Fotos".to_string(), "2024".to_string()]));
        let Ok(Location::Server { server: sv, path, .. }) = at("sftp://ana@NAS/etc") else { panic!() };
        assert_eq!((sv.root.as_str(), path), ("/", vec!["etc".to_string()]));
        let Ok(Location::Server { path, .. }) = at("https://cloud/dav/files/ana/") else { panic!() };
        assert!(path.is_empty());
        assert!(at("https://cloud/outro").unwrap_err().contains("fora da pasta"));
        // A server not added yet: the caller offers to add it.
        assert_eq!(at("ftp://nas/"), Err(String::new()));
        assert_eq!(at("sftp://bia@nas/"), Err(String::new()));
        // And the address bar shows it back.
        let loc = folder(&list[0], vec!["Fotos".into()]);
        assert_eq!(address_text(&loc), "sftp://ana@nas/home/ana/Fotos");
        assert_eq!(reach(&list, &parse_address(&address_text(&loc)).unwrap()), Ok(loc));
    }

    #[test]
    fn results_open_where_they_were_found() {
        let sv = server(Protocol::Ftp, "nas", "/srv");
        let found = Location::Server { server: Box::new(sv.clone()), path: vec!["a".into()], find: Some("x".into()) };
        let e = |id: &str| Entry { id: id.into(), name: id.rsplit('/').next().unwrap().into(), is_dir: false, size: None, modified: None, target: None, mime: None, md5: None, starred: false };
        assert_eq!(path_of(&found, &e("b/c/x.txt")).as_deref(), Some("/srv/a/b/c/x.txt"));
        assert_eq!(path_of(&folder(&sv, vec!["a".into()]), &e("x.txt")).as_deref(), Some("/srv/a/x.txt"));
        assert!(target(&found).is_none());
        assert_eq!(target(&folder(&sv, vec![])).map(|(_, d)| d), Some("/srv".into()));
    }
}

/// The panes' work on a real server: `rclone serve webdav` on a temporary folder. Needs rclone
/// on the PATH: `cargo test -p openexplorer -- --ignored on_a_real_server`.
#[cfg(test)]
mod real {
    use super::*;
    use crate::actions::{self, Clip, ClipMode};
    use std::time::{Duration, Instant};

    struct Served(std::process::Child, PathBuf);
    impl Drop for Served {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }

    /// Lets background jobs finish: polls until the status says how the last one went.
    fn settle(s: &mut State) {
        let start = Instant::now();
        loop {
            crate::poll(s, 0.0, &mut rune::Tx::default());
            let busy = s.panes.iter().any(|p| p.loading) || s.status.is_empty() || s.status.ends_with('…');
            if !busy {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(30), "stuck: {}", s.status);
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    fn names(s: &State, p: usize) -> Vec<String> {
        let mut v: Vec<String> = s.panes[p].entries.iter().map(|e| e.name.clone()).collect();
        v.sort();
        v
    }

    #[test]
    #[ignore]
    fn on_a_real_server() {
        let base = std::env::temp_dir().join(format!("oe-app-server-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (served, local) = (base.join("servidor"), base.join("local"));
        std::fs::create_dir_all(served.join("Fotos")).unwrap();
        std::fs::write(served.join("Fotos/praia.jpg"), b"areia").unwrap();
        std::fs::create_dir_all(local.join("Projeto/src")).unwrap();
        std::fs::write(local.join("Projeto/src/main.rs"), b"fn main() {}").unwrap();
        std::fs::write(local.join("notas.txt"), b"oi").unwrap();
        let child = std::process::Command::new("rclone")
            .args(["serve", "webdav", &served.display().to_string(), "--addr", "127.0.0.1:28097"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("rclone on the PATH");
        let _guard = Served(child, base.clone());
        while std::net::TcpStream::connect(("127.0.0.1", 28097)).is_err() {
            std::thread::sleep(Duration::from_millis(100));
        }
        let server = Server { id: "teste-app".into(), name: "Teste".into(), protocol: Protocol::Dav, host: "127.0.0.1".into(), port: 28097, user: String::new(), root: "/".into(), key: None, host_key: None, cert: None, connections: 4, implicit: false };
        let mut s = crate::init();
        s.cache = crate::cache::Cache::memory_only();
        s.servers.list = vec![server.clone()];
        crate::switch(&mut s, 0, folder(&server, vec![]));
        crate::switch(&mut s, 1, Location::Local(local.clone()));
        // A listing says nothing: only its loading counts.
        s.status = "listado".into();
        settle(&mut s);
        assert_eq!(names(&s, 0), ["Fotos"]);

        // Up from the computer: a folder and a file, into the server's root.
        s.status.clear();
        let items = s.panes[1].entries.clone();
        s.clip = Some(Clip { mode: ClipMode::Copy, from: s.panes[1].loc.clone(), items });
        actions::paste(&mut s, 0);
        settle(&mut s);
        assert!(s.status.starts_with("Enviado"), "{}", s.status);
        assert_eq!(std::fs::read(served.join("Projeto/src/main.rs")).unwrap(), b"fn main() {}");
        assert_eq!(names(&s, 0), ["Fotos", "Projeto", "notas.txt"]);
        // Again: the copies get free names.
        s.status.clear();
        actions::paste(&mut s, 0);
        settle(&mut s);
        assert!(served.join("notas (2).txt").is_file() && served.join("Projeto (2)").is_dir(), "{}", s.status);

        // Down to the computer, into a folder of its own.
        std::fs::create_dir(local.join("baixados")).unwrap();
        crate::switch(&mut s, 1, Location::Local(local.join("baixados")));
        let fotos = s.panes[0].entries.iter().find(|e| e.name == "Fotos").cloned().unwrap();
        s.status.clear();
        s.clip = Some(Clip { mode: ClipMode::Copy, from: s.panes[0].loc.clone(), items: vec![fotos] });
        actions::paste(&mut s, 1);
        settle(&mut s);
        assert_eq!(std::fs::read(local.join("baixados/Fotos/praia.jpg")).unwrap(), b"areia", "{}", s.status);

        // A new folder, a rename, a move into it, and the search finding it there.
        s.status.clear();
        actions::apply_name(&mut s, 0, None, "Arquivo".into());
        settle(&mut s);
        let notas = s.panes[0].entries.iter().find(|e| e.name == "notas (2).txt").cloned().unwrap();
        s.status.clear();
        actions::apply_name(&mut s, 0, Some(notas), "velhas.txt".into());
        settle(&mut s);
        let velhas = s.panes[0].entries.iter().find(|e| e.name == "velhas.txt").cloned().unwrap();
        crate::switch(&mut s, 1, folder(&server, vec!["Arquivo".into()]));
        s.status.clear();
        s.clip = Some(Clip { mode: ClipMode::Cut, from: s.panes[0].loc.clone(), items: vec![velhas] });
        actions::paste(&mut s, 1);
        settle(&mut s);
        assert!(served.join("Arquivo/velhas.txt").is_file(), "{}", s.status);
        s.search[0] = TextEdit::new("velhas");
        actions::search_server(&mut s, 0);
        // A listing says nothing: only its loading counts.
        s.status = "listado".into();
        settle(&mut s);
        assert_eq!(s.panes[0].entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["Arquivo/velhas.txt"]);
        s.search[0] = TextEdit::new("");
        actions::search_server(&mut s, 0);
        // A listing says nothing: only its loading counts.
        s.status = "listado".into();
        settle(&mut s);
        assert!(matches!(&s.panes[0].loc, Location::Server { find: None, path, .. } if path.is_empty()));

        // Deleting asks, then deletes for good.
        s.panes[0].selected = ["Projeto (2)".to_string()].into();
        actions::ask_delete(&mut s, 0);
        assert!(matches!(s.dialog, Some(Dialog::Purge { .. })));
        s.status.clear();
        actions::purge(&mut s);
        settle(&mut s);
        assert!(!served.join("Projeto (2)").exists(), "{}", s.status);
    }

    /// Adicionar servidor end to end: the login checked, the server and its password kept, its
    /// folder open; changed without retyping the password; then removed with its password.
    #[test]
    #[ignore]
    fn adding_changing_and_removing_a_server() {
        let base = std::env::temp_dir().join(format!("oe-app-add-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("raiz/Documentos")).unwrap();
        let child = std::process::Command::new("rclone")
            .args(["serve", "sftp", &base.join("raiz").display().to_string(), "--addr", "127.0.0.1:28098", "--user", "ana", "--pass", "s3nh@ forte"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("rclone on the PATH");
        let _guard = Served(child, base.clone());
        while std::net::TcpStream::connect(("127.0.0.1", 28098)).is_err() {
            std::thread::sleep(Duration::from_millis(100));
        }
        let mut s = crate::init();
        s.cache = crate::cache::Cache::memory_only();
        s.servers.store = Store::at(base.join("servers.json"));
        s.servers.list.clear();
        let wait = |s: &mut State| {
            let start = Instant::now();
            loop {
                let checking = matches!(&s.dialog, Some(Dialog::Server(f)) if f.checking.is_some());
                if !checking && !s.panes[0].loading {
                    return;
                }
                crate::poll(s, 0.0, &mut rune::Tx::default());
                assert!(start.elapsed() < Duration::from_secs(30));
                std::thread::sleep(Duration::from_millis(30));
            }
        };

        // A wrong password keeps the dialog open, saying so; nothing is kept.
        ask_add(&mut s);
        let Some(Dialog::Server(f)) = &mut s.dialog else { panic!() };
        f.host = TextEdit::new("sftp://ana:errada@127.0.0.1:28098");
        submit(&mut s);
        wait(&mut s);
        let Some(Dialog::Server(f)) = &mut s.dialog else { panic!("the dialog closed") };
        assert!(f.error.as_deref().is_some_and(|e| e.contains("recusou")), "{:?}", f.error);
        assert!(s.servers.store.servers().is_empty());

        f.password = TextEdit::new("s3nh@ forte").secure();
        submit(&mut s);
        wait(&mut s);
        let left = match &s.dialog {
            Some(Dialog::Server(f)) => f.error.clone(),
            _ => None,
        };
        assert!(s.dialog.is_none(), "{left:?}");
        let saved = s.servers.store.servers();
        assert_eq!(saved.len(), 1);
        let sv = saved[0].clone();
        assert_eq!((sv.host.as_str(), sv.port, sv.user.as_str(), sv.root.as_str()), ("127.0.0.1", 28098, "ana", "/"));
        assert!(sv.host_key.as_deref().is_some_and(|k| k.starts_with("SHA256:")));
        assert_eq!(remote::store::secret(&sv.id).as_deref(), Some("s3nh@ forte"));
        // Listed with the password from the credential store.
        assert_eq!(names(&s, 0), ["Documentos"], "{:?}", s.panes[0].error);

        // Changed (a new name) without typing the password: the stored one and the key stay.
        ask_edit(&mut s, &sv.id);
        let Some(Dialog::Server(f)) = &mut s.dialog else { panic!() };
        f.name = TextEdit::new("Meu NAS");
        submit(&mut s);
        wait(&mut s);
        assert!(s.dialog.is_none());
        let after = s.servers.store.servers()[0].clone();
        assert_eq!((after.name.as_str(), &after.host_key), ("Meu NAS", &sv.host_key));
        assert_eq!(remote::store::secret(&sv.id).as_deref(), Some("s3nh@ forte"));
        assert!(matches!(&s.panes[0].loc, Location::Server { server, .. } if server.name == "Meu NAS"));

        // A different key under the same address is refused when changing it too.
        let mut forged = after.clone();
        forged.host_key = Some("SHA256:outra".into());
        assert!(remote::probe(&mut forged, Some("s3nh@ forte")).unwrap_err().to_string().contains("mudou"));

        remove(&mut s, &sv.id);
        assert!(s.servers.store.servers().is_empty() && s.servers.list.is_empty());
        assert_eq!(remote::store::secret(&sv.id), None);
        assert!(!matches!(s.panes[0].loc, Location::Server { .. }));
    }

    /// A WebDAV server over https with a certificate of its own (as a NAS has): the check turns
    /// it down and the dialog asks; Confiar e salvar keeps the server with that certificate's
    /// fingerprint, and its folders list.
    #[test]
    #[ignore]
    fn trusting_a_servers_own_certificate() {
        let base = std::env::temp_dir().join(format!("oe-app-cert-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("raiz/Fotos")).unwrap();
        let (cert, key) = (base.join("cert.pem"), base.join("key.pem"));
        let made = std::process::Command::new("openssl")
            .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "30", "-subj", "/CN=nas.local", "-addext", "basicConstraints=critical,CA:TRUE"])
            .arg("-keyout")
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .output()
            .expect("openssl on the PATH");
        assert!(made.status.success());
        let child = std::process::Command::new("rclone")
            .args(["serve", "webdav", &base.join("raiz").display().to_string(), "--addr", "127.0.0.1:28443", "--cert", &cert.display().to_string(), "--key", &key.display().to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("rclone on the PATH");
        let _guard = Served(child, base.clone());
        while std::net::TcpStream::connect(("127.0.0.1", 28443)).is_err() {
            std::thread::sleep(Duration::from_millis(100));
        }
        let mut s = crate::init();
        s.cache = crate::cache::Cache::memory_only();
        s.servers.store = Store::at(base.join("servers.json"));
        s.servers.list.clear();
        let wait = |s: &mut State| {
            let start = Instant::now();
            loop {
                let checking = matches!(&s.dialog, Some(Dialog::Server(f)) if f.checking.is_some());
                if !checking && !s.panes[0].loading {
                    return;
                }
                crate::poll(s, 0.0, &mut rune::Tx::default());
                assert!(start.elapsed() < Duration::from_secs(30));
                std::thread::sleep(Duration::from_millis(30));
            }
        };
        ask_add(&mut s);
        let Some(Dialog::Server(f)) = &mut s.dialog else { panic!() };
        f.host = TextEdit::new("https://127.0.0.1:28443");
        submit(&mut s);
        wait(&mut s);
        let Some(Dialog::Server(f)) = &s.dialog else { panic!("saved with an untrusted certificate") };
        let fp = f.untrusted.as_ref().map(|r| r.fingerprint.clone()).expect("the dialog asks about the certificate");
        assert!(f.error.is_none() && s.servers.store.servers().is_empty());
        trust(&mut s);
        wait(&mut s);
        assert!(s.dialog.is_none());
        let saved = s.servers.store.servers();
        assert_eq!(saved[0].cert.as_deref(), Some(fp.as_str()));
        assert_eq!(names(&s, 0), ["Fotos"], "{:?}", s.panes[0].error);
        remove(&mut s, &saved[0].id);
    }

    /// rclone serving `dir` over `kind` on `port`, logged in as ana.
    fn rclone(kind: &str, dir: &Path, port: u16) -> std::process::Child {
        let mut args = vec!["serve".to_string(), kind.into(), dir.display().to_string(), "--addr".into(), format!("127.0.0.1:{port}"), "--user".into(), "ana".into(), "--pass".into(), "s3nh@".into()];
        if kind == "ftp" {
            args.extend(["--passive-port".into(), format!("{}-{}", port + 1, port + 30)]);
        }
        let child = std::process::Command::new("rclone").args(&args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().expect("rclone on the PATH");
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            std::thread::sleep(Duration::from_millis(100));
        }
        child
    }

    /// Copying between servers and within one, by Colar, as the panes do it.
    #[test]
    #[ignore]
    fn copying_between_servers_and_within_one() {
        let base = std::env::temp_dir().join(format!("oe-app-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (sd, dd, fd) = (base.join("sftp"), base.join("dav"), base.join("ftp"));
        for d in [&sd, &dd, &fd] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::create_dir_all(sd.join("Projeto/src")).unwrap();
        std::fs::write(sd.join("Projeto/src/main.rs"), vec![b'x'; 700_000]).unwrap();
        std::fs::write(sd.join("notas.txt"), b"oi").unwrap();
        std::fs::write(fd.join("de-ftp.txt"), b"ftp").unwrap();
        std::fs::write(dd.join("doc.txt"), b"dav").unwrap();
        let children = [rclone("sftp", &sd, 28501), rclone("webdav", &dd, 28502), rclone("ftp", &fd, 28503)];
        struct Kill<const N: usize>([std::process::Child; N], PathBuf);
        impl<const N: usize> Drop for Kill<N> {
            fn drop(&mut self) {
                for c in &mut self.0 {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                let _ = std::fs::remove_dir_all(&self.1);
            }
        }
        let _guard = Kill(children, base.clone());
        let mk = |id: &str, protocol: Protocol, port: u16, connections: u8| Server {
            id: id.into(),
            name: id.into(),
            protocol,
            host: "127.0.0.1".into(),
            port,
            user: "ana".into(),
            root: "/".into(),
            key: None,
            host_key: None,
            cert: None,
            connections,
            implicit: false,
        };
        let (sftp, dav, ftp) = (mk("c-sftp", Protocol::Sftp, 28501, 4), mk("c-dav", Protocol::Dav, 28502, 4), mk("c-ftp", Protocol::Ftp, 28503, 4));
        let mut s = crate::init();
        s.cache = crate::cache::Cache::memory_only();
        // The password from the credential store, as the app reads it.
        s.servers.store = Store::at(base.join("servers.json"));
        for sv in [&sftp, &dav, &ftp] {
            s.servers.store.save(sv.clone(), Some("s3nh@")).unwrap();
        }
        let paste = |s: &mut State, from: &Location, names: &[&str], to: Location, mode: ClipMode| {
            crate::switch(s, 0, from.clone());
            s.status = "listado".into();
            settle(s);
            let items: Vec<Entry> = s.panes[0].entries.iter().filter(|e| names.contains(&e.name.as_str())).cloned().collect();
            assert_eq!(items.len(), names.len(), "{:?}", s.panes[0].entries);
            crate::switch(s, 1, to);
            s.status = "listado".into();
            settle(s);
            s.status.clear();
            s.clip = Some(Clip { mode, from: from.clone(), items });
            actions::paste(s, 1);
            settle(s);
        };

        // SFTP to WebDAV: a folder and a file, straight through.
        paste(&mut s, &folder(&sftp, vec![]), &["Projeto", "notas.txt"], folder(&dav, vec![]), ClipMode::Copy);
        assert!(s.status.starts_with("Copiado: 2"), "{}", s.status);
        assert_eq!(std::fs::read(dd.join("Projeto/src/main.rs")).unwrap().len(), 700_000);
        assert_eq!(std::fs::read(dd.join("notas.txt")).unwrap(), b"oi");

        // FTP to SFTP with Recortar: the original goes once it arrived.
        paste(&mut s, &folder(&ftp, vec![]), &["de-ftp.txt"], folder(&sftp, vec![]), ClipMode::Cut);
        assert!(s.status.starts_with("Movido: 1"), "{}", s.status);
        assert_eq!(std::fs::read(sd.join("de-ftp.txt")).unwrap(), b"ftp");
        assert!(!fd.join("de-ftp.txt").exists());

        // Within SFTP, into the same folder: a copy under a free name, two connections.
        paste(&mut s, &folder(&sftp, vec![]), &["Projeto"], folder(&sftp, vec![]), ClipMode::Copy);
        assert_eq!(std::fs::read(sd.join("Projeto (2)/src/main.rs")).unwrap().len(), 700_000, "{}", s.status);

        // Within SFTP allowed one connection: through a temporary file.
        let one = Server { connections: 1, ..sftp.clone() };
        s.servers.store.save(one.clone(), None).unwrap();
        paste(&mut s, &folder(&one, vec![]), &["notas.txt"], folder(&one, vec!["Projeto".into()]), ClipMode::Copy);
        assert_eq!(std::fs::read(sd.join("Projeto/notas.txt")).unwrap(), b"oi", "{}", s.status);

        // Within WebDAV: the server copies by itself.
        paste(&mut s, &folder(&dav, vec![]), &["doc.txt"], folder(&dav, vec!["Projeto".into()]), ClipMode::Copy);
        assert_eq!(std::fs::read(dd.join("Projeto/doc.txt")).unwrap(), b"dav", "{}", s.status);
        // And a whole folder, into the folder it's in: a copy under a free name.
        paste(&mut s, &folder(&dav, vec![]), &["Projeto"], folder(&dav, vec![]), ClipMode::Copy);
        assert_eq!(std::fs::read(dd.join("Projeto (2)/src/main.rs")).unwrap().len(), 700_000, "{}", s.status);
        assert_eq!(std::fs::read(dd.join("Projeto (2)/doc.txt")).unwrap(), b"dav");

        // A folder into itself is refused.
        paste(&mut s, &folder(&sftp, vec![]), &["Projeto"], folder(&sftp, vec!["Projeto".into(), "src".into()]), ClipMode::Copy);
        assert!(s.status.contains("dentro dela mesma"), "{}", s.status);
        for sv in [&sftp, &dav, &ftp] {
            s.servers.store.remove(&sv.id).unwrap();
        }
    }

    /// A drag out of a server pane, as Explorer reads it after the drop: the description of a
    /// folder, then a file's bytes from the start and from the middle.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn dragging_out_of_a_server() {
        use crate::dragout::{Fetch, Files};
        use std::io::Read;
        let base = std::env::temp_dir().join(format!("oe-app-drag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("Fotos/2024")).unwrap();
        std::fs::write(base.join("Fotos/2024/praia.jpg"), b"0123456789").unwrap();
        std::fs::write(base.join("Fotos/capa.txt"), b"capa").unwrap();
        let _guard = Served(rclone("sftp", &base, 28520), base.clone());
        let server = Server { id: "drag".into(), name: "drag".into(), protocol: Protocol::Sftp, host: "127.0.0.1".into(), port: 28520, user: "ana".into(), root: "/".into(), key: None, host_key: None, cert: None, connections: 4, implicit: false };
        let store = Store::at(base.join("servers.json"));
        store.save(server.clone(), Some("s3nh@")).unwrap();
        let pool = std::sync::Arc::new(Pool::default());
        let fotos = Entry { id: "Fotos".into(), name: "Fotos".into(), is_dir: true, size: None, modified: None, target: None, mime: None, md5: None, starred: false };
        let files = crate::relay::ServerFiles { pool, server: server.clone(), top: vec![("/Fotos".into(), fotos)] };
        let list = files.list(&std::sync::atomic::AtomicBool::new(false)).unwrap();
        let mut paths: Vec<&str> = list.iter().map(|v| v.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["Fotos", "Fotos\\2024", "Fotos\\2024\\praia.jpg", "Fotos\\capa.txt"]);
        let praia = list.iter().find(|v| v.path.ends_with("praia.jpg")).and_then(|v| v.file.clone()).unwrap();
        assert!(matches!(&praia.fetch, Fetch::Server(p) if p == "/Fotos/2024/praia.jpg"));
        assert_eq!(praia.size, Some(10));
        let read = |from: u64| {
            let mut out = String::new();
            files.open(&praia, from).unwrap().read_to_string(&mut out).unwrap();
            out
        };
        assert_eq!(read(0), "0123456789");
        assert_eq!(read(6), "6789");
        // A stream let go of midway frees its connection: the pool still serves.
        drop(files.open(&praia, 0).unwrap());
        assert_eq!(read(0), "0123456789");
        store.remove(&server.id).unwrap();
    }

    /// Colar from the first connected Drive account into a server: a small file from My Drive
    /// arrives whole. Only reads the account. Needs a connected account and rclone.
    #[test]
    #[ignore]
    fn copying_from_drive_to_a_server() {
        let mut s = crate::init();
        s.cache = crate::cache::Cache::memory_only();
        let email = s.accounts.first().expect("uma conta conectada").email.clone();
        let drive = openexplorer_gdrive::Drive::new(openexplorer_gdrive::ClientConfig::load().unwrap(), openexplorer_gdrive::store::refresh_token(&email).unwrap());
        let mut small = None;
        drive
            .list("root", None, |page, _| {
                small = small.take().or_else(|| page.into_iter().find(|f| !f.is_dir && !f.mime.starts_with("application/vnd.google-apps.") && f.size.is_some_and(|n| n > 0 && n < 2 << 20)));
                small.is_none()
            })
            .unwrap();
        let file = small.expect("um arquivo pequeno no Meu Drive");
        let base = std::env::temp_dir().join(format!("oe-app-fromdrive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let _guard = Served(rclone("webdav", &base, 28530), base.clone());
        let server = Server { id: "do-drive".into(), name: "do-drive".into(), protocol: Protocol::Dav, host: "127.0.0.1".into(), port: 28530, user: "ana".into(), root: "/".into(), key: None, host_key: None, cert: None, connections: 4, implicit: false };
        s.servers.store = Store::at(base.join("servers.json"));
        s.servers.store.save(server.clone(), Some("s3nh@")).unwrap();
        let from = Location::Drive { email: email.clone(), space: crate::model::Space::MyDrive, path: Vec::new() };
        let item = crate::entry_of(file.clone());
        crate::switch(&mut s, 1, folder(&server, vec![]));
        s.status.clear();
        crate::relay::drive_to_server(&mut s, &from, vec![item], &folder(&server, vec![]), false, vec![folder(&server, vec![])]);
        settle(&mut s);
        assert!(s.status.starts_with("Copiado: 1"), "{}", s.status);
        let name = crate::model::shown(&file.name);
        assert_eq!(std::fs::metadata(base.join(crate::ops::windows_name(&name))).map(|m| m.len()).ok(), file.size, "{name}");
        s.servers.store.remove(&server.id).unwrap();
    }
}
