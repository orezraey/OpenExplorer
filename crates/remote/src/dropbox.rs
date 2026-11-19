//! A Dropbox account as a connection the panes, the queue and the copies between servers work
//! through: every call goes to the account open for the e-mail (see
//! `openexplorer_dropbox::reach`), so the pool's "connections" to it cost no login of their own.
//!
//! Deleting goes to Dropbox's deleted files, which can be restored for a while; copies within the
//! account happen on Dropbox's side, and a file sent where the same bytes already are isn't sent.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use openexplorer_dropbox::{Account, Entry};

use crate::{CANCELLED, Conn, Item, Progress, Result, Server};

/// The Dropbox account `server` stands for: its e-mail is the server's user.
pub struct Dropbox {
    account: Arc<Account>,
}

impl Dropbox {
    pub fn connect(server: &Server) -> Result<Dropbox> {
        Ok(Dropbox { account: openexplorer_dropbox::reach(&server.user)? })
    }
}

fn item(e: Entry) -> Item {
    let modified = e.modified.filter(|t| *t > 0).map(|t| SystemTime::UNIX_EPOCH + Duration::from_secs(t as u64));
    Item { name: e.name, is_dir: e.dir, size: (!e.dir).then_some(e.size), modified, link: false }
}

fn same_cancel(e: String) -> String {
    if e == openexplorer_dropbox::CANCELLED { CANCELLED.into() } else { e }
}

impl Conn for Dropbox {
    fn list(&mut self, dir: &str) -> Result<Vec<Item>> {
        Ok(self.account.list(dir)?.into_iter().map(item).collect())
    }

    fn stat(&mut self, path: &str) -> Result<Option<Item>> {
        Ok(self.account.stat(path)?.map(item))
    }

    fn mkdir(&mut self, path: &str) -> Result<()> {
        self.account.mkdir(path).map(|_| ())
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        self.account.rename(from, to).map(|_| ())
    }

    fn remove_file(&mut self, path: &str) -> Result<()> {
        self.account.delete(path).map(|_| ())
    }

    /// Only while it holds nothing, as Dropbox says at that moment (not the kept listing):
    /// Dropbox would delete a full one whole.
    fn remove_dir(&mut self, path: &str) -> Result<()> {
        self.account.remove_empty_dir(path).map(|_| ())
    }

    /// A whole folder goes in one call.
    fn remove(&mut self, path: &str, _is_dir: bool) -> Result<()> {
        self.account.delete(path).map(|_| ())
    }

    fn download(&mut self, path: &str, out: &mut dyn Write, progress: Progress) -> Result<()> {
        self.account.download(path, out, progress).map_err(same_cancel)
    }

    fn upload(&mut self, path: &str, src: &mut dyn Read, len: u64, progress: Progress) -> Result<()> {
        self.account.upload(path, src, len, progress).map(|_| ()).map_err(same_cancel)
    }

    /// Not sent when the same bytes are already there (same size and `content_hash`), as the
    /// folder's kept listing tells while it's known current: no request is spent asking.
    fn upload_file(&mut self, path: &str, local: &Path, progress: Progress) -> Result<()> {
        if self.account.same_content(path, local, &|| false).unwrap_or(false) {
            let len = std::fs::metadata(local).map(|m| m.len()).unwrap_or(0);
            return if progress(len) { Ok(()) } else { Err(CANCELLED.into()) };
        }
        crate::send_file(self, path, local, progress)
    }

    fn home(&mut self) -> Result<String> {
        Ok("/".into())
    }

    fn is_link(&mut self, _path: &str) -> Result<bool> {
        Ok(false)
    }

    /// Within the account Dropbox copies by itself: a file's bytes aren't sent again.
    fn copy(&mut self, from: &str, to: &str) -> Result<bool> {
        self.account.copy(from, to).map(|_| true)
    }

    /// In batch jobs (`move_batch_v2`), up to 1,000 items each.
    fn rename_many(&mut self, moves: &[(String, String)]) -> Vec<Result<()>> {
        self.account.move_many(moves).into_iter().map(|r| r.map(|_| ())).collect()
    }

    /// In batch jobs (`copy_batch_v2`), on Dropbox's side.
    fn copy_many(&mut self, copies: &[(String, String)]) -> Vec<Result<bool>> {
        self.account.copy_many(copies).into_iter().map(|r| r.map(|_| true)).collect()
    }

    /// Small files together, a few requests for up to 250 of them; bigger ones each alone. A
    /// file whose same bytes are already there isn't sent.
    fn upload_files(&mut self, files: &[(String, PathBuf)], progress: &mut dyn FnMut(usize, u64) -> bool) -> Vec<Result<()>> {
        self.account.upload_many(files, progress).into_iter().map(|r| r.map(|_| ()).map_err(same_cancel)).collect()
    }

    fn batches(&self) -> (usize, u64) {
        (openexplorer_dropbox::batch::UPLOAD_FILES, openexplorer_dropbox::batch::BATCH_FILE_MAX)
    }

    fn alive(&mut self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::Dropbox;
    use crate::{Conn, Protocol, Server, Store};

    #[test]
    fn accounts_show_as_servers_of_their_own_and_stay_out_of_the_list() {
        let sv = Server::dropbox("ana@exemplo.com");
        assert!(sv.is_dropbox() && !sv.is_mega());
        assert_eq!((sv.id.as_str(), sv.protocol, sv.host.as_str()), ("dropbox:ana@exemplo.com", Protocol::Dropbox, "dropbox.com"));
        assert_eq!(sv.address(), "dropbox://ana@exemplo.com");
        assert!(!Protocol::ALL.contains(&Protocol::Dropbox));
        let dir = std::env::temp_dir().join(format!("oe-remote-dropbox-{}", std::process::id()));
        let store = Store::at(dir.join("servers.json"));
        assert!(store.save(sv, None).is_err());
        assert!(store.servers().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_sent_again_over_the_same_bytes_costs_nothing_and_a_new_one_no_question() {
        use std::sync::atomic::Ordering;
        let fake = openexplorer_dropbox::fake::Fake::start();
        let email = format!("same-{}@exemplo.com", std::process::id());
        let account = std::sync::Arc::new(fake.account());
        openexplorer_dropbox::account::adopt(&email, account.clone());
        let dir = std::env::temp_dir().join(format!("oe-remote-dropbox-same-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let local = dir.join("a.bin");
        std::fs::write(&local, b"os mesmos bytes").unwrap();
        fake.put_file("/D/a.bin", b"os mesmos bytes");
        let mut c = Dropbox::connect(&Server::dropbox(&email)).unwrap();
        c.list("/D").unwrap();
        // The pane showing the folder keeps it known current (here: the log just read).
        account.catch_up().unwrap();
        let before = account.api.requests.load(Ordering::Relaxed);
        c.upload_file("/D/a.bin", &local, &mut |_| true).unwrap();
        assert_eq!(account.api.requests.load(Ordering::Relaxed), before);
        // A new name: sent at once, nothing asked first.
        let before = fake.calls();
        c.upload_file("/D/b.bin", &local, &mut |_| true).unwrap();
        assert_eq!(fake.calls_since(&before), ["files/upload"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_folder_that_got_files_elsewhere_is_not_removed_as_empty() {
        // Desfazer of Nova pasta, after another device put a file in it.
        let fake = openexplorer_dropbox::fake::Fake::start();
        let email = format!("rmdir-{}@exemplo.com", std::process::id());
        openexplorer_dropbox::account::adopt(&email, std::sync::Arc::new(fake.account()));
        let mut c = Dropbox::connect(&Server::dropbox(&email)).unwrap();
        c.mkdir("/Nova pasta").unwrap();
        assert!(c.list("/Nova pasta").unwrap().is_empty());
        fake.put_file("/Nova pasta/foto.jpg", b"jpg");
        assert!(c.remove_dir("/Nova pasta").is_err());
        assert!(fake.has("/Nova pasta/foto.jpg"));
        fake.remove("/Nova pasta/foto.jpg");
        c.remove_dir("/Nova pasta").unwrap();
        assert!(!fake.has("/Nova pasta"));
    }

    /// Requests to send `n` local files each over 8 MiB through `upload_file`, against the
    /// stand-in: into a folder just made (as Copiar a folder does) and into a folder whose
    /// listing is kept but not known current (no watcher waiting).
    fn send_costs(n: usize) -> (u64, u64) {
        use std::sync::atomic::Ordering;
        let fake = openexplorer_dropbox::fake::Fake::start();
        let email = format!("bench-{}-{n}@exemplo.com", std::process::id());
        let account = std::sync::Arc::new(fake.account());
        openexplorer_dropbox::account::adopt(&email, account.clone());
        let dir = std::env::temp_dir().join(format!("oe-remote-dropbox-bench-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let data: Vec<u8> = (0..(9usize << 20)).map(|i| (i * 7 % 251) as u8).collect();
        let locals: Vec<std::path::PathBuf> = (0..n)
            .map(|i| {
                let f = dir.join(format!("f{i}.bin"));
                std::fs::write(&f, &data).unwrap();
                f
            })
            .collect();
        let mut c = Dropbox::connect(&Server::dropbox(&email)).unwrap();
        // A folder just made.
        c.list("/").unwrap();
        c.mkdir("/Nova").unwrap();
        let before = account.api.requests.load(Ordering::Relaxed);
        for (i, l) in locals.iter().enumerate() {
            c.upload_file(&format!("/Nova/f{i}.bin"), l, &mut |_| true).unwrap();
        }
        let new_folder = account.api.requests.load(Ordering::Relaxed) - before;
        // A kept folder, its listing not known current.
        fake.make_dir("/Kept");
        c.list("/Kept").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(3));
        let before = account.api.requests.load(Ordering::Relaxed);
        for (i, l) in locals.iter().enumerate() {
            c.upload_file(&format!("/Kept/f{i}.bin"), l, &mut |_| true).unwrap();
        }
        let kept = account.api.requests.load(Ordering::Relaxed) - before;
        let _ = std::fs::remove_dir_all(dir);
        (new_folder, kept)
    }

    /// Run with `cargo test -p openexplorer-remote --lib dropbox::tests::upload_file_costs -- --ignored --nocapture`.
    #[test]
    #[ignore = "a benchmark: run it by name"]
    fn upload_file_costs() {
        println!("files of 9 MiB | requests, into a folder just made | requests, into a kept folder not known current");
        for n in [1usize, 5, 20] {
            let (new_folder, kept) = send_costs(n);
            println!("{n:>14} | {new_folder:>33} | {kept:>47}");
        }
    }
}
