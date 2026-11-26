//! The folders of a drive as they were listed, kept by item id, and the account's delta
//! position they are valid at.
//!
//! Only folders opened are listed ("listed": their children all known); the rest of the drive
//! isn't read. The delta feed (`/root/delta`) gives the state now of every item changed
//! anywhere since the position, so a listing is brought up to date in place rather than read
//! again: an item whose parent is a listed folder goes into it (or moves there, leaving the old
//! one), one deleted or moved somewhere unlisted leaves. Delta carries no paths, so everything
//! goes by id and `parentReference.id`.
//!
//! Applying an item's state is idempotent, so a listing read after the position was taken
//! stays right as later changes arrive; one read before it would not, so a position is always
//! taken before the first listing (see `account.rs`).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type Ix = u32;

/// An item shared from another drive that shows in this one (a shortcut "added to My files"):
/// opening it reaches that drive's item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    pub drive: String,
    pub id: String,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub id: Box<str>,
    pub parent: Option<Ix>,
    pub name: Box<str>,
    pub size: u64,
    /// Seconds since 1970; 0 when unknown.
    pub modified: i64,
    pub is_dir: bool,
    /// A folder's children, once listed.
    pub children: Option<Vec<Ix>>,
    pub remote: Option<Box<Remote>>,
    /// How many children Graph last said the folder has (0 when it didn't say). Kept in memory
    /// only: it tells a listing whether to read from both ends at once.
    pub kids: u32,
}

/// An item as Graph describes it (a listing's, a delta's, an operation's answer).
#[derive(Clone, Debug, PartialEq)]
pub struct Seen {
    pub id: String,
    pub parent: Option<String>,
    pub name: Option<String>,
    pub size: u64,
    pub modified: i64,
    pub is_dir: bool,
    pub deleted: bool,
    pub root: bool,
    pub remote: Option<Remote>,
    /// A folder's `childCount`; 0 when not given.
    pub kids: u32,
}

/// The fields the delta and single items ask for.
pub const SELECT: &str = "id,name,size,lastModifiedDateTime,folder,parentReference,deleted,root,remoteItem,package";

/// The fields a folder's listing asks for: `file` instead of `folder`, whose count of children
/// Graph works out for every folder listed (about 8 s for a page of 1,000 folders, against under
/// 2 without). An item listed with neither `file` nor `package` is a folder ([`Seen::listed`]).
pub const LIST: &str = "id,name,size,lastModifiedDateTime,parentReference,deleted,root,remoteItem,package,file";

fn time(v: Option<&Value>) -> i64 {
    v.and_then(Value::as_str).and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map(|d| d.timestamp()).unwrap_or(0)
}

impl Seen {
    /// Reads a driveItem from a listing asked with [`LIST`]: one with neither `file` nor
    /// `package` is a folder (a shortcut to another drive's item says what that is).
    pub fn listed(v: &Value) -> Option<Seen> {
        let mut s = Seen::of(v)?;
        if s.remote.is_none() && !s.root {
            s.is_dir = v.get("folder").is_some() || (v.get("file").is_none() && v.get("package").is_none());
        }
        Some(s)
    }

    /// Reads a driveItem; `None` for one without an id.
    pub fn of(v: &Value) -> Option<Seen> {
        let id = v.get("id")?.as_str()?.to_string();
        let remote = v.get("remoteItem").and_then(|r| {
            let rid = r.get("id")?.as_str()?.to_string();
            let drive = r.pointer("/parentReference/driveId").and_then(Value::as_str)?.to_string();
            Some((Remote { drive, id: rid }, r.get("folder").is_some()))
        });
        let is_dir = v.get("folder").is_some() || v.get("root").is_some() || remote.as_ref().is_some_and(|(_, d)| *d);
        Some(Seen {
            id,
            parent: v.pointer("/parentReference/id").and_then(Value::as_str).map(str::to_string),
            name: v.get("name").and_then(Value::as_str).map(str::to_string),
            size: v.get("size").and_then(Value::as_u64).unwrap_or(0),
            modified: time(v.get("lastModifiedDateTime")),
            is_dir,
            deleted: v.get("deleted").is_some(),
            root: v.get("root").is_some(),
            remote: remote.map(|(r, _)| r),
            kids: v.pointer("/folder/childCount").or_else(|| v.pointer("/remoteItem/folder/childCount")).and_then(Value::as_u64).map_or(0, |n| n.min(u32::MAX as u64) as u32),
        })
    }
}

/// What the delta brought, for the panes: the folders whose listing changed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Changes {
    /// Everything may have changed: the listings were dropped (a resync, too many changes).
    pub all: bool,
    pub folders: HashSet<String>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        !self.all && self.folders.is_empty()
    }

    /// Whether the listing of folder `id` changed.
    pub fn touches(&self, id: &str) -> bool {
        self.all || self.folders.contains(id)
    }
}

#[derive(Default)]
pub struct Tree {
    nodes: Vec<Option<Node>>,
    free: Vec<Ix>,
    by_id: HashMap<Box<str>, Ix>,
    pub root: Option<Ix>,
    /// The delta link the listings are valid at.
    pub delta: Option<String>,
}

/// Where a path leads, as far as the listings know.
#[derive(Clone, Debug, PartialEq)]
pub enum Walk {
    Found(Ix),
    /// Folder `Ix`, at segment `depth`, isn't listed yet.
    Unlisted(Ix, usize),
    /// A shortcut to another drive at segment `depth`: the rest of the path is there.
    Remote(Ix, usize),
    Missing,
}

/// Names are the same to OneDrive whatever their case.
pub fn same_name(a: &str, b: &str) -> bool {
    a == b || a.to_lowercase() == b.to_lowercase()
}

impl Tree {
    pub fn get(&self, ix: Ix) -> &Node {
        self.nodes[ix as usize].as_ref().expect("a live node")
    }

    fn get_mut(&mut self, ix: Ix) -> &mut Node {
        self.nodes[ix as usize].as_mut().expect("a live node")
    }

    pub fn find(&self, id: &str) -> Option<Ix> {
        self.by_id.get(id).copied()
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    pub fn listed(&self, ix: Ix) -> bool {
        self.get(ix).children.is_some()
    }

    fn alloc(&mut self, node: Node) -> Ix {
        let id = node.id.clone();
        let ix = match self.free.pop() {
            Some(ix) => {
                self.nodes[ix as usize] = Some(node);
                ix
            }
            None => {
                self.nodes.push(Some(node));
                (self.nodes.len() - 1) as Ix
            }
        };
        self.by_id.insert(id, ix);
        ix
    }

    /// Takes `ix` and everything under it out.
    fn drop_subtree(&mut self, ix: Ix) {
        let mut stack = vec![ix];
        while let Some(i) = stack.pop() {
            if let Some(n) = self.nodes[i as usize].take() {
                self.by_id.remove(&n.id);
                stack.extend(n.children.into_iter().flatten());
                self.free.push(i);
            }
        }
    }

    fn detach(&mut self, ix: Ix) {
        if let Some(p) = self.get(ix).parent
            && let Some(kids) = self.get_mut(p).children.as_mut()
        {
            kids.retain(|k| *k != ix);
        }
        self.get_mut(ix).parent = None;
    }

    /// Removes the item `ix` (and what it holds) from where it is.
    pub fn remove(&mut self, ix: Ix) {
        if Some(ix) == self.root {
            return;
        }
        self.detach(ix);
        self.drop_subtree(ix);
    }

    /// The drive's top, from its item.
    pub fn set_root(&mut self, s: &Seen) -> Ix {
        if let Some(r) = self.root {
            let n = self.get_mut(r);
            n.modified = s.modified;
            return r;
        }
        let ix = self.alloc(Node { id: s.id.as_str().into(), parent: None, name: "".into(), size: 0, modified: s.modified, is_dir: true, children: None, remote: None, kids: 0 });
        self.root = Some(ix);
        ix
    }

    /// The child of folder `dir` named `name` (exactly, else in another case).
    pub fn child(&self, dir: Ix, name: &str) -> Option<Ix> {
        let kids = self.get(dir).children.as_ref()?;
        kids.iter().copied().find(|k| &*self.get(*k).name == name).or_else(|| kids.iter().copied().find(|k| same_name(&self.get(*k).name, name)))
    }

    pub fn children(&self, dir: Ix) -> Option<&[Ix]> {
        self.get(dir).children.as_deref()
    }

    /// Follows `segs` from the top.
    pub fn walk(&self, segs: &[String]) -> Walk {
        let Some(mut at) = self.root else { return Walk::Missing };
        for (depth, seg) in segs.iter().enumerate() {
            let n = self.get(at);
            if n.remote.is_some() {
                return Walk::Remote(at, depth);
            }
            if !n.is_dir {
                return Walk::Missing;
            }
            if n.children.is_none() {
                return Walk::Unlisted(at, depth);
            }
            match self.child(at, seg) {
                Some(c) => at = c,
                None => return Walk::Missing,
            }
        }
        if self.get(at).remote.is_some() {
            return Walk::Remote(at, segs.len());
        }
        Walk::Found(at)
    }

    /// Takes in item `s`'s state now: into its parent when that's listed, out of where it was
    /// otherwise. The folders whose listing changed go into `touched`.
    pub fn apply(&mut self, s: &Seen, touched: &mut HashSet<String>) {
        if s.root {
            self.set_root(s);
            return;
        }
        let existing = self.find(&s.id);
        let parent = s.parent.as_deref().and_then(|p| self.find(p)).filter(|p| self.get(*p).children.is_some());
        let mut note = |t: &Tree, ix: Ix| {
            touched.insert(t.get(ix).id.to_string());
        };
        if s.deleted {
            if let Some(ix) = existing {
                if let Some(p) = self.get(ix).parent {
                    note(self, p);
                }
                self.remove(ix);
            }
            return;
        }
        match (existing, parent) {
            (Some(ix), Some(p)) => {
                let old = self.get(ix).parent;
                if old != Some(p) {
                    if let Some(o) = old {
                        note(self, o);
                    }
                    self.detach(ix);
                    self.get_mut(ix).parent = Some(p);
                    self.get_mut(p).children.as_mut().expect("a listed parent").push(ix);
                }
                note(self, p);
                let n = self.get_mut(ix);
                if let Some(name) = &s.name {
                    n.name = name.as_str().into();
                }
                n.size = s.size;
                n.modified = s.modified;
                n.kids = s.kids;
                if n.is_dir != s.is_dir || n.remote.as_deref() != s.remote.as_ref() {
                    n.is_dir = s.is_dir;
                    n.remote = s.remote.clone().map(Box::new);
                    n.children = None;
                }
            }
            (Some(ix), None) => {
                // Moved somewhere not listed (or its parent isn't known): it leaves.
                if let Some(o) = self.get(ix).parent {
                    note(self, o);
                }
                self.remove(ix);
            }
            (None, Some(p)) => {
                let Some(name) = &s.name else { return };
                let ix = self.alloc(Node {
                    id: s.id.as_str().into(),
                    parent: Some(p),
                    name: name.as_str().into(),
                    size: s.size,
                    modified: s.modified,
                    is_dir: s.is_dir,
                    children: None,
                    remote: s.remote.clone().map(Box::new),
                    kids: s.kids,
                });
                self.get_mut(p).children.as_mut().expect("a listed parent").push(ix);
                note(self, p);
            }
            (None, None) => {}
        }
    }

    /// Folder `dir`'s whole listing, read now: what it held and no longer does leaves.
    pub fn set_listing(&mut self, dir: Ix, items: &[Seen]) {
        let keep: HashSet<&str> = items.iter().map(|s| s.id.as_str()).collect();
        let gone: Vec<Ix> = self.get(dir).children.iter().flatten().copied().filter(|k| !keep.contains(&*self.get(*k).id)).collect();
        for g in gone {
            self.remove(g);
        }
        if self.get(dir).children.is_none() {
            self.get_mut(dir).children = Some(Vec::with_capacity(items.len()));
        }
        let id = self.get(dir).id.to_string();
        let mut ignored = HashSet::new();
        for s in items {
            let mut s = s.clone();
            s.parent = Some(id.clone());
            self.apply(&s, &mut ignored);
        }
    }

    /// Forgets every listing (a resync): only the top stays.
    pub fn drop_listings(&mut self) {
        let Some(r) = self.root else { return };
        let kids = self.get_mut(r).children.take().unwrap_or_default();
        for k in kids {
            self.drop_subtree(k);
        }
    }

    /// Folder `ix`'s path segments from the top.
    pub fn segs(&self, mut ix: Ix) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(p) = self.get(ix).parent {
            out.push(self.get(ix).name.to_string());
            ix = p;
        }
        out.reverse();
        out
    }

    // -----------------------------------------------------------------------------------------
    // Kept on disk

    /// The tree as records, the top first, each parent before its children.
    pub fn snapshot(&self) -> Snapshot {
        let mut nodes = Vec::with_capacity(self.len());
        let Some(r) = self.root else { return Snapshot { delta: self.delta.clone(), nodes } };
        let mut stack: Vec<(Ix, u32)> = vec![(r, u32::MAX)];
        while let Some((ix, parent)) = stack.pop() {
            let n = self.get(ix);
            let me = nodes.len() as u32;
            nodes.push(Rec {
                i: n.id.to_string(),
                p: parent,
                n: n.name.to_string(),
                s: n.size,
                m: n.modified,
                d: n.is_dir,
                l: n.children.is_some(),
                r: n.remote.as_deref().cloned(),
            });
            for k in n.children.iter().flatten().rev() {
                stack.push((*k, me));
            }
        }
        Snapshot { delta: self.delta.clone(), nodes }
    }

    /// The tree a [`Tree::snapshot`] was; `None` when the records don't make one.
    pub fn from_snapshot(snap: Snapshot) -> Option<Tree> {
        let mut t = Tree { delta: snap.delta, ..Tree::default() };
        let mut at: Vec<Ix> = Vec::with_capacity(snap.nodes.len());
        for (k, rec) in snap.nodes.into_iter().enumerate() {
            let parent = if rec.p == u32::MAX {
                if k != 0 {
                    return None;
                }
                None
            } else {
                Some(*at.get(rec.p as usize)?)
            };
            if t.by_id.contains_key(rec.i.as_str()) {
                return None;
            }
            let ix = t.alloc(Node { id: rec.i.into(), parent, name: rec.n.into(), size: rec.s, modified: rec.m, is_dir: rec.d, children: rec.l.then(Vec::new), remote: rec.r.map(Box::new), kids: 0 });
            match parent {
                Some(p) => t.get_mut(p).children.as_mut()?.push(ix),
                None => t.root = Some(ix),
            }
            at.push(ix);
        }
        Some(t)
    }
}

/// One item in a kept tree; `p` is its parent's place among the records.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rec {
    pub i: String,
    pub p: u32,
    pub n: String,
    pub s: u64,
    pub m: i64,
    pub d: bool,
    pub l: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r: Option<Remote>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub delta: Option<String>,
    pub nodes: Vec<Rec>,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub fn file(id: &str, parent: &str, name: &str) -> Seen {
        Seen { id: id.into(), parent: Some(parent.into()), name: Some(name.into()), size: 10, modified: 1, is_dir: false, deleted: false, root: false, remote: None, kids: 0 }
    }

    pub fn dir(id: &str, parent: &str, name: &str) -> Seen {
        Seen { is_dir: true, size: 0, ..file(id, parent, name) }
    }

    fn gone(id: &str) -> Seen {
        Seen { deleted: true, parent: None, name: None, ..file(id, "", "") }
    }

    fn names(t: &Tree, dir: Ix) -> Vec<String> {
        let mut v: Vec<String> = t.children(dir).unwrap().iter().map(|k| t.get(*k).name.to_string()).collect();
        v.sort();
        v
    }

    /// A top with Docs (listed: a.txt, Fotos) and Fotos not listed.
    fn tree() -> Tree {
        let mut t = Tree::default();
        let r = t.set_root(&Seen { root: true, ..dir("R", "", "root") });
        t.set_listing(r, &[dir("D", "R", "Docs"), file("x", "R", "x.txt")]);
        let d = t.find("D").unwrap();
        t.set_listing(d, &[file("a", "D", "a.txt"), dir("F", "D", "Fotos")]);
        t
    }

    #[test]
    fn graph_items_read_with_their_facets() {
        let f = Seen::of(&json!({"id":"1","name":"a.txt","size":5,"lastModifiedDateTime":"2024-03-01T10:00:00Z","parentReference":{"id":"R","driveId":"d"},"file":{}})).unwrap();
        assert_eq!((f.is_dir, f.size, f.modified, f.parent.as_deref()), (false, 5, 1709287200, Some("R")));
        let d = Seen::of(&json!({"id":"2","name":"x","folder":{"childCount":3}})).unwrap();
        assert!(d.is_dir);
        let s = Seen::of(&json!({"id":"3","name":"Deles","remoteItem":{"id":"r!9","folder":{},"parentReference":{"driveId":"other"}}})).unwrap();
        assert_eq!((s.is_dir, s.remote.clone()), (true, Some(Remote { drive: "other".into(), id: "r!9".into() })));
        // OneNote notebooks are packages: shown as files.
        assert!(!Seen::of(&json!({"id":"4","name":"Notas","package":{"type":"oneNote"}})).unwrap().is_dir);
        // A business delete has no name.
        let g = Seen::of(&json!({"id":"5","deleted":{"state":"deleted"},"parentReference":{"id":"R"}})).unwrap();
        assert!(g.deleted && g.name.is_none());
    }

    #[test]
    fn paths_walk_the_listings_whatever_the_case() {
        let t = tree();
        let segs = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(t.walk(&segs(&["Docs", "a.txt"])), Walk::Found(t.find("a").unwrap()));
        assert_eq!(t.walk(&segs(&["docs", "A.TXT"])), Walk::Found(t.find("a").unwrap()));
        assert_eq!(t.walk(&segs(&["Docs", "Fotos", "y"])), Walk::Unlisted(t.find("F").unwrap(), 2));
        assert_eq!(t.walk(&segs(&["Nada"])), Walk::Missing);
        assert_eq!(t.walk(&segs(&["x.txt", "y"])), Walk::Missing);
        assert_eq!(t.segs(t.find("a").unwrap()), ["Docs", "a.txt"]);
    }

    #[test]
    fn delta_items_land_where_they_are_now() {
        let mut t = tree();
        let mut touched = HashSet::new();
        // New in a listed folder; new in one not listed (ignored); a rename.
        t.apply(&file("n", "D", "novo.txt"), &mut touched);
        t.apply(&file("z", "F", "z.txt"), &mut touched);
        t.apply(&file("a", "D", "a2.txt"), &mut touched);
        let d = t.find("D").unwrap();
        assert_eq!(names(&t, d), ["Fotos", "a2.txt", "novo.txt"]);
        assert!(t.find("z").is_none());
        // A move to the top leaves Docs; one into Fotos (not listed) leaves the tree.
        t.apply(&file("n", "R", "novo.txt"), &mut touched);
        t.apply(&file("a", "F", "a2.txt"), &mut touched);
        assert_eq!(names(&t, d), ["Fotos"]);
        assert!(t.find("a").is_none());
        assert_eq!(names(&t, t.root.unwrap()), ["Docs", "novo.txt", "x.txt"]);
        // Deleting a folder takes what it held.
        t.apply(&gone("D"), &mut touched);
        assert!(t.find("F").is_none() && t.find("D").is_none());
        assert_eq!(touched, ["D", "R"].into_iter().map(String::from).collect());
        // The same state twice changes nothing more.
        let before = names(&t, t.root.unwrap());
        t.apply(&file("n", "R", "novo.txt"), &mut touched);
        assert_eq!(names(&t, t.root.unwrap()), before);
    }

    #[test]
    fn a_new_listing_replaces_the_old_and_keeps_listed_subfolders() {
        let mut t = tree();
        let r = t.root.unwrap();
        t.set_listing(r, &[dir("D", "R", "Docs"), file("y", "R", "y.txt")]);
        assert_eq!(names(&t, r), ["Docs", "y.txt"]);
        assert!(t.find("x").is_none());
        assert!(t.listed(t.find("D").unwrap()));
        t.drop_listings();
        assert_eq!(t.len(), 1);
        assert!(!t.listed(r));
    }

    #[test]
    fn a_snapshot_rebuilds_the_same_tree() {
        let mut t = tree();
        t.delta = Some("https://graph/delta?token=abc".into());
        let snap = t.snapshot();
        let back = Tree::from_snapshot(serde_json::from_slice(&serde_json::to_vec(&snap).unwrap()).unwrap()).unwrap();
        assert_eq!(back.snapshot(), snap);
        assert_eq!(back.delta, t.delta);
        assert_eq!(back.walk(&["Docs".into(), "a.txt".into()]), Walk::Found(back.find("a").unwrap()));
        assert!(!back.listed(back.find("F").unwrap()));
        // Records that don't make a tree are refused.
        let mut bad = snap.clone();
        bad.nodes[1].p = 7;
        assert!(Tree::from_snapshot(bad).is_none());
    }
}
