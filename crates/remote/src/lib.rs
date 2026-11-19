//! FTP, SFTP and WebDAV servers for Open Explorer: the servers the user added, their passwords in
//! the OS credential store, and one blocking connection type the panes list and copy through.
//!
//! Everything here blocks; the app runs it on background threads.

mod b2;
mod dav;
mod dropbox;
mod ftp;
mod mega;
mod onedrive;
mod sftp;
pub mod store;
mod tls;

use std::io::{Read, Write};
use std::collections::HashMap;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use openexplorer_i18n::tr;
use serde::{Deserialize, Serialize};

pub use store::Store;
pub use tls::{Rejected, fingerprint};

/// Errors are shown to the user as they are, so they're plain sentences in the interface's
/// language (see `openexplorer_i18n`).
pub type Result<T> = std::result::Result<T, String>;

/// How long connecting, or a stalled answer, may take before giving up.
pub const TIMEOUT: Duration = Duration::from_secs(20);

/// Returned by a transfer whose progress callback asked it to stop.
pub const CANCELLED: &str = "Cancelado.";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Ftp,
    /// FTP over TLS, explicit (AUTH TLS on the usual port).
    Ftps,
    Sftp,
    /// WebDAV over plain HTTP.
    Dav,
    /// WebDAV over HTTPS.
    Davs,
    /// A MEGA account (see `mega.rs`). Never written to `servers.json`: MEGA accounts are kept
    /// apart, so an older app reading that list doesn't meet a protocol it can't read.
    Mega,
    /// A Dropbox account (see `dropbox.rs`), kept apart like MEGA's.
    Dropbox,
    /// A Backblaze B2 application key (see `b2.rs`); kept apart like MEGA.
    B2,
    /// A OneDrive account (see `onedrive.rs`), kept apart from `servers.json` as MEGA is.
    OneDrive,
}

impl Protocol {
    pub const ALL: [Protocol; 5] = [Protocol::Sftp, Protocol::Ftps, Protocol::Ftp, Protocol::Davs, Protocol::Dav];

    pub fn label(self) -> &'static str {
        match self {
            Protocol::Ftp => "FTP",
            Protocol::Ftps => "FTPS",
            Protocol::Sftp => "SFTP",
            Protocol::Dav => "WebDAV (http)",
            Protocol::Davs => "WebDAV (https)",
            Protocol::Mega => "MEGA",
            Protocol::Dropbox => "Dropbox",
            Protocol::B2 => "Backblaze B2",
            Protocol::OneDrive => "OneDrive",
        }
    }

    /// The protocol picker's label, short enough for its segment.
    pub fn short(self) -> &'static str {
        match self {
            Protocol::Dav => "WebDAV http",
            Protocol::Davs => "WebDAV",
            p => p.label(),
        }
    }

    /// The scheme of the server's address: `sftp://user@host/...`.
    pub fn scheme(self) -> &'static str {
        match self {
            Protocol::Ftp => "ftp",
            Protocol::Ftps => "ftps",
            Protocol::Sftp => "sftp",
            Protocol::Dav => "http",
            Protocol::Davs => "https",
            Protocol::Mega => "mega",
            Protocol::Dropbox => "dropbox",
            Protocol::B2 => "b2",
            Protocol::OneDrive => "onedrive",
        }
    }

    pub fn from_scheme(s: &str) -> Option<Protocol> {
        Some(match s.to_ascii_lowercase().as_str() {
            "ftp" => Protocol::Ftp,
            "ftps" | "ftpes" => Protocol::Ftps,
            "sftp" | "ssh" => Protocol::Sftp,
            "http" | "dav" | "webdav" => Protocol::Dav,
            "https" | "davs" | "webdavs" => Protocol::Davs,
            _ => return None,
        })
    }

    pub fn default_port(self) -> u16 {
        match self {
            Protocol::Ftp | Protocol::Ftps => 21,
            Protocol::Sftp => 22,
            Protocol::Dav => 80,
            Protocol::Davs | Protocol::Mega | Protocol::Dropbox | Protocol::B2 | Protocol::OneDrive => 443,
        }
    }

    /// Whether the password crosses the network in the clear.
    pub fn plain(self) -> bool {
        matches!(self, Protocol::Ftp | Protocol::Dav)
    }
}

/// A known cloud/NAS software preset running on top of a server protocol (such as WebDAV),
/// kept so the tree and edit dialog show its own icon and defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Brand {
    Nextcloud,
    OwnCloud,
}

fn deserialize_brand<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<Brand>, D::Error> {
    let raw = Option::<String>::deserialize(d)?;
    Ok(match raw.as_deref() {
        Some("nextcloud") => Some(Brand::Nextcloud),
        Some("owncloud") => Some(Brand::OwnCloud),
        _ => None,
    })
}

/// A server the user added. Its password, if any, is in the credential store under `id`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Server {
    pub id: String,
    /// What the tree shows.
    pub name: String,
    pub protocol: Protocol,
    #[serde(default, deserialize_with = "deserialize_brand", skip_serializing_if = "Option::is_none")]
    pub brand: Option<Brand>,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub user: String,
    /// The folder the server opens in, which its listing hangs from: "/" or "/home/ana", or a
    /// WebDAV address's path ("/remote.php/dav/files/ana").
    pub root: String,
    /// An SSH private key to log in with, instead of or besides the password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// SFTP: the host key's fingerprint seen when it was added; a different key is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_key: Option<String>,
    /// FTPS and https: the fingerprint of a certificate the usual authorities don't vouch for
    /// (a NAS's own) that the user chose to trust; any other such certificate is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert: Option<String>,
    /// How many connections to it may be open at once, from 1 to [`MOST_CONNECTIONS`]: some
    /// servers cap them per user.
    #[serde(default = "usual_connections")]
    pub connections: u8,
    /// FTPS that starts with TLS (implicit) rather than switching to it (explicit): port 990,
    /// or a server found not to greet in the clear when it was added.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub implicit: bool,
}

/// The connections a server gets unless told otherwise.
pub const USUAL_CONNECTIONS: u8 = 4;
/// The most a server may get.
pub const MOST_CONNECTIONS: u8 = 10;

fn usual_connections() -> u8 {
    USUAL_CONNECTIONS
}

impl Server {
    /// The server a MEGA account shows as in the panes: its e-mail is the user, its session in
    /// the MEGA crate's own store.
    pub fn mega(email: &str) -> Server {
        Server {
            id: format!("mega:{email}"),
            name: email.to_string(),
            protocol: Protocol::Mega,
            brand: None,
            host: "mega.nz".into(),
            port: 443,
            user: email.to_string(),
            root: "/".into(),
            key: None,
            host_key: None,
            cert: None,
            connections: 6,
            implicit: false,
        }
    }

    pub fn is_mega(&self) -> bool {
        self.protocol == Protocol::Mega
    }

    /// The server a Dropbox account shows as in the panes: its e-mail is the user, its refresh
    /// token in the Dropbox crate's own store.
    pub fn dropbox(email: &str) -> Server {
        Server { id: format!("dropbox:{email}"), protocol: Protocol::Dropbox, host: "dropbox.com".into(), ..Server::mega(email) }
    }

    pub fn is_dropbox(&self) -> bool {
        self.protocol == Protocol::Dropbox
    }

    /// The server a Backblaze B2 key shows as in the panes: its key ID is the user, its
    /// application key in the B2 crate's own store. `name` is what the tree shows.
    pub fn b2(key_id: &str, name: &str) -> Server {
        Server {
            id: format!("b2:{key_id}"),
            name: name.to_string(),
            protocol: Protocol::B2,
            brand: None,
            host: "backblazeb2.com".into(),
            port: 443,
            user: key_id.to_string(),
            root: "/".into(),
            key: None,
            host_key: None,
            cert: None,
            connections: 6,
            implicit: false,
        }
    }

    pub fn is_b2(&self) -> bool {
        self.protocol == Protocol::B2
    }

    /// The server a OneDrive account shows as: the drive's id is the user (the account's key in
    /// the OneDrive crate), its e-mail the name.
    pub fn onedrive(drive: &str, email: &str) -> Server {
        Server { id: format!("onedrive:{drive}"), name: email.to_string(), protocol: Protocol::OneDrive, host: "graph.microsoft.com".into(), user: drive.to_string(), connections: 6, ..Server::mega("") }
    }

    pub fn is_onedrive(&self) -> bool {
        self.protocol == Protocol::OneDrive
    }

    /// `sftp://ana@host:2222`, the port only when it isn't the usual one. Never the password.
    pub fn address(&self) -> String {
        if self.is_mega() {
            return format!("mega://{}", self.user);
        }
        if self.is_dropbox() {
            return format!("dropbox://{}", self.user);
        }
        // Bucket names are unique across B2: an address names the bucket, not the key.
        if self.is_b2() {
            return "b2://".into();
        }
        if self.is_onedrive() {
            return format!("onedrive://{}", self.name);
        }
        let user = if self.user.is_empty() { String::new() } else { format!("{}@", encode_user(&self.user)) };
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        let port = if self.port == self.protocol.default_port() { String::new() } else { format!(":{}", self.port) };
        format!("{}://{user}{host}{port}", self.protocol.scheme())
    }

    /// How many connections it gets, within bounds.
    pub fn limit(&self) -> usize {
        self.connections.clamp(1, MOST_CONNECTIONS) as usize
    }

    /// The full path on the server of `segs` under [`Server::root`].
    pub fn path(&self, segs: &[String]) -> String {
        join(&self.root, segs)
    }
}

fn encode_user(u: &str) -> String {
    u.replace('%', "%25").replace('@', "%40").replace(':', "%3A").replace('/', "%2F")
}

/// `root` + `segs`, with one `/` between each: `join("/a", ["b"])` is "/a/b".
pub fn join(root: &str, segs: &[String]) -> String {
    let mut out = root.trim_end_matches('/').to_string();
    for s in segs {
        out.push('/');
        out.push_str(s);
    }
    if out.is_empty() { "/".into() } else { out }
}

/// What a folder holds.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    /// A symbolic link: listed as what it points to, but deleting it removes only the link.
    pub link: bool,
}

/// Called with the bytes moved so far; `false` stops the transfer with [`CANCELLED`].
pub type Progress<'a> = &'a mut dyn FnMut(u64) -> bool;

/// A logged-in connection. FTP serves one command at a time, so a connection is never shared:
/// each job takes one from the [`Pool`] and gives it back.
pub trait Conn: Send {
    /// The folder `dir`'s items, without `.` and `..`.
    fn list(&mut self, dir: &str) -> Result<Vec<Item>>;
    /// The item at `path`, or `None` when there's nothing there.
    fn stat(&mut self, path: &str) -> Result<Option<Item>>;
    fn mkdir(&mut self, path: &str) -> Result<()>;
    /// Renames or moves, within the server. Fails if `to` exists.
    fn rename(&mut self, from: &str, to: &str) -> Result<()>;
    fn remove_file(&mut self, path: &str) -> Result<()>;
    /// An empty folder.
    fn remove_dir(&mut self, path: &str) -> Result<()>;
    /// Writes the file at `path` into `out`.
    fn download(&mut self, path: &str, out: &mut dyn Write, progress: Progress) -> Result<()>;
    /// Creates or replaces the file at `path` with the `len` bytes of `src`.
    fn upload(&mut self, path: &str, src: &mut dyn Read, len: u64, progress: Progress) -> Result<()>;
    /// The folder the server puts the user in at login.
    fn home(&mut self) -> Result<String>;

    /// Sends the file `local` on this computer to `path`, as [`Conn::upload`] does; a server
    /// that can tell it already has the same bytes may skip sending them.
    fn upload_file(&mut self, path: &str, local: &std::path::Path, progress: Progress) -> Result<()> {
        send_file(self, path, local, progress)
    }

    /// Whether `path` is a symbolic link itself.
    fn is_link(&mut self, path: &str) -> Result<bool>;

    /// Copies a file or a whole folder within the server, on the server's side; `false` when
    /// the protocol has no such command (FTP, SFTP), and the bytes have to go through the app.
    /// Fails if `to` exists.
    fn copy(&mut self, from: &str, to: &str) -> Result<bool> {
        let _ = (from, to);
        Ok(false)
    }

    /// [`Conn::rename`] of each `(from, to)`: each one's result, in order, one failing leaving
    /// the others to go on. A server with a batch command (Dropbox) does them in a few requests
    /// (see [`Conn::batches`]).
    fn rename_many(&mut self, moves: &[(String, String)]) -> Vec<Result<()>> {
        moves.iter().map(|(from, to)| self.rename(from, to)).collect()
    }

    /// [`Conn::copy`] of each `(from, to)`: each one's result, in order, as `rename_many`.
    fn copy_many(&mut self, copies: &[(String, String)]) -> Vec<Result<bool>> {
        copies.iter().map(|(from, to)| self.copy(from, to)).collect()
    }

    /// [`Conn::upload_file`] of each `(path, local)`: each one's result, in order.
    /// `progress(i, bytes)` hears file `i`'s bytes so far, never fewer than before; `false`
    /// stops that file with [`CANCELLED`], and the others go on. A server that takes files in
    /// batches sends them together (see [`Conn::batches`]).
    fn upload_files(&mut self, files: &[(String, std::path::PathBuf)], progress: &mut dyn FnMut(usize, u64) -> bool) -> Vec<Result<()>> {
        files.iter().enumerate().map(|(i, (path, local))| self.upload_file(path, local, &mut |n| progress(i, n))).collect()
    }

    /// How many items are worth giving [`Conn::rename_many`], [`Conn::copy_many`] and
    /// [`Conn::upload_files`] at once, and the largest file worth putting in an upload batch:
    /// `(1, 0)` where they go one by one anyway.
    fn batches(&self) -> (usize, u64) {
        (1, 0)
    }

    /// A file, or a folder and all it holds. A link, to a folder or not, goes alone: what it
    /// points to stays.
    fn remove(&mut self, path: &str, is_dir: bool) -> Result<()> {
        if !is_dir || self.is_link(path)? {
            return self.remove_file(path);
        }
        remove_tree(self, path)
    }

    /// A path that keeps naming the item at `path` while the items around it change, for a
    /// batch that works through several: the path itself, except where a listing's names can
    /// shift as items go (MEGA's twins, shown as "name (2)").
    fn pin(&mut self, path: &str) -> Result<String> {
        Ok(path.to_string())
    }

    /// Whether the connection still answers, to reuse it from the pool.
    fn alive(&mut self) -> bool;

    /// Whether it can go back to the pool once its job ends: not after a transfer stopped
    /// midway, which can leave the server's answers out of step.
    fn reusable(&self) -> bool {
        true
    }
}

/// [`Conn::upload`] of the file `local`.
pub(crate) fn send_file<C: Conn + ?Sized>(c: &mut C, path: &str, local: &std::path::Path, progress: Progress) -> Result<()> {
    let mut file = std::fs::File::open(local).map_err(|e| tr!("Não consegui abrir {}: {e}", "Couldn't open {}: {e}", local.display()))?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    c.upload(path, &mut file, len, progress)
}

/// Empties folder `path` and removes it, never going through a link.
fn remove_tree<C: Conn + ?Sized>(c: &mut C, path: &str) -> Result<()> {
    for item in c.list(path)? {
        let child = format!("{}/{}", path.trim_end_matches('/'), item.name);
        if item.is_dir && !item.link {
            remove_tree(c, &child)?;
        } else {
            c.remove_file(&child)?;
        }
    }
    c.remove_dir(path)
}

/// Logs in to `server` with the TLS trust `trust` gives. `secret` is its password (or the key's
/// passphrase).
fn open(server: &Server, secret: &str, trust: &tls::Trust) -> Result<Box<dyn Conn>> {
    match server.protocol {
        Protocol::Ftp | Protocol::Ftps => Ok(Box::new(ftp::Ftp::connect(server, secret, trust)?)),
        Protocol::Sftp => Ok(Box::new(sftp::Sftp::connect(server, secret)?)),
        Protocol::Dav | Protocol::Davs => Ok(Box::new(dav::Dav::connect(server, secret, trust)?)),
        Protocol::Mega => Ok(Box::new(mega::Mega::connect(server)?)),
        Protocol::Dropbox => Ok(Box::new(dropbox::Dropbox::connect(server)?)),
        Protocol::B2 => Ok(Box::new(b2::B2::connect(server)?)),
        Protocol::OneDrive => Ok(Box::new(onedrive::OneDrive::connect(server)?)),
    }
}

/// Logs in to `server`. `secret` is its password (or the key's passphrase).
pub fn connect(server: &Server, secret: Option<&str>) -> Result<Box<dyn Conn>> {
    let trust = tls::Trust::new(server.cert.clone());
    open(server, secret.unwrap_or(""), &trust).map_err(|e| trust.refusal(&server.host).unwrap_or(e))
}

/// Why adding a server didn't work.
#[derive(Clone, Debug, PartialEq)]
pub enum Refusal {
    /// Its certificate isn't one the usual authorities vouch for (nor the one trusted before):
    /// the user may trust it by its fingerprint.
    Untrusted(Rejected),
    Failed(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Untrusted(r) => f.write_str(&tr!("O certificado do servidor não é confiável: {} (impressão digital {}).", "The server's certificate is not trusted: it {} (fingerprint {}).", r.why, r.fingerprint)),
            Refusal::Failed(e) => f.write_str(e),
        }
    }
}

impl From<String> for Refusal {
    fn from(e: String) -> Refusal {
        Refusal::Failed(e)
    }
}

/// Logs in to a server being added: checks the address, user and password, and fills in what
/// only the server knows (its home folder when no folder was given, its host key).
pub fn probe(server: &mut Server, secret: Option<&str>) -> std::result::Result<(), Refusal> {
    let secret = secret.unwrap_or("");
    let trust = tls::Trust::new(server.cert.clone());
    // Which FTPS it is, found out once here rather than at every connection.
    if server.protocol == Protocol::Ftps {
        // Silence on connecting can also be an explicit server slow to greet (a reverse DNS
        // lookup, say): implicit only if a greeting does come over TLS.
        server.implicit = server.port == 990 || ftp::silent(&server.host, server.port) && ftp::greets_over_tls(&server.host, server.port, &trust);
    }
    let opened = match server.protocol {
        // A key seen before is checked, never taken anew: a server changed under the same
        // address is refused.
        Protocol::Sftp if server.host_key.is_none() => sftp::Sftp::connect_seeing(server, secret).map(|(conn, seen)| {
            server.host_key = Some(seen);
            Box::new(conn) as Box<dyn Conn>
        }),
        _ => open(server, secret, &trust),
    };
    let mut conn = match (opened, trust.rejected()) {
        (Ok(c), _) => c,
        (Err(_), Some(r)) => return Err(Refusal::Untrusted(r)),
        (Err(e), None) => return Err(Refusal::Failed(e)),
    };
    if server.root.trim().is_empty() {
        server.root = conn.home()?;
    }
    let root = server.root.clone();
    match conn.stat(&root)? {
        Some(i) if i.is_dir => Ok(()),
        Some(_) => Err(Refusal::Failed(tr!("{root} é um arquivo, não uma pasta.", "{root} is a file, not a folder."))),
        None => Err(Refusal::Failed(tr!("A pasta {root} não existe no servidor.", "The folder {root} doesn't exist on the server."))),
    }
}

/// An idle connection given back this recently is taken as alive without asking the server.
const FRESH: Duration = Duration::from_secs(15);
/// One idle longer is closed rather than asked: a connection a router dropped silently would
/// take a long timeout to answer.
const STALE: Duration = Duration::from_secs(120);

/// Open connections waiting to be used again, per server. Connecting takes a round trip or
/// several (TLS, SSH): a folder opened after another reuses the same login. At most
/// [`Server::limit`] are out at once, so a busy server (or one capped per user) isn't flooded by
/// a tree opening down a deep folder while a copy runs; another job waits for one to come back.
#[derive(Default)]
pub struct Pool {
    state: Mutex<Slots>,
    freed: Condvar,
}

struct Idle {
    id: String,
    conn: Box<dyn Conn>,
    since: Instant,
}

#[derive(Default)]
struct Slots {
    idle: Vec<Idle>,
    /// Connections out, per server.
    busy: HashMap<String, usize>,
    /// Bumped by [`Pool::forget`]: a connection made before (with the server's old address or
    /// password) isn't kept when its job ends.
    era: HashMap<String, u64>,
}

/// Closes connections on a thread of their own: closing politely waits on the network, which
/// mustn't hold the window or the pool.
fn close_later(conns: Vec<Box<dyn Conn>>) {
    if !conns.is_empty() {
        std::thread::spawn(move || drop(conns));
    }
}

/// A connection taken from the [`Pool`]: [`Lease::done`] gives it back for the next job;
/// dropped without it (after a panic, say) it's closed. Its place is freed either way.
pub struct Lease<'a> {
    conn: Option<Box<dyn Conn>>,
    pool: &'a Pool,
    id: String,
    era: u64,
}

impl Lease<'_> {
    pub fn done(mut self) {
        let Some(conn) = self.conn.take() else { return };
        let mut st = self.pool.state.lock().unwrap_or_else(|e| e.into_inner());
        if conn.reusable() && st.era.get(&self.id).copied().unwrap_or(0) == self.era {
            st.idle.push(Idle { id: self.id.clone(), conn, since: Instant::now() });
        } else {
            drop(st);
            close_later(vec![conn]);
        }
    }
}

impl std::ops::Deref for Lease<'_> {
    type Target = dyn Conn;
    fn deref(&self) -> &(dyn Conn + 'static) {
        self.conn.as_deref().expect("a lease holds its connection")
    }
}

impl std::ops::DerefMut for Lease<'_> {
    fn deref_mut(&mut self) -> &mut (dyn Conn + 'static) {
        self.conn.as_deref_mut().expect("a lease holds its connection")
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let mut st = self.pool.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = st.busy.get_mut(&self.id) {
            *n = n.saturating_sub(1);
        }
        drop(st);
        self.pool.freed.notify_all();
    }
}

impl Pool {
    /// A connection to `server`: an idle one that still answers, else a new login once fewer
    /// than [`Server::limit`] are out. `secret` gives the password, asked only for a new login.
    pub fn take(&self, server: &Server, secret: impl Fn() -> Option<String>) -> Result<Lease<'_>> {
        Ok(self.take_all(&[(server, &secret)])?.remove(0))
    }

    /// A connection to each of two servers (or two to one), taken together: a job that held one
    /// while waiting for the other could wait forever on another job doing the same. Two to one
    /// server need it to allow two.
    pub fn take_two(&self, a: (&Server, &dyn Fn() -> Option<String>), b: (&Server, &dyn Fn() -> Option<String>)) -> Result<(Lease<'_>, Lease<'_>)> {
        if a.0.id == b.0.id && a.0.limit() < 2 {
            return Err(tr!("{} só permite uma conexão por vez.", "{} allows only one connection at a time.", a.0.name));
        }
        let mut both = self.take_all(&[a, b])?;
        let second = both.pop().expect("two leases");
        Ok((both.pop().expect("two leases"), second))
    }

    fn take_all(&self, wants: &[(&Server, &dyn Fn() -> Option<String>)]) -> Result<Vec<Lease<'_>>> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // Idle ones gone stale are closed, never asked.
        let (stale, keep): (Vec<Idle>, Vec<Idle>) = std::mem::take(&mut st.idle).into_iter().partition(|i| i.since.elapsed() > STALE);
        st.idle = keep;
        close_later(stale.into_iter().map(|i| i.conn).collect());
        // Each server must have room for all it's asked for (idle ones count among those out:
        // a server's idle and busy connections together stay within its limit).
        let fits = |st: &Slots| {
            wants.iter().all(|(sv, _)| {
                let asked = wants.iter().filter(|(o, _)| o.id == sv.id).count();
                st.busy.get(&sv.id).copied().unwrap_or(0) + asked <= sv.limit()
            })
        };
        while !fits(&st) {
            st = self.freed.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        let mut found = Vec::new();
        for (sv, _) in wants {
            *st.busy.entry(sv.id.clone()).or_default() += 1;
            let idle = st.idle.iter().position(|i| i.id == sv.id).map(|i| st.idle.swap_remove(i));
            found.push((idle, st.era.get(&sv.id).copied().unwrap_or(0)));
        }
        drop(st);
        // From here each place is its lease's: an error frees them as the leases drop.
        let mut leases: Vec<Lease<'_>> = wants.iter().zip(&found).map(|((sv, _), (_, era))| Lease { conn: None, pool: self, id: sv.id.clone(), era: *era }).collect();
        for ((lease, (sv, secret)), (idle, _)) in leases.iter_mut().zip(wants).zip(found) {
            // One used a moment ago is alive; an older one is asked, and replaced if gone.
            let reused = idle.and_then(|mut i| {
                if i.since.elapsed() < FRESH || i.conn.alive() {
                    Some(i.conn)
                } else {
                    close_later(vec![i.conn]);
                    None
                }
            });
            lease.conn = Some(match reused {
                Some(c) => c,
                None => connect(sv, secret().as_deref())?,
            });
        }
        Ok(leases)
    }

    /// Closes the idle connections to `id`, and those out when they come back: its address or
    /// password changed, or it was removed.
    pub fn forget(&self, id: &str) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        *st.era.entry(id.to_string()).or_default() += 1;
        let (gone, keep): (Vec<Idle>, Vec<Idle>) = std::mem::take(&mut st.idle).into_iter().partition(|i| i.id == id);
        st.idle = keep;
        drop(st);
        close_later(gone.into_iter().map(|i| i.conn).collect());
    }
}

/// Copies `src` to `out` in chunks, reporting the bytes so far.
pub(crate) fn pump(src: &mut dyn Read, out: &mut dyn Write, progress: Progress) -> Result<u64> {
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    loop {
        let n = match src.read(&mut buf) {
            Ok(0) => return Ok(done),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(tr!("A transferência parou: {e}", "The transfer stopped: {e}")),
        };
        out.write_all(&buf[..n]).map_err(|e| tr!("A transferência parou: {e}", "The transfer stopped: {e}"))?;
        done += n as u64;
        if !progress(done) {
            return Err(CANCELLED.into());
        }
    }
}

/// The message for an I/O error while reaching a host.
pub(crate) fn unreachable(host: &str, e: &std::io::Error) -> String {
    use std::io::ErrorKind::*;
    match e.kind() {
        ConnectionRefused => tr!("{host} recusou a conexão: o servidor não está rodando nessa porta.", "{host} refused the connection: the server isn't running on that port."),
        TimedOut | WouldBlock => tr!("{host} não respondeu a tempo.", "{host} didn't answer in time."),
        NotFound => tr!("Não encontrei o servidor {host}.", "Couldn't find the server {host}."),
        _ => tr!("Não consegui conectar a {host}: {e}", "Couldn't connect to {host}: {e}"),
    }
}

/// The socket address of `host:port`, for a connect with a timeout.
pub(crate) fn resolve(host: &str, port: u16) -> Result<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;
    (host, port)
        .to_socket_addrs()
        .map_err(|_| tr!("Não encontrei o servidor {host}.", "Couldn't find the server {host}."))?
        .next()
        .ok_or_else(|| tr!("Não encontrei o servidor {host}.", "Couldn't find the server {host}."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(protocol: Protocol, port: u16) -> Server {
        Server { id: "x".into(), name: "x".into(), protocol, brand: None, host: "nas".into(), port, user: "ana@casa".into(), root: "/".into(), key: None, host_key: None, cert: None, connections: 4, implicit: false }
    }

    #[test]
    fn addresses_leave_out_the_usual_port_and_never_carry_a_password() {
        assert_eq!(server(Protocol::Sftp, 22).address(), "sftp://ana%40casa@nas");
        assert_eq!(server(Protocol::Ftp, 2121).address(), "ftp://ana%40casa@nas:2121");
        assert_eq!(server(Protocol::Davs, 443).address(), "https://ana%40casa@nas");
    }

    /// A server in memory: each path, whether it's a folder, whether it's a link (to `/alvo`).
    struct Fake(Vec<(String, bool, bool)>);

    impl Conn for Fake {
        fn list(&mut self, dir: &str) -> Result<Vec<Item>> {
            // A link to a folder lists what the folder holds.
            let real = if self.0.iter().any(|(p, _, l)| p == dir && *l) { "/alvo/".to_string() } else { format!("{}/", dir.trim_end_matches('/')) };
            Ok(self
                .0
                .iter()
                .filter(|(p, ..)| p.starts_with(&real) && !p[real.len()..].contains('/'))
                .map(|(p, d, l)| Item { name: p[real.len()..].into(), is_dir: *d, size: None, modified: None, link: *l })
                .collect())
        }
        fn stat(&mut self, _: &str) -> Result<Option<Item>> {
            unimplemented!()
        }
        fn mkdir(&mut self, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn rename(&mut self, _: &str, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn remove_file(&mut self, path: &str) -> Result<()> {
            self.0.retain(|(p, ..)| p != path);
            Ok(())
        }
        fn remove_dir(&mut self, path: &str) -> Result<()> {
            assert!(!self.0.iter().any(|(p, ..)| p.starts_with(&format!("{path}/"))), "{path} isn't empty");
            self.0.retain(|(p, ..)| p != path);
            Ok(())
        }
        fn download(&mut self, _: &str, _: &mut dyn Write, _: Progress) -> Result<()> {
            unimplemented!()
        }
        fn upload(&mut self, _: &str, _: &mut dyn Read, _: u64, _: Progress) -> Result<()> {
            unimplemented!()
        }
        fn home(&mut self) -> Result<String> {
            unimplemented!()
        }
        fn is_link(&mut self, path: &str) -> Result<bool> {
            Ok(self.0.iter().any(|(p, _, l)| p == path && *l))
        }
        fn alive(&mut self) -> bool {
            true
        }
    }

    #[test]
    fn deleting_a_link_leaves_what_it_points_to() {
        let tree = |extra: &[(&str, bool, bool)]| {
            let mut v: Vec<(String, bool, bool)> = [("/alvo", true, false), ("/alvo/importante.txt", false, false)].iter().chain(extra).map(|(p, d, l)| (p.to_string(), *d, *l)).collect();
            v.sort();
            v
        };
        // The link itself, deleted as the folder it shows as.
        let mut c = Fake(tree(&[("/atalho", true, true)]));
        c.remove("/atalho", true).unwrap();
        assert_eq!(c.0, tree(&[]));
        // A folder holding a link: the folder goes, the link with it, the target stays.
        let mut c = Fake(tree(&[("/pasta", true, false), ("/pasta/atalho", true, true), ("/pasta/a.txt", false, false)]));
        c.remove("/pasta", true).unwrap();
        assert_eq!(c.0, tree(&[]));
    }

    /// A list saved before servers had certificates, a connection limit or implicit FTPS
    /// still loads, with the usual values.
    #[test]
    fn an_older_server_list_still_loads() {
        let old = r#"[{"id":"s1","name":"NAS","protocol":"ftps","host":"nas","port":21,"user":"ana","root":"/"},{"id":"s2","name":"Futuro","protocol":"davs","brand":"synology","host":"nas","port":443,"user":"ana","root":"/"}]"#;
        let list: Vec<Server> = serde_json::from_str(old).unwrap();
        assert_eq!((list[0].cert.as_ref(), list[0].connections, list[0].implicit), (None, USUAL_CONNECTIONS, false));
        assert_eq!(list[0].limit(), 4);
        assert_eq!(list[1].brand, None);
    }

    #[test]
    fn paths_hang_from_the_root() {
        assert_eq!(join("/", &[]), "/");
        assert_eq!(join("/", &["a".into(), "b c".into()]), "/a/b c");
        assert_eq!(join("/home/ana/", &["x".into()]), "/home/ana/x");
        assert_eq!(join("", &[]), "/");
    }
}
