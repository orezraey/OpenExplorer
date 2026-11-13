//! What the context menu and its shortcuts do: open, cut, copy, paste, delete, rename, new
//! folder and properties, on the local disk and in Google Drive. The work runs in the
//! background and ends in a [`Msg::Done`] that reloads the folders it touched.

use std::path::PathBuf;

use openexplorer_gdrive::{Details, Drive, DriveFile};
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
    /// `local` is one local item's (created, full path), read as the dialog opens.
    Properties { pane: usize, items: Vec<Entry>, details: Option<Result<Details, String>>, local: Option<(Option<std::time::SystemTime>, PathBuf)> },
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

/// Whether Colar can paste the clip into pane `p` as it is: same disk or same Drive account.
pub fn can_paste(s: &State, p: usize) -> bool {
    let Some(clip) = &s.clip else { return false };
    let to = &s.panes[p].loc;
    match (&clip.from, to) {
        (Location::Local(_), Location::Local(_)) => true,
        (Location::Drive { email: a, .. }, Location::Drive { email: b, .. }) => a == b && drive_folder(to).is_some(),
        _ => false,
    }
}

pub fn paste(s: &mut State, p: usize) {
    let Some(clip) = &s.clip else { return };
    let to = s.panes[p].loc.clone();
    if !can_paste(s, p) {
        s.status = match (&clip.from, &to) {
            (Location::Local(_), Location::Drive { .. }) | (Location::Drive { .. }, Location::Local(_)) => {
                "Copiar entre o computador e o Google Drive (envio e download) é o próximo passo; por enquanto, cole na mesma origem.".into()
            }
            (Location::Drive { .. }, Location::Drive { .. }) if drive_folder(&to).is_some() => "Colar entre contas diferentes do Google Drive ainda não é possível.".into(),
            _ => "Não dá para colar aqui.".into(),
        };
        return;
    }
    let (mode, from, items) = (clip.mode, clip.from.clone(), clip.items.clone());
    // A cut is used once; a copy can be pasted again.
    if mode == ClipMode::Cut {
        s.clip = None;
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
fn apply_name(s: &mut State, pane: usize, item: Option<Entry>, name: String) {
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

pub fn properties(s: &mut State, p: usize) {
    let items = selected(s, p);
    if items.is_empty() {
        return;
    }
    // One Drive item gets its owner, creation date and link from Drive.
    if let ([item], Location::Drive { email, .. }) = (items.as_slice(), &s.panes[p].loc)
        && !matches!(s.panes[p].loc, Location::Drive { space: Space::SharedDrives, .. })
    {
        let (id, cached, email) = (item.id.clone(), s.drives.get(email).cloned(), email.clone());
        background(s, move |emit| {
            let result = cached.map_or_else(|| open_drive(&email), Ok).and_then(|d| d.details(&id));
            emit(Msg::Details { id, result });
        });
    }
    let local = match (items.as_slice(), &s.panes[p].loc) {
        ([e], Location::Local(dir)) => Some(local_details(dir, e)),
        _ => None,
    };
    s.dialog = Some(Dialog::Properties { pane: p, items, details: None, local });
}

/// Local details for Propriedades: (created, full path).
fn local_details(dir: &std::path::Path, e: &Entry) -> (Option<std::time::SystemTime>, PathBuf) {
    let path = dir.join(&e.name);
    (std::fs::metadata(&path).and_then(|m| m.created()).ok(), path)
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
}
