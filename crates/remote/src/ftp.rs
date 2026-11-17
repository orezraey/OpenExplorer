//! FTP and FTPS (explicit TLS), through suppaftp.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use suppaftp::list::ListParser;
use suppaftp::types::FileType;
use suppaftp::{FtpError, RustlsConnector, RustlsFtpStream, Status};

use crate::{Conn, Item, Progress, Protocol, Result, Server, TIMEOUT, pump, resolve, unreachable};

pub struct Ftp {
    s: RustlsFtpStream,
    /// The server lists folders with MLSD, whose dates and sizes don't need guessing.
    mlsd: bool,
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

/// TLS that checks the server's certificate against the usual authorities.
pub(crate) fn tls_config() -> Arc<rustls::ClientConfig> {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(cfg)
}

impl Ftp {
    pub fn connect(server: &Server, secret: &str) -> Result<Ftp> {
        let host = server.host.as_str();
        let addr = resolve(host, server.port)?;
        let mut s = RustlsFtpStream::connect_with_stream(socket(addr, host)?).map_err(|e| said(host, e))?;
        // A server behind NAT answers PASV with its private address: use the one we reached.
        s.set_passive_nat_workaround(true);
        let named = host.to_string();
        let mut s = s.passive_stream_builder(move |addr| socket(addr, &named).map_err(|e| FtpError::ConnectionError(std::io::Error::other(e))));
        if server.protocol == Protocol::Ftps {
            s = s.into_secure(RustlsConnector::from(tls_config()), host).map_err(|e| secure(host, e))?;
        }
        let user = if server.user.is_empty() { "anonymous" } else { server.user.as_str() };
        // Anonymous FTP asks for some password, by custom an e-mail; many refuse an empty one.
        let secret = if server.user.is_empty() && secret.is_empty() { "anonymous@" } else { secret };
        s.login(user, secret).map_err(|e| match e {
            FtpError::UnexpectedResponse(r) if r.status == Status::NotLoggedIn => format!("{host} recusou o usuário ou a senha."),
            e => said(host, e),
        })?;
        s.transfer_type(FileType::Binary).map_err(|e| said(host, e))?;
        let features = s.feat().unwrap_or_default();
        let has = |f: &str| features.keys().any(|k| k.eq_ignore_ascii_case(f));
        if has("UTF8") {
            let _ = s.opts("UTF8", Some("ON"));
        }
        let mlsd = has("MLST");
        Ok(Ftp { s, mlsd, broken: false })
    }

    /// Whether `path` is a folder: it can be entered.
    fn enters(&mut self, path: &str) -> bool {
        self.s.cwd(path).is_ok()
    }
}

fn secure(host: &str, e: FtpError) -> String {
    match e {
        FtpError::SecureError(m) if m.contains("certificate") || m.contains("Certificate") => {
            format!("O certificado de {host} não é confiável ({m}). Servidores com certificado próprio ainda não são aceitos.")
        }
        FtpError::UnexpectedResponse(_) => format!("{host} não aceita FTPS: tente FTP."),
        e => said(host, e),
    }
}

/// The message for an FTP error, from the server's own words when it gave some.
fn said(host: &str, e: FtpError) -> String {
    match e {
        FtpError::ConnectionError(e) => unreachable(host, &e),
        FtpError::UnexpectedResponse(r) => {
            let text = String::from_utf8_lossy(&r.body).trim().to_string();
            match r.status {
                Status::NotLoggedIn => format!("{host} recusou o usuário ou a senha."),
                Status::FileUnavailable => format!("O servidor recusou: {text}"),
                Status::ExceededStorage => "O servidor está sem espaço.".into(),
                Status::BadFilename => format!("O servidor não aceita esse nome: {text}"),
                _ => format!("O servidor respondeu: {text}"),
            }
        }
        FtpError::SecureError(m) => format!("Erro de TLS com {host}: {m}"),
        e => format!("Erro de FTP com {host}: {e}"),
    }
}

impl Conn for Ftp {
    fn list(&mut self, dir: &str) -> Result<Vec<Item>> {
        let host = "o servidor";
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
                Err(e) => Err(said("o servidor", e)),
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
        self.s.mkdir(path).map_err(|e| said("o servidor", e))
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        if self.stat(to)?.is_some() {
            return Err(format!("Já existe um item chamado {}.", to.rsplit('/').next().unwrap_or(to)));
        }
        self.s.rename(from, to).map_err(|e| said("o servidor", e))
    }

    fn remove_file(&mut self, path: &str) -> Result<()> {
        self.s.rm(path).map_err(|e| said("o servidor", e))
    }

    fn remove_dir(&mut self, path: &str) -> Result<()> {
        self.s.rmdir(path).map_err(|e| said("o servidor", e))
    }

    fn download(&mut self, path: &str, out: &mut dyn Write, progress: Progress) -> Result<()> {
        let mut stream = self.s.retr_as_stream(path).map_err(|e| said("o servidor", e))?;
        match pump(&mut stream, out, progress) {
            Ok(_) => stream.finish().map_err(|e| said("o servidor", e)),
            Err(e) => {
                self.broken = true;
                let _ = self.s.abort(stream);
                Err(e)
            }
        }
    }

    fn upload(&mut self, path: &str, src: &mut dyn Read, _len: u64, progress: Progress) -> Result<()> {
        let mut stream = self.s.put_with_stream(path).map_err(|e| said("o servidor", e))?;
        match pump(src, &mut stream, progress) {
            Ok(_) => stream.finish().map_err(|e| said("o servidor", e)),
            Err(e) => {
                self.broken = true;
                let _ = self.s.abort(stream);
                // What arrived of the file is no use.
                let _ = self.s.rm(path);
                Err(e)
            }
        }
    }

    fn home(&mut self) -> Result<String> {
        self.s.pwd().map_err(|e| said("o servidor", e))
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
        let server = Server { id: "x".into(), name: "x".into(), protocol: Protocol::Ftp, host: "127.0.0.1".into(), port: 21250, user: "u".into(), root: "/".into(), key: None, host_key: None };
        let mut c = Ftp::connect(&server, "p").unwrap();
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
