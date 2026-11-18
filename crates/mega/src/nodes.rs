//! The account's files and folders as MEGA's `f` command gives them, decrypted, with the
//! changes the `sc` channel brings applied as they come (see [`crate::changes`]).
//!
//! MEGA lets two items in a folder share a name. The panes reach items by name, so a listing
//! gives each such twin a name of its own the way Windows names a copy ("Fotos (2)"), the oldest
//! keeping the real name: see [`Tree::children`].

use std::collections::{HashMap, HashSet};

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

/// A file or folder of the account, or of a folder shared with it.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub handle: String,
    /// The folder it's in; empty for the tops and for an incoming share's top folder whose
    /// parent this account can't see.
    pub parent: String,
    pub kind: Kind,
    pub name: String,
    pub size: u64,
    /// When it was made on MEGA, in seconds.
    pub created: i64,
    /// The file's own modification time, from its fingerprint.
    pub modified: Option<i64>,
    /// The node's key: 32 bytes for a file, 16 for a folder; empty for the tops.
    pub key: Vec<u8>,
    /// All its attributes, so a rename keeps the ones it doesn't touch.
    pub attrs: Map<String, Value>,
    pub owner: String,
    /// The top folder of an incoming share: who shares it, and the access (0 read, 1 read and
    /// write, 2 full).
    pub inshare: Option<(String, u8)>,
    /// Its public link's handle, when it has one.
    pub link: Option<String>,
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

/// A node from the API's JSON, decrypted; `None` when it's of no kind we know or we hold no
/// key that opens it.
pub fn decode(keys: &Keys, n: &Value) -> Option<Node> {
    let handle = n.get("h")?.as_str()?.to_string();
    let kind = Kind::of(n.get("t")?.as_i64()?)?;
    let parent = n.get("p").and_then(Value::as_str).unwrap_or_default().to_string();
    let owner = n.get("u").and_then(Value::as_str).unwrap_or_default().to_string();
    let created = n.get("ts").and_then(Value::as_i64).unwrap_or(0);
    let inshare = n.get("su").and_then(Value::as_str).map(|su| (su.to_string(), n.get("r").and_then(Value::as_u64).unwrap_or(0).min(2) as u8));
    let (name, key, attrs) = match kind {
        Kind::File | Kind::Folder => {
            let key = keys.node_key(n.get("k")?.as_str()?, kind == Kind::File)?;
            let attrs = decrypt_attrs(&attr_key(&key), n.get("a")?.as_str()?)?;
            let name = attrs.get("n").and_then(Value::as_str).unwrap_or_default().to_string();
            (name, key, attrs)
        }
        _ => (String::new(), Vec::new(), Map::new()),
    };
    let modified = attrs.get("c").and_then(Value::as_str).and_then(crypto::fingerprint_mtime);
    let size = n.get("s").and_then(Value::as_u64).unwrap_or(0);
    Some(Node { handle, parent, kind, name, size, created, modified, key, attrs, owner, inshare, link: None })
}

/// The decrypted tree.
#[derive(Default)]
pub struct Tree {
    pub nodes: HashMap<String, Node>,
    /// Each folder's items, by handle.
    kids: HashMap<String, Vec<String>>,
    pub root: Option<String>,
    pub rubbish: Option<String>,
    pub vault: Option<String>,
    /// Where the change channel was read up to.
    pub sn: String,
    /// Nodes no key opened yet (their share's key may come later), as MEGA sent them.
    locked: Vec<Value>,
}

impl Tree {
    /// The tree from an `f` answer.
    pub fn from_listing(keys: &mut Keys, f: &Value) -> Tree {
        keys.learn(f);
        let list = f.get("f").and_then(Value::as_array).cloned().unwrap_or_default();
        for n in &list {
            keys.learn_node(n);
        }
        let mut tree = Tree { sn: f.get("sn").and_then(Value::as_str).unwrap_or_default().to_string(), ..Tree::default() };
        for n in list {
            tree.put_raw(keys, n);
        }
        for ph in f.get("ph").and_then(Value::as_array).into_iter().flatten() {
            if let (Some(h), Some(p)) = (ph.get("h").and_then(Value::as_str), ph.get("ph").and_then(Value::as_str))
                && let Some(n) = tree.nodes.get_mut(h)
            {
                n.link = Some(p.to_string());
            }
        }
        tree
    }

    /// Adds or replaces a node as MEGA sent it; one no key opens waits in `locked`.
    pub fn put_raw(&mut self, keys: &Keys, raw: Value) {
        match decode(keys, &raw) {
            Some(n) => self.put(n),
            None if matches!(raw.get("t").and_then(Value::as_i64), Some(0 | 1)) => self.locked.push(raw),
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

    /// Adds or replaces `n`, keeping its link and moving it in the index of folders.
    pub fn put(&mut self, mut n: Node) {
        match n.kind {
            Kind::Root => self.root = Some(n.handle.clone()),
            Kind::Rubbish => self.rubbish = Some(n.handle.clone()),
            Kind::Vault => self.vault = Some(n.handle.clone()),
            _ => {}
        }
        if let Some(old) = self.nodes.get(&n.handle) {
            if n.link.is_none() {
                n.link = old.link.clone();
            }
            if n.inshare.is_none() {
                n.inshare = old.inshare.clone();
            }
            let old_parent = old.parent.clone();
            if old_parent != n.parent
                && let Some(v) = self.kids.get_mut(&old_parent)
            {
                v.retain(|h| *h != n.handle);
            }
        }
        let list = self.kids.entry(n.parent.clone()).or_default();
        if !list.contains(&n.handle) {
            list.push(n.handle.clone());
        }
        self.nodes.insert(n.handle.clone(), n);
    }

    /// Takes a node out, and with `subtree` all it holds.
    pub fn remove(&mut self, handle: &str, subtree: bool) {
        let Some(n) = self.nodes.remove(handle) else { return };
        if let Some(v) = self.kids.get_mut(&n.parent) {
            v.retain(|h| h != handle);
        }
        if subtree {
            for kid in self.kids.remove(handle).unwrap_or_default() {
                self.remove(&kid, true);
            }
        }
    }

    /// Gives node `handle` a public link, or takes it away.
    pub fn set_link(&mut self, handle: &str, ph: Option<String>) {
        if let Some(n) = self.nodes.get_mut(handle) {
            n.link = ph;
        }
    }

    pub fn get(&self, handle: &str) -> Option<&Node> {
        self.nodes.get(handle)
    }

    /// The items of folder `handle` with the names the panes show: unique, a twin of an
    /// earlier item named like a Windows copy. Ordered by those names.
    pub fn children(&self, handle: &str) -> Vec<(String, &Node)> {
        let mut nodes: Vec<&Node> = self.kids.get(handle).into_iter().flatten().filter_map(|h| self.nodes.get(h)).collect();
        nodes.sort_by(|a, b| (a.created, &a.handle).cmp(&(b.created, &b.handle)));
        unique_names(nodes)
    }

    /// The incoming shares' top folders, with unique names.
    pub fn inshares(&self) -> Vec<(String, &Node)> {
        let mut nodes: Vec<&Node> = self.nodes.values().filter(|n| n.inshare.is_some()).collect();
        nodes.sort_by(|a, b| (a.created, &a.handle).cmp(&(b.created, &b.handle)));
        unique_names(nodes)
    }

    /// The top this node hangs from (Cloud Drive, Rubbish Bin, the Vault or an incoming
    /// share's top folder), going up its parents.
    pub fn top_of<'a>(&'a self, mut handle: &'a str) -> Option<&'a Node> {
        let mut seen = HashSet::new();
        loop {
            let n = self.nodes.get(handle)?;
            if n.parent.is_empty() || n.inshare.is_some() || !self.nodes.contains_key(&n.parent) || !seen.insert(handle) {
                return Some(n);
            }
            handle = &n.parent;
        }
    }

    pub fn in_rubbish(&self, handle: &str) -> bool {
        self.top_of(handle).is_some_and(|t| t.kind == Kind::Rubbish)
    }

    /// Whether `inner` is `outer` or inside it.
    pub fn within(&self, inner: &str, outer: &str) -> bool {
        let mut at = inner;
        let mut seen = HashSet::new();
        while seen.insert(at.to_string()) {
            if at == outer {
                return true;
            }
            match self.nodes.get(at) {
                Some(n) if !n.parent.is_empty() => at = &n.parent,
                _ => return false,
            }
        }
        false
    }

    /// `handle` and every folder above it, the nearest first.
    pub fn ancestry(&self, handle: &str) -> Vec<&Node> {
        let mut out = Vec::new();
        let mut at = handle;
        let mut seen = HashSet::new();
        while let Some(n) = self.nodes.get(at) {
            if !seen.insert(at) {
                break;
            }
            out.push(n);
            if n.parent.is_empty() {
                break;
            }
            at = &n.parent;
        }
        out
    }

    /// Everything inside folder `handle`, the folder first and each item after its folder.
    pub fn subtree(&self, handle: &str) -> Vec<&Node> {
        let mut out = Vec::new();
        let mut todo = vec![handle.to_string()];
        let mut seen = HashSet::new();
        while let Some(h) = todo.pop() {
            if !seen.insert(h.clone()) {
                continue;
            }
            if let Some(n) = self.nodes.get(&h) {
                out.push(n);
            }
            todo.extend(self.kids.get(&h).into_iter().flatten().cloned());
        }
        out
    }

    /// The bytes the files inside folder `handle` hold.
    pub fn bytes_in(&self, handle: &str) -> u64 {
        self.subtree(handle).iter().filter(|n| n.kind == Kind::File).map(|n| n.size).sum()
    }
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

    /// A node as MEGA sends it, its key under `with` (the master key, or a share's).
    pub fn raw(keys: &Keys, h: &str, p: &str, t: i64, name: &str, owner_key: (&str, [u8; 16])) -> Value {
        let key: Vec<u8> = if t == 0 { merge_file_key(&random::<16>(), &random::<8>(), &[0; 8]).to_vec() } else { random::<16>().to_vec() };
        let mut attrs = Map::new();
        attrs.insert("n".into(), name.into());
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
        assert!(tree.in_rubbish("TRASHED1"));
        assert!(!tree.in_rubbish("FILE0001"));
        assert!(tree.within("FILE0001", "ROOT0000"));
        assert_eq!(tree.bytes_in("ROOT0000"), 20);
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
        let names: Vec<(String, String)> = tree.children("ROOT0000").into_iter().map(|(n, node)| (n, node.handle.clone())).collect();
        // The oldest keeps the name; the twin skips the real "a (2).txt".
        assert_eq!(names, [("a (2).txt".to_string(), "CCCCCCCC".to_string()), ("a (3).txt".into(), "BBBBBBBB".into()), ("a.txt".into(), "AAAAAAAA".into())]);
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
        moved.parent = "FOLDER01".into();
        tree.put(moved);
        assert_eq!(tree.children("ROOT0000").len(), 1);
        assert_eq!(tree.children("FOLDER01").len(), 2);
        assert_eq!(tree.get("FILE0002").unwrap().link.as_deref(), Some("PUBH0001"), "a link stays through a move");
        tree.remove("FOLDER01", true);
        assert!(tree.get("FILE0001").is_none() && tree.get("FILE0002").is_none());
        assert!(tree.children("ROOT0000").is_empty());
    }
}
