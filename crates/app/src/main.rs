//! Open Explorer: two panes side by side, each showing the local disk, a simulated drive or a
//! connected Google Drive account.
//!
//! Drive calls block, so they run on background threads that send a [`Msg`] back and wake the
//! window; `poll` applies it.

// A release build opens as a window alone on Windows, with no console behind it; a debug build
// keeps the console, for what it prints.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod address;
mod cache;
mod downloads;
#[cfg(windows)]
mod dragout;
mod finder;
mod icons;
mod layout;
mod model;
mod network;
mod ops;
mod settings;
mod sharing;
mod places;
mod relay;
mod servers;
mod sysicons;
mod transfers;
mod tree;
mod undo;
mod uploads;
mod ui;
mod watch;
mod work;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::SystemTime;

use openexplorer_gdrive::store::{self, StoredAccount};
use openexplorer_gdrive::{ClientConfig, Drive, DriveFile, Quota, oauth};
use rune::{AppSpec, DragItem, Key, KeyEvent, Mods, Tx, Waker};

use model::{Entry, Location, MockDrive, Pane, SortCol, Space};

const ROW_H: f32 = 22.0;

enum Msg {
    /// Part of a folder listing.
    Listed { pane: usize, request: u64, part: Part, result: Result<Vec<Entry>, String> },
    LoggedIn { pane: usize, attempt: u64, result: Result<(StoredAccount, Arc<Drive>), String> },
    /// An account a background job had to open, for `poll` to keep (see [`AccountRef::open`]).
    Opened { email: String, drive: Arc<Drive> },
    /// The folders of tree node `id`, read on a thread or from Drive.
    /// `hidden`: the Exibir arquivos ocultos the folders were read with, where it matters.
    TreeKids { id: tree::NodeId, hidden: Option<bool>, result: Result<Vec<tree::Node>, String> },
    /// The Adicionar servidor dialog's login check `attempt` ended; `secret` is the password to
    /// keep with the server (`None`: the one stored).
    ServerChecked { attempt: u64, result: Result<openexplorer_remote::Server, openexplorer_remote::Refusal>, secret: Option<String> },
    /// Computers for Rede; `done` once the network search ended.
    Servers { found: Vec<String>, done: bool },
    /// A file operation finished: reload the panes showing `reload`, and say how it went.
    Done { reload: Vec<Location>, result: Result<String, String> },
    /// Something Propriedades number `id` was waiting for.
    Prop { id: u64, update: actions::PropUpdate },
    /// Windows' icon for a file type, a folder or a drive.
    Icon { key: sysicons::IconKey, rgba: Option<sysicons::Rgba> },
    /// What the Drive folder an upload goes into holds, to check its names against.
    Clashes { up: actions::Upload, result: Result<Vec<Entry>, String> },
    /// An account's storage, for the status bar.
    Quota { email: String, result: Result<Quota, String> },
    /// What changed in `email`'s Drive since it was last asked (see [`watch`]).
    Changes { email: String, result: Result<watch::Fetched, String> },
    /// Something the Compartilhar dialog for Drive item `item` was waiting for.
    Share { item: String, update: sharing::Update },
    /// What Copiar link found out about an item's access.
    Link(sharing::LinkAnswer),
    /// The answer of a search of the computer for pane `pane`.
    Found { pane: usize, request: u64, result: Result<Vec<Entry>, String>, info: finder::Info },
    /// Where pane `pane` goes (Mostrar na pasta, the address), looked up when it had made
    /// `moves` moves (see [`model::Pane::moves`]).
    Reached { pane: usize, moves: u64, result: Result<address::Reached, String> },
    /// An action that can be undone ended, or an undo did (see [`undo::arrived`]).
    Undo(undo::Answer),
    /// Installing the search service ended: search again in the panes `reload` shows.
    Installed { reload: Vec<Location>, result: Result<String, String> },
    /// A drag of Drive items `ids` out of pane `from` ended.
    #[cfg(windows)]
    DragEnded { from: usize, ids: Vec<String>, ended: dragout::Ended },
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
    /// Uploads whose clash dialog waits for the one open (or another dialog) to close.
    clash_waiting: std::collections::VecDeque<actions::ClashAsk>,
    /// The name typed in Renomear or Nova pasta, or the e-mail typed in Compartilhar.
    name_edit: rune::TextEdit,
    /// The item being renamed in place, and its pane.
    renaming: Option<(usize, Entry)>,
    /// Each pane's search field.
    search: [rune::TextEdit; 2],
    /// Each pane's path bar while it's a field to type an address in.
    address: [Option<rune::TextEdit>; 2],
    /// The path bar's right-click menu: the pane it was opened in and where.
    address_menu: Option<(usize, rune::PointerPress)>,
    /// Each local pane's field as last seen, and when it may be searched: typing searches the
    /// computer once the keys stop for a moment.
    typed: [(String, f64); 2],
    /// The words each pane last searched the computer for.
    searched: [String; 2],
    /// What the index said with each pane's search results.
    found: [Option<finder::Info>; 2],
    /// The search service is being installed.
    installing_finder: bool,
    /// When each pane's search of the computer is asked again, while a disk is still being read
    /// or the service is starting (0: not).
    found_again: [f64; 2],
    /// Counts Propriedades dialogs, to tell their background answers apart.
    props_seq: u64,
    /// What Copiar or Recortar picked up.
    clip: Option<actions::Clip>,
    /// What Desfazer can take back, and Refazer do again.
    history: undo::History,
    /// Each pane's tree width, which its splitter changes.
    layout: layout::Layout,
    /// The disks "Este Computador" lists, read at start.
    local_drives: Vec<(PathBuf, model::DriveKind)>,
    /// The desktop and the user's folders, read at start.
    places: places::Places,
    /// What each pane's tree has open, and the folders read for it.
    tree: tree::Tree,
    /// Each pane's current listing request, bumped by every load: `poll` drops answers for an
    /// older one, and the listing thread stops fetching pages once the pane has moved on.
    live: [Arc<AtomicU64>; 2],
    cache: cache::Cache,
    mock: MockDrive,
    accounts: Vec<StoredAccount>,
    /// The FTP, SFTP and WebDAV servers added.
    servers: servers::Servers,
    /// Connected accounts by e-mail, made on first use.
    drives: HashMap<String, Arc<Drive>>,
    /// Each account's storage, asked for when its pane first opens and again after changes
    /// there: `None` while the first answer is on its way, or when it failed.
    quotas: HashMap<String, Option<Quota>>,
    /// Each account's place in its Drive change log, which keeps the panes current.
    watch: HashMap<String, watch::Watch>,
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
    /// What the user picked in Opções.
    settings: settings::Settings,
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
    let settings = settings::Settings::load();
    let mock = MockDrive::new();
    let mut panes = [Pane::new(Location::home()), Pane::new(Location::Mock(Vec::new()))];
    for p in &mut panes {
        p.load_now(&mock, settings.show_hidden);
    }
    let accounts = store::accounts();
    let tree = tree::Tree::new(accounts.iter().map(|a| a.email.clone()));
    State {
        panes,
        menu: None,
        dialog: None,
        clash_waiting: Default::default(),
        name_edit: rune::TextEdit::new(""),
        renaming: None,
        search: [rune::TextEdit::new(""), rune::TextEdit::new("")],
        address: Default::default(),
        address_menu: None,
        typed: Default::default(),
        searched: Default::default(),
        found: Default::default(),
        installing_finder: false,
        found_again: [0.0; 2],
        props_seq: 0,
        clip: None,
        history: Default::default(),
        local_drives: model::local_drives(),
        places: places::places(),
        tree,
        layout: layout::Layout::load(),
        live: Default::default(),
        cache: Default::default(),
        mock,
        accounts,
        servers: servers::Servers::load(),
        drives: HashMap::new(),
        quotas: HashMap::new(),
        watch: HashMap::new(),
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
        settings,
        transfers: Vec::new(),
        transfer_menu: None,
        transfers_live: 0,
        icons: Default::default(),
        icons_for: [u64::MAX; 2],
    }
}

fn setup(s: &mut State, waker: Waker) {
    let fixed = tree::fixed_icons(&s.places, &s.local_drives).into_iter().chain(sysicons::home_key());
    s.icons.start(rune::ImageCache::new(waker.clone()), s.outbox.clone(), waker.clone(), fixed);
    s.waker = Some(waker);
    // Each tree opens down to the folder its pane starts in.
    for p in 0..2 {
        tree::follow(s, p);
    }
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

/// How long the keys must pause before what's typed is searched, in seconds.
const TYPING_PAUSE: f64 = 0.12;
/// How often a search is asked again while a disk is still being read, in seconds.
const FOUND_AGAIN: f64 = 1.5;

fn poll(s: &mut State, now: f64, tx: &mut Tx) -> bool {
    // Each listing's file types get their icons asked for once.
    let versions = s.panes.each_ref().map(|p| p.version);
    if s.icons_for != versions {
        let relisted: Vec<usize> = (0..2).filter(|&p| s.icons_for[p] != versions[p]).collect();
        s.icons_for = versions;
        let keys = s.panes.iter().flat_map(|p| sysicons::keys_of(&p.entries, &p.loc)).collect();
        s.icons.show(keys);
        // The trees take in the folders a new listing shows.
        for p in relisted {
            tree::learn(s, p);
        }
    }
    let mut changed = s.icons.settle();
    changed |= notice(s, now);
    // A local pane searches the computer as its field is typed in, once the keys pause.
    for p in 0..2 {
        if s.search[p].text != s.typed[p].0 {
            s.typed[p] = (s.search[p].text.clone(), now + TYPING_PAUSE);
            tx.wake_at(now + TYPING_PAUSE);
        } else if s.typed[p].1 > 0.0 && now >= s.typed[p].1 {
            s.typed[p].1 = 0.0;
            if s.search[p].text != s.searched[p] {
                if s.panes[p].loc.is_local() {
                    actions::search_computer(s, p);
                    changed = true;
                } else if let Location::Server { .. } = s.panes[p].loc {
                    actions::search_server(s, p);
                    changed = true;
                }
            }
        }
        if s.found_again[p] > 0.0 && now >= s.found_again[p] {
            s.found_again[p] = 0.0;
            if matches!(s.panes[p].loc, Location::Found(_)) && !s.panes[p].loading {
                load(s, p);
            }
        }
    }
    // Rows done leave the queue when Opções says so (the status line says how the job went);
    // cancelled and failed ones stay to be seen. While any runs, its thread wakes the window a few times a second.
    // A redraw is due while any row moves, and once more when the last one stops.
    let rows = s.transfers.len();
    if s.settings.clear_completed {
        s.transfers.retain(|t| t.progress.stage() != ops::Stage::Done);
    }
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
            Msg::Opened { email, drive } => keep_drive(s, email, drive),
            Msg::TreeKids { id, hidden, result } => tree::arrived(s, id, hidden, result),
            Msg::Servers { found, done } => tree::servers_found(s, found, done),
            Msg::ServerChecked { attempt, result, secret } => servers::checked(s, attempt, result, secret),
            Msg::Listed { pane, request, part, result } => {
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
            Msg::Done { reload, result } => {
                match &result {
                    Ok(msg) => openexplorer_gdrive::log(&format!("operação: {msg}")),
                    Err(msg) => openexplorer_gdrive::log(&format!("erro: {msg}")),
                }
                if let Some(actions::Dialog::Options(opt)) = &mut s.dialog {
                    opt.refresh_logs();
                }
                match result {
                    Ok(msg) | Err(msg) => s.status = msg,
                }
                for p in 0..2 {
                    if reload.contains(&s.panes[p].loc) {
                        load(s, p);
                    }
                }
                // An upload, a copy or a delete there changes what the account uses.
                let mut touched: Vec<&String> = reload.iter().filter_map(|l| if let Location::Drive { email, .. } = l { Some(email) } else { None }).collect();
                touched.dedup();
                for email in touched {
                    ask_quota(s, email);
                }
            }
            Msg::Undo(answer) => undo::arrived(s, answer),
            Msg::Changes { email, result } => watch::arrived(s, email, result, now),
            Msg::Quota { email, result } => {
                if s.accounts.iter().any(|a| a.email == email) {
                    match result {
                        Ok(q) => {
                            s.quotas.insert(email, Some(q));
                        }
                        // Only the status bar misses it: logged, not shown.
                        Err(e) => openexplorer_gdrive::log(&format!("cota de {email}: {e}")),
                    }
                }
            }
            Msg::Share { item, update } => sharing::update(s, &item, update),
            Msg::Link(answer) => sharing::link_answer(s, answer, tx),
            #[cfg(windows)]
            Msg::DragEnded { from, ids, ended } => drive_drag_ended(s, from, ids, ended),
            Msg::Found { pane, request, result, info } => {
                if s.live[pane].load(Ordering::Relaxed) != request {
                    continue;
                }
                s.panes[pane].set_entries(result);
                // Partial: asked again until every disk is read.
                if !info.indexing.is_empty() || info.service == finder::Service::Starting {
                    s.found_again[pane] = now + FOUND_AGAIN;
                    tx.wake_at(s.found_again[pane]);
                }
                s.found[pane] = Some(info);
                drop_stale_rename(s, pane);
            }
            Msg::Installed { reload, result } => {
                s.installing_finder = false;
                s.status = match result {
                    Ok(m) | Err(m) => m,
                };
                for p in 0..2 {
                    if reload.contains(&s.panes[p].loc) {
                        load(s, p);
                    }
                }
            }
            Msg::Prop { id, update } => actions::props_update(s, id, update),
            Msg::Icon { key, rgba } => s.icons.arrived(key, rgba),
            Msg::Clashes { up, result } => actions::clashes_listed(s, up, result),
            Msg::LoggedIn { pane, attempt, result } => {
                let current = s.login == Some(attempt);
                if current {
                    s.login = None;
                } else if result.is_err() {
                    continue;
                }
                match result {
                    Ok((account, drive)) => {
                        openexplorer_gdrive::log(&format!("conta Google conectada: {}", account.email));
                        s.status = format!("Conta conectada: {}", account.email);
                        s.drives.insert(account.email.clone(), drive);
                        s.tree.account_added(&account.email);
                        let email = account.email.clone();
                        match s.accounts.iter_mut().find(|a| a.email == account.email) {
                            Some(a) => *a = account,
                            None => s.accounts.push(account),
                        }
                        navigate(s, pane, Location::drive(email, Space::MyDrive));
                    }
                    Err(e) => {
                        openexplorer_gdrive::log(&format!("erro login Google: {e}"));
                        s.status = e;
                    }
                }
            }
            Msg::Reached { pane, moves, result } => {
                // The pane moved on while the place was looked for.
                if s.panes[pane].moves != moves {
                    continue;
                }
                match result {
                    Ok(to) => {
                        s.status.clear();
                        actions::arrive(s, pane, to);
                    }
                    Err(e) => s.status = e,
                }
            }
        }
    }
    // Accounts on screen are asked what changed elsewhere, every few seconds; after the
    // messages, so a listing that just ended is read again here if changes came during it.
    changed |= watch::tick(s, now);
    for p in 0..2 {
        // The address field closes when the keyboard leaves it (a click elsewhere).
        if s.address[p].as_ref().is_some_and(|a| !a.focused) {
            s.address[p] = None;
            changed = true;
        }
        // Mostrar na pasta: the item is selected and brought into view once its folder lists it.
        let pane = &mut s.panes[p];
        let Some((id, name)) = pane.reveal.clone() else { continue };
        if pane.entry(&id).is_some() {
            pane.reveal = None;
            pane.selected = [id.clone()].into();
            pane.cursor = Some(id.clone());
            pane.anchor = Some(id.clone());
            tx.scroll_into_view_in(pane_key(p), row_key(&id));
            changed = true;
        } else if !pane.loading {
            pane.reveal = None;
            if pane.error.is_none() {
                s.status = format!("“{name}” não está mais nesta pasta.");
                changed = true;
            }
        }
    }
    // Each tree brings the folder its pane shows into view, once that's in the tree.
    for p in 0..2 {
        if tree::following(s, p) {
            let lines = tree::lines(s, p);
            if let Some(key) = tree::to_reveal(s, p, &lines) {
                tx.scroll_into_view_in(ui::tree_key(p), format!("t:{key}"));
                changed = true;
            }
        }
    }
    // An upload's clash dialog waiting for another dialog to close.
    if s.dialog.is_none() && !s.clash_waiting.is_empty() {
        actions::next_clash(s);
        changed = true;
    }
    changed
}

fn entry_of(f: DriveFile) -> Entry {
    Entry { id: f.id, name: f.name, is_dir: f.is_dir, size: f.size, modified: f.modified, target: f.target, mime: Some(f.mime), md5: f.md5, starred: f.starred }
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

/// An account as a background job reaches it: by e-mail, with the [`Drive`] the app already
/// keeps for it, if any. Taken on the UI thread, opened on the job's.
struct AccountRef {
    email: String,
    kept: Option<Arc<Drive>>,
}

impl AccountRef {
    fn of(s: &State, email: &str) -> AccountRef {
        AccountRef { email: email.to_string(), kept: s.drives.get(email).cloned() }
    }

    /// The account's Drive: the one kept, or one opened now and handed to `poll` through
    /// `emit` to keep, so the next job finds it.
    fn open(self, emit: &dyn Fn(Msg)) -> Result<Arc<Drive>, String> {
        if let Some(drive) = self.kept {
            return Ok(drive);
        }
        let drive = open_drive(&self.email)?;
        emit(Msg::Opened { email: self.email, drive: drive.clone() });
        Ok(drive)
    }
}

/// Keeps an account a background job had to open, if it's still connected.
fn keep_drive(s: &mut State, email: String, drive: Arc<Drive>) {
    if s.accounts.iter().any(|a| a.email == email) {
        s.drives.entry(email).or_insert(drive);
    }
}

/// Runs `work` with `email`'s Drive on a background thread, then emits the message `done`
/// makes of what it gave. `doing` names the work in the error a panic in it ends with ("ler a
/// cota").
fn drive_task<T>(
    s: &State,
    email: &str,
    doing: &'static str,
    work: impl FnOnce(&Drive) -> Result<T, String> + Send + 'static,
    done: impl FnOnce(Result<T, String>) -> Msg + Send + 'static,
) {
    let account = AccountRef::of(s, email);
    background(s, move |emit| {
        let result = account.open(emit).and_then(|drive| catch_panic(doing, || work(&drive)));
        emit(done(result));
    });
}

pub(crate) fn connect_with(s: &mut State, p: usize, cfg: ClientConfig, tx: &mut Tx) -> Result<(), String> {
    let pending = oauth::begin(&cfg)?;
    tx.open_url(&pending.url).map_err(|e| format!("Não consegui abrir o navegador: {e:?}"))?;
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
    Ok(())
}

fn disconnect(s: &mut State, email: &str) {
    if let Err(e) = store::remove(email) {
        s.status = e;
        return;
    }
    s.drives.remove(email);
    s.quotas.remove(email);
    s.cache.forget(email);
    s.tree.forget(email);
    s.watch.remove(email);
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

/// Asks Drive how much of `email`'s storage is used, on a background thread; the status bar
/// keeps showing the last answer meanwhile.
fn ask_quota(s: &mut State, email: &str) {
    if !s.accounts.iter().any(|a| a.email == email) {
        return;
    }
    s.quotas.entry(email.to_string()).or_insert(None);
    let who = email.to_string();
    drive_task(s, email, "ler a cota", |d| d.quota(), move |result| Msg::Quota { email: who, result });
}

// ---------------------------------------------------------------------------------------------
// Navigation

/// Lists the pane's current folder: in place for local and simulated folders, in the
/// background for Drive.
fn load(s: &mut State, p: usize) {
    // Every load gets a new number, so a Drive answer for a folder left since is dropped,
    // whatever the pane shows now.
    let request = s.live[p].fetch_add(1, Ordering::Relaxed) + 1;
    // A search of the computer asks the index service, on a thread: milliseconds.
    if let Location::Found(f) = &s.panes[p].loc {
        let (f, show_hidden) = ((**f).clone(), s.settings.show_hidden);
        let pane = &mut s.panes[p];
        pane.loading = true;
        pane.error = None;
        pane.free = None;
        // In WSL or on the network, where the index has nothing: read there, the results
        // coming in pages as they're found.
        if f.within.as_deref().is_some_and(finder::elsewhere) {
            s.found[p] = None;
            let live = s.live[p].clone();
            background(s, move |emit| {
                let alive = || live.load(Ordering::Relaxed) == request;                let mut first = true;
                let result = catch_panic("pesquisar", || {
                    finder::search_elsewhere(&f, show_hidden, &alive, &mut |entries, more| {                        emit(Msg::Listed { pane: p, request, part: Part::Page { first, more }, result: Ok(entries) });                        first = false;
                    })
                });
                if let Err(e) = result {
                    emit(Msg::Listed { pane: p, request, part: Part::Page { first, more: false }, result: Err(e) });
                }
            });
            return;
        }
        background(s, move |emit| {
            let (result, info) = finder::search(&f, show_hidden);
            emit(Msg::Found { pane: p, request, result, info });
        });
        return;
    }
    // The network's folders on a thread: a computer that's off, or a distribution starting,
    // would hold the window for seconds.
    if let Location::Local(dir) = &s.panes[p].loc
        && network::is_remote(dir)
    {
        let (dir, show_hidden) = (dir.clone(), s.settings.show_hidden);
        let pane = &mut s.panes[p];
        pane.loading = true;
        pane.error = None;
        pane.free = None;
        background(s, move |emit| {
            let result = catch_panic("listar", || model::read_local(&dir, show_hidden).map_err(|e| format!("{}: {e}", dir.display())));
            emit(Msg::Listed { pane: p, request, part: Part::WHOLE, result });
        });
        return;
    }
    if let Location::Server { .. } = &s.panes[p].loc {
        return servers::load(s, p, request);
    }
    let Location::Drive { email, space, path } = s.panes[p].loc.clone() else {
        let show_hidden = s.settings.show_hidden;
        let State { panes, mock, .. } = s;
        panes[p].load_now(mock, show_hidden);
        drop_stale_rename(s, p);
        return;
    };
    // The account's storage, the first time one of its folders opens.
    if !s.quotas.contains_key(&email) {
        ask_quota(s, &email);
    }
    let account = AccountRef::of(s, &email);
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
        let listed = |part, result| Msg::Listed { pane: p, request, part, result };
        // Else one from an earlier run, read here off the UI thread.
        if !quiet && let Some(entries) = key.as_ref().and_then(cache::load_disk) {
            emit(listed(Part::Cached, Ok(entries)));
            quiet = true;
        }
        let drive = match account.open(emit) {
            Ok(d) => d,
            Err(e) => return emit(listed(Part::Page { first: !quiet, more: false }, Err(e))),
        };
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
                    emit(listed(Part::WHOLE, Ok(std::mem::take(&mut gathered))));
                }
            } else {
                emit(listed(Part::Page { first, more }, Ok(entries.collect())));
                first = false;
            }
            live.load(Ordering::Relaxed) == request
        };
        // A panic here (a bad value from Drive, say) must not leave the pane loading forever.
        let result = catch_panic("listar", || match (&space, folder.as_deref()) {
            (Space::MyDrive, f) => drive.list(f.unwrap_or("root"), None, &mut page),
            (Space::Shared { id, .. }, f) => drive.list(f.unwrap_or(id), Some(id), &mut page),
            (Space::Trash, Some(f)) => drive.list_trashed(f, &mut page),
            // A folder opened from Compartilhados comigo, Com estrela or Recentes lists as any
            // folder, in whichever drive it sits.
            (Space::SharedWithMe | Space::Starred | Space::Recent | Space::Search(_), Some(f)) => drive.list(f, None, &mut page),
            (Space::Search(q), None) => drive.search(&q.text, &q.within, &mut page),
            (Space::SharedWithMe | Space::Starred | Space::Recent | Space::Trash, None) => match space.view() {
                Some(v) => drive.view(v, &mut page),
                None => Ok(()),
            },
            // The shared drives show as folders.
            (Space::SharedDrives, _) => drive.shared_drives().map(|list| {
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
                        starred: false,
                    })
                    .collect::<Vec<_>>()
            }).map(|entries| emit(listed(Part::WHOLE, Ok(entries)))),
        });
        if let Err(e) = result {
            // Over a cached or partly shown listing, keep it and only report the error.
            emit(listed(Part::Page { first: first && !quiet, more: false }, Err(e)));
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
    tree::follow(s, p);
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
        return actions::open_file(s, p, &e);
    }
    let mut loc = pane.loc.clone();
    match &mut loc {
        // A folder found opens where it is.
        Location::Found(_) => loc = Location::Local(PathBuf::from(&e.id)),
        Location::Local(dir) => dir.push(&e.name),
        Location::Mock(segs) => segs.push(e.name.clone()),
        Location::Drive { space: space @ Space::SharedDrives, .. } => *space = Space::Shared { id: e.id.clone(), name: e.name.clone() },
        Location::Drive { path, .. } => path.push((e.target.clone().unwrap_or_else(|| e.id.clone()), e.name.clone())),
        // A folder found opens where it is.
        Location::Server { server, .. } => {
            let segs = servers::segs_of(&pane.loc, &e);
            loc = servers::folder(server, segs);
        }
    }
    navigate(s, p, loc);
}

fn go_up(s: &mut State, p: usize) {
    let mut loc = s.panes[p].loc.clone();
    // The folder we come out of, to land on it.
    let from = match &mut loc {
        Location::Local(dir) => {
            // From a shared folder up to its computer too, which `PathBuf::pop` doesn't do.
            let Some(up) = network::parent(dir) else { return };
            let name = network::name(dir);
            *dir = up;
            name
        }
        Location::Mock(segs) => segs.pop(),
        Location::Found(_) => None,
        // From a search's results back to the folder searched.
        Location::Server { find: find @ Some(_), .. } => {
            *find = None;
            return navigate(s, p, loc);
        }
        Location::Server { path, .. } => path.pop(),
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
        // Ctrl+Shift+Z first: `is_command` matches whatever else is held.
        _ if ev.is_command('y') || (ev.is_command('z') && ev.mods.shift) => {
            undo::undo(s, true);
            return true;
        }
        _ if ev.is_command('z') => {
            undo::undo(s, false);
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
        // The address, to edit, as in Explorer.
        Key::Function(4) => {
            actions::edit_address(s, p);
            return true;
        }
        _ if ev.is_command('l') => {
            actions::edit_address(s, p);
            return true;
        }
        Key::Text(t) if ev.mods.alt && t.eq_ignore_ascii_case("d") => {
            actions::edit_address(s, p);
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
        _ if ev.is_command('f') && !matches!(s.panes[p].loc, Location::Mock(_)) => {
            tx.focus(ui::search_key(p), true);
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
    // Drive and server items go out as virtual files through a drag of our own; Rune's needs
    // files on disk.
    #[cfg(windows)]
    if matches!(s.panes[p].loc, Location::Drive { .. } | Location::Server { .. }) {
        start_drive_drag(s, p, dragged_ids(&s.panes[p], id));
        return None;
    }
    let paths = paths_for(s, p, &dragged_ids(&s.panes[p], id), true);
    if paths.is_empty() {
        return None;
    }
    Some(paths.into_iter().fold(DragItem::new(), DragItem::file).within_app())
}

/// Files on disk for the dragged entries, written first when `materialize` is set; without it
/// this only names the paths. Simulated files become small placeholder files in the temp
/// folder. Drive items have none: they're dragged as virtual files (see [`start_drive_drag`]).
fn paths_for(s: &State, p: usize, ids: &[String], materialize: bool) -> Vec<PathBuf> {
    let pane = &s.panes[p];
    match &pane.loc {
        Location::Local(_) | Location::Found(_) => ids.iter().filter_map(|id| pane.entry(id)).filter_map(|e| pane.loc.local_path(e)).collect(),
        Location::Drive { .. } | Location::Server { .. } => Vec::new(),
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

/// Starts dragging the Drive (or server) items `ids` of pane `p` out as virtual files (see
/// [`dragout`]): nothing is fetched until the drop, so any number and size of files and folders
/// go.
#[cfg(windows)]
fn start_drive_drag(s: &State, p: usize, ids: Vec<String>) {
    let pane = &s.panes[p];
    let items: Vec<&Entry> = ids.iter().filter_map(|id| pane.entry(id)).collect();
    if items.is_empty() {
        return;
    }
    let source: Arc<dyn dragout::Files> = match &pane.loc {
        Location::Drive { email, .. } => {
            let Some(drive) = s.drives.get(email).cloned() else { return };
            Arc::new(dragout::DriveSource { drive, files: items.iter().map(|e| actions::drive_file(e)).collect() })
        }
        Location::Server { server, .. } => {
            let top = items.iter().filter_map(|e| servers::path_of(&pane.loc, e).map(|path| (path, (*e).clone()))).collect();
            Arc::new(relay::ServerFiles { pool: s.servers.pool.clone(), server: (**server).clone(), top })
        }
        _ => return,
    };
    // SAFETY: plain Win32 queries; the window pressed on is in front, and is this process's.
    let hwnd = unsafe {
        let w = windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow();
        let mut pid = 0;
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(w, &mut pid);
        if pid == std::process::id() { w as isize } else { 0 }
    };
    if hwnd == 0 {
        return;
    }
    let (out, wake) = (s.outbox.clone(), s.waker.clone());
    dragout::start(
        hwnd,
        source,
        Box::new(move |ended| {
            let _ = out.send(Msg::DragEnded { from: p, ids, ended });
            if let Some(w) = &wake {
                w();
            }
        }),
    );
}

/// A drag of Drive items ended: let go over the other pane, they're copied there as the
/// arrows between the panes copy; let go over another app, that app is pulling them.
#[cfg(windows)]
fn drive_drag_ended(s: &mut State, from: usize, ids: Vec<String>, ended: dragout::Ended) {
    match ended {
        dragout::Ended::InApp(to) if to != from => {
            let pane = &mut s.panes[from];
            pane.selected = ids.into_iter().collect();
            s.active = to;
            actions::copy_across(s, from, to);
        }
        dragout::Ended::Outside => s.status = "Baixando para onde os itens foram soltos: o Windows mostra o andamento.".into(),
        _ => {}
    }
}

fn found_scene(s: &mut State, installed: bool) {
    let f = finder::found("relatorio", Some(PathBuf::from(r"C:\Users\voce")));
    s.search[0] = rune::TextEdit::new("relatorio");
    s.typed[0] = ("relatorio".into(), 0.0);
    s.searched[0] = "relatorio".into();
    s.panes[0].go(Location::Found(Box::new(f)));
    if installed {
        s.panes[0].set_entries(Ok(finder::sample()));
        s.found[0] = Some(finder::Info { total: 1234, shown: 4, indexing: vec![r"D:\".into()], service: finder::Service::Running });
    } else {
        s.panes[0].set_entries(Err(finder::MISSING.into()));
        s.found[0] = Some(finder::Info { total: 0, shown: 0, indexing: Vec::new(), service: finder::Service::Missing });
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
        Location::Found(_) => {
            s.status = "Os resultados da pesquisa não são uma pasta: solte os arquivos numa pasta.".into();
            return;
        }
        Location::Local(dir) if network::is_server(dir) => {
            s.status = "Aqui estão as pastas compartilhadas do computador: solte os arquivos numa delas.".into();
            return;
        }
        Location::Server { find: Some(_), .. } => {
            s.status = "Os resultados da pesquisa não são uma pasta: solte os arquivos numa pasta.".into();
            return;
        }
        Location::Server { .. } => {
            let real: Vec<PathBuf> = files.iter().filter(|f| !f.starts_with(&s.temp)).cloned().collect();
            if real.is_empty() {
                s.status = "Nada para enviar: arraste arquivos ou pastas do computador.".into();
                return;
            }
            servers::upload(s, real, &loc, false, vec![loc.clone()]);
            return;
        }
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
                    starred: false,
                };
                if s.mock.add(segs, entry) { done += 1 } else { skipped += 1 }
            }
        }
    }
    s.status = format!("Enviados (simulado): {done} · ignorados (já existem): {skipped}");
    load(s, p);
}

/// The clash dialog over a fake account's Drive folder, for sample files in %TEMP%.
fn clash_scene(s: &mut State, all: bool) {
    let dir = std::env::temp_dir().join("openexplorer-clash-scene");
    let _ = std::fs::create_dir_all(dir.join("Projetos"));
    for name in ["Relatório.pdf", "foto.jpg", "notas.txt"] {
        let _ = std::fs::write(dir.join(name), name);
    }
    let to = Location::Drive { email: "voce@gmail.com".into(), space: Space::MyDrive, path: vec![("x".into(), "Trabalho".into())] };
    s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into() });
    switch(s, 0, Location::Local(dir.clone()));
    s.panes[1].go(to.clone());
    let e = |id: &str, name: &str, mime: &str| Entry {
        id: id.into(),
        name: name.into(),
        is_dir: mime == openexplorer_gdrive::api::FOLDER_MIME,
        size: Some(120_000),
        modified: Some(SystemTime::now()),
        target: None,
        mime: Some(mime.into()),
        md5: None,
        starred: false,
    };
    let existing = vec![e("a", "Relatório.pdf", "application/pdf"), e("b", "foto.jpg", "image/jpeg"), e("c", "Projetos", openexplorer_gdrive::api::FOLDER_MIME)];
    s.panes[1].set_entries(Ok(existing.clone()));
    let sources = ["Projetos", "Relatório.pdf", "foto.jpg", "notas.txt"].map(|n| dir.join(n)).to_vec();
    let up = actions::Upload { email: "voce@gmail.com".into(), parent: "x".into(), to: to.clone(), sources, remove: false, reload: vec![to] };
    let chosen = if all { Vec::new() } else { vec![uploads::Clash::KeepBoth] };
    s.dialog = Some(actions::Dialog::Clash(Box::new(actions::ClashAsk { up, existing, clashes: vec![0, 1, 2], chosen, all })));
}

/// The scenes' server: `rclone serve webdav <folder> --addr 127.0.0.1:28095`.
fn server_scene(s: &mut State, find: Option<&str>) {
    let server = openexplorer_remote::Server {
        id: "cena".into(),
        name: "NAS de teste".into(),
        protocol: openexplorer_remote::Protocol::Dav,
        brand: None,
        host: "127.0.0.1".into(),
        port: 28095,
        user: String::new(),
        root: "/".into(),
        key: None,
        host_key: None,
        cert: None,
        connections: 4,
        implicit: false,
    };
    s.servers.list = vec![server.clone()];
    let loc = Location::Server { server: Box::new(server), path: Vec::new(), find: find.map(str::to_string) };
    if let Some(text) = find {
        s.search[0] = rune::TextEdit::new(text);
        s.searched[0] = text.to_string();
        s.typed[0] = (text.to_string(), 0.0);
    }
    switch(s, 0, loc);
}

/// A few Drive items for the scenes of Drive's views.
fn view_sample() -> Vec<Entry> {
    let ago = |days: u64| Some(SystemTime::now() - std::time::Duration::from_secs(days * 86_400));
    let e = |id: &str, name: &str, is_dir: bool, size: Option<u64>, days, mime: Option<&str>| Entry {
        id: id.into(),
        name: name.into(),
        is_dir,
        size,
        modified: ago(days),
        target: None,
        mime: mime.map(str::to_string),
        md5: None,
        starred: false,
    };
    vec![
        e("f1", "Projetos antigos", true, None, 40, None),
        e("f2", "Relatório 2025.pdf", false, Some(1_204_330), 12, None),
        e("f3", "Orçamento", false, None, 3, Some("application/vnd.google-apps.spreadsheet")),
        e("f4", "foto_praia.jpg", false, Some(3_480_112), 90, None),
    ]
}

fn main() {
    // `openexplorer index …`: this executable as the search service (Windows starts it so), no
    // window at all.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == openexplorer_index::cli::ARG) {
        std::process::exit(openexplorer_index::cli::run(&args[1..]));
    }
    let initial = settings::Settings::load();
    rune::App::new(AppSpec::<State> {
        min_size: Some((900.0, 420.0)),
        families: initial.font.families(),
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
            // Right after a rename in a scratch folder: the notice offers Desfazer.
            ("undo", |s| {
                let dir = std::env::temp_dir().join("openexplorer-undo-scene");
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(dir.join("notas.txt"), "txt");
                switch(s, 0, Location::Local(dir.clone()));
                let steps = vec![undo::Step::Move { from: dir.join("notas.txt"), to: dir.join("nota.txt") }];
                let record = undo::Record { what: "renomear “nota.txt” para “notas.txt”".into(), steps, reload: vec![Location::Local(dir)] };
                let said = "Renomeado para \"notas.txt\".".to_string();
                undo::arrived(s, undo::Answer::Did { record: Some(record), reload: Vec::new(), result: Ok(said.clone()) });
                // Said at once, as the one frame drawn may come before `poll` reads the inbox.
                s.status = said;
            }),
            // This computer's own shared folders (as the network shows them) and WSL's
            // distributions, read only.
            ("network", |s| {
                switch(s, 0, Location::Local(network::server_path("localhost")));
                switch(s, 1, Location::Local(network::server_path(network::WSL)));
            }),
            // A WebDAV server on this computer (`rclone serve webdav` on 127.0.0.1:28095, no
            // login) in the left pane, its tree open; nothing is saved to the user's list.
            ("server", |s| server_scene(s, None)),
            // The same server searched for "praia".
            ("server-search", |s| server_scene(s, Some("praia"))),
            // Unified Adicionar conta picker, plus provider-specific steps.
            ("add-account", |s| servers::ask_add(s)),
            ("add-account-nas", |s| {
                let mut f = servers::Form::new();
                f.category = servers::Category::Nas;
                s.dialog = Some(actions::Dialog::Server(Box::new(f)));
            }),
            ("add-gdrive", |s| {
                let mut f = servers::Form::for_provider(servers::Provider::GoogleDrive);
                f.custom_oauth = true;
                s.dialog = Some(actions::Dialog::Server(Box::new(f)));
            }),
            ("add-nextcloud", |s| {
                let mut f = servers::Form::for_provider(servers::Provider::Nextcloud);
                f.host = rune::TextEdit::new("cloud.exemplo.com");
                f.user = rune::TextEdit::new("ana");
                s.dialog = Some(actions::Dialog::Server(Box::new(f)));
            }),
            // Adicionar conta, filled in for SFTP; and for plain FTP, with its warning.
            ("add-server", |s| {
                let mut f = servers::Form::for_provider(servers::Provider::Sftp);
                f.host = rune::TextEdit::new("nas.local");
                f.user = rune::TextEdit::new("ana");
                f.password = rune::TextEdit::new("segredo").secure();
                s.dialog = Some(actions::Dialog::Server(Box::new(f)));
            }),
            ("add-server-ftp", |s| {
                let mut f = servers::Form::for_provider(servers::Provider::Ftp);
                f.protocol = openexplorer_remote::Protocol::Ftp;
                f.host = rune::TextEdit::new("192.168.0.10");
                f.error = Some("192.168.0.10 recusou o usuário ou a senha.".into());
                s.dialog = Some(actions::Dialog::Server(Box::new(f)));
            }),
            // A NAS's own certificate, turned down: the dialog shows it and asks to trust it.
            ("add-server-cert", |s| {
                let mut f = servers::Form::for_provider(servers::Provider::WebDav);
                f.protocol = openexplorer_remote::Protocol::Davs;
                f.host = rune::TextEdit::new("nas.local");
                f.user = rune::TextEdit::new("ana");
                f.untrusted = Some(openexplorer_remote::Rejected {
                    fingerprint: "75:25:66:B8:60:50:B3:6B:5E:1A:58:FE:38:DE:1A:DB:F5:D6:86:41:DC:8C:9F:50:75:A6:E5:33:69:5A:A6:36".into(),
                    why: "não foi emitido por uma autoridade conhecida (é um certificado próprio)".into(),
                });
                s.dialog = Some(actions::Dialog::Server(Box::new(f)));
            }),
            // Opções open on each sidebar tab.
            ("options", |s| s.dialog = Some(actions::Dialog::Options(Box::new(settings::OptionsView::open(settings::OptionsTab::General))))),
            ("options-transfers", |s| s.dialog = Some(actions::Dialog::Options(Box::new(settings::OptionsView::open(settings::OptionsTab::Transfers))))),
            ("options-experimental", |s| s.dialog = Some(actions::Dialog::Options(Box::new(settings::OptionsView::open(settings::OptionsTab::Experimental))))),
            ("options-logs", |s| {
                let mut v = settings::OptionsView::open(settings::OptionsTab::Logs);
                if v.log_lines.is_empty() {
                    v.log_lines = vec![
                        "22:14:03.118 conta Google conectada: voce@gmail.com".into(),
                        "22:14:04.402 list root: 248 ms (42 itens)".into(),
                        "22:15:19.084 servidor salvo: Nextcloud (cloud.exemplo.com) (https://ana@cloud.exemplo.com)".into(),
                        "22:16:40.911 operação: Enviado: 3 itens.".into(),
                        "22:18:02.330 erro: 192.168.0.10 recusou o usuário ou a senha.".into(),
                    ];
                    v.log_bytes = 412;
                }
                s.dialog = Some(actions::Dialog::Options(Box::new(v)));
            }),
            // The address being edited, all selected.
            ("address", |s| actions::edit_address(s, 0)),
            // Drive names holding `/`, shown with `／` in the path bar, the list and the rename
            // field, without a network.
            ("slash", |s| {
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into() });
                s.panes[1].go(Location::Drive { email: "voce@gmail.com".into(), space: Space::MyDrive, path: vec![("x".into(), "Séries/Filmes".into())] });
                let mut entries = view_sample();
                entries[0].name = "tes/test".into();
                entries[1].name = "Fotos 2024/25.pdf".into();
                // Renomear open on the second one, as F2 opens it.
                let renamed = entries[1].clone();
                s.panes[1].set_entries(Ok(entries));
                s.panes[1].selected = [renamed.id.clone()].into();
                s.name_edit = actions::name_field(&renamed);
                s.renaming = Some((1, renamed));
            }),
            // The path bar's menu over a Drive folder (with Copiar link do Drive), without a
            // network.
            ("address-menu", |s| {
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into() });
                s.panes[1].go(Location::Drive { email: "voce@gmail.com".into(), space: Space::MyDrive, path: vec![("x".into(), "Projetos".into())] });
                s.panes[1].set_entries(Ok(MockDrive::new().list(&["Documentos".into()])));
                s.address_menu = Some((1, rune::PointerPress { x: 820.0, y: 92.0 }));
            }),
            // A search of the computer with results, a disk still being read; and one without
            // the service installed.
            ("found", |s| found_scene(s, true)),
            ("found-missing", |s| found_scene(s, false)),
            // An upload whose names are taken in the Drive folder: all at once, or the second
            // of three asked on its own.
            ("clash", |s| clash_scene(s, true)),
            ("clash-each", |s| clash_scene(s, false)),
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
                    (Op::Download, End::drive(&drive, Some("Fotos 2026")), End::local(&PathBuf::from(r"C:\Users\voce\Downloads"))),
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
                p(4).add_items(120);
                p(4).size(640_000_000);
                p(4).begin();
                p(4).add_bytes(212_000_000);
                for _ in 0..37 {
                    p(4).item_done();
                }
            }),
            // Lixeira of an account with its menu open on an item, without a network.
            ("trash", |s| {
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into() });
                s.panes[1].go(Location::drive("voce@gmail.com", Space::Trash));
                s.panes[1].set_entries(Ok(view_sample()));
                s.panes[1].selected = ["f2".to_string()].into();
                s.menu = Some((1, rune::PointerPress { x: 900.0, y: 200.0 }));
            }),
            // Excluir permanentemente asking to confirm.
            ("purge", |s| {
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into() });
                s.panes[1].go(Location::drive("voce@gmail.com", Space::Trash));
                s.panes[1].set_entries(Ok(view_sample()));
                s.panes[1].selected = ["f2".to_string(), "f3".to_string()].into();
                actions::ask_delete(s, 1);
            }),
            // Esvaziar lixeira asking to confirm.
            ("empty-trash", |s| {
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into() });
                s.panes[1].go(Location::drive("voce@gmail.com", Space::Trash));
                s.panes[1].set_entries(Ok(view_sample()));
                actions::ask_empty_trash(s, 1);
            }),
            // Com estrela with the menu on a starred file, and the views in the tree.
            ("starred", |s| {
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into() });
                s.panes[1].go(Location::drive("voce@gmail.com", Space::Starred));
                s.panes[1].set_entries(Ok(view_sample().into_iter().map(|e| Entry { starred: true, ..e }).collect()));
                s.panes[1].selected = ["f3".to_string()].into();
                s.menu = Some((1, rune::PointerPress { x: 900.0, y: 200.0 }));
            }),
            // An account's storage in its status bar, without a network.
            ("quota", |s| {
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into() });
                s.quotas.insert("voce@gmail.com".into(), Some(Quota { limit: Some(16_106_127_360), usage: 2_898_000_000, in_drive: 2_100_000_000, in_trash: 12_000_000 }));
                s.panes[1].go(Location::drive("voce@gmail.com", Space::MyDrive));
                s.panes[1].set_entries(Ok(MockDrive::new().list(&["Documentos".into()])));
            }),
            // Compartilhar on a file shared with a few people and the link, without a network.
            ("share", |s| {
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into() });
                s.quotas.insert("voce@gmail.com".into(), Some(Quota { limit: None, usage: 734_003_200, in_drive: 600_000_000, in_trash: 0 }));
                s.panes[1].go(Location::drive("voce@gmail.com", Space::MyDrive));
                s.panes[1].set_entries(Ok(MockDrive::new().list(&["Documentos".into()])));
                let item = s.panes[1].entries.iter().find(|e| !e.is_dir).cloned().unwrap_or_else(|| s.panes[1].entries[0].clone());
                let file = serde_json::json!({"webViewLink": "https://drive.google.com/file/d/x/view", "capabilities": {"canShare": true}});
                let perms = serde_json::json!({"permissions": [
                    {"id": "1", "type": "user", "role": "owner", "emailAddress": "voce@gmail.com", "displayName": "Você"},
                    {"id": "2", "type": "user", "role": "writer", "emailAddress": "ana@exemplo.com", "displayName": "Ana Souza"},
                    {"id": "3", "type": "user", "role": "commenter", "emailAddress": "bruno@exemplo.com"},
                    {"id": "4", "type": "group", "role": "reader", "emailAddress": "equipe@exemplo.com", "displayName": "Equipe"},
                    {"id": "anyoneWithLink", "type": "anyone", "role": "reader"}
                ]});
                s.name_edit = rune::TextEdit::new("carla@exemplo.com");
                s.dialog = Some(actions::Dialog::Share(Box::new(sharing::Share {
                    email: "voce@gmail.com".into(),
                    item,
                    sharing: Some(openexplorer_gdrive::Sharing::parse(&file, &perms)),
                    busy: false,
                    error: Some("Compartilhar: HTTP 403: Sharing is restricted to members of this organization.".into()),
                    role: openexplorer_gdrive::Role::Writer,
                    notify: true,
                })));
            }),
            // The question Copiar link asks on a private item.
            ("make-public", |s| {
                let item = MockDrive::new().list(&["Documentos".into()]).into_iter().find(|e| !e.is_dir);
                if let Some(item) = item {
                    s.dialog = Some(actions::Dialog::MakePublic(Box::new(sharing::MakePublic { email: "voce@gmail.com".into(), item, link: None })));
                }
            }),
            // The shared drives list of an account, without a network.
            ("shared", |s| {
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into() });
                s.panes[1].go(Location::drive("voce@gmail.com", Space::SharedDrives));
                let d = |id: &str, name: &str| Entry { id: id.into(), name: name.into(), is_dir: true, size: None, modified: None, target: None, mime: None, md5: None, starred: false };
                s.panes[1].set_entries(Ok(vec![d("0AAa", "Equipe de design"), d("0AAb", "Financeiro"), d("0AAc", "Projetos 2026")]));
            }),
        ],
        ..AppSpec::new("Open Explorer", (1280.0, 760.0), init, ui::view)
    })
    .font_smoothing(initial.text_render.smoothing())
    .args(rune_args(std::env::args().skip(1)))
    .run();
}

/// Rune's flags: what the command line says, with scroll bars always shown on Windows unless
/// it says otherwise. Rune reads the "Show scroll bars" preference only on macOS and elsewhere
/// fades bars out at rest; on Windows, as in Explorer, a list shows where it stands.
fn rune_args(given: impl Iterator<Item = String>) -> Vec<String> {
    let mut args: Vec<String> = given.collect();
    if cfg!(windows) && !args.iter().any(|a| a == "--scroll-bars") {
        args.extend(["--scroll-bars".into(), "always".into()]);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rune gets the command line as given (so --snap and --script still work), and on Windows
    /// always-shown scroll bars unless the command line picks its own.
    #[test]
    fn rune_gets_the_command_line_and_shown_scroll_bars() {
        let args = |given: &[&str]| rune_args(given.iter().map(|a| a.to_string()));
        let shown: &[&str] = if cfg!(windows) { &["--scroll-bars", "always"] } else { &[] };
        assert_eq!(args(&["--snap", "a.png"]), [&["--snap", "a.png"][..], shown].concat());
        assert_eq!(args(&["--scroll-bars", "when-scrolling"]), ["--scroll-bars", "when-scrolling"]);
    }

    fn entries(from: usize, n: usize) -> Vec<Entry> {
        (from..from + n).map(|i| Entry { id: format!("f{i}"), name: format!("arquivo {i}"), is_dir: false, size: Some(1), modified: None, target: None, mime: None, md5: None, starred: false }).collect()
    }

    /// Dropping items whose names the Drive folder already has asks first, one by one once
    /// Aplicar a todos is off; Cancelar drops the upload, and skipping everything sends nothing.
    #[test]
    fn an_upload_into_taken_names_asks_before_starting() {
        let mut s = init();
        s.cache = cache::Cache::memory_only();
        clash_scene(&mut s, true);
        let Some(actions::Dialog::Clash(ask)) = s.dialog.take() else { panic!("no clash dialog") };
        let (to, sources) = (ask.up.to.clone(), ask.up.sources.clone());
        // The same upload as a drop on the Drive pane: checked against its listing.
        dropped(&mut s, 1, &sources);
        let Some(actions::Dialog::Clash(ask)) = &s.dialog else { panic!("no clash dialog") };
        assert_eq!(ask.clashes, [0, 1, 2]);
        assert!(ask.all);
        actions::toggle_clash_all(&mut s);
        actions::answer_clash(&mut s, Some(uploads::Clash::Skip));
        let Some(actions::Dialog::Clash(ask)) = &s.dialog else { panic!("the dialog should ask about the next one") };
        assert_eq!(ask.up.sources[ask.current()].file_name().unwrap(), "Relatório.pdf");
        actions::answer_clash(&mut s, None);
        assert!(s.dialog.is_none() && s.transfers.is_empty());
        // Every name taken, and all skipped: nothing is queued.
        let only_taken: Vec<PathBuf> = sources.iter().filter(|p| !p.ends_with("notas.txt") && !p.is_dir()).cloned().collect();
        actions::upload(&mut s, only_taken, &to, false, vec![]);
        actions::answer_clash(&mut s, Some(uploads::Clash::Skip));
        assert!(s.dialog.is_none() && s.transfers.is_empty());
        assert!(s.status.starts_with("Nada enviado"), "{}", s.status);
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
            s.outbox.send(Msg::Listed { pane: 1, request: 7, part, result: Ok(entries(from, *n)) }).unwrap();
            from += n;
        }
        poll(&mut s, 0.0, &mut Tx::default());
        assert_eq!(s.panes[1].entries.len(), 5720);
        assert!(!s.panes[1].loading);
    }
}
