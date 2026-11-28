//! One Telegram account as the panes see it: paths over the channel's tree, kept current, and
//! what each operation becomes in messages.
//!
//! | Operation | Messages |
//! |---|---|
//! | new folder | one text message |
//! | rename or move | one edit; when Telegram no longer lets the message be edited, a copy with the new record (no bytes travel) and the old one deleted |
//! | copy a file | the document sent again on Telegram's side, with a new record |
//! | copy a folder | a new folder record, then each item copied into it |
//! | delete | the messages deleted, a folder's whole contents in batches of 100 (for good: Telegram has no bin) |
//! | upload over a file | the new one sent, then the old one deleted |
//! | upload past Telegram's limit | the file in parts of the limit, each a message (see `records`) |
//!
//! The top folder also holds the account's chats, read-only (`chats`): a path under it goes
//! there, and anything that would change it is refused. An item of the storage named as the
//! chats' folder (made before it, or by another app) keeps its name; the folder of chats then
//! shows numbered, as a twin would (`Conversas do Telegram (2)`).
//!
//! The tree is brought up to date from the channel's change log before an operation reads it,
//! at most every few seconds (one small request, made under the tree's lock so two panes don't
//! ask at once); what this account does itself goes into the tree as soon as Telegram answers.
//! The lock isn't held while a message is sent or a file moves, so a listing never waits on a
//! transfer or on a FLOOD_WAIT of one.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use openexplorer_cloud::sealed::Secret;
use openexplorer_i18n::{t, tr};

use crate::backend::{Backend, Changes, GROUP, Paged, Progress, Small};
use crate::chats::{self, ChatRef, Chats, Need};
use crate::records;
use crate::tree::{Found, Item, Msg, Tree, numbered};
use crate::{Result, cache};

/// How long the tree is trusted without asking the change log.
const FRESH: Duration = Duration::from_secs(3);

/// Where an account keeps its tree between runs.
pub struct Kept {
    pub file: PathBuf,
    pub secret: Secret,
}

#[derive(Default)]
struct State {
    tree: Tree,
    pts: i32,
    loaded: bool,
    checked: Option<Instant>,
    dirty: bool,
    /// Counts the times the change log or a whole reading changed the tree.
    version: u64,
}

pub struct Account {
    pub user_id: i64,
    /// The name to show: the person's name on Telegram.
    pub name: String,
    backend: Box<dyn Backend>,
    state: Mutex<State>,
    /// The chats, read-only; locked after `state` when both are.
    chats: Mutex<Chats>,
    /// Held from a new folder's check to its message: Telegram doesn't refuse a second folder of
    /// the same name, as the other services do, so two panes making one must take turns.
    making: Mutex<()>,
    /// The size of a file's parts, when not the account's limit (the live tests, to split a file
    /// without sending gigabytes).
    part_size: AtomicU64,
    kept: Option<Kept>,
    mark: u64,
    pub(crate) forgotten: AtomicBool,
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// `/a/b/c` → (`/a/b`, `c`); `c` → (``, `c`).
fn split(path: &str) -> (&str, &str) {
    let p = path.trim_end_matches('/');
    match p.rfind('/') {
        Some(i) => (&p[..i], &p[i + 1..]),
        None => ("", p),
    }
}

fn shown(path: &str) -> String {
    let p = path.trim_matches('/');
    if p.is_empty() { "/".into() } else { format!("/{p}") }
}

fn missing(path: &str) -> String {
    tr!("{} não existe no Telegram.", "{} doesn't exist on Telegram.", shown(path))
}

fn taken(name: &str) -> String {
    tr!("Já existe um item chamado \"{name}\" nesta pasta.", "There's already an item named \"{name}\" in this folder.")
}

fn bad_name(name: &str) -> String {
    tr!("\"{name}\" não pode ser nome de arquivo ou pasta.", "\"{name}\" can't be a file or folder name.")
}

impl Account {
    /// The account over `backend`; the tree is read (from `kept`, else the channel) when first
    /// needed.
    pub fn open(user_id: i64, name: &str, backend: Box<dyn Backend>, kept: Option<Kept>) -> Account {
        Account { user_id, name: name.into(), backend, state: Mutex::default(), chats: Mutex::default(), making: Mutex::new(()), part_size: AtomicU64::new(0), kept, mark: cache::mark(), forgotten: AtomicBool::new(false) }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn chats(&self) -> MutexGuard<'_, Chats> {
        self.chats.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether `path` is in the folder of chats: read-only, and not kept as the storage is.
    pub fn in_chats(&self, path: &str) -> bool {
        self.chat_segs(path).ok().flatten().is_some()
    }

    /// Whether a walk from above passes over `path`: the folder of chats, or a chat in it (a
    /// walk from the folder would read the months of thousands of chats). A walk that starts in
    /// a chat goes into its months.
    pub fn passed_over(&self, path: &str) -> bool {
        self.chat_segs(path).ok().flatten().is_some_and(|segs| segs.len() <= 1)
    }

    /// The name the folder of chats shows as in `tree`'s top folder: its own, unless an item of
    /// the storage has it, then numbered as a twin.
    fn chats_name(tree: &mut Tree) -> String {
        let base = chats::folder_name();
        let mut taken = |name: &str| matches!(tree.find(&format!("/{name}")), Some(Found::Item(_)));
        (1..).map(|n| if n == 1 { base.to_string() } else { numbered(base, n, true) }).find(|n| !taken(n)).unwrap_or_default()
    }

    /// The chats' part of `path` with the storage as `tree` has it: the account shows its chats,
    /// the first folder is one of their folder's names or the one it shows as, and no item of the
    /// storage at the top is named so.
    fn chats_part<'a>(&self, tree: &mut Tree, path: &'a str) -> Option<Vec<&'a str>> {
        let (first, rest) = chats::split_first(path)?;
        if !chats::may_name_folder(first) || !self.backend.has_chats() || matches!(tree.find(&format!("/{first}")), Some(Found::Item(_))) {
            return None;
        }
        (chats::is_folder_name(first) || Account::chats_name(tree).to_lowercase() == first.to_lowercase()).then_some(rest)
    }

    /// The chats' part of `path`, if it's in them (see `chats_part`): a path of the storage is
    /// told apart by its first folder alone, with no lock.
    fn chat_segs<'a>(&self, path: &'a str) -> Result<Option<Vec<&'a str>>> {
        if !chats::split_first(path).is_some_and(|(first, _)| chats::may_name_folder(first)) {
            return Ok(None);
        }
        let mut st = self.current(false)?;
        Ok(self.chats_part(&mut st.tree, path))
    }

    /// Refuses a change to `path` when it's in the chats (so no item of the storage takes their
    /// folder's name).
    fn writable(&self, path: &str) -> Result<()> {
        if self.chat_segs(path)?.is_some() { Err(chats::read_only()) } else { Ok(()) }
    }

    /// The chats kept on disk, looked for once, when first needed (the storage's tree may not
    /// have been read yet).
    fn chats_from_disk(&self) {
        {
            let mut c = self.chats();
            if c.disk_tried {
                return;
            }
            c.disk_tried = true;
        }
        if let Some(k) = &self.kept
            && let Some((_, _, kept)) = cache::load(&k.file, &k.secret)
        {
            self.chats().adopt(kept);
        }
    }

    /// The items of the folder `segs` names under the folder of chats, Telegram asked with no
    /// lock held. `fresh`: what was read over a minute ago is read again.
    fn chats_list(&self, segs: &[&str], fresh: bool) -> Result<Vec<Item>> {
        self.chats_from_disk();
        for _ in 0..4 {
            let need = self.chats().plan(segs, fresh);
            match need {
                Need::Ready(items) => return Ok(items),
                Need::Missing => break,
                Need::NotFolder => return Err(chats::not_folder(segs)),
                Need::First => match self.backend.chats(None, true)? {
                    Paged::Page(p) => self.chats().put_first(p),
                    Paged::WaitUntil(until) => return Err(chats::wait_until(until)),
                },
                Need::Months(chat) => {
                    let m = self.backend.chat_months(&chat)?;
                    self.chats().put_months(chat, m);
                }
                Need::Files(chat, month) => {
                    let f = self.backend.chat_files(&chat, month)?;
                    self.chats().put_files(chat, month, f);
                }
            }
        }
        Err(chats::missing(segs))
    }

    /// The chat and message of the file `segs` names under the folder of chats.
    fn chat_file(&self, segs: &[&str]) -> Result<(ChatRef, i32)> {
        let [chat, month, name] = segs else {
            return Err(if segs.len() < 3 { chats::is_folder(segs) } else { chats::missing(segs) });
        };
        let low = name.to_lowercase();
        let item = self.chats_list(&[chat, month], false)?.into_iter().find(|i| i.name.to_lowercase() == low).ok_or_else(|| chats::missing(segs))?;
        let at = self.chats().chat(chat).ok_or_else(|| chats::missing(segs))?;
        Ok((at, item.msg))
    }

    /// Reads more of the chats for up to `budget`, never waiting on Telegram: whether anything
    /// shown changed (see `Chats::wanted_more`).
    fn more_chats(&self, budget: Duration) -> bool {
        let start = Instant::now();
        let mut changed = false;
        while start.elapsed() < budget {
            let Some(asked) = self.chats().wanted_more() else { break };
            let Ok(paged) = self.backend.chats(asked.as_ref(), false) else { break };
            let (shown, go_on) = self.chats().put_more(asked, paged);
            changed |= shown;
            if !go_on {
                break;
            }
        }
        changed
    }

    /// Adds the folder of chats to the items of the top folder, `tree`'s.
    fn with_chats(&self, tree: &mut Tree, path: &str, mut items: Vec<Item>) -> Vec<Item> {
        if path.trim_matches('/').is_empty() && self.backend.has_chats() {
            items.insert(0, chats::folder_item(Account::chats_name(tree)));
        }
        items
    }

    /// The tree, brought up to date unless it was a moment ago (or `force`).
    fn current(&self, force: bool) -> Result<MutexGuard<'_, State>> {
        let mut st = self.lock();
        if !st.loaded
            && let Some(k) = &self.kept
            && let Some((pts, msgs, kept)) = cache::load(&k.file, &k.secret)
        {
            let mut c = self.chats();
            c.adopt(kept);
            c.disk_tried = true;
            drop(c);
            st.tree = Tree::new(msgs);
            st.pts = pts;
            st.loaded = true;
        }
        if !st.loaded {
            self.read_all(&mut st)?;
            return Ok(st);
        }
        if !force && st.checked.is_some_and(|c| c.elapsed() < FRESH) {
            return Ok(st);
        }
        match self.backend.changes(st.pts)? {
            Changes::Since { upserts, deleted, pts } => {
                if !upserts.is_empty() || !deleted.is_empty() || pts != st.pts {
                    st.dirty = true;
                }
                if !upserts.is_empty() || !deleted.is_empty() {
                    st.version += 1;
                }
                for m in upserts {
                    st.tree.upsert(m);
                }
                for id in deleted {
                    st.tree.remove(id);
                }
                st.pts = pts;
                st.checked = Some(Instant::now());
            }
            Changes::TooLong => self.read_all(&mut st)?,
        }
        Ok(st)
    }

    fn read_all(&self, st: &mut State) -> Result<()> {
        let (msgs, pts) = self.backend.history()?;
        st.tree = Tree::new(msgs);
        st.pts = pts;
        st.loaded = true;
        st.dirty = true;
        st.version += 1;
        st.checked = Some(Instant::now());
        Ok(())
    }

    /// What Telegram answered, into the tree.
    fn apply(&self, sent: impl IntoIterator<Item = Msg>, gone: &[i32]) {
        let mut st = self.lock();
        for m in sent {
            st.tree.upsert(m);
        }
        for id in gone {
            st.tree.remove(*id);
        }
        st.dirty = true;
    }

    /// Asks the change log now, whatever the time since last: whether it brought anything (made
    /// elsewhere, or this app's own changes coming back, which change nothing shown).
    /// Once the chats were opened, it also reads more of them, for a moment and never waiting on
    /// Telegram (`Chats::more`).
    pub fn refresh(&self) -> Result<bool> {
        let before = self.lock().version;
        let changed = self.current(true)?.version != before;
        let more = self.more_chats(Duration::from_secs(3));
        Ok(changed || more)
    }

    /// The items of folder `path`, folders first.
    pub fn list(&self, path: &str) -> Result<Vec<Item>> {
        if let Some(segs) = self.chat_segs(path)? {
            return self.chats_list(&segs, true);
        }
        let mut st = self.current(false)?;
        match st.tree.find(path) {
            Some(f) => match f.dir() {
                Some(dir) => {
                    let dir = dir.to_string();
                    let items = st.tree.list(&dir);
                    Ok(self.with_chats(&mut st.tree, path, items))
                }
                None => Err(tr!("{} não é uma pasta.", "{} isn't a folder.", shown(path))),
            },
            None => Err(missing(path)),
        }
    }

    /// A path that keeps naming what `path` names while items around it go: a twin (an item
    /// sharing its name, shown as `a (2).txt`) by its message, any other by `path` itself.
    pub fn pin(&self, path: &str) -> Result<String> {
        if self.in_chats(path) {
            return Ok(path.to_string());
        }
        match self.current(false)?.tree.find(path) {
            Some(Found::Item(i)) if i.twin => Ok(crate::tree::pinned(i.msg)),
            Some(_) => Ok(path.to_string()),
            None => Err(missing(path)),
        }
    }

    /// The items of folder `path` as the tree holds them now, with no request: what the app's
    /// tree shows at once. `None` before the tree is first read, if `path` isn't a folder, or
    /// while the tree is being brought up to date (which may wait on Telegram): this never waits.
    pub fn kept(&self, path: &str) -> Option<Vec<Item>> {
        let mut st = match self.state.try_lock() {
            Ok(st) => st,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        if !st.loaded {
            return None;
        }
        if let Some(segs) = self.chats_part(&mut st.tree, path) {
            drop(st);
            return match self.chats.try_lock() {
                Ok(mut c) => c.kept_list(&segs),
                Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner().kept_list(&segs),
                Err(std::sync::TryLockError::WouldBlock) => None,
            };
        }
        let dir = st.tree.find(path)?.dir()?.to_string();
        let items = st.tree.list(&dir);
        Some(self.with_chats(&mut st.tree, path, items))
    }

    /// What `path` is, `None` if nothing.
    pub fn stat(&self, path: &str) -> Result<Option<Found>> {
        if let Some(segs) = self.chat_segs(path)? {
            let Some((last, parent)) = segs.split_last() else { return Ok(Some(Found::Item(chats::folder_item(Account::chats_name(&mut self.current(false)?.tree))))) };
            let low = last.to_lowercase();
            return Ok(self.chats_list(parent, false)?.into_iter().find(|i| i.name.to_lowercase() == low).map(Found::Item));
        }
        Ok(self.current(false)?.tree.find(path))
    }

    /// The folder `path`'s parent is, and the name `path` ends in, checked.
    fn place(&self, st: &mut State, path: &str) -> Result<(String, String)> {
        if self.chats_part(&mut st.tree, path).is_some() {
            return Err(chats::read_only());
        }
        let (parent, name) = split(path);
        if !records::valid_name(name) {
            return Err(bad_name(name));
        }
        let dir = st.tree.find(parent).ok_or_else(|| missing(parent))?;
        let dir = dir.dir().ok_or_else(|| tr!("{} não é uma pasta.", "{} isn't a folder.", shown(parent)))?.to_string();
        Ok((dir, name.to_string()))
    }

    pub fn mkdir(&self, path: &str) -> Result<()> {
        let _turn = self.making.lock().unwrap_or_else(|e| e.into_inner());
        let (dir, name) = {
            let mut st = self.current(false)?;
            let (dir, name) = self.place(&mut st, path)?;
            if st.tree.find(path).is_some() {
                return Err(taken(&name));
            }
            (dir, name)
        };
        let m = self.backend.send_text(&records::dir_text(&name, &records::new_id(), &dir))?;
        self.apply([m], &[]);
        Ok(())
    }

    /// Moves or renames `from` to `to`, which mustn't exist (but may differ from `from` in case
    /// only).
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.writable(from)?;
        let (item, dir, name) = {
            let mut st = self.current(false)?;
            let Some(Found::Item(item)) = st.tree.find(from) else {
                return Err(if st.tree.find(from).is_some() { t("A pasta do topo não pode ser renomeada.", "The top folder can't be renamed.").into() } else { missing(from) });
            };
            let (dir, name) = self.place(&mut st, to)?;
            if let Some(Found::Item(there)) = st.tree.find(to)
                && there.msg != item.msg
            {
                return Err(taken(&name));
            }
            if let Some(me) = &item.dir
                && st.tree.within(&dir, me)
            {
                return Err(t("Uma pasta não pode ir para dentro dela mesma.", "A folder can't go inside itself.").into());
            }
            (item, dir, name)
        };
        let texts = match &item.dir {
            Some(id) => vec![(item.msg, records::dir_text(&name, id, &dir))],
            None if !item.parts.is_empty() => {
                let group = item.group.clone().unwrap_or_else(records::new_id);
                self.part_texts(&item, &name, &dir, &group)
            }
            None if item.empty => vec![(item.msg, records::empty_text(&name, &dir, item.modified))],
            None => vec![(item.msg, records::file_text(&name, &dir, item.modified))],
        };
        for (msg, text) in texts {
            self.retag(msg, &text, item.is_dir() || item.empty)?;
        }
        Ok(())
    }

    /// The records of a file in parts under a new name, folder or group: each part keeps its
    /// number, so a file missing parts stays one missing them.
    fn part_texts(&self, item: &Item, name: &str, dir: &str, group: &str) -> Vec<(i32, String)> {
        let st = self.lock();
        item.parts
            .iter()
            .filter_map(|&msg| match records::parse(&st.tree.message(msg)?.text) {
                records::Record::Part { index, count, .. } => Some((msg, records::part_text(name, dir, item.modified, group, index, count))),
                _ => None,
            })
            .collect()
    }

    /// Gives message `msg` the record `text`: an edit, or when the message is too old to be
    /// edited, the same item sent again (`plain`: a text message, else its document copied); a
    /// folder keeps its id, so what's in it follows.
    fn retag(&self, msg: i32, text: &str, plain: bool) -> Result<()> {
        if let Some(m) = self.backend.edit(msg, text)? {
            self.apply([m], &[]);
            return Ok(());
        }
        let m = if plain { self.backend.send_text(text)? } else { self.backend.copy(msg, text)? };
        let sent = m.id;
        self.apply([m], &[]);
        self.backend.delete(&[msg]).map_err(|e| tr!("{e} (o item novo ficou na mensagem {sent})", "{e} (the new item is in message {sent})"))?;
        self.apply([], &[msg]);
        Ok(())
    }

    /// Deletes a file, or a folder: an empty one unless `everything`, else with all it holds.
    pub fn remove(&self, path: &str, everything: bool) -> Result<()> {
        self.writable(path)?;
        let ids = {
            let mut st = self.current(false)?;
            let Some(Found::Item(item)) = st.tree.find(path) else {
                return Err(if st.tree.find(path).is_some() { t("A pasta do topo não pode ser excluída.", "The top folder can't be deleted.").into() } else { missing(path) });
            };
            let mut ids = Vec::new();
            if let Some(dir) = &item.dir {
                let below = st.tree.below(dir);
                if !below.is_empty() && !everything {
                    return Err(tr!("A pasta \"{}\" não está vazia.", "The folder \"{}\" isn't empty.", item.name));
                }
                ids = below;
            }
            ids.extend(item.messages());
            ids
        };
        self.backend.delete(&ids)?;
        self.apply([], &ids);
        Ok(())
    }

    /// Sends `len` bytes of `src` to `path`, replacing the file there. `mtime` is the file's
    /// modified time (now if `None`).
    pub fn upload(&self, path: &str, src: &mut dyn Read, len: u64, mtime: Option<i64>, progress: Progress) -> Result<()> {
        let (dir, name, old) = self.upload_place(path)?;
        let mtime = mtime.unwrap_or_else(now);
        let part = self.part_size();
        if len > part {
            self.upload_parts(&dir, &name, src, len, part, mtime, progress)?;
        } else {
            let m = if len == 0 {
                self.backend.send_text(&records::empty_text(&name, &dir, mtime))?
            } else {
                self.backend.send_file(&records::file_text(&name, &dir, mtime), &name, src, len, progress)?
            };
            self.apply([m], &[]);
        }
        if !old.is_empty() {
            self.backend.delete(&old)?;
            self.apply([], &old);
        }
        Ok(())
    }

    /// The largest a part may be: the account's limit, unless set otherwise.
    fn part_size(&self) -> u64 {
        match self.part_size.load(Ordering::SeqCst) {
            0 => self.backend.max_file(),
            n => n,
        }
    }

    /// Splits files in parts of `bytes` from now on, instead of the account's limit: the live
    /// tests, which can't send gigabytes.
    pub fn set_part_size(&self, bytes: u64) {
        self.part_size.store(bytes, Ordering::SeqCst);
    }

    /// Sends a file too big for one message as `part`-sized parts (named `name.001`, `name.002`…
    /// in Telegram). One failing, or Cancelar, deletes those sent.
    #[allow(clippy::too_many_arguments)]
    fn upload_parts(&self, dir: &str, name: &str, src: &mut dyn Read, len: u64, part: u64, mtime: i64, progress: Progress) -> Result<()> {
        let count = len.div_ceil(part) as u32;
        let group = records::new_id();
        let mut sent: Vec<i32> = Vec::new();
        let mut done = 0u64;
        for index in 1..=count {
            let n = part.min(len - done);
            let caption = records::part_text(name, dir, mtime, &group, index, count);
            let result = self.backend.send_file(&caption, &format!("{name}.{index:03}"), &mut (&mut *src).take(n), n, &mut |b| progress(done + b));
            match result {
                Ok(m) => {
                    sent.push(m.id);
                    self.apply([m], &[]);
                    done += n;
                }
                Err(e) => {
                    if !sent.is_empty() && self.backend.delete(&sent).is_ok() {
                        self.apply([], &sent);
                    }
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// The folder and name `path` goes to, and the messages of the file it replaces.
    fn upload_place(&self, path: &str) -> Result<(String, String, Vec<i32>)> {
        let mut st = self.current(false)?;
        let (dir, name) = self.place(&mut st, path)?;
        let old = match st.tree.find(path) {
            Some(Found::Item(i)) if i.is_dir() => return Err(tr!("Já existe uma pasta chamada \"{name}\" aqui.", "There's already a folder named \"{name}\" here.")),
            Some(Found::Item(i)) => i.messages(),
            _ => Vec::new(),
        };
        Ok((dir, name, old))
    }

    /// Sends small files (each to its path, with its bytes and modified time) [`GROUP`] to a
    /// message group. One result per file, in order.
    pub fn upload_small(&self, files: Vec<(String, Vec<u8>, Option<i64>)>) -> Vec<Result<()>> {
        let mut results: Vec<Result<()>> = Vec::with_capacity(files.len());
        let mut batch: Vec<(usize, Small, Vec<i32>)> = Vec::new();
        let mut paths: HashSet<String> = HashSet::new();
        let flush = |batch: &mut Vec<(usize, Small, Vec<i32>)>, results: &mut Vec<Result<()>>| {
            if batch.is_empty() {
                return;
            }
            let (idx, smalls, olds): (Vec<usize>, Vec<Small>, Vec<Vec<i32>>) = batch.drain(..).fold((vec![], vec![], vec![]), |mut acc, (i, s, o)| {
                acc.0.push(i);
                acc.1.push(s);
                acc.2.push(o);
                acc
            });
            match self.backend.send_group(smalls) {
                Ok(sent) => {
                    self.apply(sent, &[]);
                    let olds: Vec<i32> = olds.into_iter().flatten().collect();
                    let gone = if olds.is_empty() { Ok(()) } else { self.backend.delete(&olds) };
                    if gone.is_ok() {
                        self.apply([], &olds);
                    }
                    for i in idx {
                        results[i] = gone.clone();
                    }
                }
                Err(e) => {
                    for i in idx {
                        results[i] = Err(e.clone());
                    }
                }
            }
        };
        for (i, (path, bytes, mtime)) in files.into_iter().enumerate() {
            results.push(Ok(()));
            if bytes.is_empty() {
                results[i] = self.upload(&path, &mut std::io::empty(), 0, mtime, &mut |_| true);
                continue;
            }
            // A path twice in one group would make two files of it: the first goes before the
            // second is placed, so the second replaces it.
            let key = path.trim_matches('/').to_lowercase();
            if !paths.insert(key.clone()) {
                flush(&mut batch, &mut results);
                paths.clear();
                paths.insert(key);
            }
            match self.upload_place(&path) {
                Err(e) => results[i] = Err(e),
                Ok((dir, name, old)) => {
                    let caption = records::file_text(&name, &dir, mtime.unwrap_or_else(now));
                    batch.push((i, Small { caption, name, bytes }, old));
                    if batch.len() == GROUP {
                        flush(&mut batch, &mut results);
                        paths.clear();
                    }
                }
            }
        }
        flush(&mut batch, &mut results);
        results
    }

    /// Writes file `path` to `out`.
    pub fn download(&self, path: &str, out: &mut dyn Write, progress: Progress) -> Result<()> {
        if let Some(segs) = self.chat_segs(path)? {
            let (chat, msg) = self.chat_file(&segs)?;
            return self.backend.chat_download(&chat, msg, out, progress);
        }
        let parts: Vec<(i32, u64)> = {
            let mut st = self.current(false)?;
            match st.tree.find(path) {
                Some(Found::Item(i)) if i.empty => return Ok(()),
                Some(Found::Item(i)) if i.incomplete => {
                    return Err(tr!("Faltam partes de \"{}\" no Telegram: o arquivo não pode ser lido.", "Parts of \"{}\" are missing on Telegram: the file can't be read.", i.name));
                }
                Some(Found::Item(i)) if !i.is_dir() => i.messages().into_iter().map(|m| (m, st.tree.message(m).and_then(|m| m.doc.as_ref()).map_or(0, |d| d.size))).collect(),
                Some(_) => return Err(tr!("{} é uma pasta.", "{} is a folder.", shown(path))),
                None => return Err(missing(path)),
            }
        };
        let mut base = 0;
        for (msg, size) in parts {
            self.backend.download(msg, out, &mut |b| progress(base + b))?;
            base += size;
        }
        Ok(())
    }

    /// Copies `from` to `to` on Telegram's side (a folder with all it holds); `to` mustn't
    /// exist.
    /// A file of the chats isn't copied here: the app copies it through its bytes
    /// ([`Account::in_chats`]).
    pub fn copy(&self, from: &str, to: &str) -> Result<()> {
        self.writable(from)?;
        let (item, dir, name) = {
            let mut st = self.current(false)?;
            let Some(Found::Item(item)) = st.tree.find(from) else { return Err(missing(from)) };
            let (dir, name) = self.place(&mut st, to)?;
            if st.tree.find(to).is_some() {
                return Err(taken(&name));
            }
            if let Some(me) = &item.dir
                && st.tree.within(&dir, me)
            {
                return Err(t("Uma pasta não pode ser copiada para dentro dela mesma.", "A folder can't be copied into itself.").into());
            }
            (item, dir, name)
        };
        self.copy_item(&item, &dir, &name)
    }

    fn copy_item(&self, item: &Item, dir: &str, name: &str) -> Result<()> {
        match &item.dir {
            None if item.empty => {
                let m = self.backend.send_text(&records::empty_text(name, dir, item.modified))?;
                self.apply([m], &[]);
            }
            None if !item.parts.is_empty() => {
                let mut made = Vec::new();
                for (msg, text) in self.part_texts(item, name, dir, &records::new_id()) {
                    match self.backend.copy(msg, &text) {
                        Ok(m) => {
                            made.push(m.id);
                            self.apply([m], &[]);
                        }
                        Err(e) => {
                            if self.backend.delete(&made).is_ok() {
                                self.apply([], &made);
                            }
                            return Err(e);
                        }
                    }
                }
            }
            None => {
                let m = self.backend.copy(item.msg, &records::file_text(name, dir, item.modified))?;
                self.apply([m], &[]);
            }
            Some(id) => {
                let new = records::new_id();
                let m = self.backend.send_text(&records::dir_text(name, &new, dir))?;
                self.apply([m], &[]);
                let inside = self.lock().tree.list(id);
                for child in inside {
                    self.copy_item(&child, &new, &child.name)?;
                }
            }
        }
        Ok(())
    }

    /// The largest file this account can send.
    pub fn max_file(&self) -> u64 {
        self.backend.max_file()
    }

    /// Until when Telegram asked to wait, if it's asking now.
    pub fn waiting_until(&self) -> Option<i64> {
        self.backend.waiting_until()
    }

    /// Ends the session on Telegram's side: the account was removed.
    pub fn sign_out(&self) {
        self.backend.sign_out();
    }

    /// Sets how many connections per DC carry file parts and how many parts may be in flight:
    /// the benchmarks, to compare.
    pub fn tune(&self, tuning: crate::lanes::Tuning) {
        self.backend.tune(tuning);
    }

    /// Per DC, how many connections carry file parts now.
    pub fn connections_open(&self) -> Vec<(i32, usize)> {
        self.backend.connections_open()
    }

    /// Stops saving the session to the credential store: the account is being removed.
    pub fn detach(&self) {
        self.backend.detach();
    }

    /// Keeps the tree on disk if it changed since last kept.
    pub fn save_cache_if_changed(&self) {
        let Some(k) = &self.kept else { return };
        let mut st = self.lock();
        let mut chats = self.chats();
        if !(st.dirty || chats.dirty) || !st.loaded {
            return;
        }
        if cache::save(&k.file, &k.secret, st.pts, st.tree.messages(), chats.kept(), &self.forgotten, self.mark) {
            st.dirty = false;
            chats.dirty = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::fake::Fake;
    use crate::records::ROOT;

    /// An account over a fake channel the test keeps a handle on.
    struct Shared(Arc<Fake>);

    impl Backend for Shared {
        fn history(&self) -> Result<(Vec<Msg>, i32)> {
            self.0.history()
        }
        fn changes(&self, pts: i32) -> Result<Changes> {
            self.0.changes(pts)
        }
        fn send_text(&self, text: &str) -> Result<Msg> {
            self.0.send_text(text)
        }
        fn edit(&self, id: i32, text: &str) -> Result<Option<Msg>> {
            self.0.edit(id, text)
        }
        fn send_file(&self, caption: &str, name: &str, src: &mut dyn Read, len: u64, progress: Progress) -> Result<Msg> {
            self.0.send_file(caption, name, src, len, progress)
        }
        fn send_group(&self, files: Vec<Small>) -> Result<Vec<Msg>> {
            self.0.send_group(files)
        }
        fn copy(&self, id: i32, caption: &str) -> Result<Msg> {
            self.0.copy(id, caption)
        }
        fn delete(&self, ids: &[i32]) -> Result<()> {
            self.0.delete(ids)
        }
        fn download(&self, id: i32, out: &mut dyn Write, progress: Progress) -> Result<()> {
            self.0.download(id, out, progress)
        }
        fn max_file(&self) -> u64 {
            self.0.max_file()
        }
    }

    fn account() -> (Account, Arc<Fake>) {
        let fake = Arc::new(Fake::new());
        (Account::open(1, "Ana", Box::new(Shared(fake.clone())), None), fake)
    }

    fn names(a: &Account, path: &str) -> Vec<String> {
        a.list(path).unwrap().into_iter().map(|i| i.name).collect()
    }

    fn put(a: &Account, path: &str, bytes: &[u8]) {
        a.upload(path, &mut &bytes[..], bytes.len() as u64, Some(42), &mut |_| true).unwrap();
    }

    fn get(a: &Account, path: &str) -> Vec<u8> {
        let mut out = Vec::new();
        a.download(path, &mut out, &mut |_| true).unwrap();
        out
    }

    fn stale(a: &Account) {
        a.lock().checked = None;
    }

    #[test]
    fn folders_files_and_paths() {
        let (a, _) = account();
        assert!(names(&a, "/").is_empty());
        a.mkdir("/Fotos").unwrap();
        a.mkdir("/Fotos/2026").unwrap();
        put(&a, "/Fotos/2026/praia.jpg", b"areia");
        assert_eq!(names(&a, "/Fotos"), ["2026"]);
        assert_eq!(get(&a, "/fotos/2026/PRAIA.jpg"), b"areia");
        let Some(Found::Item(f)) = a.stat("/Fotos/2026/praia.jpg").unwrap() else { panic!() };
        assert_eq!((f.size, f.modified), (5, 42));
        assert!(a.mkdir("/fotos").unwrap_err().contains("Já existe"));
        assert!(a.mkdir("/Nada/x").unwrap_err().contains("não existe"));
        assert!(a.mkdir("/a:b").unwrap_err().contains("não pode ser nome"));
        assert!(a.list("/Fotos/2026/praia.jpg").unwrap_err().contains("não é uma pasta"));
    }

    #[test]
    fn rename_and_move_edit_one_message_or_resend_when_too_old() {
        let (a, fake) = account();
        a.mkdir("/A").unwrap();
        a.mkdir("/B").unwrap();
        put(&a, "/A/x.txt", b"x");
        a.rename("/A/x.txt", "/B/y.txt").unwrap();
        assert!(names(&a, "/A").is_empty());
        assert_eq!(names(&a, "/B"), ["y.txt"]);
        a.rename("/B/y.txt", "/B/Y.txt").unwrap();
        assert_eq!(names(&a, "/B"), ["Y.txt"]);
        assert!(a.rename("/B", "/B/dentro").unwrap_err().contains("dentro dela mesma"));
        put(&a, "/B/z.txt", b"z");
        assert!(a.rename("/B/z.txt", "/B/y.txt").unwrap_err().contains("Já existe"));
        let edits = fake.edits.load(Ordering::SeqCst);
        // Too old to edit: the file is copied with its new record and the old message goes; a
        // folder is sent again under the same id and what's in it follows.
        fake.edits_expire.store(true, Ordering::SeqCst);
        a.rename("/B/z.txt", "/A/z2.txt").unwrap();
        a.rename("/B", "/A/B").unwrap();
        stale(&a);
        assert_eq!(names(&a, "/"), ["A"]);
        assert_eq!(names(&a, "/A"), ["B", "z2.txt"]);
        assert_eq!(names(&a, "/A/B"), ["Y.txt"]);
        assert_eq!(get(&a, "/A/z2.txt"), b"z");
        assert_eq!(fake.edits.load(Ordering::SeqCst), edits + 2);
        assert_eq!(fake.messages().len(), 4);
    }

    #[test]
    fn copies_happen_on_telegrams_side() {
        let (a, fake) = account();
        a.mkdir("/A").unwrap();
        a.mkdir("/A/sub").unwrap();
        put(&a, "/A/sub/f.bin", b"conteudo");
        put(&a, "/A/g.bin", b"g");
        a.copy("/A", "/C").unwrap();
        assert_eq!(names(&a, "/C"), ["sub", "g.bin"]);
        assert_eq!(get(&a, "/C/sub/f.bin"), b"conteudo");
        assert_eq!(fake.copies.load(Ordering::SeqCst), 2);
        assert!(a.copy("/A", "/A/sub/A").unwrap_err().contains("dentro dela mesma"));
        assert!(a.copy("/A/g.bin", "/C/g.bin").unwrap_err().contains("Já existe"));
        // The original is untouched.
        assert_eq!(names(&a, "/A"), ["sub", "g.bin"]);
    }

    #[test]
    fn delete_empty_folder_or_everything_in_it() {
        let (a, fake) = account();
        a.mkdir("/A").unwrap();
        a.mkdir("/A/sub").unwrap();
        put(&a, "/A/sub/f.bin", b"f");
        assert!(a.remove("/A", false).unwrap_err().contains("não está vazia"));
        a.remove("/A/sub/f.bin", false).unwrap();
        a.remove("/A/sub", false).unwrap();
        put(&a, "/A/g.bin", b"g");
        a.remove("/A", true).unwrap();
        assert!(names(&a, "/").is_empty());
        assert!(fake.messages().is_empty());
        assert!(a.remove("/", true).unwrap_err().contains("topo"));
    }

    #[test]
    fn an_upload_replaces_the_file_and_refuses_what_is_too_big() {
        let (a, fake) = account();
        put(&a, "/a.txt", b"um");
        put(&a, "/A.TXT", b"dois");
        assert_eq!(names(&a, "/"), ["A.TXT"]);
        assert_eq!(get(&a, "/a.txt"), b"dois");
        assert_eq!(fake.messages().len(), 1);
        // Past the limit, in parts: one file still.
        fake.max.store(3, Ordering::SeqCst);
        put(&a, "/b.txt", b"quatro");
        assert_eq!(get(&a, "/b.txt"), b"quatro");
        assert_eq!(fake.messages().len(), 3);
        a.mkdir("/p").unwrap();
        assert!(a.upload("/p", &mut &b"x"[..], 1, None, &mut |_| true).unwrap_err().contains("pasta"));
        let e = a.upload("/c.txt", &mut &b"ab"[..], 2, None, &mut |_| false).unwrap_err();
        assert_eq!(e, crate::CANCELLED);
    }

    #[test]
    fn a_file_past_the_limit_goes_in_parts_and_works_as_one() {
        let (a, fake) = account();
        fake.max.store(4, Ordering::SeqCst);
        a.mkdir("/A").unwrap();
        a.mkdir("/B").unwrap();
        let mut seen = Vec::new();
        a.upload("/A/grande.bin", &mut &b"0123456789"[..], 10, Some(77), &mut |n| {
            seen.push(n);
            true
        })
        .unwrap();
        assert_eq!(seen.last(), Some(&10));
        assert!(seen.windows(2).all(|w| w[0] <= w[1]), "progress goes onward across parts: {seen:?}");
        let Some(Found::Item(i)) = a.stat("/A/grande.bin").unwrap() else { panic!() };
        assert_eq!((i.size, i.modified, i.parts.len(), i.incomplete), (10, 77, 3, false));
        assert_eq!(names(&a, "/A"), ["grande.bin"]);
        assert_eq!(get(&a, "/A/grande.bin"), b"0123456789");
        // Moved and renamed: each part's record changes, the file stays whole.
        a.rename("/A/grande.bin", "/B/g.bin").unwrap();
        assert_eq!(get(&a, "/B/g.bin"), b"0123456789");
        fake.edits_expire.store(true, Ordering::SeqCst);
        a.rename("/B/g.bin", "/B/h.bin").unwrap();
        fake.edits_expire.store(false, Ordering::SeqCst);
        assert_eq!(get(&a, "/B/h.bin"), b"0123456789");
        // Copied on Telegram's side, in parts too; the copy is a file of its own.
        a.copy("/B", "/C").unwrap();
        assert_eq!(get(&a, "/C/h.bin"), b"0123456789");
        a.remove("/B/h.bin", false).unwrap();
        assert_eq!(get(&a, "/C/h.bin"), b"0123456789");
        // Replaced by a small one: every part goes.
        put(&a, "/C/h.bin", b"oi");
        assert_eq!(fake.messages().iter().filter(|m| m.doc.is_some()).count(), 1);
        // A part deleted elsewhere: the file shows incomplete and can't be read, but goes whole.
        a.upload("/C/x.bin", &mut &b"abcdefghij"[..], 10, None, &mut |_| true).unwrap();
        let Some(Found::Item(x)) = a.stat("/C/x.bin").unwrap() else { panic!() };
        fake.delete_elsewhere(&[x.parts[1]]);
        stale(&a);
        assert_eq!(names(&a, "/C"), ["h.bin", "x.bin (incompleto)"]);
        assert!(a.download("/C/x.bin (incompleto)", &mut Vec::new(), &mut |_| true).unwrap_err().contains("Faltam partes"));
        a.remove("/C", true).unwrap();
        assert_eq!(fake.messages().iter().filter(|m| m.doc.is_some()).count(), 0);
        // Cancelar midway: the parts sent go too.
        let e = a.upload("/A/c.bin", &mut &b"0123456789"[..], 10, None, &mut |n| n < 6).unwrap_err();
        assert_eq!(e, crate::CANCELLED);
        assert_eq!(fake.messages().iter().filter(|m| m.doc.is_some()).count(), 0);
        assert!(names(&a, "/A").is_empty());
    }

    #[test]
    fn empty_files_live_in_text_messages() {
        let (a, fake) = account();
        a.mkdir("/p").unwrap();
        put(&a, "/p/vazio.txt", b"");
        let r = a.upload_small(vec![("/p/outro.txt".into(), Vec::new(), Some(1)), ("/p/cheio.txt".into(), b"x".to_vec(), Some(1))]);
        assert!(r.iter().all(Result::is_ok), "{r:?}");
        assert_eq!(names(&a, "/p"), ["cheio.txt", "outro.txt", "vazio.txt"]);
        assert!(get(&a, "/p/vazio.txt").is_empty());
        a.rename("/p/vazio.txt", "/vazio2.txt").unwrap();
        a.copy("/vazio2.txt", "/p/copia.txt").unwrap();
        fake.edits_expire.store(true, Ordering::SeqCst);
        a.rename("/p/outro.txt", "/outro.txt").unwrap();
        assert_eq!(names(&a, "/"), ["p", "outro.txt", "vazio2.txt"]);
        assert_eq!(names(&a, "/p"), ["cheio.txt", "copia.txt"]);
        assert_eq!(fake.copies.load(Ordering::SeqCst), 0);
        assert_eq!(fake.messages().iter().filter(|m| m.doc.is_none()).count(), 4);
    }

    #[test]
    fn small_files_go_ten_to_a_group() {
        let (a, fake) = account();
        a.mkdir("/p").unwrap();
        put(&a, "/p/f03.txt", b"velho");
        let mut files: Vec<(String, Vec<u8>, Option<i64>)> = (0..23).map(|i| (format!("/p/f{i:02}.txt"), vec![i as u8], Some(9))).collect();
        files.push(("/P/F21.txt".into(), b"repetido".to_vec(), None));
        files.push(("/nada/x.txt".into(), b"x".to_vec(), None));
        let results = a.upload_small(files);
        assert!(results[..24].iter().all(Result::is_ok), "{results:?}");
        assert!(results[24].as_ref().unwrap_err().contains("não existe"));
        // 10 + 10 + 3, then the repeated path in a group of its own, replacing the first.
        assert_eq!(fake.groups.load(Ordering::SeqCst), 4);
        assert_eq!(a.list("/p").unwrap().len(), 23);
        assert_eq!(get(&a, "/p/f03.txt"), [3]);
        assert_eq!(get(&a, "/p/f21.txt"), b"repetido");
        assert_eq!(fake.messages().len(), 24);
    }

    #[test]
    fn twins_are_pinned_by_their_message() {
        let (a, fake) = account();
        let one = fake.post(&records::file_text("a.txt", ROOT, 1), Some(("a.txt", b"um")));
        let two = fake.post(&records::file_text("a.txt", ROOT, 1), Some(("a.txt", b"dois")));
        put(&a, "/b.txt", b"b");
        assert_eq!(names(&a, "/"), ["a (2).txt", "a.txt", "b.txt"]);
        // A batch pins its items before any goes: deleting the first shifts the other's name.
        let pins: Vec<String> = ["/a.txt", "/a (2).txt", "/b.txt"].iter().map(|p| a.pin(p).unwrap()).collect();
        assert_eq!(pins, [crate::tree::pinned(one.id), crate::tree::pinned(two.id), "/b.txt".to_string()]);
        a.remove(&pins[0], false).unwrap();
        assert_eq!(names(&a, "/"), ["a.txt", "b.txt"]);
        assert_eq!(get(&a, &pins[1]), b"dois");
        a.rename(&pins[1], "/c.txt").unwrap();
        assert_eq!(names(&a, "/"), ["b.txt", "c.txt"]);
        assert!(a.pin("/nada").is_err());
    }

    #[test]
    fn changes_made_elsewhere_arrive_through_the_log() {
        let (a, fake) = account();
        a.mkdir("/A").unwrap();
        let histories = fake.histories.load(Ordering::SeqCst);
        let Some(Found::Item(dir)) = a.stat("/A").unwrap() else { panic!() };
        let id = dir.dir.clone().unwrap();
        fake.post(&records::file_text("do celular.jpg", &id, 5), Some(("IMG_1.jpg", b"foto")));
        fake.post("encaminhado", Some(("doc.pdf", b"pdf")));
        fake.edit_elsewhere(dir.msg, &records::dir_text("Renomeada", &id, ROOT));
        stale(&a);
        assert_eq!(names(&a, "/"), ["Renomeada", "doc.pdf"]);
        assert_eq!(names(&a, "/Renomeada"), ["do celular.jpg"]);
        fake.delete_elsewhere(&[dir.msg]);
        assert!(a.refresh().unwrap(), "a change made elsewhere is news");
        assert!(!a.refresh().unwrap(), "asked again, nothing new");
        stale(&a);
        // Its folder gone, the file shows at the top.
        assert_eq!(names(&a, "/"), ["do celular.jpg", "doc.pdf"]);
        assert_eq!(fake.histories.load(Ordering::SeqCst), histories);
        // A log Telegram no longer has: the whole history is read again.
        fake.drop_log();
        a.lock().pts = 0;
        stale(&a);
        assert_eq!(names(&a, "/"), ["do celular.jpg", "doc.pdf"]);
        assert_eq!(fake.histories.load(Ordering::SeqCst), histories + 1);
    }

    #[test]
    fn the_tree_comes_back_from_disk_and_only_changes_are_asked() {
        let dir = std::env::temp_dir().join(format!("oe-telegram-cache-{}", std::process::id()));
        let kept = || Kept { file: dir.join("conta.tree"), secret: cache::secret(1, &[7; 256]) };
        let fake = Arc::new(Fake::new());
        let a = Account::open(1, "Ana", Box::new(Shared(fake.clone())), Some(kept()));
        a.mkdir("/A").unwrap();
        a.save_cache_if_changed();
        drop(a);
        fake.post(&records::dir_text("B", &records::new_id(), ROOT), None);
        let histories = fake.histories.load(Ordering::SeqCst);
        let a = Account::open(1, "Ana", Box::new(Shared(fake.clone())), Some(kept()));
        assert_eq!(names(&a, "/"), ["A", "B"]);
        assert_eq!(fake.histories.load(Ordering::SeqCst), histories);
        // Another key (a new login) can't open it: the channel is read.
        let a = Account::open(1, "Ana", Box::new(Shared(fake.clone())), Some(Kept { file: dir.join("conta.tree"), secret: cache::secret(1, &[8; 256]) }));
        assert_eq!(names(&a, "/"), ["A", "B"]);
        assert_eq!(fake.histories.load(Ordering::SeqCst), histories + 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    // -----------------------------------------------------------------------------------------
    // The chats

    use crate::chats::{Kind, Month};

    fn with_chats() -> (Account, Arc<Fake>) {
        let fake = Arc::new(Fake::new());
        (Account::open(1, "Ana", Box::new(fake.clone()), None), fake)
    }

    fn day(y: i32, m: u32, d: i64) -> i64 {
        Month { year: y, month: m }.range().0 + (d - 1) * 86400 + 12 * 3600
    }

    #[test]
    fn the_chats_show_as_a_folder_of_months_of_files_beside_the_storage() {
        let (a, fake) = with_chats();
        put(&a, "/notas.txt", b"minhas");
        let saved = fake.add_chat(Kind::Saved, 1, "Ana");
        let bia = fake.add_chat(Kind::User, 20, "Bia");
        fake.add_chat_file(saved, "contrato.pdf", b"pdf", day(2026, 11, 3));
        fake.add_chat_file(saved, "", b"jpg", day(2026, 10, 9));
        fake.add_chat_file(bia, "contrato.pdf", b"outro", day(2026, 11, 4));
        fake.add_chat_file(bia, "contrato.pdf", b"mais um", day(2026, 11, 5));

        assert_eq!(names(&a, "/"), ["Conversas do Telegram", "notas.txt"]);
        assert_eq!(names(&a, "/Conversas do Telegram"), ["Mensagens Salvas", "Bia"]);
        assert_eq!(names(&a, "/Conversas do Telegram/Mensagens Salvas"), ["2026-11", "2026-10"]);
        assert_eq!(names(&a, "/Conversas do Telegram/Mensagens Salvas/2026-11"), ["contrato.pdf"]);
        assert_eq!(names(&a, "/telegram chats/bia/2026-11"), ["contrato.pdf", "contrato (2).pdf"]);

        let mut out = Vec::new();
        a.download("/Conversas do Telegram/Bia/2026-11/contrato (2).pdf", &mut out, &mut |_| true).unwrap();
        assert_eq!(out, b"mais um");
        let found = a.stat("/Conversas do Telegram/Bia/2026-11/contrato.pdf").unwrap();
        assert!(matches!(found, Some(Found::Item(i)) if i.size == 5 && !i.is_dir()));
        assert!(matches!(a.stat("/Conversas do Telegram").unwrap(), Some(Found::Item(i)) if i.is_dir()));
        assert!(a.stat("/Conversas do Telegram/Ninguém").unwrap().is_none());
        // What was listed shows again with no request.
        let searches = fake.chat_searches.load(Ordering::SeqCst);
        assert_eq!(a.kept("/Conversas do Telegram/Bia/2026-11").map(|v| v.len()), Some(2));
        assert_eq!(a.kept("/Conversas do Telegram/Bia/2026-10"), None);
        assert_eq!(fake.chat_searches.load(Ordering::SeqCst), searches);
    }

    #[test]
    fn nothing_in_the_chats_changes_and_no_storage_item_takes_their_name() {
        let (a, fake) = with_chats();
        let bia = fake.add_chat(Kind::User, 20, "Bia");
        fake.add_chat_file(bia, "a.txt", b"a", day(2026, 11, 1));
        put(&a, "/b.txt", b"b");
        let ro = crate::chats::read_only();
        let file = "/Conversas do Telegram/Bia/2026-11/a.txt";
        assert_eq!(a.mkdir("/Conversas do Telegram/Bia/nova"), Err(ro.clone()));
        assert_eq!(a.mkdir("/conversas do telegram"), Err(ro.clone()));
        assert_eq!(a.remove(file, false), Err(ro.clone()));
        assert_eq!(a.rename(file, "/a.txt"), Err(ro.clone()));
        assert_eq!(a.rename("/b.txt", "/Conversas do Telegram/b.txt"), Err(ro.clone()));
        assert_eq!(a.copy(file, "/a.txt"), Err(ro.clone()));
        assert_eq!(a.upload("/Conversas do Telegram/Bia/2026-11/c.txt", &mut &b"c"[..], 1, None, &mut |_| true), Err(ro.clone()));
        assert!(a.in_chats(file) && !a.in_chats("/b.txt"));
        assert_eq!(names(&a, "/"), ["Conversas do Telegram", "b.txt"]);
    }

    #[test]
    fn a_storage_folder_named_as_the_chats_keeps_its_name_and_the_chats_show_numbered() {
        let (a, fake) = with_chats();
        // Made before the chats showed (or by another app).
        a.mkdir("/Conversas do Telegram").unwrap();
        put(&a, "/Conversas do Telegram/backup.txt", b"meu");
        let bia = fake.add_chat(Kind::User, 20, "Bia");
        fake.add_chat_file(bia, "a.txt", b"a", day(2026, 11, 1));

        assert_eq!(names(&a, "/"), ["Conversas do Telegram (2)", "Conversas do Telegram"]);
        assert_eq!(names(&a, "/Conversas do Telegram"), ["backup.txt"]);
        assert_eq!(names(&a, "/Conversas do Telegram (2)"), ["Mensagens Salvas", "Bia"]);
        assert_eq!(names(&a, "/Telegram chats"), ["Mensagens Salvas", "Bia"]);
        assert!(matches!(a.stat("/Conversas do Telegram (2)").unwrap(), Some(Found::Item(i)) if i.name == "Conversas do Telegram (2)"));
        let mut out = Vec::new();
        a.download("/Conversas do Telegram/backup.txt", &mut out, &mut |_| true).unwrap();
        assert_eq!(out, b"meu");
        a.download("/Conversas do Telegram (2)/Bia/2026-11/a.txt", &mut out, &mut |_| true).unwrap();
        assert_eq!(out, b"meua");
        // The storage's folder is the person's: it changes as any other.
        put(&a, "/Conversas do Telegram/outro.txt", b"x");
        a.rename("/Conversas do Telegram/outro.txt", "/Conversas do Telegram/novo.txt").unwrap();
        a.remove("/Conversas do Telegram/novo.txt", false).unwrap();
        assert!(a.in_chats("/Conversas do Telegram (2)/Bia") && !a.in_chats("/Conversas do Telegram/backup.txt"));
        assert!(a.passed_over("/Conversas do Telegram (2)") && !a.passed_over("/Conversas do Telegram"));
        assert_eq!(a.mkdir("/Conversas do Telegram (2)/nova"), Err(crate::chats::read_only()));
        assert_eq!(a.mkdir("/telegram chats"), Err(crate::chats::read_only()));
        // Gone, it gives the chats their name back.
        a.remove("/Conversas do Telegram", true).unwrap();
        assert_eq!(names(&a, "/"), ["Conversas do Telegram"]);
        assert_eq!(names(&a, "/Conversas do Telegram"), ["Mensagens Salvas", "Bia"]);
        assert!(a.stat("/Conversas do Telegram (2)").unwrap().is_none());
    }

    #[test]
    fn the_chats_come_a_page_at_a_time_and_the_rest_while_the_app_asks_what_changed() {
        let (a, fake) = with_chats();
        fake.page.store(2, Ordering::SeqCst);
        for i in 0..5 {
            fake.add_chat(Kind::Channel, 100 + i, &format!("Canal {i}"));
        }
        // Nothing is read before someone opens the chats.
        a.list("/").unwrap();
        assert!(!a.refresh().unwrap());
        assert_eq!(fake.chat_pages.load(Ordering::SeqCst), 0);
        assert_eq!(names(&a, "/Conversas do Telegram"), ["Mensagens Salvas", "Canal 0", "Canal 1"]);
        // Telegram asks to wait: the rest waits too, and comes after.
        fake.chats_wait.store(now() + 3600, Ordering::SeqCst);
        assert!(!a.refresh().unwrap());
        assert_eq!(a.kept("/Conversas do Telegram").unwrap().len(), 3);
        a.chats().not_before = 0;
        assert!(a.refresh().unwrap());
        assert_eq!(a.kept("/Conversas do Telegram").unwrap().len(), 6);
        assert!(!a.refresh().unwrap(), "all read: nothing more until the most recent are read again");
    }

    #[test]
    fn a_name_keeps_leading_to_its_chat_when_a_twin_comes_later() {
        let (a, fake) = with_chats();
        fake.page.store(2, Ordering::SeqCst);
        let ana = fake.add_chat(Kind::User, 50, "Ana");
        fake.add_chat(Kind::User, 60, "Bia");
        let other = fake.add_chat(Kind::User, 10, "Ana");
        fake.add_chat_file(ana, "a.txt", b"da primeira", day(2026, 11, 1));
        fake.add_chat_file(other, "a.txt", b"da outra", day(2026, 11, 1));
        let path = "/Conversas do Telegram/Ana/2026-11/a.txt";
        let get = |a: &Account| {
            let mut out = Vec::new();
            a.download(path, &mut out, &mut |_| true).unwrap();
            out
        };
        a.list("/").unwrap();
        assert_eq!(get(&a), b"da primeira");
        assert!(a.refresh().unwrap(), "the next page came");
        assert_eq!(names(&a, "/Conversas do Telegram"), ["Mensagens Salvas", "Ana", "Bia", "Ana (2)"]);
        assert_eq!(get(&a), b"da primeira");
        let mut out = Vec::new();
        a.download("/Conversas do Telegram/Ana (2)/2026-11/a.txt", &mut out, &mut |_| true).unwrap();
        assert_eq!(out, b"da outra");
    }

    #[test]
    fn the_chats_come_back_from_disk_before_the_storage_is_read() {
        let dir = std::env::temp_dir().join(format!("oe-telegram-chats-{}", std::process::id()));
        let kept = || Kept { file: dir.join("conta.tree"), secret: cache::secret(1, &[9; 256]) };
        let fake = Arc::new(Fake::new());
        fake.add_chat(Kind::User, 20, "Bia");
        fake.add_chat(Kind::Channel, 30, "Canal");
        let a = Account::open(1, "Ana", Box::new(fake.clone()), Some(kept()));
        a.list("/").unwrap();
        assert_eq!(names(&a, "/Conversas do Telegram"), ["Mensagens Salvas", "Bia", "Canal"]);
        a.save_cache_if_changed();
        drop(a);
        let pages = fake.chat_pages.load(Ordering::SeqCst);
        // A pane left in the chats opens there first, before anything reads the storage.
        let a = Account::open(1, "Ana", Box::new(fake.clone()), Some(kept()));
        assert_eq!(names(&a, "/Conversas do Telegram"), ["Mensagens Salvas", "Bia", "Canal"]);
        assert_eq!(fake.chat_pages.load(Ordering::SeqCst), pages, "no page read: they came from disk");
        // Kept again unchanged, not emptied.
        a.list("/").unwrap();
        a.save_cache_if_changed();
        drop(a);
        let a = Account::open(1, "Ana", Box::new(fake.clone()), Some(kept()));
        assert_eq!(names(&a, "/Conversas do Telegram").len(), 3);
        assert_eq!(fake.chat_pages.load(Ordering::SeqCst), pages);
        let _ = std::fs::remove_dir_all(dir);
    }
}
