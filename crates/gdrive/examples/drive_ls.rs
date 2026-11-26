//! Lists a My Drive folder, read only: each item's size and name, and the folder's totals. For
//! picking files to time downloads with (`bench\README.md`).
//!
//! With `--deep`, only the totals of everything inside, subfolders included.
//!
//! `cargo run -p openexplorer-gdrive --release --example drive_ls -- [e-mail] [folder/inside/My Drive] [--deep]`

use openexplorer_gdrive::{ClientConfig, Drive, DriveFile, store};

fn main() {
    let cfg = ClientConfig::load().expect("the app's Google client (google-client.json)");
    let deep = std::env::args().any(|a| a == "--deep");
    let args: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with("--")).collect();
    let accounts = store::accounts();
    let (account, path) = match args.first().filter(|a| a.contains('@')) {
        Some(email) => (accounts.into_iter().find(|a| a.email.eq_ignore_ascii_case(email)).expect("that account isn't connected"), args.get(1)),
        None => (accounts.into_iter().next().expect("a connected Google account"), args.first()),
    };
    let drive = Drive::new(cfg, store::refresh_token(&account.email).expect("the account's token"));
    let mut folder = "root".to_string();
    for name in path.map(|p| p.split('/').filter(|s| !s.is_empty()).collect::<Vec<_>>()).unwrap_or_default() {
        let found = children(&drive, &folder).into_iter().find(|f| f.is_dir && f.name == name).unwrap_or_else(|| panic!("no folder {name}"));
        folder = found.id;
    }
    if deep {
        let (mut folders, mut files, mut bytes, mut todo) = (0usize, 0usize, 0u64, vec![folder]);
        while let Some(f) = todo.pop() {
            for item in children(&drive, &f) {
                if item.is_dir {
                    folders += 1;
                    todo.push(item.id);
                } else {
                    files += 1;
                    bytes += item.size.unwrap_or(0);
                }
            }
        }
        println!("{folders} folders, {files} files, {:.1} MB inside", bytes as f64 / 1e6);
        return;
    }
    let items = children(&drive, &folder);
    let (mut files, mut bytes) = (0usize, 0u64);
    for f in &items {
        if f.is_dir {
            println!("{:>12}  {}/", "", f.name);
        } else {
            files += 1;
            bytes += f.size.unwrap_or(0);
            println!("{:>12}  {}", f.size.map(|b| b.to_string()).unwrap_or_else(|| "-".into()), f.name);
        }
    }
    println!("{} items, {files} files, {:.1} MB", items.len(), bytes as f64 / 1e6);
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
