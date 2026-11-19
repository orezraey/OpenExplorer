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

    /// Only while it holds nothing: Dropbox would delete a full one whole.
    fn remove_dir(&mut self, path: &str) -> Result<()> {
        if !self.account.list(path)?.is_empty() {
            return Err(openexplorer_i18n::tr!("A pasta “{}” não está vazia.", "The folder “{}” isn't empty.", path.rsplit('/').next().unwrap_or(path)));
        }
        self.account.delete(path).map(|_| ())
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

    /// Not sent when the same bytes are already there (same size and `content_hash`).
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
    use crate::{Protocol, Server, Store};

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
}
