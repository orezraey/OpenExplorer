//! A Telegram account as a connection the panes, the queue and the copies between servers work
//! through: every call goes to the account open for the user id (see
//! `openexplorer_telegram::reach_id`), so the pool's "connections" share one session, one tree
//! and one budget of requests.
//!
//! Deleting is for good (Telegram has no bin); copies within the account happen on Telegram's
//! side; small files go ten to a message group, so a folder of them costs fewer of the messages
//! Telegram paces.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use openexplorer_telegram::{Account, Found};

use crate::{CANCELLED, Conn, Item, Progress, Result, Server, onward, onward_each};

/// Files up to this size go in message groups, read whole into memory first (ten at most).
pub const GROUP_FILE_MAX: u64 = 4 * 1024 * 1024;

pub struct Telegram {
    account: Arc<Account>,
}

impl Telegram {
    pub fn connect(server: &Server) -> Result<Telegram> {
        let id: i64 = server.user.parse().map_err(|_| format!("telegram: {}", server.user))?;
        Ok(Telegram { account: openexplorer_telegram::reach_id(id)? })
    }
}

fn item(i: openexplorer_telegram::Item) -> Item {
    let modified = (i.modified > 0).then(|| SystemTime::UNIX_EPOCH + Duration::from_secs(i.modified as u64));
    Item { is_dir: i.is_dir(), size: (!i.is_dir()).then_some(i.size), modified, name: i.name, link: false }
}

fn same_cancel(e: String) -> String {
    if e == openexplorer_telegram::CANCELLED { CANCELLED.into() } else { e }
}

fn mtime(m: &std::fs::Metadata) -> Option<i64> {
    m.modified().ok()?.duration_since(SystemTime::UNIX_EPOCH).ok().map(|d| d.as_secs() as i64)
}

impl Conn for Telegram {
    fn list(&mut self, dir: &str) -> Result<Vec<Item>> {
        Ok(self.account.list(dir)?.into_iter().map(item).collect())
    }

    fn stat(&mut self, path: &str) -> Result<Option<Item>> {
        Ok(match self.account.stat(path)? {
            None => None,
            Some(Found::Root) => Some(Item { name: String::new(), is_dir: true, size: None, modified: None, link: false }),
            Some(Found::Item(i)) => Some(item(i)),
        })
    }

    fn mkdir(&mut self, path: &str) -> Result<()> {
        self.account.mkdir(path)
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        self.account.rename(from, to)
    }

    fn remove_file(&mut self, path: &str) -> Result<()> {
        self.account.remove(path, false)
    }

    fn remove_dir(&mut self, path: &str) -> Result<()> {
        self.account.remove(path, false)
    }

    /// A whole folder goes at once: its messages in batches of 100.
    fn remove(&mut self, path: &str, _is_dir: bool) -> Result<()> {
        self.account.remove(path, true)
    }

    fn download(&mut self, path: &str, out: &mut dyn Write, progress: Progress) -> Result<()> {
        self.account.download(path, out, &mut onward(progress)).map_err(same_cancel)
    }

    fn upload(&mut self, path: &str, src: &mut dyn Read, len: u64, progress: Progress) -> Result<()> {
        self.account.upload(path, src, len, None, &mut onward(progress)).map_err(same_cancel)
    }

    /// With the file's modified time, which Telegram keeps in the record.
    fn upload_file(&mut self, path: &str, local: &Path, progress: Progress) -> Result<()> {
        let mut file = std::fs::File::open(local).map_err(|e| openexplorer_i18n::tr!("Não consegui abrir {}: {e}", "Couldn't open {}: {e}", local.display()))?;
        let meta = file.metadata().map_err(|e| e.to_string())?;
        self.account.upload(path, &mut file, meta.len(), mtime(&meta), &mut onward(progress)).map_err(same_cancel)
    }

    fn home(&mut self) -> Result<String> {
        Ok("/".into())
    }

    fn is_link(&mut self, _path: &str) -> Result<bool> {
        Ok(false)
    }

    /// On Telegram's side: a document is sent again by reference, a folder item by item.
    fn copy(&mut self, from: &str, to: &str) -> Result<bool> {
        self.account.copy(from, to).map(|()| true)
    }

    /// Files up to [`GROUP_FILE_MAX`] ten to a message group; bigger ones each alone.
    fn upload_files(&mut self, files: &[(String, PathBuf)], progress: &mut dyn FnMut(usize, u64) -> bool) -> Vec<Result<()>> {
        let mut progress = onward_each(progress);
        let mut out: Vec<Option<Result<()>>> = vec![None; files.len()];
        let mut small = Vec::new();
        let mut sizes = Vec::new();
        for (i, (path, local)) in files.iter().enumerate() {
            match std::fs::metadata(local) {
                Ok(m) if m.len() <= GROUP_FILE_MAX => {
                    if !progress(i, 0) {
                        out[i] = Some(Err(CANCELLED.into()));
                        continue;
                    }
                    match std::fs::read(local) {
                        Ok(bytes) => {
                            small.push((path.clone(), bytes, mtime(&m)));
                            sizes.push((i, m.len()));
                        }
                        Err(e) => out[i] = Some(Err(openexplorer_i18n::tr!("Não consegui ler {}: {e}", "Couldn't read {}: {e}", local.display()))),
                    }
                }
                _ => out[i] = Some(self.upload_file(path, local, &mut |n| progress(i, n))),
            }
        }
        for ((i, len), r) in sizes.into_iter().zip(self.account.upload_small(small)) {
            if r.is_ok() {
                progress(i, len);
            }
            out[i] = Some(r.map_err(same_cancel));
        }
        out.into_iter().map(|r| r.unwrap_or(Ok(()))).collect()
    }

    /// A twin (`a (2).txt`) by its message: its shown name moves once the other twin goes.
    fn pin(&mut self, path: &str) -> Result<String> {
        self.account.pin(path)
    }

    fn batches(&self) -> (usize, u64) {
        (openexplorer_telegram::backend::GROUP, GROUP_FILE_MAX)
    }

    fn alive(&mut self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openexplorer_telegram::fake::Fake;

    fn conn(id: i64) -> Telegram {
        let account = Arc::new(Account::open(id, "Ana", Box::new(Fake::new()), None));
        openexplorer_telegram::open::adopt(id, account);
        Telegram::connect(&Server::telegram(id, "Ana")).unwrap()
    }

    #[test]
    fn a_channel_works_as_a_connection() {
        let mut c = conn(9001);
        assert!(c.list("/").unwrap().is_empty());
        c.mkdir("/Fotos").unwrap();
        c.upload("/Fotos/a.txt", &mut &b"abc"[..], 3, &mut |_| true).unwrap();
        let root = c.stat("/").unwrap().unwrap();
        assert!(root.is_dir);
        let a = c.stat("/Fotos/a.txt").unwrap().unwrap();
        assert_eq!((a.is_dir, a.size), (false, Some(3)));
        assert!(c.copy("/Fotos", "/Cópia").unwrap());
        c.rename("/Cópia/a.txt", "/b.txt").unwrap();
        let mut out = Vec::new();
        c.download("/b.txt", &mut out, &mut |_| true).unwrap();
        assert_eq!(out, b"abc");
        c.remove("/Fotos", true).unwrap();
        let names: Vec<String> = c.list("/").unwrap().into_iter().map(|i| i.name).collect();
        assert_eq!(names, ["Cópia", "b.txt"]);
        assert_eq!(Server::telegram(9001, "Ana").address(), "telegram://9001");
    }

    #[test]
    fn small_files_go_in_groups_and_big_ones_alone() {
        let dir = std::env::temp_dir().join(format!("oe-remote-telegram-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut files = Vec::new();
        for i in 0..12 {
            let local = dir.join(format!("f{i}.txt"));
            std::fs::write(&local, format!("arquivo {i}")).unwrap();
            files.push((format!("/f{i}.txt"), local));
        }
        let big = dir.join("grande.bin");
        std::fs::write(&big, vec![7u8; GROUP_FILE_MAX as usize + 1]).unwrap();
        files.push(("/grande.bin".into(), big));
        let mut c = conn(9002);
        let mut seen = vec![0u64; files.len()];
        let r = c.upload_files(&files, &mut |i, n| {
            seen[i] = n;
            true
        });
        assert!(r.iter().all(Result::is_ok), "{r:?}");
        assert_eq!(c.list("/").unwrap().len(), 13);
        assert_eq!(seen[12], GROUP_FILE_MAX + 1);
        assert_eq!(seen[3], 9);
        let _ = std::fs::remove_dir_all(dir);
    }
}
