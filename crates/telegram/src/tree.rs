//! The folders and files an account's channel holds, built from its messages (see `records`).
//!
//! The channel is the truth; this is a view of it, rebuilt when a message comes, changes or goes.
//! What it settles that the messages alone don't:
//!
//! - **Names that clash.** Two items in one folder may share a name (Windows doesn't tell `A` from
//!   `a`): the oldest message keeps it, the next ones show as `name (2).ext`, `name (3).ext`.
//! - **Folders that aren't there.** An item whose folder's message was deleted elsewhere, or a
//!   folder caught in a loop of parents, shows at the top rather than vanishing.
//! - **Names Windows refuses.** A document sent by hand may be named `a/b:c`; it shows as `a_b_c`.
//! - **A folder's record twice** (a resend whose old message wasn't deleted): the oldest counts.

use std::collections::{BTreeMap, HashMap, HashSet};

use openexplorer_i18n::tr;
use serde::{Deserialize, Serialize};

use crate::records::{self, ROOT, Record};

/// A message as the tree needs it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Msg {
    pub id: i32,
    pub text: String,
    /// When it was sent, seconds since 1970.
    pub date: i64,
    pub doc: Option<Doc>,
}

/// A message's document, or photo.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Doc {
    pub size: u64,
    /// The name it was sent with; what a renamed file is called is in the record.
    pub name: String,
    /// A photo (sent as one from a phone, not as a file): Telegram keeps it recompressed as JPEG,
    /// with no name.
    #[serde(default)]
    pub photo: bool,
}

/// One item of a folder.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Item {
    /// The message that holds it.
    pub msg: i32,
    /// The name shown, unique in its folder.
    pub name: String,
    /// The folder's own id; `None` for a file.
    pub dir: Option<String>,
    /// The folder it shows in.
    pub parent: String,
    pub size: u64,
    /// Modified time, seconds since 1970.
    pub modified: i64,
    /// A file of 0 bytes: a text message with no document.
    pub empty: bool,
    /// It shares its name with another item of its folder: its shown name (`a (2).txt`) moves to
    /// the other once one goes, so it's named by its message where it must stay found.
    pub twin: bool,
    /// A file too big for one message: its parts' messages, in order (`msg` is the first). Empty
    /// for any other item.
    pub parts: Vec<i32>,
    /// The id that ties the parts together.
    pub group: Option<String>,
    /// Some parts are missing (an upload cut short, or deleted elsewhere): it can't be read.
    pub incomplete: bool,
}

impl Item {
    pub fn is_dir(&self) -> bool {
        self.dir.is_some()
    }

    /// Every message the item is: its parts, or its one message.
    pub fn messages(&self) -> Vec<i32> {
        if self.parts.is_empty() { vec![self.msg] } else { self.parts.clone() }
    }
}

/// Where a path leads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    Root,
    Item(Item),
}

impl Found {
    /// The folder id, if a folder.
    pub fn dir(&self) -> Option<&str> {
        match self {
            Found::Root => Some(ROOT),
            Found::Item(i) => i.dir.as_deref(),
        }
    }
}

#[derive(Default)]
struct View {
    /// Folder id → its message.
    dirs: HashMap<String, i32>,
    /// Folder id → its items, folders first, then by name.
    children: HashMap<String, Vec<Item>>,
    /// Message → (folder it shows in, index there).
    at: HashMap<i32, (String, usize)>,
}

#[derive(Default)]
pub struct Tree {
    msgs: BTreeMap<i32, Msg>,
    view: Option<View>,
}

/// The path that names message `msg`'s item wherever it is and whatever it shows as.
pub fn pinned(msg: i32) -> String {
    format!("/\0{msg}")
}

/// `a/b:c` → `a_b_c`; empty → `sem nome`.
pub(crate) fn shown_name(name: &str) -> String {
    let mut s: String = name.chars().map(|c| if c < ' ' || r#"<>:"/\|?*"#.contains(c) { '_' } else { c }).collect();
    while s.ends_with(' ') || s.ends_with('.') {
        s.pop();
    }
    if s.is_empty() || s == "." || s == ".." { "sem nome".into() } else { s }
}

/// `nota.txt`, 2 → `nota (2).txt`; a folder or a name with no extension takes it at the end.
pub(crate) fn numbered(name: &str, n: usize, is_dir: bool) -> String {
    match name.rfind('.').filter(|&i| i > 0 && !is_dir) {
        Some(i) => format!("{} ({n}){}", &name[..i], &name[i..]),
        None => format!("{name} ({n})"),
    }
}

impl Tree {
    pub fn new(msgs: impl IntoIterator<Item = Msg>) -> Tree {
        let mut t = Tree::default();
        for m in msgs {
            if Tree::matters(&m) {
                t.msgs.insert(m.id, m);
            }
        }
        t
    }

    /// Whether a message is part of the tree: a record, or a document.
    fn matters(m: &Msg) -> bool {
        m.doc.is_some() || matches!(records::parse(&m.text), Record::Dir { .. } | Record::Empty { .. })
    }

    /// A message sent or edited.
    pub fn upsert(&mut self, m: Msg) {
        if Tree::matters(&m) {
            self.msgs.insert(m.id, m);
        } else {
            self.msgs.remove(&m.id);
        }
        self.view = None;
    }

    /// A message deleted.
    pub fn remove(&mut self, id: i32) {
        if self.msgs.remove(&id).is_some() {
            self.view = None;
        }
    }

    pub fn messages(&self) -> impl Iterator<Item = &Msg> {
        self.msgs.values()
    }

    pub fn message(&self, id: i32) -> Option<&Msg> {
        self.msgs.get(&id)
    }

    pub fn len(&self) -> usize {
        self.msgs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.msgs.is_empty()
    }

    fn view(&mut self) -> &View {
        if self.view.is_none() {
            self.view = Some(self.build());
        }
        self.view.as_ref().expect("just built")
    }

    fn build(&self) -> View {
        struct D {
            msg: i32,
            parent: String,
            name: String,
        }
        struct P {
            msg: i32,
            index: u32,
            count: u32,
            parent: String,
            name: String,
            mtime: i64,
            size: u64,
        }
        let mut dirs: HashMap<String, D> = HashMap::new();
        let mut files: Vec<Item> = Vec::new();
        let mut groups: HashMap<String, Vec<P>> = HashMap::new();
        let file = |msg: i32, parent: String, name: String, size: u64, modified: i64| Item { msg, name, parent, size, modified, ..Item::default() };
        for m in self.msgs.values() {
            match (records::parse(&m.text), &m.doc) {
                (Record::Dir { id, parent, name }, _) => {
                    dirs.entry(id).or_insert(D { msg: m.id, parent, name });
                }
                (Record::File { parent, name, mtime }, Some(doc)) => files.push(file(m.id, parent, name, doc.size, mtime.unwrap_or(m.date))),
                (Record::Empty { parent, name, mtime }, None) => files.push(Item { empty: true, ..file(m.id, parent, name, 0, mtime) }),
                (Record::Part { parent, name, mtime, group, index, count }, Some(doc)) => {
                    groups.entry(group).or_default().push(P { msg: m.id, index, count, parent, name, mtime, size: doc.size });
                }
                (Record::None, Some(doc)) => {
                    let name = match (doc.name.is_empty(), doc.photo) {
                        (false, _) => doc.name.clone(),
                        (true, true) => format!("foto-{}.jpg", m.id),
                        (true, false) => format!("mensagem-{}", m.id),
                    };
                    files.push(file(m.id, ROOT.into(), name, doc.size, m.date));
                }
                _ => {}
            }
        }
        // A file in parts shows once, as the sum of its parts, from the first part's record.
        for (group, mut parts) in groups {
            parts.sort_by_key(|p| (p.index, p.msg));
            parts.dedup_by_key(|p| p.index);
            let count = parts[0].count;
            let complete = parts.len() as u32 == count && parts.iter().enumerate().all(|(i, p)| p.index == i as u32 + 1 && p.count == count);
            let first = &parts[0];
            let name = if complete { first.name.clone() } else { tr!("{} (incompleto)", "{} (incomplete)", first.name) };
            files.push(Item {
                parts: parts.iter().map(|p| p.msg).collect(),
                group: Some(group),
                incomplete: !complete,
                ..file(first.msg, first.parent.clone(), name, parts.iter().map(|p| p.size).sum(), first.mtime)
            });
        }
        // Where each folder shows: its parent, or the top when the parent is gone or the folder
        // is the oldest of a loop.
        let mut placed: HashMap<String, String> = HashMap::new();
        for (id, d) in &dirs {
            let mut seen = HashSet::from([id.as_str()]);
            let mut at = d.parent.as_str();
            // Its own parent: a loop of one, to the top.
            let ok = at != id.as_str()
                && loop {
                if at == ROOT {
                    break true;
                }
                let Some(up) = dirs.get(at) else { break false };
                if !seen.insert(at) {
                    // A loop above, not through this folder: it's placed by the loop's own walk.
                    break true;
                }
                if up.parent == *id {
                    // A loop through this folder: its oldest member goes to the top.
                    let oldest = seen.iter().map(|s| dirs[*s].msg).min().unwrap_or(d.msg).min(up.msg);
                    break oldest != d.msg;
                }
                at = up.parent.as_str();
            };
            let parent = if ok && (d.parent == ROOT || dirs.contains_key(&d.parent)) { d.parent.clone() } else { ROOT.to_string() };
            placed.insert(id.clone(), parent);
        }
        let mut raw: HashMap<String, Vec<Item>> = HashMap::new();
        for (id, d) in &dirs {
            let parent = placed[id].clone();
            let date = self.msgs[&d.msg].date;
            raw.entry(parent.clone()).or_default().push(Item { msg: d.msg, name: shown_name(&d.name), dir: Some(id.clone()), parent, modified: date, ..Item::default() });
        }
        for mut it in files {
            if it.parent != ROOT && !dirs.contains_key(&it.parent) {
                it.parent = ROOT.to_string();
            }
            it.name = shown_name(&it.name);
            raw.entry(it.parent.clone()).or_default().push(it);
        }
        let mut view = View { dirs: dirs.iter().map(|(id, d)| (id.clone(), d.msg)).collect(), ..View::default() };
        for (parent, mut items) in raw {
            items.sort_by_key(|i| i.msg);
            let mut taken: HashSet<String> = HashSet::new();
            let mut bases: HashMap<String, usize> = HashMap::new();
            let mut base_of = Vec::with_capacity(items.len());
            for it in &mut items {
                let mut n = 1;
                let base = it.name.clone();
                *bases.entry(base.to_lowercase()).or_default() += 1;
                base_of.push(base.to_lowercase());
                while !taken.insert(it.name.to_lowercase()) {
                    n += 1;
                    it.name = numbered(&base, n, it.is_dir());
                }
            }
            for (it, base) in items.iter_mut().zip(&base_of) {
                it.twin = bases[base] > 1;
            }
            items.sort_by(|a, b| b.is_dir().cmp(&a.is_dir()).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
            for (i, it) in items.iter().enumerate() {
                view.at.insert(it.msg, (parent.clone(), i));
            }
            view.children.insert(parent, items);
        }
        view
    }

    /// The items of folder `dir` (an id; [`ROOT`] for the top), folders first.
    pub fn list(&mut self, dir: &str) -> Vec<Item> {
        self.view().children.get(dir).cloned().unwrap_or_default()
    }

    /// Whether folder `dir` exists.
    pub fn has_dir(&mut self, dir: &str) -> bool {
        dir == ROOT || self.view().dirs.contains_key(dir)
    }

    /// The item message `msg` shows as.
    pub fn item(&mut self, msg: i32) -> Option<Item> {
        let v = self.view();
        let (parent, i) = v.at.get(&msg)?;
        v.children.get(parent)?.get(*i).cloned()
    }

    /// Where `path` (`/a/b`, `a/b`, `/` or empty for the top) leads. A segment `\0<message>`
    /// ([`pinned`]) is that message's item, wherever it is.
    pub fn find(&mut self, path: &str) -> Option<Found> {
        let mut at = Found::Root;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if let Some(msg) = part.strip_prefix('\0') {
                at = Found::Item(self.item(msg.parse().ok()?)?);
                continue;
            }
            let dir = at.dir()?.to_string();
            let low = part.to_lowercase();
            let item = self.view().children.get(&dir)?.iter().find(|i| i.name.to_lowercase() == low)?.clone();
            at = Found::Item(item);
        }
        Some(at)
    }

    /// Whether folder `dir` is `ancestor` or inside it.
    pub fn within(&mut self, dir: &str, ancestor: &str) -> bool {
        let mut at = dir.to_string();
        for _ in 0..10_000 {
            if at == ancestor {
                return true;
            }
            if at == ROOT {
                return false;
            }
            let Some(&msg) = self.view().dirs.get(&at) else { return false };
            match self.item(msg) {
                Some(i) => at = i.parent,
                None => return false,
            }
        }
        false
    }

    /// Every message under folder `dir` (not its own), deepest first.
    pub fn below(&mut self, dir: &str) -> Vec<i32> {
        let mut out = Vec::new();
        for it in self.list(dir) {
            if let Some(d) = &it.dir {
                out.extend(self.below(d));
            }
            out.extend(it.messages());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::{dir_text, file_text};

    const A: &str = "aaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccc";

    fn dir(id: i32, name: &str, me: &str, parent: &str) -> Msg {
        Msg { id, text: dir_text(name, me, parent), date: 100, doc: None }
    }

    fn file(id: i32, name: &str, parent: &str) -> Msg {
        Msg { id, text: file_text(name, parent, 7), date: 100, doc: Some(Doc { size: 10, name: "enviado.bin".into(), photo: false }) }
    }

    fn names(items: &[Item]) -> Vec<&str> {
        items.iter().map(|i| i.name.as_str()).collect()
    }

    #[test]
    fn paths_lead_through_folders_whatever_the_case() {
        let mut t = Tree::new([dir(1, "Fotos", A, ROOT), dir(2, "2026", B, A), file(3, "praia.jpg", B), file(4, "nota.txt", ROOT)]);
        assert_eq!(names(&t.list(ROOT)), ["Fotos", "nota.txt"]);
        let Some(Found::Item(f)) = t.find("/fotos/2026/PRAIA.jpg") else { panic!() };
        assert_eq!((f.msg, f.size, f.modified, f.is_dir()), (3, 10, 7, false));
        assert_eq!(t.find("/"), Some(Found::Root));
        assert_eq!(t.find("Fotos/2026").and_then(|f| f.dir().map(String::from)), Some(B.into()));
        assert_eq!(t.find("/Fotos/nada"), None);
        assert_eq!(t.find("/nota.txt/x"), None);
        assert!(t.within(B, A) && t.within(A, A) && !t.within(A, B));
        assert_eq!(t.below(A), [3, 2]);
    }

    #[test]
    fn clashing_names_are_numbered_oldest_first() {
        let mut t = Tree::new([file(5, "a.txt", ROOT), file(2, "A.txt", ROOT), file(9, "a.txt", ROOT), dir(3, "a.txt", A, ROOT), dir(4, "pasta.v2", B, ROOT), dir(6, "Pasta.v2", C, ROOT)]);
        assert_eq!(names(&t.list(ROOT)), ["a.txt (2)", "pasta.v2", "Pasta.v2 (2)", "a (2).txt", "a (3).txt", "A.txt"]);
        assert_eq!(t.find("/a (3).txt").map(|f| matches!(f, Found::Item(i) if i.msg == 9)), Some(true));
        assert!(t.list(ROOT).iter().all(|i| i.twin));
        // Once the oldest goes the others shift names; pinned, each is still found.
        t.remove(2);
        assert_eq!(t.find("/a (2).txt").map(|f| matches!(f, Found::Item(i) if i.msg == 5)), Some(true));
        assert_eq!(t.find(&pinned(9)).map(|f| matches!(f, Found::Item(i) if i.msg == 9)), Some(true));
        assert_eq!(t.find(&pinned(2)), None);
        let mut alone = Tree::new([file(1, "único.txt", ROOT)]);
        assert!(!alone.list(ROOT)[0].twin);
    }

    #[test]
    fn lost_folders_loops_and_hand_sent_documents_show_at_the_top() {
        let lost = "dddddddddddddddd";
        let hand = Msg { id: 7, text: "olha isso".into(), date: 50, doc: Some(Doc { size: 3, name: "a/b:c.txt".into(), photo: false }) };
        let nameless = Msg { id: 8, text: String::new(), date: 50, doc: Some(Doc { size: 3, name: String::new(), photo: true }) };
        let newer = Msg { id: 10, text: format!("x\n#oe2 f {ROOT} 1"), date: 1, doc: Some(Doc { size: 1, name: "x".into(), photo: false }) };
        let mut t = Tree::new([
            file(1, "órfão.txt", lost),
            dir(2, "Laço 1", A, B),
            dir(3, "Laço 2", B, A),
            hand,
            nameless,
            newer,
            Msg { id: 11, text: "só texto".into(), date: 1, doc: None },
            dir(12, "Eu mesma", C, C),
            file(13, "dentro.txt", C),
        ]);
        assert_eq!(names(&t.list(ROOT)), ["Eu mesma", "Laço 1", "a_b_c.txt", "foto-8.jpg", "órfão.txt"]);
        assert_eq!(names(&t.list(A)), ["Laço 2"]);
        assert_eq!(names(&t.list(C)), ["dentro.txt"]);
        assert_eq!(t.find("/a_b_c.txt").map(|f| matches!(f, Found::Item(i) if i.modified == 50)), Some(true));
        assert_eq!(t.len(), 8);
    }

    /// What a big channel costs: building the view, and rebuilding it after one change.
    /// `cargo test -p openexplorer-telegram --release -- --ignored --nocapture big_channel`
    #[test]
    #[ignore = "a measure, not a check"]
    fn big_channel() {
        let ids: Vec<String> = (0..1000).map(|i| format!("{i:016x}")).collect();
        let mut msgs: Vec<Msg> = (0..1000).map(|i| dir(i + 1, &format!("pasta {i}"), &ids[i as usize], if i < 10 { ROOT } else { &ids[(i % 10) as usize] })).collect();
        msgs.extend((0..49_000).map(|i| file(2000 + i, &format!("arquivo {i}.jpg"), &ids[(i % 1000) as usize])));
        let t = std::time::Instant::now();
        let mut tree = Tree::new(msgs);
        let n = tree.list(&ids[5]).len();
        println!("50 mil mensagens: árvore em {:.0} ms ({n} itens numa pasta)", t.elapsed().as_secs_f64() * 1000.0);
        let t = std::time::Instant::now();
        tree.upsert(file(99_999, "novo.txt", &ids[5]));
        assert_eq!(tree.list(&ids[5]).len(), n + 1);
        println!("depois de uma mudança: {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);
    }

    #[test]
    fn an_empty_file_is_a_text_message() {
        let mut t = Tree::new([Msg { id: 1, text: records::empty_text("vazio.txt", ROOT, 9), date: 1, doc: None }]);
        let Some(Found::Item(i)) = t.find("/vazio.txt") else { panic!() };
        assert!(i.empty && !i.is_dir() && i.size == 0 && i.modified == 9);
    }

    #[test]
    fn edits_and_deletes_change_the_view() {
        let mut t = Tree::new([dir(1, "Docs", A, ROOT), file(2, "a.txt", A)]);
        assert_eq!(names(&t.list(A)), ["a.txt"]);
        t.upsert(file(2, "b.txt", ROOT));
        assert!(t.list(A).is_empty());
        assert_eq!(names(&t.list(ROOT)), ["Docs", "b.txt"]);
        t.remove(1);
        assert!(!t.has_dir(A));
        t.upsert(Msg { id: 2, text: "agora é só texto".into(), date: 1, doc: None });
        assert!(t.list(ROOT).is_empty() && t.is_empty());
        // A folder's record sent twice: the oldest counts.
        let mut t = Tree::new([dir(4, "Nova", A, ROOT), dir(3, "Velha", A, ROOT)]);
        assert_eq!(names(&t.list(ROOT)), ["Velha"]);
        assert_eq!(t.item(4), None);
    }
}
