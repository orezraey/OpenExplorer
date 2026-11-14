//! What the panes show: a folder listing on the local disk, in a simulated drive kept in memory,
//! or in a connected Google Drive account, and the selection and sort of each pane.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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
            return if self.target.is_some() { "Atalho de pasta".into() } else { "Pasta de arquivos".into() };
        }
        let google = match self.mime.as_deref() {
            Some("application/vnd.google-apps.document") => Some("Documento Google"),
            Some("application/vnd.google-apps.spreadsheet") => Some("Planilha Google"),
            Some("application/vnd.google-apps.presentation") => Some("Apresentação Google"),
            Some("application/vnd.google-apps.form") => Some("Formulário Google"),
            Some("application/vnd.google-apps.drawing") => Some("Desenho Google"),
            Some("application/vnd.google-apps.shortcut") => Some("Atalho"),
            Some(m) if m.starts_with("application/vnd.google-apps.") => Some("Arquivo Google"),
            _ => None,
        };
        if let Some(g) = google {
            return g.into();
        }
        match self.ext() {
            Some(ext) => format!("Arquivo {}", ext.to_ascii_uppercase()),
            None => "Arquivo".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Location {
    Local(PathBuf),
    /// A folder in the simulated drive, as its path segments from the root.
    Mock(Vec<String>),
    /// A folder in a connected account: the (id, name) of each folder from the top of `space`
    /// down.
    Drive { email: String, space: Space, path: Vec<(String, String)> },
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
            Space::MyDrive | Space::SharedDrives | Space::Shared { .. } => None,
        }
    }

    /// Its name in the tree and the path bar.
    pub fn label(&self) -> &str {
        match self {
            Space::MyDrive => "Meu Drive",
            Space::SharedDrives => "Drives compartilhados",
            Space::Shared { name, .. } => name,
            Space::SharedWithMe => "Compartilhados comigo",
            Space::Starred => "Com estrela",
            Space::Recent => "Recentes",
            Space::Trash => "Lixeira",
        }
    }
}

impl Location {
    pub fn drive(email: impl Into<String>, space: Space) -> Location {
        Location::Drive { email: email.into(), space, path: Vec::new() }
    }

    pub fn home() -> Location {
        Location::Local(std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\")))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortCol {
    Name,
    Size,
    Modified,
    Kind,
}

pub struct Pane {
    pub loc: Location,
    pub entries: Vec<Entry>,
    /// Entry ids.
    pub selected: HashSet<String>,
    pub anchor: Option<String>,
    pub cursor: Option<String>,
    pub sort: (SortCol, bool),
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
            version: 0,
            drop_over: false,
            error: None,
            loading: false,
            back: Vec::new(),
            forward: Vec::new(),
            free: None,
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
        self.entries.clear();
        self.version += 1;
    }

    /// Lists a local or simulated folder in place. Drive folders load in the background instead.
    pub fn load_now(&mut self, mock: &MockDrive) {
        self.free = match &self.loc {
            Location::Local(dir) => free_space(dir),
            _ => None,
        };
        let entries = match &self.loc {
            Location::Local(dir) => read_local(dir).map_err(|e| format!("{}: {e}", dir.display())),
            Location::Mock(segs) => Ok(mock.list(segs)),
            Location::Drive { .. } => return,
        };
        self.set_entries(entries);
    }

    pub fn set_entries(&mut self, entries: Result<Vec<Entry>, String>) {
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
        self.prune_selection();
        self.sort_entries();
    }

    /// Forgets selected ids the listing doesn't have (anymore).
    pub fn prune_selection(&mut self) {
        let ids: HashSet<&str> = self.entries.iter().map(|e| e.id.as_str()).collect();
        self.selected.retain(|id| ids.contains(id.as_str()));
    }

    /// Adds a later page of a listing that's still arriving.
    pub fn append_entries(&mut self, more: Vec<Entry>) {
        self.entries.extend(more);
        self.sort_entries();
    }

    pub fn sort_entries(&mut self) {
        let (col, asc) = self.sort;
        // Folders always come first, as in Explorer; ties fall back to the name.
        let order = |a: &Entry, b: &Entry, by: Ordering| {
            b.is_dir.cmp(&a.is_dir).then_with(|| {
                let o = if by == Ordering::Equal { rune_kit::natural_cmp(&a.name, &b.name) } else { by };
                if asc { o } else { o.reverse() }
            })
        };
        if col == SortCol::Kind {
            // The label is built once per entry rather than twice per comparison.
            let mut keyed: Vec<(String, Entry)> = std::mem::take(&mut self.entries).into_iter().map(|e| (e.kind_label(), e)).collect();
            keyed.sort_by(|(ka, a), (kb, b)| order(a, b, ka.cmp(kb)));
            self.entries = keyed.into_iter().map(|(_, e)| e).collect();
        } else {
            self.entries.sort_by(|a, b| {
                let by = match col {
                    SortCol::Name => Ordering::Equal,
                    SortCol::Size => a.size.cmp(&b.size),
                    SortCol::Modified => a.modified.cmp(&b.modified),
                    SortCol::Kind => unreachable!(),
                };
                order(a, b, by)
            });
        }
        self.version += 1;
    }

    pub fn title(&self) -> String {
        match &self.loc {
            Location::Local(p) => p.display().to_string(),
            Location::Mock(segs) => format!("Meu Drive/{}", segs.join("/")),
            Location::Drive { space, path, .. } => {
                let mut parts: Vec<&str> = match space {
                    Space::Shared { name, .. } => vec!["Drives compartilhados", name],
                    other => vec![other.label()],
                };
                parts.extend(path.iter().map(|(_, n)| n.as_str()));
                parts.join("/")
            }
        }
    }
}

fn read_local(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for item in std::fs::read_dir(dir)? {
        let Ok(item) = item else { continue };
        let meta = item.metadata().ok();
        let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
        let name = item.file_name().to_string_lossy().into_owned();
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
        });
    }
    Ok(out)
}

/// A Google Drive stand-in held in memory, to try the panes without an account.
pub struct MockDrive {
    folders: HashMap<String, Vec<Entry>>,
}

impl MockDrive {
    pub fn new() -> MockDrive {
        let now = SystemTime::now();
        let ago = |days: u64| now - Duration::from_secs(days * 86_400);
        let dir = |name: &str, days| Entry { id: name.into(), name: name.into(), is_dir: true, size: None, modified: Some(ago(days)), target: None, mime: None, md5: None, starred: false };
        let file = |name: String, size: u64, days| Entry { id: name.clone(), name, is_dir: false, size: Some(size), modified: Some(ago(days)), target: None, mime: None, md5: None, starred: false };

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
    // pt-BR decimal comma.
    format!("{v:.1} {}", UNITS[u]).replace('.', ",")
}

/// A date in full, as Windows' Properties shows it: "sexta-feira, 10 de fevereiro de 2023 20:19:35".
pub fn format_long(t: SystemTime) -> String {
    use chrono::{Datelike, Timelike};
    const DAYS: [&str; 7] = ["segunda-feira", "terça-feira", "quarta-feira", "quinta-feira", "sexta-feira", "sábado", "domingo"];
    const MONTHS: [&str; 12] = ["janeiro", "fevereiro", "março", "abril", "maio", "junho", "julho", "agosto", "setembro", "outubro", "novembro", "dezembro"];
    let d = chrono::DateTime::<chrono::Local>::from(t);
    format!(
        "{}, {} de {} de {} {:02}:{:02}:{:02}",
        DAYS[d.weekday().num_days_from_monday() as usize],
        d.day(),
        MONTHS[d.month0() as usize],
        d.year(),
        d.hour(),
        d.minute(),
        d.second()
    )
}

/// A size as "1,45 GB (1.553.451.055 bytes)", or just "512 bytes".
pub fn format_bytes(b: u64) -> String {
    let digits = b.to_string();
    let grouped = digits.chars().enumerate().fold(String::new(), |mut out, (i, ch)| {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(ch);
        out
    });
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
    format!("{} ({grouped} bytes)", format!("{v:.2} {}", UNITS[u]).replace('.', ","))
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

/// One step of a location's path, and where clicking it goes.
pub struct Crumb {
    pub label: String,
    pub to: Location,
}

/// The clickable path of a location: `C:\ › Users › Ana`, `ana@gmail.com › Meu Drive › Fotos`.
pub fn crumbs(loc: &Location) -> Vec<Crumb> {
    match loc {
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
        Location::Mock(segs) => std::iter::once(Crumb { label: "Drive simulado".into(), to: Location::Mock(Vec::new()) })
            .chain((0..segs.len()).map(|i| Crumb { label: segs[i].clone(), to: Location::Mock(segs[..=i].to_vec()) }))
            .collect(),
        Location::Drive { email, space, path } => {
            let mut out = vec![Crumb { label: email.clone(), to: Location::drive(email.clone(), Space::MyDrive) }];
            if let Space::Shared { .. } = space {
                out.push(Crumb { label: "Drives compartilhados".into(), to: Location::drive(email.clone(), Space::SharedDrives) });
            }
            out.push(Crumb { label: space.label().to_string(), to: Location::drive(email.clone(), space.clone()) });
            for i in 0..path.len() {
                out.push(Crumb { label: path[i].1.clone(), to: Location::Drive { email: email.clone(), space: space.clone(), path: path[..=i].to_vec() } });
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
            DriveKind::Fixed => format!("Disco local ({letter})"),
            DriveKind::Removable => format!("Unidade removível ({letter})"),
            DriveKind::Network => format!("Unidade de rede ({letter})"),
            DriveKind::Optical => format!("Unidade de DVD ({letter})"),
            DriveKind::Other => format!("Unidade ({letter})"),
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
        Entry { id: name.into(), name: name.into(), is_dir: false, size: None, modified: None, target: None, mime: None, md5: None, starred: false }
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
}
