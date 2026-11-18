//! The account's files and folders as MEGA's `f` command gives them, decrypted, with the
//! changes the `sc` channel brings applied as they come (see [`crate::changes`]).
//!
//! MEGA lets two items in a folder share a name. The panes reach items by name, so a listing
//! gives each such twin a name of its own the way Windows names a copy ("Fotos (2)"), the oldest
//! keeping the real name: see [`Tree::children`].
//!
//! # Memory
//!
//! The whole tree stays in memory, so a node is kept small: handles are fixed arrays, not
//! strings; the key sits inline; of the attributes only the name is kept apart, the rest (the
//! fingerprint, `rr`, labels…) as one short JSON text read back when a change needs them. Nodes
//! live in one vector; each folder's items are a list threaded through it, and the index of
//! names keys a hash of folder and name to the node, checked against the node itself.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::ops::Deref;

use serde_json::{Map, Value};

use crate::crypto::{self, RsaKey, attr_key, decrypt_attrs, ecb_decrypt, unb64};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Folder,
    /// Cloud Drive's top.
    Root,
    /// The Vault (backups), MEGA's old Inbox.
    Vault,
    /// Rubbish Bin's top.
    Rubbish,
}

impl Kind {
    fn of(t: i64) -> Option<Kind> {
        Some(match t {
            0 => Kind::File,
            1 => Kind::Folder,
            2 => Kind::Root,
            3 => Kind::Vault,
            4 => Kind::Rubbish,
            _ => return None,
        })
    }

    pub fn is_dir(self) -> bool {
        self != Kind::File
    }
}

/// A handle as MEGA writes it (base64 text of up to `N` ASCII characters), kept inline: empty
/// is all zeros. It reads as the `&str` it was made from, and orders the same.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id<const N: usize>(pub(crate) [u8; N]);

/// A node's or a public link's handle: 8 characters.
pub type Handle = Id<8>;
/// A user's handle: 11 characters.
pub type UserHandle = Id<11>;

impl<const N: usize> Id<N> {
    /// `None` when `text` is longer than `N` or not plain ASCII.
    pub fn new(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() > N || !bytes.iter().all(|b| b.is_ascii() && *b != 0) {
            return None;
        }
        let mut out = [0; N];
        out[..bytes.len()].copy_from_slice(bytes);
        Some(Id(out))
    }

    pub fn as_str(&self) -> &str {
        let len = self.0.iter().position(|&b| b == 0).unwrap_or(N);
        std::str::from_utf8(&self.0[..len]).unwrap_or_default()
    }
}

impl<const N: usize> Default for Id<N> {
    fn default() -> Self {
        Id([0; N])
    }
}

impl<const N: usize> Deref for Id<N> {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl<const N: usize> std::fmt::Debug for Id<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl<const N: usize> std::fmt::Display for Id<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<const N: usize> PartialEq<str> for Id<N> {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl<const N: usize> PartialEq<&str> for Id<N> {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl<const N: usize> PartialEq<String> for Id<N> {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl<const N: usize> serde::Serialize for Id<N> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

/// A node's key, inline: 32 bytes for a file, 16 for a folder, none for the tops. Reads as the
/// `&[u8]` it holds.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct NodeKey {
    bytes: [u8; 32],
    len: u8,
}

impl NodeKey {
    /// The first 32 bytes of `key`, at most.
    pub fn new(key: &[u8]) -> NodeKey {
        let len = key.len().min(32);
        let mut bytes = [0; 32];
        bytes[..len].copy_from_slice(&key[..len]);
        NodeKey { bytes, len: len as u8 }
    }
}

impl Deref for NodeKey {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

impl std::fmt::Debug for NodeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

/// A file or folder of the account, or of a folder shared with it.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub handle: Handle,
    /// The folder it's in; empty for the tops and for an incoming share's top folder whose
    /// parent this account can't see.
    pub parent: Handle,
    pub kind: Kind,
    pub name: String,
    pub size: u64,
    /// When it was made on MEGA, in seconds.
    pub created: i64,
    /// The file's own modification time, from its fingerprint.
    pub modified: Option<i64>,
    /// The node's key: 32 bytes for a file, 16 for a folder; empty for the tops.
    pub key: NodeKey,
    /// Its attributes other than the name (`n`), as JSON, so a rename keeps the ones it doesn't
    /// touch: see [`Node::attrs`].
    pub(crate) others: Option<Box<str>>,
    pub owner: UserHandle,
    /// The top folder of an incoming share: who shares it, and the access (0 read, 1 read and
    /// write, 2 full).
    pub inshare: Option<(UserHandle, u8)>,
    /// Its public link's handle, when it has one.
    pub link: Option<Handle>,
}

impl Node {
    /// A node with no attributes but its name, for tests and benchmarks.
    pub fn bare(handle: &str, parent: &str, kind: Kind, name: &str) -> Option<Node> {
        let (handle, parent) = (Handle::new(handle)?, Handle::new(parent)?);
        Some(Node { handle, parent, kind, name: name.into(), size: 0, created: 0, modified: None, key: NodeKey::default(), others: None, owner: UserHandle::default(), inshare: None, link: None })
    }

    /// All its attributes, the name (`n`) among them.
    pub fn attrs(&self) -> Map<String, Value> {
        let mut attrs: Map<String, Value> = self.others.as_deref().and_then(|t| serde_json::from_str(t).ok()).unwrap_or_default();
        if !self.name.is_empty() {
            attrs.insert("n".into(), self.name.clone().into());
        }
        attrs
    }

    /// Takes `attrs` as all its attributes: the name, the modification time its fingerprint
    /// holds, and the others.
    pub fn set_attrs(&mut self, mut attrs: Map<String, Value>) {
        self.name = match attrs.remove("n") {
            Some(Value::String(name)) => name,
            _ => String::new(),
        };
        self.modified = attrs.get("c").and_then(Value::as_str).and_then(crypto::fingerprint_mtime);
        self.others = (!attrs.is_empty()).then(|| serde_json::to_string(&attrs).unwrap_or_default().into_boxed_str());
    }
}

/// What it takes to decrypt nodes.
#[derive(Clone)]
pub struct Keys {
    /// The account's user handle.
    pub user: String,
    pub master: [u8; 16],
    pub rsa: Option<RsaKey>,
    /// The key of each shared folder, ours or someone else's, by its handle.
    pub shares: HashMap<String, [u8; 16]>,
}

impl Keys {
    /// A share key as MEGA sends it: 16 bytes under the master key, or RSA for an old
    /// incoming share.
    fn share_key(&self, text: &str) -> Option<[u8; 16]> {
        let mut raw = unb64(text).ok()?;
        if raw.len() == 16 {
            ecb_decrypt(&self.master, &mut raw);
            return raw.try_into().ok();
        }
        let plain = self.rsa.as_ref()?.decrypt_padded(&raw).ok()?;
        plain.get(..16)?.try_into().ok()
    }

    /// A node's key from its `k` field ("owner:key/share:key…"), by whichever key we hold.
    pub fn node_key(&self, k: &str, is_file: bool) -> Option<Vec<u8>> {
        let len = if is_file { 32 } else { 16 };
        for part in k.split('/') {
            let Some((id, key)) = part.split_once(':') else { continue };
            // Longer than a 32-byte key in base64: encrypted with our RSA key.
            if key.len() > 43 {
                if id != self.user {
                    continue;
                }
                let raw = unb64(key).ok()?;
                let plain = self.rsa.as_ref()?.decrypt_padded(&raw).ok()?;
                return plain.get(..len).map(<[u8]>::to_vec);
            }
            let with = if id == self.user { Some(self.master) } else { self.shares.get(id).copied() };
            let Some(with) = with else { continue };
            let mut raw = unb64(key).ok()?;
            if raw.len() != len {
                continue;
            }
            ecb_decrypt(&with, &mut raw);
            return Some(raw);
        }
        None
    }

    /// Takes in the share keys a node listing or a change carries: `ok` lists (ours, under the
    /// master key) and a node's own `sk`.
    pub fn learn(&mut self, f: &Value) {
        for list in ["ok", "ok0"] {
            for share in f.get(list).and_then(Value::as_array).into_iter().flatten() {
                if let (Some(h), Some(k)) = (share.get("h").and_then(Value::as_str), share.get("k").and_then(Value::as_str))
                    && let Some(key) = self.share_key(k)
                {
                    self.shares.insert(h.to_string(), key);
                }
            }
        }
    }

    pub(crate) fn learn_node(&mut self, n: &Value) {
        if let (Some(h), Some(sk)) = (n.get("h").and_then(Value::as_str), n.get("sk").and_then(Value::as_str))
            && let Some(key) = self.share_key(sk)
        {
            self.shares.insert(h.to_string(), key);
        }
    }
}

/// A node from the API's JSON, decrypted; `None` when it's of no kind we know, its handles
/// aren't MEGA's, or we hold no key that opens it.
pub fn decode(keys: &Keys, n: &Value) -> Option<Node> {
    let handle = Handle::new(n.get("h")?.as_str()?).filter(|h| !h.is_empty())?;
    let kind = Kind::of(n.get("t")?.as_i64()?)?;
    let parent = Handle::new(n.get("p").and_then(Value::as_str).unwrap_or_default())?;
    let owner = n.get("u").and_then(Value::as_str).and_then(UserHandle::new).unwrap_or_default();
    let created = n.get("ts").and_then(Value::as_i64).unwrap_or(0);
    let inshare = n.get("su").and_then(Value::as_str).map(|su| (UserHandle::new(su).unwrap_or_default(), n.get("r").and_then(Value::as_u64).unwrap_or(0).min(2) as u8));
    let size = n.get("s").and_then(Value::as_u64).unwrap_or(0);
    let mut node = Node { handle, parent, kind, name: String::new(), size, created, modified: None, key: NodeKey::default(), others: None, owner, inshare, link: None };
    if let Kind::File | Kind::Folder = kind {
        let key = keys.node_key(n.get("k")?.as_str()?, kind == Kind::File)?;
        node.set_attrs(decrypt_attrs(&attr_key(&key), n.get("a")?.as_str()?)?);
        node.key = NodeKey::new(&key);
    }
    Some(node)
}

/// No node: the end of a folder's list.
const NONE: u32 = u32::MAX;

/// A node in the tree, between its folder's other items.
struct Slot {
    node: Node,
    prev: u32,
    next: u32,
}

/// The decrypted tree.
#[derive(Default)]
pub struct Tree {
    /// The nodes; a removed one leaves its place to the next new one.
    slots: Vec<Option<Slot>>,
    free: Vec<u32>,
    /// Each node's place in `slots`.
    ids: HashMap<Handle, u32>,
    /// Each folder's first item; the rest follow through `Slot::next`. A folder that isn't in
    /// the tree (an incoming share's parent) still has its items here.
    first: HashMap<Handle, u32>,
    /// Each item by a hash of its folder and its own name (twins share one), so a path finds
    /// its item without going through the folder. A hash two nodes share is in `crowded`.
    names: HashMap<u64, u32>,
    crowded: HashMap<u64, Vec<u32>>,
    /// The incoming shares' top folders.
    tops: HashSet<Handle>,
    pub root: Option<String>,
    pub rubbish: Option<String>,
    pub vault: Option<String>,
    /// Where the change channel was read up to.
    pub sn: String,
    /// Nodes no key opened yet (their share's key may come later), as MEGA sent them.
    locked: Vec<Value>,
}

/// The `names` key of an item called `name` in folder `parent`.
fn name_key(parent: Handle, name: &str) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    parent.hash(&mut h);
    name.hash(&mut h);
    h.finish()
}

impl Tree {
    /// The tree from an `f` answer.
    pub fn from_listing(keys: &mut Keys, f: &Value) -> Tree {
        keys.learn(f);
        let list = f.get("f").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
        for n in list {
            keys.learn_node(n);
        }
        let mut tree = Tree { sn: f.get("sn").and_then(Value::as_str).unwrap_or_default().to_string(), ..Tree::default() };
        // Room for some changes after it, so the first ones don't double it.
        let room = list.len() + list.len() / 16;
        tree.slots.reserve(room);
        tree.ids.reserve(room);
        tree.names.reserve(room);
        for raw in list {
            match decode(keys, raw) {
                Some(n) => tree.put(n),
                None if locked(raw) => tree.locked.push(raw.clone()),
                None => {}
            }
        }
        for ph in f.get("ph").and_then(Value::as_array).into_iter().flatten() {
            if let (Some(h), Some(p)) = (ph.get("h").and_then(Value::as_str), ph.get("ph").and_then(Value::as_str)) {
                tree.set_link(h, Some(p.to_string()));
            }
        }
        tree
    }

    /// An empty tree with room for `n` nodes and some changes after them.
    pub fn with_capacity(n: usize) -> Tree {
        let room = n + n / 16;
        let mut tree = Tree::default();
        tree.slots.reserve(room);
        tree.ids.reserve(room);
        tree.names.reserve(room);
        tree
    }

    /// How many nodes it holds.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Every node, in no order.
    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.slots.iter().flatten().map(|s| &s.node)
    }

    /// The nodes no key opened yet, as MEGA sent them.
    pub fn locked_nodes(&self) -> &[Value] {
        &self.locked
    }

    pub(crate) fn set_locked(&mut self, locked: Vec<Value>) {
        self.locked = locked;
    }

    /// Adds or replaces a node as MEGA sent it; one no key opens waits in `locked`.
    pub fn put_raw(&mut self, keys: &Keys, raw: Value) {
        match decode(keys, &raw) {
            Some(n) => self.put(n),
            None if locked(&raw) => self.locked.push(raw),
            None => {}
        }
    }

    /// Tries again the nodes no key opened, after share keys arrived. Whether any opened.
    pub fn unlock(&mut self, keys: &Keys) -> bool {
        let before = self.locked.len();
        for raw in std::mem::take(&mut self.locked) {
            self.put_raw(keys, raw);
        }
        self.locked.len() < before
    }

    /// Whether any node no key opens hangs from one of `folders`.
    pub fn locked_within(&self, folders: &HashSet<&str>) -> bool {
        self.locked.iter().any(|raw| raw.get("p").and_then(Value::as_str).is_some_and(|p| folders.contains(p)))
    }

    fn slot(&self, id: u32) -> &Slot {
        self.slots[id as usize].as_ref().expect("a node in its place")
    }

    fn slot_mut(&mut self, id: u32) -> &mut Slot {
        self.slots[id as usize].as_mut().expect("a node in its place")
    }

    fn by(&self, h: Handle) -> Option<&Node> {
        self.ids.get(&h).map(|&id| &self.slot(id).node)
    }

    /// The places of folder `parent`'s items.
    fn kid_ids(&self, parent: Handle) -> impl Iterator<Item = u32> + '_ {
        let mut at = self.first.get(&parent).copied().unwrap_or(NONE);
        std::iter::from_fn(move || {
            let id = at;
            (id != NONE).then(|| {
                at = self.slot(id).next;
                id
            })
        })
    }

    /// Puts node `id` in the indexes: first of its folder's items, under its name.
    fn index(&mut self, id: u32) {
        let n = &self.slot(id).node;
        let (handle, parent, key, top) = (n.handle, n.parent, name_key(n.parent, &n.name), n.inshare.is_some());
        if top {
            self.tops.insert(handle);
        }
        let head = self.first.insert(parent, id).unwrap_or(NONE);
        let slot = self.slot_mut(id);
        (slot.prev, slot.next) = (NONE, head);
        if head != NONE {
            self.slot_mut(head).prev = id;
        }
        if let Some(same) = self.crowded.get_mut(&key) {
            same.push(id);
            return;
        }
        match self.names.entry(key) {
            Entry::Vacant(e) => {
                e.insert(id);
            }
            Entry::Occupied(e) => {
                let other = e.remove();
                self.crowded.insert(key, vec![other, id]);
            }
        }
    }

    /// Takes node `id` out of the indexes.
    fn unindex(&mut self, id: u32) {
        let Slot { node: n, prev, next } = self.slot(id);
        let (handle, parent, key, prev, next) = (n.handle, n.parent, name_key(n.parent, &n.name), *prev, *next);
        self.tops.remove(&handle);
        if prev != NONE {
            self.slot_mut(prev).next = next;
        } else if next != NONE {
            self.first.insert(parent, next);
        } else {
            self.first.remove(&parent);
        }
        if next != NONE {
            self.slot_mut(next).prev = prev;
        }
        if self.names.get(&key) == Some(&id) {
            self.names.remove(&key);
        } else if let Some(same) = self.crowded.get_mut(&key) {
            same.retain(|&x| x != id);
            if same.len() < 2 {
                let last = same.first().copied();
                self.crowded.remove(&key);
                if let Some(last) = last {
                    self.names.insert(key, last);
                }
            }
        }
    }

    /// Adds or replaces `n`, keeping its link and moving it in the indexes of folders.
    pub fn put(&mut self, mut n: Node) {
        match n.kind {
            Kind::Root => self.root = Some(n.handle.to_string()),
            Kind::Rubbish => self.rubbish = Some(n.handle.to_string()),
            Kind::Vault => self.vault = Some(n.handle.to_string()),
            _ => {}
        }
        if let Some(&id) = self.ids.get(&n.handle) {
            self.unindex(id);
            let old = &mut self.slot_mut(id).node;
            n.link = n.link.or(old.link);
            n.inshare = n.inshare.or(old.inshare);
            *old = n;
            self.index(id);
            return;
        }
        let handle = n.handle;
        let slot = Some(Slot { node: n, prev: NONE, next: NONE });
        let id = match self.free.pop() {
            Some(id) => {
                self.slots[id as usize] = slot;
                id
            }
            None => {
                self.slots.push(slot);
                u32::try_from(self.slots.len() - 1).expect("fewer than 4 billion nodes")
            }
        };
        self.ids.insert(handle, id);
        self.index(id);
    }

    /// Takes a node out, and with `subtree` all it holds.
    pub fn remove(&mut self, handle: &str, subtree: bool) {
        let Some(h) = Handle::new(handle) else { return };
        let mut todo = vec![h];
        while let Some(h) = todo.pop() {
            let Some(id) = self.ids.remove(&h) else { continue };
            self.unindex(id);
            self.slots[id as usize] = None;
            self.free.push(id);
            if subtree {
                todo.extend(self.kid_ids(h).map(|kid| self.slot(kid).node.handle));
            }
        }
    }

    /// Gives node `handle` a public link, or takes it away.
    pub fn set_link(&mut self, handle: &str, ph: Option<String>) {
        if let Some(&id) = Handle::new(handle).and_then(|h| self.ids.get(&h)) {
            self.slot_mut(id).node.link = ph.as_deref().and_then(Handle::new);
        }
    }

    pub fn get(&self, handle: &str) -> Option<&Node> {
        self.by(Handle::new(handle)?)
    }

    /// The items of folder `handle` with the names the panes show: unique, a twin of an
    /// earlier item named like a Windows copy. Ordered by those names.
    pub fn children(&self, handle: &str) -> Vec<(String, &Node)> {
        let mut nodes: Vec<&Node> = self.kids_of(handle).collect();
        nodes.sort_by_key(|n| (n.created, n.handle));
        unique_names(nodes)
    }

    /// The items of folder `handle`, unordered and under their own names.
    pub fn kids_of<'a>(&'a self, handle: &str) -> impl Iterator<Item = &'a Node> + use<'a> {
        Handle::new(handle).into_iter().flat_map(move |h| self.kid_ids(h)).map(move |id| &self.slot(id).node)
    }

    /// The item of folder `parent` the panes call `name`. A name no twin shares is the item's
    /// own, found without ordering the folder; only twins' names need [`Tree::children`].
    pub fn child_named(&self, parent: &str, name: &str) -> Option<&Node> {
        let mut same = self.same_name(parent, name);
        match (same.next(), same.next()) {
            (Some(one), None) => return Some(one),
            (None, _) if !twin_name(name) => return None,
            _ => {}
        }
        self.children(parent).into_iter().find(|(shown, _)| shown == name).map(|(_, n)| n)
    }

    /// The items of folder `parent` whose own name is `name`.
    pub fn same_name<'a, 'b>(&'a self, parent: &str, name: &'b str) -> impl Iterator<Item = &'a Node> + use<'a, 'b> {
        let parent = Handle::new(parent);
        let key = parent.map(|p| name_key(p, name));
        key.into_iter()
            .flat_map(move |k| self.names.get(&k).copied().into_iter().chain(self.crowded.get(&k).into_iter().flatten().copied()))
            .map(move |id| &self.slot(id).node)
            .filter(move |n| Some(n.parent) == parent && n.name == name)
    }

    /// The name node `n` shows with among its folder's items.
    pub fn shown_name(&self, n: &Node) -> String {
        if self.same_name(&n.parent, &n.name).nth(1).is_none() {
            return n.name.clone();
        }
        self.children(&n.parent).into_iter().find(|(_, x)| x.handle == n.handle).map_or_else(|| n.name.clone(), |(name, _)| name)
    }

    /// The incoming shares' top folders, with unique names.
    pub fn inshares(&self) -> Vec<(String, &Node)> {
        let mut nodes: Vec<&Node> = self.tops.iter().filter_map(|&h| self.by(h)).collect();
        nodes.sort_by_key(|n| (n.created, n.handle));
        unique_names(nodes)
    }

    /// The top this node hangs from (Cloud Drive, Rubbish Bin, the Vault or an incoming
    /// share's top folder), going up its parents.
    pub fn top_of(&self, handle: &str) -> Option<&Node> {
        let mut n = self.get(handle)?;
        let mut seen = HashSet::new();
        loop {
            if n.parent.is_empty() || n.inshare.is_some() || !seen.insert(n.handle) {
                return Some(n);
            }
            match self.by(n.parent) {
                Some(p) => n = p,
                None => return Some(n),
            }
        }
    }

    pub fn in_rubbish(&self, handle: &str) -> bool {
        self.top_of(handle).is_some_and(|t| t.kind == Kind::Rubbish)
    }

    /// Whether `inner` is `outer` or inside it.
    pub fn within(&self, inner: &str, outer: &str) -> bool {
        if inner == outer {
            return true;
        }
        let (Some(mut at), Some(outer)) = (Handle::new(inner), Handle::new(outer)) else { return false };
        let mut seen = HashSet::new();
        while seen.insert(at) {
            match self.by(at) {
                Some(n) if !n.parent.is_empty() => at = n.parent,
                _ => return false,
            }
            if at == outer {
                return true;
            }
        }
        false
    }

    /// `handle` and every folder above it, the nearest first.
    pub fn ancestry(&self, handle: &str) -> Vec<&Node> {
        let mut out = Vec::new();
        let mut at = self.get(handle);
        let mut seen = HashSet::new();
        while let Some(n) = at {
            if !seen.insert(n.handle) {
                break;
            }
            out.push(n);
            if n.parent.is_empty() {
                break;
            }
            at = self.by(n.parent);
        }
        out
    }

    /// Everything inside folder `handle`, the folder first and each item after its folder.
    pub fn subtree(&self, handle: &str) -> Vec<&Node> {
        let mut out = Vec::new();
        let Some(h) = Handle::new(handle) else { return out };
        let mut todo = vec![h];
        let mut seen = HashSet::new();
        while let Some(h) = todo.pop() {
            if !seen.insert(h) {
                continue;
            }
            if let Some(n) = self.by(h) {
                out.push(n);
            }
            todo.extend(self.kid_ids(h).map(|id| self.slot(id).node.handle));
        }
        out
    }

    /// The bytes the files inside folder `handle` hold.
    pub fn bytes_in(&self, handle: &str) -> u64 {
        self.subtree(handle).iter().filter(|n| n.kind == Kind::File).map(|n| n.size).sum()
    }
}

/// Whether a node no key opened may open later: a file or folder.
fn locked(raw: &Value) -> bool {
    matches!(raw.get("t").and_then(Value::as_i64), Some(0 | 1))
}

/// Whether `name` is a shown name a twin of `real` gets: "stem (k).ext".
pub fn twin_of(name: &str, real: &str, is_file: bool) -> bool {
    let (stem, ext) = match real.rfind('.') {
        Some(i) if i > 0 && is_file => real.split_at(i),
        _ => (real, ""),
    };
    let Some(mid) = name.strip_prefix(stem).and_then(|r| r.strip_prefix(" (")).and_then(|r| r.strip_suffix(ext)).and_then(|r| r.strip_suffix(')')) else { return false };
    !mid.is_empty() && mid.chars().all(|c| c.is_ascii_digit())
}

/// Whether `name` could be a twin's shown name: "stem (k)" before any extension.
fn twin_name(name: &str) -> bool {
    let Some(open) = name.rfind(" (") else { return false };
    let rest = &name[open + 2..];
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && rest[digits..].starts_with(')')
}

/// Each node's name, a later twin getting "name (2).ext" (the first free number, never another
/// item's real name).
fn unique_names(nodes: Vec<&Node>) -> Vec<(String, &Node)> {
    let real: HashSet<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
    let mut taken: HashSet<String> = HashSet::new();
    let mut out: Vec<(String, &Node)> = Vec::with_capacity(nodes.len());
    for n in nodes {
        let name = if taken.contains(&n.name) {
            let (stem, ext) = match n.name.rfind('.') {
                Some(i) if i > 0 && n.kind == Kind::File => n.name.split_at(i),
                _ => (n.name.as_str(), ""),
            };
            (2..).map(|k| format!("{stem} ({k}){ext}")).find(|c| !taken.contains(c) && !real.contains(c.as_str())).expect("some number is free")
        } else {
            n.name.clone()
        };
        taken.insert(name.clone());
        out.push((name, n));
    }
    out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()).then_with(|| a.0.cmp(&b.0)));
    out
}

impl RsaKey {
    /// RSA as MEGA's web client reads it: the plaintext padded to the key's length, less the
    /// two leading bytes (one, for keys from an old bug).
    pub fn decrypt_padded(&self, data: &[u8]) -> crate::Result<Vec<u8>> {
        let plain = self.decrypt_mpi(data)?;
        let len = self.size();
        let mut padded = vec![0u8; len.saturating_sub(plain.len())];
        padded.extend(plain);
        if padded.get(1).is_some_and(|&b| b != 0) {
            padded.insert(0, 0);
        }
        Ok(padded.get(2..).unwrap_or_default().to_vec())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::crypto::{b64, ecb_encrypt, encrypt_attrs, merge_file_key, random};
    use serde_json::json;

    pub fn keys() -> Keys {
        Keys { user: "USER0000000".into(), master: [7u8; 16], rsa: None, shares: HashMap::new() }
    }

    /// A handle for tests.
    pub fn h(text: &str) -> Handle {
        Handle::new(text).expect("a handle")
    }

    /// A node as MEGA sends it, its key under `with` (the master key, or a share's).
    pub fn raw(keys: &Keys, h: &str, p: &str, t: i64, name: &str, owner_key: (&str, [u8; 16])) -> Value {
        let mut attrs = Map::new();
        attrs.insert("n".into(), name.into());
        raw_with(keys, h, p, t, attrs, owner_key)
    }

    /// [`raw`] with attributes of its own.
    pub fn raw_with(keys: &Keys, h: &str, p: &str, t: i64, attrs: Map<String, Value>, owner_key: (&str, [u8; 16])) -> Value {
        let key: Vec<u8> = if t == 0 { merge_file_key(&random::<16>(), &random::<8>(), &[0; 8]).to_vec() } else { random::<16>().to_vec() };
        let a = encrypt_attrs(&crypto::attr_key(&key), &attrs);
        let mut k = key.clone();
        ecb_encrypt(&owner_key.1, &mut k);
        let _ = keys;
        json!({"h": h, "p": p, "t": t, "a": a, "k": format!("{}:{}", owner_key.0, b64(&k)), "u": "USER0000000", "ts": 100, "s": if t == 0 { 10 } else { 0 }})
    }

    pub fn listing(keys: &Keys) -> Value {
        let own = (keys.user.as_str(), keys.master);
        json!({
            "f": [
                {"h": "ROOT0000", "p": "", "t": 2, "a": "", "u": "USER0000000", "ts": 1},
                {"h": "BIN00000", "p": "", "t": 4, "a": "", "u": "USER0000000", "ts": 1},
                raw(keys, "FOLDER01", "ROOT0000", 1, "Fotos", own),
                raw(keys, "FILE0001", "FOLDER01", 0, "praia.jpg", own),
                raw(keys, "FILE0002", "ROOT0000", 0, "nota.txt", own),
                raw(keys, "TRASHED1", "BIN00000", 0, "velho.txt", own),
            ],
            "ph": [{"h": "FILE0002", "ph": "PUBH0001"}],
            "sn": "SN000000001",
        })
    }

    #[test]
    fn a_listing_decrypts_into_a_tree() {
        let mut k = keys();
        let tree = { let l = listing(&k); Tree::from_listing(&mut k, &l) };
        assert_eq!(tree.root.as_deref(), Some("ROOT0000"));
        assert_eq!(tree.rubbish.as_deref(), Some("BIN00000"));
        assert_eq!(tree.sn, "SN000000001");
        let top: Vec<String> = tree.children("ROOT0000").into_iter().map(|(n, _)| n).collect();
        assert_eq!(top, ["Fotos", "nota.txt"]);
        assert_eq!(tree.get("FILE0002").unwrap().link.as_deref(), Some("PUBH0001"));
        assert_eq!(tree.get("FILE0002").unwrap().owner, "USER0000000");
        assert!(tree.in_rubbish("TRASHED1"));
        assert!(!tree.in_rubbish("FILE0001"));
        assert!(tree.within("FILE0001", "ROOT0000"));
        assert!(tree.within("FILE0001", "FILE0001") && !tree.within("ROOT0000", "FILE0001") && !tree.within("FILE0001", "BIN00000"));
        assert_eq!(tree.ancestry("FILE0001").iter().map(|n| n.handle.as_str()).collect::<Vec<_>>(), ["FILE0001", "FOLDER01", "ROOT0000"]);
        assert_eq!(tree.bytes_in("ROOT0000"), 20);
        assert!(tree.get("TOO-LONG-HANDLE").is_none() && tree.get("").is_none());
    }

    #[test]
    fn handles_read_and_order_as_their_text() {
        assert_eq!(h("ABCDEFGH").as_str(), "ABCDEFGH");
        assert_eq!(Handle::default().as_str(), "");
        assert!(Handle::new("ABCDEFGHI").is_none() && Handle::new("ção").is_none());
        let mut v = [h("b"), h("ab"), h("a"), Handle::default(), h("B")];
        v.sort();
        assert_eq!(v.map(|x| x.to_string()), ["", "B", "a", "ab", "b"]);
        assert_eq!(serde_json::to_string(&h("ROOT0000")).unwrap(), "\"ROOT0000\"");
    }

    #[test]
    fn attributes_besides_the_name_are_kept() {
        let k = keys();
        let mut attrs = Map::new();
        attrs.insert("n".into(), "a.txt".into());
        attrs.insert("c".into(), crypto::fingerprint(&[1; 16], 1_700_000_000).into());
        attrs.insert("lbl".into(), 3.into());
        let mut tree = Tree::default();
        tree.put_raw(&k, raw_with(&k, "AAAAAAAA", "ROOT0000", 0, attrs.clone(), (k.user.as_str(), k.master)));
        let n = tree.get("AAAAAAAA").unwrap();
        assert_eq!((n.name.as_str(), n.modified, n.key.len()), ("a.txt", Some(1_700_000_000), 32));
        assert_eq!(n.attrs(), attrs);
        let mut renamed = n.clone();
        let mut new = n.attrs();
        new.insert("n".into(), "b.txt".into());
        renamed.set_attrs(new.clone());
        assert_eq!((renamed.name.as_str(), renamed.attrs()), ("b.txt", new));
        let mut bare = Node::bare("BBBBBBBB", "ROOT0000", Kind::Folder, "x").unwrap();
        assert_eq!(bare.attrs().len(), 1);
        bare.set_attrs(Map::new());
        assert!(bare.attrs().is_empty() && bare.name.is_empty());
    }

    #[test]
    fn twins_get_names_of_their_own() {
        let k = keys();
        let own = (k.user.as_str(), k.master);
        let mut tree = Tree::default();
        let mut a = raw(&k, "AAAAAAAA", "ROOT0000", 0, "a.txt", own);
        let mut b = raw(&k, "BBBBBBBB", "ROOT0000", 0, "a.txt", own);
        let c = raw(&k, "CCCCCCCC", "ROOT0000", 0, "a (2).txt", own);
        a["ts"] = 5.into();
        b["ts"] = 9.into();
        for n in [b, c, a] {
            tree.put_raw(&k, n);
        }
        let names: Vec<(String, String)> = tree.children("ROOT0000").into_iter().map(|(n, node)| (n, node.handle.to_string())).collect();
        // The oldest keeps the name; the twin skips the real "a (2).txt".
        assert_eq!(names, [("a (2).txt".to_string(), "CCCCCCCC".to_string()), ("a (3).txt".into(), "BBBBBBBB".into()), ("a.txt".into(), "AAAAAAAA".into())]);
    }

    #[test]
    fn twins_are_found_by_their_shown_names_and_others_directly() {
        let k = keys();
        let own = (k.user.as_str(), k.master);
        let mut tree = Tree::default();
        let mut a = raw(&k, "AAAAAAAA", "ROOT0000", 0, "a.txt", own);
        let mut b = raw(&k, "BBBBBBBB", "ROOT0000", 0, "a.txt", own);
        a["ts"] = 5.into();
        b["ts"] = 9.into();
        for n in [b, a, raw(&k, "CCCCCCCC", "ROOT0000", 0, "c (1).txt", own)] {
            tree.put_raw(&k, n);
        }
        let h = |n: Option<&Node>| n.map(|n| n.handle.to_string());
        assert_eq!(h(tree.child_named("ROOT0000", "a.txt")), Some("AAAAAAAA".into()));
        assert_eq!(h(tree.child_named("ROOT0000", "a (2).txt")), Some("BBBBBBBB".into()));
        assert_eq!(h(tree.child_named("ROOT0000", "c (1).txt")), Some("CCCCCCCC".into()));
        assert_eq!(h(tree.child_named("ROOT0000", "b.txt")), None);
        assert_eq!(h(tree.child_named("ROOT0000", "a (9).txt")), None);
        assert_eq!(tree.shown_name(tree.get("BBBBBBBB").unwrap()), "a (2).txt");
        assert_eq!(tree.shown_name(tree.get("CCCCCCCC").unwrap()), "c (1).txt");
        assert!(twin_name("x (2)") && twin_name("x (12).tar.gz") && !twin_name("x (a)") && !twin_name("x"));
        // One twin goes: the other is found directly again.
        tree.remove("AAAAAAAA", true);
        assert_eq!(tree.same_name("ROOT0000", "a.txt").count(), 1);
        assert_eq!(h(tree.child_named("ROOT0000", "a.txt")), Some("BBBBBBBB".into()));
    }

    /// Items whose folder and name hash alike are told apart by the nodes themselves.
    #[test]
    fn a_shared_hash_still_finds_each_item() {
        let mut tree = Tree::default();
        let ids = ["AAAAAAAA", "BBBBBBBB", "CCCCCCCC"];
        for (i, h) in ids.into_iter().enumerate() {
            tree.put(Node::bare(h, "ROOT0000", Kind::File, &format!("{i}.txt")).unwrap());
        }
        // As if all three hashed to one key.
        let key = name_key(super::tests::h("ROOT0000"), "0.txt");
        tree.names.clear();
        tree.crowded.insert(key, vec![0, 1, 2]);
        assert_eq!(tree.same_name("ROOT0000", "0.txt").map(|n| n.handle.to_string()).collect::<Vec<_>>(), ["AAAAAAAA"]);
        assert_eq!(tree.same_name("ROOT0000", "1.txt").count(), 0, "another key, not in the index");
        // The node that leaves takes only itself out; one left alone goes back to `names`.
        tree.crowded.insert(key, vec![0, 1]);
        tree.remove("AAAAAAAA", false);
        assert_eq!(tree.names.get(&key), Some(&1));
        assert!(tree.crowded.is_empty());
    }

    #[test]
    fn shared_items_open_with_their_share_key_once_it_comes() {
        let mut k = keys();
        let share_key = [9u8; 16];
        let item = raw(&k, "SHARED01", "INSHARE1", 0, "deles.pdf", ("INSHARE1", share_key));
        let mut tree = Tree::default();
        tree.put_raw(&k, item);
        assert!(tree.get("SHARED01").is_none());
        k.shares.insert("INSHARE1".into(), share_key);
        assert!(tree.unlock(&k));
        assert_eq!(tree.get("SHARED01").unwrap().name, "deles.pdf");
        assert_eq!(tree.kids_of("INSHARE1").count(), 1, "a folder not in the tree still lists its items");
    }

    #[test]
    fn share_keys_from_ok_lists_and_sk() {
        let mut k = keys();
        let mut sk = [3u8; 16];
        ecb_encrypt(&k.master, &mut sk);
        k.learn(&json!({"ok": [{"h": "OUTSHARE", "k": b64(&sk), "ha": "x"}]}));
        assert_eq!(k.shares.get("OUTSHARE"), Some(&[3u8; 16]));
        k.learn_node(&json!({"h": "OTHER001", "sk": b64(&sk)}));
        assert_eq!(k.shares.get("OTHER001"), Some(&[3u8; 16]));
    }

    #[test]
    fn moving_and_removing_keep_the_index_right() {
        let mut k = keys();
        let mut tree = { let l = listing(&k); Tree::from_listing(&mut k, &l) };
        let mut moved = tree.get("FILE0002").unwrap().clone();
        moved.parent = h("FOLDER01");
        tree.put(moved);
        assert_eq!(tree.children("ROOT0000").len(), 1);
        assert_eq!(tree.children("FOLDER01").len(), 2);
        assert_eq!(tree.get("FILE0002").unwrap().link.as_deref(), Some("PUBH0001"), "a link stays through a move");
        // A rename moves the item in the index of names.
        let mut renamed = tree.get("FILE0002").unwrap().clone();
        renamed.name = "novo.txt".into();
        tree.put(renamed);
        assert!(tree.child_named("FOLDER01", "nota.txt").is_none());
        assert_eq!(tree.child_named("FOLDER01", "novo.txt").map(|n| n.handle.as_str()), Some("FILE0002"));
        tree.remove("FOLDER01", true);
        assert!(tree.get("FILE0001").is_none() && tree.get("FILE0002").is_none());
        assert!(tree.children("ROOT0000").is_empty());
        assert_eq!(tree.ids.len(), 3, "only the tops and the trashed file are left");
        // A removed node's place goes to the next one.
        tree.put(Node::bare("NEWNODE1", "ROOT0000", Kind::Folder, "Nova").unwrap());
        assert_eq!(tree.slots.len(), 6);
        assert_eq!(tree.children("ROOT0000").len(), 1);
    }

    /// Items of a big folder taken out one by one, from the middle, the head and the tail of
    /// the folder's list, leave the rest listed.
    #[test]
    fn removing_items_one_by_one_keeps_the_rest() {
        let mut tree = Tree::default();
        tree.put(Node::bare("ROOT0000", "", Kind::Root, "").unwrap());
        for i in 0..100 {
            tree.put(Node::bare(&format!("H{i:07}"), "ROOT0000", Kind::File, &format!("{i}.txt")).unwrap());
        }
        for i in [50, 99, 0, 1, 98, 51] {
            tree.remove(&format!("H{i:07}"), false);
        }
        let left: HashSet<String> = tree.kids_of("ROOT0000").map(|n| n.name.clone()).collect();
        assert_eq!(left.len(), 94);
        assert!(!left.contains("50.txt") && !left.contains("99.txt") && left.contains("49.txt"));
        assert!(tree.child_named("ROOT0000", "52.txt").is_some() && tree.child_named("ROOT0000", "51.txt").is_none());
    }
}
