//! An open OneDrive account: its folders by path, kept fresh by the delta feed, and what can be
//! done to them.
//!
//! Paths are the panes': `/` is the account's top, which holds one area, Meus arquivos (the
//! drive's root); below it, the drive's folders by name. Items shared from other drives that
//! show in this one (shortcuts) open there: their contents are read live, not kept.
//!
//! Every write takes Graph's answer (the item as it is now) into the tree at once; the delta
//! feed brings the same state again later, which changes nothing.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use openexplorer_i18n::{t, tr};
use serde_json::{Value, json};

use crate::graph::{Body, Endpoints, Fail, Graph, Res, Sub, segment, transfer_agent};
use crate::tree::{Ix, SELECT, Seen, Tree, Walk};
use crate::{PendingLogin, Result, cache, log, vault};

pub use crate::tree::Changes;

/// A folder checked against the delta feed this recently is taken as current without asking.
const FRESH: Duration = Duration::from_secs(3);
/// More pages of changes than this and the listings are read again instead.
const MOST_PAGES: usize = 20;
/// How often a tree that changed is written down while the account is used.
const SAVE_EVERY: Duration = Duration::from_secs(60);
/// The page size asked of listings and of the delta feed.
const PAGE: u32 = 1000;

/// The places at the account's top.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Area {
    /// The drive's root.
    Files,
}

impl Area {
    pub const ALL: [Area; 1] = [Area::Files];

    pub fn label(self) -> &'static str {
        match self {
            Area::Files => t("Meus arquivos", "My files"),
        }
    }

    /// Its name in either language: an address typed in one still opens in the other.
    pub fn of(name: &str) -> Option<Area> {
        Area::ALL.into_iter().find(|a| match a {
            Area::Files => ["Meus arquivos", "My files"].iter().any(|n| crate::tree::same_name(n, name)),
        })
    }
}

/// A personal Microsoft account's OneDrive, or a work or school one's (OneDrive for Business).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Personal,
    Business,
}

/// What a folder holds, as the panes show it.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    /// Seconds since 1970.
    pub modified: Option<i64>,
    /// The item's id on its drive (none for the top and its areas).
    pub id: Option<String>,
}

/// The drive's storage.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Quota {
    pub total: u64,
    pub used: u64,
    pub remaining: u64,
    /// In the recycle bin.
    pub deleted: u64,
    /// `normal`, `nearing`, `critical` or `exceeded`.
    pub state: String,
}

impl Quota {
    pub fn parse(v: &Value) -> Quota {
        let n = |k: &str| v.pointer(&format!("/quota/{k}")).and_then(Value::as_u64).unwrap_or(0);
        Quota { total: n("total"), used: n("used"), remaining: n("remaining"), deleted: n("deleted"), state: v.pointer("/quota/state").and_then(Value::as_str).unwrap_or("normal").to_string() }
    }
}

/// Who an account is.
#[derive(Clone, Debug, PartialEq)]
pub struct Meta {
    /// The drive's id: the account's key everywhere.
    pub id: String,
    pub email: String,
    pub name: String,
    pub kind: Kind,
}

/// Where an item is on Graph: by id on a drive, then perhaps by path below it (an item not
/// made yet, or one in another drive not looked up).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Addr {
    pub drive: String,
    pub id: String,
    pub rest: Vec<String>,
}

impl Addr {
    pub fn url(&self) -> String {
        let mut u = format!("/drives/{}/items/{}", segment(&self.drive), segment(&self.id));
        if !self.rest.is_empty() {
            u.push_str(":/");
            u.push_str(&self.rest.iter().map(|s| segment(s)).collect::<Vec<_>>().join("/"));
            u.push(':');
        }
        u
    }

    pub fn child(&self, name: &str) -> Addr {
        let mut a = self.clone();
        a.rest.push(name.to_string());
        a
    }
}

/// An item found by its path.
#[derive(Clone, Debug)]
pub(crate) struct Loc {
    pub addr: Addr,
    /// Its place in the tree, when it's this drive's and listed.
    pub own: Option<Ix>,
    pub item: Item,
    pub parent: Option<String>,
}

enum Place {
    Top,
    Found(Loc),
    Missing,
}

struct St {
    tree: Tree,
    /// When the tree was last brought up to date with the delta feed.
    checked: Option<Instant>,
    /// Bumped whenever changes were taken in: a listing read while one arrived isn't kept.
    epoch: u64,
    dirty: bool,
    saved: Instant,
}

pub struct Account {
    pub meta: Meta,
    pub(crate) graph: Graph,
    st: Mutex<St>,
    /// One delta catch-up at a time.
    catching: Mutex<()>,
    /// Removed: nothing more is written for it.
    pub(crate) forgotten: AtomicBool,
    keep: Option<(PathBuf, cache::Secret)>,
    mark: u64,
    from_cache: bool,
}

fn parts(path: &str) -> Vec<String> {
    path.split('/').filter(|s| !s.is_empty()).map(str::to_string).collect()
}

fn item_of(s: &Seen) -> Item {
    Item { name: s.name.clone().unwrap_or_default(), is_dir: s.is_dir, size: (!s.is_dir).then_some(s.size), modified: (s.modified > 0).then_some(s.modified), id: Some(s.id.clone()) }
}

fn area_item(a: Area) -> Item {
    Item { name: a.label().to_string(), is_dir: true, size: None, modified: None, id: None }
}

/// Names OneDrive refuses (on either kind of account).
fn check_name(name: &str) -> Res<()> {
    if name.trim().is_empty() || name == "." || name == ".." {
        return Err(Fail::new(t("Esse nome não pode ser usado.", "That name can't be used.")));
    }
    if let Some(c) = name.chars().find(|c| r#"/\*<>?:|""#.contains(*c)) {
        return Err(Fail::new(tr!("O OneDrive não aceita {c} em nomes.", "OneDrive doesn't accept {c} in names.")));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(Fail::new(t("O OneDrive não aceita nomes terminados em ponto ou espaço.", "OneDrive doesn't accept names ending in a dot or a space.")));
    }
    Ok(())
}

fn not_found(path: &str) -> Fail {
    Fail { status: 404, code: "itemNotFound".into(), text: tr!("Não encontrei {path} no OneDrive.", "Couldn't find {path} on OneDrive.") }
}

impl Account {
    /// Opens the account: its kept tree brought up to date (one delta request when nothing
    /// changed), else a fresh delta position and the drive's top.
    pub fn start(meta: Meta, graph: Graph, keep: Option<(PathBuf, cache::Secret)>) -> Res<Account> {
        let mark = cache::mark();
        let kept = keep.as_ref().and_then(|(file, secret)| match cache::read(file, secret) {
            Ok(t) if t.root.is_some() && t.delta.is_some() => Some(t),
            Ok(_) | Err(cache::Refusal::Missing) => None,
            Err(e) => {
                log(&format!("OneDrive {}: árvore guardada recusada ({e:?}); lendo de novo", meta.id));
                cache::forget_file(file);
                None
            }
        });
        let from_cache = kept.is_some();
        let st = St { tree: kept.unwrap_or_default(), checked: None, epoch: 0, dirty: !from_cache, saved: Instant::now() };
        let a = Account { meta, graph, st: Mutex::new(st), catching: Mutex::new(()), forgotten: AtomicBool::new(false), keep, mark, from_cache };
        if from_cache {
            a.catch_up()?;
        } else {
            a.establish()?;
        }
        Ok(a)
    }

    pub fn started_from_cache(&self) -> bool {
        self.from_cache
    }

    fn st(&self) -> std::sync::MutexGuard<'_, St> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn drive(&self) -> &str {
        &self.meta.id
    }

    // -----------------------------------------------------------------------------------------
    // Keeping up with the delta feed

    /// A delta position from now (`token=latest`, no enumeration) and the drive's top.
    fn establish(&self) -> Res<()> {
        let v = self.graph.get(&format!("/drives/{}/root/delta?$select={SELECT}&$top={PAGE}&token=latest", segment(self.drive())), t("Ler as mudanças", "Read the changes"))?;
        let link = v.get("@odata.deltaLink").and_then(Value::as_str).ok_or_else(|| Fail::new(t("O OneDrive não deu a posição das mudanças.", "OneDrive gave no change position.")))?.to_string();
        let need_root = self.st().tree.root.is_none();
        let root = if need_root { Some(self.graph.get(&format!("/drives/{}/root?$select={SELECT}", segment(self.drive())), t("Abrir o OneDrive", "Open OneDrive"))?) } else { None };
        let mut st = self.st();
        if let Some(r) = root.as_ref().and_then(Seen::of) {
            st.tree.set_root(&Seen { root: true, ..r });
        }
        st.tree.delta = Some(link);
        st.checked = Some(Instant::now());
        st.dirty = true;
        Ok(())
    }

    /// The listings dropped and a new position taken: the delta position was refused (410) or
    /// too much changed to read through.
    fn resync(&self) -> Res<Changes> {
        {
            let mut st = self.st();
            st.tree.drop_listings();
            st.tree.delta = None;
            st.epoch += 1;
            st.dirty = true;
        }
        self.establish()?;
        Ok(Changes { all: true, folders: HashSet::new() })
    }

    /// Reads the delta feed from the kept position and takes it in.
    pub fn catch_up(&self) -> Res<Changes> {
        let _one = self.catching.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut url) = self.st().tree.delta.clone() else {
            self.establish()?;
            return Ok(Changes { all: true, folders: HashSet::new() });
        };
        let mut seen = Vec::new();
        let mut pages = 0;
        let link = loop {
            let reply = self.graph.send("GET", &url, Body::None, &[])?;
            if reply.status == 410 {
                log(&format!("OneDrive {}: posição de mudanças recusada (410); listas de novo", self.meta.id));
                return self.resync();
            }
            let v = reply.json(t("Ler as mudanças", "Read the changes"))?;
            seen.extend(v.get("value").and_then(Value::as_array).into_iter().flatten().filter_map(Seen::of));
            if let Some(next) = v.get("@odata.nextLink").and_then(Value::as_str) {
                pages += 1;
                if pages >= MOST_PAGES {
                    log(&format!("OneDrive {}: mais de {MOST_PAGES} páginas de mudanças; listas de novo", self.meta.id));
                    return self.resync();
                }
                url = next.to_string();
                continue;
            }
            match v.get("@odata.deltaLink").and_then(Value::as_str) {
                Some(l) => break l.to_string(),
                None => return Err(Fail::new(t("O OneDrive não deu a posição das mudanças.", "OneDrive gave no change position."))),
            }
        };
        let mut touched = HashSet::new();
        let mut st = self.st();
        for s in &seen {
            st.tree.apply(s, &mut touched);
        }
        st.tree.delta = Some(link);
        st.checked = Some(Instant::now());
        if !seen.is_empty() {
            st.epoch += 1;
            st.dirty = true;
        }
        Ok(Changes { all: false, folders: touched })
    }

    /// Brought up to date unless it was a moment ago.
    fn fresh(&self) -> Res<()> {
        if self.st().checked.is_some_and(|c| c.elapsed() < FRESH) {
            return Ok(());
        }
        self.catch_up().map(|_| ())
    }

    /// What changed elsewhere since the last ask (the app asks every few seconds while a pane
    /// shows the account), the tree written down when due.
    pub fn poll(&self) -> Result<Changes> {
        let c = self.catch_up()?;
        self.save_if_due();
        Ok(c)
    }

    // -----------------------------------------------------------------------------------------
    // Finding and listing

    /// Every page of a listing at `url`.
    fn pages(&self, url: String, what: &str) -> Res<Vec<Seen>> {
        let mut out = Vec::new();
        let mut next = Some(url);
        while let Some(u) = next {
            let v = self.graph.get(&u, what)?;
            out.extend(v.get("value").and_then(Value::as_array).into_iter().flatten().filter_map(Seen::of));
            next = v.get("@odata.nextLink").and_then(Value::as_str).map(str::to_string);
        }
        Ok(out)
    }

    /// Lists this drive's folder `id` from Graph and keeps the listing, unless changes came in
    /// while it was read (then it's only returned).
    fn list_own(&self, id: &str) -> Res<Vec<Seen>> {
        let epoch = self.st().epoch;
        let url = format!("/drives/{}/items/{}/children?$select={SELECT}&$top={PAGE}", segment(self.drive()), segment(id));
        let seen = self.pages(url, t("Listar a pasta", "List the folder"))?;
        let mut st = self.st();
        if st.epoch == epoch
            && let Some(ix) = st.tree.find(id)
            && st.tree.get(ix).is_dir
        {
            st.tree.set_listing(ix, &seen);
            st.dirty = true;
        }
        Ok(seen)
    }

    fn own_loc(&self, tree: &Tree, ix: Ix) -> Loc {
        let n = tree.get(ix);
        let item = Item { name: n.name.to_string(), is_dir: n.is_dir, size: (!n.is_dir).then_some(n.size), modified: (n.modified > 0).then_some(n.modified), id: Some(n.id.to_string()) };
        let addr = match &n.remote {
            Some(r) => Addr { drive: r.drive.clone(), id: r.id.clone(), rest: Vec::new() },
            None => Addr { drive: self.meta.id.clone(), id: n.id.to_string(), rest: Vec::new() },
        };
        Loc { addr, own: n.remote.is_none().then_some(ix), item, parent: n.parent.map(|p| tree.get(p).id.to_string()) }
    }

    /// An item asked of Graph by its address.
    fn live(&self, addr: &Addr) -> Res<Option<Loc>> {
        match self.graph.get(&format!("{}?$select={SELECT}", addr.url()), t("Abrir", "Open")) {
            Ok(v) => {
                let s = Seen::of(&v).ok_or_else(|| Fail::new(t("Resposta inesperada do OneDrive.", "Unexpected answer from OneDrive.")))?;
                let drive = v.pointer("/parentReference/driveId").and_then(Value::as_str).unwrap_or(&addr.drive).to_string();
                let a = match &s.remote {
                    Some(r) => Addr { drive: r.drive.clone(), id: r.id.clone(), rest: Vec::new() },
                    None => Addr { drive, id: s.id.clone(), rest: Vec::new() },
                };
                Ok(Some(Loc { addr: a, own: None, item: item_of(&s), parent: s.parent.clone() }))
            }
            Err(f) if f.not_found() => Ok(None),
            Err(f) => Err(f),
        }
    }

    fn locate(&self, path: &str) -> Res<Place> {
        let segs = parts(path);
        let Some(first) = segs.first() else { return Ok(Place::Top) };
        if Area::of(first).is_none() {
            return Ok(Place::Missing);
        }
        self.fresh()?;
        let rest = &segs[1..];
        for _ in 0..rest.len() + 2 {
            let walk = self.st().tree.walk(rest);
            match walk {
                Walk::Found(ix) => {
                    let st = self.st();
                    return Ok(Place::Found(self.own_loc(&st.tree, ix)));
                }
                Walk::Missing => return Ok(Place::Missing),
                Walk::Remote(ix, depth) => {
                    let base = {
                        let st = self.st();
                        self.own_loc(&st.tree, ix)
                    };
                    if depth == rest.len() {
                        return Ok(Place::Found(base));
                    }
                    let addr = Addr { rest: rest[depth..].to_vec(), ..base.addr };
                    return Ok(self.live(&addr)?.map_or(Place::Missing, Place::Found));
                }
                Walk::Unlisted(ix, _) => {
                    let id = self.st().tree.get(ix).id.to_string();
                    self.list_own(&id)?;
                }
            }
        }
        // Changes kept arriving while folders were listed: asked by path instead.
        let root = {
            let st = self.st();
            st.tree.root.map(|r| st.tree.get(r).id.to_string()).unwrap_or_default()
        };
        let addr = Addr { drive: self.meta.id.clone(), id: root, rest: rest.to_vec() };
        Ok(self.live(&addr)?.map_or(Place::Missing, Place::Found))
    }

    fn found(&self, path: &str) -> Res<Loc> {
        match self.locate(path)? {
            Place::Found(l) => Ok(l),
            Place::Top => Err(Fail::new(t("O topo da conta não pode mudar.", "The account's top can't change."))),
            Place::Missing => Err(not_found(path)),
        }
    }

    /// The folder `path` goes in, and its name there.
    fn parent_of(&self, path: &str) -> Res<(Loc, String)> {
        let segs = parts(path);
        if segs.len() < 2 {
            return Err(Fail::new(t("O topo da conta só tem Meus arquivos: abra essa pasta.", "The account's top only holds My files: open that folder.")));
        }
        let name = segs[segs.len() - 1].clone();
        let parent = format!("/{}", segs[..segs.len() - 1].join("/"));
        let loc = self.found(&parent)?;
        if !loc.item.is_dir {
            return Err(Fail::new(tr!("{parent} não é uma pasta.", "{parent} isn't a folder.")));
        }
        Ok((loc, name))
    }

    /// Folder `path`'s items.
    pub fn list(&self, path: &str) -> Result<Vec<Item>> {
        let loc = match self.locate(path)? {
            Place::Top => return Ok(Area::ALL.into_iter().map(area_item).collect()),
            Place::Missing => return Err(not_found(path).into()),
            Place::Found(l) => l,
        };
        if !loc.item.is_dir {
            return Err(tr!("{path} não é uma pasta.", "{path} isn't a folder."));
        }
        if let Some(ix) = loc.own {
            let st = self.st();
            if let Some(kids) = st.tree.children(ix) {
                return Ok(kids.iter().map(|k| self.own_loc(&st.tree, *k).item).collect());
            }
            drop(st);
            return Ok(self.list_own(&loc.addr.id)?.iter().map(item_of).collect());
        }
        let url = format!("{}/children?$select={SELECT}&$top={PAGE}", loc.addr.url());
        Ok(self.pages(url, t("Listar a pasta", "List the folder"))?.iter().map(item_of).collect())
    }

    /// The item at `path`, if there's one.
    pub fn stat(&self, path: &str) -> Result<Option<Item>> {
        Ok(match self.locate(path)? {
            Place::Top => Some(Item { name: String::new(), is_dir: true, size: None, modified: None, id: None }),
            Place::Found(_) if parts(path).len() == 1 => Area::of(&parts(path)[0]).map(area_item),
            Place::Found(l) => Some(l.item),
            Place::Missing => None,
        })
    }

    /// The id of the folder a pane shows at `path`, from what's kept (no request): the delta
    /// tells which folders changed by id.
    pub fn folder_id(&self, path: &str) -> Option<String> {
        let segs = parts(path);
        Area::of(segs.first()?)?;
        let st = self.st();
        match st.tree.walk(&segs[1..]) {
            Walk::Found(ix) | Walk::Unlisted(ix, _) | Walk::Remote(ix, _) => Some(st.tree.get(ix).id.to_string()),
            Walk::Missing => None,
        }
    }

    /// The item's id at `path` (for undo: a deleted item is restored by it).
    pub fn id_of(&self, path: &str) -> Result<String> {
        Ok(self.found(path)?.addr.id)
    }

    pub(crate) fn found_addr(&self, path: &str) -> Res<Addr> {
        Ok(self.found(path)?.addr)
    }

    /// The address a new item at `path` gets (by its folder's id and its name), and the name.
    pub(crate) fn new_child(&self, path: &str) -> Res<(String, String)> {
        let (parent, name) = self.parent_of(path)?;
        check_name(&name)?;
        Ok((parent.addr.child(&name).url(), name))
    }

    // -----------------------------------------------------------------------------------------
    // Changing

    /// Takes an item Graph answered with into the tree.
    pub(crate) fn took(&self, v: &Value) {
        if let Some(s) = Seen::of(v) {
            let mut st = self.st();
            st.tree.apply(&s, &mut HashSet::new());
            st.dirty = true;
        }
    }

    fn gone(&self, id: &str) {
        let mut st = self.st();
        if let Some(ix) = st.tree.find(id) {
            st.tree.remove(ix);
            st.dirty = true;
        }
    }

    pub fn mkdir(&self, path: &str) -> Result<Item> {
        let (parent, name) = self.parent_of(path)?;
        check_name(&name)?;
        let body = json!({ "name": name, "folder": {}, "@microsoft.graph.conflictBehavior": "fail" });
        let v = self.graph.call("POST", &format!("{}/children", parent.addr.url()), Some(&body), t("Criar a pasta", "Create the folder"))?;
        self.took(&v);
        Ok(Seen::of(&v).map(|s| item_of(&s)).unwrap_or_else(|| Item { name, is_dir: true, size: None, modified: None, id: None }))
    }

    /// Renames or moves within the drive. Fails if `to` exists (but for the item itself, in
    /// another case).
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        let src = self.found(from)?;
        let (dst, name) = self.parent_of(to)?;
        check_name(&name)?;
        if let Place::Found(there) = self.locate(to)?
            && there.addr != src.addr
        {
            return Err(tr!("Já existe {to} no OneDrive.", "{to} already exists on OneDrive."));
        }
        if src.addr.drive != dst.addr.drive {
            return Err(t("O OneDrive não move itens entre o seu drive e uma pasta compartilhada: copie e depois exclua.", "OneDrive doesn't move items between your drive and a shared folder: copy, then delete.").into());
        }
        let mut body = json!({ "name": name });
        if src.parent.as_deref() != Some(dst.addr.id.as_str()) {
            body["parentReference"] = json!({ "id": dst.addr.id });
        }
        let v = self.graph.call("PATCH", &src.addr.url(), Some(&body), t("Renomear", "Rename"))?;
        self.took(&v);
        Ok(())
    }

    /// Sends a file or a whole folder to the recycle bin.
    pub fn remove(&self, path: &str) -> Result<()> {
        let src = self.found(path)?;
        let r = self.graph.send("DELETE", &src.addr.url(), Body::None, &[])?;
        if !r.ok() && r.status != 404 {
            return Err(crate::graph::failure(r.status, &r.body, t("Excluir", "Delete")).into());
        }
        self.gone(&src.addr.id);
        Ok(())
    }

    /// Several items to the recycle bin in as few requests as it takes (`$batch`): what went
    /// (id and name, for undo), and the first error.
    pub fn trash_many(&self, paths: &[String]) -> (Vec<(String, String)>, Option<String>) {
        let mut locs = Vec::new();
        for p in paths {
            match self.found(p) {
                Ok(l) => locs.push(l),
                Err(e) => return (Vec::new(), Some(e.text)),
            }
        }
        let subs: Vec<Sub> = locs.iter().map(|l| Sub { method: "DELETE", url: l.addr.url(), body: None }).collect();
        let answers = match self.graph.batch(&subs, t("Excluir", "Delete")) {
            Ok(a) => a,
            Err(e) => return (Vec::new(), Some(e.text)),
        };
        let mut done = Vec::new();
        let mut first = None;
        for (l, (status, body)) in locs.iter().zip(answers) {
            if (200..300).contains(&status) || status == 404 {
                self.gone(&l.addr.id);
                done.push((l.addr.id.clone(), l.item.name.clone()));
            } else if first.is_none() {
                first = Some(crate::graph::failure(status, &body.to_string(), &l.item.name).text);
            }
        }
        (done, first)
    }

    /// Brings items back from the recycle bin by id, where they were. Personal accounts only:
    /// Graph doesn't restore on work or school ones.
    pub fn restore_many(&self, ids: &[String]) -> (Vec<String>, Option<String>) {
        if self.meta.kind == Kind::Business {
            return (Vec::new(), Some(business_bin().into()));
        }
        let subs: Vec<Sub> = ids.iter().map(|id| Sub { method: "POST", url: format!("/drives/{}/items/{}/restore", segment(self.drive()), segment(id)), body: Some(json!({})) }).collect();
        let answers = match self.graph.batch(&subs, t("Restaurar", "Restore")) {
            Ok(a) => a,
            Err(e) => return (Vec::new(), Some(e.text)),
        };
        let mut done = Vec::new();
        let mut first = None;
        for (id, (status, body)) in ids.iter().zip(answers) {
            if (200..300).contains(&status) {
                self.took(&body);
                done.push(id.clone());
            } else if first.is_none() {
                first = Some(crate::graph::failure(status, &body.to_string(), t("Restaurar", "Restore")).text);
            }
        }
        (done, first)
    }

    pub fn restore(&self, id: &str) -> Result<()> {
        let (_, err) = self.restore_many(&[id.to_string()]);
        err.map_or(Ok(()), Err)
    }

    /// Copies a file or a folder within OneDrive, on Microsoft's side, and waits for it.
    pub fn copy(&self, from: &str, to: &str) -> Result<()> {
        let src = self.found(from)?;
        let (dst, name) = self.parent_of(to)?;
        check_name(&name)?;
        if let Place::Found(_) = self.locate(to)? {
            return Err(tr!("Já existe {to} no OneDrive.", "{to} already exists on OneDrive."));
        }
        let body = json!({ "parentReference": { "driveId": dst.addr.drive, "id": dst.addr.id }, "name": name });
        let r = self.graph.send("POST", &format!("{}/copy", src.addr.url()), Body::Json(&body), &[])?;
        if !r.ok() {
            return Err(crate::graph::failure(r.status, &r.body, t("Copiar", "Copy")).into());
        }
        let monitor = r.location.ok_or_else(|| t("O OneDrive não deu como acompanhar a cópia.", "OneDrive gave no way to follow the copy."))?;
        let id = wait_copy(&monitor)?;
        if dst.addr.drive == self.meta.id
            && let Some(id) = id
            && let Ok(v) = self.graph.get(&format!("/drives/{}/items/{}?$select={SELECT}", segment(self.drive()), segment(&id)), t("Copiar", "Copy"))
        {
            self.took(&v);
        }
        Ok(())
    }

    pub fn quota(&self) -> Result<Quota> {
        let v = self.graph.get(&format!("/drives/{}?$select=quota", segment(self.drive())), t("Ler a cota", "Read the quota"))?;
        Ok(Quota::parse(&v))
    }

    // -----------------------------------------------------------------------------------------
    // Sharing links

    fn link_permissions(&self, loc: &Loc) -> Res<Vec<Value>> {
        let v = self.graph.get(&format!("{}/permissions", loc.addr.url()), t("Ler o compartilhamento", "Read the sharing"))?;
        Ok(v.get("value")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|p| p.pointer("/link/webUrl").is_some() && p.get("inheritedFrom").is_none_or(Value::is_null) && matches!(p.pointer("/link/scope").and_then(Value::as_str), Some("anonymous" | "organization")))
            .cloned()
            .collect())
    }

    /// The item's sharing link (anyone, or anyone in the organization), if it has one.
    pub fn link_of(&self, path: &str) -> Result<Option<String>> {
        let loc = self.found(path)?;
        Ok(self.link_permissions(&loc)?.iter().find_map(|p| p.pointer("/link/webUrl").and_then(Value::as_str).map(str::to_string)))
    }

    /// Makes a view link: for anyone with it, or (where the organization allows no such
    /// links) for anyone in the organization.
    pub fn create_link(&self, path: &str) -> Result<String> {
        let loc = self.found(path)?;
        let url = format!("{}/createLink", loc.addr.url());
        let mut v = self.graph.send("POST", &url, Body::Json(&json!({ "type": "view", "scope": "anonymous" })), &[])?;
        if !v.ok() && self.meta.kind == Kind::Business && v.status != 404 {
            v = self.graph.send("POST", &url, Body::Json(&json!({ "type": "view", "scope": "organization" })), &[])?;
        }
        let v = v.json(t("Criar o link", "Make the link"))?;
        v.pointer("/link/webUrl").and_then(Value::as_str).map(str::to_string).ok_or_else(|| t("O OneDrive não deu o link.", "OneDrive gave no link.").into())
    }

    /// Removes the item's sharing links; how many went.
    pub fn remove_link(&self, path: &str) -> Result<usize> {
        let loc = self.found(path)?;
        let perms = self.link_permissions(&loc)?;
        for p in &perms {
            let id = p.get("id").and_then(Value::as_str).unwrap_or_default();
            let r = self.graph.send("DELETE", &format!("{}/permissions/{}", loc.addr.url(), segment(id)), Body::None, &[])?;
            if !r.ok() && r.status != 404 {
                return Err(crate::graph::failure(r.status, &r.body, t("Remover o link", "Remove the link")).into());
            }
        }
        Ok(perms.len())
    }

    // -----------------------------------------------------------------------------------------
    // Kept on disk

    /// Writes the tree down now (when there's a folder to keep it in).
    pub fn save_cache(&self) {
        let Some((file, secret)) = &self.keep else { return };
        let snap = {
            let mut st = self.st();
            st.dirty = false;
            st.saved = Instant::now();
            st.tree.snapshot()
        };
        match cache::write(file, secret, &snap, &self.forgotten, self.mark) {
            Ok(_) => {}
            Err(e) => {
                log(&format!("OneDrive {}: não consegui guardar a árvore: {e}", self.meta.id));
                self.st().dirty = true;
            }
        }
    }

    pub fn save_cache_if_changed(&self) {
        if self.st().dirty {
            self.save_cache();
        }
    }

    fn save_if_due(&self) {
        let due = {
            let st = self.st();
            st.dirty && st.saved.elapsed() >= SAVE_EVERY
        };
        if due {
            self.save_cache();
        }
    }

    /// How many items the tree holds (for the log and tests).
    pub fn kept(&self) -> usize {
        self.st().tree.len()
    }
}

pub fn business_bin() -> &'static str {
    t(
        "Contas corporativas ou de estudante não deixam aplicativos restaurar da lixeira: restaure pela lixeira do OneDrive no site.",
        "Work or school accounts don't let apps restore from the recycle bin: restore it from OneDrive's recycle bin on the website.",
    )
}

/// Follows a copy's monitor (no token: the address is its own key) until it ends: the new
/// item's id when said.
fn wait_copy(monitor: &str) -> Result<Option<String>> {
    let agent = transfer_agent();
    let started = Instant::now();
    let mut wait = Duration::from_millis(250);
    loop {
        let mut resp = agent.get(monitor).call().map_err(|e| tr!("Não consegui acompanhar a cópia: {e}", "Couldn't follow the copy: {e}"))?;
        let status = resp.status().as_u16();
        if (300..400).contains(&status) {
            // Done: the monitor sends to the new item.
            let to = resp.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or_default();
            return Ok(to.split("/items/").nth(1).map(|s| s.split(['?', '/']).next().unwrap_or(s).to_string()));
        }
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        if status == 429 || status == 503 {
            std::thread::sleep(Duration::from_secs(crate::graph::retry_after(&resp).unwrap_or(2).min(60)));
            continue;
        }
        if !(200..300).contains(&status) {
            return Err(crate::graph::failure(status, &body, t("Copiar", "Copy")).text);
        }
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        match v.get("status").and_then(Value::as_str).unwrap_or("inProgress") {
            "completed" => return Ok(v.get("resourceId").and_then(Value::as_str).map(str::to_string)),
            "failed" | "deleteFailed" => {
                let code = v.pointer("/error/code").and_then(Value::as_str).unwrap_or_default();
                return Err(if code == "nameAlreadyExists" {
                    t("Copiar: já existe um item com esse nome.", "Copy: an item with that name already exists.").into()
                } else {
                    tr!("O OneDrive não conseguiu copiar: {}", "OneDrive couldn't copy: {}", v.pointer("/error/message").and_then(Value::as_str).unwrap_or(code))
                });
            }
            _ => {}
        }
        if started.elapsed() > Duration::from_secs(60 * 60) {
            return Err(t("A cópia no OneDrive não terminou em uma hora.", "The copy on OneDrive didn't end within an hour.").into());
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(Duration::from_secs(2));
    }
}

// ---------------------------------------------------------------------------------------------
// The accounts open now

static OPEN: LazyLock<Mutex<HashMap<String, Arc<Account>>>> = LazyLock::new(Mutex::default);
/// One lock per account, so two panes reaching it at once open it once.
static OPENING: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = LazyLock::new(Mutex::default);

/// The account open for drive `id`, if it is.
pub fn cached(id: &str) -> Option<Arc<Account>> {
    OPEN.lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned()
}

/// The account's secret as kept, once it is.
type Held = Arc<Mutex<Option<(String, vault::Secret)>>>;

/// Keeps each new refresh token in the credential store, with the rest of the secret.
fn keeper(held: Held) -> Box<dyn Fn(&str) + Send + Sync> {
    Box::new(move |new: &str| {
        let mut held = held.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((id, s)) = held.as_mut() {
            s.refresh = new.to_string();
            if let Err(e) = vault::put(id, s) {
                log(&format!("OneDrive {id}: não consegui guardar o novo token: {e}"));
            }
        }
    })
}

fn kept_file(id: &str, secret: &vault::Secret) -> Option<(PathBuf, cache::Secret)> {
    Some((cache::file_of(id)?, cache::Secret::of(id, &secret.key()?)))
}

fn meta_of(id: &str, s: &vault::Secret) -> Meta {
    Meta { id: id.to_string(), email: s.email.clone(), name: s.name.clone(), kind: if s.business { Kind::Business } else { Kind::Personal } }
}

/// The account of drive `id`: open already, or opened from what the credential store keeps.
pub fn reach(id: &str) -> Result<Arc<Account>> {
    if let Some(a) = cached(id) {
        return Ok(a);
    }
    let gate = OPENING.lock().unwrap_or_else(|e| e.into_inner()).entry(id.to_string()).or_default().clone();
    let _held = gate.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(a) = cached(id) {
        return Ok(a);
    }
    let secret = vault::get(id)?;
    let held: Held = Arc::new(Mutex::new(Some((id.to_string(), secret.clone()))));
    let graph = Graph::new(secret.client(), Endpoints::default(), secret.refresh.clone(), keeper(held));
    let account = Arc::new(Account::start(meta_of(id, &secret), graph, kept_file(id, &secret))?);
    log(&format!("OneDrive {id} aberto ({} itens guardados, do disco: {})", account.kept(), account.started_from_cache()));
    OPEN.lock().unwrap_or_else(|e| e.into_inner()).insert(id.to_string(), account.clone());
    Ok(account)
}

/// Ends a login the browser came back from: who signed in, their drive, and the account kept
/// in the credential store.
pub fn connect(login: &PendingLogin) -> Result<Arc<Account>> {
    let answer = login.finish()?;
    let held: Held = Arc::new(Mutex::new(None));
    let graph = Graph::from_login(login.cfg.clone(), login.ends.clone(), &answer, keeper(held.clone()))?;
    let me = graph.get("/me?$select=displayName,mail,userPrincipalName", t("Ler a conta", "Read the account"))?;
    let drive = graph.get("/me/drive?$select=id,driveType", t("Abrir o OneDrive", "Open OneDrive"))?;
    let id = drive.get("id").and_then(Value::as_str).ok_or_else(|| t("A conta não tem OneDrive.", "The account has no OneDrive."))?.to_string();
    let business = drive.get("driveType").and_then(Value::as_str) != Some("personal");
    let email = me.get("mail").and_then(Value::as_str).filter(|m| !m.is_empty()).or_else(|| me.get("userPrincipalName").and_then(Value::as_str)).unwrap_or(&id).to_string();
    let name = me.get("displayName").and_then(Value::as_str).unwrap_or(&email).to_string();
    let secret = vault::Secret::new(graph.refresh_token(), &login.cfg, &email, &name, business);
    // Nothing else uses this connection yet, so no renewal can come between reading the token
    // and keeping it; from then on each new one is kept as it comes.
    vault::put(&id, &secret)?;
    *held.lock().unwrap_or_else(|e| e.into_inner()) = Some((id.clone(), secret.clone()));
    if let Some((file, _)) = kept_file(&id, &secret) {
        // A file from an earlier connection of this account was sealed with another key.
        cache::forget_file(&file);
    }
    let account = Arc::new(Account::start(meta_of(&id, &secret), graph, kept_file(&id, &secret))?);
    if let Some(old) = OPEN.lock().unwrap_or_else(|e| e.into_inner()).insert(id, account.clone()) {
        old.forgotten.store(true, Ordering::SeqCst);
    }
    Ok(account)
}

/// An account opened from a refresh token alone: nothing kept, nothing registered (tests with
/// a real account, tools). New refresh tokens go to `rotated`.
pub fn open_with_refresh(cfg: crate::ClientConfig, refresh: &str, rotated: Box<dyn Fn(&str) + Send + Sync>) -> Result<Account> {
    let graph = Graph::new(cfg, Endpoints::default(), refresh.to_string(), rotated);
    let me = graph.get("/me?$select=displayName,mail,userPrincipalName", t("Ler a conta", "Read the account"))?;
    let drive = graph.get("/me/drive?$select=id,driveType", t("Abrir o OneDrive", "Open OneDrive"))?;
    let id = drive.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
    let kind = if drive.get("driveType").and_then(Value::as_str) == Some("personal") { Kind::Personal } else { Kind::Business };
    let email = me.get("mail").and_then(Value::as_str).or_else(|| me.get("userPrincipalName").and_then(Value::as_str)).unwrap_or_default().to_string();
    let name = me.get("displayName").and_then(Value::as_str).unwrap_or_default().to_string();
    Ok(Account::start(Meta { id, email, name, kind }, graph, None)?)
}

/// Writes down the tree of each open account that changed: the app calls it as it closes.
pub fn save_caches() {
    let open: Vec<Arc<Account>> = OPEN.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
    for a in open {
        a.save_cache_if_changed();
    }
}

/// Forgets account `id`: its tokens leave the credential store and its kept tree is deleted.
/// (Microsoft has no sign-out a public client can call for one app's tokens; they expire.)
pub fn disconnect(id: &str) {
    if let Some(a) = OPEN.lock().unwrap_or_else(|e| e.into_inner()).remove(id) {
        a.forgotten.store(true, Ordering::SeqCst);
    }
    if let Some(file) = cache::file_of(id) {
        cache::forget_file(&file);
    }
    vault::forget(id);
}

#[cfg(test)]
impl Account {
    /// Folder `id`'s whole listing from Graph, not kept: what opening it costs without the tree.
    pub(crate) fn list_fresh(&self, id: &str) -> Vec<Item> {
        let url = format!("/drives/{}/items/{}/children?$select={SELECT}&$top={PAGE}", segment(self.drive()), segment(id));
        self.pages(url, "listar").unwrap().iter().map(item_of).collect()
    }
}

/// An account on a stand-in for Graph, for tests.
#[cfg(test)]
pub(crate) fn on(ends: Endpoints, kind: Kind, keep: Option<(PathBuf, cache::Secret)>) -> Res<Account> {
    let cfg = crate::ClientConfig { client_id: "0b1c2d3e-4f50-6172-8394-a5b6c7d8e9f0".into(), tenant: "common".into() };
    let graph = Graph::new(cfg, ends, "r0".into(), Box::new(|_| {}));
    Account::start(Meta { id: crate::fake::DRIVE.into(), email: "ana@exemplo.com".into(), name: "Ana".into(), kind }, graph, keep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_onedrive_refuses() {
        assert!(check_name("relatório final.docx").is_ok());
        assert!(check_name("a:b").is_err() && check_name("x?").is_err() && check_name("fim.").is_err() && check_name(" ").is_err());
    }

    #[test]
    fn addresses_go_by_id_then_by_path() {
        let a = Addr { drive: "b!x".into(), id: "01AB".into(), rest: vec![] };
        assert_eq!(a.url(), "/drives/b!x/items/01AB");
        assert_eq!(a.child("novo #1.txt").url(), "/drives/b!x/items/01AB:/novo%20%231.txt:");
        assert_eq!(Area::of("my FILES"), Some(Area::Files));
        assert_eq!(Area::of("Lixeira"), None);
    }

    #[test]
    fn the_quota_reads_whole() {
        let q = Quota::parse(&json!({"quota":{"total":5368709120u64,"used":1073741824u64,"remaining":4294967296u64,"deleted":1024,"state":"normal"}}));
        assert_eq!((q.total, q.used, q.deleted, q.state.as_str()), (5368709120, 1073741824, 1024, "normal"));
    }
}
