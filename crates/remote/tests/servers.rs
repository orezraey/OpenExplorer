//! Each protocol against a real server: `rclone serve` on a temporary folder. Needs rclone on
//! the PATH, so it's ignored by default: `cargo test -p openexplorer-remote -- --ignored`.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use openexplorer_remote::{Pool, Protocol, Server, connect, probe};

struct Served {
    child: Child,
    dir: PathBuf,
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn serve(kind: &str, port: u16) -> Served {
    let dir = std::env::temp_dir().join(format!("oe-remote-{kind}-{port}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("raiz/Fotos de férias")).unwrap();
    std::fs::write(dir.join("raiz/Fotos de férias/praia #1.txt"), b"areia").unwrap();
    let mut args = vec!["serve".to_string(), kind.into(), dir.join("raiz").display().to_string(), "--addr".into(), format!("127.0.0.1:{port}"), "--user".into(), "ana".into(), "--pass".into(), "s3nh@ forte".into()];
    if kind == "ftp" {
        args.extend(["--passive-port".into(), format!("{}-{}", port + 1, port + 40)]);
    }
    let child = Command::new("rclone").args(&args).stdout(Stdio::null()).stderr(Stdio::null()).spawn().expect("rclone on the PATH");
    let start = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(start.elapsed() < Duration::from_secs(20), "rclone serve {kind} didn't start");
        std::thread::sleep(Duration::from_millis(100));
    }
    Served { child, dir }
}

fn server(protocol: Protocol, port: u16) -> Server {
    Server { id: format!("t-{port}"), name: "teste".into(), protocol, host: "127.0.0.1".into(), port, user: "ana".into(), root: String::new(), key: None, host_key: None }
}

/// Everything the panes do with a server, start to end.
fn round_trip(protocol: Protocol, kind: &str, port: u16) {
    let served = serve(kind, port);
    let mut s = server(protocol, port);
    let secret = Some("s3nh@ forte");
    probe(&mut s, secret).unwrap_or_else(|e| panic!("{kind}: {e}"));
    assert_eq!(s.root, "/", "{kind}");
    if protocol == Protocol::Sftp {
        assert!(s.host_key.as_deref().is_some_and(|k| k.starts_with("SHA256:")), "{:?}", s.host_key);
    }

    // A wrong password is refused with a clear message.
    let refused = connect(&s, Some("errada")).map(|_| ()).unwrap_err();
    assert!(refused.contains("recusou"), "{kind}: {refused}");

    let pool = Pool::default();
    let mut c = pool.take(&s, || secret.map(str::to_string)).unwrap();
    let top = c.list("/").unwrap();
    assert!(top.iter().any(|i| i.name == "Fotos de férias" && i.is_dir), "{kind}: {top:?}");
    let inner = c.list("/Fotos de férias").unwrap();
    assert_eq!(inner.len(), 1, "{kind}: {inner:?}");
    assert_eq!((inner[0].name.as_str(), inner[0].size), ("praia #1.txt", Some(5)), "{kind}");

    let mut got = Vec::new();
    c.download("/Fotos de férias/praia #1.txt", &mut got, &mut |_| true).unwrap();
    assert_eq!(got, b"areia", "{kind}");

    c.mkdir("/Novo").unwrap();
    let data = vec![7u8; 600_000];
    let mut seen = 0;
    c.upload("/Novo/grande.bin", &mut data.as_slice(), data.len() as u64, &mut |n| {
        seen = n;
        true
    })
    .unwrap();
    assert_eq!(seen, 600_000, "{kind}");
    assert_eq!(std::fs::read(served.dir.join("raiz/Novo/grande.bin")).unwrap().len(), 600_000, "{kind}");
    assert_eq!(c.stat("/Novo/grande.bin").unwrap().and_then(|i| i.size), Some(600_000), "{kind}");
    assert!(c.stat("/Novo").unwrap().is_some_and(|i| i.is_dir), "{kind}");
    assert_eq!(c.stat("/nada aqui").unwrap(), None, "{kind}");

    // A cancelled upload leaves nothing behind.
    let stopped = c.upload("/Novo/parado.bin", &mut data.as_slice(), data.len() as u64, &mut |n| n < 300_000).unwrap_err();
    assert_eq!(stopped, openexplorer_remote::CANCELLED, "{kind}");
    c.done();
    let mut c = pool.take(&s, || secret.map(str::to_string)).unwrap();
    assert_eq!(c.stat("/Novo/parado.bin").unwrap(), None, "{kind}");

    c.rename("/Novo/grande.bin", "/Novo/renomeado.bin").unwrap();
    assert!(c.rename("/Fotos de férias", "/Novo").is_err(), "{kind}: renaming onto an existing folder");
    c.rename("/Novo", "/Fotos de férias/Novo").unwrap();
    assert!(served.dir.join("raiz/Fotos de férias/Novo/renomeado.bin").exists(), "{kind}");

    c.remove("/Fotos de férias", true).unwrap();
    assert!(c.list("/").unwrap().is_empty(), "{kind}");
    c.done();
}

#[test]
#[ignore]
fn ftp() {
    round_trip(Protocol::Ftp, "ftp", 21210);
}

#[test]
#[ignore]
fn sftp() {
    round_trip(Protocol::Sftp, "sftp", 22220);
}

#[test]
#[ignore]
fn webdav() {
    round_trip(Protocol::Dav, "webdav", 28080);
}

#[test]
#[ignore]
fn sftp_refuses_a_changed_host_key() {
    let _served = serve("sftp", 22260);
    let mut s = server(Protocol::Sftp, 22260);
    s.host_key = Some("SHA256:outra".into());
    let e = connect(&s, Some("s3nh@ forte")).map(|_| ()).unwrap_err();
    assert!(e.contains("mudou"), "{e}");
}

#[test]
#[ignore]
fn a_server_gets_at_most_a_few_connections_at_once() {
    let _served = serve("webdav", 28084);
    let s = server(Protocol::Dav, 28084);
    let pool = std::sync::Arc::new(Pool::default());
    let held: Vec<_> = (0..openexplorer_remote::PER_SERVER).map(|_| pool.take(&s, || Some("s3nh@ forte".into())).unwrap()).collect();
    // One more waits until one comes back.
    let (tx, rx) = std::sync::mpsc::channel();
    let (p, s2) = (pool.clone(), s.clone());
    std::thread::spawn(move || {
        let c = p.take(&s2, || Some("s3nh@ forte".into())).unwrap();
        tx.send(()).unwrap();
        c.done();
    });
    assert!(rx.recv_timeout(Duration::from_millis(500)).is_err(), "a fifth connection was opened");
    drop(held);
    rx.recv_timeout(Duration::from_secs(5)).expect("the waiting job got a connection");
}
