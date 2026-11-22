//! A channel in memory, for the tests of `account` and the app's: messages with their bytes, a
//! change log, edits that can be made to expire, and counts of what was asked.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};

use crate::backend::{Backend, Changes, Progress, Small};
use crate::tree::{Doc, Msg};
use crate::{CANCELLED, Result};

enum Event {
    Upsert(i32),
    Delete(Vec<i32>),
}

#[derive(Default)]
struct Channel {
    msgs: BTreeMap<i32, (Msg, Vec<u8>)>,
    next: i32,
    now: i64,
    log: Vec<(i32, Event)>,
    pts: i32,
    /// The log before this position is gone: older positions get [`Changes::TooLong`].
    oldest: i32,
}

impl Channel {
    fn add(&mut self, text: &str, doc: Option<(String, Vec<u8>)>) -> Msg {
        self.next += 1;
        self.now += 1;
        let (doc, bytes) = match doc {
            Some((name, bytes)) => (Some(Doc { size: bytes.len() as u64, name, photo: false }), bytes),
            None => (None, Vec::new()),
        };
        let m = Msg { id: self.next, text: text.into(), date: self.now, doc };
        self.msgs.insert(m.id, (m.clone(), bytes));
        self.note(Event::Upsert(m.id));
        m
    }

    fn note(&mut self, e: Event) {
        self.pts += 1;
        self.log.push((self.pts, e));
    }
}

#[derive(Default)]
pub struct Fake {
    ch: Mutex<Channel>,
    /// Edits answer "too old to edit".
    pub edits_expire: AtomicBool,
    pub histories: AtomicUsize,
    pub sends: AtomicUsize,
    pub groups: AtomicUsize,
    pub edits: AtomicUsize,
    pub copies: AtomicUsize,
    /// The largest file; 0 is the free account's 2000 MiB.
    pub max: AtomicUsize,
    /// Until when Telegram asks to wait (seconds since 1970); 0 when it doesn't.
    pub waiting: AtomicI64,
}

impl Fake {
    pub fn new() -> Fake {
        Fake::default()
    }

    /// A message as someone else would send it: by hand, from a phone, from another PC.
    pub fn post(&self, text: &str, doc: Option<(&str, &[u8])>) -> Msg {
        self.ch.lock().unwrap().add(text, doc.map(|(n, b)| (n.to_string(), b.to_vec())))
    }

    /// An edit made elsewhere.
    pub fn edit_elsewhere(&self, id: i32, text: &str) {
        let mut ch = self.ch.lock().unwrap();
        if let Some((m, _)) = ch.msgs.get_mut(&id) {
            m.text = text.into();
            ch.note(Event::Upsert(id));
        }
    }

    pub fn delete_elsewhere(&self, ids: &[i32]) {
        let _ = self.delete(ids);
    }

    /// Forgets the change log, as Telegram does when a client was away too long.
    pub fn drop_log(&self) {
        let mut ch = self.ch.lock().unwrap();
        ch.oldest = ch.pts;
        ch.log.clear();
    }

    pub fn messages(&self) -> Vec<Msg> {
        self.ch.lock().unwrap().msgs.values().map(|(m, _)| m.clone()).collect()
    }

    pub fn bytes(&self, id: i32) -> Option<Vec<u8>> {
        self.ch.lock().unwrap().msgs.get(&id).map(|(_, b)| b.clone())
    }
}

impl Backend for Fake {
    fn history(&self) -> Result<(Vec<Msg>, i32)> {
        self.histories.fetch_add(1, Ordering::SeqCst);
        let ch = self.ch.lock().unwrap();
        Ok((ch.msgs.values().rev().map(|(m, _)| m.clone()).collect(), ch.pts))
    }

    fn changes(&self, pts: i32) -> Result<Changes> {
        let ch = self.ch.lock().unwrap();
        if pts < ch.oldest {
            return Ok(Changes::TooLong);
        }
        let mut upserts = Vec::new();
        let mut deleted = Vec::new();
        for (at, e) in &ch.log {
            if *at <= pts {
                continue;
            }
            match e {
                Event::Upsert(id) => {
                    if let Some((m, _)) = ch.msgs.get(id) {
                        upserts.retain(|u: &Msg| u.id != *id);
                        upserts.push(m.clone());
                    }
                }
                Event::Delete(ids) => deleted.extend(ids),
            }
        }
        upserts.retain(|u| !deleted.contains(&u.id));
        Ok(Changes::Since { upserts, deleted, pts: ch.pts })
    }

    fn send_text(&self, text: &str) -> Result<Msg> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        Ok(self.ch.lock().unwrap().add(text, None))
    }

    fn edit(&self, id: i32, text: &str) -> Result<Option<Msg>> {
        self.edits.fetch_add(1, Ordering::SeqCst);
        if self.edits_expire.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let mut ch = self.ch.lock().unwrap();
        let m = {
            let (m, _) = ch.msgs.get_mut(&id).ok_or("A mensagem não existe mais.")?;
            m.text = text.into();
            m.clone()
        };
        ch.note(Event::Upsert(id));
        Ok(Some(m))
    }

    fn send_file(&self, caption: &str, name: &str, src: &mut dyn Read, len: u64, progress: Progress) -> Result<Msg> {
        if len > self.max_file() {
            return Err("grande demais".into());
        }
        let mut bytes = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = src.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n]);
            if !progress(bytes.len() as u64) {
                return Err(CANCELLED.into());
            }
        }
        if bytes.len() as u64 != len {
            return Err(format!("leu {} de {len} bytes", bytes.len()));
        }
        self.sends.fetch_add(1, Ordering::SeqCst);
        Ok(self.ch.lock().unwrap().add(caption, Some((name.into(), bytes))))
    }

    fn send_group(&self, files: Vec<Small>) -> Result<Vec<Msg>> {
        assert!(files.len() <= crate::backend::GROUP, "a group holds at most 10");
        self.groups.fetch_add(1, Ordering::SeqCst);
        let mut ch = self.ch.lock().unwrap();
        Ok(files.into_iter().map(|f| ch.add(&f.caption, Some((f.name, f.bytes)))).collect())
    }

    fn copy(&self, id: i32, caption: &str) -> Result<Msg> {
        self.copies.fetch_add(1, Ordering::SeqCst);
        let mut ch = self.ch.lock().unwrap();
        let (m, bytes) = ch.msgs.get(&id).cloned().ok_or("A mensagem não existe mais.")?;
        let doc = m.doc.ok_or("A mensagem não tem arquivo.")?;
        Ok(ch.add(caption, Some((doc.name, bytes))))
    }

    fn delete(&self, ids: &[i32]) -> Result<()> {
        let mut ch = self.ch.lock().unwrap();
        let gone: Vec<i32> = ids.iter().copied().filter(|id| ch.msgs.remove(id).is_some()).collect();
        if !gone.is_empty() {
            ch.note(Event::Delete(gone));
        }
        Ok(())
    }

    fn download(&self, id: i32, out: &mut dyn Write, progress: Progress) -> Result<()> {
        let bytes = self.bytes(id).ok_or("A mensagem não existe mais.")?;
        let mut done = 0;
        for chunk in bytes.chunks(4096) {
            out.write_all(chunk).map_err(|e| e.to_string())?;
            done += chunk.len() as u64;
            if !progress(done) {
                return Err(CANCELLED.into());
            }
        }
        Ok(())
    }

    fn max_file(&self) -> u64 {
        match self.max.load(Ordering::SeqCst) {
            0 => 4000 * 512 * 1024,
            n => n as u64,
        }
    }

    fn waiting_until(&self) -> Option<i64> {
        Some(self.waiting.load(Ordering::SeqCst)).filter(|t| *t > 0)
    }
}

/// A channel the test keeps a handle on while an account works over it.
impl Backend for std::sync::Arc<Fake> {
    fn history(&self) -> Result<(Vec<Msg>, i32)> {
        (**self).history()
    }
    fn changes(&self, pts: i32) -> Result<Changes> {
        (**self).changes(pts)
    }
    fn send_text(&self, text: &str) -> Result<Msg> {
        (**self).send_text(text)
    }
    fn edit(&self, id: i32, text: &str) -> Result<Option<Msg>> {
        (**self).edit(id, text)
    }
    fn send_file(&self, caption: &str, name: &str, src: &mut dyn Read, len: u64, progress: Progress) -> Result<Msg> {
        (**self).send_file(caption, name, src, len, progress)
    }
    fn send_group(&self, files: Vec<Small>) -> Result<Vec<Msg>> {
        (**self).send_group(files)
    }
    fn copy(&self, id: i32, caption: &str) -> Result<Msg> {
        (**self).copy(id, caption)
    }
    fn delete(&self, ids: &[i32]) -> Result<()> {
        (**self).delete(ids)
    }
    fn download(&self, id: i32, out: &mut dyn Write, progress: Progress) -> Result<()> {
        (**self).download(id, out, progress)
    }
    fn max_file(&self) -> u64 {
        (**self).max_file()
    }
    fn waiting_until(&self) -> Option<i64> {
        (**self).waiting_until()
    }
}
