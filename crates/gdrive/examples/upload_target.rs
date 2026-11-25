//! Where the speed comparison's uploads go, and when they've arrived (see `bench/README.md`):
//! folders under "Open Explorer benchmark/uploads" in My Drive.
//!
//! - `make <n>`: makes `n` empty folders there, one name a line. They're made before an app
//!   starts, so it lists them fresh.
//! - `wait <folder> <files> <bytes>`: prints `ready` once it can ask Drive, then asks every
//!   200 ms until the folder holds `files` files of `bytes` in all (Drive shows a file only once
//!   its upload finished), and prints `done`. Gives up after 10 minutes.
//! - `clean`: deletes for good every folder there, to give the room back.
//!
//! `cargo run -p openexplorer-gdrive --release --example upload_target -- <e-mail> <what> ...`

use std::io::Write;
use std::time::{Duration, Instant};

use openexplorer_gdrive::{ClientConfig, Drive, DriveFile, store};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let email = args.first().expect("the account's e-mail");
    let cfg = ClientConfig::load().expect("the app's Google client (google-client.json)");
    let token = store::refresh_token(email).expect("the account's token");
    let drive = Drive::new(cfg, token);
    let root = folder(&drive, "root", "Open Explorer benchmark");
    let uploads = folder(&drive, &root, "uploads");
    match args.get(1).map(String::as_str) {
        Some("make") => {
            let n: usize = args.get(2).and_then(|n| n.parse().ok()).expect("how many folders");
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
            for k in 0..n {
                let name = format!("up-{stamp}-{k:02}");
                drive.create_folder(&name, &uploads).unwrap_or_else(|e| panic!("making {name}: {e}"));
                println!("{name}");
            }
        }
        Some("wait") => {
            let name = &args[2];
            let files: usize = args[3].parse().expect("the number of files");
            let bytes: u64 = args[4].parse().expect("the bytes in all");
            let id = children(&drive, &uploads).into_iter().find(|f| f.is_dir && &f.name == name).map(|f| f.id).expect("that folder");
            // The first request warms the connection and the token, so `ready` means the next
            // ones are as quick as they get.
            children(&drive, &id);
            println!("ready");
            std::io::stdout().flush().unwrap();
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(600) {
                let got: Vec<DriveFile> = children(&drive, &id).into_iter().filter(|f| !f.is_dir).collect();
                if got.len() == files && got.iter().map(|f| f.size.unwrap_or(0)).sum::<u64>() == bytes {
                    println!("done");
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            println!("timeout");
            std::process::exit(1);
        }
        Some("clean") => {
            for f in children(&drive, &uploads).into_iter().filter(|f| f.is_dir) {
                drive.delete_forever(&f.id).unwrap_or_else(|e| panic!("deleting {}: {e}", f.name));
                println!("deleted {}", f.name);
            }
        }
        _ => panic!("make <n> | wait <folder> <files> <bytes> | clean"),
    }
}

fn folder(drive: &Drive, parent: &str, name: &str) -> String {
    match children(drive, parent).into_iter().find(|f| f.is_dir && f.name == name) {
        Some(f) => f.id,
        None => drive.create_folder(name, parent).unwrap_or_else(|e| panic!("making {name}: {e}")).id,
    }
}

fn children(drive: &Drive, parent: &str) -> Vec<DriveFile> {
    let mut all = Vec::new();
    drive
        .list(parent, None, |page, _| {
            all.extend(page);
            true
        })
        .unwrap_or_else(|e| panic!("listing {parent}: {e}"));
    all
}
