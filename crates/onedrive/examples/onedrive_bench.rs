//! Fills a OneDrive account with the data the speed comparison opens (see `bench/README.md`),
//! under "Open Explorer benchmark" in "Meus arquivos":
//!
//! - `1000 folders`: 1,000 empty folders.
//! - `5000 files`: 5,000 files of 1 KB (5 MB in all).
//! - `100 folders`: 100 empty folders, for downloading a tree of folders.
//! - `transfer/large/large.bin`: one file of 500 MiB, for a big download.
//!
//! Run again, it adds only what's missing.
//!
//! The speed comparison's uploads go to folders under `uploads` there, which this also tends:
//!
//! - `make <n>`: makes `n` empty folders, one name a line. They're made before an app starts,
//!   so it lists them fresh.
//! - `wait <folder> <files> <bytes>`: prints `ready` once it can ask, then asks every 200 ms
//!   until the folder holds `files` files of `bytes` in all (a file shows only once its upload
//!   finished), and prints `done`. Gives up after 10 minutes.
//! - `clean`: deletes every folder there.
//!
//! `cargo run -p openexplorer-onedrive --release --example onedrive_bench -- <the drive's id, as onedrive-accounts.json names it>`

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::io::Write;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ROOT: &str = "/Meus arquivos/Open Explorer benchmark";
const WORKERS: usize = 6;

fn main() {
    let key = std::env::args().nth(1).expect("the account (the drive's id, as onedrive-accounts.json names it)");
    let account = openexplorer_onedrive::reach(&key).unwrap_or_else(|e| panic!("reaching the account: {e}"));
    let arg = |n: usize| std::env::args().nth(n).unwrap_or_else(|| panic!("make <n> | wait <folder> <files> <bytes> | clean"));
    match std::env::args().nth(2).as_deref() {
        Some("make") => return make(&account, arg(3).parse().expect("how many folders")),
        Some("wait") => return wait(&account, &arg(3), arg(4).parse().expect("the number of files"), arg(5).parse().expect("the bytes in all")),
        Some("clean") => return clean(&account),
        Some(other) => panic!("{other}? make <n> | wait <folder> <files> <bytes> | clean"),
        None => {}
    }
    println!("account {key}");
    let started = Instant::now();
    let names = |n: usize, what: &str| -> Vec<String> { (1..=n).map(|i| format!("{what} {i:04}")).collect() };
    for folder in ["", "/1000 folders", "/5000 files", "/100 folders", "/transfer", "/transfer/large", "/uploads"] {
        let path = format!("{ROOT}{folder}");
        if !names_in(&account, parent_of(&path)).contains(name_of(&path)) {
            account.mkdir(&path).unwrap_or_else(|e| panic!("making {path}: {e}"));
        }
    }
    fill(&account, &format!("{ROOT}/1000 folders"), names(1000, "folder"), false);
    fill(&account, &format!("{ROOT}/100 folders"), names(100, "folder"), false);
    fill(&account, &format!("{ROOT}/5000 files"), names(5000, "file").into_iter().map(|n| format!("{n}.txt")).collect(), true);
    let large = format!("{ROOT}/transfer/large");
    if !names_in(&account, &large).contains("large.bin") {
        println!("{large}: sending large.bin (500 MiB)");
        account.upload(&format!("{large}/large.bin"), &mut Noise { left: LARGE, x: 0x9E37_79B9_7F4A_7C15 }, LARGE, &mut |_| true).unwrap_or_else(|e| panic!("sending large.bin: {e}"));
    }
    println!("done in {:.0} s", started.elapsed().as_secs_f64());
}

const LARGE: u64 = 500 << 20;

/// `left` bytes that don't repeat, so nothing on the way can shrink them.
struct Noise {
    left: u64,
    x: u64,
}

impl std::io::Read for Noise {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = (buf.len() as u64).min(self.left) as usize;
        for chunk in buf[..n].chunks_mut(8) {
            self.x ^= self.x << 13;
            self.x ^= self.x >> 7;
            self.x ^= self.x << 17;
            chunk.copy_from_slice(&self.x.to_le_bytes()[..chunk.len()]);
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// A folder's items as they are now, not as kept: (name, folder, size).
fn fresh(account: &openexplorer_onedrive::Account, path: &str) -> Vec<(String, bool, u64)> {
    // What changed since the last ask first: the listing may come from the kept tree.
    let _ = account.poll();
    account.list(path).unwrap_or_else(|e| panic!("listing {path}: {e}")).into_iter().map(|i| (i.name.to_string(), i.is_dir, i.size.unwrap_or(0))).collect()
}

fn make(account: &openexplorer_onedrive::Account, n: usize) {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
    for k in 0..n {
        let name = format!("up-{stamp}-{k:02}");
        account.mkdir(&format!("{ROOT}/uploads/{name}")).unwrap_or_else(|e| panic!("making {name}: {e}"));
        println!("{name}");
    }
}

fn wait(account: &openexplorer_onedrive::Account, name: &str, files: usize, bytes: u64) {
    let path = format!("{ROOT}/uploads/{name}");
    // The first ask warms the connection and the token, so `ready` means the next ones are as
    // quick as they get.
    fresh(account, &path);
    println!("ready");
    std::io::stdout().flush().unwrap();
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(600) {
        let got: Vec<u64> = fresh(account, &path).into_iter().filter(|(_, dir, _)| !dir).map(|(_, _, size)| size).collect();
        if got.len() == files && got.iter().sum::<u64>() == bytes {
            println!("done");
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    println!("timeout");
    std::process::exit(1);
}

fn clean(account: &openexplorer_onedrive::Account) {
    for (name, dir, _) in fresh(account, &format!("{ROOT}/uploads")) {
        if dir {
            let path = format!("{ROOT}/uploads/{name}");
            account.remove(&path).unwrap_or_else(|e| panic!("deleting {name}: {e}"));
            println!("deleted {name}");
        }
    }
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}

fn name_of(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, n)| n)
}

fn names_in(account: &openexplorer_onedrive::Account, path: &str) -> HashSet<String> {
    account.list(if path.is_empty() { "/" } else { path }).unwrap_or_else(|e| panic!("listing {path}: {e}")).into_iter().map(|i| i.name.to_string()).collect()
}

/// Makes in `parent` each of `names` not there yet: folders, or 1 KB files.
fn fill(account: &openexplorer_onedrive::Account, parent: &str, names: Vec<String>, files: bool) {
    let have = names_in(account, parent);
    let todo: Vec<String> = names.into_iter().filter(|n| !have.contains(n)).collect();
    let total = todo.len();
    println!("{parent}: {} there, {total} to make", have.len());
    let queue = Mutex::new(todo);
    let (done, failed) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let body = [b'x'; 1024];
    std::thread::scope(|scope| {
        for _ in 0..WORKERS {
            scope.spawn(|| loop {
                let Some(name) = queue.lock().unwrap().pop() else { break };
                let path = format!("{parent}/{name}");
                let out = if files { account.upload(&path, &mut &body[..], body.len() as u64, &mut |_| true).map(|_| ()) } else { account.mkdir(&path).map(|_| ()) };
                if let Err(e) = out {
                    failed.fetch_add(1, Ordering::Relaxed);
                    eprintln!("  failed {name}: {e}");
                }
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_multiple_of(250) || n == total {
                    println!("  {n}/{total}");
                }
            });
        }
    });
    let failed = failed.load(Ordering::Relaxed);
    if failed > 0 {
        println!("  {failed} failed: run again to make them");
    }
}
