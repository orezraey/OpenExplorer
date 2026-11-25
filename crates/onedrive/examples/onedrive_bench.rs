//! Fills a OneDrive account with the data the speed comparison opens (see `bench/README.md`),
//! under "Open Explorer benchmark" in "Meus arquivos":
//!
//! - `1000 folders`: 1,000 empty folders.
//! - `5000 files`: 5,000 files of 1 KB (5 MB in all).
//! - `100 folders`: 100 empty folders, for downloading a tree of folders.
//!
//! Run again, it adds only what's missing.
//!
//! `cargo run -p openexplorer-onedrive --release --example onedrive_bench -- <the drive's id, as onedrive-accounts.json names it>`

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

const ROOT: &str = "/Meus arquivos/Open Explorer benchmark";
const WORKERS: usize = 6;

fn main() {
    let key = std::env::args().nth(1).expect("the account (the drive's id, as onedrive-accounts.json names it)");
    let account = openexplorer_onedrive::reach(&key).unwrap_or_else(|e| panic!("reaching the account: {e}"));
    println!("account {key}");
    let started = Instant::now();
    let names = |n: usize, what: &str| -> Vec<String> { (1..=n).map(|i| format!("{what} {i:04}")).collect() };
    for folder in ["", "/1000 folders", "/5000 files", "/100 folders"] {
        let path = format!("{ROOT}{folder}");
        if !names_in(&account, parent_of(&path)).contains(name_of(&path)) {
            account.mkdir(&path).unwrap_or_else(|e| panic!("making {path}: {e}"));
        }
    }
    fill(&account, &format!("{ROOT}/1000 folders"), names(1000, "folder"), false);
    fill(&account, &format!("{ROOT}/100 folders"), names(100, "folder"), false);
    fill(&account, &format!("{ROOT}/5000 files"), names(5000, "file").into_iter().map(|n| format!("{n}.txt")).collect(), true);
    println!("done in {:.0} s", started.elapsed().as_secs_f64());
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
