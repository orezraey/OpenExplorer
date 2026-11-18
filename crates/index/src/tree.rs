//! One volume's files in memory, the way Everything keeps them: a node per MFT record (its
//! parent, size, date) and every name in two flat arenas, as written and folded (see
//! [`crate::fold`]). A search runs `memmem` over the folded arena, a few tens of megabytes for
//! millions of files, split across the cores; paths are only built for what's found.
//!
//! Names are only appended: a renamed or deleted file's old name stays in the arenas, marked
//! dead, until [`Tree::maybe_compact`] rebuilds them.
//!
//! Paths use the separator of the root: `C:\dir\file` on Windows, `/dir/file` on Linux, where
//! names are also matched exactly when a folder is looked up (two can differ only in case).

use std::io::{Read, Write};

use memchr::memmem;

use crate::fold;

/// The record of a volume's root directory, in NTFS and in [`crate::walk`].
pub const ROOT: u32 = 5;
const NONE: u32 = u32::MAX;
/// Deeper than this, a parent chain is taken for a loop (a tree being changed under us).
const MAX_DEPTH: usize = 512;

pub const DIR: u8 = 1;
pub const HIDDEN: u8 = 2;
pub const SYSTEM: u8 = 4;

#[derive(Clone, Copy)]
struct Node {
    parent: u32,
    /// Its name in `slots`, or [`NONE`] for a record that holds no file.
    slot: u32,
    size: u64,
    /// Seconds since 1970.
    modified: u32,
    flags: u8,
}

const EMPTY: Node = Node { parent: NONE, slot: NONE, size: 0, modified: 0, flags: 0 };

/// Where a name sits in the arenas; `record` is [`NONE`] once the name is dead.
#[derive(Clone, Copy)]
struct Slot {
    record: u32,
    name: u32,
    folded: u32,
    name_len: u16,
    folded_len: u16,
}

/// A file as a search hands it on.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: u32,
}

/// The words of a search, folded: each must be in the name.
#[derive(Clone, Debug)]
pub struct Query {
    words: Vec<Vec<u8>>,
}

impl Query {
    pub fn new(text: &str) -> Option<Query> {
        let mut words: Vec<Vec<u8>> = text.split_whitespace().map(fold::fold).filter(|w| !w.is_empty()).collect();
        words.sort();
        words.dedup();
        // The longest word is looked for first: it has the fewest hits to check.
        words.sort_by_key(|w| std::cmp::Reverse(w.len()));
        (!words.is_empty()).then_some(Query { words })
    }
}

pub struct Tree {
    /// "C:\" or "/", what paths start with.
    root: String,
    /// `\` or `/`, from the root.
    sep: char,
    nodes: Vec<Node>,
    slots: Vec<Slot>,
    names: Vec<u8>,
    /// Folded names, each with a 0 before and after it, so a word never matches across two
    /// names and `\0name\0` finds a name whole.
    folded: Vec<u8>,
    /// Arena bytes held by dead names.
    dead: usize,
    live: usize,
    /// Per record, when [`Tree::link`] asked for them: its first child, next and previous
    /// sibling. They find a file by its folder and name (inotify says what changed by name) and
    /// a folder's whole subtree. Empty otherwise: the MFT has no need of them.
    kids: Vec<[u32; 3]>,
    linked: bool,
}

impl Tree {
    pub fn new(root: &str) -> Tree {
        let sep = if root.starts_with('/') { '/' } else { '\\' };
        Tree { root: root.to_string(), sep, nodes: Vec::new(), slots: Vec::new(), names: Vec::new(), folded: vec![0], dead: 0, live: 0, kids: Vec::new(), linked: false }
    }

    /// Keeps each folder's children from now on (see `kids`).
    pub fn link(&mut self) {
        self.linked = true;
        self.kids = vec![[NONE; 3]; self.nodes.len().max(ROOT as usize + 1)];
        for r in 0..self.nodes.len() {
            let n = self.nodes[r];
            if n.slot != NONE {
                self.attach(r as u32, n.parent);
            }
        }
    }

    fn attach(&mut self, record: u32, parent: u32) {
        if parent == NONE {
            return;
        }
        let need = record.max(parent) as usize + 1;
        if self.kids.len() < need {
            self.kids.resize(need, [NONE; 3]);
        }
        let head = self.kids[parent as usize][0];
        self.kids[record as usize][1] = head;
        self.kids[record as usize][2] = NONE;
        if head != NONE {
            self.kids[head as usize][2] = record;
        }
        self.kids[parent as usize][0] = record;
    }

    fn detach(&mut self, record: u32, parent: u32) {
        let Some(&[_, next, prev]) = self.kids.get(record as usize) else { return };
        if prev != NONE {
            self.kids[prev as usize][1] = next;
        } else if let Some(p) = self.kids.get_mut(parent as usize)
            && p[0] == record
        {
            p[0] = next;
        }
        if next != NONE {
            self.kids[next as usize][2] = prev;
        }
        self.kids[record as usize][1] = NONE;
        self.kids[record as usize][2] = NONE;
    }

    /// The child of folder `parent` named `name` (exactly), once [`Tree::link`] was called.
    pub fn child(&self, parent: u32, name: &str) -> Option<u32> {
        let mut c = self.kids.get(parent as usize)?[0];
        while c != NONE {
            let n = self.nodes[c as usize];
            if n.slot != NONE && self.name_of(n.slot) == name {
                return Some(c);
            }
            c = self.kids[c as usize][1];
        }
        None
    }

    /// The children of `parent`, once [`Tree::link`] was called.
    pub fn children(&self, parent: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut c = self.kids.get(parent as usize).map_or(NONE, |k| k[0]);
        while c != NONE {
            out.push(c);
            c = self.kids[c as usize][1];
        }
        out
    }

    /// Removes `record` and, for a folder, everything under it (with [`Tree::link`]); returns
    /// the records removed, for whoever keeps more about them.
    pub fn remove_tree(&mut self, record: u32) -> Vec<u32> {
        if !self.contains(record) {
            return Vec::new();
        }
        let parent = self.nodes[record as usize].parent;
        if self.linked {
            self.detach(record, parent);
        }
        let mut gone = Vec::new();
        let mut stack = vec![record];
        while let Some(r) = stack.pop() {
            if self.linked {
                let mut c = self.kids[r as usize][0];
                while c != NONE {
                    stack.push(c);
                    c = self.kids[c as usize][1];
                }
                self.kids[r as usize] = [NONE; 3];
            }
            let slot = std::mem::replace(&mut self.nodes[r as usize], EMPTY).slot;
            if slot != NONE {
                self.kill(slot);
                self.live -= 1;
                gone.push(r);
            }
        }
        gone
    }

    /// Whether `record` is a folder: the root, or one the tree holds.
    pub fn is_dir(&self, record: u32) -> bool {
        record == ROOT || self.nodes.get(record as usize).is_some_and(|n| n.slot != NONE && n.flags & DIR != 0)
    }

    /// Records the nodes have room for: one past the highest.
    pub fn records(&self) -> usize {
        self.nodes.len()
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    /// Files and folders held.
    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Bytes the tree takes in memory.
    pub fn memory(&self) -> usize {
        self.nodes.capacity() * size_of::<Node>() + self.kids.capacity() * size_of::<[u32; 3]>() + self.slots.capacity() * size_of::<Slot>() + self.names.capacity() + self.folded.capacity()
    }

    /// Room for records up to `records` and about `names` bytes of names, before a bulk load.
    pub fn reserve(&mut self, records: usize, names: usize) {
        self.nodes.reserve(records.saturating_sub(self.nodes.len()));
        self.slots.reserve(records);
        self.names.reserve(names);
        self.folded.reserve(names + records);
    }

    /// Gives back what a bulk load reserved and didn't use.
    pub fn shrink(&mut self) {
        self.nodes.shrink_to_fit();
        self.kids.shrink_to_fit();
        self.slots.shrink_to_fit();
        self.names.shrink_to_fit();
        self.folded.shrink_to_fit();
    }

    /// Adds `record`, or updates it: a new name or parent, a new size or date.
    pub fn set(&mut self, record: u32, parent: u32, name: &str, flags: u8, size: u64, modified: u32) {
        if record == NONE || name.is_empty() || name.len() > u16::MAX as usize {
            return;
        }
        let r = record as usize;
        if r >= self.nodes.len() {
            self.nodes.resize(r + 1, EMPTY);
        }
        let old = self.nodes[r].slot;
        let old_parent = self.nodes[r].parent;
        let slot = if old != NONE && self.name_of(old) == name {
            old
        } else {
            if old == NONE {
                self.live += 1;
            } else {
                self.kill(old);
            }
            self.push(record, name)
        };
        self.nodes[r] = Node { parent, slot, size, modified, flags };
        if self.linked && (old == NONE || old_parent != parent) {
            if old != NONE {
                self.detach(record, old_parent);
            }
            self.attach(record, parent);
        }
    }

    /// Updates only the size and date of `record`, if it's held.
    pub fn touch(&mut self, record: u32, size: u64, modified: u32) {
        if let Some(n) = self.nodes.get_mut(record as usize)
            && n.slot != NONE
        {
            n.size = size;
            n.modified = modified;
        }
    }

    pub fn remove(&mut self, record: u32) {
        let Some(n) = self.nodes.get_mut(record as usize) else { return };
        let old = std::mem::replace(n, EMPTY);
        if old.slot != NONE {
            self.kill(old.slot);
            self.live -= 1;
            if self.linked {
                self.detach(record, old.parent);
            }
        }
    }

    /// The size and date held for `record`.
    pub fn meta(&self, record: u32) -> Option<(u64, u32)> {
        let n = self.nodes.get(record as usize)?;
        (n.slot != NONE).then_some((n.size, n.modified))
    }

    pub fn contains(&self, record: u32) -> bool {
        self.nodes.get(record as usize).is_some_and(|n| n.slot != NONE)
    }

    fn push(&mut self, record: u32, name: &str) -> u32 {
        let slot = Slot { record, name: self.names.len() as u32, folded: self.folded.len() as u32, name_len: name.len() as u16, folded_len: 0 };
        self.names.extend_from_slice(name.as_bytes());
        fold::fold_into(name, &mut self.folded);
        let folded_len = (self.folded.len() - slot.folded as usize).min(u16::MAX as usize);
        self.folded.truncate(slot.folded as usize + folded_len);
        self.folded.push(0);
        self.slots.push(Slot { folded_len: folded_len as u16, ..slot });
        (self.slots.len() - 1) as u32
    }

    fn kill(&mut self, slot: u32) {
        let s = &mut self.slots[slot as usize];
        s.record = NONE;
        self.dead += s.name_len as usize + s.folded_len as usize + 1;
    }

    /// Rebuilds the arenas without dead names once they take a third of them.
    pub fn maybe_compact(&mut self) {
        if self.dead > 1 << 20 && self.dead * 3 > self.names.len() + self.folded.len() {
            self.compact();
        }
    }

    pub fn compact(&mut self) {
        let (slots, names, folded) = (std::mem::take(&mut self.slots), std::mem::take(&mut self.names), std::mem::take(&mut self.folded));
        self.names = Vec::with_capacity(names.len() - self.dead.min(names.len()));
        self.folded = Vec::with_capacity(folded.len());
        self.folded.push(0);
        self.slots = Vec::with_capacity(self.live);
        for s in slots.into_iter().filter(|s| s.record != NONE) {
            let new = Slot { name: self.names.len() as u32, folded: self.folded.len() as u32, ..s };
            self.names.extend_from_slice(&names[s.name as usize..][..s.name_len as usize]);
            self.folded.extend_from_slice(&folded[s.folded as usize..][..s.folded_len as usize]);
            self.folded.push(0);
            self.nodes[s.record as usize].slot = self.slots.len() as u32;
            self.slots.push(new);
        }
        self.shrink();
        self.dead = 0;
    }

    fn name_of(&self, slot: u32) -> &str {
        let s = self.slots[slot as usize];
        // Only whole `&str`s are pushed, so a slot's bytes are valid UTF-8.
        std::str::from_utf8(&self.names[s.name as usize..][..s.name_len as usize]).unwrap_or("")
    }

    /// The name of `record`, if held.
    pub fn name(&self, record: u32) -> Option<&str> {
        let n = self.nodes.get(record as usize)?;
        (n.slot != NONE).then(|| self.name_of(n.slot))
    }

    /// `record`'s full path, "C:\dir\file"; `None` when its parents don't reach the root (NTFS's
    /// own files under `$Extend`, or a folder whose record we don't hold).
    pub fn path(&self, record: u32) -> Option<String> {
        let mut parts: Vec<&str> = Vec::new();
        let mut r = record;
        for _ in 0..MAX_DEPTH {
            if r == ROOT {
                let mut path = self.root.clone();
                for (i, p) in parts.iter().rev().enumerate() {
                    if i > 0 {
                        path.push(self.sep);
                    }
                    path.push_str(p);
                }
                return Some(path);
            }
            let n = self.nodes.get(r as usize)?;
            if n.slot == NONE {
                return None;
            }
            parts.push(self.name_of(n.slot));
            r = n.parent;
        }
        None
    }

    /// Whether `record` sits somewhere under the folder `dir`, and reaches the root; with
    /// `skip_hidden`, also that neither it nor a folder between it and `dir` is hidden.
    fn is_under(&self, record: u32, dir: u32, skip_hidden: bool) -> bool {
        let Some(n) = self.nodes.get(record as usize) else { return false };
        if skip_hidden && self.hidden(n) {
            return false;
        }
        let mut r = n.parent;
        for _ in 0..MAX_DEPTH {
            if r == dir {
                return true;
            }
            if r == ROOT {
                return false;
            }
            match self.nodes.get(r as usize) {
                Some(n) if n.slot != NONE && !(skip_hidden && self.hidden(n)) => r = n.parent,
                _ => return false,
            }
        }
        false
    }

    /// Hidden by its attribute, or a dotfile.
    fn hidden(&self, n: &Node) -> bool {
        n.slot != NONE && (n.flags & HIDDEN != 0 || self.name_of(n.slot).starts_with('.'))
    }

    pub fn row(&self, record: u32) -> Option<Row> {
        let n = self.nodes.get(record as usize)?;
        Some(Row { path: self.path(record)?, is_dir: n.flags & DIR != 0, size: n.size, modified: n.modified })
    }

    /// The folder at `path` ("C:\Users\Ana", "/home/ana", or the root), found by its names: each
    /// one is looked for whole in the folded arena, then checked as written (exactly on Linux).
    pub fn find(&self, path: &str) -> Option<u32> {
        let exact = self.sep == '/';
        let rest = path.get(..self.root.len()).filter(|p| if exact { *p == self.root } else { p.eq_ignore_ascii_case(&self.root) }).map(|_| &path[self.root.len()..])?;
        let mut at = ROOT;
        for part in rest.split(['\\', '/']).filter(|p| !p.is_empty()) {
            let mut needle = vec![0];
            fold::fold_into(part, &mut needle);
            needle.push(0);
            let lower = part.to_lowercase();
            at = memmem::find_iter(&self.folded, &needle).find_map(|pos| {
                let start = pos as u32 + 1;
                let i = self.slots.partition_point(|s| s.folded < start);
                let s = self.slots.get(i).filter(|s| s.folded == start && s.record != NONE)?;
                let n = self.nodes[s.record as usize];
                let name = self.name_of(i as u32);
                (n.parent == at && n.flags & DIR != 0 && if exact { name == part } else { name.to_lowercase() == lower }).then_some(s.record)
            })?;
        }
        Some(at)
    }

    /// Records whose name holds every word of `q`, under the folder `within` when given: up to
    /// `limit` of them in `out`; returns how many there are in all. Runs on `threads` threads.
    /// With `skip_hidden`, hidden files and what's inside hidden folders below `within` are left
    /// out, as a listing without hidden files would.
    pub fn search(&self, q: &Query, within: Option<u32>, skip_hidden: bool, limit: usize, threads: usize, out: &mut Vec<u32>) -> usize {
        let n = self.slots.len();
        let threads = threads.clamp(1, 64).min(n / 20_000 + 1);
        let parts: Vec<(usize, Vec<u32>)> = std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..threads).map(|t| (n * t / threads, n * (t + 1) / threads)).map(|(a, b)| scope.spawn(move || self.search_slots(q, within, skip_hidden, limit, a, b))).collect();
            jobs.into_iter().map(|j| j.join().unwrap_or_default()).collect()
        });
        let mut total = 0;
        for (count, hits) in parts {
            total += count;
            let room = limit.saturating_sub(out.len());
            out.extend(hits.into_iter().take(room));
        }
        total
    }

    /// [`Tree::search`] over the slots `a..b`.
    fn search_slots(&self, q: &Query, within: Option<u32>, skip_hidden: bool, limit: usize, a: usize, b: usize) -> (usize, Vec<u32>) {
        if a >= b {
            return (0, Vec::new());
        }
        let (first, rest) = q.words.split_first().expect("a query has a word");
        let lo = self.slots[a].folded as usize;
        let last = self.slots[b - 1];
        let hi = last.folded as usize + last.folded_len as usize;
        let finder = memmem::Finder::new(first);
        let others: Vec<memmem::Finder> = rest.iter().map(memmem::Finder::new).collect();
        let (mut count, mut hits) = (0, Vec::new());
        // Past the name of the last hit: a name counts once, however many times it holds a word.
        let mut skip = 0;
        let mut at = a;
        for pos in finder.find_iter(&self.folded[lo..hi]) {
            let p = lo + pos;
            if p < skip {
                continue;
            }
            at += self.slots[at..b].partition_point(|s| s.folded as usize <= p) - 1;
            let s = self.slots[at];
            skip = s.folded as usize + s.folded_len as usize;
            if s.record == NONE {
                continue;
            }
            let name = &self.folded[s.folded as usize..skip];
            if !others.iter().all(|f| f.find(name).is_some()) || !self.is_under(s.record, within.unwrap_or(ROOT), skip_hidden) {
                continue;
            }
            count += 1;
            if hits.len() < limit {
                hits.push(s.record);
            }
        }
        (count, hits)
    }
}

/// What a saved tree starts with, and its layout's version.
const MAGIC: &[u8; 8] = b"OEINDEX1";

impl Tree {
    /// Writes the tree, to be read back by [`Tree::load`] at the next start: searches answer
    /// from it at once while the disks are read again.
    pub fn save(&self, w: &mut impl Write) -> std::io::Result<()> {
        w.write_all(MAGIC)?;
        let mut head = Vec::new();
        for n in [self.root.len(), self.nodes.len(), self.slots.len(), self.names.len(), self.folded.len(), self.dead, self.live] {
            head.extend_from_slice(&(n as u64).to_le_bytes());
        }
        head.extend_from_slice(self.root.as_bytes());
        w.write_all(&head)?;
        let mut buf = Vec::with_capacity(self.nodes.len() * 21);
        for n in &self.nodes {
            buf.extend_from_slice(&n.parent.to_le_bytes());
            buf.extend_from_slice(&n.slot.to_le_bytes());
            buf.extend_from_slice(&n.size.to_le_bytes());
            buf.extend_from_slice(&n.modified.to_le_bytes());
            buf.push(n.flags);
        }
        w.write_all(&buf)?;
        buf.clear();
        for s in &self.slots {
            for v in [s.record, s.name, s.folded] {
                buf.extend_from_slice(&v.to_le_bytes());
            }
            buf.extend_from_slice(&s.name_len.to_le_bytes());
            buf.extend_from_slice(&s.folded_len.to_le_bytes());
        }
        w.write_all(&buf)?;
        w.write_all(&self.names)?;
        w.write_all(&self.folded)?;
        w.flush()
    }

    /// Reads what [`Tree::save`] wrote; an error for anything else, or a tree that doesn't hold
    /// together (a file cut short or from another version).
    pub fn load(r: &mut impl Read) -> std::io::Result<Tree> {
        let bad = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("índice salvo inválido: {what}"));
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(bad("versão"));
        }
        let mut head = [0u8; 56];
        r.read_exact(&mut head)?;
        let n = |i: usize| u64::from_le_bytes(head[i * 8..i * 8 + 8].try_into().unwrap()) as usize;
        let (root_len, nodes, slots, names, folded, dead, live) = (n(0), n(1), n(2), n(3), n(4), n(5), n(6));
        if root_len > 4096 || nodes > u32::MAX as usize || slots > u32::MAX as usize || names > u32::MAX as usize || folded > u32::MAX as usize {
            return Err(bad("tamanhos"));
        }
        let read = |r: &mut dyn Read, len: usize| -> std::io::Result<Vec<u8>> {
            let mut v = Vec::new();
            r.take(len as u64).read_to_end(&mut v)?;
            if v.len() == len { Ok(v) } else { Err(bad("cortado")) }
        };
        let root = String::from_utf8(read(r, root_len)?).map_err(|_| bad("raiz"))?;
        let mut t = Tree::new(&root);
        let raw = read(r, nodes * 21)?;
        t.nodes = raw
            .as_chunks::<21>().0.iter()
            .map(|c| Node {
                parent: u32::from_le_bytes(c[0..4].try_into().unwrap()),
                slot: u32::from_le_bytes(c[4..8].try_into().unwrap()),
                size: u64::from_le_bytes(c[8..16].try_into().unwrap()),
                modified: u32::from_le_bytes(c[16..20].try_into().unwrap()),
                flags: c[20],
            })
            .collect();
        let raw = read(r, slots * 16)?;
        t.slots = raw
            .as_chunks::<16>().0.iter()
            .map(|c| Slot {
                record: u32::from_le_bytes(c[0..4].try_into().unwrap()),
                name: u32::from_le_bytes(c[4..8].try_into().unwrap()),
                folded: u32::from_le_bytes(c[8..12].try_into().unwrap()),
                name_len: u16::from_le_bytes(c[12..14].try_into().unwrap()),
                folded_len: u16::from_le_bytes(c[14..16].try_into().unwrap()),
            })
            .collect();
        t.names = read(r, names)?;
        t.folded = read(r, folded)?;
        (t.dead, t.live) = (dead, live);
        // Every live slot within the arenas and owned by its node, every node's slot real: what
        // the searches index by without checking.
        let ok = t.folded.first() == Some(&0)
            && t.slots.iter().enumerate().all(|(i, s)| {
                s.name as usize + s.name_len as usize <= t.names.len()
                    && s.folded as usize + (s.folded_len as usize) < t.folded.len()
                    && std::str::from_utf8(&t.names[s.name as usize..][..s.name_len as usize]).is_ok()
                    && (s.record == NONE || t.nodes.get(s.record as usize).is_some_and(|n| n.slot == i as u32))
            })
            && t.slots.windows(2).all(|w| w[0].folded < w[1].folded)
            && t.nodes.iter().enumerate().all(|(i, n)| n.slot == NONE || t.slots.get(n.slot as usize).is_some_and(|s| s.record == i as u32))
            && t.nodes.iter().filter(|n| n.slot != NONE).count() == live;
        if !ok {
            return Err(bad("inconsistente"));
        }
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C:\Docs\Relatório 2025.pdf, C:\Docs\Fotos\relatorio.jpg, C:\Outros\Relatórios\, C:\a.txt
    fn sample() -> Tree {
        let mut t = Tree::new("C:\\");
        t.set(30, ROOT, "Docs", DIR, 0, 0);
        t.set(31, 30, "Relatório 2025.pdf", 0, 1000, 7);
        t.set(32, 30, "Fotos", DIR, 0, 0);
        t.set(33, 32, "relatorio.jpg", 0, 20, 8);
        t.set(40, ROOT, "Outros", DIR, 0, 0);
        t.set(41, 40, "Relatórios", DIR, 0, 0);
        t.set(50, ROOT, "a.txt", 0, 1, 1);
        t
    }

    fn found(t: &Tree, text: &str, within: Option<u32>) -> Vec<String> {
        let mut out = Vec::new();
        let total = t.search(&Query::new(text).unwrap(), within, false, usize::MAX, 4, &mut out);
        assert_eq!(total, out.len());
        let mut paths: Vec<String> = out.into_iter().map(|r| t.path(r).unwrap()).collect();
        paths.sort();
        paths
    }

    #[test]
    fn every_word_in_the_name_whatever_case_and_accents() {
        let t = sample();
        assert_eq!(found(&t, "RELATORIO", None), ["C:\\Docs\\Fotos\\relatorio.jpg", "C:\\Docs\\Relatório 2025.pdf", "C:\\Outros\\Relatórios"]);
        assert_eq!(found(&t, "2025 rel", None), ["C:\\Docs\\Relatório 2025.pdf"]);
        assert_eq!(found(&t, "pdf 2024", None), Vec::<String>::new());
        // A word can't match across two names.
        assert_eq!(found(&t, "pdffotos", None), Vec::<String>::new());
        assert!(Query::new("   ").is_none());
    }

    #[test]
    fn a_search_keeps_to_a_folder_found_by_its_path() {
        let t = sample();
        let docs = t.find("c:\\docs").unwrap();
        assert_eq!(docs, 30);
        assert_eq!(t.find("C:\\Docs\\FOTOS\\"), Some(32));
        assert_eq!(t.find("C:\\"), Some(ROOT));
        assert_eq!(t.find("C:\\Docs\\Relatório 2025.pdf"), None, "not a folder");
        assert_eq!(t.find("C:\\Nada"), None);
        assert_eq!(t.find("D:\\Docs"), None);
        assert_eq!(found(&t, "relatorio", Some(docs)), ["C:\\Docs\\Fotos\\relatorio.jpg", "C:\\Docs\\Relatório 2025.pdf"]);
    }

    #[test]
    fn hidden_files_and_what_hidden_folders_hold_can_be_left_out() {
        let mut t = sample();
        t.set(60, ROOT, "AppData", DIR | HIDDEN, 0, 0);
        t.set(61, 60, "relatorio.log", 0, 1, 1);
        t.set(62, 30, ".relatorio", 0, 1, 1);
        t.set(63, 30, "relatorio oculto.txt", HIDDEN, 1, 1);
        let paths = |within: Option<u32>, skip: bool| {
            let mut out = Vec::new();
            let total = t.search(&Query::new("relatorio").unwrap(), within, skip, usize::MAX, 2, &mut out);
            assert_eq!(total, out.len());
            let mut v: Vec<String> = out.into_iter().map(|r| t.path(r).unwrap()).collect();
            v.sort();
            v
        };
        assert_eq!(paths(None, false).len(), 6);
        assert_eq!(paths(None, true), ["C:\\Docs\\Fotos\\relatorio.jpg", "C:\\Docs\\Relatório 2025.pdf", "C:\\Outros\\Relatórios"]);
        // From inside a hidden folder, what it holds still shows.
        assert_eq!(paths(Some(60), true), ["C:\\AppData\\relatorio.log"]);
    }

    #[test]
    fn renames_moves_and_deletes_show_at_once_and_compact_away() {
        let mut t = sample();
        t.set(31, 40, "Balanço.pdf", 0, 5, 9); // renamed and moved
        t.remove(33);
        t.set(41, 40, "Relatórios", DIR, 0, 3); // same name: only touched
        assert_eq!(found(&t, "relatorio", None), ["C:\\Outros\\Relatórios"]);
        assert_eq!(found(&t, "balanco", None), ["C:\\Outros\\Balanço.pdf"]);
        assert_eq!(t.len(), 6);
        t.compact();
        assert_eq!(found(&t, "relatorio", None), ["C:\\Outros\\Relatórios"]);
        assert_eq!(found(&t, "a", None).len(), 3, "Balanço, Relatórios, a.txt");
        assert_eq!(t.row(31), Some(Row { path: "C:\\Outros\\Balanço.pdf".into(), is_dir: false, size: 5, modified: 9 }));
        // A file whose folder isn't held has no path and isn't found.
        t.set(60, 999, "solto.txt", 0, 0, 0);
        assert_eq!(t.path(60), None);
        assert_eq!(found(&t, "solto", None), Vec::<String>::new());
    }

    #[test]
    fn linux_paths_and_children_by_name() {
        let mut t = Tree::new("/");
        t.link();
        t.set(30, ROOT, "home", DIR, 0, 0);
        t.set(31, 30, "ana", DIR, 0, 0);
        t.set(32, 31, "Notas.txt", 0, 3, 0);
        t.set(33, 31, "notas.txt", 0, 4, 0);
        assert_eq!(t.path(32).as_deref(), Some("/home/ana/Notas.txt"));
        assert_eq!(t.find("/home/ana"), Some(31));
        assert_eq!(t.find("/HOME/ana"), None, "Linux names are exact");
        assert_eq!((t.child(31, "Notas.txt"), t.child(31, "notas.txt"), t.child(31, "x")), (Some(32), Some(33), None));
        // A move takes the file out of one folder's children into another's.
        t.set(32, 30, "Notas.txt", 0, 3, 0);
        assert_eq!((t.child(31, "Notas.txt"), t.child(30, "Notas.txt")), (None, Some(32)));
        let mut kids = t.children(30);
        kids.sort();
        assert_eq!(kids, [31, 32]);
        // A folder goes with what's under it.
        assert_eq!(t.remove_tree(31), [31, 33]);
        assert_eq!((t.len(), t.children(30)), (2, vec![32]));
        t.remove(32);
        assert_eq!(t.children(30), Vec::<u32>::new());
    }

    #[test]
    fn a_saved_tree_loads_as_it_was_and_damage_is_refused() {
        let mut t = sample();
        t.remove(33);
        t.set(31, 40, "Balanço.pdf", 0, 5, 9);
        let mut buf = Vec::new();
        t.save(&mut buf).unwrap();
        let back = Tree::load(&mut buf.as_slice()).unwrap();
        assert_eq!(back.len(), t.len());
        assert_eq!(found(&back, "balanco", None), ["C:\\Outros\\Balanço.pdf"]);
        assert_eq!(back.row(31), t.row(31));
        assert!(Tree::load(&mut &buf[..buf.len() - 1]).is_err());
        let mut broken = buf.clone();
        broken[8 + 56 + 3 + 4] ^= 0x7f; // the first node's slot
        assert!(Tree::load(&mut broken.as_slice()).is_err());
        assert!(Tree::load(&mut &b"OEINDEX0"[..]).is_err());
    }

    #[test]
    fn many_names_split_across_threads_count_once_each() {
        let mut t = Tree::new("C:\\");
        for i in 0..100_000u32 {
            t.set(100 + i, ROOT, &format!("arquivo {i} aa.txt"), 0, 0, 0);
        }
        let mut out = Vec::new();
        assert_eq!(t.search(&Query::new("aa").unwrap(), None, false, 10, 8, &mut out), 100_000);
        assert_eq!(out.len(), 10);
        out.clear();
        let sevens = (0..100_000u32).filter(|i| i.to_string().contains("777")).count();
        assert_eq!(t.search(&Query::new("arquivo 777").unwrap(), None, false, usize::MAX, 8, &mut out), sevens);
        assert_eq!(out.len(), sevens);
    }
}
