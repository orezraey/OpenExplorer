//! The service's index: one [`Tree`] per volume, each built and kept current on a thread of its
//! own, and the answer to a search over all of them.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use crate::proto::{Answer, Header, Request, Volume};
use crate::tree::{Query, Tree};

/// One volume's files and how far along they are.
pub struct Vol {
    pub tree: RwLock<Tree>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    indexing: bool,
    error: Option<String>,
    took_ms: Option<u64>,
    /// The folder last searched under, and its record: typing in a folder's search field asks
    /// for the same folder again and again.
    within: Option<(String, Option<u32>)>,
}

impl Vol {
    pub fn new(root: &str) -> Arc<Vol> {
        Arc::new(Vol { tree: RwLock::new(Tree::new(root)), state: Mutex::new(State { indexing: true, ..State::default() }) })
    }

    /// The volume's first read is done, in `took_ms`; `tree` replaces what was held.
    pub fn ready(&self, tree: Tree, took_ms: u64) {
        *self.tree.write().unwrap() = tree;
        let mut s = self.state.lock().unwrap();
        *s = State { took_ms: Some(took_ms), ..State::default() };
    }

    /// A tree saved by an earlier run, searchable while the volume is read again.
    pub fn loaded(&self, tree: Tree) {
        *self.tree.write().unwrap() = tree;
        self.state.lock().unwrap().indexing = true;
    }

    /// Something to say about the volume while it's indexed (an inotify cap reached), or
    /// nothing anymore.
    pub fn note(&self, note: Option<String>) {
        self.state.lock().unwrap().error = note;
    }

    /// Reading the volume failed; what was held, if anything, stays searchable.
    pub fn failed(&self, error: String) {
        let mut s = self.state.lock().unwrap();
        s.indexing = false;
        s.error = Some(error);
    }

    /// Being read again (its journal was lost): results stay, maybe stale, until it's done.
    pub fn reindexing(&self) {
        self.state.lock().unwrap().indexing = true;
    }

    fn describe(&self) -> Volume {
        let s = self.state.lock().unwrap();
        let t = self.tree.read().unwrap();
        Volume { root: t.root().to_string(), files: t.len(), indexing: s.indexing, error: s.error.clone(), took_ms: s.took_ms }
    }

    /// The record of the folder `path` in this volume. The last one is kept, and checked
    /// (its path built again, a few steps) rather than looked for, unless it moved or went.
    fn folder(&self, tree: &Tree, path: &str) -> Option<u32> {
        let trim = |p: &str| p.trim_end_matches(['\\', '/']).to_lowercase();
        if let Some((p, Some(r))) = &self.state.lock().unwrap().within
            && p == path
            && tree.path(*r).is_some_and(|now| trim(&now) == trim(path))
        {
            return Some(*r);
        }
        let r = tree.find(path);
        self.state.lock().unwrap().within = Some((path.to_string(), r));
        r
    }
}

/// The most rows one answer sends, whatever the request asks.
pub const MAX_ROWS: usize = 200_000;

pub struct Index {
    pub vols: Vec<Arc<Vol>>,
    threads: usize,
}

impl Index {
    pub fn new(vols: Vec<Arc<Vol>>) -> Index {
        Index { vols, threads: std::thread::available_parallelism().map_or(4, |n| n.get()) }
    }

    pub fn answer(&self, req: &Request) -> Answer {
        let t = Instant::now();
        let volumes = self.vols.iter().map(|v| v.describe()).collect();
        let Some(query) = Query::new(&req.text) else { return Answer { header: Header { volumes, ..Header::default() }, rows: Vec::new() } };
        let mut rows = Vec::new();
        let mut total = 0;
        let mut searched = false;
        for v in &self.vols {
            let tree = v.tree.read().unwrap();
            let within = match &req.within {
                None => None,
                Some(w) if w.len() >= tree.root().len() && w[..tree.root().len()].eq_ignore_ascii_case(tree.root()) => match v.folder(&tree, w) {
                    Some(r) => Some(r),
                    None => continue,
                },
                Some(_) => continue,
            };
            searched = true;
            let mut found = Vec::new();
            total += tree.search(&query, within, req.skip_hidden, req.limit.min(MAX_ROWS).saturating_sub(rows.len()), self.threads, &mut found);
            rows.extend(found.into_iter().filter_map(|r| tree.row(r)));
        }
        let took_us = t.elapsed().as_micros() as u64;
        // A folder on no indexed volume (a pen drive, a network drive, FAT) isn't "nothing found".
        let error = match &req.within {
            Some(w) if !searched && !volumes.iter().any(|v: &Volume| w.get(..v.root.len()).is_some_and(|d| d.eq_ignore_ascii_case(&v.root))) => {
                Some("Este disco não está no índice: a pesquisa lê só discos NTFS fixos (não pen drives nem unidades de rede).".into())
            }
            _ => None,
        };
        Answer { header: Header { total, rows: rows.len(), volumes, error, took_us }, rows }
    }
}

/// Builds and follows NTFS volume `letter` into `vol`: its journal is opened, the MFT read, and
/// then the journal read every half second. When the journal lost changes (it wrapped, or was
/// recreated) the volume is read again.
#[cfg(windows)]
pub fn follow(vol: Arc<Vol>, letter: char) {
    use crate::ntfs;
    loop {
        let t = Instant::now();
        let built = ntfs::journal(letter).and_then(|j| ntfs::scan(letter).map(|tree| (j, tree)));
        let (mut journal, tree) = match built {
            Ok(b) => b,
            Err(e) => {
                vol.failed(e);
                // A volume that can't be read now may later (a disk being unlocked).
                std::thread::sleep(std::time::Duration::from_secs(60));
                continue;
            }
        };
        vol.ready(tree, t.elapsed().as_millis() as u64);
        loop {
            match journal.read() {
                Ok(page) => {
                    let changes = ntfs::changes(&page.records);
                    if !changes.is_empty() {
                        ntfs::apply(&vol.tree, changes);
                    }
                    if page.caught_up {
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                }
                Err(_) => {
                    vol.reindexing();
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{DIR, ROOT};

    fn vol(root: &str, files: &[(u32, u32, &str, u8)]) -> Arc<Vol> {
        let v = Vol::new(root);
        let mut t = Tree::new(root);
        for (r, p, n, f) in files {
            t.set(*r, *p, n, *f, 10, 20);
        }
        v.ready(t, 1);
        v
    }

    #[test]
    fn a_search_runs_over_every_volume_or_under_one_folder() {
        let c = vol("C:\\", &[(30, ROOT, "Projetos", DIR), (31, 30, "plano.txt", 0), (32, ROOT, "plano velho.txt", 0)]);
        let d = vol("D:\\", &[(30, ROOT, "Backup", DIR), (31, 30, "plano.txt", 0)]);
        let index = Index::new(vec![c, d]);
        let ask = |text: &str, within: Option<&str>, limit| index.answer(&Request { text: text.into(), within: within.map(Into::into), limit, ..Request::default() });
        let a = ask("PLANO", None, 100);
        assert_eq!((a.header.total, a.rows.len()), (3, 3));
        assert_eq!(a.header.volumes.iter().map(|v| (v.root.as_str(), v.files, v.indexing)).collect::<Vec<_>>(), [("C:\\", 3, false), ("D:\\", 2, false)]);
        let a = ask("plano", Some("c:\\projetos"), 100);
        assert_eq!(a.rows.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(), ["C:\\Projetos\\plano.txt"]);
        // Asked again (the cached folder), and in a folder that isn't there.
        assert_eq!(ask("plano", Some("c:\\projetos"), 100).rows.len(), 1);
        assert_eq!(ask("plano", Some("C:\\Nada"), 100).header.total, 0);
        assert_eq!(ask("plano", Some("C:\\Nada"), 100).header.error, None);
        assert!(ask("plano", Some("E:\\Fotos"), 100).header.error.is_some(), "a disk the index doesn't hold");
        // The limit caps the rows across volumes, not the count.
        let a = ask("plano", None, 2);
        assert_eq!((a.header.total, a.rows.len()), (3, 2));
        assert_eq!(ask("plano", None, usize::MAX).rows.len(), 3);
        assert_eq!(ask("   ", None, 10).header.total, 0);
    }
}
