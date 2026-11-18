//! Desfazer (Ctrl+Z) and Refazer (Ctrl+Y, Ctrl+Shift+Z) for what was done to files: renames,
//! moves, new folders, copies and what went to a trash.
//!
//! A job that can be undone records, as it goes, the [`Step`]s that take back what it did, and
//! ends in a [`Msg::Undo`] carrying them as a [`Record`] for the list in [`History`]. Undoing a
//! record checks first that every place it would touch is still as the action left it, and
//! does nothing at all if one isn't; then it runs the steps, each giving back the step that
//! redoes it. Nothing is deleted for good on the way: what goes away goes to the Recycle Bin or
//! the Drive trash, except a new folder that is still empty.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use openexplorer_gdrive::{Drive, Status};
use openexplorer_remote::{Conn, Pool, Server};

use crate::model::Location;
use crate::{AccountRef, Msg, State, background, catch_panic, drive_task, ops};

/// How many actions Desfazer remembers.
pub const KEEP: usize = 20;

/// One change, as undoing or redoing makes it. Running it gives the step that takes it back.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// The item at `from` on the computer goes to `to`: a rename, or a move into another folder
    /// under the same name.
    Move { from: PathBuf, to: PathBuf },
    /// The item at `path` goes to the Recycle Bin.
    Recycle { path: PathBuf },
    /// What went to the Recycle Bin from `path` at `since` (seconds since 1970) or later comes
    /// back there; the latest, if it went more than once.
    Unrecycle { path: PathBuf, since: i64 },
    /// A new folder at `path`.
    MakeDir { path: PathBuf },
    /// The folder at `path` goes, only while it's empty.
    RemoveDir { path: PathBuf },
    /// Drive item `id` of account `email`, named `from`, is renamed `to`.
    DriveRename { email: String, id: String, from: String, to: String },
    /// Drive item `id`, named `name`, goes from folder `from` to folder `to`.
    DriveMove { email: String, id: String, name: String, from: String, to: String },
    /// Drive item `id`, named `name`, goes to the trash. With `empty` (a new folder), only while
    /// it holds nothing.
    DriveTrash { email: String, id: String, name: String, empty: bool },
    /// Drive item `id`, named `name`, comes back from the trash. `empty` is kept for the
    /// [`Step::DriveTrash`] that takes it back.
    DriveRestore { email: String, id: String, name: String, empty: bool },
    /// On `server`, the item at `from` goes to `to`.
    ServerMove { server: Server, from: String, to: String },
    /// A new folder at `path` on `server`.
    ServerMakeDir { server: Server, path: String },
    /// The folder at `path` on `server` goes, only while it's empty.
    ServerRemoveDir { server: Server, path: String },
}

/// What undoes one action, or redoes one undone.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    /// The action in a few words, for the messages: "renomear “a” para “b”", "mover 3 itens".
    pub what: String,
    pub steps: Vec<Step>,
    /// The places the action reloaded, reloaded again after undoing it.
    pub reload: Vec<Location>,
}

/// The steps a job records as it goes, from whichever of its threads did the work. Each
/// [`Rec::add`] is one item the action touched.
#[derive(Default)]
pub struct Rec(Mutex<(Vec<Step>, usize)>);

impl Rec {
    /// The steps that take back what was just done to one item.
    pub fn add(&self, steps: impl IntoIterator<Item = Step>) {
        let mut rec = self.0.lock().unwrap_or_else(|e| e.into_inner());
        rec.0.extend(steps);
        rec.1 += 1;
    }

    /// The record of what was done (`what` says it, given how many items), if anything was.
    fn record(&self, what: impl FnOnce(usize) -> String, reload: Vec<Location>) -> Option<Record> {
        let (steps, items) = std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()));
        (!steps.is_empty()).then(|| Record { what: what(items), steps, reload })
    }
}

/// "1 item", "3 itens".
pub fn items(n: usize) -> String {
    if n == 1 { "1 item".into() } else { format!("{n} itens") }
}

/// What a background job gives back for Desfazer.
pub enum Answer {
    /// An action ended; `record` takes back what of it was done.
    Did { record: Option<Record>, reload: Vec<Location>, result: Result<String, String> },
    /// An undo (or with `redo`, a redo) ended. `back` takes back what it did, for the other
    /// list; `left` is what of the record it didn't get to, back on its own list.
    Undid { redo: bool, back: Option<Record>, left: Option<Record>, reload: Vec<Location>, result: Result<String, String> },
}

/// The actions that can be undone, and the ones undone that can be redone.
#[derive(Default)]
pub struct History {
    /// Newest last.
    done: Vec<Record>,
    undone: Vec<Record>,
    /// An undo or redo is running.
    busy: bool,
    /// The message whose notice offers Desfazer (`false`) or Refazer (`true`).
    offer: Option<(String, bool)>,
}

impl History {
    /// A new action: the oldest goes past [`KEEP`], and what was undone can't be redone any
    /// more, as in every editor.
    pub fn push(&mut self, r: Record) {
        self.keep(r);
        self.undone.clear();
    }

    fn keep(&mut self, r: Record) {
        self.done.push(r);
        if self.done.len() > KEEP {
            self.done.remove(0);
        }
    }

    /// Whether the notice saying `status` offers Refazer (`Some(true)`) or Desfazer
    /// (`Some(false)`): only the notice of the latest action, while it's still there to take
    /// back.
    pub fn offers(&self, status: &str) -> Option<bool> {
        let (said, redo) = self.offer.as_ref()?;
        let list = if *redo { &self.undone } else { &self.done };
        (said == status && !list.is_empty() && !self.busy).then_some(*redo)
    }

    #[cfg(test)]
    pub fn lens(&self) -> (usize, usize) {
        (self.done.len(), self.undone.len())
    }
}

fn text(result: &Result<String, String>) -> String {
    match result {
        Ok(m) | Err(m) => m.clone(),
    }
}

/// An answer arrived: its records join the lists, then it's handled as any [`Msg::Done`]
/// (its message, the panes to reload), sent back through the inbox so that stays in one place.
pub fn arrived(s: &mut State, answer: Answer) {
    let h = &mut s.undo;
    let (reload, result) = match answer {
        Answer::Did { record, reload, result } => {
            if let Some(r) = record {
                h.push(r);
                h.offer = Some((text(&result), false));
            }
            (reload, result)
        }
        Answer::Undid { redo, back, left, reload, result } => {
            h.busy = false;
            h.offer = back.is_some().then(|| (text(&result), !redo));
            let (to_back, to_left) = if redo { (&mut h.done, &mut h.undone) } else { (&mut h.undone, &mut h.done) };
            to_back.extend(back);
            to_left.extend(left);
            if h.done.len() > KEEP {
                h.done.remove(0);
            }
            (reload, result)
        }
    };
    let _ = s.outbox.send(Msg::Done { reload, result });
}

// ---------------------------------------------------------------------------------------------
// Jobs that record

fn did(rec: &Rec, what: impl FnOnce(usize) -> String, reload: Vec<Location>, result: Result<String, String>) -> Msg {
    Msg::Undo(Answer::Did { record: rec.record(what, reload.clone()), reload, result })
}

/// Runs `work` on the disk on a background thread, then reloads `reload` and says how it went;
/// what `work` records can be undone, `what` saying it for how many items.
pub fn local_job(s: &State, reload: Vec<Location>, what: impl FnOnce(usize) -> String + Send + 'static, work: impl FnOnce(&Rec) -> Result<String, String> + Send + 'static) {
    background(s, move |emit| {
        let rec = Rec::default();
        let result = catch_panic("operar no disco", || work(&rec));
        emit(did(&rec, what, reload, result));
    });
}

/// [`local_job`] with `email`'s Drive.
pub fn drive_job(
    s: &State,
    email: &str,
    reload: Vec<Location>,
    what: impl FnOnce(usize) -> String + Send + 'static,
    work: impl FnOnce(&Drive, &Rec) -> Result<String, String> + Send + 'static,
) {
    let rec = Arc::new(Rec::default());
    let job = rec.clone();
    drive_task(s, email, "operar no Drive", move |d| Ok(work(d, &job)), move |result| did(&rec, what, reload, result.and_then(|r| r)));
}

/// [`local_job`] with a connection to `server`.
pub fn server_job(
    s: &State,
    server: &Server,
    reload: Vec<Location>,
    what: impl FnOnce(usize) -> String + Send + 'static,
    work: impl FnOnce(&mut dyn Conn, &Rec) -> Result<String, String> + Send + 'static,
) {
    let (pool, server) = (s.servers.pool.clone(), server.clone());
    background(s, move |emit| {
        let rec = Rec::default();
        let result = catch_panic("operar no servidor", || crate::servers::with(&pool, &server, |c| work(c, &rec)));
        emit(did(&rec, what, reload, result));
    });
}

// ---------------------------------------------------------------------------------------------
// Undoing

/// Ctrl+Z (or with `redo`, Ctrl+Y): undoes the latest action, or redoes the latest undone, on a
/// background thread.
pub fn undo(s: &mut State, redo: bool) {
    if s.undo.busy {
        s.status = format!("Espere: ainda {} a última ação.", if redo { "refazendo" } else { "desfazendo" });
        return;
    }
    let list = if redo { &mut s.undo.undone } else { &mut s.undo.done };
    let Some(record) = list.pop() else {
        s.status = if redo { "Nada para refazer." } else { "Nada para desfazer." }.into();
        return;
    };
    s.undo.busy = true;
    s.undo.offer = None;
    s.status = format!("{} {}…", if redo { "Refazendo:" } else { "Desfazendo:" }, record.what);
    let mut accounts: Vec<AccountRef> = Vec::new();
    for step in &record.steps {
        if let Some(email) = step.account()
            && !accounts.iter().any(|a| a.email == email)
        {
            accounts.push(AccountRef::of(s, email));
        }
    }
    let pool = s.servers.pool.clone();
    background(s, move |emit| {
        let reload = record.reload.clone();
        let mut live = Live { accounts, open: HashMap::new(), pool, emit };
        let replayed = catch_panic(if redo { "refazer" } else { "desfazer" }, || Ok(replay(record, &mut live, redo)));
        let (back, left, result) = match replayed {
            Ok(r) => (r.back, r.left, r.result),
            Err(e) => (None, None, Err(e)),
        };
        emit(Msg::Undo(Answer::Undid { redo, back, left, reload, result }));
    });
}

/// How a record went when replayed.
pub struct Replayed {
    pub back: Option<Record>,
    pub left: Option<Record>,
    pub result: Result<String, String>,
}

/// Undoes (with `redo`, redoes) `record` through `reach`. Every step is checked before any
/// runs: if one finds its item changed, nothing is done and the record is dropped, as it can't
/// be taken back any more. A step that fails while running stops the rest, which stay to be
/// tried again.
pub fn replay(record: Record, reach: &mut dyn Reach, redo: bool) -> Replayed {
    let (verb, done) = if redo { ("refazer", "Refeito") } else { ("desfazer", "Desfeito") };
    let Record { what, steps, reload } = record;
    let mut bin = Bin::default();
    if let Some(why) = steps.iter().find_map(|step| step.check(reach, &mut bin).err()) {
        return Replayed { back: None, left: None, result: Err(format!("Não dá para {verb} “{what}”: {why}")) };
    }
    let mut back = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        match step.run(reach, &mut bin) {
            Ok(inverse) => back.push(inverse),
            Err(e) => {
                let n = steps.len();
                let left = Some(Record { what: what.clone(), steps: steps[i..].to_vec(), reload: reload.clone() });
                let back = (!back.is_empty()).then(|| Record { what: what.clone(), steps: back, reload });
                return Replayed { back, left, result: Err(format!("{done} só em parte ({i} de {n}) — “{what}”: {e}")) };
            }
        }
    }
    Replayed { back: Some(Record { what: what.clone(), steps: back, reload }), left: None, result: Ok(format!("{done}: {what}.")) }
}

/// The Drive accounts and servers a replay reaches.
pub trait Reach {
    fn drive(&mut self, email: &str) -> Result<&dyn DriveSide, String>;
    fn server(&mut self, server: &Server, work: &mut dyn FnMut(&mut dyn Conn) -> Result<(), String>) -> Result<(), String>;
}

/// What a replay asks of a Drive account.
pub trait DriveSide {
    fn status(&self, id: &str) -> Result<Status, String>;
    /// Whether folder `id` holds anything.
    fn has_children(&self, id: &str) -> Result<bool, String>;
    /// Whether something (out of the trash) is named `name` in folder `parent`.
    fn taken(&self, parent: &str, name: &str) -> Result<bool, String>;
    fn rename(&self, id: &str, name: &str) -> Result<(), String>;
    fn move_to(&self, id: &str, from: &str, to: &str) -> Result<(), String>;
    fn trash(&self, id: &str) -> Result<(), String>;
    fn restore(&self, id: &str) -> Result<(), String>;
}

impl DriveSide for Drive {
    fn status(&self, id: &str) -> Result<Status, String> {
        Drive::status(self, id)
    }
    fn has_children(&self, id: &str) -> Result<bool, String> {
        Drive::has_children(self, id)
    }
    fn taken(&self, parent: &str, name: &str) -> Result<bool, String> {
        self.child_named(parent, None, name).map(|f| f.is_some())
    }
    fn rename(&self, id: &str, name: &str) -> Result<(), String> {
        Drive::rename(self, id, name).map(|_| ())
    }
    fn move_to(&self, id: &str, from: &str, to: &str) -> Result<(), String> {
        Drive::move_to(self, id, from, to).map(|_| ())
    }
    fn trash(&self, id: &str) -> Result<(), String> {
        Drive::trash(self, id).map(|_| ())
    }
    fn restore(&self, id: &str) -> Result<(), String> {
        Drive::restore(self, id).map(|_| ())
    }
}

/// The accounts and servers as the app reaches them: an account is opened on first use.
struct Live<'a> {
    accounts: Vec<AccountRef>,
    open: HashMap<String, Arc<Drive>>,
    pool: Arc<Pool>,
    emit: &'a dyn Fn(Msg),
}

impl Reach for Live<'_> {
    fn drive(&mut self, email: &str) -> Result<&dyn DriveSide, String> {
        if !self.open.contains_key(email) {
            let at = self.accounts.iter().position(|a| a.email == email).ok_or_else(|| format!("A conta {email} não está conectada."))?;
            let drive = self.accounts.remove(at).open(self.emit)?;
            self.open.insert(email.to_string(), drive);
        }
        Ok(&**self.open.get(email).expect("opened above"))
    }

    fn server(&mut self, server: &Server, work: &mut dyn FnMut(&mut dyn Conn) -> Result<(), String>) -> Result<(), String> {
        crate::servers::with(&self.pool, server, work)
    }
}

// ---------------------------------------------------------------------------------------------
// Steps

fn name_of(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

fn dir_of(path: &Path) -> String {
    path.parent().map(|d| d.display().to_string()).unwrap_or_default()
}

/// Whether what's sent to the trash from `path` lands in a Recycle Bin. Shared folders and WSL
/// have none, and USB sticks and memory cards usually don't either: Windows deletes from them
/// for good.
pub fn has_recycle_bin(path: &Path) -> bool {
    !crate::network::is_remote(path) && crate::model::drive_kind(path) == crate::model::DriveKind::Fixed
}

/// Whether something is at `path` (a broken link too).
fn there(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Same path, as the system compares them: Windows ignores case.
fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) { a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase() } else { a == b }
}

/// The server path's name.
fn server_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// The Recycle Bin's listing, read once per replay: reading it is slow, and undoing a delete of
/// 100 items would read it twice for each.
#[derive(Default)]
pub struct Bin(Option<Vec<Recycled>>);

#[cfg(any(windows, all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))))]
type Recycled = trash::TrashItem;
#[cfg(not(any(windows, all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))))]
type Recycled = ();

impl Bin {
    /// What went to the Recycle Bin from `path` at `since` or later; the latest, if several did.
    fn find(&mut self, path: &Path, since: i64) -> Result<Option<Recycled>, String> {
        let all = match &mut self.0 {
            Some(all) => all,
            none => none.insert(read_bin()?),
        };
        Ok(pick(all, path, since))
    }
}

#[cfg(any(windows, all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))))]
fn read_bin() -> Result<Vec<Recycled>, String> {
    trash::os_limited::list().map_err(|e| format!("Não consegui ler a Lixeira: {e}"))
}

#[cfg(not(any(windows, all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))))]
fn read_bin() -> Result<Vec<Recycled>, String> {
    Err("Restaurar da Lixeira não é possível neste sistema.".into())
}

#[cfg(any(windows, all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))))]
fn pick(all: &[Recycled], path: &Path, since: i64) -> Option<Recycled> {
    let (dir, name) = (spelled_out(path.parent()?), path.file_name()?);
    let named = |it: &&Recycled| same_path(Path::new(&it.name), Path::new(name));
    all.iter().filter(|it| it.time_deleted >= since).filter(named).filter(|it| same_path(&spelled_out(&it.original_parent), &dir)).max_by_key(|it| it.time_deleted).cloned()
}

#[cfg(not(any(windows, all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))))]
fn pick(_: &[Recycled], _: &Path, _: i64) -> Option<Recycled> {
    None
}

/// `dir` named as the Recycle Bin keeps it: with Windows' short names, which %TEMP% often has
/// (`CAU~1`), spelled out.
#[cfg(any(windows, all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))))]
fn spelled_out(dir: &Path) -> PathBuf {
    std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

impl Step {
    /// The Drive account the step works in.
    fn account(&self) -> Option<&str> {
        match self {
            Step::DriveRename { email, .. } | Step::DriveMove { email, .. } | Step::DriveTrash { email, .. } | Step::DriveRestore { email, .. } => Some(email),
            _ => None,
        }
    }

    /// Whether the item the step changes is still as it expects, or what changed.
    fn check(&self, reach: &mut dyn Reach, bin: &mut Bin) -> Result<(), String> {
        match self {
            Step::Move { from, to } => {
                if !there(from) {
                    return Err(format!("“{}” não está mais em {}.", name_of(from), dir_of(from)));
                }
                if there(to) && !same_path(from, to) {
                    return Err(format!("já existe “{}” em {}.", name_of(to), dir_of(to)));
                }
                if !to.parent().is_some_and(Path::is_dir) {
                    return Err(format!("a pasta {} não existe mais.", dir_of(to)));
                }
                Ok(())
            }
            Step::Recycle { path } => {
                if !there(path) {
                    return Err(format!("“{}” não está mais em {}.", name_of(path), dir_of(path)));
                }
                // Without a Recycle Bin (a shared folder, WSL, a USB stick) it would be gone for good.
                if !has_recycle_bin(path) {
                    return Err(format!("{} não tem Lixeira, e “{}” seria apagado para sempre.", dir_of(path), name_of(path)));
                }
                Ok(())
            }
            Step::Unrecycle { path, since } => {
                if there(path) {
                    return Err(format!("já existe “{}” em {}.", name_of(path), dir_of(path)));
                }
                match bin.find(path, *since)? {
                    Some(_) => Ok(()),
                    None => Err(format!("“{}” não está mais na Lixeira.", name_of(path))),
                }
            }
            Step::MakeDir { path } => {
                if there(path) {
                    return Err(format!("já existe “{}” em {}.", name_of(path), dir_of(path)));
                }
                if !path.parent().is_some_and(Path::is_dir) {
                    return Err(format!("a pasta {} não existe mais.", dir_of(path)));
                }
                Ok(())
            }
            Step::RemoveDir { path } => {
                let meta = std::fs::symlink_metadata(path).map_err(|_| format!("“{}” não está mais em {}.", name_of(path), dir_of(path)))?;
                if !meta.is_dir() {
                    return Err(format!("“{}” não é mais uma pasta.", name_of(path)));
                }
                let empty = std::fs::read_dir(path).map(|mut d| d.next().is_none()).unwrap_or(false);
                if !empty {
                    return Err(format!("a pasta “{}” não está mais vazia.", name_of(path)));
                }
                Ok(())
            }
            Step::DriveRename { email, id, from, to } => {
                let d = reach.drive(email)?;
                let st = d.status(id)?;
                drive_alive(&st, from)?;
                if let Some(parent) = st.parents.first()
                    && !from.eq(to)
                    && d.taken(parent, to)?
                {
                    return Err(format!("já existe “{to}” nessa pasta do Drive."));
                }
                Ok(())
            }
            Step::DriveMove { email, id, name, from, to } => {
                let d = reach.drive(email)?;
                let st = d.status(id)?;
                drive_alive(&st, name)?;
                if !st.parents.contains(from) {
                    return Err(format!("“{name}” mudou de pasta no Drive."));
                }
                if d.taken(to, name)? {
                    return Err(format!("já existe “{name}” na pasta de destino do Drive."));
                }
                Ok(())
            }
            Step::DriveTrash { email, id, name, empty } => {
                let d = reach.drive(email)?;
                drive_alive(&d.status(id)?, name)?;
                if *empty && d.has_children(id)? {
                    return Err(format!("a pasta “{name}” não está mais vazia."));
                }
                Ok(())
            }
            Step::DriveRestore { email, id, name, .. } => {
                let d = reach.drive(email)?;
                let st = d.status(id)?;
                if !st.trashed {
                    return Err(format!("“{name}” não está mais na lixeira do Drive."));
                }
                if st.name != *name {
                    return Err(format!("“{name}” mudou de nome no Drive."));
                }
                if let Some(parent) = st.parents.first()
                    && d.taken(parent, name)?
                {
                    return Err(format!("já existe “{name}” na pasta de onde ele saiu, no Drive."));
                }
                Ok(())
            }
            Step::ServerMove { server, from, to } => reach.server(server, &mut |c| {
                if c.stat(from)?.is_none() {
                    return Err(format!("“{}” não está mais no servidor.", server_name(from)));
                }
                if c.stat(to)?.is_some() {
                    return Err(format!("já existe “{}” no servidor.", server_name(to)));
                }
                Ok(())
            }),
            Step::ServerMakeDir { server, path } => reach.server(server, &mut |c| match c.stat(path)? {
                Some(_) => Err(format!("já existe “{}” no servidor.", server_name(path))),
                None => Ok(()),
            }),
            Step::ServerRemoveDir { server, path } => reach.server(server, &mut |c| match c.stat(path)? {
                None => Err(format!("“{}” não está mais no servidor.", server_name(path))),
                Some(it) if !it.is_dir || it.link => Err(format!("“{}” não é mais uma pasta.", server_name(path))),
                Some(_) if !c.list(path)?.is_empty() => Err(format!("a pasta “{}” não está mais vazia.", server_name(path))),
                Some(_) => Ok(()),
            }),
        }
    }

    /// Makes the change, giving the step that takes it back.
    fn run(&self, reach: &mut dyn Reach, bin: &mut Bin) -> Result<Step, String> {
        Ok(match self {
            Step::Move { from, to } => {
                let into = to.parent().ok_or("Destino sem pasta.")?;
                if from.parent() == Some(into) {
                    ops::rename(from, &name_of(to))?;
                } else if from.file_name() == to.file_name() {
                    // Across disks, a copy then a delete of the original, as the move was.
                    ops::move_into(from, into, &ops::Progress::new(None))?;
                } else {
                    std::fs::rename(from, to).map_err(|e| format!("Não consegui mover “{}”: {e}", name_of(from)))?;
                }
                Step::Move { from: to.clone(), to: from.clone() }
            }
            Step::Recycle { path } => {
                // The Recycle Bin dates items to the second.
                let since = now_secs() - 1;
                ops::to_recycle_bin(std::slice::from_ref(path))?;
                Step::Unrecycle { path: path.clone(), since }
            }
            Step::Unrecycle { path, since } => {
                #[cfg(any(windows, all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))))]
                {
                    let item = bin.find(path, *since)?.ok_or_else(|| format!("“{}” não está mais na Lixeira.", name_of(path)))?;
                    trash::os_limited::restore_all([item]).map_err(|e| format!("Não consegui restaurar “{}” da Lixeira: {e}", name_of(path)))?;
                }
                #[cfg(not(any(windows, all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))))]
                bin.find(path, *since)?;
                Step::Recycle { path: path.clone() }
            }
            Step::MakeDir { path } => {
                std::fs::create_dir(path).map_err(|e| format!("Não consegui criar a pasta “{}”: {e}", name_of(path)))?;
                Step::RemoveDir { path: path.clone() }
            }
            Step::RemoveDir { path } => {
                // Fails if anything arrived in it since the check: it's never emptied here.
                std::fs::remove_dir(path).map_err(|e| format!("Não consegui remover a pasta “{}”: {e}", name_of(path)))?;
                Step::MakeDir { path: path.clone() }
            }
            Step::DriveRename { email, id, from, to } => {
                reach.drive(email)?.rename(id, to)?;
                Step::DriveRename { email: email.clone(), id: id.clone(), from: to.clone(), to: from.clone() }
            }
            Step::DriveMove { email, id, name, from, to } => {
                reach.drive(email)?.move_to(id, from, to)?;
                Step::DriveMove { email: email.clone(), id: id.clone(), name: name.clone(), from: to.clone(), to: from.clone() }
            }
            Step::DriveTrash { email, id, name, empty } => {
                reach.drive(email)?.trash(id)?;
                Step::DriveRestore { email: email.clone(), id: id.clone(), name: name.clone(), empty: *empty }
            }
            Step::DriveRestore { email, id, name, empty } => {
                reach.drive(email)?.restore(id)?;
                Step::DriveTrash { email: email.clone(), id: id.clone(), name: name.clone(), empty: *empty }
            }
            Step::ServerMove { server, from, to } => {
                reach.server(server, &mut |c| c.rename(from, to))?;
                Step::ServerMove { server: server.clone(), from: to.clone(), to: from.clone() }
            }
            Step::ServerMakeDir { server, path } => {
                reach.server(server, &mut |c| c.mkdir(path))?;
                Step::ServerRemoveDir { server: server.clone(), path: path.clone() }
            }
            Step::ServerRemoveDir { server, path } => {
                // An empty folder only: the server refuses one that isn't.
                reach.server(server, &mut |c| c.remove_dir(path))?;
                Step::ServerMakeDir { server: server.clone(), path: path.clone() }
            }
        })
    }
}

/// Whether a Drive item is still out of the trash under the name `name`.
fn drive_alive(st: &Status, name: &str) -> Result<(), String> {
    if st.trashed {
        return Err(format!("“{name}” está na lixeira do Drive."));
    }
    if st.name != name {
        return Err(format!("“{name}” mudou de nome no Drive (agora “{}”).", st.name));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::io::{Read, Write};

    use openexplorer_remote::{Item, Protocol};

    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("openexplorer-undo-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn record(what: &str, steps: Vec<Step>) -> Record {
        Record { what: what.into(), steps, reload: Vec::new() }
    }

    /// A Drive account in memory: each file's status by id.
    #[derive(Default)]
    struct FakeDrive(RefCell<HashMap<String, Status>>);

    impl FakeDrive {
        fn put(&self, id: &str, name: &str, parent: &str) {
            self.0.borrow_mut().insert(id.into(), Status { name: name.into(), parents: vec![parent.into()], trashed: false });
        }
        fn get(&self, id: &str) -> Status {
            self.0.borrow()[id].clone()
        }
        fn change(&self, id: &str, f: impl FnOnce(&mut Status)) {
            f(self.0.borrow_mut().get_mut(id).unwrap())
        }
    }

    impl DriveSide for FakeDrive {
        fn status(&self, id: &str) -> Result<Status, String> {
            self.0.borrow().get(id).cloned().ok_or_else(|| format!("{id} não encontrado"))
        }
        fn has_children(&self, id: &str) -> Result<bool, String> {
            Ok(self.0.borrow().values().any(|s| !s.trashed && s.parents.iter().any(|p| p == id)))
        }
        fn taken(&self, parent: &str, name: &str) -> Result<bool, String> {
            Ok(self.0.borrow().values().any(|s| !s.trashed && s.name == name && s.parents.iter().any(|p| p == parent)))
        }
        fn rename(&self, id: &str, name: &str) -> Result<(), String> {
            self.change(id, |s| s.name = name.into());
            Ok(())
        }
        fn move_to(&self, id: &str, from: &str, to: &str) -> Result<(), String> {
            self.change(id, |s| {
                s.parents.retain(|p| p != from);
                s.parents.push(to.into());
            });
            Ok(())
        }
        fn trash(&self, id: &str) -> Result<(), String> {
            self.change(id, |s| s.trashed = true);
            Ok(())
        }
        fn restore(&self, id: &str) -> Result<(), String> {
            self.change(id, |s| s.trashed = false);
            Ok(())
        }
    }

    /// A server in memory: each path, and whether it's a folder.
    #[derive(Default)]
    struct FakeConn(BTreeMap<String, bool>);

    impl FakeConn {
        fn item(&self, path: &str) -> Option<Item> {
            self.0.get(path).map(|&is_dir| Item { name: server_name(path).into(), is_dir, size: None, modified: None, link: false })
        }
    }

    impl Conn for FakeConn {
        fn list(&mut self, dir: &str) -> openexplorer_remote::Result<Vec<Item>> {
            let inside = |p: &&String| p.rsplit_once('/').is_some_and(|(d, _)| d == dir);
            Ok(self.0.keys().filter(inside).filter_map(|p| self.item(p)).collect())
        }
        fn stat(&mut self, path: &str) -> openexplorer_remote::Result<Option<Item>> {
            Ok(self.item(path))
        }
        fn mkdir(&mut self, path: &str) -> openexplorer_remote::Result<()> {
            self.0.insert(path.into(), true);
            Ok(())
        }
        fn rename(&mut self, from: &str, to: &str) -> openexplorer_remote::Result<()> {
            if self.0.contains_key(to) {
                return Err("existe".into());
            }
            let is_dir = self.0.remove(from).ok_or("não existe")?;
            self.0.insert(to.into(), is_dir);
            Ok(())
        }
        fn remove_file(&mut self, path: &str) -> openexplorer_remote::Result<()> {
            self.0.remove(path).map(|_| ()).ok_or_else(|| "não existe".into())
        }
        fn remove_dir(&mut self, path: &str) -> openexplorer_remote::Result<()> {
            if !self.list(path)?.is_empty() {
                return Err("não está vazia".into());
            }
            self.remove_file(path)
        }
        fn download(&mut self, _: &str, _: &mut dyn Write, _: openexplorer_remote::Progress) -> openexplorer_remote::Result<()> {
            Err("sem dados".into())
        }
        fn upload(&mut self, _: &str, _: &mut dyn Read, _: u64, _: openexplorer_remote::Progress) -> openexplorer_remote::Result<()> {
            Err("sem dados".into())
        }
        fn home(&mut self) -> openexplorer_remote::Result<String> {
            Ok("/".into())
        }
        fn is_link(&mut self, _: &str) -> openexplorer_remote::Result<bool> {
            Ok(false)
        }
        fn alive(&mut self) -> bool {
            true
        }
    }

    #[derive(Default)]
    struct Fake {
        drive: FakeDrive,
        conn: FakeConn,
    }

    impl Reach for Fake {
        fn drive(&mut self, _: &str) -> Result<&dyn DriveSide, String> {
            Ok(&self.drive)
        }
        fn server(&mut self, _: &Server, work: &mut dyn FnMut(&mut dyn Conn) -> Result<(), String>) -> Result<(), String> {
            work(&mut self.conn)
        }
    }

    /// Undoes `r`, runs `between`, then redoes what the undo gave back, checking each went
    /// through.
    fn undo_then_redo(r: Record, reach: &mut Fake, between: impl FnOnce(&mut Fake)) {
        let undone = replay(r, reach, false);
        assert!(undone.result.is_ok(), "{:?}", undone.result);
        assert!(undone.left.is_none());
        between(reach);
        let redone = replay(undone.back.expect("an undo gives back its redo"), reach, true);
        assert!(redone.result.as_ref().is_ok_and(|m| m.starts_with("Refeito")), "{:?}", redone.result);
    }

    #[test]
    fn the_list_keeps_the_latest_twenty_and_a_new_action_ends_redo() {
        let mut h = History::default();
        for i in 0..KEEP + 5 {
            h.push(record(&format!("ação {i}"), vec![Step::MakeDir { path: i.to_string().into() }]));
        }
        assert_eq!(h.lens(), (KEEP, 0));
        assert_eq!(h.done[0].what, "ação 5", "the oldest went first");
        let last = h.done.pop().unwrap();
        h.undone.push(last);
        assert_eq!(h.lens(), (KEEP - 1, 1));
        h.push(record("nova", vec![]));
        assert_eq!(h.lens(), (KEEP, 0), "a new action can't sit before one undone");

        // Only the notice of the latest action offers Desfazer, and not while an undo runs.
        h.offer = Some(("Renomeado para \"b\".".into(), false));
        assert_eq!(h.offers("Renomeado para \"b\"."), Some(false));
        assert_eq!(h.offers("Copiado: 1 item."), None);
        h.busy = true;
        assert_eq!(h.offers("Renomeado para \"b\"."), None);
        h.busy = false;
        h.offer = Some(("Desfeito: x.".into(), true));
        assert_eq!(h.offers("Desfeito: x."), None, "nothing to redo");
    }

    #[test]
    fn local_renames_moves_and_new_folders_go_back_and_forth() {
        let dir = scratch("local");
        let (a, sub) = (dir.join("a.txt"), dir.join("sub"));
        std::fs::write(&a, "oi").unwrap();
        std::fs::create_dir(&sub).unwrap();
        let mut reach = Fake::default();

        // Renamed a.txt to b.txt: Desfazer names it back, Refazer again.
        let b = dir.join("b.txt");
        std::fs::rename(&a, &b).unwrap();
        undo_then_redo(record("renomear", vec![Step::Move { from: b.clone(), to: a.clone() }]), &mut reach, |_| {
            assert!(a.exists() && !b.exists());
        });
        assert!(b.exists() && !a.exists());

        // Moved into sub: back out, and in again.
        let moved = sub.join("b.txt");
        std::fs::rename(&b, &moved).unwrap();
        undo_then_redo(record("mover", vec![Step::Move { from: moved.clone(), to: b.clone() }]), &mut reach, |_| {
            assert!(b.exists() && !moved.exists());
        });
        assert_eq!(std::fs::read_to_string(&moved).unwrap(), "oi");

        // A new folder, still empty, goes; Refazer makes it again.
        let fresh = dir.join("Nova pasta");
        std::fs::create_dir(&fresh).unwrap();
        undo_then_redo(record("criar", vec![Step::RemoveDir { path: fresh.clone() }]), &mut reach, |_| assert!(!fresh.exists()));
        assert!(fresh.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_is_undone_when_things_changed() {
        let dir = scratch("changed");
        let mut reach = Fake::default();
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&b, "renomeado").unwrap();

        // Something else now has the old name.
        std::fs::write(&a, "outro").unwrap();
        let r = replay(record("renomear “a.txt” para “b.txt”", vec![Step::Move { from: b.clone(), to: a.clone() }]), &mut reach, false);
        let why = r.result.unwrap_err();
        assert!(why.starts_with("Não dá para desfazer “renomear “a.txt” para “b.txt””: já existe “a.txt”"), "{why}");
        assert!(r.back.is_none() && r.left.is_none(), "a record that can't be undone is dropped");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "outro");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "renomeado");

        // Of two items, one is gone: neither moves.
        std::fs::remove_file(&a).unwrap();
        let sub = dir.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let steps = vec![Step::Move { from: b.clone(), to: sub.join("b.txt") }, Step::Move { from: dir.join("sumiu.txt"), to: sub.join("sumiu.txt") }];
        let r = replay(record("mover 2 itens", steps), &mut reach, false);
        assert!(r.result.unwrap_err().contains("“sumiu.txt” não está mais em"));
        assert!(b.exists(), "checked before anything ran");

        // A new folder with something in it now stays, with what's inside.
        let fresh = dir.join("Nova pasta");
        std::fs::create_dir(&fresh).unwrap();
        std::fs::write(fresh.join("dentro.txt"), "x").unwrap();
        let r = replay(record("criar a pasta", vec![Step::RemoveDir { path: fresh.clone() }]), &mut reach, false);
        assert!(r.result.unwrap_err().contains("não está mais vazia"));
        assert!(fresh.join("dentro.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A copy goes to the Recycle Bin and comes back from it, through the system's own Recycle
    /// Bin: the test's file is its own, in a folder it removes at the end.
    #[cfg(any(windows, all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))))]
    #[test]
    fn copies_go_to_the_recycle_bin_and_come_back() {
        /// Even if the test fails, its file doesn't stay in the user's Recycle Bin.
        struct Tidy(PathBuf);
        impl Drop for Tidy {
            fn drop(&mut self) {
                let file = self.0.join("cópia de teste.txt");
                if !there(&file)
                    && let Ok(Some(item)) = Bin::default().find(&file, 0)
                {
                    let _ = trash::os_limited::restore_all([item]);
                }
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = scratch("recycle");
        let _tidy = Tidy(dir.clone());
        let copy = dir.join("cópia de teste.txt");
        std::fs::write(&copy, "cópia").unwrap();
        let mut reach = Fake::default();
        let undone = replay(record("copiar 1 item", vec![Step::Recycle { path: copy.clone() }]), &mut reach, false);
        assert_eq!(undone.result, Ok("Desfeito: copiar 1 item.".into()));
        assert!(!copy.exists(), "the copy went to the Recycle Bin");
        let back = undone.back.unwrap();
        assert!(matches!(&back.steps[..], [Step::Unrecycle { path, .. }] if *path == copy));

        // Something new at its name keeps it in the Recycle Bin.
        std::fs::write(&copy, "outro").unwrap();
        let refused = replay(back.clone(), &mut reach, true);
        assert!(refused.result.unwrap_err().contains("já existe"));
        std::fs::remove_file(&copy).unwrap();

        let redone = replay(back, &mut reach, true);
        assert_eq!(redone.result, Ok("Refeito: copiar 1 item.".into()));
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), "cópia", "back from the Recycle Bin");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drive_renames_moves_and_trash_check_the_item_first() {
        let mut reach = Fake::default();
        reach.drive.put("f1", "novo.txt", "root");
        reach.drive.put("p2", "Pasta", "root");
        let email = "a@b".to_string();

        // Renamed velho.txt to novo.txt: undone and redone.
        let rename = Step::DriveRename { email: email.clone(), id: "f1".into(), from: "novo.txt".into(), to: "velho.txt".into() };
        undo_then_redo(record("renomear", vec![rename.clone()]), &mut reach, |r| assert_eq!(r.drive.get("f1").name, "velho.txt"));
        assert_eq!(reach.drive.get("f1").name, "novo.txt");

        // Renamed again on the website meanwhile: left alone.
        reach.drive.change("f1", |s| s.name = "outro.txt".into());
        let r = replay(record("renomear", vec![rename]), &mut reach, false);
        assert!(r.result.unwrap_err().contains("mudou de nome no Drive (agora “outro.txt”)"));
        assert_eq!(reach.drive.get("f1").name, "outro.txt");

        // Moved into Pasta: back out and in again; refused once something took its name at the
        // origin.
        DriveSide::move_to(&reach.drive, "f1", "root", "p2").unwrap();
        let back = Step::DriveMove { email: email.clone(), id: "f1".into(), name: "outro.txt".into(), from: "p2".into(), to: "root".into() };
        undo_then_redo(record("mover", vec![back.clone()]), &mut reach, |r| assert_eq!(r.drive.get("f1").parents, ["root"]));
        reach.drive.put("f9", "outro.txt", "root");
        assert!(replay(record("mover", vec![back]), &mut reach, false).result.unwrap_err().contains("já existe “outro.txt”"));
        assert_eq!(reach.drive.get("f1").parents, ["p2"]);

        // Sent to the trash: restored, and trashed again by Refazer.
        DriveSide::trash(&reach.drive, "f9").unwrap();
        let restore = Step::DriveRestore { email: email.clone(), id: "f9".into(), name: "outro.txt".into(), empty: false };
        undo_then_redo(record("mover para a Lixeira", vec![restore]), &mut reach, |r| assert!(!r.drive.get("f9").trashed));
        assert!(reach.drive.get("f9").trashed);

        // A new folder goes to the trash only while empty.
        let new = Step::DriveTrash { email, id: "p2".into(), name: "Pasta".into(), empty: true };
        assert!(replay(record("criar a pasta", vec![new.clone()]), &mut reach, false).result.unwrap_err().contains("não está mais vazia"));
        DriveSide::trash(&reach.drive, "f1").unwrap();
        assert!(replay(record("criar a pasta", vec![new]), &mut reach, false).result.is_ok());
        assert!(reach.drive.get("p2").trashed);
    }

    #[test]
    fn server_renames_and_new_folders_check_the_server_first() {
        let server = Server {
            id: "s".into(),
            name: "s".into(),
            protocol: Protocol::Sftp,
            brand: None,
            host: "h".into(),
            port: 22,
            user: "u".into(),
            root: "/".into(),
            key: None,
            host_key: None,
            cert: None,
            connections: 4,
            implicit: false,
        };
        let mut reach = Fake::default();
        reach.conn.0.extend([("/docs".to_string(), true), ("/docs/b.txt".to_string(), false)]);

        let rename = Step::ServerMove { server: server.clone(), from: "/docs/b.txt".into(), to: "/docs/a.txt".into() };
        undo_then_redo(record("renomear", vec![rename.clone()]), &mut reach, |r| assert!(r.conn.0.contains_key("/docs/a.txt")));
        assert!(reach.conn.0.contains_key("/docs/b.txt"));
        reach.conn.0.insert("/docs/a.txt".into(), false);
        assert!(replay(record("renomear", vec![rename]), &mut reach, false).result.unwrap_err().contains("já existe “a.txt” no servidor"));

        reach.conn.0.insert("/nova".into(), true);
        let new = Step::ServerRemoveDir { server, path: "/nova".into() };
        undo_then_redo(record("criar a pasta", vec![new.clone()]), &mut reach, |r| assert!(!r.conn.0.contains_key("/nova")));
        reach.conn.0.insert("/nova/x".into(), false);
        assert!(replay(record("criar a pasta", vec![new]), &mut reach, false).result.unwrap_err().contains("não está mais vazia"));
    }

    /// Drains `s`'s inbox as `poll` does for Desfazer, up to the next Done: answers join the
    /// lists, and the Done says its message.
    fn settle(s: &mut State) {
        while let Ok(msg) = s.inbox.recv_timeout(std::time::Duration::from_secs(10)) {
            match msg {
                Msg::Undo(answer) => arrived(s, answer),
                Msg::Done { result, .. } => {
                    s.status = text(&result);
                    return;
                }
                _ => {}
            }
        }
        panic!("no answer");
    }

    #[test]
    fn a_rename_in_a_pane_is_undone_with_ctrl_z_and_redone() {
        let dir = scratch("pane");
        std::fs::write(dir.join("a.txt"), "oi").unwrap();
        let mut s = crate::init();
        s.panes[0] = crate::model::Pane::new(Location::Local(dir.clone()));
        s.panes[0].load_now(&s.mock, s.settings.show_hidden);
        let item = s.panes[0].entries.iter().find(|e| e.name == "a.txt").cloned().expect("listed");

        crate::actions::apply_name(&mut s, 0, Some(item), "b.txt".into());
        settle(&mut s);
        assert_eq!(s.status, "Renomeado para \"b.txt\".");
        assert_eq!(s.undo.offers(&s.status), Some(false), "the notice offers Desfazer");

        undo(&mut s, false);
        assert_eq!(s.status, "Desfazendo: renomear “a.txt” para “b.txt”…");
        settle(&mut s);
        assert_eq!(s.status, "Desfeito: renomear “a.txt” para “b.txt”.");
        assert!(dir.join("a.txt").exists() && !dir.join("b.txt").exists());
        assert_eq!(s.undo.offers(&s.status), Some(true), "and then Refazer");

        undo(&mut s, true);
        settle(&mut s);
        assert!(dir.join("b.txt").exists() && !dir.join("a.txt").exists());
        assert_eq!(s.undo.lens(), (1, 0));

        // A new folder, then Ctrl+Z: it goes, and the rename is next.
        crate::actions::apply_name(&mut s, 0, None, "Nova pasta".into());
        settle(&mut s);
        assert!(dir.join("Nova pasta").is_dir());
        undo(&mut s, false);
        settle(&mut s);
        assert!(!dir.join("Nova pasta").exists());
        assert_eq!(s.undo.lens(), (1, 1));
        undo(&mut s, false);
        settle(&mut s);
        assert!(dir.join("a.txt").exists());
        undo(&mut s, false);
        assert_eq!(s.status, "Nada para desfazer.");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two panes on two folders of a scratch folder, each listed.
    fn two_panes(tag: &str) -> (State, PathBuf, PathBuf) {
        let dir = scratch(tag);
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        for name in ["um.txt", "dois.txt"] {
            std::fs::write(a.join(name), name).unwrap();
        }
        let mut s = crate::init();
        s.panes[0] = crate::model::Pane::new(Location::Local(a.clone()));
        s.panes[1] = crate::model::Pane::new(Location::Local(b.clone()));
        for p in &mut s.panes {
            p.load_now(&s.mock, s.settings.show_hidden);
        }
        s.panes[0].selected = s.panes[0].entries.iter().map(|e| e.id.clone()).collect();
        (s, a, b)
    }

    #[test]
    fn a_cut_and_paste_goes_back_where_it_came_from() {
        let (mut s, a, b) = two_panes("cut");
        crate::actions::pick_up(&mut s, 0, crate::actions::ClipMode::Cut);
        crate::actions::paste(&mut s, 1);
        settle(&mut s);
        assert!(b.join("um.txt").exists() && b.join("dois.txt").exists() && !a.join("um.txt").exists(), "{}", s.status);

        undo(&mut s, false);
        settle(&mut s);
        assert_eq!(s.status, "Desfeito: mover 2 itens.");
        assert_eq!(std::fs::read_to_string(a.join("um.txt")).unwrap(), "um.txt");
        assert!(a.join("dois.txt").exists() && !b.join("um.txt").exists() && !b.join("dois.txt").exists());
        undo(&mut s, true);
        settle(&mut s);
        assert!(b.join("um.txt").exists() && !a.join("um.txt").exists(), "{}", s.status);
        let _ = std::fs::remove_dir_all(a.parent().unwrap());
    }

    /// A delete sends to the Recycle Bin and Desfazer brings it all back; the test's own files,
    /// restored even if it fails.
    #[cfg(windows)]
    #[test]
    fn a_delete_comes_back_from_the_recycle_bin() {
        struct Tidy(PathBuf);
        impl Drop for Tidy {
            fn drop(&mut self) {
                for name in ["um.txt", "dois.txt"] {
                    let file = self.0.join(name);
                    if !there(&file)
                        && let Ok(Some(item)) = Bin::default().find(&file, 0)
                    {
                        let _ = trash::os_limited::restore_all([item]);
                    }
                }
                let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
            }
        }
        let (mut s, a, _) = two_panes("delete");
        let _tidy = Tidy(a.clone());
        s.settings.skip_recycle_bin = false;
        crate::actions::ask_delete(&mut s, 0);
        assert!(matches!(s.dialog, Some(crate::actions::Dialog::Delete { .. })));
        crate::actions::delete(&mut s);
        settle(&mut s);
        assert!(!a.join("um.txt").exists() && !a.join("dois.txt").exists(), "{}", s.status);

        undo(&mut s, false);
        settle(&mut s);
        assert_eq!(s.status, "Desfeito: mover 2 itens para a Lixeira.");
        assert_eq!(std::fs::read_to_string(a.join("dois.txt")).unwrap(), "dois.txt");
        assert!(a.join("um.txt").exists());
    }
}
