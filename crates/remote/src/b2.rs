//! A Backblaze B2 key as a connection the panes, the queue and the copies between servers work
//! through: every call goes to the key's shared account (see `openexplorer_b2::reach`), so the
//! pool's "connections" to it cost no login of their own. What each connection keeps is its
//! upload URL, reused for every file it sends (B2 asks for one per thread).
//!
//! The top lists the buckets; deleting hides (see `openexplorer_b2`'s docs); copies within the
//! account happen on B2's side.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use openexplorer_b2::{Account, Uploads};

use crate::{CANCELLED, Conn, Item, Progress, Result, Server};

/// The B2 key `server` stands for: its key ID is the server's user.
pub struct B2 {
    account: Arc<Account>,
    up: Uploads,
}

impl B2 {
    pub fn connect(server: &Server) -> Result<B2> {
        Ok(B2 { account: openexplorer_b2::reach(&server.user)?, up: Uploads::default() })
    }
}

fn item(i: openexplorer_b2::Item) -> Item {
    let modified = i.modified.filter(|t| *t > 0).map(|t| SystemTime::UNIX_EPOCH + Duration::from_millis(t as u64));
    Item { name: i.name, is_dir: i.is_dir, size: i.size, modified, link: false }
}

/// The crate's word for a cancelled transfer is the same as ours; kept apart in case it changes.
fn same_cancel(e: String) -> String {
    if e == openexplorer_b2::CANCELLED { CANCELLED.into() } else { e }
}

/// An upload's progress that never goes back: a part or a file sent again after a busy answer
/// starts its count over, and the queue's rows add what's new since the last count (a count
/// going back would wrap around).
fn onward(progress: Progress<'_>) -> impl FnMut(u64) -> bool + '_ {
    let mut top = 0;
    move |n| {
        top = top.max(n);
        progress(top)
    }
}

impl Conn for B2 {
    fn list(&mut self, dir: &str) -> Result<Vec<Item>> {
        Ok(self.account.list(dir)?.into_iter().map(item).collect())
    }

    fn stat(&mut self, path: &str) -> Result<Option<Item>> {
        Ok(self.account.stat(path)?.map(item))
    }

    fn mkdir(&mut self, path: &str) -> Result<()> {
        self.account.mkdir(&mut self.up, path)
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        self.account.rename(from, to)
    }

    /// Hidden, not deleted: its versions stay as the bucket's rules say.
    fn remove_file(&mut self, path: &str) -> Result<()> {
        self.account.hide(path, false).map(|_| ()).map_err(String::from)
    }

    fn remove_dir(&mut self, path: &str) -> Result<()> {
        self.account.remove_dir(path)
    }

    /// A whole folder in one pass over its files, not folder by folder.
    fn remove(&mut self, path: &str, is_dir: bool) -> Result<()> {
        self.account.hide(path, is_dir).map(|_| ()).map_err(String::from)
    }

    fn download(&mut self, path: &str, out: &mut dyn Write, progress: Progress) -> Result<()> {
        self.account.download(path, out, progress).map_err(same_cancel)
    }

    fn upload(&mut self, path: &str, src: &mut dyn Read, len: u64, progress: Progress) -> Result<()> {
        self.account.upload(&mut self.up, path, src, len, &mut onward(progress)).map(|_| ()).map_err(same_cancel)
    }

    /// From this computer, a large file's upload notes which file it sends, so one that stops
    /// midway can be taken up again from the Envios inacabados dialog.
    fn upload_file(&mut self, path: &str, local: &std::path::Path, progress: Progress) -> Result<()> {
        self.account.upload_file(&mut self.up, path, local, &mut onward(progress)).map(|_| ()).map_err(same_cancel)
    }

    fn home(&mut self) -> Result<String> {
        Ok("/".into())
    }

    fn is_link(&mut self, _path: &str) -> Result<bool> {
        Ok(false)
    }

    /// Within the account B2 copies by itself: a file's bytes aren't sent again.
    fn copy(&mut self, from: &str, to: &str) -> Result<bool> {
        self.account.copy(from, to).map(|_| true)
    }

    fn alive(&mut self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn upload_progress_never_goes_back() {
        let mut seen = Vec::new();
        let mut report = |n| {
            seen.push(n);
            true
        };
        let mut onward = super::onward(&mut report);
        for n in [10, 20, 0, 5, 30] {
            onward(n);
        }
        drop(onward);
        assert_eq!(seen, [10, 20, 20, 20, 30]);
    }
}
