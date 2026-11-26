//! What the panes show: a folder listing on the local disk, in a simulated drive kept in memory,
//! or in a connected Google Drive account, and the selection and sort of each pane.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use openexplorer_i18n::{t, tr};

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    /// Unique in its folder: the name on disk and in the simulated drive, the file id in Google
    /// Drive (where two files can share a name).
    pub id: String,
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    #[serde(with = "unix_ms")]
    pub modified: Option<SystemTime>,
    /// For a Drive shortcut to a folder: the folder it opens, instead of `id`.
    pub target: Option<String>,
    /// Drive's MIME type, which names Google's own formats (Docs, Sheets...) that have no
    /// extension.
    #[serde(default)]
    pub mime: Option<String>,
    /// Drive's MD5 of the file, from the listing, so Propriedades shows it at once.
    #[serde(default)]
    pub md5: Option<String>,
    /// Starred in Google Drive, for the menu to offer Adicionar or Remover estrela.
    #[serde(default)]
    pub starred: bool,
    /// Where a Google Drive search result came from (not persisted in disk folder caches).
    #[serde(skip)]
    pub drive_origin: Option<Box<DriveOrigin>>,
}

/// Where a Google Drive search result comes from: its account, shared drive (if any),
/// ownership flag, and formatted label for the 4th column ("Local").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveOrigin {
    pub email: String,
    /// `(drive_id, drive_name)` when the item is in a shared drive.
    pub shared_drive: Option<(String, String)>,
    pub owned_by_me: bool,
    /// Column text: e.g. "Séries HD", "Meu Drive", "Compartilhados comigo",
    /// or in multi-account search: "conta@gmail.com · Séries HD".
    pub label: String,
}

/// What a file is, for its icon and the Tipo column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Folder,
    FolderLink,
    Image,
    Video,
    Audio,
    Pdf,
    Doc,
    Sheet,
    Slides,
    Archive,
    Code,
    Text,
    App,
    Torrent,
    Other,
}

impl Entry {
    /// The lowercased extension, if the name has a plausible one.
    pub fn ext(&self) -> Option<String> {
        let (stem, ext) = self.name.rsplit_once('.')?;
        (!stem.is_empty() && !ext.is_empty() && ext.len() <= 8 && !ext.contains(' ')).then(|| ext.to_ascii_lowercase())
    }

    pub fn kind(&self) -> Kind {
        if self.is_dir {
            return if self.target.is_some() { Kind::FolderLink } else { Kind::Folder };
        }
        match self.mime.as_deref() {
            Some("application/vnd.google-apps.document") => return Kind::Doc,
            Some("application/vnd.google-apps.spreadsheet") => return Kind::Sheet,
            Some("application/vnd.google-apps.presentation") => return Kind::Slides,
            _ => {}
        }
        match self.ext().as_deref() {
            Some("jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "heic" | "heif" | "svg" | "tif" | "tiff" | "raw" | "ico" | "avif") => Kind::Image,
            Some("mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "m4v" | "flv" | "mpg" | "mpeg" | "m2ts") => Kind::Video,
            Some("mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "opus" | "wma") => Kind::Audio,
            Some("pdf") => Kind::Pdf,
            Some("doc" | "docx" | "odt" | "rtf") => Kind::Doc,
            Some("xls" | "xlsx" | "xlsm" | "ods" | "csv") => Kind::Sheet,
            Some("ppt" | "pptx" | "odp" | "key") => Kind::Slides,
            Some("zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" | "iso") => Kind::Archive,
            Some("rs" | "js" | "ts" | "tsx" | "jsx" | "py" | "java" | "c" | "cpp" | "h" | "cs" | "go" | "html" | "css" | "json" | "toml" | "yaml" | "yml" | "xml" | "sh" | "ps1" | "bat" | "sql" | "ipynb") => Kind::Code,
            Some("txt" | "md" | "log" | "ini" | "cfg" | "srt") => Kind::Text,
            Some("exe" | "msi" | "apk" | "dmg" | "appimage" | "deb" | "rpm" | "lnk") => Kind::App,
            Some("torrent") => Kind::Torrent,
            _ => Kind::Other,
        }
    }

    /// The Tipo column: "Pasta de arquivos", "Arquivo JPG", "Documento Google"...
    pub fn kind_label(&self) -> String {
        if self.is_dir {
            return if self.target.is_some() { t("Atalho de pasta", "Folder shortcut").into() } else { t("Pasta de arquivos", "File folder").into() };
        }
        let google = match self.mime.as_deref() {
            Some("application/vnd.google-apps.document") => Some(t("Documento Google", "Google Doc")),
            Some("application/vnd.google-apps.spreadsheet") => Some(t("Planilha Google", "Google Sheet")),
            Some("application/vnd.google-apps.presentation") => Some(t("Apresentação Google", "Google Slides")),
            Some("application/vnd.google-apps.form") => Some(t("Formulário Google", "Google Form")),
            Some("application/vnd.google-apps.drawing") => Some(t("Desenho Google", "Google Drawing")),
            Some("application/vnd.google-apps.shortcut") => Some(t("Atalho", "Shortcut")),
            Some(m) if m.starts_with("application/vnd.google-apps.") => Some(t("Arquivo Google", "Google file")),
            _ => None,
        };
        if let Some(g) = google {
            return g.into();
        }
        match self.ext() {
            Some(ext) => {
                let ext = ext.to_ascii_uppercase();
                tr!("Arquivo {ext}", "{ext} File")
            }
            None => t("Arquivo", "File").into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Location {
    Local(PathBuf),
    /// What the search index found on the computer: files from many folders, each entry's id
    /// its full path (see [`crate::finder`]).
    Found(Box<Found>),
    /// A folder in the simulated drive, as its path segments from the root.
    Mock(Vec<String>),
    /// A folder in a connected account: the (id, name) of each folder from the top of `space`
    /// down.
    Drive { email: String, space: Space, path: Vec<(String, String)> },
    /// A folder on an FTP, SFTP or WebDAV server the user added: the server, and the folders
    /// from its root down. With `find`, the search for those words in that folder and below,
    /// each result's id its path from the folder searched (see [`crate::servers`]).
    Server { server: Box<openexplorer_remote::Server>, path: Vec<String>, find: Option<String> },
}

/// A search of the computer: its words, the folder it looks under (everywhere when `None`), and
/// that place in words.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub text: String,
    pub within: Option<PathBuf>,
    pub place: String,
}

/// Where a Drive location's folders hang from.
#[derive(Clone, Debug, PartialEq)]
pub enum Space {
    MyDrive,
    /// The list of the account's shared drives; its "folders" are the drives. `path` is empty.
    SharedDrives,
    /// Inside one shared drive.
    Shared { id: String, name: String },
    /// Drive's lists of files gathered by a property, not a place: Compartilhados comigo, Com
    /// estrela, Recentes and Lixeira. With an empty `path` the location is the list itself; a
    /// folder opened from it lists as any folder (in Lixeira, a trashed folder's contents).
    SharedWithMe,
    Starred,
    Recent,
    Trash,
    /// Results of a search; a folder opened from them lists as any folder.
    Search(Box<Search>),
}

/// A search's words and where it looks, as a pane shows its results.
#[derive(Clone, Debug, PartialEq)]
pub struct Search {
    pub text: String,
    pub within: openexplorer_gdrive::search::Within,
    /// Where it looks, in words: "Meu Drive", "Projetos", "todo o Drive".
    pub place: String,
}

impl Space {
    /// The Drive list a view shows, `None` for the spaces made of folders.
    pub fn view(&self) -> Option<openexplorer_gdrive::View> {
        use openexplorer_gdrive::View;
        match self {
            Space::SharedWithMe => Some(View::SharedWithMe),
            Space::Starred => Some(View::Starred),
            Space::Recent => Some(View::Recent),
            Space::Trash => Some(View::Trash),
            Space::MyDrive | Space::SharedDrives | Space::Shared { .. } | Space::Search(_) => None,
        }
    }

    /// Its name in the tree and the path bar.
    pub fn label(&self) -> &str {
        match self {
            Space::MyDrive => t("Meu Drive", "My Drive"),
            Space::SharedDrives => t("Drives compartilhados", "Shared drives"),
            Space::Shared { name, .. } => name,
            Space::SharedWithMe => t("Compartilhados comigo", "Shared with me"),
            Space::Starred => t("Com estrela", "Starred"),
            Space::Recent => t("Recentes", "Recent"),
            Space::Trash => t("Lixeira", "Trash"),
            Space::Search(_) => t("Pesquisa", "Search"),
        }
    }
}

impl Location {
    pub fn drive(email: impl Into<String>, space: Space) -> Location {
        Location::Drive { email: email.into(), space, path: Vec::new() }
    }

    /// Whether what this lists is on the computer's disks: a folder, or search results.
    pub fn is_local(&self) -> bool {
        matches!(self, Location::Local(_) | Location::Found(_))
    }

    /// Where `e`, listed here, is on disk: in the folder shown or, for a search's results, where
    /// it was found. `None` off the computer.
    pub fn local_path(&self, e: &Entry) -> Option<PathBuf> {
        match self {
            Location::Local(dir) => Some(dir.join(&e.name)),
            Location::Found(_) => Some(PathBuf::from(&e.id)),
            _ => None,
        }
    }

    /// Whether this lists a search's results read from folders (the computer's, a server's):
    /// each entry's id is its path, and the Tipo column shows the folder it's in.
    pub fn finds(&self) -> bool {
        matches!(self, Location::Found(_) | Location::Server { find: Some(_), .. })
    }

    /// Whether this lists Google Drive search results,
    /// where the 4th column shows the item's Drive/account location.
    pub fn is_drive_search(&self) -> bool {
        matches!(self, Location::Drive { space: Space::Search(_), path, .. } if path.is_empty())
    }

    pub fn home() -> Location {
        let (var, fallback) = if cfg!(windows) { ("USERPROFILE", "C:\\") } else { ("HOME", "/") };
        Location::Local(std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(fallback)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortCol {
    Name,
    Size,
    Modified,
    Kind,
}

/// How a pane displays its folder's entries, as in Windows Explorer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ViewMode {
    ExtraLargeIcons,
    LargeIcons,
    MediumIcons,
    List,
    #[default]
    #[serde(other)]
    Details,
}

impl ViewMode {
    pub const ALL: [ViewMode; 5] = [
        ViewMode::ExtraLargeIcons,
        ViewMode::LargeIcons,
        ViewMode::MediumIcons,
        ViewMode::List,
        ViewMode::Details,
    ];

    /// Advances directly to the next view mode (clicking the main icon half of the split button).
    pub fn next(self) -> ViewMode {
        match self {
            ViewMode::ExtraLargeIcons => ViewMode::LargeIcons,
            ViewMode::LargeIcons => ViewMode::MediumIcons,
            ViewMode::MediumIcons => ViewMode::List,
            ViewMode::List => ViewMode::Details,
            ViewMode::Details => ViewMode::ExtraLargeIcons,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ViewMode::ExtraLargeIcons => t("Ícones extra grandes", "Extra large icons"),
            ViewMode::LargeIcons => t("Ícones grandes", "Large icons"),
            ViewMode::MediumIcons => t("Ícones médios", "Medium icons"),
            ViewMode::List => t("Lista", "List"),
            ViewMode::Details => t("Detalhes", "Details"),
        }
    }

    pub fn icon(self) -> &'static rune::Icon {
        match self {
            ViewMode::ExtraLargeIcons => &crate::icons::VM_EXTRA_LARGE,
            ViewMode::LargeIcons => &crate::icons::VM_LARGE,
            ViewMode::MediumIcons => &crate::icons::VM_MEDIUM,
            ViewMode::List => &crate::icons::VM_LIST,
            ViewMode::Details => &crate::icons::VM_DETAILS,
        }
    }

    pub fn is_grid(self) -> bool {
        matches!(self, ViewMode::ExtraLargeIcons | ViewMode::LargeIcons | ViewMode::MediumIcons)
    }

    /// `(tile_w, tile_h, icon_size)` for grid modes, sized to match Windows Explorer.
    pub fn tile_metrics(self) -> (f32, f32, f32) {
        match self {
            ViewMode::ExtraLargeIcons => (236.0, 268.0, 200.0),
            ViewMode::LargeIcons => (128.0, 152.0, 96.0),
            ViewMode::MediumIcons => (96.0, 102.0, 48.0),
            ViewMode::List | ViewMode::Details => (0.0, 0.0, 16.0),
        }
    }
}

pub struct Pane {
    pub loc: Location,
    pub entries: Vec<Entry>,
    /// Entry ids.
    pub selected: HashSet<String>,
    pub anchor: Option<String>,
    pub cursor: Option<String>,
    pub sort: (SortCol, bool),
    pub view_mode: ViewMode,
    /// Bumped whenever `entries` change, so the virtual list skips re-reading 50,000 keys.
    pub version: u64,
    pub drop_over: bool,
    pub error: Option<String>,
    /// A listing is on its way from a background thread.
    pub loading: bool,
    /// Where Voltar and Avançar go.
    pub back: Vec<Location>,
    pub forward: Vec<Location>,
    /// Free space on the local disk shown, read with each listing.
    pub free: Option<u64>,
    /// The entry (id, name) to select and scroll to once the listing brings it (Mostrar na
    /// pasta).
    pub reveal: Option<(String, String)>,
    /// Counts moves to another location (not reloads), so an answer looked up for where the pane
    /// was (Mostrar na pasta, the address) is dropped once it moved on.
    pub moves: u64,
    /// Where the cursor's item stood in the last listing that had it, and whether it was
    /// selected: once a new listing lacks it (deleted, moved, renamed elsewhere), the cursor
    /// goes to the item now there rather than pointing at nothing. Kept across a listing's
    /// pages, since the item may be on one still to come.
    pub cursor_was: Option<(usize, bool)>,
}

/// How a pane orders entries: folders first, as in Explorer, then by the column (`by`), ties
/// by name; `asc` turns all but the folders-first around.
fn order(asc: bool, a: &Entry, b: &Entry, by: Ordering) -> Ordering {
    b.is_dir.cmp(&a.is_dir).then_with(|| {
        let o = if by == Ordering::Equal { rune_kit::natural_cmp(&a.name, &b.name) } else { by };
        if asc { o } else { o.reverse() }
    })
}

/// What the fourth column sorts by: the type, the folder (for computer/server searches),
/// or the Drive origin label (for Google Drive search results).
fn kind_key(found: bool, e: &Entry) -> String {
    if found {
        crate::finder::folder_of(e)
    } else if let Some(origin) = &e.drive_origin {
        origin.label.clone()
    } else {
        e.kind_label()
    }
}

fn by_column(col: SortCol, a: &Entry, b: &Entry) -> Ordering {
    match col {
        SortCol::Name => Ordering::Equal,
        SortCol::Size => a.size.cmp(&b.size),
        SortCol::Modified => a.modified.cmp(&b.modified),
        SortCol::Kind => a.kind_label().cmp(&b.kind_label()),
    }
}

impl Pane {
    pub fn new(loc: Location) -> Pane {
        Pane {
            loc,
            entries: Vec::new(),
            selected: HashSet::new(),
            anchor: None,
            cursor: None,
            sort: (SortCol::Name, true),
            view_mode: ViewMode::Details,
            version: 0,
            drop_over: false,
            error: None,
            loading: false,
            back: Vec::new(),
            forward: Vec::new(),
            free: None,
            reveal: None,
            moves: 0,
            cursor_was: None,
        }
    }

    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }

    pub fn entry(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Moves to `loc`, forgetting the selection.
    pub fn go(&mut self, loc: Location) {
        self.loc = loc;
        self.selected.clear();
        self.cursor = None;
        self.anchor = None;
        self.reveal = None;
        self.cursor_was = None;
        self.moves += 1;
        self.entries.clear();
        self.version += 1;
    }

    /// Lists a local or simulated folder in place. Drive folders, and the network's, load in the
    /// background instead.
    pub fn load_now(&mut self, mock: &MockDrive, show_hidden: bool) {
        self.free = match &self.loc {
            Location::Local(dir) => free_space(dir),
            _ => None,
        };
        let entries = match &self.loc {
            Location::Local(dir) => read_local(dir, show_hidden).map_err(|e| format!("{}: {e}", dir.display())),
            Location::Mock(segs) => Ok(mock.list(segs)),
            Location::Drive { .. } | Location::Found(_) | Location::Server { .. } => return,
        };
        self.set_entries(entries);
    }

    pub fn set_entries(&mut self, entries: Result<Vec<Entry>, String>) {
        // A cursor not in the listing being replaced (one set for a folder still loading, or
        // the item of an earlier page) keeps the place noted before.
        if let Some(c) = &self.cursor
            && let Some(i) = self.index_of(c)
        {
            self.cursor_was = Some((i, self.selected.contains(c)));
        }
        self.loading = false;
        match entries {
            Ok(v) => {
                self.error = None;
                self.entries = v;
            }
            Err(e) => {
                self.error = Some(e);
                self.entries.clear();
            }
        }
        // Sorted first: the cursor falls back on the item shown at its old place.
        self.sort_entries();
        self.prune_selection();
    }

    /// Forgets selected ids the listing doesn't have (anymore). A cursor whose item went moves
    /// to the item now at its place, or the last one, and the selection follows it when it
    /// was that item, as in Explorer; the next arrow then goes on from there, not the top.
    pub fn prune_selection(&mut self) {
        let ids: HashSet<&str> = self.entries.iter().map(|e| e.id.as_str()).collect();
        self.selected.retain(|id| ids.contains(id.as_str()));
        if self.anchor.as_deref().is_some_and(|a| !ids.contains(a)) {
            self.anchor = None;
        }
        let Some(c) = &self.cursor else { return };
        if ids.contains(c.as_str()) {
            return;
        }
        let at = self.cursor_was.and_then(|(i, sel)| Some((self.entries.get(i).or(self.entries.last())?, sel)));
        let Some((e, was_selected)) = at else {
            self.cursor = None;
            return;
        };
        let id = e.id.clone();
        if was_selected && self.selected.is_empty() {
            self.selected.insert(id.clone());
            self.anchor = Some(id.clone());
        }
        self.anchor.get_or_insert_with(|| id.clone());
        self.cursor = Some(id);
    }

    /// Adds a later page of a listing that's still arriving: the page is sorted on its own and
    /// merged in, each of its entries finding its place by binary search, rather than sorting
    /// everything again, which a search's hundred thousand results made slow page after page.
    pub fn append_entries(&mut self, more: Vec<Entry>) {
        let (col, asc) = self.sort;
        // By Tipo, a new entry's label is built once, not at each comparison.
        let found = self.loc.finds();
        let label = |e: &Entry| (col == SortCol::Kind).then(|| kind_key(found, e));
        let cmp = |a: &Entry, ka: &Option<String>, b: &Entry, kb: &Option<String>| {
            let by = match (ka, kb) {
                (Some(x), Some(y)) => x.cmp(y),
                _ => by_column(col, a, b),
            };
            order(asc, a, b, by)
        };
        let mut more: Vec<(Option<String>, Entry)> = more.into_iter().map(|e| (label(&e), e)).collect();
        more.sort_by(|(ka, a), (kb, b)| cmp(a, ka, b, kb));
        let old = std::mem::take(&mut self.entries);
        // Where each new entry goes among the old ones, after any equal to it.
        let mut cuts = Vec::with_capacity(more.len());
        let mut at = 0;
        for (k, e) in &more {
            at += old[at..].partition_point(|o| cmp(o, &label(o), e, k) != Ordering::Greater);
            cuts.push(at);
        }
        let more = more.into_iter().map(|(_, e)| e);
        let mut merged = Vec::with_capacity(old.len() + more.len());
        let mut old = old.into_iter();
        let mut taken = 0;
        for (e, cut) in more.into_iter().zip(cuts) {
            merged.extend(old.by_ref().take(cut - taken));
            taken = cut;
            merged.push(e);
        }
        merged.extend(old);
        self.entries = merged;
        self.version += 1;
    }

    pub fn sort_entries(&mut self) {
        let (col, asc) = self.sort;
        if col == SortCol::Kind {
            // The label is built once per entry rather than twice per comparison.
            let found = self.loc.finds();
            let mut keyed: Vec<(String, Entry)> = std::mem::take(&mut self.entries).into_iter().map(|e| (kind_key(found, &e), e)).collect();
            keyed.sort_by(|(ka, a), (kb, b)| order(asc, a, b, ka.cmp(kb)));
            self.entries = keyed.into_iter().map(|(_, e)| e).collect();
        } else {
            self.entries.sort_by(|a, b| order(asc, a, b, by_column(col, a, b)));
        }
        self.version += 1;
    }

    pub fn title(&self) -> String {
        match &self.loc {
            Location::Local(p) => p.display().to_string(),
            Location::Found(f) => tr!("“{}” em {}", "“{}” in {}", f.text, f.place),
            Location::Mock(segs) => tr!("Meu Drive/{}", "My Drive/{}", segs.join("/")),
            Location::Drive { space, path, .. } => {
                let mut parts: Vec<&str> = match space {
                    Space::Shared { name, .. } => vec![t("Drives compartilhados", "Shared drives"), name],
                    other => vec![other.label()],
                };
                parts.extend(path.iter().map(|(_, n)| n.as_str()));
                parts.join("/")
            }
            Location::Server { server, path, find } => {
                let place = std::iter::once(server.name.as_str()).chain(path.iter().map(String::as_str)).collect::<Vec<_>>().join("/");
                match find {
                    Some(text) => tr!("“{text}” em {place}", "“{text}” in {place}"),
                    None => place,
                }
            }
        }
    }
}

/// Whether `name` or its OS metadata marks it as hidden (`.` dotfiles, or Windows'
/// `FILE_ATTRIBUTE_HIDDEN`).
pub fn is_hidden(name: &str, meta: Option<&std::fs::Metadata>) -> bool {
    if name.starts_with('.') {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.is_some_and(|m| m.file_attributes() & 0x2 != 0) {
            return true;
        }
    }
    #[cfg(not(windows))]
    let _ = meta;
    false
}

/// What a folder on the computer holds; a computer on the network (`\\servidor`) holds its
/// shared folders.
pub fn read_local(dir: &Path, show_hidden: bool) -> std::io::Result<Vec<Entry>> {
    if let Some((server, _)) = crate::network::unc(dir).filter(|(_, share)| share.is_empty()) {
        let folder = |name: String| Entry { id: name.clone(), name, is_dir: true, size: None, modified: None, target: None, mime: None, md5: None, starred: false, drive_origin: None };
        return Ok(crate::network::shares(&server)?.into_iter().map(folder).collect());
    }
    let mut out = Vec::new();
    for item in std::fs::read_dir(dir)? {
        let Ok(item) = item else { continue };
        let name = item.file_name().to_string_lossy().into_owned();
        let meta = item.metadata().ok();
        if !show_hidden && is_hidden(&name, meta.as_ref()) {
            continue;
        }
        let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
        out.push(Entry {
            id: name.clone(),
            name,
            is_dir,
            size: if is_dir { None } else { meta.as_ref().map(|m| m.len()) },
            modified: meta.and_then(|m| m.modified().ok()),
            target: None,
            mime: None,
            md5: None,
            starred: false,
            drive_origin: None,
        });
    }
    Ok(out)
}

/// A Google Drive stand-in held in memory, to try the panes without an account.
pub struct MockDrive {
    folders: HashMap<String, Vec<Entry>>,
}

impl MockDrive {
    /// No folders: the window stand-in's, which nothing lists.
    pub fn empty() -> MockDrive {
        MockDrive { folders: HashMap::new() }
    }

    pub fn new() -> MockDrive {
        let now = SystemTime::now();
        let ago = |days: u64| now - Duration::from_secs(days * 86_400);
        let dir = |name: &str, days| Entry { id: name.into(), name: name.into(), is_dir: true, size: None, modified: Some(ago(days)), target: None, mime: None, md5: None, starred: false, drive_origin: None };
        let file = |name: String, size: u64, days| Entry { id: name.clone(), name, is_dir: false, size: Some(size), modified: Some(ago(days)), target: None, mime: None, md5: None, starred: false, drive_origin: None };

        let mut folders = HashMap::new();
        folders.insert(
            String::new(),
            vec![
                dir("Documentos", 3),
                dir("Fotos", 10),
                dir("Teste 50.000 arquivos", 1),
                file("Leia-me.txt".into(), 2_310, 40),
                file("Planilha 2026.xlsx".into(), 184_220, 2),
            ],
        );
        folders.insert(
            "Documentos".into(),
            vec![file("Contrato.pdf".into(), 1_204_330, 12), file("Notas.md".into(), 8_120, 1), file("Orçamento.docx".into(), 54_002, 30)],
        );
        // A small xorshift keeps the generated data the same on every run.
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        folders.insert("Fotos".into(), (1..=300).map(|i| file(format!("IMG_{i:04}.jpg"), 900_000 + next() % 6_000_000, next() % 900)).collect());
        folders.insert(
            "Teste 50.000 arquivos".into(),
            (1..=50_000).map(|i| file(format!("arquivo_{i:05}.dat"), next() % 50_000_000, next() % 2_000)).collect(),
        );
        MockDrive { folders }
    }

    pub fn list(&self, segs: &[String]) -> Vec<Entry> {
        self.folders.get(&segs.join("/")).cloned().unwrap_or_default()
    }

    /// Adds an entry to a folder unless one of that name is there. Returns whether it did.
    pub fn add(&mut self, segs: &[String], entry: Entry) -> bool {
        let key = segs.join("/");
        if entry.is_dir {
            let child = if key.is_empty() { entry.name.clone() } else { format!("{key}/{}", entry.name) };
            self.folders.entry(child).or_default();
        }
        let list = self.folders.entry(key).or_default();
        if list.iter().any(|e| e.name == entry.name) {
            return false;
        }
        list.push(entry);
        true
    }
}

pub fn format_size(b: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if b < 1024 {
        return format!("{b} B");
    }
    let mut v = b as f64 / 1024.0;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    openexplorer_i18n::decimal(format!("{v:.1} {}", UNITS[u]))
}

/// A date in full, as Windows' Properties shows it: "sexta-feira, 10 de fevereiro de 2023 20:19:35".
pub fn format_long(t: SystemTime) -> String {
    use chrono::{Datelike, Timelike};
    const DAYS: [&str; 7] = ["segunda-feira", "terça-feira", "quarta-feira", "quinta-feira", "sexta-feira", "sábado", "domingo"];
    const MONTHS: [&str; 12] = ["janeiro", "fevereiro", "março", "abril", "maio", "junho", "julho", "agosto", "setembro", "outubro", "novembro", "dezembro"];
    const DAYS_EN: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
    const MONTHS_EN: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    let d = chrono::DateTime::<chrono::Local>::from(t);
    let (day, month) = (d.weekday().num_days_from_monday() as usize, d.month0() as usize);
    tr!(
        "{0}, {1} de {2} de {3} {4:02}:{5:02}:{6:02}",
        "{0}, {1} {2} {3} {4:02}:{5:02}:{6:02}",
        openexplorer_i18n::t(DAYS[day], DAYS_EN[day]),
        d.day(),
        openexplorer_i18n::t(MONTHS[month], MONTHS_EN[month]),
        d.year(),
        d.hour(),
        d.minute(),
        d.second()
    )
}

/// A size as "1,45 GB (1.553.451.055 bytes)", or just "512 bytes".
pub fn format_bytes(b: u64) -> String {
    let grouped = openexplorer_i18n::grouped(b);
    if b < 1024 {
        return format!("{grouped} bytes");
    }
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    let (mut v, mut u) = (b as f64 / 1024.0, 0);
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    // Two decimals here, where the exact count follows; the list's one decimal is enough there.
    format!("{} ({grouped} bytes)", openexplorer_i18n::decimal(format!("{v:.2} {}", UNITS[u])))
}

pub fn format_time(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(t).format("%d/%m/%Y %H:%M").to_string()
}

/// `Option<SystemTime>` as signed milliseconds since 1970 for the disk cache: serde's own
/// `SystemTime` format can't hold a time before 1970, and Drive has files dated far earlier.
mod unix_ms {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &Option<SystemTime>, s: S) -> Result<S::Ok, S::Error> {
        let ms = t.map(|t| match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_millis() as i64,
            Err(e) => -(e.duration().as_millis() as i64),
        });
        match ms {
            Some(ms) => s.serialize_some(&ms),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<SystemTime>, D::Error> {
        Ok(Option::<i64>::deserialize(d)?.and_then(|ms| {
            let span = Duration::from_millis(ms.unsigned_abs());
            if ms >= 0 { UNIX_EPOCH.checked_add(span) } else { UNIX_EPOCH.checked_sub(span) }
        }))
    }
}

/// What a `/` in a Drive name shows as: U+FF0F, the full-width solidus. It looks like one but
/// separates nothing, in an address or on the computer (where a name can't hold `/`), as in
/// Air Explorer. Drive keeps the `/`.
pub const NAME_SLASH: char = '／';

/// A name as the app shows it, in lists, paths, dialogs and names given on the computer: each
/// `/` as [`NAME_SLASH`] (only Drive names have one).
pub fn shown(name: &str) -> String {
    name.replace('/', &NAME_SLASH.to_string())
}

/// A name typed over a shown one, as Drive gets it: each [`NAME_SLASH`] back to `/`.
pub fn typed(name: &str) -> String {
    name.replace(NAME_SLASH, "/")
}

/// One step of a location's path, and where clicking it goes.
pub struct Crumb {
    pub label: String,
    pub to: Location,
}

/// The clickable path of a location: `C:\ › Users › Ana`, `ana@gmail.com › Meu Drive › Fotos`.
pub fn crumbs(loc: &Location) -> Vec<Crumb> {
    match loc {
        Location::Local(dir) if let Some((server, share)) = crate::network::unc(dir) => {
            use crate::network::{is_wsl, server_path, share_path};
            let label = if is_wsl(&server) { "Linux".to_string() } else { format!(r"\\{server}") };
            let mut out = vec![Crumb { label, to: Location::Local(server_path(&server)) }];
            if share.is_empty() {
                return out;
            }
            let mut at = share_path(&server, &share);
            out.push(Crumb { label: share, to: Location::Local(at.clone()) });
            for c in dir.strip_prefix(&at).map(|r| r.components().collect::<Vec<_>>()).unwrap_or_default() {
                at.push(c);
                out.push(Crumb { label: c.as_os_str().to_string_lossy().into_owned(), to: Location::Local(at.clone()) });
            }
            out
        }
        Location::Local(dir) => {
            let mut at = PathBuf::new();
            dir.components()
                .filter_map(|c| {
                    match c {
                        std::path::Component::Prefix(_) => {
                            at.push(c.as_os_str());
                            at.push(std::path::MAIN_SEPARATOR_STR);
                            return Some(Crumb { label: at.display().to_string(), to: Location::Local(at.clone()) });
                        }
                        std::path::Component::RootDir => {
                            if at.as_os_str().is_empty() {
                                at.push("/");
                                return Some(Crumb { label: "/".into(), to: Location::Local(at.clone()) });
                            }
                            return None;
                        }
                        _ => at.push(c.as_os_str()),
                    }
                    Some(Crumb { label: c.as_os_str().to_string_lossy().into_owned(), to: Location::Local(at.clone()) })
                })
                .collect()
        }
        // The folder searched, then the search.
        Location::Found(f) => {
            let mut out = f.within.as_ref().map(|w| crumbs(&Location::Local(w.clone()))).unwrap_or_default();
            out.push(Crumb { label: tr!("“{}” em {}", "“{}” in {}", f.text, f.place), to: loc.clone() });
            out
        }
        Location::Mock(segs) => std::iter::once(Crumb { label: t("Drive simulado", "Simulated drive").into(), to: Location::Mock(Vec::new()) })
            .chain((0..segs.len()).map(|i| Crumb { label: segs[i].clone(), to: Location::Mock(segs[..=i].to_vec()) }))
            .collect(),
        Location::Drive { email, space, path } => {
            let mut out = vec![Crumb { label: email.clone(), to: Location::drive(email.clone(), Space::MyDrive) }];
            if let Space::Shared { .. } = space {
                out.push(Crumb { label: t("Drives compartilhados", "Shared drives").into(), to: Location::drive(email.clone(), Space::SharedDrives) });
            }
            let label = match space {
                Space::Search(q) => tr!("“{}” em {}", "“{}” in {}", q.text, q.place),
                _ => shown(space.label()),
            };
            out.push(Crumb { label, to: Location::drive(email.clone(), space.clone()) });
            for i in 0..path.len() {
                out.push(Crumb { label: shown(&path[i].1), to: Location::Drive { email: email.clone(), space: space.clone(), path: path[..=i].to_vec() } });
            }
            out
        }
        Location::Server { server, path, find } => {
            let at = |path: &[String]| Location::Server { server: server.clone(), path: path.to_vec(), find: None };
            let mut out = vec![Crumb { label: server.name.clone(), to: at(&[]) }];
            for i in 0..path.len() {
                out.push(Crumb { label: path[i].clone(), to: at(&path[..=i]) });
            }
            if let Some(text) = find {
                let place = path.last().unwrap_or(&server.name);
                out.push(Crumb { label: tr!("“{text}” em {place}", "“{text}” in {place}"), to: loc.clone() });
            }
            out
        }
    }
}

/// What kind of drive a letter is, as Windows reports it without touching the media.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriveKind {
    Fixed,
    Removable,
    Network,
    Optical,
    Other,
}

impl DriveKind {
    pub fn label(self, letter: &str) -> String {
        match self {
            DriveKind::Fixed => tr!("Disco local ({letter})", "Local Disk ({letter})"),
            DriveKind::Removable => tr!("Unidade removível ({letter})", "Removable Disk ({letter})"),
            DriveKind::Network => tr!("Unidade de rede ({letter})", "Network Drive ({letter})"),
            DriveKind::Optical => tr!("Unidade de DVD ({letter})", "DVD Drive ({letter})"),
            DriveKind::Other => tr!("Unidade ({letter})", "Drive ({letter})"),
        }
    }
}

/// The kind of drive `dir` is on. Answers from Windows' own records, so it doesn't wait on a
/// slow network share or an empty DVD drive.
pub fn drive_kind(dir: &Path) -> DriveKind {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let Some(std::path::Component::Prefix(prefix)) = dir.components().next() else { return DriveKind::Other };
        let mut root = prefix.as_os_str().to_os_string();
        root.push("\\");
        let wide: Vec<u16> = root.encode_wide().chain([0]).collect();
        // SAFETY: a NUL-terminated path.
        // The DRIVE_* values of GetDriveTypeW: 2 removable, 3 fixed, 4 remote, 5 CD-ROM, 6 RAM disk.
        match unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(wide.as_ptr()) } {
            2 => DriveKind::Removable,
            3 | 6 => DriveKind::Fixed,
            4 => DriveKind::Network,
            5 => DriveKind::Optical,
            _ => DriveKind::Other,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        DriveKind::Fixed
    }
}

/// The disks Windows shows in "Este computador", by letter, with their kind.
pub fn local_drives() -> Vec<(PathBuf, DriveKind)> {
    #[cfg(windows)]
    {
        // A bit per letter that has a drive, without touching the drives themselves (an empty
        // DVD drive would make `exists` wait).
        let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
        (0..26u8)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| PathBuf::from(format!("{}:\\", (b'A' + i) as char)))
            .map(|d| {
                let kind = drive_kind(&d);
                (d, kind)
            })
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec![(PathBuf::from("/"), DriveKind::Fixed)]
    }
}

/// Free bytes on the disk holding `dir`. Only asked of fixed disks: on a network share the
/// call can block the window for as long as the server takes to answer.
pub fn free_space(dir: &Path) -> Option<u64> {
    if drive_kind(dir) != DriveKind::Fixed {
        return None;
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
        let mut free = 0u64;
        // SAFETY: a NUL-terminated path and a valid out pointer; the other outs may be null.
        let ok = unsafe { windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
        (ok != 0).then_some(free)
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> Entry {
        Entry { id: name.into(), name: name.into(), is_dir: false, size: None, modified: None, target: None, mime: None, md5: None, starred: false, drive_origin: None }
    }

    /// Pages merged in one by one end in the order a full sort gives, for every column and
    /// direction, folders first.
    #[test]
    fn pages_merge_into_the_sorted_order() {
        let mut seed = 7u64;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            seed >> 33
        };
        let pages: Vec<Vec<Entry>> = (0..6)
            .map(|_| {
                (0..50)
                    .map(|_| {
                        let n = next();
                        let ext = ["txt", "pdf", "mkv"][(n % 3) as usize];
                        Entry { is_dir: n % 7 == 0, size: Some(n % 5), modified: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(n % 11)), ..file(&format!("item {} {}.{ext}", n % 40, n)) }
                    })
                    .collect()
            })
            .collect();
        for col in [SortCol::Name, SortCol::Size, SortCol::Modified, SortCol::Kind] {
            for asc in [true, false] {
                let mut merged = Pane::new(Location::Local("C:\\".into()));
                merged.sort = (col, asc);
                for page in &pages {
                    merged.append_entries(page.clone());
                }
                let mut whole = Pane::new(Location::Local("C:\\".into()));
                whole.sort = (col, asc);
                whole.entries = pages.concat();
                whole.sort_entries();
                let names = |p: &Pane| p.entries.iter().map(|e| e.id.clone()).collect::<Vec<_>>();
                assert_eq!(names(&merged), names(&whole), "{col:?} {asc}");
            }
        }
    }

    /// The UI thread's share of a big search: 110 pages of a thousand results, in no order,
    /// each added to the pane as it lands.
    /// `cargo test -p openexplorer --release absorbing_a_big_search -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn absorbing_a_big_search() {
        let mut pane = Pane::new(Location::Local("C:\\".into()));
        let mut seed = 12345u64;
        let mut name = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            format!("Episódio {} - Anime {}.mkv", seed >> 54, (seed >> 20) % 100_000)
        };
        let t = std::time::Instant::now();
        let mut worst = std::time::Duration::ZERO;
        for _ in 0..110 {
            let page: Vec<Entry> = (0..1000).map(|_| file(&name())).collect();
            let p = std::time::Instant::now();
            pane.append_entries(page);
            worst = worst.max(p.elapsed());
        }
        println!("{} itens em {:.0} ms, pior página {:.1} ms", pane.entries.len(), t.elapsed().as_secs_f64() * 1000.0, worst.as_secs_f64() * 1000.0);
    }

    #[test]
    fn kinds_come_from_the_extension_or_google_mime() {
        assert_eq!(file("photo_2025.JPG").kind(), Kind::Image);
        assert_eq!(file("photo_2025.JPG").kind_label(), "Arquivo JPG");
        assert_eq!(file("Life.Is.Beautiful.mkv").kind(), Kind::Video);
        assert_eq!(file("Assinantes Plex").kind_label(), "Arquivo");
        assert_eq!(file(".bashrc").kind_label(), "Arquivo");
        let doc = Entry { mime: Some("application/vnd.google-apps.spreadsheet".into()), ..file("Planilha sem título") };
        assert_eq!((doc.kind(), doc.kind_label().as_str()), (Kind::Sheet, "Planilha Google"));
        let dir = Entry { is_dir: true, ..file("Fotos") };
        assert_eq!(dir.kind_label(), "Pasta de arquivos");
    }

    #[test]
    fn sorting_by_kind_keeps_folders_first_then_groups_types() {
        let mut p = Pane::new(Location::Mock(Vec::new()));
        p.set_entries(Ok(["b.png", "a.pdf", "c.png", "z.pdf"].map(file).into_iter().chain([Entry { is_dir: true, ..file("Fotos") }]).collect()));
        p.sort = (SortCol::Kind, true);
        p.sort_entries();
        let names: Vec<&str> = p.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Fotos", "a.pdf", "z.pdf", "b.png", "c.png"]);
        p.sort = (SortCol::Kind, false);
        p.sort_entries();
        let names: Vec<&str> = p.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Fotos", "c.png", "b.png", "z.pdf", "a.pdf"]);
    }

    #[test]
    fn long_dates_and_byte_counts_read_as_in_windows() {
        use chrono::TimeZone;
        let t: SystemTime = chrono::Local.with_ymd_and_hms(2023, 2, 10, 20, 19, 35).unwrap().into();
        assert_eq!(format_long(t), "sexta-feira, 10 de fevereiro de 2023 20:19:35");
        assert_eq!(format_bytes(1_553_451_055), "1,45 GB (1.553.451.055 bytes)");
        assert_eq!(format_bytes(512), "512 bytes");
    }

    #[test]
    fn crumbs_lead_back_to_each_step() {
        let c = crumbs(&Location::Drive {
            email: "a@b".into(),
            space: Space::Shared { id: "0A".into(), name: "Equipe".into() },
            path: vec![("f1".into(), "Fotos".into()), ("f2".into(), "2026".into())],
        });
        let labels: Vec<&str> = c.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["a@b", "Drives compartilhados", "Equipe", "Fotos", "2026"]);
        assert!(matches!(&c[3].to, Location::Drive { path, .. } if path.len() == 1));
        // A folder opened from Com estrela leads back to the list it came from.
        let c = crumbs(&Location::Drive { email: "a@b".into(), space: Space::Starred, path: vec![("f1".into(), "Fotos".into())] });
        let labels: Vec<&str> = c.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["a@b", "Com estrela", "Fotos"]);
        assert_eq!(c[1].to, Location::drive("a@b", Space::Starred));
        let p = Pane::new(Location::Drive { email: "a@b".into(), space: Space::Trash, path: vec![("f1".into(), "Velha".into())] });
        assert_eq!(p.title(), "Lixeira/Velha");
        #[cfg(windows)]
        {
            let c = crumbs(&Location::Local(PathBuf::from(r"C:\Users\Ana")));
            let labels: Vec<&str> = c.iter().map(|c| c.label.as_str()).collect();
            assert_eq!(labels, [r"C:\", "Users", "Ana"]);
            assert_eq!(c[1].to, Location::Local(PathBuf::from(r"C:\Users")));
        }
    }

    #[test]
    fn hidden_entries_follow_show_hidden() {
        let dir = std::env::temp_dir().join(format!("oe-hidden-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".segredo")).unwrap();
        std::fs::write(dir.join("visivel.txt"), "oi").unwrap();
        std::fs::write(dir.join(".oculto"), "x").unwrap();
        let names = |show: bool| -> Vec<String> {
            let mut v: Vec<String> = read_local(&dir, show).unwrap().into_iter().map(|e| e.name).collect();
            v.sort();
            v
        };
        assert_eq!(names(false), ["visivel.txt"]);
        assert_eq!(names(true), [".oculto", ".segredo", "visivel.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn view_mode_cycles_through_all_five_modes() {
        let mut m = ViewMode::default();
        assert_eq!(m, ViewMode::Details);
        let mut seen = Vec::new();
        for _ in 0..5 {
            m = m.next();
            seen.push(m);
        }
        assert_eq!(seen, ViewMode::ALL);
        assert!(ViewMode::ExtraLargeIcons.is_grid());
        assert!(ViewMode::LargeIcons.is_grid());
        assert!(ViewMode::MediumIcons.is_grid());
        assert!(!ViewMode::List.is_grid());
        assert!(!ViewMode::Details.is_grid());
    }
}
