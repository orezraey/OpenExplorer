//! SFTP, through russh. russh is async: each connection runs its own single-thread tokio
//! runtime, and every call blocks on it.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use openexplorer_i18n::{t, tr};
use russh::client::{self, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::Disconnect;
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::StatusCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime::Runtime;

use crate::{Conn, Item, Progress, Result, Server, TIMEOUT, resolve, unreachable};

pub struct Sftp {
    rt: Runtime,
    sftp: SftpSession,
    ssh: Handle<Client>,
}

/// Checks the server's key against the one seen when it was added.
pub struct Client {
    expected: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
}

fn fingerprint(key: &PublicKeyOrCertificate) -> String {
    let key: PublicKey = match key {
        PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
        PublicKeyOrCertificate::Certificate(c) => PublicKey::from(c.public_key().clone()),
    };
    key.fingerprint(HashAlg::Sha256).to_string()
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> std::result::Result<bool, Self::Error> {
        let seen = fingerprint(key);
        let ok = self.expected.as_ref().is_none_or(|e| *e == seen);
        *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(seen);
        Ok(ok)
    }
}

fn status(e: &SftpError) -> Option<StatusCode> {
    match e {
        SftpError::Status(s) => Some(s.status_code),
        _ => None,
    }
}

fn said(e: SftpError) -> String {
    match &e {
        SftpError::Status(s) => match s.status_code {
            StatusCode::NoSuchFile => t("Não existe no servidor.", "It doesn't exist on the server.").into(),
            StatusCode::PermissionDenied => t("O servidor negou a permissão.", "The server denied permission.").into(),
            _ if !s.error_message.is_empty() => tr!("O servidor respondeu: {}", "The server answered: {}", s.error_message),
            _ => tr!("O servidor respondeu: {:?}", "The server answered: {:?}", s.status_code),
        },
        SftpError::Timeout => t("O servidor não respondeu a tempo.", "The server didn't answer in time.").into(),
        _ => tr!("Erro de SFTP: {e}", "SFTP error: {e}"),
    }
}

/// The keys `~/.ssh` usually holds, tried when no password or key was given.
fn usual_keys() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) else { return Vec::new() };
    let dir = PathBuf::from(home).join(".ssh");
    ["id_ed25519", "id_ecdsa", "id_rsa"].iter().map(|n| dir.join(n)).filter(|p| p.is_file()).collect()
}

impl Sftp {
    pub fn connect(server: &Server, secret: &str) -> Result<Sftp> {
        Sftp::open(server, secret, server.host_key.clone()).map(|(c, _)| c)
    }

    /// Connects accepting whatever host key the server has, and says which it was.
    pub fn connect_seeing(server: &Server, secret: &str) -> Result<(Sftp, String)> {
        Sftp::open(server, secret, None)
    }

    fn open(server: &Server, secret: &str, expected: Option<String>) -> Result<(Sftp, String)> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
        let host = server.host.clone();
        let addr = resolve(&host, server.port)?;
        let seen = Arc::new(Mutex::new(None));
        let handler = Client { expected: expected.clone(), seen: seen.clone() };
        let saw = seen.clone();
        let user = server.user.clone();
        let key = server.key.clone();
        let secret = secret.to_string();
        let (ssh, sftp) = rt.block_on(async move {
            let cfg = client::Config { inactivity_timeout: None, keepalive_interval: Some(Duration::from_secs(30)), ..Default::default() };
            let socket = tokio::time::timeout(TIMEOUT, tokio::net::TcpStream::connect(addr))
                .await
                .map_err(|_| tr!("{host} não respondeu a tempo.", "{host} didn't answer in time."))?
                .map_err(|e| unreachable(&host, &e))?;
            let _ = socket.set_nodelay(true);
            let mut ssh = match tokio::time::timeout(TIMEOUT, client::connect_stream(Arc::new(cfg), socket, handler)).await {
                Err(_) => return Err(tr!("{host} não respondeu a tempo.", "{host} didn't answer in time.")),
                Ok(Err(russh::Error::UnknownKey)) => {
                    let now = saw.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default();
                    return Err(tr!(
                        "A chave de {host} mudou: era {}, agora é {now}. Pode ser outro computador se passando pelo servidor. Se a mudança é esperada, remova o servidor e adicione de novo.",
                        "The server key of {host} changed: it was {}, now it is {now}. It may be another computer posing as the server. If the change is expected, remove the server and add it again.",
                        expected.unwrap_or_default()
                    ));
                }
                Ok(Err(e)) => return Err(tr!("Não consegui falar SSH com {host}: {e}", "Couldn't speak SSH with {host}: {e}")),
                Ok(Ok(h)) => h,
            };
            if !login(&mut ssh, &user, key.as_deref(), &secret).await? {
                return Err(tr!("{host} recusou o usuário, a senha ou a chave.", "{host} refused the user name, password or key."));
            }
            let channel = ssh.channel_open_session().await.map_err(|e| tr!("{host} recusou abrir uma sessão: {e}", "{host} refused to open a session: {e}"))?;
            channel.request_subsystem(true, "sftp").await.map_err(|e| tr!("{host} não oferece SFTP: {e}", "{host} doesn't offer SFTP: {e}"))?;
            let sftp = SftpSession::new(channel.into_stream()).await.map_err(|e| tr!("{host} não oferece SFTP: {e}", "{host} doesn't offer SFTP: {e}"))?;
            Ok::<_, String>((ssh, sftp))
        })?;
        sftp.set_timeout(60);
        let fp = seen.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default();
        Ok((Sftp { rt, sftp, ssh }, fp))
    }
}

/// Logs in with the key given, else the password (also as keyboard-interactive, which many
/// servers ask for instead), else the usual keys in `~/.ssh`.
async fn login(ssh: &mut Handle<Client>, user: &str, key: Option<&str>, secret: &str) -> Result<bool> {
    let failed = |e: russh::Error| tr!("O login falhou: {e}", "The login failed: {e}");
    let with_key = async |ssh: &mut Handle<Client>, path: &std::path::Path, pass: Option<&str>| -> Result<bool> {
        let key = russh::keys::load_secret_key(path, pass).map_err(|e| match e {
            russh::keys::Error::KeyIsEncrypted => tr!("A chave {} pede a senha dela: escreva-a no campo Senha.", "The key {} asks for its password: type it in the Password field.", path.display()),
            e => tr!("Não consegui ler a chave {}: {e}", "Couldn't read the key {}: {e}", path.display()),
        })?;
        let hash = ssh.best_supported_rsa_hash().await.ok().flatten().flatten();
        Ok(ssh.authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await.map_err(failed)?.success())
    };
    if let Some(path) = key {
        return with_key(ssh, std::path::Path::new(path), (!secret.is_empty()).then_some(secret)).await;
    }
    if secret.is_empty() {
        for path in usual_keys() {
            // An encrypted key without its passphrase is skipped.
            if let Ok(true) = with_key(ssh, &path, None).await {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    if ssh.authenticate_password(user, secret).await.map_err(failed)?.success() {
        return Ok(true);
    }
    let mut reply = ssh.authenticate_keyboard_interactive_start(user, None).await.map_err(failed)?;
    for _ in 0..4 {
        match reply {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                let answers = prompts.iter().map(|_| secret.to_string()).collect();
                reply = ssh.authenticate_keyboard_interactive_respond(answers).await.map_err(failed)?;
            }
        }
    }
    Ok(false)
}

fn item(name: String, m: &russh_sftp::protocol::FileAttributes, link: bool) -> Item {
    let is_dir = m.is_dir();
    Item { name, is_dir, size: (!is_dir).then(|| m.size.unwrap_or(0)), modified: m.mtime.map(|t| UNIX_EPOCH + Duration::from_secs(t as u64)), link }
}

impl Conn for Sftp {
    fn list(&mut self, dir: &str) -> Result<Vec<Item>> {
        let entries = self.rt.block_on(self.sftp.read_dir(dir)).map_err(said)?;
        let mut out = Vec::new();
        for e in entries {
            let name = e.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let meta = e.metadata();
            if meta.is_symlink() {
                // A link shows as what it points to; a broken one as a file.
                let path = format!("{}/{name}", dir.trim_end_matches('/'));
                let target = self.rt.block_on(self.sftp.metadata(path)).unwrap_or(meta);
                out.push(item(name, &target, true));
            } else {
                out.push(item(name, &meta, false));
            }
        }
        Ok(out)
    }

    fn stat(&mut self, path: &str) -> Result<Option<Item>> {
        let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or(path).to_string();
        match self.rt.block_on(self.sftp.metadata(path)) {
            Ok(m) => Ok(Some(item(name, &m, false))),
            Err(e) if status(&e) == Some(StatusCode::NoSuchFile) => Ok(None),
            Err(e) => Err(said(e)),
        }
    }

    fn is_link(&mut self, path: &str) -> Result<bool> {
        match self.rt.block_on(self.sftp.symlink_metadata(path)) {
            Ok(m) => Ok(m.is_symlink()),
            Err(e) if status(&e) == Some(StatusCode::NoSuchFile) => Ok(false),
            Err(e) => Err(said(e)),
        }
    }

    fn mkdir(&mut self, path: &str) -> Result<()> {
        self.rt.block_on(self.sftp.create_dir(path)).map_err(said)
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        if self.stat(to)?.is_some() {
            return Err(tr!("Já existe um item chamado {}.", "There's already an item named {}.", to.rsplit('/').next().unwrap_or(to)));
        }
        self.rt.block_on(self.sftp.rename(from, to)).map_err(said)
    }

    fn remove_file(&mut self, path: &str) -> Result<()> {
        self.rt.block_on(self.sftp.remove_file(path)).map_err(said)
    }

    fn remove_dir(&mut self, path: &str) -> Result<()> {
        self.rt.block_on(self.sftp.remove_dir(path)).map_err(said)
    }

    fn download(&mut self, path: &str, out: &mut dyn Write, progress: Progress) -> Result<()> {
        let Sftp { rt, sftp, .. } = self;
        rt.block_on(async {
            let mut file = sftp.open(path).await.map_err(said)?;
            let mut buf = vec![0u8; 256 * 1024];
            let mut done = 0u64;
            let read = async {
                loop {
                    let n = file.read(&mut buf).await.map_err(|e| tr!("A transferência parou: {e}", "The transfer stopped: {e}"))?;
                    if n == 0 {
                        return Ok(());
                    }
                    out.write_all(&buf[..n]).map_err(|e| tr!("A transferência parou: {e}", "The transfer stopped: {e}"))?;
                    done += n as u64;
                    if !progress(done) {
                        return Err(crate::CANCELLED.to_string());
                    }
                }
            }
            .await;
            // Closed however it ended: the connection goes back to the pool, and an open handle
            // would stay with it.
            // Not for long, though: after a dropped connection it would wait out the timeout.
            let closed = tokio::time::timeout(Duration::from_secs(5), file.close()).await;
            read.and(
                closed
                    .map_err(|_| t("A transferência parou: o servidor não respondeu.", "The transfer stopped: the server didn't answer.").to_string())?
                    .map_err(|e| tr!("A transferência parou: {e}", "The transfer stopped: {e}")),
            )
        })
    }

    fn upload(&mut self, path: &str, src: &mut dyn Read, _len: u64, progress: Progress) -> Result<()> {
        let Sftp { rt, sftp, .. } = self;
        // Only a file this upload made is taken away on failure: a create the server refused
        // (a read-only file already there, say) leaves what was there.
        let mut made = false;
        let result = rt.block_on(async {
            let mut file = sftp.create(path).await.map_err(said)?;
            made = true;
            let mut buf = vec![0u8; 256 * 1024];
            let mut done = 0u64;
            loop {
                let n = src.read(&mut buf).map_err(|e| tr!("A transferência parou: {e}", "The transfer stopped: {e}"))?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n]).await.map_err(|e| tr!("A transferência parou: {e}", "The transfer stopped: {e}"))?;
                done += n as u64;
                if !progress(done) {
                    return Err(crate::CANCELLED.to_string());
                }
            }
            // Closed now, not when dropped: until then the server may hold the file open.
            file.close().await.map_err(|e| tr!("A transferência parou: {e}", "The transfer stopped: {e}"))
        });
        if result.is_err() && made {
            // What arrived of the file is no use.
            let _ = rt.block_on(sftp.remove_file(path));
        }
        result
    }

    fn sets_dates(&self) -> bool {
        true
    }

    /// SETSTAT with the two times (the protocol sets them together), to the second.
    fn set_modified(&mut self, path: &str, at: SystemTime) -> Result<bool> {
        let secs = at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()).min(u64::from(u32::MAX)) as u32;
        let times = russh_sftp::protocol::FileAttributes { atime: Some(secs), mtime: Some(secs), ..Default::default() };
        match self.rt.block_on(self.sftp.set_metadata(path, times)) {
            Ok(()) => Ok(true),
            Err(e) if matches!(status(&e), Some(StatusCode::OpUnsupported | StatusCode::PermissionDenied)) => Ok(false),
            Err(e) => Err(said(e)),
        }
    }

    fn home(&mut self) -> Result<String> {
        self.rt.block_on(self.sftp.canonicalize(".")).map_err(said)
    }

    fn alive(&mut self) -> bool {
        // A connection whose network went away answers nothing: not worth the full timeout.
        !self.ssh.is_closed() && self.rt.block_on(async { tokio::time::timeout(Duration::from_secs(5), self.sftp.canonicalize(".")).await }).is_ok_and(|r| r.is_ok())
    }
}

impl Drop for Sftp {
    fn drop(&mut self) {
        let Sftp { rt, sftp, ssh } = self;
        // Politely, but not for long: a dead connection would hold the thread until it times out.
        let _ = rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(3), async {
                let _ = sftp.close().await;
                ssh.disconnect(Disconnect::ByApplication, "", "pt-BR").await
            })
            .await
        });
    }
}
