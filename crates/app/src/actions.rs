//! What the context menu and its shortcuts do: open, cut, copy, paste, delete, rename, new
//! folder and properties, on the local disk and in Google Drive. The work runs in the
//! background and ends in a [`Msg::Done`] that reloads the folders it touched.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use openexplorer_gdrive::{Details, Drive, DriveFile, Revision, Sharing};
use rune::{TextEdit, Tx};

use crate::model::{Entry, Location, Space};
use crate::transfers::{self, End, Op};
use crate::uploads::{Clash, Existing, Item, Put};
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
    /// Excluir permanentemente, from Lixeira.
    Purge { pane: usize, items: Vec<Entry> },
    /// Esvaziar lixeira of an account.
    EmptyTrash { email: String },
    /// The name being typed is `State::name_edit`.
    NewFolder { pane: usize, error: Option<String> },
    Properties(Box<Props>),
    /// Opções: the theme and transfers at once, applied as they change.
    Options,
    /// Names an upload would repeat in its Drive folder: Substituir, Pular or Manter os dois.
    Clash(Box<ClashAsk>),
    /// Compartilhar: who has access to a Drive item.
    Share(Box<crate::sharing::Share>),
    /// Copiar link on a private item: open it to anyone with the link first?
    MakePublic(Box<crate::sharing::MakePublic>),
}

pub fn selected(s: &State, p: usize) -> Vec<Entry> {
    let pane = &s.panes[p];
    pane.entries.iter().filter(|e| pane.selected.contains(&e.id)).cloned().collect()
}

/// The folder a Drive location shows, to write into: (account, folder id). `None` for the
/// shared drives list and Drive's views (Com estrela...), which aren't folders, and anywhere in
/// Lixeira.
pub(crate) fn drive_folder(loc: &Location) -> Option<(String, String)> {
    let Location::Drive { email, space, path } = loc else { return None };
    let id = match (path.last(), space) {
        (_, Space::Trash) => return None,
        (Some((id, _)), _) => id.clone(),
        (None, Space::MyDrive) => "root".into(),
        (None, Space::Shared { id, .. }) => id.clone(),
        (None, _) => return None,
    };
    Some((email.clone(), id))
}

/// The account of a Drive location.
fn account(loc: &Location) -> Option<&str> {
    match loc {
        Location::Drive { email, .. } => Some(email),
        _ => None,
    }
}

/// Whether `loc` is a folder items can be put in (Colar, Nova pasta) and taken out of
/// (Recortar): not the shared drives list, Drive's views or Lixeira, nor the simulated drive.
pub fn editable(loc: &Location) -> bool {
    match loc {
        Location::Local(_) => true,
        Location::Mock(_) => false,
        Location::Drive { .. } => drive_folder(loc).is_some(),
    }
}

/// Whether the items `loc` lists can be renamed, copied, starred or sent to the trash: also in
/// Compartilhados comigo, Com estrela and Recentes, which list real files; not the shared drives
/// themselves, nor what's already in Lixeira.
pub fn items_editable(loc: &Location) -> bool {
    match loc {
        Location::Drive { space: Space::SharedDrives | Space::Trash, .. } => false,
        Location::Drive { .. } => true,
        _ => editable(loc),
    }
}

/// Lixeira itself, where Restaurar and Excluir permanentemente apply (not a trashed folder's
/// contents, which come back with it).
pub fn in_trash(loc: &Location) -> bool {
    matches!(loc, Location::Drive { space: Space::Trash, path, .. } if path.is_empty())
}

/// Why `loc` doesn't take what was asked (pasting, a new folder, cutting).
fn why_not(loc: &Location) -> String {
    match loc {
        Location::Drive { space: Space::SharedDrives, .. } => "Drives compartilhados lista os drives: abra um deles para trabalhar nas pastas.".into(),
        Location::Drive { space: Space::Trash, path, .. } if path.is_empty() => "A Lixeira não recebe itens: use Restaurar ou Excluir permanentemente.".into(),
        Location::Drive { space: Space::Trash, .. } => "Esta pasta está na lixeira: restaure-a para mexer no que tem dentro.".into(),
        Location::Drive { space, path, .. } if path.is_empty() => {
            format!("{} é uma lista do Google Drive, não uma pasta: abra uma pasta para colar, criar pastas ou recortar.", space.label())
        }
        Location::Mock(_) => "O drive simulado só serve para testar a interface; abra o disco local ou o Google Drive.".into(),
        _ => "Não dá para fazer isso aqui.".into(),
    }
}

fn not_here(s: &mut State, p: usize) {
    s.status = why_not(&s.panes[p].loc);
}

/// Every location the panes show in `email`'s Drive, to reload after a change whose effects
/// reach past one folder: a restored file goes back to its folder, a star shows in Com estrela.
fn account_panes(s: &State, email: &str) -> Vec<Location> {
    s.panes.iter().map(|p| p.loc.clone()).filter(|l| account(l) == Some(email)).collect()
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

pub fn open(s: &mut State, p: usize, _: &mut Tx) {
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
    open_file(s, p, item);
}

/// Opens a file with the program Windows associates with it. A Drive file is downloaded to a
/// temp folder first, as a row of the queue, and opens when it arrives; a file in one of
/// Google's own formats opens in the browser instead, where editing it changes the original.
pub fn open_file(s: &mut State, p: usize, item: &Entry) {
    let loc = s.panes[p].loc.clone();
    match &loc {
        Location::Local(dir) => {
            if let Err(e) = ops::open(&dir.join(&item.name)) {
                s.status = e;
            }
        }
        Location::Drive { .. } if item.mime.as_deref().is_some_and(|m| m.starts_with("application/vnd.google-apps.")) => {
            // ShellExecute opens a URL in the default browser.
            if let Err(e) = ops::open(std::path::Path::new(&format!("https://drive.google.com/open?id={}", item.id))) {
                s.status = format!("Não consegui abrir o navegador: {e}");
            }
        }
        Location::Drive { .. } => {
            // Each file in its own folder, so two files of the same name don't meet. A copy
            // already there from an earlier Abrir, as big as the file and as new, opens at once.
            let dir = std::env::temp_dir().join("OpenExplorer-abrir").join(ops::windows_name(&item.id));
            let ready = dir.join(ops::windows_name(&item.name));
            let fresh = std::fs::metadata(&ready).ok().is_some_and(|m| Some(m.len()) == item.size && item.modified.is_some() && m.modified().ok() == item.modified);
            if fresh {
                if let Err(e) = ops::open(&ready) {
                    s.status = e;
                }
                return;
            }
            if let Err(e) = std::fs::create_dir_all(&dir) {
                s.status = format!("Não consegui criar a pasta temporária: {e}");
                return;
            }
            // An older copy goes, so the new one takes its name.
            let _ = std::fs::remove_file(&ready);
            download(s, &loc, vec![item.clone()], dir, Then::Open, Vec::new());
        }
        Location::Mock(_) => not_here(s, p),
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
    // A cut needs the folder the items leave; a copy only the items.
    let ok = if mode == ClipMode::Cut { editable(&loc) } else { items_editable(&loc) };
    if !ok {
        return not_here(s, p);
    }
    s.status = format!("{} {} — use Colar (Ctrl+V) na pasta de destino.", if mode == ClipMode::Cut { "Recortado:" } else { "Copiado:" }, count(items.len()));
    s.clip = Some(Clip { mode, from: loc, items });
}

/// Whether items of `from` can be copied or moved into `to` as it is: same disk, same Drive
/// account, from the computer up to a Drive folder, or from a Drive folder down to the computer.
fn fits(from: &Location, to: &Location) -> bool {
    match (from, to) {
        (Location::Local(_), Location::Local(_)) => true,
        (Location::Local(_), Location::Drive { .. }) => drive_folder(to).is_some(),
        (Location::Drive { .. }, Location::Local(_)) => drive_folder(from).is_some(),
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
    if !items_editable(&loc) {
        return match loc {
            Location::Mock(_) => not_here(s, from),
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
    if matches!(to, Location::Drive { .. }) && drive_folder(&to).is_none() {
        return not_here(s, p);
    }
    if !fits(&from, &to) {
        s.status = match (&from, &to) {
            (Location::Local(_), Location::Drive { .. }) => "Abra uma pasta do Drive (ou um drive compartilhado) para enviar para ela.".into(),
            (Location::Drive { .. }, Location::Drive { .. }) if drive_folder(&to).is_some() => "Colar entre contas diferentes do Google Drive ainda não é possível.".into(),
            _ => "Não dá para colar aqui.".into(),
        };
        return;
    }
    let reload = vec![to.clone(), from.clone()];
    let op = if mode == ClipMode::Cut { Op::Move } else { Op::Copy };
    match (&from, &to) {
        (Location::Local(src), Location::Local(dst)) => {
            let sources = items.iter().map(|e| src.join(&e.name)).collect();
            local_transfer(s, op, sources, dst.clone(), reload);
        }
        (Location::Local(src), Location::Drive { .. }) => {
            let sources = items.iter().map(|e| src.join(&e.name)).collect();
            upload(s, sources, &to, mode == ClipMode::Cut, reload);
        }
        (Location::Drive { .. }, Location::Local(dst)) => {
            let then = if mode == ClipMode::Cut { Then::Trash } else { Then::Nothing };
            download(s, &from, items, dst.clone(), then, reload);
        }
        _ => {
            // Copied out of a view (Com estrela...), the items have no one folder they come from.
            let src = drive_folder(&from).map(|(_, id)| id).unwrap_or_default();
            let (Some(email), Some((_, dst))) = (account(&from).map(str::to_string), drive_folder(&to)) else { return };
            let ends = items.iter().map(|e| (End::drive(&from, Some(&e.name)), End::drive(&to, None)));
            let batch = transfers::queue(&mut s.transfers, s.waker.clone(), op, ends);
            // Drive copies on its side: a file's bar fills when it's done; a folder's size isn't
            // known.
            for (p, e) in batch.rows().iter().zip(&items) {
                if let (false, Some(size)) = (e.is_dir, e.size) {
                    p.size(size);
                }
            }
            drive_job(s, &email, reload, move |d| {
                batch.run(|i, _| {
                    let e = &items[i];
                    // Shortcuts are copied as shortcuts, not followed into their target.
                    let file = DriveFile { is_dir: e.is_dir && e.target.is_none(), target: None, ..drive_file(e) };
                    match mode {
                        ClipMode::Copy if src == dst => d.copy(&file, &dst, &format!("Cópia de {}", e.name)).map(|_| ()),
                        ClipMode::Copy => d.copy(&file, &dst, &e.name).map(|_| ()),
                        ClipMode::Cut if src == dst => Ok(()),
                        ClipMode::Cut if src.is_empty() => Err("Recortar precisa da pasta de origem.".into()),
                        ClipMode::Cut => d.move_to(&e.id, &src, &dst).map(|_| ()),
                    }
                })
            });
        }
    }
}

/// Copies or moves `sources` into the folder `dst` on a background thread, a row each in the
/// transfer queue: first every size is added up, then each item goes, chunk by chunk.
pub fn local_transfer(s: &mut State, op: Op, sources: Vec<PathBuf>, dst: PathBuf, reload: Vec<Location>) {
    let to = End::local(&dst);
    let batch = transfers::queue(&mut s.transfers, s.waker.clone(), op, sources.iter().map(|p| (End::local(p), to.clone())));
    local_job(s, reload, move || {
        for (p, path) in batch.rows().iter().zip(&sources) {
            p.size(ops::measure(path, &|| p.cancelled()));
        }
        batch.run(|i, p| {
            let moved = match op {
                Op::Copy | Op::Upload | Op::Download => ops::copy_into(&sources[i], &dst, p),
                Op::Move => ops::move_into(&sources[i], &dst, p),
            };
            moved.map(|_| ())
        })
    });
}

/// An upload waiting for its names to be checked against the Drive folder it goes into.
pub struct Upload {
    pub email: String,
    /// The Drive folder's id, and where it is.
    pub parent: String,
    pub to: Location,
    pub sources: Vec<PathBuf>,
    pub remove: bool,
    pub reload: Vec<Location>,
}

/// Uploads `sources` (files and folders on the computer) into the Drive folder `to` shows, a
/// row each in the queue. With `remove` (Recortar), each original goes to the Recycle Bin once
/// all of it arrived.
///
/// Names are checked first against what the folder holds: the listing a pane shows of it
/// when it's complete, else a fresh one. When any is taken, the clash dialog asks what to do
/// before anything starts.
pub fn upload(s: &mut State, sources: Vec<PathBuf>, to: &Location, remove: bool, reload: Vec<Location>) {
    let Some((email, parent)) = drive_folder(to) else {
        s.status = "Abra uma pasta do Drive (ou um drive compartilhado) para enviar para ela.".into();
        return;
    };
    let up = Upload { email: email.clone(), parent: parent.clone(), to: to.clone(), sources, remove, reload };
    if let Some(pane) = s.panes.iter().find(|p| p.loc == *to && !p.loading && p.error.is_none()) {
        let existing = pane.entries.clone();
        return ask_clashes(s, up, existing);
    }
    let (cached, outbox, wake) = (s.drives.get(&email).cloned(), s.outbox.clone(), s.waker.clone());
    std::thread::spawn(move || {
        let (made, result) = match cached.map_or_else(|| crate::open_drive(&email).map(|d| (Some(d.clone()), d)), |d| Ok((None, d))) {
            Ok((made, d)) => {
                let mut out = Vec::new();
                let listed = d.list(&parent, None, |page, _| {
                    out.extend(page.into_iter().map(crate::entry_of));
                    true
                });
                (made.map(|d| (email, d)), listed.map(|_| out))
            }
            Err(e) => (None, Err(e)),
        };
        let _ = outbox.send(Msg::Clashes { up, made, result });
        if let Some(w) = wake {
            w();
        }
    });
}

/// The Drive folder's listing for an upload arrived.
pub fn clashes_listed(s: &mut State, up: Upload, result: Result<Vec<Entry>, String>) {
    match result {
        Ok(existing) => ask_clashes(s, up, existing),
        Err(e) => s.status = format!("Não consegui ver o que já existe no destino: {e}"),
    }
}

/// An upload whose names are taken in its Drive folder, waiting for Substituir, Pular or
/// Manter os dois.
pub struct ClashAsk {
    pub up: Upload,
    /// What the folder holds.
    pub existing: Vec<Entry>,
    /// The sources whose names are taken, by position.
    pub clashes: Vec<usize>,
    /// The choices made so far, for the first of `clashes`.
    pub chosen: Vec<Clash>,
    /// Aplicar a todos: the next choice goes for every clash left.
    pub all: bool,
}

impl ClashAsk {
    /// The source the dialog asks about now, by position.
    pub fn current(&self) -> usize {
        self.clashes[self.chosen.len().min(self.clashes.len() - 1)]
    }

    /// What's already in the folder under `name`.
    pub fn there(&self, name: &str) -> Option<&Entry> {
        self.existing.iter().find(|e| e.name == name)
    }
}

fn file_name(p: &std::path::Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Starts `up` at once if none of its names is taken in `existing`, else asks: in the clash
/// dialog, or after the one open now.
fn ask_clashes(s: &mut State, up: Upload, existing: Vec<Entry>) {
    let names: Vec<String> = up.sources.iter().map(|p| file_name(p)).collect();
    let found: Vec<Existing> = existing.iter().map(Existing::of_entry).collect();
    let clashes = crate::uploads::clashes(&names, &found);
    if clashes.is_empty() {
        return start_upload(s, up, &found, &[]);
    }
    let ask = ClashAsk { up, existing, clashes, chosen: Vec::new(), all: true };
    if s.dialog.is_none() {
        s.dialog = Some(Dialog::Clash(Box::new(ask)));
    } else {
        s.clash_waiting.push_back(ask);
    }
}

/// Aplicar a todos, ticked or not.
pub fn toggle_clash_all(s: &mut State) {
    if let Some(Dialog::Clash(ask)) = &mut s.dialog {
        ask.all = !ask.all;
    }
}

/// An answer in the clash dialog: a choice for the item it asks about (for all left, with
/// Aplicar a todos), or `None` for Cancelar, which drops the whole upload. Once every clash
/// has its choice the upload starts.
pub fn answer_clash(s: &mut State, choice: Option<Clash>) {
    let Some(Dialog::Clash(mut ask)) = s.dialog.take() else { return };
    let Some(c) = choice else {
        s.status = "Envio cancelado.".into();
        return next_clash(s);
    };
    let n = if ask.all { ask.clashes.len() - ask.chosen.len() } else { 1 };
    ask.chosen.extend(std::iter::repeat_n(c, n));
    if ask.chosen.len() < ask.clashes.len() {
        s.dialog = Some(Dialog::Clash(ask));
        return;
    }
    let found: Vec<Existing> = ask.existing.iter().map(Existing::of_entry).collect();
    let choices: Vec<(usize, Clash)> = ask.clashes.iter().copied().zip(ask.chosen.iter().copied()).collect();
    start_upload(s, ask.up, &found, &choices);
    next_clash(s);
}

/// Opens the clash dialog of the next upload waiting for one, if the window has no dialog.
pub fn next_clash(s: &mut State) {
    if s.dialog.is_none()
        && let Some(ask) = s.clash_waiting.pop_front()
    {
        s.dialog = Some(Dialog::Clash(Box::new(ask)));
    }
}

/// Queues `up`'s rows and starts it, each taken name settled by its entry in `choices`
/// (source position, choice). Items skipped get no row.
fn start_upload(s: &mut State, up: Upload, existing: &[Existing], choices: &[(usize, Clash)]) {
    let choice = |i: usize| choices.iter().find(|(j, _)| *j == i).map_or(Clash::KeepBoth, |(_, c)| *c);
    let local: Vec<(String, bool)> = up.sources.iter().map(|p| (file_name(p), p.is_dir())).collect();
    let puts = crate::uploads::resolve(&local, existing, choice);
    let mut items = Vec::new();
    let mut skipped = 0;
    for (i, (path, put)) in up.sources.into_iter().zip(puts).enumerate() {
        if put == Put::Skip {
            skipped += 1;
        } else {
            items.push(Item { path, put, clash: choice(i) });
        }
    }
    if items.is_empty() {
        s.status = format!("Nada enviado: {} já existe{} no destino.", count(skipped), if skipped == 1 { "" } else { "m" });
        return;
    }
    let dest = End::drive(&up.to, None);
    let batch = transfers::queue(&mut s.transfers, s.waker.clone(), Op::Upload, items.iter().map(|it| (End::local(&it.path), dest.clone())));
    let at_once = s.settings.uploads.into();
    let (parent, remove) = (up.parent, up.remove);
    drive_job(s, &up.email, up.reload, move |d| {
        let summary = crate::uploads::run(d, &batch, &items, &parent, remove, at_once);
        match summary {
            Ok(line) if skipped > 0 => Ok(format!("{line} Pulados (já existiam): {skipped}.")),
            other => other,
        }
    });
}

/// A listed Drive item as the Drive client takes it.
pub fn drive_file(e: &Entry) -> DriveFile {
    DriveFile {
        id: e.id.clone(),
        name: e.name.clone(),
        is_dir: e.is_dir,
        size: e.size,
        modified: e.modified,
        mime: e.mime.clone().unwrap_or_default(),
        target: e.target.clone(),
        md5: e.md5.clone(),
        starred: e.starred,
    }
}

/// What happens once a download row's files all arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Then {
    Nothing,
    /// Recortar and Colar: the original goes to the Drive trash, from where it can be restored.
    Trash,
    /// Abrir: the file opens with its program.
    Open,
}

/// Downloads `items` of the Drive folder `from` into the folder `dst` on the computer, a row
/// each in the queue; then `then`.
pub fn download(s: &mut State, from: &Location, items: Vec<Entry>, dst: PathBuf, then: Then, reload: Vec<Location>) {
    let Some((email, _)) = drive_folder(from) else {
        s.status = "Não dá para baixar daqui.".into();
        return;
    };
    let to = End::local(&dst);
    let batch = transfers::queue(&mut s.transfers, s.waker.clone(), Op::Download, items.iter().map(|e| (End::drive(from, Some(&e.name)), to.clone())));
    let files: Vec<DriveFile> = items.iter().map(drive_file).collect();
    let (at_once, segmented) = (s.settings.downloads.into(), s.settings.segmented_downloads);
    drive_job(s, &email, reload, move |d| {
        let finish = |i: usize, path: &std::path::Path| match then {
            Then::Nothing => Ok(()),
            Then::Trash => d.trash(&files[i].id).map(|_| ()),
            Then::Open => ops::open(path),
        };
        crate::downloads::run(d, &batch, &files, &dst, at_once, segmented, &finish)
    });
}

/// Cancelar on a row of the queue.
pub fn cancel_transfer(row: &ops::Progress) {
    row.cancel();
}

/// Cancelar todas: every row not over yet.
pub fn cancel_all_transfers(s: &mut State) {
    for t in &s.transfers {
        t.progress.cancel();
    }
}

/// Takes a row off the list, once it's over.
pub fn remove_transfer(s: &mut State, row: &Arc<ops::Progress>) {
    s.transfers.retain(|t| !(Arc::ptr_eq(&t.progress, row) && t.progress.over()));
}

/// Takes every row that's over off the list.
pub fn clear_transfers(s: &mut State) {
    s.transfers.retain(|t| !t.progress.over());
}

// ---------------------------------------------------------------------------------------------
// Delete

pub fn ask_delete(s: &mut State, p: usize) {
    let items = selected(s, p);
    if items.is_empty() {
        return;
    }
    // In Lixeira, Del deletes for good, after asking.
    if in_trash(&s.panes[p].loc) {
        s.dialog = Some(Dialog::Purge { pane: p, items });
        return;
    }
    if !items_editable(&s.panes[p].loc) {
        return not_here(s, p);
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
        Location::Drive { email, .. } => {
            // Lixeira, if a pane shows it, gets the items too.
            drive_job(s, email, account_panes(s, email), move |d| {
                for e in &items {
                    d.trash(&e.id)?;
                }
                Ok(format!("Movido para a lixeira do Google Drive: {}.", count(n)))
            });
        }
        Location::Mock(_) => return not_here(s, pane),
    }
    s.status = format!("Excluindo {}…", count(n));
}

// ---------------------------------------------------------------------------------------------
// Lixeira and stars

/// Restaurar: the selected items in Lixeira go back where they were.
pub fn restore(s: &mut State, p: usize) {
    let items = selected(s, p);
    let loc = s.panes[p].loc.clone();
    let (false, true, Some(email)) = (items.is_empty(), in_trash(&loc), account(&loc)) else { return };
    let n = items.len();
    drive_job(s, email, account_panes(s, email), move |d| {
        for e in &items {
            d.restore(&e.id)?;
        }
        Ok(format!("Restaurado: {}.", count(n)))
    });
    s.status = format!("Restaurando {}…", count(n));
}

/// Excluir permanentemente: asks first, as it can't be undone.
pub fn ask_purge(s: &mut State, p: usize) {
    let items = selected(s, p);
    if !items.is_empty() && in_trash(&s.panes[p].loc) {
        s.dialog = Some(Dialog::Purge { pane: p, items });
    }
}

/// The confirmed Excluir permanentemente.
pub fn purge(s: &mut State) {
    let Some(Dialog::Purge { pane, items }) = s.dialog.take() else { return };
    let loc = s.panes[pane].loc.clone();
    let Some(email) = account(&loc) else { return };
    let n = items.len();
    drive_job(s, email, account_panes(s, email), move |d| {
        for (i, e) in items.iter().enumerate() {
            // Say how far it got: what went before an error is gone for good.
            d.delete_forever(&e.id).map_err(|err| if i == 0 { err } else { format!("{err} (já excluídos para sempre: {})", count(i)) })?;
        }
        Ok(format!("Excluído permanentemente: {}.", count(n)))
    });
    s.status = format!("Excluindo permanentemente {}…", count(n));
}

/// Esvaziar lixeira: asks first, as it can't be undone.
pub fn ask_empty_trash(s: &mut State, p: usize) {
    let loc = &s.panes[p].loc;
    if let (true, Some(email)) = (in_trash(loc), account(loc)) {
        s.dialog = Some(Dialog::EmptyTrash { email: email.to_string() });
    }
}

/// The confirmed Esvaziar lixeira: Drive deletes for good everything in the account's My Drive
/// trash.
pub fn empty_trash(s: &mut State) {
    let Some(Dialog::EmptyTrash { email }) = s.dialog.take() else { return };
    drive_job(s, &email, account_panes(s, &email), |d| d.empty_trash().map(|_| "Lixeira esvaziada.".into()));
    s.status = "Esvaziando a lixeira…".into();
}

/// Whether the star item adds stars: unless every selected item has one already.
pub fn stars_to_add(s: &State, p: usize) -> bool {
    !selected(s, p).iter().all(|e| e.starred)
}

/// Adicionar estrela or Remover estrela on the selection, as [`stars_to_add`] says.
pub fn star(s: &mut State, p: usize) {
    let items = selected(s, p);
    let loc = s.panes[p].loc.clone();
    let (false, true, Some(email)) = (items.is_empty(), items_editable(&loc), account(&loc)) else { return };
    let on = stars_to_add(s, p);
    let n = items.len();
    drive_job(s, email, account_panes(s, email), move |d| {
        for e in &items {
            d.set_starred(&e.id, on)?;
        }
        Ok(format!("{}: {}.", if on { "Estrela adicionada" } else { "Estrela removida" }, count(n)))
    });
}

// ---------------------------------------------------------------------------------------------
// Rename, new folder

/// Starts renaming the selected item in place, as Explorer does: its name turns into a field
/// with the part before the extension selected. Enter or a click elsewhere renames, Esc cancels.
pub fn ask_rename(s: &mut State, p: usize, tx: &mut Tx) {
    let items = selected(s, p);
    let [item] = items.as_slice() else { return };
    if !items_editable(&s.panes[p].loc) {
        return not_here(s, p);
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
        return not_here(s, p);
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
        (Location::Drive { email, .. }, Some(item)) => {
            drive_job(s, email, account_panes(s, email), move |d| d.rename(&item.id, &name).map(|_| format!("Renomeado para \"{name}\".")));
        }
        (Location::Drive { .. }, None) => {
            let Some((email, folder)) = drive_folder(&loc) else { return not_here(s, pane) };
            drive_job(s, &email, vec![loc.clone()], move |d| d.create_folder(&name, &folder).map(|_| format!("Pasta \"{name}\" criada.")));
        }
        (Location::Mock(_), _) => not_here(s, pane),
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
    Sharing(Result<Sharing, String>),
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
    /// Who a Drive item is shared with.
    pub sharing: Option<Result<Sharing, String>>,
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
        self.single().is_some() && items_editable(&s.panes[self.pane].loc)
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
    if let ([item], Some(email)) = (items.as_slice(), account(&loc).map(str::to_string)) {
        let (item_id, cached) = (item.id.clone(), s.drives.get(&email).cloned());
        let (sharing_id, sharing_drive, sharing_email) = (item_id.clone(), cached.clone(), email.clone());
        props_job(s, id, |e| PropUpdate::Details(Err(e)), move || PropUpdate::Details(cached.map_or_else(|| open_drive(&email), Ok).and_then(|d| d.details(&item_id))));
        props_job(s, id, |e| PropUpdate::Sharing(Err(e)), move || {
            PropUpdate::Sharing(sharing_drive.map_or_else(|| open_drive(&sharing_email), Ok).and_then(|d| d.sharing(&sharing_id)))
        });
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
        sharing: None,
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
        PropUpdate::Sharing(r) => p.sharing = Some(r),
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
        // Drive's views aren't folders, but a folder opened from one is; nothing in Lixeira is.
        for view in [Space::SharedWithMe, Space::Starred, Space::Recent, Space::Trash] {
            assert_eq!(drive_folder(&drive(view, &[])), None);
        }
        assert_eq!(drive_folder(&drive(Space::Starred, &["f1"])), Some(("a@b".into(), "f1".into())));
        assert_eq!(drive_folder(&drive(Space::Trash, &["f1"])), None);
    }

    #[test]
    fn views_take_no_paste_or_new_folder_and_say_why() {
        let mut s = crate::init();
        s.cache = crate::cache::Cache::memory_only();
        s.panes[1].go(drive(Space::Starred, &[]));
        ask_new_folder(&mut s, 1, &mut Tx::default());
        assert!(s.dialog.is_none());
        assert!(s.status.starts_with("Com estrela é uma lista do Google Drive"), "{}", s.status);
        s.clip = Some(Clip { mode: ClipMode::Copy, from: drive(Space::MyDrive, &[]), items: vec![] });
        s.status.clear();
        paste(&mut s, 1);
        assert!(s.status.starts_with("Com estrela é uma lista"), "{}", s.status);
        assert!(s.transfers.is_empty());
        s.panes[1].go(drive(Space::Trash, &[]));
        paste(&mut s, 1);
        assert!(s.status.contains("Restaurar"), "{}", s.status);
        // Items in a view can still be copied, starred, renamed and trashed; in Lixeira not.
        assert!(items_editable(&drive(Space::Recent, &[])) && !editable(&drive(Space::Recent, &[])));
        assert!(!items_editable(&drive(Space::Trash, &[])));
        assert!(in_trash(&drive(Space::Trash, &[])) && !in_trash(&drive(Space::Trash, &["f1"])));
    }

    #[test]
    fn lixeira_asks_before_deleting_for_good() {
        let mut s = crate::init();
        s.cache = crate::cache::Cache::memory_only();
        s.panes[1].go(drive(Space::Trash, &[]));
        let e = |id: &str| Entry { id: id.into(), name: id.into(), is_dir: false, size: None, modified: None, target: None, mime: None, md5: None, starred: false };
        s.panes[1].set_entries(Ok(vec![e("a"), e("b")]));
        s.panes[1].selected = ["a".to_string()].into();
        // Del in Lixeira opens Excluir permanentemente, not the move to the trash.
        ask_delete(&mut s, 1);
        assert!(matches!(&s.dialog, Some(Dialog::Purge { pane: 1, items }) if items.len() == 1));
        s.dialog = None;
        ask_empty_trash(&mut s, 1);
        assert!(matches!(&s.dialog, Some(Dialog::EmptyTrash { email }) if email == "a@b"));
        // Elsewhere neither applies.
        s.dialog = None;
        s.panes[1].go(drive(Space::MyDrive, &[]));
        ask_empty_trash(&mut s, 1);
        assert!(s.dialog.is_none());
    }

    #[test]
    fn the_star_item_adds_unless_all_have_one() {
        let mut s = crate::init();
        s.panes[1].go(drive(Space::MyDrive, &[]));
        let e = |id: &str, starred| Entry { id: id.into(), name: id.into(), is_dir: false, size: None, modified: None, target: None, mime: None, md5: None, starred };
        s.panes[1].set_entries(Ok(vec![e("a", true), e("b", false)]));
        s.panes[1].selected = ["a".to_string()].into();
        assert!(!stars_to_add(&s, 1));
        s.panes[1].selected.insert("b".into());
        assert!(stars_to_add(&s, 1));
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
        // Its row in the queue sized the file, filled up and is done.
        let [t] = s.transfers.as_slice() else { panic!("one transfer: {}", s.transfers.len()) };
        assert_eq!((t.op, &t.from, &t.to), (Op::Copy, &End::local(&a.join("nota.txt")), &End::local(&b)));
        let p = &t.progress;
        assert_eq!(p.stage(), crate::ops::Stage::Done);
        assert_eq!((p.bytes.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed)), (2, 2));
        assert_eq!(std::fs::read_to_string(b.join("nota.txt")).unwrap(), "oi");
        assert!(a.join("nota.txt").exists(), "a copy leaves the original");
        assert!(matches!(&s.clip, Some(Clip { mode: ClipMode::Cut, .. })), "what Recortar picked up stays");
        let _ = std::fs::remove_dir_all(&root);
    }
}
