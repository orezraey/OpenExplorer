//! A OneDrive account as a connection the panes, the queue and the copies between servers work
//! through: every call goes to the account open in the OneDrive crate (see
//! `openexplorer_onedrive::reach`), so the pool's "connections" to it cost no login of their own.
//! The server's user is the drive's id; its name, the e-mail shown.
//!
//! Deleting sends to OneDrive's recycle bin, a whole folder in one step.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use openexplorer_onedrive::Account;

use crate::{CANCELLED, Conn, Item, Progress, Result, Server, onward};

pub struct OneDrive {
    account: Arc<Account>,
}

impl OneDrive {
    pub fn connect(server: &Server) -> Result<OneDrive> {
        Ok(OneDrive { account: openexplorer_onedrive::reach(&server.user)? })
    }
}

fn item(i: openexplorer_onedrive::Item) -> Item {
    let modified = i.modified.filter(|t| *t > 0).map(|t| SystemTime::UNIX_EPOCH + Duration::from_secs(t as u64));
    Item { name: i.name, is_dir: i.is_dir, size: i.size, modified, link: false }
}

fn same_cancel(e: String) -> String {
    if e == openexplorer_onedrive::CANCELLED { CANCELLED.into() } else { e }
}

impl Conn for OneDrive {
    fn list(&mut self, dir: &str) -> Result<Vec<Item>> {
        Ok(self.account.list(dir)?.into_iter().map(item).collect())
    }

    fn read_ahead(&mut self, dir: &str) {
        self.account.read_ahead(dir);
    }

    fn list_pages(&mut self, dir: &str, page: &mut dyn FnMut(Vec<Item>, bool) -> bool) -> Result<()> {
        self.account.list_paged(dir, &mut |items, more| page(items.into_iter().map(item).collect(), more))
    }

    fn stat(&mut self, path: &str) -> Result<Option<Item>> {
        Ok(self.account.stat(path)?.map(item))
    }

    fn mkdir(&mut self, path: &str) -> Result<()> {
        self.account.mkdir(path).map(|_| ())
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        self.account.rename(from, to)
    }

    fn remove_file(&mut self, path: &str) -> Result<()> {
        self.account.remove(path)
    }

    /// Only while it's empty, as OneDrive says now: undoing Nova pasta never takes what arrived.
    fn remove_dir(&mut self, path: &str) -> Result<()> {
        self.account.remove_empty_dir(path)
    }

    /// A whole folder goes to the recycle bin in one step.
    fn remove(&mut self, path: &str, _is_dir: bool) -> Result<()> {
        self.account.remove(path)
    }

    fn download(&mut self, path: &str, out: &mut dyn Write, progress: Progress) -> Result<()> {
        self.account.download(path, out, &mut onward(progress)).map_err(same_cancel)
    }

    fn upload(&mut self, path: &str, src: &mut dyn Read, len: u64, progress: Progress) -> Result<()> {
        self.account.upload(path, src, len, &mut onward(progress)).map(|_| ()).map_err(same_cancel)
    }

    fn home(&mut self) -> Result<String> {
        Ok("/".into())
    }

    fn is_link(&mut self, _path: &str) -> Result<bool> {
        Ok(false)
    }

    /// Within the account OneDrive copies by itself: the bytes aren't sent again.
    fn copy(&mut self, from: &str, to: &str) -> Result<bool> {
        self.account.copy(from, to).map(|_| true)
    }

    fn alive(&mut self) -> bool {
        true
    }
}
