//! What the context menu and its shortcuts do: open, cut, copy, paste, delete, rename, new
//! folder and properties, on the local disk and in Google Drive. The work runs in the
//! background and ends in a [`Msg::Done`] that reloads the folders it touched.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use openexplorer_gdrive::{Details, Drive, DriveFile, Revision};
use rune::{TextEdit, Tx};

use crate::model::{Entry, Location, Space};
use crate::{Msg, State, background, open as open_entry, open_drive, ops};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipMode {
    Copy,
    Cut,
}

/// What Copiar or Recortar picked up, to Colar elsewhere.
pub struct Clip {
    pub mode: ClipMode,
    pub from: Location,
    pub items: Vec<Entry>,
}

/// The dialog open over the window, if any.
pub enum Dialog {
    Delete { pane: usize, items: Vec<Entry> },
    /// The name being typed is `State::name_edit`.
    NewFolder { pane: usize, error: Option<String> },
    Properties(Box<Props>),
}

pub fn selected(s: &State, p: usize) -> Vec<Entry> {
    let pane = &s.panes[p];
    pane.entries.iter().filter(|e| pane.selected.contains(&e.id)).cloned().collect()
}

/// The folder a Drive location shows: (account, folder id). `None` for the shared drives
/// list, which isn't a folder.
fn drive_folder(loc: &Location) -> Option<(String, String)> {
    let Location::Drive { email, space, path } = loc else { return None };
    let id = match (path.last(), space) {
        (Some((id, _)), _) => id.clone(),
        (None, Space::MyDrive) => "root".into(),
        (None, Space::Shared { id, .. }) => id.clone(),
        (None, Space::SharedDrives) => return None,
    };
    Some((email.clone(), id))
}

/// Whether items in `loc` can be changed (renamed, deleted, cut): not in the shared drives list,
/// whose items are the drives themselves, and not in the simulated drive.
pub fn editable(loc: &Location) -> bool {
    match loc {
        Location::Local(_) => true,
        Location::Mock(_) => false,
        Location::Drive { .. } => drive_folder(loc).is_some(),
    }
}

fn not_here(s: &mut State) {
    s.status = "O drive simulado só serve para testar a interface; abra o disco local ou o Google Drive.".into();
}

/// Runs `work` on a background thread with the account's [`Drive`], then reloads `reload`.
fn drive_job(s: &State, email: &str, reload: Vec<Location>, work: impl FnOnce(&Drive) -> Result<String, String> + Send + 'static) {
    let (cached, email) = (s.drives.get(email).cloned(), email.to_string());
    background(s, move |emit| {
        let (drive, made) = match cached {
            Some(d) => (d, None),
            None => match open_drive(&email) {
                Ok(d) => (d.clone(), Some((email, d))),
                Err(e) => return emit(Msg::Done { reload, made: None, result: Err(e) }),
            },
        };
        let result = crate::catch_panic("operar no Drive", || work(&drive));
        emit(Msg::Done { reload, made, result });
    });
}

fn local_job(s: &State, reload: Vec<Location>, work: impl FnOnce() -> Result<String, String> + Send + 'static) {
    background(s, move |emit| emit(Msg::Done { reload, made: None, result: crate::catch_panic("operar no disco", work) }));
}

/// How many items, in words: "1 item", "3 itens".
fn count(n: usize) -> String {
    if n == 1 { "1 item".into() } else { format!("{n} itens") }
}

// ---------------------------------------------------------------------------------------------
// Open

pub fn open(s: &mut State, p: usize, tx: &mut Tx) {
    let items = selected(s, p);
    let [item] = items.as_slice() else {
        if !items.is_empty() {
            s.status = "Selecione um item só para abrir.".into();
        }
        return;
    };
    if item.is_dir {
        return open_entry(s, p, &item.id);
    }
    match &s.panes[p].loc {
        Location::Local(dir) => {
            if let Err(e) = ops::open(&dir.join(&item.name)) {
                s.status = e;
            }
        }
        Location::Drive { .. } => {
            // Downloading comes with transfers; until then the file opens in Drive on the web.
            if let Err(e) = tx.open_url(&format!("https://drive.google.com/open?id={}", item.id)) {
                s.status = format!("Não consegui abrir o navegador: {e:?}");
            }
        }
        Location::Mock(_) => not_here(s),
    }
}

// ---------------------------------------------------------------------------------------------
// Cut, copy, paste

pub fn pick_up(s: &mut State, p: usize, mode: ClipMode) {
    let items = selected(s, p);
    if items.is_empty() {
        return;
    }
    let loc = s.panes[p].loc.clone();
    if !editable(&loc) || (mode == ClipMode::Cut && matches!(loc, Location::Mock(_))) {
        return not_here(s);
    }
    s.status = format!("{} {} — use Colar (Ctrl+V) na pasta de destino.", if mode == ClipMode::Cut { "Recortado:" } else { "Copiado:" }, count(items.len()));
    s.clip = Some(Clip { mode, from: loc, items });
}

/// Whether items of `from` can be copied or moved into `to` as it is: same disk or same Drive
/// account.
fn fits(from: &Location, to: &Location) -> bool {
    match (from, to) {
        (Location::Local(_), Location::Local(_)) => true,
        (Location::Drive { email: a, .. }, Location::Drive { email: b, .. }) => a == b && drive_folder(to).is_some(),
        _ => false,
    }
}

/// Whether Colar can paste the clip into pane `p`.
pub fn can_paste(s: &State, p: usize) -> bool {
    s.clip.as_ref().is_some_and(|c| fits(&c.from, &s.panes[p].loc))
}

/// The arrows between the panes: copies pane `from`'s selection into the folder pane `to`
/// shows, as Copiar there and Colar here would, without touching what Copiar picked up.
pub fn copy_across(s: &mut State, from: usize, to: usize) {
    let items = selected(s, from);
    if items.is_empty() {
        s.status = "Selecione no outro painel o que copiar.".into();
        return;
    }
    let loc = s.panes[from].loc.clone();
    if !editable(&loc) {
        return match loc {
            Location::Mock(_) => not_here(s),
            _ => s.status = "Não dá para copiar daqui.".into(),
        };
    }
    transfer(s, ClipMode::Copy, loc, items, to);
}

pub fn paste(s: &mut State, p: usize) {
    let Some(clip) = &s.clip else { return };
    let (mode, from, items) = (clip.mode, clip.from.clone(), clip.items.clone());
    // A cut is used once; a copy can be pasted again.
    if mode == ClipMode::Cut && fits(&from, &s.panes[p].loc) {
        s.clip = None;
    }
    transfer(s, mode, from, items, p);
}

/// Copies or moves `items` of `from` into the folder pane `p` shows, or says why it can't.
fn transfer(s: &mut State, mode: ClipMode, from: Location, items: Vec<Entry>, p: usize) {
    let to = s.panes[p].loc.clone();
    if !fits(&from, &to) {
        s.status = match (&from, &to) {
            (Location::Local(_), Location::Drive { .. }) | (Location::Drive { .. }, Location::Local(_)) => {
                "Copiar entre o computador e o Google Drive (envio e download) é o próximo passo; por enquanto, cole na mesma origem.".into()
            }
            (Location::Drive { .. }, Location::Drive { .. }) if drive_folder(&to).is_some() => "Colar entre contas diferentes do Google Drive ainda não é possível.".into(),
            _ => "Não dá para colar aqui.".into(),
        };
        return;
    }
    let reload = vec![to.clone(), from.clone()];
    let n = items.len();
    match (&from, &to) {
        (Location::Local(src), Location::Local(dst)) => {
            let (src, dst) = (src.clone(), dst.clone());
            local_job(s, reload, move || {
                for e in &items {
                    let path = src.join(&e.name);
                    match mode {
                        ClipMode::Copy => ops::copy_into(&path, &dst)?,
                        ClipMode::Cut => ops::move_into(&path, &dst)?,
                    };
                }
                Ok(format!("{}: {}.", if mode == ClipMode::Cut { "Movido" } else { "Copiado" }, count(n)))
            });
        }
        _ => {
            let (Some((email, src)), Some((_, dst))) = (drive_folder(&from), drive_folder(&to)) else { return };
            drive_job(s, &email, reload, move |d| {
                for e in &items {
                    let file = DriveFile {
                        id: e.id.clone(),
                        name: e.name.clone(),
                        is_dir: e.is_dir && e.target.is_none(),
                        size: e.size,
                        modified: e.modified,
                        mime: e.mime.clone().unwrap_or_default(),
                        target: None,
                        md5: None,
                    };
                    match mode {
                        ClipMode::Copy if src == dst => d.copy(&file, &dst, &format!("Cópia de {}", e.name)).map(|_| ())?,
                        ClipMode::Copy => d.copy(&file, &dst, &e.name).map(|_| ())?,
                        ClipMode::Cut if src == dst => {}
                        ClipMode::Cut => d.move_to(&e.id, &src, &dst).map(|_| ())?,
                    }
                }
                Ok(format!("{} no Google Drive: {}.", if mode == ClipMode::Cut { "Movido" } else { "Copiado" }, count(n)))
            });
        }
    }
    s.status = format!("{} {}…", if mode == ClipMode::Cut { "Movendo" } else { "Copiando" }, count(n));
}

// ---------------------------------------------------------------------------------------------
// Delete

pub fn ask_delete(s: &mut State, p: usize) {
    let items = selected(s, p);
    if items.is_empty() {
        return;
    }
    if !editable(&s.panes[p].loc) {
        return not_here(s);
    }
    s.dialog = Some(Dialog::Delete { pane: p, items });
}

pub fn delete(s: &mut State) {
    let Some(Dialog::Delete { pane, items }) = s.dialog.take() else { return };
    let loc = s.panes[pane].loc.clone();
    let n = items.len();
    match &loc {
        Location::Local(dir) => {
            let paths: Vec<PathBuf> = items.iter().map(|e| dir.join(&e.name)).collect();
            local_job(s, vec![loc.clone()], move || ops::to_recycle_bin(&paths).map(|_| format!("Movido para a Lixeira: {}.", count(n))));
        }
        Location::Drive { .. } => {
            let Some((email, _)) = drive_folder(&loc) else { return };
            drive_job(s, &email, vec![loc.clone()], move |d| {
                for e in &items {
                    d.trash(&e.id)?;
                }
                Ok(format!("Movido para a lixeira do Google Drive: {}.", count(n)))
            });
        }
        Location::Mock(_) => return not_here(s),
    }
    s.status = format!("Excluindo {}…", count(n));
}

// ---------------------------------------------------------------------------------------------
// Rename, new folder

/// Starts renaming the selected item in place, as Explorer does: its name turns into a field
/// with the part before the extension selected. Enter or a click elsewhere renames, Esc cancels.
pub fn ask_rename(s: &mut State, p: usize, tx: &mut Tx) {
    let items = selected(s, p);
    let [item] = items.as_slice() else { return };
    if !editable(&s.panes[p].loc) {
        return not_here(s);
    }
    let mut edit = TextEdit::new(item.name.clone());
    edit.set_selection(0, stem_len(&item.name, item.is_dir));
    s.name_edit = edit;
    s.renaming = Some((p, item.clone()));
    tx.scroll_into_view_in(crate::pane_key(p), crate::row_key(&item.id));
}

/// How much of a name Renomear selects, in bytes (`set_selection` takes byte offsets: "Relatório"
/// is 10 bytes for 9 letters): the part before the extension, as in Explorer, or all of a
/// folder's name or of a name that starts with its only dot (".bashrc").
fn stem_len(name: &str, is_dir: bool) -> usize {
    match name.rfind('.') {
        Some(i) if i > 0 && !is_dir => i,
        _ => name.len(),
    }
}

/// Why `name` can't name an item in `loc`, or `None`. Windows refuses characters such as `:`
/// and `?` that Google Drive accepts, so its rules apply only on the local disk.
fn name_problem(loc: &Location, name: &str) -> Option<String> {
    match loc {
        Location::Local(_) => ops::invalid_name(name),
        _ if name.trim().is_empty() => Some("O nome não pode ficar vazio.".into()),
        _ => None,
    }
}

/// Renames to the name typed in place. A name the system won't take keeps the field open and
/// says why.
pub fn commit_rename(s: &mut State) {
    let Some((pane, item)) = s.renaming.clone() else { return };
    let name = s.name_edit.text.trim().to_string();
    if let Some(why) = name_problem(&s.panes[pane].loc, &name) {
        s.status = why;
        return;
    }
    s.renaming = None;
    apply_name(s, pane, Some(item), name);
}

pub fn cancel_rename(s: &mut State) {
    s.renaming = None;
}

pub fn ask_new_folder(s: &mut State, p: usize, _: &mut Tx) {
    if !editable(&s.panes[p].loc) {
        return not_here(s);
    }
    let mut edit = TextEdit::new("Nova pasta");
    edit.set_selection(0, "Nova pasta".len());
    s.name_edit = edit;
    s.dialog = Some(Dialog::NewFolder { pane: p, error: None });
}

/// Confirms Nova pasta with the name typed.
pub fn submit_name(s: &mut State) {
    let name = s.name_edit.text.trim().to_string();
    let Some(Dialog::NewFolder { pane, error }) = &mut s.dialog else { return };
    if let Some(why) = name_problem(&s.panes[*pane].loc, &name) {
        *error = Some(why);
        return;
    }
    let pane = *pane;
    s.dialog = None;
    apply_name(s, pane, None, name);
}

/// Renames `item` to `name`, or with no item creates folder `name`, in pane `pane`'s folder.
pub fn apply_name(s: &mut State, pane: usize, item: Option<Entry>, name: String) {
    if item.as_ref().is_some_and(|i| i.name == name) {
        return;
    }
    let loc = s.panes[pane].loc.clone();
    match (&loc, item) {
        (Location::Local(dir), Some(item)) => {
            let from = dir.join(&item.name);
            local_job(s, vec![loc.clone()], move || ops::rename(&from, &name).map(|_| format!("Renomeado para \"{name}\".")));
        }
        (Location::Local(dir), None) => {
            let dir = dir.clone();
            local_job(s, vec![loc.clone()], move || ops::new_folder(&dir, &name).map(|_| format!("Pasta \"{name}\" criada.")));
        }
        (Location::Drive { .. }, item) => {
            let Some((email, folder)) = drive_folder(&loc) else { return };
            drive_job(s, &email, vec![loc.clone()], move |d| match item {
                Some(item) => d.rename(&item.id, &name).map(|_| format!("Renomeado para \"{name}\".")),
                None => d.create_folder(&name, &folder).map(|_| format!("Pasta \"{name}\" criada.")),
            });
        }
        (Location::Mock(_), _) => not_here(s),
    }
}

// ---------------------------------------------------------------------------------------------
// Properties

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropTab {
    General,
    Details,
    Versions,
}

/// One local item's facts, read as Propriedades opens.
pub struct LocalInfo {
    pub path: PathBuf,
    pub created: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
    pub attributes: Vec<&'static str>,
}

/// What a background task found for the open Propriedades.
pub enum PropUpdate {
    Details(Result<Details, String>),
    Totals(ops::Totals),
    Md5(Result<String, String>),
    Revisions(Result<Vec<Revision>, String>),
}

/// The open Propriedades: what it shows and what's still being worked out.
pub struct Props {
    pub pane: usize,
    pub items: Vec<Entry>,
    pub tab: PropTab,
    /// Numbers this dialog, so answers meant for one closed since are dropped.
    pub id: u64,
    /// Cleared when the dialog closes, so a folder walk or a hash stops early.
    pub alive: Arc<AtomicBool>,
    pub local: Option<LocalInfo>,
    /// Folder contents adding up, for local folders and several items.
    pub totals: Option<ops::Totals>,
    pub md5: Option<Result<String, String>>,
    pub md5_running: bool,
    /// The Tipo row: the items' shared type, or "Vários tipos".
    pub kind: String,
    /// The listed sizes added up (files only, folders' contents not included).
    pub listed_bytes: u64,
    pub details: Option<Result<Details, String>>,
    pub revisions: Option<Result<Vec<Revision>, String>>,
    pub revisions_asked: bool,
}

impl Props {
    pub fn single(&self) -> Option<&Entry> {
        match self.items.as_slice() {
            [e] => Some(e),
            _ => None,
        }
    }

    /// Whether the name field renames: one item, somewhere names can change.
    pub fn renamable(&self, s: &State) -> bool {
        self.single().is_some() && editable(&s.panes[self.pane].loc)
    }

    /// The Histórico de versões tab is for one Drive file (folders have no versions).
    pub fn has_versions(&self, s: &State) -> bool {
        matches!(s.panes[self.pane].loc, Location::Drive { .. }) && self.single().is_some_and(|e| !e.is_dir)
    }
}

fn props_mut(s: &mut State) -> Option<&mut Props> {
    match &mut s.dialog {
        Some(Dialog::Properties(p)) => Some(p),
        _ => None,
    }
}

/// Runs `work` for the open Propriedades; its answer lands only if that dialog is still open.
/// `work` gives what the task found; a panic in it ends that same task with the error (the hash
/// row stops saying "Calculando…"), through `failed`.
fn props_job(s: &State, id: u64, failed: fn(String) -> PropUpdate, work: impl FnOnce() -> PropUpdate + Send + 'static) {
    background(s, move |emit| emit(Msg::Prop { id, update: crate::catch_panic("ler as propriedades", || Ok(work())).unwrap_or_else(failed) }));
}

pub fn properties(s: &mut State, p: usize) {
    let items = selected(s, p);
    if items.is_empty() {
        return;
    }
    close_properties(s);
    s.props_seq += 1;
    let id = s.props_seq;
    let alive = Arc::new(AtomicBool::new(true));
    let loc = s.panes[p].loc.clone();
    let mut local = None;
    if let Location::Local(dir) = &loc {
        // One item: its dates and attributes now (one metadata call). Folders and several
        // items: their contents add up in the background.
        if let [e] = items.as_slice() {
            let path = dir.join(&e.name);
            let meta = std::fs::symlink_metadata(&path).ok();
            local = Some(LocalInfo {
                created: meta.as_ref().and_then(|m| m.created().ok()),
                accessed: meta.as_ref().and_then(|m| m.accessed().ok()),
                attributes: meta.as_ref().map(ops::attributes).unwrap_or_default(),
                path,
            });
        }
        if items.len() > 1 || items.iter().any(|e| e.is_dir) {
            let paths: Vec<PathBuf> = items.iter().map(|e| dir.join(&e.name)).collect();
            let alive = alive.clone();
            props_job(s, id, |_| PropUpdate::Totals(ops::Totals::default()), move || PropUpdate::Totals(ops::totals(&paths, &|| !alive.load(Ordering::Relaxed))));
        }
    }
    // One Drive item: its owner, dates, description and checksum from Drive.
    if let ([item], Some((email, _))) = (items.as_slice(), drive_folder(&loc)) {
        let (item_id, cached) = (item.id.clone(), s.drives.get(&email).cloned());
        props_job(s, id, |e| PropUpdate::Details(Err(e)), move || PropUpdate::Details(cached.map_or_else(|| open_drive(&email), Ok).and_then(|d| d.details(&item_id))));
    }
    if let [item] = items.as_slice() {
        let mut edit = TextEdit::new(item.name.clone());
        edit.set_selection(0, stem_len(&item.name, item.is_dir));
        s.name_edit = edit;
    }
    let first = items[0].kind_label();
    let kind = if items.iter().all(|e| e.kind_label() == first) { first } else { "Vários tipos".into() };
    let listed_bytes = items.iter().filter_map(|e| e.size).sum();
    s.dialog = Some(Dialog::Properties(Box::new(Props {
        pane: p,
        kind,
        listed_bytes,
        items,
        tab: PropTab::General,
        id,
        alive,
        local,
        totals: None,
        md5: None,
        md5_running: false,
        details: None,
        revisions: None,
        revisions_asked: false,
    })));
}

/// An answer for Propriedades number `id`.
pub fn props_update(s: &mut State, id: u64, update: PropUpdate) {
    let Some(p) = props_mut(s).filter(|p| p.id == id) else { return };
    match update {
        PropUpdate::Details(d) => p.details = Some(d),
        PropUpdate::Totals(t) => p.totals = Some(t),
        PropUpdate::Md5(m) => {
            p.md5 = Some(m);
            p.md5_running = false;
        }
        PropUpdate::Revisions(r) => p.revisions = Some(r),
    }
}

pub fn props_tab(s: &mut State, tab: PropTab) {
    let Some(p) = props_mut(s) else { return };
    p.tab = tab;
    // Versions load the first time their tab opens.
    if tab != PropTab::Versions || p.revisions_asked {
        return;
    }
    p.revisions_asked = true;
    let (id, pane) = (p.id, p.pane);
    let Some(item_id) = p.single().map(|e| e.id.clone()) else { return };
    let Location::Drive { email, .. } = &s.panes[pane].loc else { return };
    let (cached, email) = (s.drives.get(email).cloned(), email.clone());
    props_job(s, id, |e| PropUpdate::Revisions(Err(e)), move || PropUpdate::Revisions(cached.map_or_else(|| open_drive(&email), Ok).and_then(|d| d.revisions(&item_id))));
}

/// Works out a local file's MD5 on request: reading a large file takes a while.
pub fn props_md5(s: &mut State) {
    let Some(p) = props_mut(s) else { return };
    let Some(path) = p.local.as_ref().map(|l| l.path.clone()) else { return };
    p.md5_running = true;
    let (id, alive) = (p.id, p.alive.clone());
    props_job(s, id, |e| PropUpdate::Md5(Err(e)), move || PropUpdate::Md5(ops::md5_file(&path, &|| !alive.load(Ordering::Relaxed))));
}

/// OK: renames when the name was changed, then closes.
pub fn props_ok(s: &mut State) {
    let Some(Dialog::Properties(p)) = &s.dialog else { return };
    let rename = p.renamable(s).then(|| (p.pane, p.single().cloned())).and_then(|(pane, e)| e.map(|e| (pane, e)));
    let name = s.name_edit.text.trim().to_string();
    if let Some((pane, item)) = rename
        && name != item.name
    {
        if let Some(why) = name_problem(&s.panes[pane].loc, &name) {
            s.status = why;
            return;
        }
        close_properties(s);
        apply_name(s, pane, Some(item), name);
        return;
    }
    close_properties(s);
}

/// Closes Propriedades and stops what it still had running.
pub fn close_properties(s: &mut State) {
    if let Some(Dialog::Properties(p)) = &s.dialog {
        p.alive.store(false, Ordering::Relaxed);
        s.dialog = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(space: Space, path: &[&str]) -> Location {
        Location::Drive { email: "a@b".into(), space, path: path.iter().map(|id| (id.to_string(), id.to_string())).collect() }
    }

    #[test]
    fn drive_folders_resolve_to_the_id_to_write_into() {
        assert_eq!(drive_folder(&drive(Space::MyDrive, &[])), Some(("a@b".into(), "root".into())));
        assert_eq!(drive_folder(&drive(Space::MyDrive, &["f1", "f2"])), Some(("a@b".into(), "f2".into())));
        let shared = Space::Shared { id: "0AA".into(), name: "Equipe".into() };
        assert_eq!(drive_folder(&drive(shared.clone(), &[])), Some(("a@b".into(), "0AA".into())));
        assert_eq!(drive_folder(&drive(Space::SharedDrives, &[])), None);
    }

    #[test]
    fn rename_selects_the_name_before_its_extension() {
        fn pick(name: &str, dir: bool) -> &str {
            &name[..stem_len(name, dir)]
        }
        assert_eq!(pick("Relatório.pdf", false), "Relatório");
        assert_eq!(pick("arquivo.tar.gz", false), "arquivo.tar");
        assert_eq!(pick(".bashrc", false), ".bashrc");
        assert_eq!(pick("Fotos 2.0", true), "Fotos 2.0");
        assert_eq!(pick("LEIAME", false), "LEIAME");
    }

    #[test]
    fn drive_names_are_not_held_to_windows_rules() {
        assert!(name_problem(&drive(Space::MyDrive, &[]), "Reunião: pauta?").is_none());
        assert!(name_problem(&Location::Local("C:\\".into()), "Reunião: pauta?").is_some());
        assert!(name_problem(&drive(Space::MyDrive, &[]), "  ").is_some());
    }

    #[test]
    fn only_real_folders_are_editable() {
        assert!(editable(&Location::Local("C:\\\\".into())));
        assert!(editable(&drive(Space::MyDrive, &[])));
        assert!(!editable(&drive(Space::SharedDrives, &[])));
        assert!(!editable(&Location::Mock(vec![])));
    }

    #[test]
    fn arrows_copy_the_selection_across_and_keep_the_clip() {
        let root = std::env::temp_dir().join(format!("openexplorer-across-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("nota.txt"), "oi").unwrap();
        let mut s = crate::init();
        s.panes[0] = crate::model::Pane::new(Location::Local(a.clone()));
        s.panes[1] = crate::model::Pane::new(Location::Local(b.clone()));
        for p in &mut s.panes {
            p.load_now(&s.mock);
        }
        s.clip = Some(Clip { mode: ClipMode::Cut, from: Location::Local(b.clone()), items: Vec::new() });

        copy_across(&mut s, 1, 0);
        assert!(s.status.contains("Selecione"), "nothing selected on the right: {}", s.status);

        s.panes[0].selected.insert("nota.txt".into());
        copy_across(&mut s, 0, 1);
        let done = s.inbox.recv_timeout(std::time::Duration::from_secs(10)).expect("the copy answers");
        assert!(matches!(done, Msg::Done { result: Ok(_), .. }));
        assert_eq!(std::fs::read_to_string(b.join("nota.txt")).unwrap(), "oi");
        assert!(a.join("nota.txt").exists(), "a copy leaves the original");
        assert!(matches!(&s.clip, Some(Clip { mode: ClipMode::Cut, .. })), "what Recortar picked up stays");
        let _ = std::fs::remove_dir_all(&root);
    }
}
