//! The tree beside each pane, as in Air Explorer and Explorer's navigation pane: Área de
//! Trabalho, holding the desktop's folders, the user's folders, the home folder, Este
//! Computador with its disks, Linux with WSL's distributions and Rede with the computers on the
//! network and their shared folders; then each Google Drive account with Meu Drive, its shared
//! drives and Drive's lists; then the simulated drive.
//!
//! A folder's arrow opens it. Its folders are read the first time it opens (from the disk on a
//! thread, or from Drive) and kept, and refreshed whenever a pane lists that folder. Each pane
//! opens its own tree down to the folder it shows and marks it.
//!
//! The tree is one Tab stop and goes by keyboard as a WAI-ARIA tree does: the arrows move a
//! highlighted row (opening and closing nodes sideways), Home and End reach the ends, Enter and
//! Space go where a click goes, and typing a name's first letters jumps to it (see [`key`]).

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use openexplorer_i18n::t;
use rune::{Icon, Key, KeyEvent, Tx};

use crate::model::{DriveKind, Entry, Location, Space, shown};
use crate::network::{self, WSL, is_wsl, server_path, share_path};
use crate::places::Places;
use crate::sysicons::IconKey;
use crate::{Msg, State, background, drive_task, icons};

/// Which node: also what its folders are kept by.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NodeId {
    Desktop,
    Computer,
    /// Rede: the computers on the network (each a `Local` node, `\\servidor`).
    Network,
    /// WSL's distributions, `\\wsl.localhost`.
    Linux,
    Local(PathBuf),
    Account(String),
    /// In an account: `"root"` (Meu Drive), `"shared-drives"`, `"view:…"` for Drive's lists,
    /// else a folder's or a shared drive's id.
    Drive(String, String),
    /// The simulated drive's folders, from its top.
    Mock(Vec<String>),
    /// A server added, by id, and its folders from its root.
    Server(String, Vec<String>),
}

impl NodeId {
    /// Its row's key, unique in a pane's tree.
    pub fn key(&self) -> String {
        match self {
            NodeId::Desktop => "desktop".into(),
            NodeId::Computer => "computer".into(),
            NodeId::Network => "network".into(),
            NodeId::Linux => "linux".into(),
            NodeId::Local(p) => format!("l:{}", p.display()),
            NodeId::Account(e) => format!("a:{e}"),
            NodeId::Drive(e, id) => format!("d:{e}:{id}"),
            NodeId::Mock(segs) => format!("m:{}", segs.join("/")),
            NodeId::Server(id, segs) => format!("s:{id}:{}", segs.join("/")),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Node {
    pub id: NodeId,
    pub label: String,
    /// Windows' icon, when there's one to ask for; else `fallback`.
    pub icon: Option<IconKey>,
    pub fallback: &'static Icon,
    /// Where a click goes; `None` for Este Computador, which only opens and closes.
    pub to: Option<Location>,
    /// Whether it may hold folders: it has an arrow.
    pub opens: bool,
}

/// A node's folders as the tree knows them.
#[derive(Clone)]
pub enum Kids {
    /// Not asked for yet.
    Unknown,
    Loading,
    Ready(Vec<Node>),
    Failed(String),
}

#[derive(Default)]
pub struct Tree {
    /// Folders read so far, of the nodes whose folders aren't known without asking.
    kids: HashMap<NodeId, Kids>,
    /// Each pane's open nodes.
    open: [HashSet<NodeId>; 2],
    /// Bumped on every change to what the trees show, so the view skips re-reading its rows.
    pub version: u64,
    /// Per pane: bring the node of the folder shown into view once it's in the tree.
    follow: [bool; 2],
    /// Rede's computers, by name: those the network search found, those of the network drives,
    /// and those reached by address this session.
    servers: Vec<String>,
    /// Rede was searched, and the search is still going.
    searched: bool,
    searching: bool,
    /// Per pane: the row the keyboard is on, by its line's key.
    cursor: [Option<String>; 2],
    /// Per pane: the letters typed to jump to a row, and when the last one came (app clock).
    typed: [(String, f64); 2],
    /// Per pane: whether the tree has keyboard focus, so its row under the keyboard shows a ring.
    pub focused: [bool; 2],
}

/// A row of a pane's tree.
pub enum Line {
    Heading(&'static str),
    Node {
        /// Unique in the tree: a folder that shows twice (Downloads under Área de Trabalho and
        /// under C: › Users › …) gets its id's key, then that key and `#2`.
        key: String,
        depth: usize,
        node: Box<Node>,
        open: bool,
        /// Opened and holds no folders: no arrow.
        empty: bool,
        /// The node of the folder the pane shows.
        on: bool,
    },
    /// Under an open node whose folders are on their way (`loading`), or failed.
    Note { key: String, depth: usize, text: String, loading: bool },
}

impl Line {
    pub fn key(&self) -> String {
        match self {
            Line::Heading(h) => format!("h:{h}"),
            Line::Node { key, .. } | Line::Note { key, .. } => key.clone(),
        }
    }
}

impl Tree {
    /// The nodes open from the start: Este Computador (once Área de Trabalho is expanded) and
    /// each account. Área de Trabalho itself starts collapsed so each pane expands it
    /// independently on demand.
    pub fn new(accounts: impl IntoIterator<Item = String>) -> Tree {
        let open: HashSet<NodeId> = [NodeId::Computer].into_iter().chain(accounts.into_iter().map(NodeId::Account)).collect();
        Tree { open: [open.clone(), open], ..Tree::default() }
    }

    /// A newly connected account shows open.
    pub fn account_added(&mut self, email: &str) {
        for open in &mut self.open {
            open.insert(NodeId::Account(email.to_string()));
        }
        self.version += 1;
    }

    /// Drops what the trees read and opened of a disconnected account, so connecting it again
    /// reads its folders afresh.
    pub fn forget(&mut self, email: &str) {
        let of = |id: &NodeId| matches!(id, NodeId::Account(e) | NodeId::Drive(e, _) if e == email);
        self.kids.retain(|id, _| !of(id));
        for open in &mut self.open {
            open.retain(|id| !of(id));
        }
        self.version += 1;
    }

    /// Drops what the trees read and opened of a server changed or removed.
    pub fn server_changed(&mut self, id: &str) {
        let of = |n: &NodeId| matches!(n, NodeId::Server(s, _) if s == id);
        self.kids.retain(|n, _| !of(n));
        for open in &mut self.open {
            open.retain(|n| !of(n));
        }
        self.version += 1;
    }

    /// A newly connected MEGA account shows open, with its three areas.
    pub fn server_opened(&mut self, id: &str) {
        for open in &mut self.open {
            open.insert(NodeId::Server(id.to_string(), Vec::new()));
        }
        self.version += 1;
    }

    /// Adds a computer to Rede, unless it's there by another spelling.
    fn saw_server(&mut self, server: &str) {
        if !self.servers.iter().any(|s| s.eq_ignore_ascii_case(server)) {
            self.servers.push(server.to_string());
            self.version += 1;
        }
    }

    /// A computer's name as Rede spells it ("NAS" for a typed `\\nas`), so they're one node.
    fn spelled(&self, server: &str) -> String {
        self.servers.iter().find(|s| s.eq_ignore_ascii_case(server)).cloned().unwrap_or_else(|| server.to_string())
    }
}

// ---------------------------------------------------------------------------------------------
// Nodes

fn desktop_node(places: &Places) -> Option<Node> {
    let (path, name) = places.desktop.clone()?;
    Some(Node { id: NodeId::Desktop, label: name, icon: Some(IconKey::Path(path.clone())), fallback: &icons::DESKTOP, to: Some(Location::Local(path)), opens: true })
}

fn computer_node() -> Node {
    Node { id: NodeId::Computer, label: t("Este Computador", "This PC").into(), icon: Some(IconKey::Computer), fallback: &icons::COMPUTER, to: None, opens: true }
}

fn network_node() -> Node {
    Node { id: NodeId::Network, label: t("Rede", "Network").into(), icon: None, fallback: &icons::NETWORK, to: None, opens: true }
}

fn linux_node() -> Node {
    Node { id: NodeId::Linux, label: "Linux".into(), icon: None, fallback: &icons::LINUX, to: Some(Location::Local(server_path(WSL))), opens: true }
}

/// A computer on the network, `\\servidor`, which holds its shared folders. Windows' icon for
/// it would ask the computer, so it's the app's own.
fn server_node(server: &str) -> Node {
    let path = server_path(server);
    Node { id: NodeId::Local(path.clone()), label: server.into(), icon: None, fallback: &icons::COMPUTER, to: Some(Location::Local(path)), opens: true }
}

/// A folder on the computer, a shared folder or a distribution: one of the user's folders by
/// its Windows name and icon, any other by its own name.
fn local_node(places: &Places, path: PathBuf) -> Node {
    let (label, icon, fallback): (String, Option<IconKey>, &'static Icon) = match places.name_of(&path) {
        Some(name) => (name.to_string(), Some(IconKey::Path(path.clone())), &icons::FOLDER),
        None => {
            let label = network::name(&path).unwrap_or_else(|| path.display().to_string());
            match network::unc(&path) {
                Some((server, _)) if is_wsl(&server) && path.parent().is_none() => (label, None, &icons::LINUX),
                _ => (label, Some(IconKey::Folder), &icons::FOLDER),
            }
        }
    };
    Node { id: NodeId::Local(path.clone()), label, icon, fallback, to: Some(Location::Local(path)), opens: true }
}

/// A disk, named as Explorer names it ("Disco Local (C:)").
fn disk_node(path: &Path, kind: DriveKind) -> Node {
    let shown = path.display().to_string();
    // "C:" for C:\; "/" stays itself.
    let letter = match shown.trim_end_matches(['\\', '/']) {
        "" => shown.as_str(),
        l => l,
    };
    let label = kind.label(letter);
    Node { id: NodeId::Local(path.to_path_buf()), label, icon: Some(IconKey::Path(path.to_path_buf())), fallback: &icons::DISK, to: Some(Location::Local(path.to_path_buf())), opens: true }
}

fn account_node(email: &str) -> Node {
    Node { id: NodeId::Account(email.into()), label: email.into(), icon: None, fallback: &icons::PROVIDER_GDRIVE, to: Some(Location::drive(email, Space::MyDrive)), opens: true }
}

/// One of an account's spaces: Meu Drive and Drives compartilhados open; Drive's lists don't.
fn space_node(email: &str, space: Space) -> Node {
    let (id, fallback, icon, opens): (&str, &'static Icon, Option<IconKey>, bool) = match space {
        Space::MyDrive => ("root", &icons::FOLDER, Some(IconKey::Folder), true),
        Space::SharedDrives => ("shared-drives", &icons::SHARED, None, true),
        Space::SharedWithMe => ("view:shared-with-me", &icons::SHARED_WITH_ME, None, false),
        Space::Starred => ("view:starred", &icons::STAR, None, false),
        Space::Recent => ("view:recent", &icons::RECENT, None, false),
        Space::Trash => ("view:trash", &icons::TRASH, None, false),
        Space::Shared { .. } | Space::Search(_) => unreachable!("not one of an account's spaces"),
    };
    Node { id: NodeId::Drive(email.into(), id.into()), label: space.label().into(), icon, fallback, to: Some(Location::drive(email, space)), opens }
}

fn shared_drive_node(email: &str, id: String, name: String) -> Node {
    let label = shown(&name);
    Node { id: NodeId::Drive(email.into(), id.clone()), label, icon: None, fallback: &icons::SHARED, to: Some(Location::drive(email, Space::Shared { id, name })), opens: true }
}

/// The last folder of a Drive location (`path` not empty).
fn drive_folder_node(email: &str, space: Space, path: Vec<(String, String)>) -> Node {
    let (id, name) = path.last().cloned().unwrap_or_default();
    let to = Location::Drive { email: email.into(), space, path };
    Node { id: NodeId::Drive(email.into(), id), label: shown(&name), icon: Some(IconKey::Folder), fallback: &icons::FOLDER, to: Some(to), opens: true }
}

fn mock_node(segs: Vec<String>) -> Node {
    let (label, fallback): (String, &'static Icon) = match segs.last() {
        Some(name) => (name.clone(), &icons::FOLDER),
        None => (t("Drive simulado", "Simulated drive").into(), &icons::LAB),
    };
    let icon = (!segs.is_empty()).then_some(IconKey::Folder);
    Node { id: NodeId::Mock(segs.clone()), label, icon, fallback, to: Some(Location::Mock(segs)), opens: true }
}

/// A server's folder at `path` under its root; the server itself with an empty `path`.
fn remote_node(server: &openexplorer_remote::Server, path: Vec<String>) -> Node {
    let (label, icon, fallback): (String, Option<IconKey>, &'static Icon) = match path.last() {
        // A MEGA account's Rubbish Bin and Incoming shares, with icons of their own.
        Some(name) if server.is_mega() && path.len() == 1 && crate::mega::area_of(&path) == Some(openexplorer_mega::Area::Rubbish) => (name.clone(), None, &icons::TRASH),
        Some(name) if server.is_mega() && path.len() == 1 && crate::mega::area_of(&path) == Some(openexplorer_mega::Area::Incoming) => (name.clone(), None, &icons::SHARED_WITH_ME),
        Some(name) if crate::onedrive::bin_node(server, &path) => (name.clone(), None, &icons::TRASH),
        Some(name) => (name.clone(), Some(IconKey::Folder), &icons::FOLDER),
        None => (server.name.clone(), None, crate::servers::Provider::of_server(server).icon()),
    };
    let to = crate::servers::folder(server, path.clone());
    // A OneDrive Lixeira's folders don't open.
    let opens = !crate::onedrive::bin_node(server, &path);
    Node { id: NodeId::Server(server.id.clone(), path), label, icon, fallback, to: Some(to), opens }
}

fn sorted(mut nodes: Vec<Node>) -> Vec<Node> {
    nodes.sort_by(|a, b| rune_kit::natural_cmp(&a.label.to_lowercase(), &b.label.to_lowercase()));
    nodes
}

// ---------------------------------------------------------------------------------------------
// Folders of a node

/// A node's folders: worked out at once where nothing is read (Este Computador, an account,
/// the simulated drive), else as kept (borrowed, not copied, as the tree redraws often).
fn kids_of<'a>(s: &'a State, node: &Node) -> Cow<'a, Kids> {
    Cow::Owned(match &node.id {
        NodeId::Computer => Kids::Ready(s.local_drives.iter().map(|(d, kind)| disk_node(d, *kind)).collect()),
        NodeId::Linux => Kids::Ready(s.places.wsl.iter().map(|d| local_node(&s.places, share_path(WSL, d))).collect()),
        NodeId::Network if !s.tree.searched => Kids::Unknown,
        NodeId::Network => Kids::Ready(sorted(s.tree.servers.iter().map(|sv| server_node(sv)).collect())),
        NodeId::Account(email) => Kids::Ready([Space::MyDrive, Space::SharedDrives, Space::SharedWithMe, Space::Starred, Space::Recent, Space::Trash].into_iter().map(|sp| space_node(email, sp)).collect()),
        NodeId::Mock(segs) => Kids::Ready(sorted(s.mock.list(segs).into_iter().filter(|e| e.is_dir).map(|e| mock_node(segs.iter().cloned().chain([e.name]).collect())).collect())),
        id => return s.tree.kids.get(id).map_or(Cow::Owned(Kids::Unknown), Cow::Borrowed),
    })
}

/// Asks for `node`'s folders if they aren't known yet: the disk's on a thread, Drive's through
/// the account.
fn load(s: &mut State, node: &Node) {
    if !matches!(*kids_of(s, node), Kids::Unknown) {
        return;
    }
    let id = node.id.clone();
    // Folders read from disk or a server follow Exibir arquivos ocultos: an answer read with the
    // setting since changed is dropped (see `arrived`).
    let hidden = matches!(node.id, NodeId::Desktop | NodeId::Local(_) | NodeId::Server(..)).then_some(s.settings.show_hidden);
    let done = {
        let id = id.clone();
        move |result| Msg::TreeKids { id, hidden, result }
    };
    match (&node.id, node.to.clone()) {
        (NodeId::Network, _) => return search_network(s),
        (NodeId::Desktop, _) => {
            let (places, show_hidden) = (s.places.clone(), s.settings.show_hidden);
            background(s, move |emit| emit(done(crate::catch_panic(t("ler a área de trabalho", "read the Desktop"), || Ok(desktop_kids(&places, show_hidden))))));
        }
        (NodeId::Local(path), _) => {
            let (places, path, show_hidden) = (s.places.clone(), path.clone(), s.settings.show_hidden);
            background(s, move |emit| emit(done(crate::catch_panic(t("ler as pastas", "read the folders"), || local_kids(&places, &path, show_hidden)))));
        }
        (NodeId::Drive(email, _), Some(Location::Drive { space, path, .. })) => {
            let (email2, space2) = (email.clone(), space.clone());
            let work = move |d: &openexplorer_gdrive::Drive| -> Result<Vec<Node>, String> {
                if space2 == Space::SharedDrives {
                    return Ok(sorted(d.shared_drives()?.into_iter().map(|sd| shared_drive_node(&email2, sd.id, sd.name)).collect()));
                }
                let (parent, shared) = match (&space2, path.last()) {
                    (Space::Shared { id, .. }, last) => (last.map_or(id.clone(), |(f, _)| f.clone()), Some(id.clone())),
                    (_, Some((f, _))) => (f.clone(), None),
                    (_, None) => ("root".to_string(), None),
                };
                let folders = d.folders(&parent, shared.as_deref())?;
                Ok(sorted(folders.into_iter().map(|f| drive_folder_node(&email2, space2.clone(), extend(&path, &f.target.unwrap_or(f.id), &f.name))).collect()))
            };
            drive_task(s, &email.clone(), t("ler as pastas do Drive", "read the Drive folders"), work, done);
        }
        (NodeId::Server(_, path), Some(Location::Server { server, .. })) => {
            let (pool, server, path, show_hidden) = (s.servers.pool.clone(), server.clone(), path.clone(), s.settings.show_hidden);
            let nodes = |server: &openexplorer_remote::Server, path: &[String], names: Vec<String>| sorted(names.into_iter().map(|n| remote_node(server, extend_segs(path, &n))).collect());
            // A cloud account's kept listing shows at once; the folder is listed behind it, and
            // replaces it only when that works.
            let kept = crate::accounts::kept_folders(&server, &path, show_hidden);
            let shown = kept.is_some();
            if let Some(names) = kept {
                s.tree.kids.insert(id.clone(), Kids::Ready(nodes(&server, &path, names)));
                s.tree.version += 1;
            }
            background(s, move |emit| {
                let result = crate::catch_panic(t("ler as pastas do servidor", "read the server's folders"), || crate::servers::folders(&pool, &server, &path, show_hidden));
                if shown && result.is_err() {
                    return;
                }
                emit(done(result.map(|names| nodes(&server, &path, names))));
            });
            if shown {
                return;
            }
        }
        _ => return,
    }
    s.tree.kids.insert(id, Kids::Loading);
    s.tree.version += 1;
}

/// Looks for Rede's computers: the network drives' at once, then those the network answers
/// for, which takes up to half a minute. Those found join the ones known; none leave.
fn search_network(s: &mut State) {
    if s.tree.searching {
        return;
    }
    s.tree.searched = true;
    s.tree.searching = true;
    s.tree.version += 1;
    let drives = s.local_drives.clone();
    background(s, move |emit| {
        emit(Msg::Servers { found: network::mapped_servers(&drives), done: false });
        let found = crate::catch_panic(t("procurar computadores na rede", "look for computers on the network"), || Ok(network::computers())).unwrap_or_default();
        emit(Msg::Servers { found, done: true });
    });
}

/// Computers the network search found.
pub fn servers_found(s: &mut State, found: Vec<String>, done: bool) {
    for server in found {
        s.tree.saw_server(&server);
    }
    if done {
        s.tree.searching = false;
    }
    s.tree.version += 1;
}

fn extend_segs(path: &[String], name: &str) -> Vec<String> {
    path.iter().cloned().chain([name.to_string()]).collect()
}

fn extend(path: &[(String, String)], id: &str, name: &str) -> Vec<(String, String)> {
    path.iter().cloned().chain([(id.to_string(), name.to_string())]).collect()
}

/// Área de Trabalho's folders: the desktop's own (the user's and the shared one), the user's
/// folders, the home folder and Este Computador, by name.
fn desktop_kids(places: &Places, show_hidden: bool) -> Vec<Node> {
    let mut seen = HashSet::new();
    let mut nodes = Vec::new();
    let on_desktop = places.desktop.iter().map(|(p, _)| p.clone()).chain(places.desktop_also.iter().cloned());
    for dir in on_desktop {
        for path in subfolders(&dir, show_hidden).unwrap_or_default() {
            if seen.insert(path.clone()) {
                nodes.push(local_node(places, path));
            }
        }
    }
    for (path, _) in &places.user {
        if seen.insert(path.clone()) {
            nodes.push(local_node(places, path.clone()));
        }
    }
    nodes.extend(beside_computer(places));
    sorted(nodes)
}

/// Este Computador, Rede and Linux (where WSL has distributions).
fn beside_computer(places: &Places) -> Vec<Node> {
    [computer_node(), network_node()].into_iter().chain((!places.wsl.is_empty()).then(linux_node)).collect()
}

fn local_kids(places: &Places, dir: &Path, show_hidden: bool) -> Result<Vec<Node>, String> {
    let folders = subfolders(dir, show_hidden).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(sorted(folders.into_iter().map(|p| local_node(places, p)).collect()))
}

/// The folders in `dir` (links to folders included). The listing already says which entries
/// are folders; only a link is looked up, so a folder of thousands of files costs no more.
fn folders_in(dir: &Path, show_hidden: bool) -> std::io::Result<Vec<PathBuf>> {
    let folder = |e: &std::fs::DirEntry| {
        if !e.file_type().is_ok_and(|t| t.is_dir() || (t.is_symlink() && e.path().is_dir())) {
            return false;
        }
        if show_hidden {
            return true;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        !crate::model::is_hidden(&name, e.metadata().ok().as_ref())
    };
    Ok(std::fs::read_dir(dir)?.filter_map(Result::ok).filter(folder).map(|e| e.path()).collect())
}

/// The folders in `dir`; in a computer on the network, its shared folders.
fn subfolders(dir: &Path, show_hidden: bool) -> std::io::Result<Vec<PathBuf>> {
    if let Some((server, share)) = network::unc(dir)
        && share.is_empty()
    {
        return Ok(network::shares(&server)?.into_iter().map(|name| share_path(&server, &name)).collect());
    }
    folders_in(dir, show_hidden)
}

/// Takes in what changed in `email`'s Drive (see `watch`): the folder lists a changed folder
/// now sits in or sat in, and the list of shared drives when one changed, are dropped, and
/// read again where a tree has them open. `root` is Meu Drive's real id, which changes use.
pub fn drive_changed(s: &mut State, email: &str, changes: &[openexplorer_gdrive::Change], root: Option<&str>) {
    let mut stale: HashSet<String> = HashSet::new();
    // Which of the account's lists hold each folder, looked up once rather than per change.
    let mut holders: HashMap<&str, Vec<&str>> = HashMap::new();
    for (id, kids) in &s.tree.kids {
        if let (NodeId::Drive(e, folder), Kids::Ready(nodes)) = (id, kids)
            && e == email
        {
            for n in nodes {
                if let NodeId::Drive(_, k) = &n.id {
                    holders.entry(k.as_str()).or_default().push(folder.as_str());
                }
            }
        }
    }
    for c in changes {
        if c.drive {
            stale.insert("shared-drives".into());
            continue;
        }
        // The tree lists folders only; something gone for good may have been one.
        if c.file.as_ref().is_some_and(|f| !f.is_dir) {
            continue;
        }
        if !c.trashed {
            stale.extend(c.parents.iter().map(|p| if Some(p.as_str()) == root { "root".to_string() } else { p.clone() }));
        }
        stale.extend(holders.get(c.file_id.as_str()).into_iter().flatten().map(|f| f.to_string()));
    }
    let stale: Vec<NodeId> = stale.into_iter().map(|f| NodeId::Drive(email.to_string(), f)).filter(|id| s.tree.kids.contains_key(id)).collect();
    if stale.is_empty() {
        return;
    }
    // The nodes to read again, found before their lists are dropped.
    let opened: HashSet<&NodeId> = s.tree.open[0].iter().chain(s.tree.open[1].iter()).collect();
    let again: Vec<Node> = stale
        .iter()
        .filter(|id| opened.contains(id))
        .filter_map(|id| match id {
            NodeId::Drive(_, f) if f == "root" => Some(space_node(email, Space::MyDrive)),
            NodeId::Drive(_, f) if f == "shared-drives" => Some(space_node(email, Space::SharedDrives)),
            _ => s.tree.kids.values().find_map(|k| match k {
                Kids::Ready(nodes) => nodes.iter().find(|n| n.id == *id).cloned(),
                _ => None,
            }),
        })
        .collect();
    for id in &stale {
        s.tree.kids.remove(id);
    }
    s.tree.version += 1;
    for node in again {
        load(s, &node);
    }
}

/// Takes in what changed in a MEGA account (see `mega::tick`): the folders of server `id` that
/// `stale` says may differ are dropped, and read again where a tree has them open.
pub fn server_folders_changed(s: &mut State, id: &str, stale: &dyn Fn(&[String]) -> bool) {
    let gone: Vec<Vec<String>> = s.tree.kids.keys().filter_map(|n| match n {
        NodeId::Server(sid, segs) if sid == id && stale(segs) => Some(segs.clone()),
        _ => None,
    }).collect();
    if gone.is_empty() {
        return;
    }
    let opened: HashSet<NodeId> = s.tree.open[0].iter().chain(s.tree.open[1].iter()).cloned().collect();
    for segs in &gone {
        s.tree.kids.remove(&NodeId::Server(id.to_string(), segs.clone()));
    }
    s.tree.version += 1;
    let Some(server) = s.servers.list.iter().find(|sv| sv.id == id).cloned() else { return };
    for segs in gone {
        if opened.contains(&NodeId::Server(id.to_string(), segs.clone())) {
            load(s, &remote_node(&server, segs));
        }
    }
}

/// Re-reads open local and server nodes in the tree after `show_hidden` changes.
pub fn refresh_hidden(s: &mut State) {
    s.tree.kids.retain(|id, _| !matches!(id, NodeId::Desktop | NodeId::Local(_) | NodeId::Server(..)));
    s.tree.version += 1;
    let opened: HashSet<NodeId> = s.tree.open[0].iter().chain(s.tree.open[1].iter()).cloned().collect();
    if opened.contains(&NodeId::Desktop)
        && let Some(d) = desktop_node(&s.places)
    {
        load(s, &d);
    }
    for id in opened {
        match id {
            NodeId::Local(path) => {
                let n = local_node(&s.places, path);
                load(s, &n);
            }
            NodeId::Server(sv_id, segs) => {
                if let Some(sv) = s.servers.list.iter().find(|sv| sv.id == sv_id).cloned() {
                    load(s, &remote_node(&sv, segs));
                }
            }
            _ => {}
        }
    }
}

/// `id`'s folders arrived, read with Exibir arquivos ocultos as `hidden` says (`None` where it
/// doesn't matter). Read before the setting changed, they're dropped: `refresh_hidden` asked for
/// them again.
pub fn arrived(s: &mut State, id: NodeId, hidden: Option<bool>, result: Result<Vec<Node>, String>) {
    if hidden.is_some_and(|h| h != s.settings.show_hidden) {
        return;
    }
    s.tree.kids.insert(id, result.map_or_else(Kids::Failed, Kids::Ready));
    s.tree.version += 1;
}

/// What pane `p`'s complete listing says of its folder's folders: the tree, if it had read
/// them, takes them in, so a folder made, renamed or deleted there shows.
pub fn learn(s: &mut State, p: usize) {
    let pane = &s.panes[p];
    if pane.loading || pane.error.is_some() {
        return;
    }
    let Some(node) = chain(s, &pane.loc).pop() else { return };
    // The simulated drive's folders are read as the tree shows; a renamed one needs new keys.
    if matches!(node.id, NodeId::Mock(_)) {
        s.tree.version += 1;
        return;
    }
    // Linux's distributions, as a listing of `\\wsl.localhost` has them now: one installed or
    // removed since the app started.
    if node.id == NodeId::Linux {
        let mut names: Vec<String> = pane.entries.iter().filter(|e| e.is_dir).map(|e| e.name.clone()).collect();
        names.sort_by_key(|n| n.to_lowercase());
        if names != s.places.wsl {
            s.places.wsl = names;
            s.tree.version += 1;
        }
        return;
    }
    if !matches!(s.tree.kids.get(&node.id), Some(Kids::Ready(_) | Kids::Failed(_))) {
        return;
    }
    let dirs = pane.entries.iter().filter(|e| e.is_dir);
    let kids: Vec<Node> = match (&node.id, &pane.loc) {
        // Under the node's path, which may spell a computer as Rede does rather than as typed.
        (NodeId::Local(at), Location::Local(_)) => dirs.map(|e| local_node(&s.places, at.join(&e.name))).collect(),
        (NodeId::Drive(email, _), Location::Drive { space: Space::SharedDrives, .. }) => dirs.map(|e| shared_drive_node(email, e.id.clone(), e.name.clone())).collect(),
        (NodeId::Drive(email, _), Location::Drive { space, path, .. }) => dirs.map(|e| drive_folder_node(email, space.clone(), extend(path, opened(e), &e.name))).collect(),
        (NodeId::Server(..), Location::Server { server, path, find: None }) => dirs.map(|e| remote_node(server, extend_segs(path, &e.name))).collect(),
        _ => return,
    };
    s.tree.kids.insert(node.id, Kids::Ready(sorted(kids)));
    s.tree.version += 1;
}

/// The folder a Drive entry opens: a shortcut's target, else itself.
fn opened(e: &Entry) -> &str {
    e.target.as_deref().unwrap_or(&e.id)
}

// ---------------------------------------------------------------------------------------------
// Opening, following

/// The arrow of `node`: opens it (reading its folders the first time) or closes it.
pub fn toggle(s: &mut State, p: usize, node: &Node) {
    if !s.tree.open[p].remove(&node.id) {
        s.tree.open[p].insert(node.id.clone());
        // Rede looks again each time it opens: a computer may have come on since.
        if node.id == NodeId::Network {
            search_network(s);
        } else {
            load(s, node);
        }
    }
    s.tree.version += 1;
}

/// The nodes from the tree's top down to the one for `loc` (which may not be read yet). On the
/// computer the shortest way wins: Downloads under Área de Trabalho rather than under
/// Este Computador › C: › Users › ….
fn chain(s: &State, loc: &Location) -> Vec<Node> {
    match loc {
        Location::Local(path) => local_chain(s, path),
        Location::Found(f) => f.within.as_ref().map(|w| local_chain(s, w)).unwrap_or_default(),
        Location::Mock(segs) => (0..=segs.len()).map(|i| mock_node(segs[..i].to_vec())).collect(),
        Location::Server { server, path, .. } => (0..=path.len()).map(|i| remote_node(server, path[..i].to_vec())).collect(),
        Location::Drive { email, space, path } => {
            let mut out = vec![account_node(email)];
            match space {
                Space::Search(_) => return out,
                Space::Shared { id, name } => out.extend([space_node(email, Space::SharedDrives), shared_drive_node(email, id.clone(), name.clone())]),
                Space::MyDrive | Space::SharedDrives => out.push(space_node(email, space.clone())),
                // Drive's lists don't open: the list stands for what's opened from it.
                _ => {
                    out.push(space_node(email, space.clone()));
                    return out;
                }
            }
            out.extend((1..=path.len()).map(|i| drive_folder_node(email, space.clone(), path[..i].to_vec())));
            out
        }
    }
}

fn local_chain(s: &State, path: &Path) -> Vec<Node> {
    let places = &s.places;
    let desktop = desktop_node(places);
    // Where each top folder hangs from: the desktop's (itself), the user's folders and home
    // (under Área de Trabalho), the disks (under Este Computador).
    let mut anchors: Vec<(PathBuf, Vec<Node>)> = Vec::new();
    if let Some(d) = &desktop
        && let Some((dir, _)) = &places.desktop
    {
        anchors.push((dir.clone(), vec![d.clone()]));
        for also in &places.desktop_also {
            anchors.push((also.clone(), vec![d.clone()]));
        }
    }
    let top: Vec<Node> = desktop.iter().cloned().collect();
    // On the network: Rede › the computer (or Linux, for WSL's) › the shared folder › ….
    if let Some((server, share)) = network::unc(path) {
        let mut out = top;
        let spelled = if is_wsl(&server) {
            out.push(linux_node());
            WSL.to_string()
        } else {
            let spelled = s.tree.spelled(&server);
            out.extend([network_node(), server_node(&spelled)]);
            spelled
        };
        if share.is_empty() {
            return out;
        }
        let mut at = share_path(&spelled, &share);
        out.push(local_node(places, at.clone()));
        for c in path.strip_prefix(share_path(&server, &share)).map(|r| r.components().collect::<Vec<_>>()).unwrap_or_default() {
            at.push(c);
            out.push(local_node(places, at.clone()));
        }
        return out;
    }
    // Without a desktop the tree starts at Este Computador: the user's folders are on the disks.
    if let Some(d) = &desktop {
        for (dir, _) in &places.user {
            anchors.push((dir.clone(), vec![d.clone(), local_node(places, dir.clone())]));
        }
    }
    for (d, kind) in &s.local_drives {
        let disk = disk_node(d, *kind);
        anchors.push((d.clone(), top.iter().cloned().chain([computer_node(), disk]).collect()));
    }
    // The longest anchor holding `path`, then a node for each folder below it.
    let Some((base, mut out)) = anchors.into_iter().filter(|(a, _)| path.starts_with(a)).max_by_key(|(a, _)| a.components().count()) else { return Vec::new() };
    let mut at = base.clone();
    for c in path.strip_prefix(&base).map(|r| r.components().collect::<Vec<_>>()).unwrap_or_default() {
        at.push(c);
        out.push(local_node(places, at.clone()));
    }
    out
}

/// Opens pane `p`'s tree down to the folder it now shows (reading what's needed on the way)
/// and has the next frames bring that node into view.
pub fn follow(s: &mut State, p: usize) {
    // A computer reached by address shows in Rede, where the network search may not find it.
    if let Location::Local(dir) = &s.panes[p].loc
        && let Some((server, _)) = network::unc(dir)
        && !is_wsl(&server)
    {
        s.tree.saw_server(&server);
    }
    let chain = chain(s, &s.panes[p].loc.clone());
    for node in chain.iter().take(chain.len().saturating_sub(1)) {
        if node.opens {
            s.tree.open[p].insert(node.id.clone());
            load(s, node);
        }
    }
    s.tree.follow[p] = true;
    s.tree.version += 1;
}

/// The rows of pane `p`'s tree, as they show: each open node followed by its folders.
pub fn lines(s: &State, p: usize) -> Vec<Line> {
    let current = chain(s, &s.panes[p].loc).pop().map(|n| n.id);
    let mut w = Walk { s, p, current: current.as_ref(), seen: HashMap::new(), out: Vec::new() };
    match desktop_node(&s.places) {
        Some(desktop) => w.node(desktop, 0),
        None => {
            for node in beside_computer(&s.places) {
                w.node(node, 0);
            }
        }
    }
    if !s.accounts.is_empty() || !s.servers.list.is_empty() {
        w.out.push(Line::Heading(t("Contas", "Accounts")));
    }
    for a in &s.accounts {
        w.node(account_node(&a.email), 0);
    }
    for server in &s.servers.list {
        w.node(remote_node(server, Vec::new()), 0);
    }
    w.out.push(Line::Heading(t("Testes", "Tests")));
    w.node(mock_node(Vec::new()), 0);
    w.out
}

/// Building a tree's rows, each open node followed by its folders.
struct Walk<'a> {
    s: &'a State,
    p: usize,
    current: Option<&'a NodeId>,
    /// How many times each node's key was given, to tell apart a folder that shows twice.
    seen: HashMap<String, usize>,
    out: Vec<Line>,
}

impl Walk<'_> {
    fn node(&mut self, node: Node, depth: usize) {
        let (s, p) = (self.s, self.p);
        let open = node.opens && s.tree.open[p].contains(&node.id);
        let kids = if open { kids_of(s, &node) } else { Cow::Owned(Kids::Unknown) };
        // Rede keeps its arrow: closing and opening it looks again.
        let network = node.id == NodeId::Network;
        let empty = matches!(&*kids, Kids::Ready(k) if k.is_empty()) && !network;
        let on = self.current == Some(&node.id);
        let id = node.id.key();
        let times = self.seen.entry(id.clone()).or_default();
        *times += 1;
        let key = if *times == 1 { id } else { format!("{id}#{times}") };
        let note = |text: String, loading| Line::Note { key: format!("note:{key}"), depth: depth + 1, text, loading };
        let after = match &*kids {
            Kids::Loading => Some(note(t("Carregando…", "Loading…").into(), true)),
            Kids::Failed(e) => Some(note(e.clone(), false)),
            // Not `loading`: following a computer reached by address needn't wait on the search.
            Kids::Ready(_) if network && s.tree.searching => Some(note(t("Procurando computadores…", "Looking for computers…").into(), false)),
            Kids::Ready(k) if network && k.is_empty() => Some(note(t(r"Nenhum computador encontrado. Digite \\nome na barra de endereço.", r"No computers found. Type \\name in the address bar.").into(), false)),
            Kids::Ready(_) | Kids::Unknown => None,
        };
        self.out.push(Line::Node { key, depth, node: Box::new(node), open, empty, on });
        if let Kids::Ready(kids) = &*kids {
            for k in kids {
                self.node(k.clone(), depth + 1);
            }
        }
        self.out.extend(after);
    }
}

/// Whether pane `p`'s tree still has to bring the folder shown into view.
pub fn following(s: &State, p: usize) -> bool {
    s.tree.follow[p]
}

/// While following, the row to bring into view once it shows: its key. Following ends there,
/// or once nothing on the way is still being read.
pub fn to_reveal(s: &mut State, p: usize, lines: &[Line]) -> Option<String> {
    if !s.tree.follow[p] {
        return None;
    }
    if let Some(on) = lines.iter().find_map(|l| matches!(l, Line::Node { on: true, .. }).then(|| l.key())) {
        s.tree.follow[p] = false;
        // The keyboard picks up from the folder shown, as Explorer's tree does.
        set_cursor(s, p, on.clone());
        return Some(on);
    }
    if !lines.iter().any(|l| matches!(l, Line::Note { loading: true, .. })) {
        s.tree.follow[p] = false;
    }
    None
}

/// The icons the tree asks Windows for besides folders': the desktop, the user's folders and
/// the disks.
pub fn fixed_icons(places: &Places, drives: &[(PathBuf, DriveKind)]) -> Vec<IconKey> {
    places.desktop.iter().map(|(p, _)| p.clone()).chain(places.user.iter().map(|(p, _)| p.clone())).chain(drives.iter().map(|(d, _)| d.clone())).map(IconKey::Path).collect()
}

// ---------------------------------------------------------------------------------------------
// Keyboard

/// How long typing may pause before the next letter starts a new name, in seconds: the file
/// list's own pause, so a name is typed at one speed everywhere.
const TYPE_PAUSE: f64 = crate::keyboard::PAUSE;

/// What a key does in a pane's tree.
#[derive(Clone, Debug, PartialEq)]
pub enum Move {
    Up,
    Down,
    PageUp,
    PageDown,
    First,
    Last,
    /// →: opens a closed node, or goes to the first folder of an open one.
    Open,
    /// ←: closes an open node, or goes to the node holding this one.
    Close,
    /// Enter or Space: where a click on the row goes.
    Go,
    /// Letters typed, to jump to a row whose name starts with them.
    Type(String),
}

/// The row the keyboard is on in pane `p`'s tree, by its key.
pub fn cursor(s: &State, p: usize) -> Option<&str> {
    s.tree.cursor[p].as_deref()
}

/// Puts the keyboard on a row (one clicked).
pub fn set_cursor(s: &mut State, p: usize, key: String) {
    if s.tree.cursor[p].as_ref() != Some(&key) {
        s.tree.cursor[p] = Some(key);
        s.tree.version += 1;
    }
}

/// Whether letters typed in pane `p`'s tree are still making up a name at `now`: Space then
/// types a space rather than going to the row.
pub fn typing(s: &State, p: usize, now: f64) -> bool {
    let (typed, at) = &s.tree.typed[p];
    !typed.is_empty() && now - at <= TYPE_PAUSE
}

/// The keyboard came to or left pane `p`'s tree. Coming in, it lands on the folder shown (or the
/// first row) when it isn't on a row that's still there.
pub fn focus(s: &mut State, p: usize, on: bool) {
    if s.tree.focused[p] == on {
        return;
    }
    s.tree.focused[p] = on;
    if on {
        let lines = lines(s, p);
        if let Some((i, false)) = resolve(&lines, s.tree.cursor[p].as_deref()) {
            s.tree.cursor[p] = Some(lines[i].key());
        }
    }
    s.tree.version += 1;
}

/// The node rows: `(index in lines, depth)`.
fn nodes(lines: &[Line]) -> impl Iterator<Item = (usize, usize)> + '_ {
    lines.iter().enumerate().filter_map(|(i, l)| match l {
        Line::Node { depth, .. } => Some((i, *depth)),
        _ => None,
    })
}

/// The row `cursor` names, and whether it's still there; one gone (its node closed, its folder
/// deleted) falls back to the folder shown, else the first row.
fn resolve(lines: &[Line], cursor: Option<&str>) -> Option<(usize, bool)> {
    if let Some(c) = cursor
        && let Some((i, _)) = nodes(lines).find(|&(i, _)| lines[i].key() == c)
    {
        return Some((i, true));
    }
    let on = lines.iter().position(|l| matches!(l, Line::Node { on: true, .. }));
    on.or_else(|| nodes(lines).next().map(|(i, _)| i)).map(|i| (i, false))
}

/// Does `m` in pane `p`'s tree at `now` (app clock, seconds): moves the keyboard's row, opens or
/// closes a node, or goes to a node's place. The key of the keyboard's row, to bring into view.
pub fn press(s: &mut State, p: usize, m: Move, now: f64) -> Option<String> {
    // A page is the rows the tree shows, as in the file list.
    let page = crate::keyboard::page((s.edges[p].tree.height() / crate::ROW_H).floor() as usize) as isize;
    let all = lines(s, p);
    let (at, kept) = resolve(&all, s.tree.cursor[p].as_deref())?;
    if !matches!(m, Move::Type(_)) {
        s.tree.typed[p].0.clear();
    }
    let rows: Vec<(usize, usize)> = nodes(&all).collect();
    let here = rows.iter().position(|(i, _)| *i == at).unwrap_or(0);
    let step = |d: isize| rows[(here as isize + d).clamp(0, rows.len() as isize - 1) as usize].0;
    let Line::Node { node, depth, open, empty, .. } = &all[at] else { return None };
    let to = match m {
        // The first arrow after the keyboard's row went away lands where `resolve` put it.
        Move::Up | Move::Down if !kept => at,
        Move::Up => step(-1),
        Move::Down => step(1),
        Move::PageUp => step(-page),
        Move::PageDown => step(page),
        Move::First => rows[0].0,
        Move::Last => rows[rows.len() - 1].0,
        Move::Open if node.opens && !*empty && !*open => {
            let node = (**node).clone();
            toggle(s, p, &node);
            at
        }
        // Its first folder, once they're read: until then (a note shows), it stays.
        Move::Open => match all.get(at + 1) {
            Some(Line::Node { depth: d, .. }) if *open && *d == depth + 1 => at + 1,
            _ => at,
        },
        Move::Close if *open => {
            let node = (**node).clone();
            toggle(s, p, &node);
            at
        }
        Move::Close => rows[..here].iter().rev().find(|(_, d)| d < depth).map_or(at, |(i, _)| *i),
        Move::Go => {
            let node = (**node).clone();
            match &node.to {
                Some(to) => crate::navigate(s, p, to.clone()),
                None => toggle(s, p, &node),
            }
            at
        }
        Move::Type(t) => find_typed(s, p, &all, here, &t, now).unwrap_or(at),
    };
    let key = all[to].key();
    s.tree.cursor[p] = Some(key.clone());
    s.tree.version += 1;
    Some(key)
}

/// The row the letters typed so far lead to, `t` just typed: the next whose name (ignoring case
/// and accents) starts with them, from the keyboard's row (`here`, among the node rows) while a
/// name is being spelled out, or after it for a first letter. A letter pressed over and over goes
/// through the rows starting with it, as in Explorer.
fn find_typed(s: &mut State, p: usize, all: &[Line], here: usize, t: &str, now: f64) -> Option<usize> {
    let (typed, at) = &mut s.tree.typed[p];
    if now - *at > TYPE_PAUSE {
        typed.clear();
    }
    *at = now;
    typed.push_str(t);
    let rows: Vec<usize> = nodes(all).map(|(i, _)| i).collect();
    // Matched as the file list matches: the same folding, cycling and continuing.
    let k = crate::keyboard::find_by(rows.len(), Some(here), typed, |k, out| {
        if let Line::Node { node, .. } = &all[rows[k]] {
            openexplorer_index::fold::fold_into(&node.label, out);
        }
    })?;
    Some(rows[k])
}

/// Keys while pane `p`'s tree has focus. The tree takes those it moves by, and those that would
/// act on the files beside it (Delete, F2, copying...), which mean nothing here; the pane's other
/// keys (the address, Voltar, Atualizar, the search) go on to it.
pub fn key(s: &mut State, p: usize, ev: &KeyEvent, tx: &mut Tx) -> bool {
    let mods = ev.mods;
    let plain = !mods.cmd && !mods.ctrl && !mods.alt && !mods.logo;
    let m = match &ev.key {
        Key::Up if plain => Move::Up,
        Key::Down if plain => Move::Down,
        Key::PageUp if plain => Move::PageUp,
        Key::PageDown if plain => Move::PageDown,
        Key::Home if plain => Move::First,
        Key::End if plain => Move::Last,
        Key::Right if plain => Move::Open,
        Key::Left if plain => Move::Close,
        Key::Enter if plain => Move::Go,
        Key::Text(t) if plain && t == " " && !typing(s, p, s.now) => Move::Go,
        Key::Text(t) if plain && !t.chars().any(char::is_control) => Move::Type(t.clone()),
        // Out of the tree to the files beside it, either way: the tree sits between the path bar
        // and the list, and Tab in the list goes on to the other pane.
        Key::Tab => {
            tx.focus(crate::pane_key(p), true);
            return true;
        }
        // Voltar and Avançar work from anywhere in the pane.
        Key::Left | Key::Right if mods.alt => return false,
        // With modifiers these would move or open the files beside it.
        Key::Up | Key::Down | Key::Left | Key::Right | Key::Home | Key::End | Key::PageUp | Key::PageDown | Key::Enter => return true,
        Key::Delete | Key::Function(2) => return true,
        _ if ['a', 'c', 'n', 'v', 'x'].iter().any(|&c| ev.is_command(c)) => return true,
        _ => return false,
    };
    s.active = p;
    s.tree.focused[p] = true;
    if let Some(row) = press(s, p, m, s.now) {
        tx.scroll_into_view_in(crate::ui::tree_key(p), format!("t:{row}"));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DriveKind;

    /// A state whose desktop, user's folders and disk are scratch folders of their own under
    /// `name` (tests run at once).
    fn scene(name: &str) -> (State, PathBuf) {
        let root = std::env::temp_dir().join("openexplorer-tree-test").join(name);
        let _ = std::fs::remove_dir_all(&root);
        for d in ["Desk/Projeto", "Home/Downloads/Filmes", "Home/Notas", "Disco/Dados/2024"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let mut s = crate::init();
        s.places = Places {
            desktop: Some((root.join("Desk"), "Área de Trabalho".into())),
            desktop_also: Vec::new(),
            documents: None,
            pictures: None,
            user: vec![(root.join("Home/Downloads"), "Downloads".into()), (root.join("Home"), "Pessoal".into())],
            wsl: vec!["Ubuntu".into()],
        };
        s.local_drives = vec![(root.join("Disco"), DriveKind::Fixed)];
        (s, root)
    }

    fn ids(nodes: &[Node]) -> Vec<NodeId> {
        nodes.iter().map(|n| n.id.clone()).collect()
    }

    /// Each folder hangs from the nearest special folder: the user's folders and home under
    /// Área de Trabalho, anything else on a disk under Este Computador.
    #[test]
    fn a_folder_hangs_from_the_nearest_special_folder() {
        let (s, root) = scene("hangs");
        let local = |p: &str| NodeId::Local(root.join(p));
        assert_eq!(ids(&chain(&s, &Location::Local(root.join("Home/Downloads/Filmes")))), [NodeId::Desktop, local("Home/Downloads"), local("Home/Downloads/Filmes")]);
        assert_eq!(ids(&chain(&s, &Location::Local(root.join("Home/Notas")))), [NodeId::Desktop, local("Home"), local("Home/Notas")]);
        assert_eq!(ids(&chain(&s, &Location::Local(root.join("Desk/Projeto")))), [NodeId::Desktop, local("Desk/Projeto")]);
        assert_eq!(ids(&chain(&s, &Location::Local(root.join("Disco/Dados/2024")))), [NodeId::Desktop, NodeId::Computer, local("Disco"), local("Disco/Dados"), local("Disco/Dados/2024")]);
        // The user's folders show by their Windows names.
        assert_eq!(chain(&s, &Location::Local(root.join("Home")))[1].label, "Pessoal");
    }

    /// Área de Trabalho holds the desktop's folders, the user's folders and Este Computador,
    /// by name; a disk's folders read from it.
    #[test]
    fn the_desktop_and_a_folder_list_their_folders() {
        let (s, root) = scene("lists");
        let labels = |nodes: Vec<Node>| nodes.into_iter().map(|n| n.label).collect::<Vec<_>>();
        assert_eq!(labels(desktop_kids(&s.places, true)), ["Downloads", "Este Computador", "Linux", "Pessoal", "Projeto", "Rede"]);
        assert_eq!(labels(local_kids(&s.places, &root.join("Home"), true).unwrap()), ["Downloads", "Notas"]);
        assert!(local_kids(&s.places, &root.join("não existe"), true).is_err());
    }

    /// A folder on the network hangs from Rede › its computer (as Rede spells it) › its shared
    /// folder; one in WSL from Linux › its distribution. Rede lists the computers known.
    #[test]
    fn network_folders_hang_from_rede_and_linux() {
        let (mut s, _) = scene("network");
        let local = |p: &str| NodeId::Local(PathBuf::from(p));
        s.tree.saw_server("NAS");
        let nas = chain(&s, &Location::Local(PathBuf::from(r"\\nas\Fotos\2024")));
        assert_eq!(ids(&nas), [NodeId::Desktop, NodeId::Network, local(r"\\NAS"), local(r"\\NAS\Fotos"), local(r"\\NAS\Fotos\2024")]);
        assert_eq!(nas.iter().map(|n| n.label.as_str()).collect::<Vec<_>>(), ["Área de Trabalho", "Rede", "NAS", "Fotos", "2024"]);
        assert_eq!(ids(&chain(&s, &Location::Local(PathBuf::from(r"\\nas")))), [NodeId::Desktop, NodeId::Network, local(r"\\NAS")]);
        let wsl = chain(&s, &Location::Local(PathBuf::from(r"\\wsl.localhost\Ubuntu\home")));
        assert_eq!(ids(&wsl), [NodeId::Desktop, NodeId::Linux, local(r"\\wsl.localhost\Ubuntu"), local(r"\\wsl.localhost\Ubuntu\home")]);
        assert_eq!(ids(&chain(&s, &Location::Local(PathBuf::from(r"\\wsl.localhost")))), [NodeId::Desktop, NodeId::Linux]);
        // Linux lists the distributions; Rede, once searched, the computers known.
        let Kids::Ready(distros) = &*kids_of(&s, &linux_node()) else { panic!() };
        assert_eq!(ids(distros), [local(r"\\wsl.localhost\Ubuntu")]);
        s.tree.searched = true;
        s.tree.saw_server("nas");
        s.tree.saw_server("Estudio");
        let Kids::Ready(servers) = &*kids_of(&s, &network_node()) else { panic!() };
        assert_eq!(servers.iter().map(|n| n.label.as_str()).collect::<Vec<_>>(), ["Estudio", "NAS"]);

        // A listing of `\\nas` gives `\\NAS` its folders under Rede's spelling, so they match
        // the way down to them.
        let dir = |name: &str| Entry { id: name.into(), name: name.into(), is_dir: true, size: None, modified: None, target: None, mime: None, md5: None, starred: false };
        s.tree.kids.insert(local(r"\\NAS"), Kids::Ready(Vec::new()));
        s.panes[0].go(Location::Local(PathBuf::from(r"\\nas")));
        s.panes[0].set_entries(Ok(vec![dir("Fotos")]));
        learn(&mut s, 0);
        let Some(Kids::Ready(shares)) = s.tree.kids.get(&local(r"\\NAS")) else { panic!() };
        assert_eq!(ids(shares), [local(r"\\NAS\Fotos")]);
        // A listing of Linux brings a distribution installed since.
        s.panes[0].go(Location::Local(PathBuf::from(r"\\wsl.localhost")));
        s.panes[0].set_entries(Ok(vec![dir("Ubuntu"), dir("Debian")]));
        learn(&mut s, 0);
        assert_eq!(s.places.wsl, ["Debian", "Ubuntu"]);
    }

    /// The arrow opens and closes a node; the simulated drive's folders show at once, the
    /// folder shown marked.
    #[test]
    fn an_arrow_opens_and_closes_a_node() {
        let (mut s, _) = scene("arrow");
        s.panes[1].go(Location::Mock(vec!["Fotos".into()]));
        let shown = |s: &State| -> Vec<(usize, String, bool)> {
            lines(s, 1).into_iter().filter_map(|l| match l {
                Line::Node { depth, node, on, .. } if matches!(node.id, NodeId::Mock(_)) => Some((depth, node.label, on)),
                _ => None,
            }).collect()
        };
        assert_eq!(shown(&s), [(0, "Drive simulado".to_string(), false)]);
        let root = mock_node(Vec::new());
        toggle(&mut s, 1, &root);
        let open = shown(&s);
        assert_eq!(open.iter().map(|(d, l, _)| (*d, l.as_str())).collect::<Vec<_>>(), [(0, "Drive simulado"), (1, "Documentos"), (1, "Fotos"), (1, "Teste 50.000 arquivos")]);
        assert!(open.iter().any(|(_, l, on)| l == "Fotos" && *on));
        toggle(&mut s, 1, &root);
        assert_eq!(shown(&s).len(), 1);
    }

    /// Following a folder opens the way down to it; a listing of a folder whose folders the
    /// tree read replaces them.
    #[test]
    fn the_tree_follows_and_learns_from_listings() {
        let (mut s, root) = scene("learns");
        s.panes[0].go(Location::Local(root.join("Disco/Dados")));
        follow(&mut s, 0);
        for id in [NodeId::Desktop, NodeId::Computer, NodeId::Local(root.join("Disco"))] {
            assert!(s.tree.open[0].contains(&id), "{id:?}");
        }
        let dados = NodeId::Local(root.join("Disco/Dados"));
        s.tree.kids.insert(dados.clone(), Kids::Ready(Vec::new()));
        std::fs::create_dir_all(root.join("Disco/Dados/2025")).unwrap();
        let State { panes, mock, .. } = &mut s;
        panes[0].load_now(mock, true);
        learn(&mut s, 0);
        let Some(Kids::Ready(kids)) = s.tree.kids.get(&dados) else { panic!("not read") };
        assert_eq!(kids.iter().map(|k| k.label.as_str()).collect::<Vec<_>>(), ["2024", "2025"]);
    }

    /// Rows have keys of their own though a folder shows twice and two nodes load at once, and
    /// a disconnected account's folders are dropped.
    #[test]
    fn every_row_has_its_own_key() {
        let (mut s, root) = scene("keys");
        let downloads = local_node(&s.places, root.join("Home/Downloads"));
        let home = local_node(&s.places, root.join("Home"));
        s.tree.kids.insert(NodeId::Desktop, Kids::Ready(vec![downloads.clone(), home.clone()]));
        s.tree.kids.insert(home.id.clone(), Kids::Ready(vec![downloads.clone(), local_node(&s.places, root.join("Home/Notas"))]));
        s.tree.kids.insert(downloads.id.clone(), Kids::Loading);
        s.tree.kids.insert(NodeId::Local(root.join("Home/Notas")), Kids::Loading);
        for id in [NodeId::Desktop, home.id.clone(), downloads.id.clone(), NodeId::Local(root.join("Home/Notas"))] {
            s.tree.open[0].insert(id);
        }
        let keys: Vec<String> = lines(&s, 0).iter().map(Line::key).collect();
        assert_eq!(keys.iter().filter(|k| k.starts_with(&downloads.id.key())).count(), 2);
        assert_eq!(keys.len(), keys.iter().collect::<HashSet<_>>().len(), "{keys:#?}");
        assert_eq!(keys.iter().filter(|k| k.starts_with("note:")).count(), 3);

        s.tree.kids.insert(NodeId::Drive("a@b".into(), "root".into()), Kids::Ready(Vec::new()));
        s.tree.open[1].insert(NodeId::Account("a@b".into()));
        s.tree.forget("a@b");
        assert!(s.tree.kids.keys().all(|id| !matches!(id, NodeId::Drive(..))) && !s.tree.open[1].contains(&NodeId::Account("a@b".into())));
    }

    /// A folder moved on the website drops the lists it left and joined; a file changes no tree.
    #[test]
    fn a_folder_changed_in_drive_drops_the_lists_it_left_and_joined() {
        let (mut s, _) = scene("drive-changed");
        let (root, dest, other) = (NodeId::Drive("a@b".into(), "root".into()), NodeId::Drive("a@b".into(), "p2".into()), NodeId::Drive("a@b".into(), "p3".into()));
        let moved = drive_folder_node("a@b", Space::MyDrive, vec![("f1".into(), "Fotos".into())]);
        s.tree.kids.insert(root.clone(), Kids::Ready(vec![moved]));
        s.tree.kids.insert(dest.clone(), Kids::Ready(Vec::new()));
        s.tree.kids.insert(other.clone(), Kids::Ready(Vec::new()));
        let folder = |id: &str, dir: bool| openexplorer_gdrive::DriveFile {
            id: id.into(),
            name: id.into(),
            is_dir: dir,
            size: None,
            modified: None,
            mime: String::new(),
            target: None,
            starred: false,
            md5: None,
        };
        let change = |id: &str, dir: bool, parent: &str| openexplorer_gdrive::Change { file_id: id.into(), file: Some(folder(id, dir)), parents: vec![parent.into()], trashed: false, drive: false };
        drive_changed(&mut s, "a@b", &[change("x", false, "p3")], Some("R"));
        assert!(s.tree.kids.contains_key(&other), "a file changes no folder list");
        drive_changed(&mut s, "a@b", &[change("f1", true, "p2")], Some("R"));
        assert!(!s.tree.kids.contains_key(&root) && !s.tree.kids.contains_key(&dest) && s.tree.kids.contains_key(&other));
    }

    /// Folders read before Exibir arquivos ocultos changed don't land over the ones read after.
    #[test]
    fn folders_read_with_the_old_hidden_setting_are_dropped() {
        let (mut s, root) = scene("hidden-race");
        let home = NodeId::Local(root.join("Home"));
        s.settings.show_hidden = true;
        arrived(&mut s, home.clone(), Some(false), Ok(Vec::new()));
        assert!(!s.tree.kids.contains_key(&home));
        arrived(&mut s, home.clone(), Some(true), Ok(Vec::new()));
        assert!(matches!(s.tree.kids.get(&home), Some(Kids::Ready(_))));
        // Drive's folders don't depend on it.
        let drive = NodeId::Account("a@b".into());
        arrived(&mut s, drive.clone(), None, Ok(Vec::new()));
        assert!(matches!(s.tree.kids.get(&drive), Some(Kids::Ready(_))));
    }

    /// A scene whose pane 0 shows the simulated drive's top, with Área de Trabalho's folders
    /// read and nothing of the user's accounts or servers: the rows are Área de Trabalho,
    /// Downloads, Este Computador › Disco local, Linux, Pessoal, Projeto, Rede, then (under
    /// Testes) Drive simulado.
    fn keyboard_scene(name: &str) -> State {
        let (mut s, _) = scene(name);
        s.accounts.clear();
        s.servers.list.clear();
        let kids = desktop_kids(&s.places, true);
        s.tree.kids.insert(NodeId::Desktop, Kids::Ready(kids));
        s.tree.open[0].insert(NodeId::Desktop);
        s.panes[0].go(Location::Mock(Vec::new()));
        s
    }

    #[test]
    fn desktop_starts_collapsed_and_expands_independently_per_pane() {
        let (mut s, _) = scene("desktop-independent");
        assert!(!s.tree.open[0].contains(&NodeId::Desktop));
        assert!(!s.tree.open[1].contains(&NodeId::Desktop));
        let desktop = desktop_node(&s.places).unwrap();
        toggle(&mut s, 0, &desktop);
        assert!(s.tree.open[0].contains(&NodeId::Desktop));
        assert!(!s.tree.open[1].contains(&NodeId::Desktop));
    }

    /// The label of the row the keyboard is on.
    fn on_row(s: &State) -> String {
        let key = cursor(s, 0).expect("a row").to_string();
        lines(s, 0)
            .into_iter()
            .find_map(|l| match l {
                Line::Node { key: k, node, .. } if k == key => Some(node.label),
                _ => None,
            })
            .expect("the row shows")
    }

    /// The arrows go row by row past headings, Home and End to the ends; coming in, the keyboard
    /// lands on the folder shown.
    #[test]
    fn the_arrows_move_through_the_rows() {
        let mut s = keyboard_scene("keys-move");
        focus(&mut s, 0, true);
        assert_eq!(on_row(&s), "Drive simulado");
        press(&mut s, 0, Move::Up, 0.0);
        assert_eq!(on_row(&s), "Rede", "the Testes heading is skipped");
        press(&mut s, 0, Move::First, 0.0);
        assert_eq!(on_row(&s), "Área de Trabalho");
        press(&mut s, 0, Move::Up, 0.0);
        assert_eq!(on_row(&s), "Área de Trabalho", "the first row is the end");
        press(&mut s, 0, Move::Down, 0.0);
        assert_eq!(on_row(&s), "Downloads");
        press(&mut s, 0, Move::PageDown, 0.0);
        assert_eq!(on_row(&s), "Drive simulado");
        press(&mut s, 0, Move::First, 0.0);
        let key = press(&mut s, 0, Move::Last, 0.0);
        assert_eq!(on_row(&s), "Drive simulado");
        assert_eq!(key.as_deref(), Some("m:"), "the row to bring into view");
    }

    /// → opens a closed node, then goes to its first folder; ← goes up to the node holding a
    /// folder, then closes it; Enter goes to a folder, or opens and closes Este Computador.
    #[test]
    fn the_side_arrows_open_and_close_and_enter_goes() {
        let mut s = keyboard_scene("keys-open");
        focus(&mut s, 0, true);
        press(&mut s, 0, Move::Open, 0.0);
        assert!(s.tree.open[0].contains(&NodeId::Mock(Vec::new())));
        assert_eq!(on_row(&s), "Drive simulado", "opening stays on the node");
        press(&mut s, 0, Move::Open, 0.0);
        assert_eq!(on_row(&s), "Documentos");
        press(&mut s, 0, Move::Down, 0.0);
        press(&mut s, 0, Move::Go, 0.0);
        assert_eq!(s.panes[0].loc, Location::Mock(vec!["Fotos".into()]));
        assert_eq!(on_row(&s), "Fotos");
        press(&mut s, 0, Move::Close, 0.0);
        assert_eq!(on_row(&s), "Drive simulado");
        press(&mut s, 0, Move::Close, 0.0);
        assert!(!s.tree.open[0].contains(&NodeId::Mock(Vec::new())));
        press(&mut s, 0, Move::Close, 0.0);
        assert_eq!(on_row(&s), "Drive simulado", "a closed top node has nowhere to go");

        // Este Computador goes nowhere: Enter opens and closes it.
        press(&mut s, 0, Move::First, 0.0);
        press(&mut s, 0, Move::Down, 0.0);
        press(&mut s, 0, Move::Down, 0.0);
        assert_eq!(on_row(&s), "Este Computador");
        press(&mut s, 0, Move::Go, 0.0);
        assert!(!s.tree.open[0].contains(&NodeId::Computer));
        press(&mut s, 0, Move::Go, 0.0);
        assert!(s.tree.open[0].contains(&NodeId::Computer));
        press(&mut s, 0, Move::Open, 0.0);
        assert!(on_row(&s).starts_with("Disco local"));
        press(&mut s, 0, Move::Close, 0.0);
        assert_eq!(on_row(&s), "Este Computador");
    }

    /// Typing jumps to the next row whose name starts with the letters, ignoring case and
    /// accents; the same letter again goes on to the next; a pause starts a new name.
    #[test]
    fn typing_jumps_to_a_row_by_its_name() {
        let mut s = keyboard_scene("keys-type");
        focus(&mut s, 0, true);
        let typed = |s: &mut State, t: &str, now: f64| {
            press(s, 0, Move::Type(t.into()), now);
            on_row(s)
        };
        assert_eq!(typed(&mut s, "p", 10.0), "Pessoal", "after the row, going round");
        assert_eq!(typed(&mut s, "p", 10.2), "Projeto");
        assert_eq!(typed(&mut s, "P", 10.4), "Pessoal");
        assert_eq!(typed(&mut s, "a", 20.0), "Área de Trabalho");
        assert_eq!(typed(&mut s, "d", 30.0), "Downloads");
        assert_eq!(typed(&mut s, "r", 30.3), "Drive simulado", "“dr” spelled out");
        assert!(typing(&s, 0, 30.5), "Space types while a name is being spelled");
        assert!(!typing(&s, 0, 32.0));
        assert_eq!(typed(&mut s, "r", 32.0), "Rede", "a pause starts a new name");
        assert_eq!(typed(&mut s, "x", 40.0), "Rede", "no match stays");
        assert_eq!(typed(&mut s, "ESTE", 50.0), "Este Computador");
    }

    /// The keyboard's row gone (its node closed) falls back to the folder shown, and the first
    /// arrow then lands there.
    #[test]
    fn a_row_gone_falls_back_to_the_folder_shown() {
        let mut s = keyboard_scene("keys-gone");
        focus(&mut s, 0, true);
        press(&mut s, 0, Move::Open, 0.0);
        press(&mut s, 0, Move::Open, 0.0);
        assert_eq!(on_row(&s), "Documentos");
        toggle(&mut s, 0, &mock_node(Vec::new()));
        press(&mut s, 0, Move::Down, 0.0);
        assert_eq!(on_row(&s), "Drive simulado");
        // Leaving and coming back keeps a row that's still there.
        press(&mut s, 0, Move::First, 0.0);
        focus(&mut s, 0, false);
        focus(&mut s, 0, true);
        assert_eq!(on_row(&s), "Área de Trabalho");
    }
}
