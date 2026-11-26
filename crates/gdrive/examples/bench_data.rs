//! Fills a Google Drive account with the data the speed comparison opens and transfers (see
//! `bench/README.md`), under "Open Explorer benchmark" in My Drive:
//!
//! - `5000 files`: 5,000 files of 1 KB, for listing a big folder.
//! - `1000 folders`: 1,000 empty folders, for listing a folder of folders.
//! - `transfer/large/large.bin`: one file of 500 MiB, for a big download.
//! - `transfer/huge/huge.bin`: one file of 2 GiB, for a download long enough that the line's
//!   ups and downs average out; skipped while the account has no room for it.
//! - `transfer/small`: 200 files of 512 KiB (100 MiB), for many downloads.
//!
//! Run again, it adds only what's missing, so a run cut short picks up where it stopped. Drive
//! paces writes (a few a second per account); the client waits its rate limits out, so the
//! first run takes a while.
//!
//! - `100 folders`: 100 empty folders, for downloading a tree of folders.
//!
//! With `--shortcuts`, for an account with no room left: `5000 files` gets 5,000 shortcuts to
//! files already at the top of My Drive (a shortcut takes no storage and lists as a file; Drive
//! refuses more than about 500 to one file, so they spread over several), and
//! nothing is uploaded. For the big download, one of two copies of `openexplorer-speed-500.bin`
//! at the top (left by the upload_speed example) moves into `transfer/large`, which takes no
//! room either.
//!
//! With `--huge`, only `transfer/huge`.
//!
//! `cargo run -p openexplorer-gdrive --release --example bench_data -- [e-mail] [--shortcuts|--huge]`
//! (without an e-mail, the first connected account)

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use openexplorer_gdrive::{ClientConfig, Drive, DriveFile, store};

const ROOT: &str = "Open Explorer benchmark";
const WORKERS: usize = 8;

fn main() {
    let cfg = ClientConfig::load().expect("the app's Google client (google-client.json)");
    let accounts = store::accounts();
    let shortcuts = std::env::args().any(|a| a == "--shortcuts");
    let account = match std::env::args().skip(1).find(|a| !a.starts_with("--")) {
        Some(email) => accounts.into_iter().find(|a| a.email.eq_ignore_ascii_case(&email)).expect("that account isn't connected"),
        None => accounts.into_iter().next().expect("a connected Google account"),
    };
    let token = store::refresh_token(&account.email).expect("the account's token");
    let drive = Drive::new(cfg, token);
    println!("account {}", account.email);
    let started = Instant::now();

    let root = folder(&drive, "root", ROOT);
    let files = folder(&drive, &root, "5000 files");
    let folders = folder(&drive, &root, "1000 folders");
    let transfer = folder(&drive, &root, "transfer");
    if std::env::args().any(|a| a == "--huge") {
        huge(&drive, &transfer);
        println!("done in {:.0} s", started.elapsed().as_secs_f64());
        return;
    }
    let small = folder(&drive, &transfer, "small");

    let names: Vec<String> = (1..=5000).map(|i| format!("file {i:04}.txt")).collect();
    if shortcuts {
        let targets: Vec<DriveFile> = children(&drive, "root").into_iter().filter(|f| !f.is_dir).collect();
        assert!(targets.len() >= 12, "12 files at the top of My Drive to point shortcuts at");
        // The first file already has about 500 from an earlier run: the rest go to the others,
        // 450 each at most.
        let next = std::sync::atomic::AtomicUsize::new(0);
        fill(&drive, &files, &names, |name| {
            let n = next.fetch_add(1, Ordering::Relaxed);
            Job::Shortcut(targets[1 + n / 450].id.clone(), name)
        });
    } else {
        let tiny = local_file("bench-1k.txt", 1 << 10);
        fill(&drive, &files, &names, |name| Job::Upload(tiny.clone(), name));
    }

    let names: Vec<String> = (1..=1000).map(|i| format!("folder {i:04}")).collect();
    fill(&drive, &folders, &names, Job::Folder);
    let hundred = folder(&drive, &root, "100 folders");
    let names: Vec<String> = (1..=100).map(|i| format!("folder {i:03}")).collect();
    fill(&drive, &hundred, &names, Job::Folder);
    if shortcuts {
        let large = folder(&drive, &transfer, "large");
        if children(&drive, &large).is_empty() {
            let copies: Vec<DriveFile> = children(&drive, "root").into_iter().filter(|f| f.name == "openexplorer-speed-500.bin").collect();
            match copies.as_slice() {
                [_, second, ..] => {
                    drive.move_to(&second.id, "root", &large).expect("moving a copy into transfer/large");
                    println!("moved a copy of {} into transfer/large", second.name);
                }
                _ => println!("transfer/large: no second copy of openexplorer-speed-500.bin to move"),
            }
        }
        println!("done in {:.0} s (nothing uploaded)", started.elapsed().as_secs_f64());
        return;
    }

    let half_mb = local_file("bench-512k.bin", 512 << 10);
    let names: Vec<String> = (1..=200).map(|i| format!("small {i:03}.bin")).collect();
    fill(&drive, &small, &names, |name| Job::Upload(half_mb.clone(), name));

    let large_dir = folder(&drive, &transfer, "large");
    let large = local_file("bench-500m.bin", 500 << 20);
    fill(&drive, &large_dir, &["large.bin".to_string()], |name| Job::Upload(large.clone(), name));

    huge(&drive, &transfer);

    println!("done in {:.0} s", started.elapsed().as_secs_f64());
}

/// `transfer/huge/huge.bin`, when it isn't there and the account has room for it.
fn huge(drive: &Drive, transfer: &str) {
    let huge_dir = folder(drive, transfer, "huge");
    if children(drive, &huge_dir).iter().all(|f| f.name != "huge.bin") {
        let free = drive.quota().expect("the account's storage").free();
        if free.is_some_and(|n| n < (2 << 30) + (100 << 20)) {
            println!("transfer/huge: the account has {} MB free, not enough for 2 GiB", free.unwrap_or(0) >> 20);
        } else {
            let huge = local_file("bench-2g.bin", 2 << 30);
            fill(drive, &huge_dir, &["huge.bin".to_string()], |name| Job::Upload(huge.clone(), name));
        }
    }
}

enum Job {
    Folder(String),
    Upload(PathBuf, String),
    /// A shortcut to the file with this id.
    Shortcut(String, String),
}

/// The folder `name` in `parent`, made if it isn't there.
fn folder(drive: &Drive, parent: &str, name: &str) -> String {
    let found = children(drive, parent).into_iter().find(|f| f.is_dir && f.name == name);
    match found {
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

/// Makes in `parent` each of `names` not there yet, `WORKERS` at a time.
fn fill(drive: &Drive, parent: &str, names: &[String], job: impl Fn(String) -> Job) {
    let have: HashSet<String> = children(drive, parent).into_iter().map(|f| f.name).collect();
    let todo: Vec<Job> = names.iter().filter(|n| !have.contains(*n)).map(|n| job(n.clone())).collect();
    let total = todo.len();
    println!("{parent}: {} there, {total} to make", names.len() - total);
    if total == 0 {
        return;
    }
    let queue = Mutex::new(todo);
    let done = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..WORKERS {
            scope.spawn(|| {
                loop {
                    let Some(job) = queue.lock().unwrap().pop() else { break };
                    let result = match &job {
                        Job::Folder(name) => drive.create_folder(name, parent).map(|_| ()),
                        Job::Upload(path, name) => drive.upload(path, parent, name, &mut |_| true).map(|_| ()),
                        Job::Shortcut(target, name) => drive.create_shortcut(name, target, parent).map(|_| ()),
                    };
                    if let Err(e) = result {
                        failed.fetch_add(1, Ordering::Relaxed);
                        eprintln!("  failed: {e}");
                    }
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if n.is_multiple_of(250) || n == total {
                        println!("  {n}/{total}");
                    }
                }
            });
        }
    });
    let failed = failed.load(Ordering::Relaxed);
    if failed > 0 {
        println!("  {failed} failed: run again to make them");
    }
}

/// A local file of `size` bytes that don't repeat (so nothing on the way can shrink them), kept
/// in the temp folder for the next run.
fn local_file(name: &str, size: usize) -> PathBuf {
    let path = std::env::temp_dir().join(name);
    if std::fs::metadata(&path).map(|m| m.len() as usize).unwrap_or(0) == size {
        return path;
    }
    write_noise(&path, size);
    path
}

fn write_noise(path: &Path, size: usize) {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).expect("a temp file"));
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15 ^ size as u64;
    let mut left = size;
    while left > 0 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let n = left.min(8);
        f.write_all(&x.to_le_bytes()[..n]).unwrap();
        left -= n;
    }
}
