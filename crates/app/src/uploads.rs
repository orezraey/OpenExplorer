//! Uploads from the computer to Google Drive, as rows of the transfer queue.
//!
//! A job adds up each row's size (a quick local walk), then workers (Uploads simultâneos in
//! Opções, 4 unless changed) share one
//! list of work: making a Drive folder, which adds the folder's subfolders and files to the
//! list, and sending a file. Files start going up as soon as their folder exists, so a tree of
//! hundreds of folders shows progress from the first seconds instead of after all of them are
//! made. A row's bar counts the bytes of all its files, and its state counts items done.
//!
//! Drive lets two items share a name, so names are checked first, as Air Explorer does: a
//! folder that's already there is merged into (its id reused, not a second folder made), and
//! a file whose name is taken is replaced (sent as a new version of that file), skipped or
//! sent under a free name ("nome (2).ext"), as the user chose for the item it's in.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use openexplorer_gdrive::api::FOLDER_MIME;
use openexplorer_gdrive::{Drive, DriveFile};

use crate::model::Entry;
use crate::ops::{self, CANCELLED, Progress, Stage};
use crate::transfers::Batch;
use crate::work::{Queue, Slots};

/// What to do with a file whose name is already in the Drive folder it goes into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clash {
    /// Send it as a new version of the file there (Substituir).
    Replace,
    /// Leave it out (Pular).
    Skip,
    /// Send it under a free name, "nome (2).ext" (Manter os dois).
    KeepBoth,
}

/// What an item already in a Drive folder is, as far as a clash goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Folder,
    /// A file with content, which can take a new version.
    File,
    /// A Google Doc, Sheet, ... or a shortcut: neither merged into nor replaced.
    Other,
}

/// An item already in the Drive folder an upload goes into.
#[derive(Clone, Debug)]
pub struct Existing {
    pub name: String,
    pub id: String,
    pub kind: Kind,
}

fn kind_of(mime: &str, is_shortcut: bool) -> Kind {
    match mime {
        _ if is_shortcut => Kind::Other,
        FOLDER_MIME => Kind::Folder,
        m if m.starts_with("application/vnd.google-apps.") => Kind::Other,
        _ => Kind::File,
    }
}

impl Existing {
    pub fn of_entry(e: &Entry) -> Existing {
        let mime = e.mime.as_deref().unwrap_or(if e.is_dir { FOLDER_MIME } else { "" });
        Existing { name: e.name.clone(), id: e.id.clone(), kind: kind_of(mime, e.target.is_some()) }
    }

    fn of_file(f: &DriveFile) -> Existing {
        Existing { name: f.name.clone(), id: f.id.clone(), kind: kind_of(&f.mime, f.target.is_some()) }
    }
}

/// How one item goes into a Drive folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Put {
    /// As a new item with this name.
    New(String),
    /// As a new version of file `id`.
    Replace(String),
    /// A folder whose contents go into the folder `id` already there.
    Merge(String),
    /// Left out.
    Skip,
}

/// One item of an upload: where it is on disk, how it goes in, and what to do with clashes
/// inside it (a merged folder's files).
pub struct Item {
    pub path: PathBuf,
    pub put: Put,
    pub clash: Clash,
}

/// Which of `local` (names) are taken in `existing`, by position.
pub fn clashes(local: &[String], existing: &[Existing]) -> Vec<usize> {
    let names: HashSet<&str> = existing.iter().map(|e| e.name.as_str()).collect();
    local.iter().enumerate().filter(|(_, n)| names.contains(n.as_str())).map(|(i, _)| i).collect()
}

/// How each of `local` (name, is a folder) goes into a folder that already holds `existing`,
/// with `clash(i)` the choice for item `i` when its name is taken there. A folder meeting a
/// folder is always merged into it. Neither a file and a folder nor a Google Doc can be one
/// another's new version, so Substituir sends those under a free name, as Manter os dois does;
/// a free name is one neither Drive nor the other items being sent have.
pub fn resolve(local: &[(String, bool)], existing: &[Existing], clash: impl Fn(usize) -> Clash) -> Vec<Put> {
    let mut there: HashMap<&str, &Existing> = HashMap::new();
    for e in existing {
        // With duplicates already there, the first one listed is the one taken.
        there.entry(e.name.as_str()).or_insert(e);
    }
    let mut taken: HashSet<String> = existing.iter().map(|e| e.name.clone()).chain(local.iter().map(|(n, _)| n.clone())).collect();
    local
        .iter()
        .enumerate()
        .map(|(i, (name, is_dir))| {
            let Some(e) = there.get(name.as_str()) else { return Put::New(name.clone()) };
            match (is_dir, e.kind, clash(i)) {
                (true, Kind::Folder, _) => Put::Merge(e.id.clone()),
                (_, _, Clash::Skip) => Put::Skip,
                (false, Kind::File, Clash::Replace) => Put::Replace(e.id.clone()),
                _ => {
                    let free = numbered(name, *is_dir, &taken);
                    taken.insert(free.clone());
                    Put::New(free)
                }
            }
        })
        .collect()
}

/// `name` with the first number from 2 that makes it free, as Windows does: "nome (2).ext",
/// "Pasta (3)". A folder's name has no extension to keep at the end.
pub fn numbered(name: &str, is_dir: bool, taken: &HashSet<String>) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && !is_dir => name.split_at(i),
        _ => (name, ""),
    };
    (2..).map(|k| format!("{stem} ({k}){ext}")).find(|n| !taken.contains(n)).expect("some number is free")
}

/// One piece of work of a row.
enum Work {
    /// Put folder `dir` into Drive folder `parent`: make it as `name`, or with `into` use the
    /// folder already there; then queue what's in it, its clashes settled by `clash`.
    Folder { row: usize, dir: PathBuf, name: String, parent: String, into: Option<String>, clash: Clash },
    /// Send the file at `path` into Drive folder `parent` as `name`, or with `replace` as a new
    /// version of that file.
    File { row: usize, path: PathBuf, name: String, parent: String, replace: Option<String> },
}

impl Work {
    fn row(&self) -> usize {
        match self {
            Work::Folder { row, .. } | Work::File { row, .. } => *row,
        }
    }

    /// The work for the item at `path` going into `parent` as `put`; none when it's skipped.
    fn of(row: usize, path: PathBuf, put: Put, parent: &str, clash: Clash) -> Option<Work> {
        let parent = parent.to_string();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Some(match put {
            Put::Skip => return None,
            Put::Merge(id) => Work::Folder { row, dir: path, name, parent, into: Some(id), clash },
            Put::Replace(id) => Work::File { row, path, name, parent, replace: Some(id) },
            Put::New(name) if path.is_dir() => Work::Folder { row, dir: path, name, parent, into: None, clash },
            Put::New(name) => Work::File { row, path, name, parent, replace: None },
        })
    }
}

/// What a row still has going.
struct Row {
    /// Work queued or running.
    left: AtomicUsize,
    /// Some work was skipped (Cancelar, or after an error).
    skipped: AtomicBool,
    /// Something in it was left out because its name was taken (Pular): a move then keeps
    /// the original, which still holds it.
    kept: AtomicBool,
    error: Mutex<Option<String>>,
}

/// Uploads each of `items` (a row of `batch` each) into the Drive folder `parent`, with
/// `at_once` folders made and files sent at the same time. With `remove`, a row whose files all
/// arrived then goes to the Recycle Bin (Recortar and Colar). An item to skip ends its row
/// at once.
pub fn run(d: &Drive, batch: &Batch, items: &[Item], parent: &str, remove: bool, at_once: usize, slots: &Slots) -> Result<String, String> {
    let rows = batch.rows();
    for (p, item) in rows.iter().zip(items) {
        p.size(ops::measure(&item.path, &|| p.cancelled()));
    }
    let state: Vec<Row> = rows.iter().map(|_| Row { left: AtomicUsize::new(1), skipped: AtomicBool::new(false), kept: AtomicBool::new(false), error: Mutex::new(None) }).collect();
    let mut first = VecDeque::new();
    for (row, item) in items.iter().enumerate() {
        match Work::of(row, item.path.clone(), item.put.clone(), parent, item.clash) {
            Some(w) => {
                rows[row].add_items(1);
                first.push_back(w);
            }
            None => rows[row].end(Ok(())),
        }
    }
    let queue = Queue::new(first);
    std::thread::scope(|scope| {
        for _ in 0..at_once.max(1) {
            scope.spawn(|| {
                while let Some((work, _finished)) = queue.next() {
                    let row = work.row();
                    let (p, st) = (&rows[row], &state[row]);
                    let skip = || p.cancelled() || st.error.lock().map(|e| e.is_some()).unwrap_or(true);
                    let _slot = slots.acquire_unless(skip);
                    if skip() {
                        st.skipped.store(true, Ordering::Relaxed);
                    } else {
                        if p.stage() == Stage::Waiting {
                            p.begin();
                        }
                        let result = match &work {
                            Work::Folder { row, dir, name, parent, into, clash } => {
                                open_folder(d, *row, dir, name, parent, into.as_deref(), *clash, p, st).map(|more| {
                                    // The folder's contents join the row before this item
                                    // counts as done, so the row can't end in between.
                                    st.left.fetch_add(more.len(), Ordering::Relaxed);
                                    p.add_items(more.len() as u64);
                                    queue.add(more);
                                })
                            }
                            Work::File { path, name, parent, replace, .. } => send(d, path, name, parent, replace.as_deref(), p),
                        };
                        match result {
                            Ok(()) => {}
                            Err(e) if e == CANCELLED => st.skipped.store(true, Ordering::Relaxed),
                            Err(e) => {
                                if let Ok(mut first) = st.error.lock() {
                                    first.get_or_insert(e);
                                }
                            }
                        }
                    }
                    p.item_done();
                    // The row's last piece of work ends it: done when nothing was skipped,
                    // even if Cancelar came too late to stop anything.
                    if st.left.fetch_sub(1, Ordering::Relaxed) == 1 {
                        let error = st.error.lock().ok().and_then(|e| e.clone());
                        p.end(match error {
                            Some(e) => Err(e),
                            None if st.skipped.load(Ordering::Relaxed) => Err(CANCELLED.into()),
                            None => finish(&items[row].path, remove && !st.kept.load(Ordering::Relaxed)),
                        });
                    }
                }
            });
        }
    });
    batch.summary()
}

/// After a row's files all arrived: with `remove` (a move), the original goes to the
/// Recycle Bin.
fn finish(path: &Path, remove: bool) -> Result<(), String> {
    if remove { ops::to_recycle_bin(&[path.to_path_buf()]) } else { Ok(()) }
}

/// Every item directly in Drive folder `id`.
pub fn listing(d: &Drive, id: &str) -> Result<Vec<Existing>, String> {
    let mut out = Vec::new();
    d.list(id, None, |page, _| {
        out.extend(page.iter().map(Existing::of_file));
        true
    })?;
    Ok(out)
}

/// Puts folder `dir` into Drive folder `parent`: makes it as `name`, or with `into` lists the
/// folder already there; then lists what goes in it, its names settled against what that
/// folder holds by `clash`. What's skipped comes off the row's size and keeps a move's
/// original.
#[allow(clippy::too_many_arguments)]
fn open_folder(d: &Drive, row: usize, dir: &Path, name: &str, parent: &str, into: Option<&str>, clash: Clash, p: &Progress, st: &Row) -> Result<Vec<Work>, String> {
    let (id, existing) = match into {
        Some(id) => (id.to_string(), listing(d, id)?),
        None => (d.create_folder(name, parent)?.id, Vec::new()),
    };
    let items: Vec<(PathBuf, String, bool)> = std::fs::read_dir(dir)
        .map_err(|e| openexplorer_i18n::tr!("Não consegui ler \"{}\": {e}", "Couldn't read \"{}\": {e}", dir.display()))?
        .flatten()
        .map(|item| {
            let path = item.path();
            let is_dir = path.is_dir();
            (path, item.file_name().to_string_lossy().into_owned(), is_dir)
        })
        .collect();
    let local: Vec<(String, bool)> = items.iter().map(|(_, n, is_dir)| (n.clone(), *is_dir)).collect();
    let puts = resolve(&local, &existing, |_| clash);
    let mut work = Vec::new();
    for ((path, _, _), put) in items.into_iter().zip(puts) {
        if put == Put::Skip {
            p.unsize(ops::measure(&path, &|| p.cancelled()));
            st.kept.store(true, Ordering::Relaxed);
        }
        work.extend(Work::of(row, path, put, &id, clash));
    }
    Ok(work)
}

/// Sends one file, as a new file or a new version of `replace`, keeping its row's bar in
/// step: bytes resent after a dropped connection are taken back first.
fn send(d: &Drive, path: &Path, name: &str, parent: &str, replace: Option<&str>, p: &Progress) -> Result<(), String> {
    let mut reported = 0u64;
    let mut sent = |done: u64| {
        if done > reported {
            p.add_bytes(done - reported);
        } else {
            p.sub_bytes(reported - done);
        }
        reported = done;
        !p.cancelled()
    };
    let result = match replace {
        Some(id) => d.upload_version(id, path, &mut sent),
        None => d.upload(path, parent, name, &mut sent),
    };
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn there(name: &str, id: &str, kind: Kind) -> Existing {
        Existing { name: name.into(), id: id.into(), kind }
    }

    fn local(items: &[(&str, bool)]) -> Vec<(String, bool)> {
        items.iter().map(|(n, d)| (n.to_string(), *d)).collect()
    }

    #[test]
    fn taken_names_follow_the_choice_and_folders_merge() {
        let existing = [
            there("nota.txt", "f1", Kind::File),
            there("Fotos", "d1", Kind::Folder),
            there("Planilha", "g1", Kind::Other),
            there("nota (2).txt", "f2", Kind::File),
        ];
        let items = local(&[("nota.txt", false), ("Fotos", true), ("Planilha", false), ("nova.txt", false), ("nota (3).txt", false)]);
        let all = |c| resolve(&items, &existing, move |_| c);
        assert_eq!(
            all(Clash::Replace),
            [Put::Replace("f1".into()), Put::Merge("d1".into()), Put::New("Planilha (2)".into()), Put::New("nova.txt".into()), Put::New("nota (3).txt".into())]
        );
        assert_eq!(all(Clash::Skip), [Put::Skip, Put::Merge("d1".into()), Put::Skip, Put::New("nova.txt".into()), Put::New("nota (3).txt".into())]);
        // "nota (2).txt" is in Drive and "nota (3).txt" is being sent: the copy takes (4).
        assert_eq!(all(Clash::KeepBoth)[0], Put::New("nota (4).txt".into()));
        // One choice per item.
        let mixed = resolve(&items, &existing, |i| if i == 0 { Clash::Skip } else { Clash::KeepBoth });
        assert_eq!((&mixed[0], &mixed[2]), (&Put::Skip, &Put::New("Planilha (2)".into())));
        assert_eq!(clashes(&items.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(), &existing), [0, 1, 2]);
    }

    #[test]
    fn a_folder_meeting_a_file_is_not_merged() {
        let existing = [there("Relatório", "f1", Kind::File)];
        let put = |c| resolve(&local(&[("Relatório", true)]), &existing, move |_| c).remove(0);
        assert_eq!(put(Clash::Replace), Put::New("Relatório (2)".into()));
        assert_eq!(put(Clash::Skip), Put::Skip);
        let existing = [there("Relatório", "d1", Kind::Folder)];
        assert_eq!(resolve(&local(&[("Relatório", false)]), &existing, |_| Clash::Replace), [Put::New("Relatório (2)".into())]);
    }

    #[test]
    fn free_names_number_as_windows_does() {
        let taken: HashSet<String> = ["a.tar.gz", "a.tar (2).gz", "Pasta.v1", ".bashrc"].map(String::from).into();
        assert_eq!(numbered("a.tar.gz", false, &taken), "a.tar (3).gz");
        assert_eq!(numbered("Pasta.v1", true, &taken), "Pasta.v1 (2)");
        assert_eq!(numbered(".bashrc", false, &taken), ".bashrc (2)");
        assert_eq!(numbered("LEIAME", false, &taken), "LEIAME (2)");
    }

    /// Each choice against the real Google Drive of the first connected account, in a test
    /// folder made for it and moved to the trash at the end. Needs the network and a login:
    /// `cargo test -p openexplorer live_drive -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_drive_each_choice() {
        use crate::transfers::{End, Op, queue};
        let account = openexplorer_gdrive::store::accounts().into_iter().next().expect("no connected account");
        let d = crate::open_drive(&account.email).unwrap();
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let root = d.create_folder(&format!("OpenExplorer teste conflitos {stamp}"), "root").unwrap();
        println!("pasta de teste: {} ({})", root.name, root.id);
        // To the trash however the test ends.
        struct Trash<'a>(&'a Drive, String);
        impl Drop for Trash<'_> {
            fn drop(&mut self) {
                let ok = self.0.trash(&self.1).is_ok();
                println!("pasta de teste movida para a lixeira: {ok}");
            }
        }
        let _trash = Trash(&d, root.id.clone());
        let src = std::env::temp_dir().join(format!("openexplorer-live-{stamp}"));
        std::fs::create_dir_all(src.join("Pasta")).unwrap();
        // Content `v` of every file: a small one, one big enough for the resumable route, and
        // one inside a folder (plus, from v2, a new one there).
        let write = |v: u8| {
            std::fs::write(src.join("a.txt"), format!("versão {v}")).unwrap();
            std::fs::write(src.join("big.bin"), vec![v; 6 * 1024 * 1024]).unwrap();
            std::fs::write(src.join("Pasta").join("dentro.txt"), format!("dentro {v}")).unwrap();
            if v > 1 {
                std::fs::write(src.join("Pasta").join("novo.txt"), format!("novo {v}")).unwrap();
            }
        };
        let sources = ["a.txt", "big.bin", "Pasta"].map(|n| src.join(n));
        let names = |id: &str| -> Vec<(String, String, Option<String>)> {
            let mut out = Vec::new();
            d.list(id, None, |page, _| {
                out.extend(page.into_iter().map(|f| (f.name, f.id, f.md5)));
                true
            })
            .unwrap();
            out.sort();
            out
        };
        let md5 = |p: &Path| ops::md5_file(p, &|| false).unwrap();
        // One upload of all three, with `clash` for every taken name, as the dialog would.
        let upload = |clash: Clash| {
            let existing = listing(&d, &root.id).unwrap();
            let local: Vec<(String, bool)> = sources.iter().map(|p| (p.file_name().unwrap().to_string_lossy().into_owned(), p.is_dir())).collect();
            let items: Vec<Item> = resolve(&local, &existing, |_| clash).into_iter().zip(&sources).filter(|(put, _)| *put != Put::Skip).map(|(put, p)| Item { path: p.clone(), put, clash }).collect();
            let mut list = Vec::new();
            let batch = queue(&mut list, None, Op::Upload, items.iter().map(|it| (End::local(&it.path), End::local(&it.path))));
            run(&d, &batch, &items, &root.id, false, 4, &Slots::new(4)).unwrap()
        };
        let folder = |list: &[(String, String, Option<String>)]| -> String {
            let pastas: Vec<_> = list.iter().filter(|(n, ..)| n == "Pasta").collect();
            assert_eq!(pastas.len(), 1, "one Pasta, merged into: {list:?}");
            pastas[0].1.clone()
        };
        let find = |list: &[(String, String, Option<String>)], name: &str| list.iter().find(|(n, ..)| n == name).cloned().unwrap_or_else(|| panic!("{name} missing: {list:?}"));

        write(1);
        upload(Clash::KeepBoth);
        let first = names(&root.id);
        let inside_first = names(&folder(&first));
        println!("v1: {first:?}\n    {inside_first:?}");

        // Substituir: same ids, new content, the folder merged into and its new file added.
        write(2);
        println!("Substituir: {}", upload(Clash::Replace));
        let now = names(&root.id);
        assert_eq!(now.len(), 3, "{now:?}");
        for name in ["a.txt", "big.bin"] {
            let (_, id, sum) = find(&now, name);
            assert_eq!(id, find(&first, name).1, "{name} kept its id");
            assert_eq!(sum.as_deref(), Some(md5(&src.join(name)).as_str()), "{name} has the new content");
            assert_eq!(d.revisions(&id).unwrap().len(), 2, "{name} has two versions");
        }
        let inside = names(&folder(&now));
        assert_eq!(folder(&now), folder(&first));
        assert_eq!(inside.len(), 2, "{inside:?}");
        assert_eq!(find(&inside, "dentro.txt").1, find(&inside_first, "dentro.txt").1);
        assert_eq!(find(&inside, "dentro.txt").2.unwrap(), md5(&src.join("Pasta").join("dentro.txt")));

        // Pular: nothing changes.
        write(3);
        println!("Pular: {}", upload(Clash::Skip));
        assert_eq!(names(&root.id), now);
        assert_eq!(names(&folder(&now)), inside);

        // Manter os dois: numbered copies beside the originals, in the same folder.
        write(4);
        println!("Manter os dois: {}", upload(Clash::KeepBoth));
        let both = names(&root.id);
        let got: Vec<&str> = both.iter().map(|(n, ..)| n.as_str()).collect();
        assert_eq!(got, ["Pasta", "a (2).txt", "a.txt", "big (2).bin", "big.bin"]);
        assert_eq!(find(&both, "a (2).txt").2.unwrap(), md5(&src.join("a.txt")));
        assert_eq!(find(&both, "a.txt").2, find(&now, "a.txt").2, "the original is untouched");
        let inside: Vec<String> = names(&folder(&both)).into_iter().map(|(n, ..)| n).collect();
        assert_eq!(inside, ["dentro (2).txt", "dentro.txt", "novo (2).txt", "novo.txt"]);
        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn listed_items_tell_folders_files_and_google_formats_apart() {
        let e = |mime: &str, target: bool| Entry {
            id: "x".into(),
            name: "x".into(),
            is_dir: mime == FOLDER_MIME || target,
            size: None,
            modified: None,
            target: target.then(|| "t".into()),
            mime: Some(mime.into()),
            md5: None,
            starred: false,
            drive_origin: None,
        };
        assert_eq!(Existing::of_entry(&e(FOLDER_MIME, false)).kind, Kind::Folder);
        assert_eq!(Existing::of_entry(&e("image/jpeg", false)).kind, Kind::File);
        assert_eq!(Existing::of_entry(&e("application/vnd.google-apps.document", false)).kind, Kind::Other);
        assert_eq!(Existing::of_entry(&e("application/vnd.google-apps.shortcut", true)).kind, Kind::Other);
    }
}
