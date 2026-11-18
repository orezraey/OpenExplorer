//! A logged-in account: its session, its decrypted tree (kept current by [`crate::changes`]) and
//! the changes the panes make, each applied to the tree as soon as MEGA confirms it, so a
//! folder made and then filled at once finds itself.
//!
//! One `Account` per e-mail is open at a time, shared by every pane and transfer (see
//! [`reach`]): MEGA's whole tree comes in one answer, read once.
//!
//! The panes reach items by path. The account's top holds three folders, Cloud Drive, Rubbish
//! Bin and Incoming shares, named in the interface's language (either language is understood);
//! twins in a folder have names of their own ([`crate::nodes::Tree::children`]). A path segment
//! `\0<handle>` names a node itself, wherever it is.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use openexplorer_i18n::{t, tr};
use serde_json::{Map, Value, json};

use crate::api::{Api, Fail, request_id};
use crate::crypto::{attr_key, b64, ecb_encrypt, encrypt_attrs, random};
use crate::nodes::{Keys, Kind, Node, Tree};
use crate::session::{self, Refused, Session};
use crate::{Result, store};

/// The account's three top folders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Area {
    Cloud,
    Rubbish,
    Incoming,
}

impl Area {
    pub const ALL: [Area; 3] = [Area::Cloud, Area::Rubbish, Area::Incoming];

    /// Its name in the interface's language.
    pub fn label(self) -> &'static str {
        match self {
            Area::Cloud => t("Drive na nuvem", "Cloud Drive"),
            Area::Rubbish => t("Lixeira", "Rubbish Bin"),
            Area::Incoming => t("Compartilhados comigo", "Incoming shares"),
        }
    }

    fn named(seg: &str) -> Option<Area> {
        Area::ALL.into_iter().find(|a| match a {
            Area::Cloud => seg == "Drive na nuvem" || seg == "Cloud Drive",
            Area::Rubbish => seg == "Lixeira" || seg == "Rubbish Bin",
            Area::Incoming => seg == "Compartilhados comigo" || seg == "Incoming shares",
        })
    }
}

/// What a path names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// The account's top, holding the three areas.
    Top,
    /// Incoming shares itself: the folders others share, not a folder of its own.
    Shares,
    Node(String),
}

/// An item of a listing.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// Unique in its folder (see the module docs).
    pub name: String,
    pub handle: Option<String>,
    pub is_dir: bool,
    pub size: Option<u64>,
    /// Seconds since 1970: the file's own time, else when it reached MEGA.
    pub modified: Option<i64>,
}

impl Item {
    fn of(name: String, n: &Node) -> Item {
        let is_dir = n.kind.is_dir();
        Item { name, handle: Some(n.handle.clone()), is_dir, size: (!is_dir).then_some(n.size), modified: n.modified.or(Some(n.created).filter(|t| *t > 0)) }
    }

    fn area(a: Area) -> Item {
        Item { name: a.label().into(), handle: None, is_dir: true, size: None, modified: None }
    }
}

pub(crate) struct Inner {
    pub keys: Keys,
    pub tree: Tree,
    /// Something changed that only a fresh listing shows (a new share, new keys).
    pub refetch: bool,
}

pub struct Account {
    pub email: String,
    pub(crate) api: Api,
    pub(crate) session: Session,
    pub(crate) inner: Mutex<Inner>,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account").field("email", &self.email).finish_non_exhaustive()
    }
}

/// The paths' segments, without empty ones.
fn segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

/// A path that names node `handle` itself.
pub fn handle_path(handle: &str) -> String {
    format!("/\0{handle}")
}

impl Account {
    /// An account from a session: its tree is read here.
    pub fn start(email: &str, api: Api, session: Session) -> Result<Account> {
        let keys = Keys { user: session.user.clone(), master: session.master, rsa: session.rsa.clone(), shares: HashMap::new() };
        let account = Account { email: session::normal_email(email), api, session, inner: Mutex::new(Inner { keys, tree: Tree::default(), refetch: false }) };
        account.fetch()?;
        Ok(account)
    }

    /// An account over a tree given whole, for tests.
    #[cfg(test)]
    pub(crate) fn offline(keys: Keys, tree: Tree) -> Account {
        let session = Session { user: keys.user.clone(), master: keys.master, sek: [0; 16], sid: String::new(), rsa: None };
        Account { email: "ana@exemplo.com".into(), api: Api::new("http://127.0.0.1:9"), session, inner: Mutex::new(Inner { keys, tree, refetch: false }) }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reads the whole tree again, with the share keys the account keeps.
    pub fn fetch(&self) -> Result<()> {
        let mut keys = self.lock().keys.clone();
        // Accounts since 2023 keep share keys in an attribute of their own; older ones have
        // none (ENOENT).
        match self.api.call(json!({"a": "uga", "u": keys.user, "ua": "^!keys", "v": 1})) {
            Ok(v) => match v.get("av").and_then(Value::as_str).map(crate::crypto::unb64) {
                Some(Ok(raw)) => match crate::crypto::share_keys(&keys.master, &raw) {
                    Ok(found) => keys.shares.extend(found),
                    Err(e) => crate::api::log(&format!("MEGA ^!keys: {e}")),
                },
                _ => crate::api::log("MEGA ^!keys: sem valor"),
            },
            Err(Fail::Code(crate::api::ENOENT)) => {}
            Err(f) => crate::api::log(&format!("MEGA ^!keys: {}", f.text())),
        }
        let listing = self.api.run(json!({"a": "f", "c": 1, "r": 1}))?;
        let tree = Tree::from_listing(&mut keys, &listing);
        if tree.root.is_none() {
            return Err(t("O MEGA não mandou a pasta principal da conta.", "MEGA didn't send the account's main folder.").into());
        }
        let mut inner = self.lock();
        *inner = Inner { keys, tree, refetch: false };
        Ok(())
    }

    /// The session's text to keep (see [`Session::save`]).
    pub fn saved(&self) -> String {
        self.session.save()
    }

    /// Ends the session on MEGA's side.
    pub fn logout(&self) {
        let _ = self.api.call(json!({"a": "sml"}));
    }

    // -----------------------------------------------------------------------------------------
    // Paths

    /// What `path` names, or `None` when nothing's there.
    pub fn resolve(&self, path: &str) -> Option<Place> {
        resolve(&self.lock().tree, path)
    }

    fn node_at(&self, path: &str) -> Result<Node> {
        let inner = self.lock();
        match resolve(&inner.tree, path) {
            Some(Place::Node(h)) => Ok(inner.tree.get(&h).cloned().expect("a resolved node")),
            Some(_) => Err(t("Essa pasta do MEGA não pode ser alterada.", "That MEGA folder can't be changed.").into()),
            None => Err(tr!("“{}” não está mais no MEGA.", "“{}” is no longer on MEGA.", last_name(path))),
        }
    }

    /// The handle of the folder `path` names, for the panes to tell whether a change touches it.
    pub fn folder_handle(&self, path: &str) -> Option<String> {
        match self.resolve(path)? {
            Place::Node(h) => Some(h),
            _ => None,
        }
    }

    /// A folder's items (the top's are the three areas).
    pub fn list(&self, path: &str) -> Result<Vec<Item>> {
        let inner = self.lock();
        let tree = &inner.tree;
        match resolve(tree, path) {
            Some(Place::Top) => Ok(Area::ALL.into_iter().map(Item::area).collect()),
            Some(Place::Shares) => Ok(tree.inshares().into_iter().map(|(name, n)| Item::of(name, n)).collect()),
            Some(Place::Node(h)) => match tree.get(&h) {
                Some(n) if n.kind.is_dir() => Ok(tree.children(&h).into_iter().map(|(name, n)| Item::of(name, n)).collect()),
                _ => Err(tr!("“{}” é um arquivo, não uma pasta.", "“{}” is a file, not a folder.", last_name(path))),
            },
            None => Err(tr!("A pasta “{}” não está mais no MEGA.", "The folder “{}” is no longer on MEGA.", last_name(path))),
        }
    }

    pub fn stat(&self, path: &str) -> Option<Item> {
        let inner = self.lock();
        let tree = &inner.tree;
        match resolve(tree, path)? {
            Place::Top => Some(Item { name: String::new(), handle: None, is_dir: true, size: None, modified: None }),
            Place::Shares => Some(Item::area(Area::Incoming)),
            Place::Node(h) => {
                let n = tree.get(&h)?;
                let name = shown_name(tree, n);
                Some(Item::of(name, n))
            }
        }
    }

    /// The node at `path`, for the app's own calls (links, restore).
    pub fn node(&self, path: &str) -> Result<Node> {
        self.node_at(path)
    }

    /// Whether `path` is in the Rubbish Bin (and so deleting there is for good).
    pub fn in_rubbish(&self, path: &str) -> bool {
        let inner = self.lock();
        match resolve(&inner.tree, path) {
            Some(Place::Node(h)) => inner.tree.in_rubbish(&h),
            _ => false,
        }
    }

    /// The folder `path`'s parent names, and the last segment: where something new goes.
    pub(crate) fn parent_and_name(&self, path: &str) -> Result<(Node, String)> {
        let (dir, name) = split(path);
        if name.is_empty() || name.starts_with('\0') {
            return Err(t("Falta o nome.", "The name is missing.").into());
        }
        check_name(name)?;
        let inner = self.lock();
        let parent = match resolve(&inner.tree, dir) {
            Some(Place::Node(h)) => inner.tree.get(&h).cloned().expect("a resolved node"),
            Some(Place::Top) => return Err(t("No topo da conta só ficam Drive na nuvem, Lixeira e Compartilhados comigo: abra uma delas.", "The account's top only holds Cloud Drive, Rubbish Bin and Incoming shares: open one of them.").into()),
            Some(Place::Shares) => return Err(t("Compartilhados comigo só mostra as pastas que outras pessoas compartilham: abra uma delas.", "Incoming shares only shows the folders other people share: open one of them.").into()),
            None => return Err(tr!("A pasta “{}” não está mais no MEGA.", "The folder “{}” is no longer on MEGA.", last_name(dir))),
        };
        Ok((parent, name.to_string()))
    }

    /// Refuses putting something new in folder `parent`: a file, the Rubbish Bin, a share only
    /// read here.
    pub(crate) fn writable(&self, parent: &Node) -> Result<()> {
        if !parent.kind.is_dir() {
            return Err(t("Isso é um arquivo, não uma pasta.", "That's a file, not a folder.").into());
        }
        let inner = self.lock();
        if inner.tree.in_rubbish(&parent.handle) {
            return Err(t("A Lixeira do MEGA não recebe itens novos: exclua para mandar algo para ela.", "The MEGA Rubbish Bin doesn't take new items: delete something to send it there.").into());
        }
        if let Some((_, access)) = inner.tree.top_of(&parent.handle).and_then(|t| t.inshare.clone())
            && access == 0
        {
            return Err(t("Esta pasta foi compartilhada com você só para leitura.", "This folder was shared with you as read-only.").into());
        }
        Ok(())
    }

    /// Refuses a name an item of `parent` already has (MEGA would take it; the panes couldn't
    /// tell the two apart).
    fn free(&self, parent: &str, name: &str, except: Option<&str>) -> Result<()> {
        let inner = self.lock();
        if inner.tree.children(parent).iter().any(|(_, n)| n.name == name && Some(n.handle.as_str()) != except) {
            return Err(tr!("Já existe “{name}” nessa pasta do MEGA.", "“{name}” already exists in that MEGA folder."));
        }
        Ok(())
    }

    /// The `cr` element that gives the keys of new nodes to every share above `target`, so
    /// whoever the folder is shared with (or a link to it) can open them. `keys` are the new
    /// nodes' keys, by their place in the command's list.
    fn share_keys_for(&self, target: &str, keys: &[Vec<u8>]) -> Option<Value> {
        let inner = self.lock();
        let shares: Vec<(String, [u8; 16])> = inner.tree.ancestry(target).into_iter().filter_map(|n| inner.keys.shares.get(&n.handle).map(|k| (n.handle.clone(), *k))).collect();
        if shares.is_empty() || keys.is_empty() {
            return None;
        }
        let mut links = Vec::new();
        for (si, (_, sk)) in shares.iter().enumerate() {
            for (ni, key) in keys.iter().enumerate() {
                let mut k = key.clone();
                ecb_encrypt(sk, &mut k);
                links.extend([Value::from(si), Value::from(ni), Value::from(b64(&k))]);
            }
        }
        Some(json!([shares.iter().map(|(h, _)| h.clone()).collect::<Vec<_>>(), [], links]))
    }

    /// Puts new nodes (`p`) under `target` and takes MEGA's answer into the tree.
    pub(crate) fn put_nodes(&self, target: &str, nodes: Vec<Value>, keys: &[Vec<u8>]) -> Result<Vec<Node>> {
        let mut cmd = json!({"a": "p", "t": target, "n": nodes, "i": request_id()});
        if let Some(cr) = self.share_keys_for(target, keys) {
            cmd["cr"] = cr;
        }
        let answer = self.api.run(cmd)?;
        let mut inner = self.lock();
        let Inner { keys, tree, .. } = &mut *inner;
        let mut out = Vec::new();
        for raw in answer.get("f").and_then(Value::as_array).into_iter().flatten() {
            if let Some(n) = crate::nodes::decode(keys, raw) {
                out.push(n.clone());
                tree.put(n);
            }
        }
        Ok(out)
    }

    /// The node key under the master key, as `p` takes it.
    pub(crate) fn wrap(&self, key: &[u8]) -> String {
        let mut k = key.to_vec();
        ecb_encrypt(&self.session.master, &mut k);
        b64(&k)
    }

    // -----------------------------------------------------------------------------------------
    // Changes

    /// A new folder at `path`.
    pub fn mkdir(&self, path: &str) -> Result<Node> {
        let (parent, name) = self.parent_and_name(path)?;
        self.writable(&parent)?;
        self.free(&parent.handle, &name, None)?;
        let key = random::<16>();
        let mut attrs = Map::new();
        attrs.insert("n".into(), name.clone().into());
        let node = json!({"h": "xxxxxxxx", "t": 1, "a": encrypt_attrs(&key, &attrs), "k": self.wrap(&key)});
        self.put_nodes(&parent.handle, vec![node], &[key.to_vec()])?.pop().ok_or_else(|| tr!("O MEGA não criou a pasta “{name}”.", "MEGA didn't create the folder “{name}”."))
    }

    /// Changes node `h`'s attributes to `attrs`, on MEGA and in the tree.
    fn set_attrs(&self, n: &Node, attrs: Map<String, Value>) -> Result<()> {
        let at = encrypt_attrs(&attr_key(&n.key), &attrs);
        self.api.run(json!({"a": "a", "n": n.handle, "at": at, "i": request_id()}))?;
        self.apply_attrs(&n.handle, attrs);
        Ok(())
    }

    fn apply_attrs(&self, h: &str, attrs: Map<String, Value>) {
        let mut inner = self.lock();
        if let Some(mut n) = inner.tree.get(h).cloned() {
            n.name = attrs.get("n").and_then(Value::as_str).unwrap_or_default().to_string();
            n.modified = attrs.get("c").and_then(Value::as_str).and_then(crate::crypto::fingerprint_mtime);
            n.attrs = attrs;
            inner.tree.put(n);
        }
    }

    fn apply_parent(&self, h: &str, parent: &str) {
        let mut inner = self.lock();
        if let Some(mut n) = inner.tree.get(h).cloned() {
            n.parent = parent.to_string();
            inner.tree.put(n);
        }
    }

    /// Renames node `h`, keeping its other attributes.
    pub fn rename_node(&self, n: &Node, name: &str) -> Result<()> {
        check_name(name)?;
        if n.name == name {
            return Ok(());
        }
        if !n.parent.is_empty() && !self.lock().tree.in_rubbish(&n.handle) {
            self.free(&n.parent, name, Some(&n.handle))?;
        }
        let mut attrs = n.attrs.clone();
        attrs.insert("n".into(), name.into());
        self.set_attrs(n, attrs)
    }

    /// Moves node `n` into folder `to` (within the account).
    pub fn move_node(&self, n: &Node, to: &Node) -> Result<()> {
        if n.parent == to.handle {
            return Ok(());
        }
        if n.kind.is_dir() && self.lock().tree.within(&to.handle, &n.handle) {
            return Err(tr!("Não dá para mover a pasta “{}” para dentro dela mesma.", "Can't move the folder “{}” into itself.", n.name));
        }
        self.api.run(json!({"a": "m", "n": n.handle, "t": to.handle, "i": request_id()}))?;
        self.apply_parent(&n.handle, &to.handle);
        Ok(())
    }

    /// Renames or moves the item at `from` to `to` (a path whose last segment is the new name).
    /// Into the Rubbish Bin it's sending to the bin; a name taken there doesn't matter.
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        let n = self.node_at(from)?;
        if n.parent.is_empty() || n.inshare.is_some() {
            return Err(tr!("“{}” não pode ser renomeado nem movido daqui.", "“{}” can't be renamed or moved from here.", n.name));
        }
        let (parent, name) = self.parent_and_name(to)?;
        let into_bin = {
            let inner = self.lock();
            inner.tree.in_rubbish(&parent.handle) && !inner.tree.in_rubbish(&n.handle)
        };
        if into_bin {
            self.trash(&n)?;
            let n = self.lock().tree.get(&n.handle).cloned().unwrap_or(n);
            return if n.name == name { Ok(()) } else { self.rename_node(&n, &name) };
        }
        if parent.handle != n.parent {
            self.writable(&parent)?;
            self.free(&parent.handle, &name, Some(&n.handle))?;
            self.move_node(&n, &parent)?;
        }
        let n = self.lock().tree.get(&n.handle).cloned().unwrap_or(n);
        self.rename_node(&n, &name)
    }

    /// Sends node `n` to the Rubbish Bin, noting where it was (the `rr` attribute MEGA's apps
    /// restore it by).
    pub fn trash(&self, n: &Node) -> Result<()> {
        let bin = self.lock().tree.rubbish.clone().ok_or(t("A conta não tem Lixeira.", "The account has no Rubbish Bin."))?;
        if self.lock().tree.in_rubbish(&n.handle) {
            return Ok(());
        }
        let mut attrs = n.attrs.clone();
        attrs.insert("rr".into(), n.parent.clone().into());
        let at = encrypt_attrs(&attr_key(&n.key), &attrs);
        let answers = self.api.batch(&[json!({"a": "m", "n": n.handle, "t": bin, "i": request_id()}), json!({"a": "a", "n": n.handle, "at": at, "i": request_id()})]).map_err(String::from)?;
        if let Some(code) = answers.first().and_then(Value::as_i64).filter(|c| *c < 0) {
            return Err(crate::api::error_text(code));
        }
        self.apply_parent(&n.handle, &bin);
        if answers.get(1).and_then(Value::as_i64).is_none_or(|c| c >= 0) {
            self.apply_attrs(&n.handle, attrs);
        }
        Ok(())
    }

    /// Takes node `n` out of the Rubbish Bin, back to the folder it was in (when that's still
    /// there and not in the bin), else to Cloud Drive's top. Gives the folder it went to.
    pub fn restore(&self, n: &Node) -> Result<Node> {
        let to = {
            let inner = self.lock();
            let back = n.attrs.get("rr").and_then(Value::as_str).and_then(|h| inner.tree.get(h)).filter(|p| p.kind.is_dir() && !inner.tree.in_rubbish(&p.handle)).cloned();
            match back {
                Some(p) => p,
                None => inner.tree.root.as_ref().and_then(|r| inner.tree.get(r)).cloned().ok_or(t("A conta não tem Drive na nuvem.", "The account has no Cloud Drive."))?,
            }
        };
        self.move_node(n, &to)?;
        let mut attrs = n.attrs.clone();
        if attrs.remove("rr").is_some() {
            let n = self.lock().tree.get(&n.handle).cloned().unwrap_or_else(|| n.clone());
            // Restored even if this fails: the attribute only says where it came from.
            let _ = self.set_attrs(&n, attrs);
        }
        Ok(to)
    }

    /// Deletes node `n` and all it holds, for good.
    pub fn delete(&self, n: &Node) -> Result<()> {
        self.api.run(json!({"a": "d", "n": n.handle, "i": request_id()}))?;
        self.lock().tree.remove(&n.handle, true);
        Ok(())
    }

    /// Deletes the item at `path`: to the Rubbish Bin, or for good when it's already there.
    pub fn remove(&self, path: &str) -> Result<()> {
        let n = self.node_at(path)?;
        if n.parent.is_empty() || n.inshare.is_some() {
            return Err(tr!("“{}” não pode ser excluído daqui.", "“{}” can't be deleted from here.", n.name));
        }
        if self.lock().tree.in_rubbish(&n.handle) { self.delete(&n) } else { self.trash(&n) }
    }

    /// Deletes the empty folder at `path` for good (undoing a new folder).
    pub fn remove_empty_dir(&self, path: &str) -> Result<()> {
        let n = self.node_at(path)?;
        if !n.kind.is_dir() || n.parent.is_empty() || n.inshare.is_some() {
            return Err(tr!("“{}” não é uma pasta que possa ser excluída.", "“{}” isn't a folder that can be deleted.", n.name));
        }
        if !self.lock().tree.children(&n.handle).is_empty() {
            return Err(tr!("A pasta “{}” não está vazia.", "The folder “{}” isn't empty.", n.name));
        }
        self.delete(&n)
    }

    /// Copies the item at `from` to `to` on MEGA's side: a file's contents aren't sent again.
    pub fn copy(&self, from: &str, to: &str) -> Result<Node> {
        let src = self.node_at(from)?;
        let (parent, name) = self.parent_and_name(to)?;
        self.writable(&parent)?;
        self.free(&parent.handle, &name, None)?;
        if src.kind.is_dir() && self.lock().tree.within(&parent.handle, &src.handle) {
            return Err(tr!("Não dá para copiar a pasta “{}” para dentro dela mesma.", "Can't copy the folder “{}” into itself.", src.name));
        }
        let (nodes, keys) = {
            let inner = self.lock();
            let mut nodes = Vec::new();
            let mut keys = Vec::new();
            for n in inner.tree.subtree(&src.handle) {
                // A file keeps its key; a folder gets a new one.
                let key = if n.kind == Kind::File { n.key.clone() } else { random::<16>().to_vec() };
                let mut attrs = n.attrs.clone();
                attrs.remove("rr");
                if n.handle == src.handle {
                    attrs.insert("n".into(), name.clone().into());
                }
                let mut node = json!({"h": n.handle, "t": if n.kind == Kind::File { 0 } else { 1 }, "a": encrypt_attrs(&attr_key(&key), &attrs), "k": self.wrap(&key)});
                if n.handle != src.handle {
                    node["p"] = n.parent.clone().into();
                }
                nodes.push(node);
                keys.push(key);
            }
            (nodes, keys)
        };
        let made = self.put_nodes(&parent.handle, nodes, &keys)?;
        made.into_iter().find(|n| n.parent == parent.handle).ok_or_else(|| tr!("O MEGA não copiou “{}”.", "MEGA didn't copy “{}”.", src.name))
    }
}

/// What `path` names in `tree`.
pub(crate) fn resolve(tree: &Tree, path: &str) -> Option<Place> {
    let mut at = Place::Top;
    for seg in segments(path) {
        if let Some(h) = seg.strip_prefix('\0') {
            tree.get(h)?;
            at = Place::Node(h.to_string());
            continue;
        }
        at = match at {
            Place::Top => match Area::named(seg)? {
                Area::Cloud => Place::Node(tree.root.clone()?),
                Area::Rubbish => Place::Node(tree.rubbish.clone()?),
                Area::Incoming => Place::Shares,
            },
            Place::Shares => Place::Node(tree.inshares().into_iter().find(|(name, _)| name == seg)?.1.handle.clone()),
            Place::Node(h) => Place::Node(tree.children(&h).into_iter().find(|(name, _)| name == seg)?.1.handle.clone()),
        };
    }
    Some(at)
}

/// The name node `n` shows with in its folder.
fn shown_name(tree: &Tree, n: &Node) -> String {
    match n.kind {
        Kind::Root => Area::Cloud.label().into(),
        Kind::Rubbish => Area::Rubbish.label().into(),
        _ if n.inshare.is_some() => tree.inshares().into_iter().find(|(_, x)| x.handle == n.handle).map_or_else(|| n.name.clone(), |(name, _)| name),
        _ => tree.children(&n.parent).into_iter().find(|(_, x)| x.handle == n.handle).map_or_else(|| n.name.clone(), |(name, _)| name),
    }
}

/// `path` split at its last `/`: the folder and the name.
fn split(path: &str) -> (&str, &str) {
    let path = path.trim_end_matches('/');
    match path.rsplit_once('/') {
        Some((dir, name)) => (if dir.is_empty() { "/" } else { dir }, name),
        None => ("/", path),
    }
}

fn last_name(path: &str) -> String {
    split(path).1.trim_start_matches('\0').to_string()
}

/// Names MEGA's apps refuse.
fn check_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name == "." || name == ".." {
        return Err(t("Esse nome não pode ser usado.", "That name can't be used.").into());
    }
    if name.contains('/') || name.contains('\0') {
        return Err(t("O nome não pode ter /.", "The name can't have /.").into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The accounts open now

static OPEN: LazyLock<Mutex<HashMap<String, Arc<Account>>>> = LazyLock::new(Mutex::default);
/// One lock per e-mail, so two panes reaching the same account at once log in once.
static OPENING: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = LazyLock::new(Mutex::default);

/// The account open for `email`, if it is.
pub fn cached(email: &str) -> Option<Arc<Account>> {
    OPEN.lock().unwrap_or_else(|e| e.into_inner()).get(&session::normal_email(email)).cloned()
}

/// The account for `email`: open already, or resumed from the session kept in the credential
/// store (its tree read here, which takes a moment on a big account).
pub fn reach(email: &str) -> Result<Arc<Account>> {
    let email = session::normal_email(email);
    if let Some(a) = cached(&email) {
        return Ok(a);
    }
    let gate = OPENING.lock().unwrap_or_else(|e| e.into_inner()).entry(email.clone()).or_default().clone();
    let _held = gate.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(a) = cached(&email) {
        return Ok(a);
    }
    let saved = store::session(&email)?;
    let api = Api::default();
    let session = session::resume(&api, &saved).map_err(|f| match f {
        Fail::Code(crate::api::ESID | crate::api::ENOENT) => t("A sessão do MEGA terminou (saiu pelo site ou mudou a senha?). Remova a conta e conecte de novo.", "The MEGA session ended (signed out on the website or changed the password?). Remove the account and connect it again.").to_string(),
        f => f.text(),
    })?;
    let account = Arc::new(Account::start(&email, api, session)?);
    OPEN.lock().unwrap_or_else(|e| e.into_inner()).insert(email, account.clone());
    Ok(account)
}

/// Logs in with e-mail and password (and the two-factor code) and keeps the session in the
/// credential store. The password isn't kept anywhere.
pub fn connect(email: &str, password: &str, code: Option<&str>) -> std::result::Result<Arc<Account>, Refused> {
    let email = session::normal_email(email);
    let api = Api::default();
    let session = session::login(&api, &email, password, code)?;
    let account = Account::start(&email, api, session).map_err(Refused::Failed)?;
    store::set_session(&email, &account.saved()).map_err(Refused::Failed)?;
    let account = Arc::new(account);
    OPEN.lock().unwrap_or_else(|e| e.into_inner()).insert(email, account.clone());
    Ok(account)
}

/// Forgets account `email`: its session ends on MEGA's side (when it's open) and leaves the
/// credential store.
pub fn disconnect(email: &str) {
    let email = session::normal_email(email);
    let open = OPEN.lock().unwrap_or_else(|e| e.into_inner()).remove(&email);
    if let Some(a) = open {
        a.logout();
    }
    store::forget_session(&email);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::tests::{keys, listing};

    fn account() -> Account {
        let mut k = keys();
        let tree = { let l = listing(&k); Tree::from_listing(&mut k, &l) };
        Account::offline(k, tree)
    }

    #[test]
    fn paths_reach_the_areas_in_either_language() {
        let a = account();
        assert_eq!(a.resolve("/"), Some(Place::Top));
        assert_eq!(a.resolve("/Drive na nuvem"), Some(Place::Node("ROOT0000".into())));
        assert_eq!(a.resolve("/Cloud Drive/Fotos/praia.jpg"), Some(Place::Node("FILE0001".into())));
        assert_eq!(a.resolve("/Rubbish Bin/velho.txt"), Some(Place::Node("TRASHED1".into())));
        assert_eq!(a.resolve("/Lixeira"), Some(Place::Node("BIN00000".into())));
        assert_eq!(a.resolve("/Compartilhados comigo"), Some(Place::Shares));
        assert_eq!(a.resolve("/Drive na nuvem/nada"), None);
        assert_eq!(a.resolve("/Outra"), None);
        assert_eq!(a.resolve(&handle_path("FILE0001")), Some(Place::Node("FILE0001".into())));
        assert_eq!(a.resolve(&format!("{}/praia.jpg", handle_path("FOLDER01"))), Some(Place::Node("FILE0001".into())));
    }

    #[test]
    fn listings_show_areas_then_folders() {
        let a = account();
        let top: Vec<String> = a.list("/").unwrap().into_iter().map(|i| i.name).collect();
        assert_eq!(top, ["Drive na nuvem", "Lixeira", "Compartilhados comigo"]);
        let root = a.list("/Drive na nuvem").unwrap();
        assert_eq!(root.iter().map(|i| (i.name.as_str(), i.is_dir, i.size)).collect::<Vec<_>>(), [("Fotos", true, None), ("nota.txt", false, Some(10))]);
        assert!(a.list("/Drive na nuvem/nota.txt").is_err());
        assert_eq!(a.stat("/Drive na nuvem/Fotos").unwrap().handle.as_deref(), Some("FOLDER01"));
        assert!(a.in_rubbish("/Lixeira/velho.txt"));
        assert!(!a.in_rubbish("/Drive na nuvem/nota.txt"));
    }

    #[test]
    fn writes_where_nothing_can_go_are_refused_before_asking_mega() {
        let a = account();
        // The API here is unreachable: each of these must fail before calling it.
        let refusal = |r: Result<Node>| r.unwrap_err();
        assert!(refusal(a.mkdir("/Nova")).contains("topo"));
        assert!(refusal(a.mkdir("/Compartilhados comigo/Nova")).contains("compartilham"));
        assert!(refusal(a.mkdir("/Lixeira/Nova")).contains("Lixeira"));
        assert!(refusal(a.mkdir("/Drive na nuvem/Fotos")).contains("Já existe"));
        assert!(refusal(a.mkdir("/Drive na nuvem/nota.txt/x")).contains("arquivo"));
        assert!(a.rename("/Drive na nuvem", "/Drive na nuvem2").is_err());
        assert!(a.remove("/Lixeira").is_err());
        assert!(a.rename("/Drive na nuvem/Fotos", "/Drive na nuvem/nota.txt").unwrap_err().contains("Já existe"));
        assert!(a.rename("/Drive na nuvem/Fotos", "/Drive na nuvem/Fotos/Dentro").unwrap_err().contains("dentro dela"));
        assert!(a.copy("/Drive na nuvem/Fotos", "/Drive na nuvem/Fotos/Dentro").unwrap_err().contains("dentro dela"));
        assert!(a.remove_empty_dir("/Drive na nuvem/Fotos").unwrap_err().contains("vazia"));
    }

    #[test]
    fn read_only_shares_refuse_new_items() {
        let mut k = keys();
        let mut l = listing(&k);
        let sk = [4u8; 16];
        k.shares.insert("INSHARE1".into(), sk);
        let mut top = crate::nodes::tests::raw(&k, "INSHARE1", "", 1, "Deles", ("INSHARE1", sk));
        top["su"] = "OTHERUSER01".into();
        top["r"] = 0.into();
        l["f"].as_array_mut().unwrap().push(top);
        let tree = Tree::from_listing(&mut k, &l);
        let a = Account::offline(k, tree);
        assert_eq!(a.list("/Compartilhados comigo").unwrap()[0].name, "Deles");
        assert!(a.mkdir("/Compartilhados comigo/Deles/Nova").unwrap_err().contains("leitura"));
        assert!(a.remove("/Compartilhados comigo/Deles").is_err());
    }

    #[test]
    fn new_nodes_in_a_shared_folder_carry_the_share_key() {
        let mut k = keys();
        k.shares.insert("FOLDER01".into(), [5u8; 16]);
        let tree = { let l = listing(&k); Tree::from_listing(&mut k, &l) };
        let a = Account::offline(k, tree);
        let node_key = vec![1u8; 16];
        let cr = a.share_keys_for("FOLDER01", std::slice::from_ref(&node_key)).unwrap();
        let mut want = node_key.clone();
        ecb_encrypt(&[5u8; 16], &mut want);
        assert_eq!(cr, json!([["FOLDER01"], [], [0, 0, b64(&want)]]));
        assert_eq!(a.share_keys_for("ROOT0000", &[node_key]), None);
    }

    #[test]
    fn paths_split_at_the_last_slash() {
        assert_eq!(split("/a/b/c"), ("/a/b", "c"));
        assert_eq!(split("/a"), ("/", "a"));
        assert_eq!(last_name(&handle_path("XYZ")), "XYZ");
        assert!(check_name("a/b").is_err() && check_name("..").is_err() && check_name("ok").is_ok());
    }
}
