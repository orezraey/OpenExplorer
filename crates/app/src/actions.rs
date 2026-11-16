//! What the context menu and its shortcuts do: open, cut, copy, paste, delete, rename, new
//! folder and properties, on the local disk and in Google Drive. The work runs in the
//! background and ends in a [`Msg::Done`] that reloads the folders it touched.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use openexplorer_gdrive::{Details, Drive, DriveFile, Revision, Sharing};
use rune::{TextEdit, Tx};

use crate::address::{self, Reached, Target};
use crate::model::{Entry, Location, Space, shown, typed};
use crate::transfers::{self, End, Op};
use crate::uploads::{Clash, Existing, Item, Put};
use crate::{Msg, State, background, drive_task, open as open_entry, ops};

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
        Location::Mock(_) | Location::Found(_) => false,
        Location::Drive { .. } => drive_folder(loc).is_some(),
    }
}

/// Whether the items `loc` lists can be renamed, copied, starred or sent to the trash: also in
/// Compartilhados comigo, Com estrela and Recentes, which list real files; not the shared drives
/// themselves, nor what's already in Lixeira.
pub fn items_editable(loc: &Location) -> bool {
    match loc {
        Location::Drive { space: Space::SharedDrives | Space::Trash, .. } => false,
        Location::Drive { .. } | Location::Found(_) => true,
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
        Location::Drive { space: Space::Search(_), path, .. } if path.is_empty() => {
            "Os resultados da pesquisa não são uma pasta: abra uma pasta para colar, criar pastas ou recortar.".into()
        }
        Location::Drive { space, path, .. } if path.is_empty() => {
            format!("{} é uma lista do Google Drive, não uma pasta: abra uma pasta para colar, criar pastas ou recortar.", space.label())
        }
        Location::Mock(_) => "O drive simulado só serve para testar a interface; abra o disco local ou o Google Drive.".into(),
        Location::Found(_) => "Os resultados da pesquisa não são uma pasta: abra uma pasta para colar, criar pastas ou recortar.".into(),
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
    drive_task(s, email, "operar no Drive", work, move |result| Msg::Done { reload, result });
}

fn local_job(s: &State, reload: Vec<Location>, work: impl FnOnce() -> Result<String, String> + Send + 'static) {
    background(s, move |emit| emit(Msg::Done { reload, result: crate::catch_panic("operar no disco", work) }));
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
        Location::Local(_) | Location::Found(_) => {
            if let Some(path) = loc.local_path(item)
                && let Err(e) = ops::open(&path)
            {
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

/// Whether `loc` lists search results, whose items Mostrar na pasta takes to their folder.
pub fn shows_results(loc: &Location) -> bool {
    matches!(loc, Location::Found(_)) || matches!(loc, Location::Drive { space: Space::Search(_), path, .. } if path.is_empty())
}

/// Mostrar na pasta: goes from a search's results to the folder the selected item sits in, and
/// selects it there. A Drive item's folder is looked for first, climbing through its parents.
pub fn show_in_folder(s: &mut State, p: usize, _: &mut Tx) {
    let items = selected(s, p);
    let [item] = items.as_slice() else { return };
    let loc = s.panes[p].loc.clone();
    match &loc {
        Location::Found(_) => {
            let path = PathBuf::from(&item.id);
            let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
                s.status = format!("“{}” é a raiz de um disco: não fica em pasta nenhuma.", item.name);
                return;
            };
            let name = name.to_string_lossy().into_owned();
            arrive(s, p, Reached { loc: Location::Local(dir.to_path_buf()), reveal: Some((name.clone(), name)) });
        }
        Location::Drive { email, .. } if shows_results(&loc) => {
            s.status = format!("Procurando a pasta de “{}”…", shown(&item.name));
            let (account, item) = (email.clone(), (item.id.clone(), item.name.clone()));
            let find = move |d: &Drive| d.place(&item.0).map(|place| Reached { loc: address::place_location(account, place), reveal: Some(item) });
            reach(s, p, email, "achar a pasta", find);
        }
        _ => {}
    }
}

/// Takes pane `p` to `to` as a step Voltar undoes, selecting what `to` reveals once it's
/// listed.
pub fn arrive(s: &mut State, p: usize, to: Reached) {
    crate::navigate(s, p, to.loc);
    s.panes[p].reveal = to.reveal;
}

/// Looks for where pane `p` goes with `email`'s Drive in the background; the pane goes there
/// unless it moved on meanwhile. `doing` names the work in a panic's error.
fn reach(s: &State, p: usize, email: &str, doing: &'static str, find: impl FnOnce(&Drive) -> Result<Reached, String> + Send + 'static) {
    let moves = s.panes[p].moves;
    drive_task(s, email, doing, find, move |result| Msg::Reached { pane: p, moves, result });
}

// ---------------------------------------------------------------------------------------------
// Address bar

/// Turns pane `p`'s path bar into a field holding its address, all selected (a click on the
/// bar's empty part, Ctrl+L, Alt+D or F4, as in Explorer).
pub fn edit_address(s: &mut State, p: usize) {
    let text = address::text(&s.panes[p].loc);
    let mut edit = TextEdit::new(text.clone());
    edit.set_selection(0, text.len());
    edit.focused = true;
    s.address[p] = Some(edit);
    s.active = p;
}

/// Goes where `typed` leads from pane `p`: a folder, or a file's folder with the file selected.
/// Looking it up can take a while (a network drive, Drive), so it runs in the background.
/// `false` when `typed` can't be read at all: the address field, if open, stays to fix it.
pub fn go_to_address(s: &mut State, p: usize, typed: &str) -> bool {
    let accounts: Vec<String> = s.accounts.iter().map(|a| a.email.clone()).collect();
    let target = match address::parse(typed, &s.panes[p].loc, &accounts) {
        Ok(t) => t,
        Err(e) => {
            s.status = e;
            return false;
        }
    };
    s.address[p] = None;
    match target {
        Target::Mock(segs) => crate::navigate(s, p, Location::Mock(segs)),
        Target::Local(path) => {
            let moves = s.panes[p].moves;
            background(s, move |emit| emit(Msg::Reached { pane: p, moves, result: crate::catch_panic("abrir o endereço", || address::reach_local(&path)) }));
        }
        Target::Drive { email, way } => {
            let account = email.clone();
            reach(s, p, &email, "abrir o endereço", move |d| address::reach_drive(d, account, way));
        }
    }
    true
}

/// Copiar endereço: what the bar shows, as text.
pub fn copy_address(s: &mut State, p: usize, tx: &mut Tx) {
    tx.copy(address::text(&s.panes[p].loc));
    s.status = "Endereço copiado.".into();
}

/// Copiar link do Drive: the drive.google.com link of the place pane `p` shows.
pub fn copy_drive_link(s: &mut State, p: usize, tx: &mut Tx) {
    if let Some(link) = address::drive_link(&s.panes[p].loc) {
        tx.copy(link);
        s.status = "Link do Drive copiado.".into();
    }
}

/// Colar e ir: goes where the clipboard's text leads.
pub fn paste_and_go(_: &mut State, p: usize, tx: &mut Tx) {
    tx.read_clipboard(move |s: &mut State, content: &rune::ClipboardContent, _: &mut Tx| {
        go_to_address(s, p, content.text());
    });
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
        (Location::Local(_) | Location::Found(_), Location::Local(_)) => true,
        (Location::Local(_) | Location::Found(_), Location::Drive { .. }) => drive_folder(to).is_some(),
        // Downloads only need the items, so any listing of them will do (Com estrela...),
        // except the shared drives list and Lixeira.
        (Location::Drive { .. }, Location::Local(_)) => items_editable(from),
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
            (Location::Local(_) | Location::Found(_), Location::Drive { .. }) => "Abra uma pasta do Drive (ou um drive compartilhado) para enviar para ela.".into(),
            (Location::Drive { .. }, Location::Drive { .. }) if drive_folder(&to).is_some() => "Colar entre contas diferentes do Google Drive ainda não é possível.".into(),
            _ => "Não dá para colar aqui.".into(),
        };
        return;
    }
    let reload = vec![to.clone(), from.clone()];
    let op = if mode == ClipMode::Cut { Op::Move } else { Op::Copy };
    match (&from, &to) {
        (Location::Local(_) | Location::Found(_), Location::Local(dst)) => {
            let sources = items.iter().filter_map(|e| from.local_path(e)).collect();
            local_transfer(s, op, sources, dst.clone(), reload);
        }
        (Location::Local(_) | Location::Found(_), Location::Drive { .. }) => {
            let sources = items.iter().filter_map(|e| from.local_path(e)).collect();
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
    let list = move |d: &Drive| {
        let mut out = Vec::new();
        d.list(&parent, None, |page, _| {
            out.extend(page.into_iter().map(crate::entry_of));
            true
        })?;
        Ok(out)
    };
    drive_task(s, &email, "listar a pasta", list, move |result| Msg::Clashes { up, result });
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
    let Some(email) = account(from).map(str::to_string) else {
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

/// Pesquisar: the words in pane `p`'s search field, where the pane is: a folder with what's
/// under it, a whole shared drive, My Drive; from Drive's views, everything. A search from a
/// search's results looks where that one did.
pub fn search(s: &mut State, p: usize) {
    if s.panes[p].loc.is_local() {
        return search_computer(s, p);
    }
    if let Some(to) = search_from(&s.panes[p].loc, &s.search[p].text) {
        crate::navigate(s, p, to);
    }
}

/// Searches the computer for the words in pane `p`'s field, under the folder the pane shows (or
/// where its results looked). From results, the new ones take their place rather than adding a
/// step to Voltar, so typing a word letter by letter goes back in one step. Emptied, the field
/// takes the pane back to the folder searched.
pub fn search_computer(s: &mut State, p: usize) {
    let text = s.search[p].text.clone();
    s.searched[p] = text.clone();
    let loc = s.panes[p].loc.clone();
    let within = match &loc {
        Location::Local(dir) => Some(dir.clone()),
        Location::Found(f) => f.within.clone(),
        _ => return,
    };
    if text.trim().is_empty() {
        if matches!(loc, Location::Found(_)) {
            // Back to the folder the search started from: Voltar's last step, or the folder
            // searched.
            match (s.panes[p].back.last(), within) {
                (Some(Location::Local(_)), _) => crate::go_back(s, p),
                (_, Some(dir)) => crate::switch(s, p, Location::Local(dir)),
                _ => {}
            }
        }
        return;
    }
    let to = Location::Found(Box::new(crate::finder::found(&text, within)));
    if matches!(loc, Location::Found(_)) {
        crate::switch(s, p, to);
    } else {
        crate::navigate(s, p, to);
    }
}

/// Installs the search service, Windows asking for an administrator's permission, and searches
/// again in the panes waiting on it.
pub fn install_finder(s: &mut State) {
    if s.installing_finder {
        return;
    }
    s.installing_finder = true;
    s.status = "Instalando o serviço de pesquisa (o Windows pede permissão)…".into();
    let reload: Vec<Location> = s.panes.iter().map(|p| p.loc.clone()).filter(|l| matches!(l, Location::Found(_))).collect();
    crate::background(s, move |emit| emit(crate::Msg::Installed { reload, result: crate::finder::install() }));
}

/// Where searching `text` from `loc` leads: its results, scoped as [`search`] says.
fn search_from(loc: &Location, text: &str) -> Option<Location> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let Location::Drive { email, .. } = loc else { return None };
    if text.is_empty() {
        return None;
    }
    let (within, place) = search_scope(loc)?;
    Some(Location::Drive { email: email.clone(), space: Space::Search(Box::new(crate::model::Search { text, within, place })), path: Vec::new() })
}

/// Where a search from `loc` looks, and that place in words; `None` off Drive.
pub(crate) fn search_scope(loc: &Location) -> Option<(openexplorer_gdrive::search::Within, String)> {
    use openexplorer_gdrive::search::Within;
    let Location::Drive { space, path, .. } = loc else { return None };
    Some(match (space, path.last()) {
        (Space::Search(q), None) => (q.within.clone(), q.place.clone()),
        // A trashed folder's contents aren't searchable on their own.
        (Space::Trash, _) => (Within::Everything, "todo o Drive".into()),
        (Space::Shared { id, .. }, Some((f, name))) => (Within::Folder { id: f.clone(), shared_drive: Some(id.clone()) }, shown(name)),
        (Space::Shared { id, name }, None) => (Within::SharedDrive(id.clone()), shown(name)),
        (_, Some((f, name))) => (Within::Folder { id: f.clone(), shared_drive: None }, shown(name)),
        (Space::MyDrive, None) => (Within::MyDrive, "Meu Drive".into()),
        _ => (Within::Everything, "todo o Drive".into()),
    })
}

/// Pesquisar em todo o Drive (or the whole computer): the same words, everywhere.
pub fn search_everywhere(s: &mut State, p: usize) {
    if let Location::Found(f) = &s.panes[p].loc {
        let to = Location::Found(Box::new(crate::finder::found(&f.text, None)));
        return crate::switch(s, p, to);
    }
    let Location::Drive { email, space: Space::Search(q), .. } = s.panes[p].loc.clone() else { return };
    let q = crate::model::Search { within: openexplorer_gdrive::search::Within::Everything, place: "todo o Drive".into(), ..*q };
    crate::navigate(s, p, Location::Drive { email, space: Space::Search(Box::new(q)), path: Vec::new() });
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
        Location::Local(_) | Location::Found(_) => {
            let paths: Vec<PathBuf> = items.iter().filter_map(|e| loc.local_path(e)).collect();
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
    s.name_edit = name_field(item);
    s.renaming = Some((p, item.clone()));
    tx.scroll_into_view_in(crate::pane_key(p), crate::row_key(&item.id));
}

/// The field Renomear (and Propriedades' name) starts with: the name as shown, its stem
/// selected.
pub(crate) fn name_field(item: &Entry) -> TextEdit {
    let name = shown(&item.name);
    let mut edit = TextEdit::new(name.clone());
    edit.set_selection(0, stem_len(&name, item.is_dir));
    edit
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
        Location::Local(_) | Location::Found(_) => ops::invalid_name(name),
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
/// Renames `item` (or makes a new folder, `None`) to `name` as typed over the shown one: in
/// Drive each `／` goes back to the `/` it shows (see [`crate::model::NAME_SLASH`]).
pub fn apply_name(s: &mut State, pane: usize, item: Option<Entry>, name: String) {
    let loc = s.panes[pane].loc.clone();
    let shown_name = name.clone();
    let name = if matches!(loc, Location::Drive { .. }) { typed(&name) } else { name };
    // Unchanged as shown (a real `／` kept), or the same name typed with `/`.
    if item.as_ref().is_some_and(|i| shown(&i.name) == shown_name || i.name == name) {
        return;
    }
    match (&loc, item) {
        (Location::Local(_) | Location::Found(_), Some(item)) => {
            let Some(from) = loc.local_path(&item) else { return };
            local_job(s, vec![loc.clone()], move || ops::rename(&from, &name).map(|_| format!("Renomeado para \"{name}\".")));
        }
        (Location::Local(dir), None) => {
            let dir = dir.clone();
            local_job(s, vec![loc.clone()], move || ops::new_folder(&dir, &name).map(|_| format!("Pasta \"{name}\" criada.")));
        }
        (Location::Drive { email, .. }, Some(item)) => {
            drive_job(s, email, account_panes(s, email), move |d| d.rename(&item.id, &name).map(|_| format!("Renomeado para \"{shown_name}\".")));
        }
        (Location::Drive { .. }, None) => {
            let Some((email, folder)) = drive_folder(&loc) else { return not_here(s, pane) };
            drive_job(s, &email, vec![loc.clone()], move |d| d.create_folder(&name, &folder).map(|_| format!("Pasta \"{shown_name}\" criada.")));
        }
        (Location::Mock(_) | Location::Found(_), _) => not_here(s, pane),
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

/// Runs local `work` for the open Propriedades (Drive's answers come through [`drive_task`]); its
/// answer lands only if that dialog is still open. `work` gives what the task found; a panic in
/// it ends that same task with the error (the hash row stops saying "Calculando…"), through
/// `failed`.
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
    if loc.is_local() {
        // One item: its dates and attributes now (one metadata call). Folders and several
        // items: their contents add up in the background.
        if let [e] = items.as_slice()
            && let Some(path) = loc.local_path(e)
        {
            let meta = std::fs::symlink_metadata(&path).ok();
            local = Some(LocalInfo {
                created: meta.as_ref().and_then(|m| m.created().ok()),
                accessed: meta.as_ref().and_then(|m| m.accessed().ok()),
                attributes: meta.as_ref().map(ops::attributes).unwrap_or_default(),
                path,
            });
        }
        if items.len() > 1 || items.iter().any(|e| e.is_dir) {
            let paths: Vec<PathBuf> = items.iter().filter_map(|e| loc.local_path(e)).collect();
            let alive = alive.clone();
            props_job(s, id, |_| PropUpdate::Totals(ops::Totals::default()), move || PropUpdate::Totals(ops::totals(&paths, &|| !alive.load(Ordering::Relaxed))));
        }
    }
    // One Drive item: its owner, dates, description and checksum from Drive.
    if let ([item], Some(email)) = (items.as_slice(), account(&loc)) {
        let (details_id, sharing_id) = (item.id.clone(), item.id.clone());
        drive_task(s, email, "ler as propriedades", move |d| d.details(&details_id), move |r| Msg::Prop { id, update: PropUpdate::Details(r) });
        drive_task(s, email, "ler as propriedades", move |d| d.sharing(&sharing_id), move |r| Msg::Prop { id, update: PropUpdate::Sharing(r) });
    }
    if let [item] = items.as_slice() {
        s.name_edit = name_field(item);
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
    let Some(email) = account(&s.panes[pane].loc) else { return };
    drive_task(s, email, "ler as versões", move |d| d.revisions(&item_id), move |r| Msg::Prop { id, update: PropUpdate::Revisions(r) });
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
        && name != shown(&item.name)
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

    /// A Drive name's `/` shows as `／` in the rename field, its stem selected by bytes, and
    /// what's typed goes back to `/` for Drive.
    #[test]
    fn a_slash_in_a_name_is_edited_as_shown() {
        let e = Entry { id: "f1".into(), name: "a/b.txt".into(), is_dir: false, size: None, modified: None, target: None, mime: None, md5: None, starred: false };
        let edit = name_field(&e);
        assert_eq!(edit.text, "a／b.txt");
        assert_eq!((edit.anchor.min(edit.focus), edit.anchor.max(edit.focus)), (0, "a／b".len()));
        assert_eq!(typed(&edit.text), "a/b.txt");
        assert_eq!(shown("Relatório 2025.pdf"), "Relatório 2025.pdf");
    }

    fn drive(space: Space, path: &[&str]) -> Location {
        Location::Drive { email: "a@b".into(), space, path: path.iter().map(|id| (id.to_string(), id.to_string())).collect() }
    }

    /// A search looks where the pane is: under a folder (in its shared drive when it sits in
    /// one), a whole shared drive, My Drive; from views and Lixeira, everywhere. Searching from
    /// results keeps their scope; blank words search nothing.
    #[test]
    fn a_search_looks_where_the_pane_is() {
        use openexplorer_gdrive::search::Within;
        let scope = |loc: &Location, text: &str| match search_from(loc, text) {
            Some(Location::Drive { space: Space::Search(q), path, .. }) if path.is_empty() => Some((q.text, q.within, q.place)),
            other => panic!("not a search: {other:?}"),
        };
        let shared = Space::Shared { id: "0AA".into(), name: "Equipe".into() };
        assert_eq!(search_from(&drive(Space::MyDrive, &[]), "  "), None);
        assert_eq!(search_from(&Location::Local("C:\\".into()), "x"), None);
        assert_eq!(scope(&drive(Space::MyDrive, &[]), " relatório   2025 "), Some(("relatório 2025".into(), Within::MyDrive, "Meu Drive".into())));
        assert_eq!(scope(&drive(Space::MyDrive, &["f1", "f2"]), "x").map(|q| q.1), Some(Within::Folder { id: "f2".into(), shared_drive: None }));
        assert_eq!(scope(&drive(shared.clone(), &[]), "x").map(|q| q.1), Some(Within::SharedDrive("0AA".into())));
        assert_eq!(scope(&drive(shared, &["f1"]), "x").map(|q| q.1), Some(Within::Folder { id: "f1".into(), shared_drive: Some("0AA".into()) }));
        assert_eq!(scope(&drive(Space::Starred, &["f1"]), "x").map(|q| q.1), Some(Within::Folder { id: "f1".into(), shared_drive: None }));
        for everywhere in [drive(Space::Starred, &[]), drive(Space::SharedDrives, &[]), drive(Space::Trash, &[]), drive(Space::Trash, &["f1"])] {
            assert_eq!(scope(&everywhere, "x").map(|q| q.1), Some(Within::Everything));
        }
        let results = search_from(&drive(Space::MyDrive, &["f1"]), "a").unwrap();
        assert_eq!(scope(&results, "b"), Some(("b".into(), Within::Folder { id: "f1".into(), shared_drive: None }, "f1".into())));
        // A folder opened from the results searches under it.
        let Location::Drive { email, space, .. } = results else { unreachable!() };
        let inside = Location::Drive { email, space, path: vec![("f9".into(), "Nove".into())] };
        assert_eq!(scope(&inside, "c").map(|q| (q.1, q.2)), Some((Within::Folder { id: "f9".into(), shared_drive: None }, "Nove".into())));
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
        // Items listed in a view download like any others; Lixeira's and the drives list don't.
        let local = Location::Local("C:\\".into());
        assert!(fits(&drive(Space::Starred, &[]), &local));
        assert!(fits(&drive(Space::SharedWithMe, &[]), &local));
        assert!(!fits(&drive(Space::Trash, &[]), &local));
        assert!(!fits(&drive(Space::SharedDrives, &[]), &local));
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
