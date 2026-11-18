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
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use openexplorer_i18n::{t, tr};
use serde_json::{Map, Value, json};

use crate::api::{Api, Fail, request_id};
use crate::crypto::{attr_key, b64, ecb_encrypt, encrypt_attrs, random};
use crate::nodes::{Handle, Keys, Kind, Node, Tree};
use crate::changes::Caught;
use crate::session::{self, Refused, Session};
use crate::{Result, cache, store};

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
        Item { name, handle: Some(n.handle.to_string()), is_dir, size: (!is_dir).then_some(n.size), modified: n.modified.or(Some(n.created).filter(|t| *t > 0)) }
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
    /// Where the tree is kept between runs and what seals it (see [`crate::cache`]); `None`
    /// keeps nothing.
    kept: Option<(PathBuf, cache::Secret)>,
    /// When the tree was last kept, and whether it changed since.
    saved: Mutex<Saved>,
    /// The account was removed: a save still on its way writes nothing.
    forgotten: AtomicBool,
    /// Whether this start read the kept tree (and only the changes since), not the whole one.
    from_cache: AtomicBool,
}

#[derive(Default)]
struct Saved {
    at: Option<Instant>,
    dirty: bool,
}

/// How often, at most, polling keeps the tree again while it changes.
const SAVE_EVERY: Duration = Duration::from_secs(5 * 60);
/// Rounds of the change channel a start reads before giving up on the kept tree.
const CATCH_UP_ROUNDS: usize = 1000;

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account").field("email", &self.email).finish_non_exhaustive()
    }
}

/// The keys a session opens nodes with before any share key is known.
fn session_keys(s: &Session) -> Keys {
    Keys { user: s.user.clone(), master: s.master, rsa: s.rsa.clone(), shares: HashMap::new() }
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
    /// An account from a session: its tree is read here, from the tree kept in the cache folder
    /// (see [`crate::cache::set_dir`]) and the changes since, or whole.
    pub fn start(email: &str, api: Api, session: Session) -> Result<Account> {
        Account::start_kept(email, api, session, cache::file_of(email))
    }

    /// [`Account::start`] with the tree kept in `file`, or nowhere.
    pub fn start_kept(email: &str, api: Api, session: Session, file: Option<PathBuf>) -> Result<Account> {
        let kept = file.map(|f| (f, cache::Secret::of(&session.user, &session.master, &session.sek)));
        let keys = session_keys(&session);
        let account = Account { email: session::normal_email(email), api, session, inner: Mutex::new(Inner { keys, tree: Tree::default(), refetch: false }), kept, saved: Mutex::default(), forgotten: AtomicBool::new(false), from_cache: AtomicBool::new(false) };
        if !account.resume_kept()? {
            account.fetch()?;
        }
        Ok(account)
    }

    /// An account over a tree given whole, for tests.
    #[cfg(test)]
    pub(crate) fn offline(keys: Keys, tree: Tree) -> Account {
        let session = Session { user: keys.user.clone(), master: keys.master, sek: [0; 16], sid: String::new(), rsa: None };
        Account { email: "ana@exemplo.com".into(), api: Api::unreachable(), session, inner: Mutex::new(Inner { keys, tree, refetch: false }), kept: None, saved: Mutex::default(), forgotten: AtomicBool::new(false), from_cache: AtomicBool::new(false) }
    }

    /// Whether this start read the kept tree and only the changes since, not the whole tree.
    pub fn started_from_cache(&self) -> bool {
        self.from_cache.load(Ordering::SeqCst)
    }

    /// Takes in the kept tree and the changes since it was kept. `false` when there's none to
    /// use: no file, one this session can't open, one whose sequence number MEGA no longer
    /// takes (then the file is thrown away), or the change channel failing (then the file stays
    /// until a whole read replaces it). A start never shows a tree it couldn't bring up to date.
    fn resume_kept(&self) -> Result<bool> {
        let Some((file, secret)) = &self.kept else { return Ok(false) };
        let started = Instant::now();
        let (tree, shares) = match cache::read(file, secret) {
            Ok((tree, _)) if tree.root.is_none() || tree.sn.is_empty() => {
                crate::api::log("MEGA cache: árvore guardada sem a pasta principal, descartada");
                cache::forget_file(file);
                return Ok(false);
            }
            Ok(kept) => kept,
            Err(cache::Refusal::Missing) => return Ok(false),
            Err(why) => {
                crate::api::log(&format!("MEGA cache: {why:?}, descartado"));
                cache::forget_file(file);
                return Ok(false);
            }
        };
        let nodes = tree.len();
        let read = started.elapsed();
        {
            let mut inner = self.lock();
            inner.keys.shares = shares;
            inner.tree = tree;
            inner.refetch = false;
        }
        let mut changes = crate::Changes::default();
        let caught = self.read_changes(CATCH_UP_ROUNDS, &mut changes);
        match caught {
            Ok(Caught::Up) => {
                crate::api::log(&format!("MEGA cache: {nodes} nós lidos em {} ms, em dia em {} ms ({} pastas mudaram)", read.as_millis(), started.elapsed().as_millis(), changes.folders.len()));
                self.from_cache.store(true, Ordering::SeqCst);
                self.saved.lock().unwrap_or_else(|e| e.into_inner()).dirty = !changes.is_empty();
                Ok(true)
            }
            Ok(caught) => {
                crate::api::log(&format!("MEGA cache: o canal de mudanças não pôs a árvore em dia ({caught:?}), lendo a conta inteira"));
                cache::forget_file(file);
                self.drop_kept();
                Ok(false)
            }
            // The channel failing (no network, an answer it can't read) doesn't show the kept
            // tree: the whole one is read, which fails the start as before if MEGA can't be
            // reached, the file staying for the next try; and overwrites the file if it can.
            Err(e) => {
                crate::api::log(&format!("MEGA cache: o canal de mudanças falhou ({e}), lendo a conta inteira"));
                self.drop_kept();
                Ok(false)
            }
        }
    }

    /// Back to what a start without a kept tree has: no tree, and no share keys but what the
    /// whole listing brings (the kept ones may be of shares that ended).
    fn drop_kept(&self) {
        *self.lock() = Inner { keys: session_keys(&self.session), tree: Tree::default(), refetch: false };
    }

    /// Keeps the tree on disk now, if this account keeps it. Encoded under the lock, sealed
    /// and written after.
    pub fn save_cache(&self) {
        let Some((file, secret)) = &self.kept else { return };
        let started = Instant::now();
        let body = {
            let inner = self.lock();
            if inner.tree.root.is_none() || inner.refetch {
                return;
            }
            // Marked kept while the tree can't change: a change applied after this snapshot
            // marks it changed again.
            *self.saved.lock().unwrap_or_else(|e| e.into_inner()) = Saved { at: Some(Instant::now()), dirty: false };
            cache::encode(&inner.tree, &inner.keys.shares)
        };
        let encoded = started.elapsed();
        match cache::write_unless(file, secret, body, &self.forgotten) {
            Ok(()) => crate::api::log(&format!("MEGA cache: árvore guardada em {} ms ({} ms montando)", started.elapsed().as_millis(), encoded.as_millis())),
            Err(e) => {
                crate::api::log(&format!("MEGA cache: não consegui guardar a árvore: {e}"));
                self.saved.lock().unwrap_or_else(|e| e.into_inner()).dirty = true;
            }
        }
    }

    /// After a poll: notes whether the tree changed, and keeps it when it changed since it was
    /// last kept (by this poll or before: a start's catch-up) and it's been a while.
    pub(crate) fn polled(&self, changed: bool) {
        let due = {
            let mut saved = self.saved.lock().unwrap_or_else(|e| e.into_inner());
            saved.dirty |= changed;
            saved.dirty && saved.at.is_none_or(|at| at.elapsed() >= SAVE_EVERY)
        };
        if due {
            self.save_cache();
        }
    }

    /// Keeps the tree if it changed since it was last kept (the app calls it before closing).
    pub fn save_cache_if_changed(&self) {
        if self.saved.lock().unwrap_or_else(|e| e.into_inner()).dirty {
            self.save_cache();
        }
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
                    Ok(found) => {
                        crate::api::log(&format!("MEGA ^!keys: {} chaves de compartilhamento", found.len()));
                        keys.shares.extend(found)
                    }
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
        *self.lock() = Inner { keys, tree, refetch: false };
        self.save_cache();
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

    /// A path that keeps naming what `path` names while the items around it change: a twin's
    /// shown name ("name (2)") moves to the other twin once one goes, so a twin is named by its
    /// handle; any other item by `path` itself.
    pub fn pin(&self, path: &str) -> Result<String> {
        let inner = self.lock();
        match resolve(&inner.tree, path) {
            Some(Place::Node(h)) => {
                let n = inner.tree.get(&h).expect("a resolved node");
                Ok(if !n.parent.is_empty() && inner.tree.same_name(&n.parent, &n.name).nth(1).is_some() { handle_path(&h) } else { path.to_string() })
            }
            Some(_) => Ok(path.to_string()),
            None => Err(tr!("“{}” não está mais no MEGA.", "“{}” is no longer on MEGA.", last_name(path))),
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
        if let Some(top) = inner.tree.top_of(&parent.handle)
            && let Some((_, access)) = &top.inshare
        {
            if *access == 0 {
                return Err(t("Esta pasta foi compartilhada com você só para leitura.", "This folder was shared with you as read-only.").into());
            }
            // Without the share's key, whoever shares it couldn't open what goes in.
            if !inner.keys.shares.contains_key(top.handle.as_str()) {
                return Err(t("O Open Explorer não tem a chave desta pasta compartilhada, então quem compartilhou não conseguiria abrir o que fosse colocado nela. Use o site do MEGA para isso.", "Open Explorer doesn't have this shared folder's key, so whoever shared it couldn't open what goes in. Use the MEGA website for this.").into());
            }
        }
        Ok(())
    }

    /// Refuses a name an item of `parent` already has (MEGA would take it; the panes couldn't
    /// tell the two apart).
    fn free(&self, parent: &str, name: &str, except: Option<&str>) -> Result<()> {
        let inner = self.lock();
        if inner.tree.same_name(parent, name).any(|n| Some(n.handle.as_str()) != except) {
            return Err(tr!("Já existe “{name}” nessa pasta do MEGA.", "“{name}” already exists in that MEGA folder."));
        }
        Ok(())
    }

    /// The `cr` element that gives the keys of new nodes to every share above `target`, so
    /// whoever the folder is shared with (or a link to it) can open them. `keys` are the new
    /// nodes' keys, by their place in the command's list.
    fn share_keys_for(&self, target: &str, keys: &[Vec<u8>]) -> Option<Value> {
        let inner = self.lock();
        let shares: Vec<(String, [u8; 16])> = inner.tree.ancestry(target).into_iter().filter_map(|n| inner.keys.shares.get(n.handle.as_str()).map(|k| (n.handle.to_string(), *k))).collect();
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
            n.set_attrs(attrs);
            inner.tree.put(n);
        }
    }

    fn apply_parent(&self, h: &str, parent: &str) {
        let mut inner = self.lock();
        if let (Some(mut n), Some(parent)) = (inner.tree.get(h).cloned(), Handle::new(parent)) {
            n.parent = parent;
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
            self.free(&n.parent, name, Some(n.handle.as_str()))?;
        }
        let mut attrs = n.attrs();
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
        // Moved under the name the panes show a twin by ("Fotos (2)"), it keeps its own name:
        // the twin it's told apart from isn't where it goes (or it's a twin again there).
        let keep_own = parent.handle != n.parent && (name == n.name || crate::nodes::twin_of(&name, &n.name, n.kind == Kind::File));
        let name = if keep_own { n.name.clone() } else { name };
        if into_bin {
            self.trash(&n)?;
            let n = self.lock().tree.get(&n.handle).cloned().unwrap_or(n);
            return if n.name == name { Ok(()) } else { self.rename_node(&n, &name) };
        }
        if parent.handle != n.parent {
            // MEGA moves only within one tree: the account's own folders, or one share.
            let (from, into) = {
                let inner = self.lock();
                (inner.tree.top_of(&n.handle).map(|t| t.handle), inner.tree.top_of(&parent.handle).map(|t| (t.handle, t.inshare.is_some())))
            };
            let other_tree = match (&from, &into) {
                (Some(f), Some((t, shared))) => f != t && (*shared || self.in_share(&n.handle)),
                _ => false,
            };
            if other_tree {
                return Err(tr!("O MEGA não move “{}” entre uma pasta compartilhada com você e as suas: copie e depois exclua.", "MEGA doesn't move “{}” between a folder shared with you and your own: copy it, then delete.", n.name));
            }
            self.writable(&parent)?;
            if !keep_own {
                self.free(&parent.handle, &name, Some(n.handle.as_str()))?;
            }
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
        let mut attrs = n.attrs();
        attrs.insert("rr".into(), n.parent.to_string().into());
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
            let back = n.attrs().get("rr").and_then(Value::as_str).and_then(|h| inner.tree.get(h)).filter(|p| p.kind.is_dir() && !inner.tree.in_rubbish(&p.handle)).cloned();
            match back {
                Some(p) => p,
                None => inner.tree.root.as_ref().and_then(|r| inner.tree.get(r)).cloned().ok_or(t("A conta não tem Drive na nuvem.", "The account has no Cloud Drive."))?,
            }
        };
        self.move_node(n, &to)?;
        let mut attrs = n.attrs();
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

    /// Whether node `handle` is inside a folder someone else shares: what's there belongs to
    /// them, and can't go to this account's Rubbish Bin.
    pub fn in_share(&self, handle: &str) -> bool {
        self.lock().tree.top_of(handle).is_some_and(|t| t.inshare.is_some())
    }

    /// Deletes the item at `path`: to the Rubbish Bin, or for good when it's already there or in
    /// a folder shared with this account (MEGA moves nothing between the two).
    pub fn remove(&self, path: &str) -> Result<()> {
        let n = self.node_at(path)?;
        if n.parent.is_empty() || n.inshare.is_some() {
            return Err(tr!("“{}” não pode ser excluído daqui.", "“{}” can't be deleted from here.", n.name));
        }
        self.discard(&n)
    }

    /// Node `n` to the Rubbish Bin, or deleted for good where it can't go there.
    pub(crate) fn discard(&self, n: &Node) -> Result<()> {
        if self.lock().tree.in_rubbish(&n.handle) || self.in_share(&n.handle) { self.delete(n) } else { self.trash(n) }
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
            // Items no key here opens can't be copied: the copy would come out short.
            let folders: std::collections::HashSet<&str> = inner.tree.subtree(&src.handle).into_iter().filter(|n| n.kind.is_dir()).map(|n| n.handle.as_str()).collect();
            if inner.tree.locked_within(&folders) {
                return Err(tr!("Alguns itens de “{}” não podem ser abertos por esta conta, e a cópia sairia incompleta.", "Some items in “{}” can't be opened by this account, so the copy would be incomplete.", src.name));
            }
            let mut nodes = Vec::new();
            let mut keys = Vec::new();
            for n in inner.tree.subtree(&src.handle) {
                // A file keeps its key; a folder gets a new one.
                let key = if n.kind == Kind::File { n.key.to_vec() } else { random::<16>().to_vec() };
                let mut attrs = n.attrs();
                attrs.remove("rr");
                if n.handle == src.handle {
                    attrs.insert("n".into(), name.clone().into());
                }
                let mut node = json!({"h": n.handle, "t": if n.kind == Kind::File { 0 } else { 1 }, "a": encrypt_attrs(&attr_key(&key), &attrs), "k": self.wrap(&key)});
                if n.handle != src.handle {
                    node["p"] = n.parent.to_string().into();
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
            Place::Shares => Place::Node(tree.inshares().into_iter().find(|(name, _)| name == seg)?.1.handle.to_string()),
            Place::Node(h) => Place::Node(tree.child_named(&h, seg)?.handle.to_string()),
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
        _ => tree.shown_name(n),
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

/// Keeps the tree of each account open whose tree changed since it was last kept: the app
/// calls it as it closes, so the next start has less to catch up on.
pub fn save_caches() {
    let open: Vec<Arc<Account>> = OPEN.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
    for a in open {
        a.save_cache_if_changed();
    }
}

/// Forgets account `email`: it leaves the credential store at once, and its session ends on
/// MEGA's side on a thread of its own (an account added again meanwhile keeps its new session).
pub fn disconnect(email: &str) {
    let email = session::normal_email(email);
    let open = OPEN.lock().unwrap_or_else(|e| e.into_inner()).remove(&email);
    // Its kept tree goes too, and a save on its way writes nothing.
    if let Some(a) = &open {
        a.forgotten.store(true, Ordering::SeqCst);
    }
    cache::forget(&email);
    // Not opened this run: the kept session's id alone ends it.
    let sid = match &open {
        Some(_) => None,
        None => store::session(&email).ok().and_then(|saved| session::sid_of(&saved)),
    };
    store::forget_session(&email);
    std::thread::spawn(move || match (open, sid) {
        (Some(a), _) => a.logout(),
        (None, Some(sid)) => {
            let api = Api::default();
            api.set_sid(Some(sid));
            let _ = api.call(json!({"a": "sml"}));
        }
        (None, None) => {}
    });
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
        assert!(a.in_share("INSHARE1") && !a.in_share("FILE0001"));
        // MEGA moves only within one tree: out of a share into Cloud Drive, or back, is refused here.
        assert!(a.rename("/Drive na nuvem/nota.txt", "/Compartilhados comigo/Deles/nota.txt").unwrap_err().contains("copie"));
    }

    /// A share whose key this account doesn't hold takes nothing new (its owner couldn't open
    /// it), and a folder with items no key opens isn't copied short.
    #[test]
    fn writes_needing_a_missing_key_are_refused() {
        let mut k = keys();
        let mut l = listing(&k);
        let sk = [4u8; 16];
        k.shares.insert("INSHARE1".into(), sk);
        let mut top = crate::nodes::tests::raw(&k, "INSHARE1", "", 1, "Deles", ("INSHARE1", sk));
        top["su"] = "OTHERUSER01".into();
        top["r"] = 1.into();
        l["f"].as_array_mut().unwrap().push(top);
        // An item in Fotos under a share key this account never got.
        l["f"].as_array_mut().unwrap().push(crate::nodes::tests::raw(&k, "LOCKED01", "FOLDER01", 0, "segredo.txt", ("SOMESHARE", [8u8; 16])));
        let tree = Tree::from_listing(&mut k, &l);
        k.shares.remove("INSHARE1");
        let a = Account::offline(k, tree);
        assert!(a.mkdir("/Compartilhados comigo/Deles/Nova").unwrap_err().contains("chave"));
        assert!(a.copy("/Drive na nuvem/Fotos", "/Drive na nuvem/Fotos 2").unwrap_err().contains("incompleta"));
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

    /// Benchmark (`cargo test -p openexplorer-mega --release -- --ignored --nocapture big_folder`):
    /// every item of a 5 000-item folder looked up by path and stat'ed, as a download of the
    /// folder does, plus one twin.
    #[test]
    #[ignore = "benchmark"]
    fn resolving_every_item_of_a_big_folder() {
        let mut tree = Tree::default();
        let node = |h: String, p: &str, kind: Kind, name: String, created: i64| {
            let mut n = Node::bare(&h, p, kind, &name).unwrap();
            (n.size, n.created, n.key) = (1, created, crate::nodes::NodeKey::new(&[0; 32]));
            n
        };
        tree.put(node("ROOT0000".into(), "", Kind::Root, String::new(), 0));
        for i in 0..5000 {
            tree.put(node(format!("H{i:07}"), "ROOT0000", Kind::File, format!("foto {i}.jpg"), i));
        }
        tree.put(node("TWIN0000".into(), "ROOT0000", Kind::File, "foto 7.jpg".into(), 9999));
        let a = Account::offline(keys(), tree);
        let started = std::time::Instant::now();
        for i in 0..5000 {
            let path = format!("/Drive na nuvem/foto {i}.jpg");
            assert_eq!(a.resolve(&path), Some(Place::Node(format!("H{i:07}"))));
            assert!(a.stat(&path).is_some());
        }
        assert_eq!(a.resolve("/Drive na nuvem/foto 7 (2).jpg"), Some(Place::Node("TWIN0000".into())));
        eprintln!("5000 lookups + stats: {} ms", started.elapsed().as_millis());
    }

    /// A twin is pinned by its handle, so it's still found once the other twin is gone; other
    /// items keep their paths.
    #[test]
    fn twins_are_pinned_by_handle() {
        let mut k = keys();
        let mut l = listing(&k);
        let own = (k.user.clone(), k.master);
        let mut twin = crate::nodes::tests::raw(&k, "TWIN0001", "ROOT0000", 0, "nota.txt", (own.0.as_str(), own.1));
        twin["ts"] = 500.into();
        l["f"].as_array_mut().unwrap().push(twin);
        let tree = Tree::from_listing(&mut k, &l);
        let a = Account::offline(k, tree);
        assert_eq!(a.pin("/Drive na nuvem/Fotos").unwrap(), "/Drive na nuvem/Fotos");
        assert_eq!(a.pin("/Drive na nuvem").unwrap(), "/Drive na nuvem");
        let first = a.pin("/Drive na nuvem/nota.txt").unwrap();
        let second = a.pin("/Drive na nuvem/nota (2).txt").unwrap();
        assert_eq!((first.as_str(), second.as_str()), (handle_path("FILE0002").as_str(), handle_path("TWIN0001").as_str()));
        // The first twin goes: "nota (2).txt" no longer names anything, its pin still does.
        a.lock().tree.remove("FILE0002", true);
        assert!(a.resolve("/Drive na nuvem/nota (2).txt").is_none());
        assert_eq!(a.resolve(&second), Some(Place::Node("TWIN0001".into())));
        assert_eq!(a.resolve(&format!("{second}/../x")), None);
        assert!(a.pin("/Drive na nuvem/nada").is_err());
    }

    /// A twin moved by its shown name keeps its own, and comes back a twin when the move is
    /// undone; any other name given to a move is a rename, which a taken name refuses.
    #[test]
    fn twins_keep_their_names_when_moved() {
        let mut k = keys();
        let mut l = listing(&k);
        let own = (k.user.clone(), k.master);
        let mut twin = crate::nodes::tests::raw(&k, "TWIN0001", "ROOT0000", 0, "nota.txt", (own.0.as_str(), own.1));
        twin["ts"] = 500.into();
        l["f"].as_array_mut().unwrap().push(twin);
        let tree = Tree::from_listing(&mut k, &l);
        let a = Account::offline(k, tree);
        // Offline: the move itself fails at the network, after the checks that matter here.
        let moved = a.rename("/Drive na nuvem/nota (2).txt", "/Drive na nuvem/Fotos/nota (2).txt");
        assert!(!moved.unwrap_err().contains("Já existe"));
        assert!(a.rename("/Drive na nuvem/nota (2).txt", "/Drive na nuvem/Fotos/praia.jpg").unwrap_err().contains("Já existe"));
        assert!(crate::nodes::twin_of("nota (2).txt", "nota.txt", true) && crate::nodes::twin_of("Fotos (3)", "Fotos", false));
        assert!(!crate::nodes::twin_of("nota (2).txt", "nota.md", true) && !crate::nodes::twin_of("nota.txt (2)", "nota.txt", true));
    }

    /// A stand-in for MEGA on a local port: `sc` answers by the sequence number asked for (none
    /// known: error -6, as MEGA turns away one it no longer has), `cs` answers `f` with
    /// `listing` and `uga` with ENOENT. Counts the `f` asked for.
    struct FakeMega {
        base: String,
        fs: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl FakeMega {
        fn start(sc: HashMap<&'static str, Value>, listing: Value) -> FakeMega {
            use std::io::{BufRead, BufReader, Read, Write};
            let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.local_addr().unwrap());
            let fs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = fs.clone();
            std::thread::spawn(move || {
                for conn in server.incoming() {
                    let Ok(mut conn) = conn else { return };
                    let mut reader = BufReader::new(conn.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        continue;
                    }
                    let mut len = 0;
                    loop {
                        let mut h = String::new();
                        if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                            break;
                        }
                        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                            len = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0; len];
                    let _ = reader.read_exact(&mut body);
                    let path = line.split(' ').nth(1).unwrap_or_default();
                    let answer = if path.starts_with("/sc") {
                        let sn = path.split(['?', '&']).find_map(|p| p.strip_prefix("sn=")).unwrap_or_default();
                        sc.get(sn).cloned().unwrap_or(json!(-6))
                    } else {
                        let cmds: Vec<Value> = serde_json::from_slice(&body).unwrap_or_default();
                        Value::Array(
                            cmds.iter()
                                .map(|c| match c["a"].as_str() {
                                    Some("f") => {
                                        counted.fetch_add(1, Ordering::SeqCst);
                                        listing.clone()
                                    }
                                    _ => json!(-9),
                                })
                                .collect(),
                        )
                    };
                    // Two stand-ins for a channel answering wrong: a server error, and a body
                    // that isn't JSON.
                    let (status, text) = match answer.as_str() {
                        Some("HTTP 500") => ("500 Internal Server Error", String::new()),
                        Some("not JSON") => ("200 OK", "<html>busy</html>".to_string()),
                        _ => ("200 OK", answer.to_string()),
                    };
                    let _ = write!(conn, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
                }
            });
            FakeMega { base, fs }
        }

        fn api(&self) -> Api {
            Api::new(&self.base)
        }

        fn listings(&self) -> usize {
            self.fs.load(Ordering::SeqCst)
        }
    }

    fn test_session(sek: u8) -> Session {
        let k = keys();
        Session { user: k.user.clone(), master: k.master, sek: [sek; 16], sid: "SID".into(), rsa: None }
    }

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oe-mega-start-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        cache::file_in(&dir, "ana@exemplo.com")
    }

    /// The kept tree as a start leaves it, from `listing` (its `sn` SN000000001).
    fn keep(file: &std::path::Path, sek: u8) {
        let mut k = keys();
        let tree = { let l = listing(&k); Tree::from_listing(&mut k, &l) };
        // The key of a share that may have ended since.
        k.shares.insert("OLDSHARE".into(), [3; 16]);
        let s = test_session(sek);
        cache::write(file,&cache::Secret::of(&s.user, &s.master, &s.sek), cache::encode(&tree, &k.shares)).unwrap();
    }

    /// The whole tree MEGA would send now: `listing` plus one folder made since.
    fn fresh_listing() -> Value {
        let k = keys();
        let mut l = listing(&k);
        l["f"].as_array_mut().unwrap().push(crate::nodes::tests::raw(&k, "LATER001", "ROOT0000", 1, "Feita depois", (k.user.as_str(), k.master)));
        l["sn"] = "SN000000009".into();
        l
    }

    #[test]
    fn a_start_reads_the_kept_tree_and_only_the_changes_since() {
        let file = temp_file("catch-up");
        keep(&file, 1);
        let k = keys();
        let new = crate::nodes::tests::raw(&k, "NEWFILE1", "FOLDER01", 0, "nova.jpg", (k.user.as_str(), k.master));
        let mut sc = HashMap::new();
        sc.insert("SN000000001", json!({"a": [{"a": "t", "t": {"f": [new]}}, {"a": "d", "n": "FILE0002"}], "sn": "SN000000002"}));
        sc.insert("SN000000002", json!({"w": "https://example.invalid/wait", "sn": "SN000000002"}));
        let mega = FakeMega::start(sc, fresh_listing());
        let a = Account::start_kept("ana@exemplo.com", mega.api(), test_session(1), Some(file.clone())).unwrap();
        assert!(a.started_from_cache());
        assert_eq!(mega.listings(), 0, "no whole listing asked for");
        assert!(a.resolve("/Drive na nuvem/Fotos/nova.jpg").is_some(), "a change since it was kept is in");
        assert!(a.resolve("/Drive na nuvem/nota.txt").is_none(), "and so is a deletion");
        assert_eq!(a.lock().tree.sn, "SN000000002");
        assert!(a.lock().keys.shares.contains_key("OLDSHARE"), "the kept share keys are taken in");
        // The changes are kept on the next save, with the new sequence number.
        a.save_cache_if_changed();
        let s = test_session(1);
        let (kept, _) = cache::read(&file, &cache::Secret::of(&s.user, &s.master, &s.sek)).unwrap();
        assert_eq!(kept.sn, "SN000000002");
        assert!(kept.get("NEWFILE1").is_some() && kept.get("FILE0002").is_none());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// MEGA turning the kept sequence number away, a share changed meanwhile (only a fresh
    /// listing has its keys), the channel answering an error page or something unreadable, a
    /// file of another session, a damaged one: each falls back to the whole listing, and the
    /// file kept after is the fresh tree.
    #[test]
    fn a_kept_tree_that_cant_be_brought_up_to_date_is_thrown_away() {
        let cases: [(&str, Option<Value>, u8); 6] = [
            ("rejected", None, 1),
            ("share", Some(json!({"a": [{"a": "s2", "n": "FOLDER01"}], "sn": "SN000000002"})), 1),
            ("http-500", Some(json!("HTTP 500")), 1),
            ("not-json", Some(json!("not JSON")), 1),
            ("other-session", Some(json!({"w": "x", "sn": "SN000000001"})), 2),
            ("damaged", Some(json!({"w": "x", "sn": "SN000000001"})), 1),
        ];
        for (name, answer, sek) in cases {
            let file = temp_file(name);
            keep(&file, 1);
            if name == "damaged" {
                let mut raw = std::fs::read(&file).unwrap();
                let mid = raw.len() / 2;
                raw[mid] ^= 0x80;
                std::fs::write(&file, raw).unwrap();
            }
            let mut sc = HashMap::new();
            if let Some(answer) = answer {
                sc.insert("SN000000001", answer);
            }
            let mega = FakeMega::start(sc, fresh_listing());
            let a = Account::start_kept("ana@exemplo.com", mega.api(), test_session(sek), Some(file.clone())).unwrap();
            assert!(!a.started_from_cache(), "{name}");
            assert_eq!(mega.listings(), 1, "{name}: the whole listing read once");
            assert!(a.resolve("/Drive na nuvem/Feita depois").is_some(), "{name}: the fresh tree is shown");
            assert!(!a.lock().keys.shares.contains_key("OLDSHARE"), "{name}: only the share keys the whole listing brings");
            let s = test_session(sek);
            let (kept, _) = cache::read(&file, &cache::Secret::of(&s.user, &s.master, &s.sek)).unwrap();
            assert_eq!(kept.sn, "SN000000009", "{name}: the fresh tree is kept");
            let _ = std::fs::remove_dir_all(file.parent().unwrap());
        }
    }

    /// Without a network the start fails, as it would without a kept tree: a tree that couldn't
    /// be brought up to date isn't shown. The file stays for the next try.
    #[test]
    fn no_network_fails_the_start_and_keeps_the_file() {
        let file = temp_file("offline");
        keep(&file, 1);
        assert!(Account::start_kept("ana@exemplo.com", Api::unreachable(), test_session(1), Some(file.clone())).is_err());
        assert!(file.exists());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn a_removed_account_writes_no_tree() {
        let file = temp_file("removed");
        let mut k = keys();
        let tree = { let l = listing(&k); Tree::from_listing(&mut k, &l) };
        let mut a = Account::offline(k, tree);
        let s = test_session(0);
        a.kept = Some((file.clone(), cache::Secret::of(&s.user, &s.master, &s.sek)));
        a.save_cache();
        assert!(file.exists());
        cache::forget_file(&file);
        a.forgotten.store(true, Ordering::SeqCst);
        a.save_cache();
        assert!(!file.exists(), "a save after the account was removed writes nothing");
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn paths_split_at_the_last_slash() {
        assert_eq!(split("/a/b/c"), ("/a/b", "c"));
        assert_eq!(split("/a"), ("/", "a"));
        assert_eq!(last_name(&handle_path("XYZ")), "XYZ");
        assert!(check_name("a/b").is_err() && check_name("..").is_err() && check_name("ok").is_ok());
    }
}
