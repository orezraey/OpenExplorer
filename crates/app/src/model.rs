//! What the panes show: a folder listing on the local disk, in a simulated drive kept in memory,
//! or in a connected Google Drive account, and the selection and sort of each pane.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[derive(Clone, Debug)]
pub struct Entry {
    /// Unique in its folder: the name on disk and in the simulated drive, the file id in Google
    /// Drive (where two files can share a name).
    pub id: String,
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    /// For a Drive shortcut to a folder: the folder it opens, instead of `id`.
    pub target: Option<String>,
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
    /// Numbers each listing request, so an answer for a folder already left is dropped.
    pub request: u64,
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
            request: 0,
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
        self.selected.retain(|id| self.entries.iter().any(|e| &e.id == id));
        self.sort_entries();
    }

    pub fn sort_entries(&mut self) {
        let (col, asc) = self.sort;
        self.entries.sort_by(|a, b| {
            // Folders always come first, as in Explorer.
            b.is_dir.cmp(&a.is_dir).then_with(|| {
                let o = match col {
                    SortCol::Name => rune_kit::natural_cmp(&a.name, &b.name),
                    SortCol::Size => a.size.cmp(&b.size),
                    SortCol::Modified => a.modified.cmp(&b.modified),
                };
                let o = if o == Ordering::Equal { rune_kit::natural_cmp(&a.name, &b.name) } else { o };
                if asc { o } else { o.reverse() }
            })
        });
        self.version += 1;
    }

    pub fn title(&self) -> String {
        match &self.loc {
            Location::Local(p) => p.display().to_string(),
            Location::Mock(segs) => format!("Meu Drive/{}", segs.join("/")),
            Location::Drive { space, path, .. } => {
                let mut parts: Vec<&str> = match space {
                    Space::MyDrive => vec!["Meu Drive"],
                    Space::SharedDrives => vec!["Drives compartilhados"],
                    Space::Shared { name, .. } => vec!["Drives compartilhados", name],
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
        let dir = |name: &str, days| Entry { id: name.into(), name: name.into(), is_dir: true, size: None, modified: Some(ago(days)), target: None };
        let file = |name: String, size: u64, days| Entry { id: name.clone(), name, is_dir: false, size: Some(size), modified: Some(ago(days)), target: None };

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

pub fn format_time(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(t).format("%d/%m/%Y %H:%M").to_string()
}
