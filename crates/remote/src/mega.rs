//! A MEGA account as a connection the panes, the queue and the copies between servers work
//! through: every path call goes to the account's shared session (see
//! `openexplorer_mega::reach`), so the pool's "connections" to it cost no login of their own.
//!
//! Deleting outside the Rubbish Bin sends to the bin; inside it, it's for good.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use openexplorer_mega::Account;

use crate::{CANCELLED, Conn, Item, Progress, Result, Server, onward};

/// The MEGA account `server` stands for: its e-mail is the server's user.
pub struct Mega {
    account: Arc<Account>,
}

impl Mega {
    pub fn connect(server: &Server) -> Result<Mega> {
        Ok(Mega { account: openexplorer_mega::reach(&server.user)? })
    }
}

fn item(i: openexplorer_mega::Item) -> Item {
    let modified = i.modified.filter(|t| *t > 0).map(|t| SystemTime::UNIX_EPOCH + Duration::from_secs(t as u64));
    Item { name: i.name, is_dir: i.is_dir, size: i.size, modified, link: false }
}

/// The crate's word for a cancelled transfer is the same as ours; kept apart in case it changes.
fn same_cancel(e: String) -> String {
    if e == openexplorer_mega::CANCELLED { CANCELLED.into() } else { e }
}

impl Conn for Mega {
    fn list(&mut self, dir: &str) -> Result<Vec<Item>> {
        Ok(self.account.list(dir)?.into_iter().map(item).collect())
    }

    fn stat(&mut self, path: &str) -> Result<Option<Item>> {
        Ok(self.account.stat(path).map(item))
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

    fn remove_dir(&mut self, path: &str) -> Result<()> {
        self.account.remove_empty_dir(path)
    }

    /// A whole folder goes in one step (to the bin, or for good from it).
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

    /// Within the account MEGA copies by itself: a file's bytes aren't sent again.
    fn copy(&mut self, from: &str, to: &str) -> Result<bool> {
        self.account.copy(from, to).map(|_| true)
    }

    fn pin(&mut self, path: &str) -> Result<String> {
        self.account.pin(path)
    }

    fn alive(&mut self) -> bool {
        true
    }
}
