//! Times downloading one Drive file in one stream and in parallel segments, rounds alternating,
//! into the temp folder (deleted after each run). For choosing how big files come down.
//!
//! `cargo run -p openexplorer-gdrive --release --example download_speed -- <folder/in/My Drive> [rounds] [pieces,…]`
//! (pieces: the numbers of parallel segments to compare, 1,4 by default)
//! (the first file in that folder, first connected account)

use std::time::Instant;

use openexplorer_gdrive::{ClientConfig, Drive, DriveFile, store};

fn main() {
    let path = std::env::args().nth(1).expect("a folder in My Drive");
    let rounds: usize = std::env::args().nth(2).and_then(|r| r.parse().ok()).unwrap_or(3);
    let cfg = ClientConfig::load().expect("the app's Google client");
    let account = store::accounts().into_iter().next().expect("a connected Google account");
    let drive = Drive::new(cfg, store::refresh_token(&account.email).expect("the account's token"));
    let mut folder = "root".to_string();
    for name in path.split('/').filter(|s| !s.is_empty()) {
        folder = children(&drive, &folder).into_iter().find(|f| f.is_dir && f.name == name).unwrap_or_else(|| panic!("no folder {name}")).id;
    }
    let file = children(&drive, &folder).into_iter().find(|f| !f.is_dir).expect("a file in the folder");
    let mb = file.size.unwrap_or(0) as f64 / 1e6;
    println!("{} ({mb:.0} MB)", file.name);
    let to = std::env::temp_dir().join("openexplorer-download-speed.bin");
    let pieces: Vec<usize> = std::env::args().nth(3).map(|p| p.split(',').filter_map(|n| n.parse().ok()).collect()).unwrap_or_else(|| vec![1, 4]);
    for round in 1..=rounds {
        for &n in &pieces {
            let t = Instant::now();
            let bytes = drive.download_in(&file, &to, n, &mut |_| true).expect("download");
            let s = t.elapsed().as_secs_f64();
            println!("round {round} {n:>2} piece(s) {s:6.2} s  {:6.1} MB/s", bytes as f64 / 1e6 / s);
            let _ = std::fs::remove_file(&to);
        }
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
