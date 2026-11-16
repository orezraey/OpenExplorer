//! The tree beside each pane, as in Air Explorer and Explorer's navigation pane: Área de
//! Trabalho, holding the desktop's folders, the user's folders, the home folder and Este
//! Computador with its disks; then each Google Drive account with Meu Drive, its shared drives
//! and Drive's lists; then the simulated drive.
//!
//! A folder's arrow opens it. Its folders are read the first time it opens (from the disk on a
//! thread, or from Drive) and kept, and refreshed whenever a pane lists that folder. Each pane
//! opens its own tree down to the folder it shows and marks it.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rune::Icon;

use crate::model::{DriveKind, Entry, Location, Space, shown};
use crate::places::Places;
use crate::sysicons::IconKey;
use crate::{Msg, State, background, drive_task, icons};

/// Which node: also what its folders are kept by.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NodeId {
    Desktop,
    Computer,
    Local(PathBuf),
    Account(String),
    /// In an account: `"root"` (Meu Drive), `"shared-drives"`, `"view:…"` for Drive's lists,
    /// else a folder's or a shared drive's id.
    Drive(String, String),
    /// The simulated drive's folders, from its top.
    Mock(Vec<String>),
}

impl NodeId {
    /// Its row's key, unique in a pane's tree.
    pub fn key(&self) -> String {
        match self {
            NodeId::Desktop => "desktop".into(),
            NodeId::Computer => "computer".into(),
            NodeId::Local(p) => format!("l:{}", p.display()),
            NodeId::Account(e) => format!("a:{e}"),
            NodeId::Drive(e, id) => format!("d:{e}:{id}"),
            NodeId::Mock(segs) => format!("m:{}", segs.join("/")),
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
}

/// A row of a pane's tree.
pub enum Line {
    Heading(&'static str),
    Node {
        /// Unique in the tree: a folder that shows twice (Downloads under Área de Trabalho and
        /// under C: › Users › …) gets its id's key, then that key and `#2`.
        key: String,
        depth: usize,
        node: Node,
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
    /// The nodes open from the start: Área de Trabalho, Este Computador and each account.
    pub fn new(accounts: impl IntoIterator<Item = String>) -> Tree {
        let open: HashSet<NodeId> = [NodeId::Desktop, NodeId::Computer].into_iter().chain(accounts.into_iter().map(NodeId::Account)).collect();
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
}

// ---------------------------------------------------------------------------------------------
// Nodes

fn desktop_node(places: &Places) -> Option<Node> {
    let (path, name) = places.desktop.clone()?;
    Some(Node { id: NodeId::Desktop, label: name, icon: Some(IconKey::Path(path.clone())), fallback: &icons::DESKTOP, to: Some(Location::Local(path)), opens: true })
}

fn computer_node() -> Node {
    Node { id: NodeId::Computer, label: "Este Computador".into(), icon: None, fallback: &icons::COMPUTER, to: None, opens: true }
}

/// A folder on the computer: one of the user's folders by its Windows name and icon, any
/// other by its own name.
fn local_node(places: &Places, path: PathBuf) -> Node {
    let (label, icon) = match places.name_of(&path) {
        Some(name) => (name.to_string(), IconKey::Path(path.clone())),
        None => (path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned()), IconKey::Folder),
    };
    Node { id: NodeId::Local(path.clone()), label, icon: Some(icon), fallback: &icons::FOLDER, to: Some(Location::Local(path)), opens: true }
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
    Node { id: NodeId::Account(email.into()), label: email.into(), icon: None, fallback: &icons::CLOUD, to: Some(Location::drive(email, Space::MyDrive)), opens: true }
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
        None => ("Drive simulado".into(), &icons::LAB),
    };
    let icon = (!segs.is_empty()).then_some(IconKey::Folder);
    Node { id: NodeId::Mock(segs.clone()), label, icon, fallback, to: Some(Location::Mock(segs)), opens: true }
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
    let done = {
        let id = id.clone();
        move |result| Msg::TreeKids { id, result }
    };
    match (&node.id, node.to.clone()) {
        (NodeId::Desktop, _) => {
            let places = s.places.clone();
            background(s, move |emit| emit(done(crate::catch_panic("ler a área de trabalho", || Ok(desktop_kids(&places))))));
        }
        (NodeId::Local(path), _) => {
            let (places, path) = (s.places.clone(), path.clone());
            background(s, move |emit| emit(done(crate::catch_panic("ler as pastas", || local_kids(&places, &path)))));
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
            drive_task(s, &email.clone(), "ler as pastas do Drive", work, done);
        }
        _ => return,
    }
    s.tree.kids.insert(id, Kids::Loading);
    s.tree.version += 1;
}

fn extend(path: &[(String, String)], id: &str, name: &str) -> Vec<(String, String)> {
    path.iter().cloned().chain([(id.to_string(), name.to_string())]).collect()
}

/// Área de Trabalho's folders: the desktop's own (the user's and the shared one), the user's
/// folders, the home folder and Este Computador, by name.
fn desktop_kids(places: &Places) -> Vec<Node> {
    let mut seen = HashSet::new();
    let mut nodes = Vec::new();
    let on_desktop = places.desktop.iter().map(|(p, _)| p.clone()).chain(places.desktop_also.iter().cloned());
    for dir in on_desktop {
        for path in subfolders(&dir).unwrap_or_default() {
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
    nodes.push(computer_node());
    sorted(nodes)
}

fn local_kids(places: &Places, dir: &Path) -> Result<Vec<Node>, String> {
    let folders = subfolders(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(sorted(folders.into_iter().map(|p| local_node(places, p)).collect()))
}

/// The folders in `dir` (links to folders included). The listing already says which entries
/// are folders; only a link is looked up, so a folder of thousands of files costs no more.
fn subfolders(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let folder = |e: &std::fs::DirEntry| e.file_type().is_ok_and(|t| t.is_dir() || (t.is_symlink() && e.path().is_dir()));
    Ok(std::fs::read_dir(dir)?.filter_map(Result::ok).filter(folder).map(|e| e.path()).collect())
}

/// `id`'s folders arrived.
pub fn arrived(s: &mut State, id: NodeId, result: Result<Vec<Node>, String>) {
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
    if !matches!(s.tree.kids.get(&node.id), Some(Kids::Ready(_) | Kids::Failed(_))) {
        return;
    }
    let dirs = pane.entries.iter().filter(|e| e.is_dir);
    let kids: Vec<Node> = match (&node.id, &pane.loc) {
        (NodeId::Local(_), Location::Local(dir)) => dirs.map(|e| local_node(&s.places, dir.join(&e.name))).collect(),
        (NodeId::Drive(email, _), Location::Drive { space: Space::SharedDrives, .. }) => dirs.map(|e| shared_drive_node(email, e.id.clone(), e.name.clone())).collect(),
        (NodeId::Drive(email, _), Location::Drive { space, path, .. }) => dirs.map(|e| drive_folder_node(email, space.clone(), extend(path, opened(e), &e.name))).collect(),
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
        load(s, node);
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
    w.node(desktop_node(&s.places).unwrap_or_else(computer_node), 0);
    if !s.accounts.is_empty() {
        w.out.push(Line::Heading("Google Drive"));
    }
    for a in &s.accounts {
        w.node(account_node(&a.email), 0);
    }
    w.out.push(Line::Heading("Testes"));
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
        let empty = matches!(&*kids, Kids::Ready(k) if k.is_empty());
        let on = self.current == Some(&node.id);
        let id = node.id.key();
        let times = self.seen.entry(id.clone()).or_default();
        *times += 1;
        let key = if *times == 1 { id } else { format!("{id}#{times}") };
        let note = |text: String, loading| Line::Note { key: format!("note:{key}"), depth: depth + 1, text, loading };
        let after = match &*kids {
            Kids::Loading => Some(note("Carregando…".into(), true)),
            Kids::Failed(e) => Some(note(e.clone(), false)),
            Kids::Ready(_) | Kids::Unknown => None,
        };
        self.out.push(Line::Node { key, depth, node, open, empty, on });
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
            user: vec![(root.join("Home/Downloads"), "Downloads".into()), (root.join("Home"), "Pessoal".into())],
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
        assert_eq!(labels(desktop_kids(&s.places)), ["Downloads", "Este Computador", "Pessoal", "Projeto"]);
        assert_eq!(labels(local_kids(&s.places, &root.join("Home")).unwrap()), ["Downloads", "Notas"]);
        assert!(local_kids(&s.places, &root.join("não existe")).is_err());
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
        panes[0].load_now(mock);
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
        for id in [home.id.clone(), downloads.id.clone(), NodeId::Local(root.join("Home/Notas"))] {
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
}
