//! Copying where neither end is the computer: within one server, between two servers, and
//! between a server and Google Drive.
//!
//! Between servers the bytes go straight through: one connection downloads into a small pipe
//! while another uploads from it, nothing written to the disk. A WebDAV server copies within
//! itself on its own (COPY). A server allowed only one connection relays through a temporary
//! file instead. Google Drive takes and gives files on disk, so a copy to or from it passes
//! through a temporary file, a file at a time, deleted as soon as it's sent.

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use openexplorer_gdrive::{Drive, DriveFile};
use openexplorer_i18n::{t, tr};
use openexplorer_remote::{Conn, store};

use crate::actions::drive_folder;
use crate::model::{Entry, Location, Space};
use crate::ops::{CANCELLED, Progress};
use crate::servers::{child, end, free_name, measure, path_of, pin_all, target, with};
use crate::transfers::{self, End, Op};
use crate::{Msg, State, background, catch_panic, drive_task};

/// How deep a copy goes into folders (links followed): a link back up would go round forever.
const DEEPEST: usize = 64;

// ---------------------------------------------------------------------------------------------
// A pipe between a download and an upload

struct PipeIn(SyncSender<Result<Vec<u8>, String>>);

impl Write for PipeIn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.send(Ok(buf.to_vec())).map_err(|_| io::Error::other(t("o envio parou", "the transfer stopped")))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct PipeOut {
    rx: Receiver<Result<Vec<u8>, String>>,
    chunk: Vec<u8>,
    at: usize,
}

impl Read for PipeOut {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.at == self.chunk.len() {
            match self.rx.recv() {
                Ok(Ok(chunk)) => (self.chunk, self.at) = (chunk, 0),
                // The download failed: an error, never the end of the file, so the upload
                // doesn't keep half of it as if it were whole.
                Ok(Err(e)) => return Err(io::Error::other(e)),
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.chunk.len() - self.at);
        buf[..n].copy_from_slice(&self.chunk[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

// ---------------------------------------------------------------------------------------------
// Between servers, or within one

/// The connections a copy between servers works with: one at each end, or a single one to a
/// server that allows no more, through which each file is relayed by a temporary file.
enum Ends<'a> {
    Two(&'a mut dyn Conn, &'a mut dyn Conn),
    One(&'a mut dyn Conn),
}

impl Ends<'_> {
    fn src(&mut self) -> &mut dyn Conn {
        match self {
            Ends::Two(a, _) => *a,
            Ends::One(c) => *c,
        }
    }

    fn dst(&mut self) -> &mut dyn Conn {
        match self {
            Ends::Two(_, b) => *b,
            Ends::One(c) => *c,
        }
    }

    /// One file, `len` bytes, from `from` at the source to `to` at the destination; on any
    /// failure `to` is taken away. Progress counts the bytes arriving.
    fn file(&mut self, from: &str, to: &str, len: u64, p: &Progress) -> Result<(), String> {
        let done = match self {
            Ends::Two(a, b) => stream(&mut **a, from, &mut **b, to, len, p),
            Ends::One(c) => relay(&mut **c, from, to, len, p),
        };
        if done.is_err() {
            let _ = self.dst().remove_file(to);
        }
        done
    }
}

/// Downloads on `a` while uploading on `b`, through a pipe of a few chunks.
fn stream(a: &mut dyn Conn, from: &str, b: &mut dyn Conn, to: &str, len: u64, p: &Progress) -> Result<(), String> {
    let (tx, rx) = sync_channel(8);
    std::thread::scope(|scope| {
        let reading = scope.spawn(move || {
            let mut pipe = PipeIn(tx);
            let got = a.download(from, &mut pipe, &mut |_| !p.cancelled());
            if let Err(e) = &got {
                let _ = pipe.0.send(Err(e.clone()));
            }
            got
        });
        let mut out = PipeOut { rx, chunk: Vec::new(), at: 0 };
        let mut last = 0;
        let sent = b.upload(to, &mut out, len, &mut |n| {
            p.add_bytes(n - last);
            last = n;
            !p.cancelled()
        });
        // The upload stopping closes the pipe, which stops the download.
        drop(out);
        let got = reading.join().unwrap_or_else(|_| Err(t("A leitura do servidor de origem parou.", "Reading from the source server stopped.").into()));
        match (got, sent) {
            _ if p.cancelled() => Err(CANCELLED.into()),
            (Err(e), _) => Err(e),
            (_, Err(e)) => Err(e),
            (Ok(()), Ok(())) => Ok(()),
        }
    })
}

/// A file within a server that allows one connection: down to a temporary file, then up.
fn relay(c: &mut dyn Conn, from: &str, to: &str, len: u64, p: &Progress) -> Result<(), String> {
    let tmp = Temp::new()?;
    let mut file = std::fs::File::create(&tmp.0).map_err(|e| tr!("Não consegui criar o arquivo temporário: {e}", "Couldn't create the temporary file: {e}"))?;
    // The bar counts both ways (its size is twice the file's): down, then up.
    let mut last = 0;
    c.download(from, &mut file, &mut |n| {
        p.add_bytes(n - last);
        last = n;
        !p.cancelled()
    })
    .map_err(|e| if p.cancelled() { CANCELLED.into() } else { e })?;
    drop(file);
    let mut file = std::fs::File::open(&tmp.0).map_err(|e| tr!("Não consegui ler o arquivo temporário: {e}", "Couldn't read the temporary file: {e}"))?;
    let mut last = 0;
    c.upload(to, &mut file, len, &mut |n| {
        p.add_bytes(n - last);
        last = n;
        !p.cancelled()
    })
    .map_err(|e| if p.cancelled() { CANCELLED.into() } else { e })
}

/// A file or a folder and all it holds, from `from` to `to`, `depth` folders down.
fn copy_tree(ends: &mut Ends, from: &str, to: &str, is_dir: bool, len: u64, p: &Progress, depth: usize) -> Result<(), String> {
    if p.cancelled() {
        return Err(CANCELLED.into());
    }
    if !is_dir {
        return ends.file(from, to, len, p);
    }
    if depth > DEEPEST {
        return Err(tr!("{from}: pastas demais umas dentro das outras (um link que volta para cima?).", "{from}: too many folders inside one another (a link that loops back up?)."));
    }
    ends.dst().mkdir(to)?;
    for it in ends.src().list(from)? {
        copy_tree(ends, &child(from, &it.name), &child(to, &it.name), it.is_dir, it.size.unwrap_or(0), p, depth + 1)?;
    }
    Ok(())
}

/// Copies (or with `cut` moves) `items` of the server location `from` into the server folder
/// `to` shows: on another server, or a copy within the same one. A row each in the queue.
pub fn between_servers(s: &mut State, from: &Location, items: Vec<Entry>, to: &Location, cut: bool, reload: Vec<Location>) {
    let (Location::Server { server: a, .. }, Some((b, dir))) = (from, target(to)) else { return };
    let (a, b) = ((**a).clone(), b.clone());
    let paths: Vec<String> = items.iter().filter_map(|e| path_of(from, e)).collect();
    let op = if cut { Op::Move } else { Op::Copy };
    let batch = transfers::queue(&mut s.transfers, s.waker.clone(), op, items.iter().map(|e| (end(from, Some(&e.name)), end(to, None))));
    let pool = s.servers.pool.clone();
    background(s, move |emit| {
        let result = catch_panic(t("copiar entre servidores", "copy between servers"), || {
            let same = a.id == b.id;
            // A server allowing a single connection relays each file through the disk, which
            // the bar counts twice: down, then up.
            let one = same && a.limit() < 2;
            // Each item as it is now, whatever Recortar does to the others meanwhile; and how
            // many copies within the server go in one batch (Dropbox's), else one at a time.
            let (pinned, most) = with(&pool, &a, |c| {
                for ((p, e), path) in batch.rows().iter().zip(&items).zip(&paths) {
                    let size = if e.is_dir { measure(c, path, &|| p.cancelled()) } else { e.size.unwrap_or(0) };
                    p.size(if one { size * 2 } else { size });
                }
                Ok((pin_all(c, &paths), c.batches().0))
            })?;
            let most = if same && !cut && !one { most.max(1) } else { 1 };
            let mut taken: HashSet<String> = with(&pool, &b, |c| c.list(&dir))?.into_iter().map(|i| i.name.to_lowercase()).collect();
            let (secret_a, secret_b) = (|| store::secret(&a.id), || store::secret(&b.id));
            // Where the copy lands, its name free in `dir`; or why it can't be.
            let mut place = |i: usize| -> Result<String, String> {
                let (e, path) = (&items[i], &paths[i]);
                if e.is_dir && same && (dir == *path || dir.starts_with(&format!("{path}/"))) {
                    return Err(tr!("Não dá para copiar a pasta \"{}\" para dentro dela mesma.", "Can't copy the folder \"{}\" into itself.", e.name));
                }
                let name = free_name(&e.name, e.is_dir, &taken);
                taken.insert(name.to_lowercase());
                Ok(child(&dir, &name))
            };
            // One item: on the server's side, or down and up again.
            let each = |i: usize, p: &Progress, to: String| -> Result<(), String> {
                let (e, at) = (&items[i], &pinned[i]);
                let size = if e.is_dir { 0 } else { e.size.unwrap_or(0) };
                if one {
                    let mut lease = pool.take(&a, secret_a)?;
                    let done = copy_tree(&mut Ends::One(&mut *lease), at, &to, e.is_dir, size, p, 0);
                    if done.is_err() {
                        let _ = lease.remove(&to, e.is_dir);
                    }
                    lease.done();
                    done
                } else {
                    let (mut la, mut lb) = pool.take_two((&a, &secret_a), (&b, &secret_b))?;
                    // Within one WebDAV server, the server copies by itself.
                    let done = match same && !cut {
                        true => match la.copy(at, &to) {
                            Ok(true) => {
                                p.add_bytes(p.total.load(std::sync::atomic::Ordering::Relaxed));
                                Ok(())
                            }
                            Ok(false) => copy_tree(&mut Ends::Two(&mut *la, &mut *lb), at, &to, e.is_dir, size, p, 0),
                            Err(e) => Err(e),
                        },
                        false => copy_tree(&mut Ends::Two(&mut *la, &mut *lb), at, &to, e.is_dir, size, p, 0),
                    };
                    if done.is_err() {
                        let _ = lb.remove(&to, e.is_dir);
                    }
                    // Recortar: the original goes once all of it arrived.
                    let done = done.and_then(|_| if cut { la.remove(at, e.is_dir) } else { Ok(()) });
                    la.done();
                    lb.done();
                    done
                }
            };
            let next = std::sync::atomic::AtomicUsize::new(0);
            let mut more = true;
            while more {
                let mut rows = Vec::new();
                while rows.len() < most {
                    let Some((i, p)) = batch.next(&next) else {
                        more = false;
                        break;
                    };
                    match place(i) {
                        Ok(to) => rows.push((i, p, to)),
                        Err(e) => p.end(Err(e)),
                    }
                }
                match rows.len() {
                    0 => {}
                    1 => {
                        let (i, p, to) = rows.pop().expect("one row");
                        p.end(each(i, p, to));
                    }
                    // Copies within one server that has a batch for them, together.
                    _ => {
                        let copies: Vec<(String, String)> = rows.iter().map(|(i, _, to)| (pinned[*i].clone(), to.clone())).collect();
                        let copied = match pool.take(&a, secret_a) {
                            Ok(mut lease) => {
                                let r = lease.copy_many(&copies);
                                lease.done();
                                r
                            }
                            Err(e) => vec![Err(e); copies.len()],
                        };
                        for ((i, p, to), r) in rows.into_iter().zip(copied) {
                            p.end(match r {
                                Ok(true) => {
                                    p.add_bytes(p.total.load(std::sync::atomic::Ordering::Relaxed));
                                    Ok(())
                                }
                                Ok(false) => each(i, p, to),
                                Err(e) => Err(e),
                            });
                        }
                    }
                }
            }
            batch.summary()
        });
        emit(Msg::Done { reload, result });
    });
}

// ---------------------------------------------------------------------------------------------
// Between a server and Google Drive

/// A temporary file, deleted when dropped.
struct Temp(PathBuf);

impl Temp {
    fn new() -> Result<Temp, String> {
        let dir = std::env::temp_dir().join("OpenExplorer-copiar");
        std::fs::create_dir_all(&dir).map_err(|e| tr!("Não consegui criar a pasta temporária: {e}", "Couldn't create the temporary folder: {e}"))?;
        // A number of its own for each, so jobs running at once never share one.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(Temp(dir.join(format!("{:x}-{n:x}", std::process::id()))))
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The shared drive a Drive location is in, which listing its folders needs.
fn shared_drive(loc: &Location) -> Option<String> {
    match loc {
        Location::Drive { space: Space::Shared { id, .. }, .. } => Some(id.clone()),
        _ => None,
    }
}

/// What a Drive folder holds, all its pages.
fn drive_list(d: &Drive, folder: &str, shared: Option<&str>) -> Result<Vec<DriveFile>, String> {
    let mut out = Vec::new();
    d.list(folder, shared, |page, _| {
        out.extend(page);
        true
    })?;
    Ok(out)
}

/// Copies (or with `cut` moves) `items` of the server location `from` into the Drive folder
/// `to` shows, each file through a temporary one.
pub fn server_to_drive(s: &mut State, from: &Location, items: Vec<Entry>, to: &Location, cut: bool, reload: Vec<Location>) {
    let (Location::Server { server, .. }, Some((email, parent))) = (from, drive_folder(to)) else { return };
    let server = (**server).clone();
    let shared = shared_drive(to);
    let paths: Vec<String> = items.iter().filter_map(|e| path_of(from, e)).collect();
    let op = if cut { Op::Move } else { Op::Copy };
    let batch = transfers::queue(&mut s.transfers, s.waker.clone(), op, items.iter().map(|e| (end(from, Some(&e.name)), End::drive(to, None))));
    let pool = s.servers.pool.clone();
    let work = move |d: &Drive| {
        with(&pool, &server, |c| {
            // Down and then up: the bar counts the bytes twice.
            for ((p, e), path) in batch.rows().iter().zip(&items).zip(&paths) {
                p.size(2 * if e.is_dir { measure(c, path, &|| p.cancelled()) } else { e.size.unwrap_or(0) });
            }
            let mut taken: HashSet<String> = drive_list(d, &parent, shared.as_deref())?.into_iter().map(|f| f.name.to_lowercase()).collect();
            // Each item as it is now, whatever Recortar does to the others meanwhile.
            let paths = pin_all(c, &paths);
            batch.run(|i, p| {
                let e = &items[i];
                let name = free_name(&e.name, e.is_dir, &taken);
                taken.insert(name.to_lowercase());
                up(c, d, &paths[i], e.is_dir, &parent, &name, p, 0)?;
                if cut { c.remove(&paths[i], e.is_dir) } else { Ok(()) }
            })
        })
    };
    drive_task(s, &email, t("copiar para o Drive", "copy to Drive"), work, move |result| Msg::Done { reload, result });
}

/// A file or a folder and all it holds, from `path` on the server into Drive folder `parent`
/// as `name`.
#[allow(clippy::too_many_arguments)]
fn up(c: &mut dyn Conn, d: &Drive, path: &str, is_dir: bool, parent: &str, name: &str, p: &Progress, depth: usize) -> Result<(), String> {
    if p.cancelled() {
        return Err(CANCELLED.into());
    }
    if is_dir {
        if depth > DEEPEST {
            return Err(tr!("{path}: pastas demais umas dentro das outras (um link que volta para cima?).", "{path}: too many folders inside one another (a link that loops back up?)."));
        }
        let folder = d.create_folder(name, parent)?;
        let filled = c.list(path).and_then(|items| items.iter().try_for_each(|it| up(c, d, &child(path, &it.name), it.is_dir, &folder.id, &it.name, p, depth + 1)));
        // A copy that stopped leaves no half-filled folder in Drive: it goes to the trash.
        if filled.is_err() && depth == 0 {
            let _ = d.trash(&folder.id);
        }
        return filled;
    }
    let tmp = Temp::new()?;
    let mut file = std::fs::File::create(&tmp.0).map_err(|e| tr!("Não consegui criar o arquivo temporário: {e}", "Couldn't create the temporary file: {e}"))?;
    let mut last = 0;
    c.download(path, &mut file, &mut |n| {
        p.add_bytes(n - last);
        last = n;
        !p.cancelled()
    })
    .map_err(|e| if p.cancelled() { CANCELLED.into() } else { e })?;
    drop(file);
    let mut last = 0;
    d.upload(&tmp.0, parent, name, &mut |n| {
        p.add_bytes(n.saturating_sub(last));
        last = n;
        !p.cancelled()
    })
    .map(|_| ())
    .map_err(|e| if p.cancelled() { CANCELLED.into() } else { e })
}

/// Copies (or with `cut` moves) `items` of the Drive location `from` into the server folder `to`
/// shows, each file through a temporary one. Google's own formats (Docs, Sheets...) go as the
/// files they export to, as a download does.
pub fn drive_to_server(s: &mut State, from: &Location, items: Vec<Entry>, to: &Location, cut: bool, reload: Vec<Location>) {
    let (Location::Drive { email, .. }, Some((server, dir))) = (from, target(to)) else { return };
    let (email, server) = (email.clone(), server.clone());
    let shared = shared_drive(from);
    let files: Vec<DriveFile> = items.iter().map(crate::actions::drive_file).collect();
    let op = if cut { Op::Move } else { Op::Copy };
    let batch = transfers::queue(&mut s.transfers, s.waker.clone(), op, items.iter().map(|e| (End::drive(from, Some(&e.name)), end(to, None))));
    let pool = s.servers.pool.clone();
    let work = move |d: &Drive| {
        with(&pool, &server, |c| {
            // Down and then up: the bar counts the bytes twice. A folder's size isn't known.
            for (p, f) in batch.rows().iter().zip(&files) {
                if let (false, Some(size)) = (f.is_dir, f.size) {
                    p.size(2 * size);
                }
            }
            let mut taken: HashSet<String> = c.list(&dir)?.into_iter().map(|i| i.name.to_lowercase()).collect();
            batch.run(|i, p| {
                let f = &files[i];
                let is_dir = f.is_dir;
                let name = free_name(&crate::model::shown(&openexplorer_gdrive::download::local_name(f)), is_dir, &taken);
                taken.insert(name.to_lowercase());
                let target = child(&dir, &name);
                if !fetchable(f) {
                    return Err(tr!("\"{}\" é um item do Google que não vira arquivo (formulário, atalho para arquivo…): não dá para copiá-lo.", "\"{}\" is a Google item that can't become a file (a form, a shortcut to a file…): it can't be copied.", f.name));
                }
                let left_out = match down(c, d, f, &target, shared.as_deref(), p, 0) {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = c.remove(&target, is_dir);
                        return Err(e);
                    }
                };
                match (cut, left_out) {
                    (true, 0) => d.trash(&f.id).map(|_| ()),
                    // Recortar keeps the original when something in it couldn't come.
                    (true, n) => Err(tr!("Copiado, mas {n} item(ns) do Google que não viram arquivo ficaram de fora; o original continua no Drive.", "Copied, but {n} Google item(s) that can't become files were left out; the original stays in Drive.")),
                    (false, _) => Ok(()),
                }
            })
        })
    };
    drive_task(s, &email, t("copiar do Drive", "copy from Drive"), work, move |result| Msg::Done { reload, result });
}

/// Whether a Drive item has bytes to copy: a folder, a file, or one of Google's formats that
/// exports to a file (not a form, a site, a shortcut to a file).
fn fetchable(f: &DriveFile) -> bool {
    f.is_dir || !f.mime.starts_with("application/vnd.google-apps.") || openexplorer_gdrive::download::export_format(&f.mime).is_some()
}

/// A Drive file or folder (a shortcut to a folder as the folder) and all it holds, to `target`
/// on the server. Says how many items inside had no bytes to copy and stayed out.
fn down(c: &mut dyn Conn, d: &Drive, f: &DriveFile, target: &str, shared: Option<&str>, p: &Progress, depth: usize) -> Result<usize, String> {
    if p.cancelled() {
        return Err(CANCELLED.into());
    }
    if f.is_dir {
        if depth > DEEPEST {
            return Err(tr!("{}: pastas demais umas dentro das outras.", "{}: too many folders inside one another.", f.name));
        }
        c.mkdir(target)?;
        let folder = f.target.clone().unwrap_or_else(|| f.id.clone());
        let mut taken = HashSet::new();
        let mut left_out = 0;
        for kid in drive_list(d, &folder, shared)? {
            if !fetchable(&kid) {
                left_out += 1;
                continue;
            }
            // Drive allows two items of one name in a folder; a server doesn't.
            let name = free_name(&crate::model::shown(&openexplorer_gdrive::download::local_name(&kid)), kid.is_dir, &taken);
            taken.insert(name.to_lowercase());
            left_out += down(c, d, &kid, &child(target, &name), shared, p, depth + 1)?;
        }
        return Ok(left_out);
    }
    let tmp = Temp::new()?;
    let mut last = 0;
    let len = d
        .download(f, &tmp.0, false, &mut |n| {
            p.add_bytes(n.saturating_sub(last));
            last = n;
            !p.cancelled()
        })
        .map_err(|e| if p.cancelled() { CANCELLED.into() } else { e })?;
    let mut file = std::fs::File::open(&tmp.0).map_err(|e| tr!("Não consegui ler o arquivo temporário: {e}", "Couldn't read the temporary file: {e}"))?;
    let mut last = 0;
    c.upload(target, &mut file, len, &mut |n| {
        p.add_bytes(n - last);
        last = n;
        !p.cancelled()
    })
    .map(|_| 0)
    .map_err(|e| if p.cancelled() { CANCELLED.into() } else { e })
}

// ---------------------------------------------------------------------------------------------
// Dragged out of a server pane

/// A server's items dragged out of the app as virtual files (see [`crate::dragout`]): listed
/// when the drag starts, each file's bytes pulled through a connection of its own while the
/// target reads them.
#[cfg(windows)]
pub struct ServerFiles {
    pub pool: std::sync::Arc<openexplorer_remote::Pool>,
    pub server: openexplorer_remote::Server,
    /// Each dragged item's path on the server, and the item.
    pub top: Vec<(String, Entry)>,
}

#[cfg(windows)]
impl crate::dragout::Files for ServerFiles {
    fn list(&self, stop: &std::sync::atomic::AtomicBool) -> Result<Vec<crate::dragout::Virtual>, String> {
        with(&self.pool, &self.server, |c| {
            let mut out = Vec::new();
            let mut names = HashSet::new();
            for (path, e) in &self.top {
                described(c, path, &e.name, e.is_dir, e.size, e.modified, "", &mut names, &mut out, stop, 0)?;
            }
            Ok(out)
        })
    }

    fn open(&self, file: &crate::dragout::File, from: u64) -> Result<Box<dyn Read + Send>, String> {
        let crate::dragout::Fetch::Server(path) = &file.fetch else { return Err(t("Arquivo de outro lugar.", "File from another place.").into()) };
        let (tx, rx) = sync_channel(8);
        let (pool, server, path) = (self.pool.clone(), self.server.clone(), path.clone());
        std::thread::spawn(move || {
            let mut pipe = PipeIn(tx);
            // The target letting go of the stream closes the pipe, which stops this.
            if let Err(e) = with(&pool, &server, |c| c.download(&path, &mut pipe, &mut |_| true)) {
                let _ = pipe.0.send(Err(e));
            }
        });
        let mut r = PipeOut { rx, chunk: Vec::new(), at: 0 };
        // From a place past the start: the bytes before it are read and let go.
        io::copy(&mut (&mut r).take(from), &mut io::sink()).map_err(|e| e.to_string())?;
        Ok(Box::new(r))
    }
}

/// Adds item `name` at `path` (and for a folder all it holds) to the description, under
/// `prefix`. Names are made fit for Windows and kept apart there ("Nota" and "nota", "a:b" and
/// "a_b" would land on one file); links to folders aren't followed.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn described(
    c: &mut dyn Conn,
    path: &str,
    name: &str,
    is_dir: bool,
    size: Option<u64>,
    modified: Option<std::time::SystemTime>,
    prefix: &str,
    names: &mut HashSet<String>,
    out: &mut Vec<crate::dragout::Virtual>,
    stop: &std::sync::atomic::AtomicBool,
    depth: usize,
) -> Result<(), String> {
    use crate::dragout::{Fetch, File, Virtual};
    if stop.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(CANCELLED.into());
    }
    let name = free_name(&crate::ops::windows_name(name), is_dir, names);
    names.insert(name.to_lowercase());
    let at = if prefix.is_empty() { name.clone() } else { format!("{prefix}\\{name}") };
    // The description holds paths of up to 259 characters; a longer one stays out.
    if at.encode_utf16().count() > 259 {
        return Ok(());
    }
    if !is_dir {
        out.push(Virtual { path: at, file: Some(File { name, size, modified, fetch: Fetch::Server(path.to_string()) }) });
        return Ok(());
    }
    out.push(Virtual { path: at.clone(), file: None });
    if depth >= DEEPEST {
        return Ok(());
    }
    let mut inner = HashSet::new();
    for it in c.list(path)? {
        if it.is_dir && it.link {
            continue;
        }
        described(c, &child(path, &it.name), &it.name, it.is_dir, it.size, it.modified, &at, &mut inner, out, stop, depth + 1)?;
    }
    Ok(())
}
