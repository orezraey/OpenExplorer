//! FTP and FTPS (explicit, AUTH TLS, or implicit, TLS from the start), through suppaftp.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, SystemTime};

use openexplorer_i18n::{t, tr};
use suppaftp::list::ListParser;
use suppaftp::types::FileType;
use suppaftp::{FtpError, RustlsConnector, RustlsFtpStream, Status};

use crate::{Conn, Item, Progress, Protocol, Result, Server, TIMEOUT, pump, resolve, unreachable};

pub struct Ftp {
    s: RustlsFtpStream,
    /// The server lists folders with MLSD, whose dates and sizes don't need guessing.
    mlsd: bool,
    /// Whether the server takes MFMT, which gives a file a modification time: as its features
    /// say, or not known yet when they couldn't be read.
    mfmt: Option<bool>,
    /// A transfer stopped midway (ABOR): the server's answers may be out of step from here.
    broken: bool,
}

/// How long a stalled answer may take on an open connection.
const STALL: Duration = Duration::from_secs(60);

fn socket(addr: std::net::SocketAddr, host: &str) -> Result<TcpStream> {
    let tcp = TcpStream::connect_timeout(&addr, TIMEOUT).map_err(|e| unreachable(host, &e))?;
    let _ = tcp.set_read_timeout(Some(STALL));
    let _ = tcp.set_write_timeout(Some(STALL));
    Ok(tcp)
}

/// Whether the server at `host:port` says nothing on connecting: it waits for TLS first
/// (implicit FTPS), rather than greeting in the clear and switching on AUTH TLS (explicit).
pub(crate) fn silent(host: &str, port: u16) -> bool {
    let Ok(addr) = resolve(host, port) else { return false };
    let Ok(tcp) = TcpStream::connect_timeout(&addr, TIMEOUT) else { return false };
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(4)));
    matches!(tcp.peek(&mut [0u8; 1]), Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
}

/// Whether the server at `host:port` greets once TLS is up (implicit FTPS). A certificate
/// turned down counts: TLS was what it spoke, and `trust` notes the certificate.
pub(crate) fn greets_over_tls(host: &str, port: u16, trust: &crate::tls::Trust) -> bool {
    let Ok(addr) = resolve(host, port) else { return false };
    // suppaftp waits on this connection without a limit: a server that answers nothing at all
    // would hold the check forever, so it runs aside, and is given up on after a while.
    let (tx, rx) = std::sync::mpsc::channel();
    let (config, host_name) = (crate::tls::config(trust), host.to_string());
    std::thread::spawn(move || {
        let _ = tx.send(RustlsFtpStream::connect_secure_implicit(addr, RustlsConnector::from(config), &host_name).is_ok());
    });
    rx.recv_timeout(TIMEOUT).unwrap_or(false) || trust.rejected().is_some()
}

impl Ftp {
    pub fn connect(server: &Server, secret: &str, trust: &crate::tls::Trust) -> Result<Ftp> {
        let host = server.host.as_str();
        let addr = resolve(host, server.port)?;
        let tls = server.protocol == Protocol::Ftps;
        // Implicit FTPS (port 990, and servers that greet only over TLS) starts with TLS;
        // explicit FTPS greets in the clear, then switches.
        let implicit = tls && (server.port == 990 || server.implicit);
        let mut s = if implicit {
            let s = RustlsFtpStream::connect_secure_implicit(addr, RustlsConnector::from(crate::tls::config(trust)), host).map_err(|e| secure(host, e))?;
            // suppaftp opens this one without the timeouts the others get.
            let _ = s.get_ref().set_read_timeout(Some(STALL));
            let _ = s.get_ref().set_write_timeout(Some(STALL));
            s
        } else {
            RustlsFtpStream::connect_with_stream(socket(addr, host)?).map_err(|e| said(host, e))?
        };
        // A server behind NAT answers PASV with its private address: use the one we reached.
        s.set_passive_nat_workaround(true);
        let named = host.to_string();
        let mut s = s.passive_stream_builder(move |addr| socket(addr, &named).map_err(|e| FtpError::ConnectionError(std::io::Error::other(e))));
        if tls && !implicit {
            s = s.into_secure(RustlsConnector::from(crate::tls::config(trust)), host).map_err(|e| secure(host, e))?;
        }
        let user = if server.user.is_empty() { "anonymous" } else { server.user.as_str() };
        // Anonymous FTP asks for some password, by custom an e-mail; many refuse an empty one.
        let secret = if server.user.is_empty() && secret.is_empty() { "anonymous@" } else { secret };
        s.login(user, secret).map_err(|e| match e {
            FtpError::UnexpectedResponse(r) if r.status == Status::NotLoggedIn => tr!("{host} recusou o usuário ou a senha.", "{host} refused the user name or password."),
            e => said(host, e),
        })?;
        // Implicit FTPS asks for the data connections to be encrypted too (suppaftp does it only
        // on the explicit switch, and always encrypts them): a server that won't can't be used.
        if implicit {
            for command in ["PBSZ 0", "PROT P"] {
                s.custom_command(command, &[Status::CommandOk])
                    .map_err(|_| {
                        tr!(
                            "{host} usa FTPS, mas não aceita criptografar as transferências (recusou {command}). Tente o servidor como FTP, se ele aceitar.",
                            "{host} uses FTPS, but won't encrypt the transfers (it refused {command}). Try the server as FTP, if it takes that."
                        )
                    })?;
            }
        }
        s.transfer_type(FileType::Binary).map_err(|e| said(host, e))?;
        // What the server lists among its features, or nothing known when the list can't be
        // read: suppaftp gives up on a list with a line it doesn't expect (rclone's has a blank
        // one), though the server has the features all the same.
        let features = s.feat().ok();
        let lists = |f: &str| features.as_ref().map(|all| all.keys().any(|k| k.eq_ignore_ascii_case(f)));
        if lists("UTF8") != Some(false) {
            let _ = s.opts("UTF8", Some("ON"));
        }
        // With no list to go by, MLST is tried on the root: where it answers, folders are
        // listed by MLSD, with dates to the second (LIST gives the minute, or only the day for
        // a file of last year, and a date compared against one never matches).
        let mlsd = match lists("MLST") {
            Some(listed) => listed,
            None => s.mlst(Some("/")).is_ok(),
        };
        Ok(Ftp { s, mlsd, mfmt: lists("MFMT"), broken: false })
    }

    /// Whether `path` is a folder: it can be entered.
    fn enters(&mut self, path: &str) -> bool {
        self.s.cwd(path).is_ok()
    }
}

/// The command that gives `path` the modification time `at`: the time as `YYYYMMDDHHMMSS` in
/// UTC, to the second.
fn mfmt(path: &str, at: SystemTime) -> String {
    format!("MFMT {} {path}", chrono::DateTime::<chrono::Utc>::from(at).format("%Y%m%d%H%M%S"))
}

fn secure(host: &str, e: FtpError) -> String {
    match e {
        FtpError::UnexpectedResponse(_) => tr!("{host} não aceita FTPS: tente FTP.", "{host} doesn't take FTPS: try FTP."),
        e => said(host, e),
    }
}

/// What a message calls the server where the host isn't at hand.
fn server() -> &'static str {
    t("o servidor", "the server")
}

/// The message for an FTP error, from the server's own words when it gave some.
fn said(host: &str, e: FtpError) -> String {
    match e {
        FtpError::ConnectionError(e) => unreachable(host, &e),
        FtpError::UnexpectedResponse(r) => {
            let text = String::from_utf8_lossy(&r.body).trim().to_string();
            match r.status {
                Status::NotLoggedIn => tr!("{host} recusou o usuário ou a senha.", "{host} refused the user name or password."),
                Status::FileUnavailable => tr!("O servidor recusou: {text}", "The server refused: {text}"),
                Status::ExceededStorage => t("O servidor está sem espaço.", "The server is out of space.").into(),
                Status::BadFilename => tr!("O servidor não aceita esse nome: {text}", "The server doesn't take that name: {text}"),
                _ => tr!("O servidor respondeu: {text}", "The server answered: {text}"),
            }
        }
        FtpError::SecureError(m) => tr!("Erro de TLS com {host}: {m}", "TLS error with {host}: {m}"),
        e => tr!("Erro de FTP com {host}: {e}", "FTP error with {host}: {e}"),
    }
}

impl Conn for Ftp {
    fn list(&mut self, dir: &str) -> Result<Vec<Item>> {
        let host = server();
        let mut out = Vec::new();
        let mut links = Vec::new();
        if self.mlsd {
            for line in self.s.mlsd(Some(dir)).map_err(|e| said(host, e))? {
                // Lines the parser can't read are skipped rather than failing the folder.
                let Ok(f) = ListParser::parse_mlsd(&line) else { continue };
                let name = f.name().to_string();
                if name == "." || name == ".." || name.is_empty() {
                    continue;
                }
                if f.is_symlink() {
                    links.push(out.len());
                }
                let is_dir = f.is_directory();
                out.push(Item { name, is_dir, size: (!is_dir).then_some(f.size() as u64), modified: Some(f.modified()), link: f.is_symlink() });
            }
        } else {
            for line in self.s.list(Some(dir)).map_err(|e| said(host, e))? {
                let Ok(f) = ListParser::parse_posix(&line).or_else(|_| ListParser::parse_dos(&line)) else { continue };
                let name = f.name().to_string();
                if name == "." || name == ".." || name.is_empty() {
                    continue;
                }
                if f.is_symlink() {
                    links.push(out.len());
                }
                let is_dir = f.is_directory();
                out.push(Item { name, is_dir, size: (!is_dir).then_some(f.size() as u64), modified: Some(f.modified()), link: f.is_symlink() });
            }
        }
        // A link to a folder opens like the folder.
        for i in links {
            let path = format!("{}/{}", dir.trim_end_matches('/'), out[i].name);
            if self.enters(&path) {
                out[i].is_dir = true;
                out[i].size = None;
            }
        }
        Ok(out)
    }

    fn stat(&mut self, path: &str) -> Result<Option<Item>> {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            return Ok(Some(Item { name: "/".into(), is_dir: true, size: None, modified: None, link: false }));
        }
        let (parent, name) = trimmed.rsplit_once('/').unwrap_or(("", trimmed));
        let parent = if parent.is_empty() { "/" } else { parent };
        if self.mlsd {
            return match self.s.mlst(Some(trimmed)) {
                Ok(line) => {
                    let line = line.lines().find(|l| l.contains(';')).unwrap_or(&line).trim().to_string();
                    let Ok(f) = ListParser::parse_mlst(&line) else {
                        return Ok(self.enters(trimmed).then(|| Item { name: name.into(), is_dir: true, size: None, modified: None, link: false }));
                    };
                    let is_dir = f.is_directory();
                    Ok(Some(Item { name: name.into(), is_dir, size: (!is_dir).then_some(f.size() as u64), modified: Some(f.modified()), link: false }))
                }
                Err(FtpError::UnexpectedResponse(r)) if r.status == Status::FileUnavailable => Ok(None),
                Err(e) => Err(said(server(), e)),
            };
        }
        if self.enters(trimmed) {
            return Ok(Some(Item { name: name.into(), is_dir: true, size: None, modified: None, link: false }));
        }
        Ok(self.list(parent)?.into_iter().find(|i| i.name == name))
    }

    fn is_link(&mut self, path: &str) -> Result<bool> {
        let trimmed = path.trim_end_matches('/');
        let Some((parent, name)) = trimmed.rsplit_once('/') else { return Ok(false) };
        let parent = if parent.is_empty() { "/" } else { parent };
        Ok(self.list(parent)?.into_iter().any(|i| i.name == name && i.link))
    }

    fn mkdir(&mut self, path: &str) -> Result<()> {
        self.s.mkdir(path).map_err(|e| said(server(), e))
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        if self.stat(to)?.is_some() {
            return Err(tr!("Já existe um item chamado {}.", "There's already an item named {}.", to.rsplit('/').next().unwrap_or(to)));
        }
        self.s.rename(from, to).map_err(|e| said(server(), e))
    }

    fn remove_file(&mut self, path: &str) -> Result<()> {
        self.s.rm(path).map_err(|e| said(server(), e))
    }

    fn remove_dir(&mut self, path: &str) -> Result<()> {
        self.s.rmdir(path).map_err(|e| said(server(), e))
    }

    fn download(&mut self, path: &str, out: &mut dyn Write, progress: Progress) -> Result<()> {
        let mut stream = self.s.retr_as_stream(path).map_err(|e| said(server(), e))?;
        match pump(&mut stream, out, progress) {
            Ok(_) => stream.finish().map_err(|e| said(server(), e)),
            Err(e) => {
                self.broken = true;
                let _ = self.s.abort(stream);
                Err(e)
            }
        }
    }

    fn upload(&mut self, path: &str, src: &mut dyn Read, _len: u64, progress: Progress) -> Result<()> {
        let mut stream = self.s.put_with_stream(path).map_err(|e| said(server(), e))?;
        match pump(src, &mut stream, progress) {
            Ok(_) => stream.finish().map_err(|e| said(server(), e)),
            Err(e) => {
                self.broken = true;
                let _ = self.s.abort(stream);
                // What arrived of the file is no use.
                let _ = self.s.rm(path);
                Err(e)
            }
        }
    }

    fn sets_dates(&self) -> bool {
        true
    }

    /// MFMT, where the server lists it among its features: `MFMT 20240131235959 path`, in UTC.
    /// Not asked of a server that doesn't list it. One whose features couldn't be read is
    /// asked, and not again once it says it has no such command. A file's date refused says
    /// no more than that it wasn't kept.
    fn set_modified(&mut self, path: &str, at: SystemTime) -> Result<bool> {
        if self.mfmt == Some(false) {
            return Ok(false);
        }
        match self.s.custom_command(mfmt(path, at), &[Status::File]) {
            Ok(_) => {
                self.mfmt = Some(true);
                Ok(true)
            }
            Err(FtpError::UnexpectedResponse(r)) => {
                if self.mfmt.is_none() && matches!(r.status.code(), 500 | 502) {
                    self.mfmt = Some(false);
                }
                Ok(false)
            }
            Err(e) => {
                self.broken = true;
                Err(said(server(), e))
            }
        }
    }

    fn home(&mut self) -> Result<String> {
        self.s.pwd().map_err(|e| said(server(), e))
    }

    fn alive(&mut self) -> bool {
        !self.broken && self.s.noop().is_ok()
    }

    fn reusable(&self) -> bool {
        !self.broken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An FTP server's control connection, enough to log in: it lists `features`, takes MFMT
    /// unless the path holds "locked", and keeps every command it hears.
    fn fake(features: &'static [&'static str]) -> (Server, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let heard = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = heard.clone();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else { return };
            let mut out = stream.try_clone().unwrap();
            let mut say = move |line: &str| out.write_all(format!("{line}\r\n").as_bytes()).is_ok();
            say("220 ready");
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { return };
                log.lock().unwrap().push(line.clone());
                let (verb, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
                let answered = match verb.to_ascii_uppercase().as_str() {
                    "USER" => say("331 password"),
                    "PASS" => say("230 in"),
                    // A feature named "" is a blank line, as rclone's list has; "no MFMT" is a
                    // server that has no such command, whatever its list says.
                    "FEAT" => say("211-Features:") && features.iter().filter(|f| **f != "no MFMT").all(|f| say(&if f.is_empty() { String::new() } else { format!(" {f}") })) && say("211 End"),
                    "MLST" if features.iter().any(|f| f.starts_with("MLST")) => say("250-Start") && say(&format!(" type=dir;modify=20200102030405; {rest}")) && say("250 End"),
                    "MLST" | "MFMT" if verb.eq_ignore_ascii_case("MLST") || features.contains(&"no MFMT") => say("500 no such command"),
                    "MFMT" if rest.contains("locked") => say("550 not allowed"),
                    "MFMT" => say(&format!("213 Modify={}", rest.replacen(' ', "; ", 1))),
                    "QUIT" => return,
                    _ => say("200 fine"),
                };
                if !answered {
                    return;
                }
            }
        });
        let server = Server { id: "x".into(), name: "x".into(), protocol: Protocol::Ftp, brand: None, host: "127.0.0.1".into(), port, user: "u".into(), root: "/".into(), key: None, host_key: None, cert: None, connections: 4, implicit: false };
        (server, heard)
    }

    /// A server that lists MFMT is given a file's date in UTC to the second; one that refuses
    /// it for a file says so and goes on working; one that doesn't list it is never asked.
    #[test]
    fn a_files_date_goes_by_mfmt_where_the_server_lists_it() {
        // 2020-01-02 03:04:05 UTC, and a fraction that isn't sent.
        let at = SystemTime::UNIX_EPOCH + Duration::from_millis(1_577_934_245_700);
        assert_eq!(mfmt("/Photos/a b.txt", at), "MFMT 20200102030405 /Photos/a b.txt");

        let (server, heard) = fake(&["UTF8", "MFMT", "MLST type*;size*;modify*;"]);
        let mut c = Ftp::connect(&server, "p", &crate::tls::Trust::default()).unwrap();
        assert!(c.mfmt == Some(true) && c.mlsd);
        assert_eq!(c.set_modified("/Photos/a b.txt", at), Ok(true));
        assert_eq!(c.set_modified("/Photos/locked.txt", at), Ok(false));
        assert!(c.alive(), "a refused date leaves the connection as it was");
        assert_eq!(c.set_modified("/again.txt", at), Ok(true));
        let said: Vec<String> = heard.lock().unwrap().iter().filter(|l| l.starts_with("MFMT")).cloned().collect();
        assert_eq!(said, ["MFMT 20200102030405 /Photos/a b.txt", "MFMT 20200102030405 /Photos/locked.txt", "MFMT 20200102030405 /again.txt"]);

        let (server, heard) = fake(&["UTF8"]);
        let mut c = Ftp::connect(&server, "p", &crate::tls::Trust::default()).unwrap();
        assert!(c.mfmt == Some(false) && !c.mlsd);
        assert_eq!(c.set_modified("/a.txt", at), Ok(false));
        assert!(!heard.lock().unwrap().iter().any(|l| l.starts_with("MFMT") || l.starts_with("MLST")), "{:?}", heard.lock().unwrap());
    }

    /// A list of features that can't be read (a blank line in it, as rclone's has) says nothing
    /// of what the server can do: MLST is tried, so folders list by MLSD with dates to the
    /// second, and MFMT is tried on the first file and not again once the server has no such
    /// command.
    #[test]
    fn features_that_cant_be_read_are_found_by_trying() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_577_934_245);
        let commands = |heard: &std::sync::Mutex<Vec<String>>, verb: &str| heard.lock().unwrap().iter().filter(|l| l.starts_with(verb)).count();

        // rclone's: MLST among them, no MFMT.
        let (server, heard) = fake(&["UTF8", "MLST Type*;Modify*;Size*;", "", "no MFMT"]);
        let mut c = Ftp::connect(&server, "p", &crate::tls::Trust::default()).unwrap();
        assert!(c.mlsd && c.mfmt.is_none(), "MLST answered, MFMT not known yet");
        assert_eq!(commands(&heard, "OPTS UTF8 ON"), 1);
        assert_eq!(c.set_modified("/a.txt", at), Ok(false));
        assert_eq!(c.mfmt, Some(false));
        assert_eq!(c.set_modified("/b.txt", at), Ok(false));
        assert_eq!(commands(&heard, "MFMT"), 1, "asked once");
        assert!(c.alive());

        // One that takes MFMT though its list can't be read, and has no MLST.
        let (server, heard) = fake(&["", "UTF8"]);
        let mut c = Ftp::connect(&server, "p", &crate::tls::Trust::default()).unwrap();
        assert!(!c.mlsd && c.mfmt.is_none());
        // A file it won't date doesn't say the command is missing.
        assert_eq!(c.set_modified("/locked.txt", at), Ok(false));
        assert_eq!(c.mfmt, None);
        assert_eq!(c.set_modified("/a.txt", at), Ok(true));
        assert_eq!((c.mfmt, commands(&heard, "MFMT")), (Some(true), 2));
    }

    /// Both ways of listing, MLSD and LIST, against `rclone serve ftp`, whatever it offers.
    #[test]
    #[ignore]
    fn lists_with_and_without_mlsd() {
        let dir = std::env::temp_dir().join(format!("oe-ftp-list-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Pasta de fotos")).unwrap();
        std::fs::write(dir.join("Pasta de fotos/a b.txt"), b"12345").unwrap();
        let mut child = std::process::Command::new("rclone")
            .args(["serve", "ftp", &dir.display().to_string(), "--addr", "127.0.0.1:21250", "--passive-port", "21251-21270", "--user", "u", "--pass", "p"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        while TcpStream::connect(("127.0.0.1", 21250)).is_err() {
            std::thread::sleep(Duration::from_millis(100));
        }
        let server = Server { id: "x".into(), name: "x".into(), protocol: Protocol::Ftp, brand: None, host: "127.0.0.1".into(), port: 21250, user: "u".into(), root: "/".into(), key: None, host_key: None, cert: None, connections: 4, implicit: false };
        let mut c = Ftp::connect(&server, "p", &crate::tls::Trust::default()).unwrap();
        let offered = c.mlsd;
        for mlsd in [true, false] {
            c.mlsd = mlsd;
            let top = c.list("/").unwrap();
            assert!(top.iter().any(|i| i.name == "Pasta de fotos" && i.is_dir), "mlsd {mlsd}: {top:?}");
            let inner = c.list("/Pasta de fotos").unwrap();
            assert_eq!(inner.iter().map(|i| (i.name.as_str(), i.size)).collect::<Vec<_>>(), [("a b.txt", Some(5))], "mlsd {mlsd}");
            assert!(c.stat("/Pasta de fotos").unwrap().is_some_and(|i| i.is_dir), "mlsd {mlsd}");
            assert_eq!(c.stat("/Pasta de fotos/a b.txt").unwrap().and_then(|i| i.size), Some(5), "mlsd {mlsd} (offered {offered})");
            assert_eq!(c.stat("/nada").unwrap(), None, "mlsd {mlsd}");
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("rclone offers MLSD: {offered}");
    }
}

