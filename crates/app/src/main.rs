//! Open Explorer: two panes side by side, each showing the local disk, a simulated drive or a
//! connected Google Drive account.
//!
//! Drive calls block, so they run on background threads that send a [`Msg`] back and wake the
//! window; `poll` applies it.

mod actions;
mod cache;
mod icons;
mod layout;
mod model;
mod ops;
mod sysicons;
mod transfers;
mod uploads;
mod ui;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::SystemTime;

use openexplorer_gdrive::store::{self, StoredAccount};
use openexplorer_gdrive::{ClientConfig, Drive, DriveFile, oauth};
use rune::{AppSpec, DragItem, Key, KeyEvent, Mods, Tx, Waker};

use model::{Entry, Location, MockDrive, Pane, SortCol, Space};

const ROW_H: f32 = 22.0;

enum Msg {
    /// Part of a folder listing. `made` carries an account opened on the background thread,
    /// for `poll` to keep.
    Listed { pane: usize, request: u64, made: Option<(String, Arc<Drive>)>, part: Part, result: Result<Vec<Entry>, String> },
    LoggedIn { pane: usize, attempt: u64, result: Result<(StoredAccount, Arc<Drive>), String> },
    /// A file operation finished: reload the panes showing `reload`, and say how it went.
    /// `made` is an account the operation had to open, for `poll` to keep.
    Done { reload: Vec<Location>, made: Option<(String, Arc<Drive>)>, result: Result<String, String> },
    /// Something Propriedades number `id` was waiting for.
    Prop { id: u64, update: actions::PropUpdate },
    /// Windows' icon for a file type, a folder or a drive.
    Icon { key: sysicons::IconKey, rgba: Option<sysicons::Rgba> },
}

/// Which part of a listing a [`Msg::Listed`] is.
#[derive(Clone, Copy, PartialEq)]
enum Part {
    /// A listing saved by an earlier run, shown while the fresh one loads.
    Cached,
    /// Fresh entries. `first` replaces what the pane shows, later pages add to it; `more` says
    /// another page is on its way.
    Page { first: bool, more: bool },
}

impl Part {
    const WHOLE: Part = Part::Page { first: true, more: false };
}

struct State {
    panes: [Pane; 2],
    /// The context menu: the pane it was opened in and where.
    menu: Option<(usize, rune::PointerPress)>,
    dialog: Option<actions::Dialog>,
    /// The name typed in Renomear or Nova pasta.
    name_edit: rune::TextEdit,
    /// The item being renamed in place, and its pane.
    renaming: Option<(usize, Entry)>,
    /// Counts Propriedades dialogs, to tell their background answers apart.
    props_seq: u64,
    /// What Copiar or Recortar picked up.
    clip: Option<actions::Clip>,
    /// Each pane's tree width, which its splitter changes.
    layout: layout::Layout,
    /// The disks "Este computador" lists, read at start.
    local_drives: Vec<(PathBuf, model::DriveKind)>,
    /// Each pane's current listing request, bumped by every load: `poll` drops answers for an
    /// older one, and the listing thread stops fetching pages once the pane has moved on.
    live: [Arc<AtomicU64>; 2],
    cache: cache::Cache,
    mock: MockDrive,
    accounts: Vec<StoredAccount>,
    /// Connected accounts by e-mail, made on first use.
    drives: HashMap<String, Arc<Drive>>,
    /// The login waiting for the browser, numbered from `logins`. Clicking again starts a new
    /// one; an older attempt's error is then dropped, its success still kept.
    login: Option<u64>,
    logins: u64,
    active: usize,
    /// The pane a drag started in and the files it carried, so a drop back on the same pane
    /// is ignored (the drop can arrive after the drag's end).
    last_drag: Option<(usize, Vec<PathBuf>)>,
    /// What the last action said ("Copiado: 3 itens.", an error...), shown for a few seconds
    /// in a notice at the bottom right.
    status: String,
    /// The message the notice was last started for, and when it goes (app clock, seconds).
    status_seen: String,
    status_until: f64,
    /// The app clock at the last frame.
    now: f64,
    temp: PathBuf,
    inbox: Receiver<Msg>,
    outbox: Sender<Msg>,
    waker: Option<Waker>,
    /// Copies and moves under way, each with its bar.
    transfers: Vec<transfers::Transfer>,
    /// The transfer queue's right-click menu: the row it was opened on, and where.
    transfer_menu: Option<(Option<Arc<ops::Progress>>, rune::PointerPress)>,
    /// How many rows were still waiting or running at the last frame.
    transfers_live: usize,
    /// Windows' own icons, as Explorer shows them.
    icons: sysicons::SysIcons,
    /// The listing version each pane's icons were last asked for.
    icons_for: [u64; 2],
}

fn init() -> State {
    let (outbox, inbox) = channel();
    let mock = MockDrive::new();
    let mut panes = [Pane::new(Location::home()), Pane::new(Location::Mock(Vec::new()))];
    for p in &mut panes {
        p.load_now(&mock);
    }
    State {
        panes,
        menu: None,
        dialog: None,
        name_edit: rune::TextEdit::new(""),
        renaming: None,
        props_seq: 0,
        clip: None,
        local_drives: model::local_drives(),
        layout: layout::Layout::load(),
        live: Default::default(),
        cache: Default::default(),
        mock,
        accounts: store::accounts(),
        drives: HashMap::new(),
        login: None,
        logins: 0,
        active: 0,
        last_drag: None,
        status: String::new(),
        status_seen: String::new(),
        status_until: 0.0,
        now: 0.0,
        temp: std::env::temp_dir().join("OpenExplorer-drag"),
        inbox,
        outbox,
        waker: None,
        transfers: Vec::new(),
        transfer_menu: None,
        transfers_live: 0,
        icons: Default::default(),
        icons_for: [u64::MAX; 2],
    }
}

fn setup(s: &mut State, waker: Waker) {
    let fixed = s.local_drives.iter().map(|(p, _)| sysicons::drive_key(p)).chain(sysicons::home_key());
    s.icons.start(rune::ImageCache::new(waker.clone()), s.outbox.clone(), waker.clone(), fixed);
    s.waker = Some(waker);
}

/// Runs `f`, turning a panic into an error that names it (and logging it), so background work
/// that hits a bug still ends with a message instead of never answering. `doing` says what it
/// was ("listar", "conectar").
fn catch_panic<T>(doing: &str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|p| {
        let what = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
        openexplorer_gdrive::log(&format!("pânico ao {doing}: {what}"));
        Err(format!("Erro interno ao {doing} ({what}). Detalhes no log."))
    })
}

/// Runs `work` on a new thread. Each message it emits reaches `poll` at once.
fn background(s: &State, work: impl FnOnce(&dyn Fn(Msg)) + Send + 'static) {
    let (out, wake) = (s.outbox.clone(), s.waker.clone());
    std::thread::spawn(move || {
        work(&|msg| {
            let _ = out.send(msg);
            if let Some(w) = &wake {
                w();
            }
        })
    });
}

/// Starts the notice for a new message, and takes it away when its time is up; a wake is
/// set for then, as nothing else may draw a frame. The same message said again later shows
/// again, as the expired one is cleared.
fn notice(s: &mut State, now: f64) -> bool {
    s.now = now;
    if s.status != s.status_seen {
        s.status_seen = s.status.clone();
        s.status_until = now + NOTICE_SECS;
        if let Some(wake) = s.waker.clone() {
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs_f64(NOTICE_SECS + 0.1));
                wake();
            });
        }
        return true;
    }
    if !s.status.is_empty() && now >= s.status_until {
        s.status.clear();
        s.status_seen.clear();
        return true;
    }
    false
}

/// Seconds a message stays on screen.
const NOTICE_SECS: f64 = 5.0;

fn poll(s: &mut State, now: f64, _tx: &mut Tx) -> bool {
    // Each listing's file types get their icons asked for once.
    let versions = s.panes.each_ref().map(|p| p.version);
    if s.icons_for != versions {
        s.icons_for = versions;
        let keys = s.panes.iter().flat_map(|p| sysicons::keys_of(&p.entries, &p.loc)).collect();
        s.icons.show(keys);
    }
    let mut changed = s.icons.settle();
    changed |= notice(s, now);
    // Rows done leave the queue (the status line says how the job went); cancelled and failed
    // ones stay to be seen. While any runs, its thread wakes the window a few times a second.
    // A redraw is due while any row moves, and once more when the last one stops.
    let rows = s.transfers.len();
    s.transfers.retain(|t| t.progress.stage() != ops::Stage::Done);
    // The queue's menu goes with its row, and with the queue when that empties and hides.
    let gone = match &s.transfer_menu {
        Some((Some(row), _)) => !s.transfers.iter().any(|t| Arc::ptr_eq(&t.progress, row)),
        Some((None, _)) => s.transfers.is_empty(),
        None => false,
    };
    if gone {
        s.transfer_menu = None;
    }
    let live = s.transfers.iter().filter(|t| !t.progress.over()).count();
    changed |= live > 0 || live != s.transfers_live || s.transfers.len() != rows;
    s.transfers_live = live;
    while let Ok(msg) = s.inbox.try_recv() {
        changed = true;
        match msg {
            Msg::Listed { pane, request, made, part, result } => {
                if let Some((email, drive)) = made
                    && s.accounts.iter().any(|a| a.email == email)
                {
                    s.drives.entry(email).or_insert(drive);
                }
                if s.live[pane].load(Ordering::Relaxed) != request {
                    continue;
                }
                let (first, more) = match part {
                    Part::Cached => (true, true),
                    Part::Page { first, more } => (first, more),
                };
                let ok = result.is_ok();
                let p = &mut s.panes[pane];
                // The selection (the folder just left, say) may be on a page still to come.
                let selected = p.selected.clone();
                match result {
                    r if first => p.set_entries(r),
                    Ok(page) => p.append_entries(page),
                    // A later page, or the refresh of a cached listing, failed: keep what the pane
                    // shows and say so.
                    Err(e) => s.status = format!("Não consegui terminar a listagem, mostrando o que chegou: {e}"),
                }
                let p = &mut s.panes[pane];
                p.loading = ok && more;
                if p.loading {
                    p.selected = selected;
                } else {
                    p.prune_selection();
                }
                drop_stale_rename(s, pane);
                // A complete fresh listing replaces the cached one.
                if ok
                    && !more
                    && part != Part::Cached
                    && let Some(k) = cache::key(&s.panes[pane].loc)
                {
                    let entries = s.panes[pane].entries.clone();
                    s.cache.put(k, entries);
                }
            }
            Msg::Done { reload, made, result } => {
                if let Some((email, drive)) = made
                    && s.accounts.iter().any(|a| a.email == email)
                {
                    s.drives.entry(email).or_insert(drive);
                }
                match result {
                    Ok(msg) | Err(msg) => s.status = msg,
                }
                for p in 0..2 {
                    if reload.contains(&s.panes[p].loc) {
                        load(s, p);
                    }
                }
            }
            Msg::Prop { id, update } => actions::props_update(s, id, update),
            Msg::Icon { key, rgba } => s.icons.arrived(key, rgba),
            Msg::LoggedIn { pane, attempt, result } => {
                let current = s.login == Some(attempt);
                if current {
                    s.login = None;
                } else if result.is_err() {
                    continue;
                }
                match result {
                    Ok((account, drive)) => {
                        s.status = format!("Conta conectada: {}", account.email);
                        s.drives.insert(account.email.clone(), drive);
                        let email = account.email.clone();
                        match s.accounts.iter_mut().find(|a| a.email == account.email) {
                            Some(a) => *a = account,
                            None => s.accounts.push(account),
                        }
                        navigate(s, pane, Location::drive(email, Space::MyDrive));
                    }
                    Err(e) => s.status = e,
                }
            }
        }
    }
    changed
}

fn entry_of(f: DriveFile) -> Entry {
    Entry { id: f.id, name: f.name, is_dir: f.is_dir, size: f.size, modified: f.modified, target: f.target, mime: Some(f.mime), md5: f.md5 }
}

fn pane_key(p: usize) -> String {
    format!("pane-{p}")
}

fn row_key(id: &str) -> String {
    format!("r:{id}")
}

// ---------------------------------------------------------------------------------------------
// Accounts

/// Opens a stored account: reads the client file and the credential store, so it runs off the
/// UI thread.
fn open_drive(email: &str) -> Result<Arc<Drive>, String> {
    let cfg = ClientConfig::load()?;
    let token = store::refresh_token(email)?;
    Ok(Arc::new(Drive::new(cfg, token)))
}

fn connect(s: &mut State, p: usize, tx: &mut Tx) {
    let cfg = match ClientConfig::load() {
        Ok(c) => c,
        Err(e) => {
            s.status = e;
            return;
        }
    };
    let pending = match oauth::begin(&cfg) {
        Ok(l) => l,
        Err(e) => {
            s.status = e;
            return;
        }
    };
    if let Err(e) = tx.open_url(&pending.url) {
        s.status = format!("Não consegui abrir o navegador: {e:?}");
        return;
    }
    s.logins += 1;
    let attempt = s.logins;
    s.login = Some(attempt);
    s.status = "Faça o login no navegador que abriu. Esperando o Google…".into();
    background(s, move |emit| {
        let result = catch_panic("conectar", || {
            let tokens = pending.finish(&cfg)?;
            let drive = Drive::from_login(cfg, &tokens)?;
            let who = drive.about()?;
            let account = StoredAccount { email: who.email, name: who.name };
            store::save(account.clone(), drive.refresh_token())?;
            Ok((account, Arc::new(drive)))
        });
        emit(Msg::LoggedIn { pane: p, attempt, result });
    });
}

fn disconnect(s: &mut State, email: &str) {
    if let Err(e) = store::remove(email) {
        s.status = e;
        return;
    }
    s.drives.remove(email);
    s.cache.forget(email);
    s.accounts.retain(|a| a.email != email);
    for p in 0..2 {
        let gone = |l: &Location| matches!(l, Location::Drive { email: e, .. } if e == email);
        s.panes[p].back.retain(|l| !gone(l));
        s.panes[p].forward.retain(|l| !gone(l));
        if matches!(&s.panes[p].loc, Location::Drive { email: e, .. } if e == email) {
            switch(s, p, Location::Mock(Vec::new()));
        }
    }
    s.status = format!("Conta desconectada: {email}");
}

// ---------------------------------------------------------------------------------------------
// Navigation

/// Lists the pane's current folder: in place for local and simulated folders, in the
/// background for Drive.
fn load(s: &mut State, p: usize) {
    // Every load gets a new number, so a Drive answer for a folder left since is dropped,
    // whatever the pane shows now.
    let request = s.live[p].fetch_add(1, Ordering::Relaxed) + 1;
    let Location::Drive { email, space, path } = s.panes[p].loc.clone() else {
        let State { panes, mock, .. } = s;
        panes[p].load_now(mock);
        drop_stale_rename(s, p);
        return;
    };
    let opened = s.drives.get(&email).cloned();
    let key = cache::key(&s.panes[p].loc);
    // A folder seen this session shows at once; the fresh listing then replaces it whole.
    let in_memory = key.as_ref().and_then(|k| s.cache.get(k)).map(|e| e.to_vec());
    let pane = &mut s.panes[p];
    let mut quiet = in_memory.is_some();
    if let Some(entries) = in_memory {
        pane.set_entries(Ok(entries));
    }
    pane.loading = true;
    pane.error = None;
    let live = s.live[p].clone();
    let folder = path.last().map(|(id, _)| id.clone());
    background(s, move |emit| {
        let listed = |made, part, result| Msg::Listed { pane: p, request, made, part, result };
        // Else one from an earlier run, read here off the UI thread.
        if !quiet && let Some(entries) = key.as_ref().and_then(cache::load_disk) {
            emit(listed(None, Part::Cached, Ok(entries)));
            quiet = true;
        }
        let (drive, made) = match opened {
            Some(d) => (d, None),
            None => match open_drive(&email) {
                Ok(d) => (d.clone(), Some((email, d))),
                Err(e) => return emit(listed(None, Part::Page { first: !quiet, more: false }, Err(e))),
            },
        };
        // Handed over with the first message, from whichever path sends it.
        let made = std::cell::Cell::new(made);
        // Without a cached listing each page shows as soon as it lands, so a big folder fills
        // in instead of staying blank. With one, pages are gathered and replace it in one go,
        // so the list doesn't shrink to the first page and grow back.
        let mut first = true;
        let mut gathered = Vec::new();
        let mut page = |files: Vec<DriveFile>, more: bool| {
            let entries = files.into_iter().map(entry_of);
            if quiet {
                gathered.extend(entries);
                if !more {
                    emit(listed(made.take(), Part::WHOLE, Ok(std::mem::take(&mut gathered))));
                }
            } else {
                emit(listed(made.take(), Part::Page { first, more }, Ok(entries.collect())));
                first = false;
            }
            live.load(Ordering::Relaxed) == request
        };
        // A panic here (a bad value from Drive, say) must not leave the pane loading forever.
        let result = catch_panic("listar", || match &space {
            Space::MyDrive => drive.list(folder.as_deref().unwrap_or("root"), None, &mut page),
            Space::Shared { id, .. } => drive.list(folder.as_deref().unwrap_or(id), Some(id), &mut page),
            // The shared drives show as folders.
            Space::SharedDrives => drive.shared_drives().map(|list| {
                list.into_iter()
                    .map(|d| Entry {
                        name: if d.hidden { format!("{} (oculto)", d.name) } else { d.name },
                        id: d.id,
                        is_dir: true,
                        size: None,
                        modified: None,
                        target: None,
                        mime: None,
                        md5: None,
                    })
                    .collect::<Vec<_>>()
            }).map(|entries| emit(listed(made.take(), Part::WHOLE, Ok(entries)))),
        });
        if let Err(e) = result {
            // Over a cached or partly shown listing, keep it and only report the error.
            emit(listed(made.take(), Part::Page { first: first && !quiet, more: false }, Err(e)));
        }
    });
}

fn switch(s: &mut State, p: usize, loc: Location) {
    s.active = p;
    if s.renaming.as_ref().is_some_and(|(rp, _)| *rp == p) {
        s.renaming = None;
    }
    s.panes[p].go(loc);
    load(s, p);
}

/// Ends a rename whose item a reload took out of pane `p` (deleted or moved by another
/// operation): its field is gone, and the pane's keys would otherwise stay with it.
fn drop_stale_rename(s: &mut State, p: usize) {
    if let Some((rp, item)) = &s.renaming
        && *rp == p
        && s.panes[p].entry(&item.id).is_none()
        && !s.panes[p].loading
    {
        s.renaming = None;
    }
}

/// Goes to `loc` as a step the reader can take back with Voltar.
fn navigate(s: &mut State, p: usize, loc: Location) {
    let pane = &mut s.panes[p];
    if pane.loc == loc {
        s.active = p;
        return;
    }
    let from = std::mem::replace(&mut pane.loc, loc.clone());
    pane.back.push(from);
    pane.forward.clear();
    switch(s, p, loc);
}

fn go_back(s: &mut State, p: usize) {
    let pane = &mut s.panes[p];
    if let Some(loc) = pane.back.pop() {
        pane.forward.push(pane.loc.clone());
        switch(s, p, loc);
    }
}

fn go_forward(s: &mut State, p: usize) {
    let pane = &mut s.panes[p];
    if let Some(loc) = pane.forward.pop() {
        pane.back.push(pane.loc.clone());
        switch(s, p, loc);
    }
}

fn open(s: &mut State, p: usize, id: &str) {
    let pane = &mut s.panes[p];
    let Some(e) = pane.entry(id).cloned() else { return };
    if !e.is_dir {
        s.status = format!("Abrir \"{}\" ainda não faz parte do protótipo.", e.name);
        return;
    }
    let mut loc = pane.loc.clone();
    match &mut loc {
        Location::Local(dir) => dir.push(&e.name),
        Location::Mock(segs) => segs.push(e.name.clone()),
        Location::Drive { space: space @ Space::SharedDrives, .. } => *space = Space::Shared { id: e.id.clone(), name: e.name.clone() },
        Location::Drive { path, .. } => path.push((e.target.clone().unwrap_or_else(|| e.id.clone()), e.name.clone())),
    }
    navigate(s, p, loc);
}

fn go_up(s: &mut State, p: usize) {
    let mut loc = s.panes[p].loc.clone();
    // The folder we come out of, to land on it.
    let from = match &mut loc {
        Location::Local(dir) => {
            let name = dir.file_name().map(|n| n.to_string_lossy().into_owned());
            if !dir.pop() {
                return;
            }
            name
        }
        Location::Mock(segs) => segs.pop(),
        Location::Drive { space, path, .. } => match (path.pop(), &*space) {
            (Some((id, _)), _) => Some(id),
            // From the top of a shared drive up to the list of shared drives.
            (None, Space::Shared { id, .. }) => {
                let id = id.clone();
                *space = Space::SharedDrives;
                Some(id)
            }
            (None, _) => None,
        },
    };
    if from.is_none() {
        return;
    }
    navigate(s, p, loc);
    if let Some(id) = from {
        let pane = &mut s.panes[p];
        pane.selected = [id.clone()].into();
        pane.cursor = Some(id.clone());
        pane.anchor = Some(id);
    }
}

// ---------------------------------------------------------------------------------------------
// Selection and keys

fn click(s: &mut State, p: usize, id: &str, mods: Mods) {
    s.active = p;
    let pane = &mut s.panes[p];
    if mods.cmd {
        if !pane.selected.remove(id) {
            pane.selected.insert(id.to_string());
        }
        pane.anchor = Some(id.to_string());
    } else if mods.shift && pane.anchor.is_some() {
        let a = pane.anchor.as_deref().and_then(|a| pane.index_of(a));
        let b = pane.index_of(id);
        if let (Some(a), Some(b)) = (a, b) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            pane.selected = pane.entries[lo..=hi].iter().map(|e| e.id.clone()).collect();
        }
    } else {
        pane.selected = [id.to_string()].into();
        pane.anchor = Some(id.to_string());
    }
    pane.cursor = Some(id.to_string());
}

fn key(s: &mut State, p: usize, ev: &KeyEvent, tx: &mut Tx) -> bool {
    // Keys belong to the name field while renaming.
    if s.renaming.is_some() {
        return false;
    }
    let n = s.panes[p].entries.len();
    let at = s.panes[p].cursor.as_deref().and_then(|c| s.panes[p].index_of(c));
    let step = |d: isize| -> Option<usize> {
        if n == 0 {
            return None;
        }
        Some(match at {
            None => 0,
            Some(i) => (i as isize + d).clamp(0, n as isize - 1) as usize,
        })
    };
    let to = match &ev.key {
        _ if ev.is_command('c') => {
            actions::pick_up(s, p, actions::ClipMode::Copy);
            return true;
        }
        _ if ev.is_command('x') => {
            actions::pick_up(s, p, actions::ClipMode::Cut);
            return true;
        }
        _ if ev.is_command('v') => {
            actions::paste(s, p);
            return true;
        }
        _ if ev.is_command('n') && ev.mods.shift => {
            actions::ask_new_folder(s, p, tx);
            return true;
        }
        Key::Delete => {
            actions::ask_delete(s, p);
            return true;
        }
        Key::Function(2) => {
            actions::ask_rename(s, p, tx);
            return true;
        }
        Key::Enter if ev.mods.alt => {
            actions::properties(s, p);
            return true;
        }
        Key::Left if ev.mods.alt => {
            go_back(s, p);
            return true;
        }
        Key::Right if ev.mods.alt => {
            go_forward(s, p);
            return true;
        }
        Key::Up => step(-1),
        Key::Down => step(1),
        Key::PageUp => step(-20),
        Key::PageDown => step(20),
        Key::Home => step(-(n as isize)),
        Key::End => step(n as isize),
        Key::Enter => {
            actions::open(s, p, tx);
            return true;
        }
        Key::Backspace => {
            go_up(s, p);
            return true;
        }
        Key::Tab => {
            s.active = 1 - p;
            tx.focus(pane_key(1 - p), true);
            return true;
        }
        _ if ev.is_command('a') => {
            let pane = &mut s.panes[p];
            pane.selected = pane.entries.iter().map(|e| e.id.clone()).collect();
            return true;
        }
        _ if ev.is_command('r') => {
            load(s, p);
            return true;
        }
        _ => return false,
    };
    if let Some(i) = to {
        let id = s.panes[p].entries[i].id.clone();
        click(s, p, &id, Mods { shift: ev.mods.shift, ..Mods::default() });
        tx.scroll_into_view_in(pane_key(p), row_key(&id));
    }
    true
}

fn set_sort(s: &mut State, p: usize, col: SortCol) {
    let pane = &mut s.panes[p];
    pane.sort = if pane.sort.0 == col { (col, !pane.sort.1) } else { (col, true) };
    pane.sort_entries();
}

// ---------------------------------------------------------------------------------------------
// Drag and drop

/// The ids a drag that started on `id` carries: the selection if `id` is in it, else `id`.
fn dragged_ids(pane: &Pane, id: &str) -> Vec<String> {
    if pane.selected.contains(id) {
        pane.entries.iter().filter(|e| pane.selected.contains(&e.id)).map(|e| e.id.clone()).collect()
    } else {
        vec![id.to_string()]
    }
}

fn drag_item(s: &State, p: usize, id: &str) -> Option<DragItem> {
    let paths = paths_for(s, p, &dragged_ids(&s.panes[p], id), true);
    if paths.is_empty() {
        return None;
    }
    Some(paths.into_iter().fold(DragItem::new(), DragItem::file).within_app())
}

/// Files on disk for the dragged entries. Simulated files become small placeholder files in
/// the temp folder first, as a download would, when `materialize` is set; without it this only
/// names the paths. Drive downloads aren't written yet: none.
fn paths_for(s: &State, p: usize, ids: &[String], materialize: bool) -> Vec<PathBuf> {
    let pane = &s.panes[p];
    match &pane.loc {
        Location::Local(dir) => ids.iter().filter_map(|id| pane.entry(id)).map(|e| dir.join(&e.name)).collect(),
        Location::Drive { .. } => Vec::new(),
        Location::Mock(segs) => {
            let dir = s.temp.join(if segs.is_empty() { "Meu Drive".to_string() } else { segs.join("_") });
            if materialize && std::fs::create_dir_all(&dir).is_err() {
                return Vec::new();
            }
            ids.iter()
                .filter_map(|id| {
                    let e = pane.entry(id)?;
                    let path = dir.join(&e.name);
                    let ok = if !materialize {
                        true
                    } else if e.is_dir {
                        std::fs::create_dir_all(&path).is_ok()
                    } else {
                        path.exists() || std::fs::write(&path, format!("Arquivo simulado: {}/{}\n", pane.title(), e.name)).is_ok()
                    };
                    ok.then_some(path)
                })
                .collect()
        }
    }
}

fn dropped(s: &mut State, p: usize, files: &[PathBuf]) {
    if let Some((from, paths)) = &s.last_drag
        && *from == p
        && paths.as_slice() == files
    {
        s.last_drag = None;
        return;
    }
    s.last_drag = None;
    s.active = p;
    let loc = s.panes[p].loc.clone();
    let (mut done, mut skipped) = (0, 0);
    match &loc {
        Location::Drive { .. } => {
            // Files and folders from Explorer (or the other pane) go up to this Drive folder;
            // the simulated drive's items are placeholders in the temp folder and stay out.
            let real: Vec<PathBuf> = files.iter().filter(|f| !f.starts_with(&s.temp)).cloned().collect();
            if real.is_empty() {
                s.status = "Nada para enviar: arraste arquivos ou pastas do computador.".into();
                return;
            }
            let left_out = files.len() - real.len();
            actions::upload(s, real, &loc, false, vec![loc.clone()]);
            if left_out > 0 {
                s.status = format!("Itens do drive simulado não são enviados ({left_out}).");
            }
            return;
        }
        Location::Local(dir) => {
            // Files and folders from Explorer copy in the background with a bar; ones already
            // here are left alone.
            let fresh: Vec<PathBuf> = files.iter().filter(|f| f.parent() != Some(dir.as_path()) && f.file_name().is_some_and(|n| !dir.join(n).exists())).cloned().collect();
            let skipped = files.len() - fresh.len();
            if !fresh.is_empty() {
                actions::local_transfer(s, transfers::Op::Copy, fresh, dir.clone(), vec![loc.clone()]);
            }
            if skipped > 0 {
                s.status = format!("Ignorados (já estão aqui): {skipped}.");
            }
            return;
        }
        Location::Mock(segs) => {
            for f in files {
                let Some(name) = f.file_name() else { continue };
                let meta = std::fs::metadata(f).ok();
                let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
                let name = name.to_string_lossy().into_owned();
                let entry = Entry {
                    id: name.clone(),
                    name,
                    is_dir,
                    size: if is_dir { None } else { meta.as_ref().map(|m| m.len()) },
                    modified: Some(SystemTime::now()),
                    target: None,
                    mime: None,
                    md5: None,
                };
                if s.mock.add(segs, entry) { done += 1 } else { skipped += 1 }
            }
        }
    }
    s.status = format!("Enviados (simulado): {done} · ignorados (já existem): {skipped}");
    load(s, p);
}

fn main() {
    rune::run(AppSpec::<State> {
        min_size: Some((900.0, 420.0)),
        setup: Some(setup),
        poll: Some(poll),
        scenes: &[
            ("stress", |s| switch(s, 1, Location::Mock(vec!["Teste 50.000 arquivos".into()]))),
            // How a connected account looks, without a network: a fake account and a listing.
            ("account", |s| {
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into() });
                s.panes[1].go(Location::Drive { email: "voce@gmail.com".into(), space: Space::MyDrive, path: vec![("x".into(), "Projetos".into())] });
                s.panes[1].set_entries(Ok(MockDrive::new().list(&["Documentos".into()])));
                s.status = "Conta conectada: voce@gmail.com".into();
            }),
            // A scratch folder of sample files in %TEMP%, for trying file operations headless
            // without touching real files.
            ("ops", |s| {
                let dir = std::env::temp_dir().join("openexplorer-ops-scene");
                let _ = std::fs::remove_dir_all(&dir);
                let _ = std::fs::create_dir_all(dir.join("Projetos"));
                for (name, body) in [("Relatório.pdf", "pdf"), ("foto.jpg", "jpg"), ("notas.txt", "txt")] {
                    let _ = std::fs::write(dir.join(name), body);
                }
                switch(s, 0, Location::Local(dir.clone()));
                switch(s, 1, Location::Local(dir.join("Projetos")));
            }),
            // The transfer queue with a row in each state.
            ("copying", |s| {
                use transfers::{End, Op};
                let from = |name: &str| End::local(&PathBuf::from(r"D:\Desenhos").join(name));
                let drive = Location::drive("voce@gmail.com", Space::MyDrive);
                let rows = [
                    (Op::Copy, from("Episódio 05.mkv"), End::local(&PathBuf::from(r"C:\Users\voce\Vídeos"))),
                    (Op::Copy, from("Episódio 06.mkv"), End::local(&PathBuf::from(r"C:\Users\voce\Vídeos"))),
                    (Op::Move, End::drive(&drive, Some("Relatório.pdf")), End::drive(&drive, None)),
                    (Op::Copy, from("Episódio 07.mkv"), End::local(&PathBuf::from(r"E:\"))),
                ];
                for (op, a, b) in rows {
                    std::mem::forget(transfers::queue(&mut s.transfers, None, op, [(a, b)]));
                }
                let p = |i: usize| s.transfers[i].progress.clone();
                p(0).size(2_858_000_000);
                p(0).begin();
                p(0).add_bytes(54_760_000);
                p(1).size(1_240_000_000);
                p(2).cancel();
                p(2).end(Err(ops::CANCELLED.into()));
                p(3).size(900_000_000);
                p(3).begin();
                p(3).end(Err("Não há espaço suficiente no disco.".into()));
            }),
            // The shared drives list of an account, without a network.
            ("shared", |s| {
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into() });
                s.panes[1].go(Location::drive("voce@gmail.com", Space::SharedDrives));
                let d = |id: &str, name: &str| Entry { id: id.into(), name: name.into(), is_dir: true, size: None, modified: None, target: None, mime: None, md5: None };
                s.panes[1].set_entries(Ok(vec![d("0AAa", "Equipe de design"), d("0AAb", "Financeiro"), d("0AAc", "Projetos 2026")]));
            }),
        ],
        ..AppSpec::new("Open Explorer", (1280.0, 760.0), init, ui::view)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(from: usize, n: usize) -> Vec<Entry> {
        (from..from + n).map(|i| Entry { id: format!("f{i}"), name: format!("arquivo {i}"), is_dir: false, size: Some(1), modified: None, target: None, mime: None, md5: None }).collect()
    }

    #[test]
    fn a_streamed_listing_keeps_every_page() {
        let mut s = init();
        s.cache = cache::Cache::memory_only();
        s.panes[1].go(Location::Drive { email: "t@t".into(), space: Space::Shared { id: "0AAx".into(), name: "x".into() }, path: vec![("big".into(), "Grande".into())] });
        s.live[1].store(7, Ordering::Relaxed);
        let pages = [1000, 1000, 1000, 1000, 1000, 720];
        let mut from = 0;
        for (i, n) in pages.iter().enumerate() {
            let part = Part::Page { first: i == 0, more: i + 1 < pages.len() };
            s.outbox.send(Msg::Listed { pane: 1, request: 7, made: None, part, result: Ok(entries(from, *n)) }).unwrap();
            from += n;
        }
        poll(&mut s, 0.0, &mut Tx::default());
        assert_eq!(s.panes[1].entries.len(), 5720);
        assert!(!s.panes[1].loading);
    }
}
