//! Open Explorer: two panes side by side, each showing the local disk, a simulated drive or a
//! connected Google Drive account.
//!
//! Drive calls block, so they run on background threads that send a [`Msg`] back and wake the
//! window; `poll` applies it.

// A release build opens as a window alone on Windows, with no console behind it; a debug build
// keeps the console, for what it prints.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod accounts;
mod actions;
mod address;
mod anim;
mod autoscroll;
mod b2;
mod cache;
mod caches;
mod downloads;
mod dropbox;
#[cfg(windows)]
mod dragout;
mod finder;
mod history;
mod icons;
mod keyboard;
mod lasso;
mod layout;
mod mega;
mod model;
mod network;
mod onedrive;
mod ops;
mod settings;
mod sharing;
mod places;
mod relay;
mod servers;
mod services;
mod sysicons;
mod tabs;
mod telegram;
mod transfers;
mod tree;
mod undo;
mod updater;
mod uploads;
mod ui;
mod watch;
mod window_place;
mod work;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::SystemTime;

use openexplorer_gdrive::store::{self, StoredAccount};
use openexplorer_gdrive::{ClientConfig, Drive, DriveFile, Quota, oauth};
use openexplorer_i18n::{t, tr};
use rune::{AppSpec, DragItem, Key, KeyEvent, LiveRegion, Mods, Tx, Waker};

use layout::MAX_PANES;
use model::{Entry, Location, MockDrive, Pane, SortCol, Space};

const ROW_H: f32 = 22.0;

enum Msg {
    /// Part of a folder listing. `at`, on a Drive listing's last part: where in the change log
    /// it's valid (see [`cache::Pos`]).
    Listed { pane: usize, request: u64, part: Part, result: Result<Vec<Entry>, String>, at: Option<cache::Pos> },
    /// The change log proved the cached listing pane `pane` shows still stands: no need to read
    /// the folder again. It's kept as `listing`, valid at its new position.
    Unchanged { pane: usize, request: u64, listing: cache::Listing },
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
    Icon { key: sysicons::IconKey, rgba: Option<sysicons::IconBitmaps> },
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
    /// Something background work on a MEGA account brought back.
    Mega(mega::Answer),
    Dropbox(dropbox::Answer),
    /// Something background work on a B2 key brought back.
    B2(b2::Answer),
    /// Something background work on a OneDrive account brought back.
    OneDrive(onedrive::Answer),
    /// Something background work on a Telegram account brought back.
    Telegram(telegram::Answer),
    /// What the caches take, measured or cleared for Opções › Cache.
    Caches(caches::Answer),
    /// Installing the search service ended: search again in the panes `reload` shows.
    Installed { reload: Vec<Location>, result: Result<String, String> },
    /// A drag of Drive items `ids` out of pane `from` ended.
    #[cfg(windows)]
    DragEnded { from: usize, ids: Vec<String>, ended: dragout::Ended },
    /// Built-in updater check, download progress or completion.
    Update(updater::Answer),
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MainTab {
    #[default]
    Explore,
    Accounts,
}

struct State {
    main_tab: MainTab,
    view_menu_open: bool,
    /// Which pane's view-mode dropdown menu is currently open, if any.
    view_mode_menu: Option<usize>,
    /// Number of columns per pane in icon-grid view modes (updated from viewport width).
    grid_cols: [usize; MAX_PANES],
    /// Number of rows per column in multi-column List view mode (updated from viewport height).
    list_rows: [usize; MAX_PANES],
    accounts_filter: Option<servers::Provider>,
    accounts_search: rune::TextEdit,
    panes: [Pane; MAX_PANES],
    tabs: [tabs::PaneTabs; MAX_PANES],
    /// Whether each pane's left folder tree is shown.
    show_tree: [bool; MAX_PANES],
    /// The context menu: the pane it was opened in and where.
    menu: Option<(usize, rune::PointerPress)>,
    /// The tree's right-click menu: pane, node, whether it's open, whether it's empty, and where.
    tree_menu: Option<(usize, tree::Node, bool, bool, rune::PointerPress)>,
    /// Folders being renamed that a pane or a tab is in: the folder they're in, the folder and
    /// its new place. When the rename's reload of that folder comes back done, the panes and
    /// tabs inside move with it.
    renamed: Vec<(Location, Location, Location)>,
    /// A rectangle selection being dragged out in a file list.
    lasso: Option<lasso::Lasso>,
    /// Where each pane's file list viewport is and how far it's scrolled, for the rectangle
    /// selection and the menu the keyboard opens.
    views: [lasso::Viewport; MAX_PANES],
    /// Smooth Ctrl+wheel / pinch zoom state for each pane's file list.
    zoom: [anim::ZoomState; MAX_PANES],
    /// Gliding keyboard selection highlight for each pane.
    kbd_sel: [anim::KeyboardSel; MAX_PANES],
    /// Fading marquee/lasso rectangle after mouse release.
    lasso_fade: Option<anim::LassoFade>,
    /// Smooth row reorder glide when sorting a pane's file list.
    row_reorder: [anim::RowReorder; MAX_PANES],
    /// Active header column drag-to-reorder state.
    col_drag: Option<anim::ColDrag>,
    dialog: Option<actions::Dialog>,
    /// The window was asked to close while transfers ran: Fechar o Open Explorer? shows over
    /// everything, the dialog under it untouched (see [`actions::close_requested`]).
    quit_asked: bool,
    /// The question came from quitting the whole app (macOS: ⌘Q, Encerrar in the Dock, logging
    /// out), not from closing the window: Fechar mesmo assim then quits it.
    quit_app: bool,
    /// Linux: a menu Rune asks for (a text field's Recortar, Copiar, Colar), drawn in the window
    /// as the system draws none there.
    popup: Option<rune::PopupMenuRequest>,
    /// The same for the account window (Registrar conta), which shares this state.
    account_popup: Option<rune::PopupMenuRequest>,
    /// The window opened with the Acrylic background (Opções > Experimental): its page and the
    /// panes' folder trees show the material. Set at start, as the window's transparency is.
    acrylic: bool,
    /// Uploads whose clash dialog waits for the one open (or another dialog) to close.
    clash_waiting: std::collections::VecDeque<actions::ClashAsk>,
    /// The name typed in Renomear or Nova pasta, or the e-mail typed in Compartilhar.
    name_edit: rune::TextEdit,
    /// The item being renamed in place, and its pane.
    renaming: Option<(usize, Entry)>,
    /// Each pane's search field.
    search: [rune::TextEdit; MAX_PANES],
    /// Each pane's path bar while it's a field to type an address in.
    address: [Option<rune::TextEdit>; MAX_PANES],
    /// The addresses reached and searches run, which ↑ and ↓ bring back in those fields.
    history: history::History,
    /// The path bar's right-click menu: the pane it was opened in and where.
    address_menu: Option<(usize, rune::PointerPress)>,
    /// Each local pane's field as last seen, and when it may be searched: typing searches the
    /// computer once the keys stop for a moment.
    typed: [(String, f64); MAX_PANES],
    /// The words each pane last searched the computer for.
    searched: [String; MAX_PANES],
    /// What the index said with each pane's search results.
    found: [Option<finder::Info>; MAX_PANES],
    /// The search service is being installed.
    installing_finder: bool,
    /// When each pane's search of the computer is asked again, while a disk is still being read
    /// or the service is starting (0: not).
    found_again: [f64; MAX_PANES],
    /// Counts Propriedades dialogs, to tell their background answers apart.
    props_seq: u64,
    /// What Copiar or Recortar picked up.
    clip: Option<actions::Clip>,
    /// What Desfazer can take back, and Refazer do again.
    undo: undo::History,
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
    live: [Arc<AtomicU64>; MAX_PANES],
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
    /// The start of a name typed in a list, to reach it.
    typeahead: keyboard::Typeahead,
    /// How many rows each pane's list shows, for Page Up and Page Down (0 before its layout).
    page_rows: [usize; MAX_PANES],
    /// The pane a drag started in and the files it carried, so a drop back on the same pane
    /// is ignored (the drop can arrive after the drag's end).
    last_drag: Option<(usize, Vec<PathBuf>)>,
    /// A drag of files over a pane, which scrolls its list or tree near their edges, and where
    /// each pane's list and tree are and how far they've scrolled.
    autoscroll: Option<autoscroll::Drag>,
    edges: [autoscroll::Pane; MAX_PANES],
    /// What the last action said ("Copiado: 3 itens.", an error...), shown for a few seconds
    /// in a notice at the bottom right.
    status: String,
    /// The message the notice was last started for, and when it goes (app clock, seconds).
    status_seen: String,
    status_until: f64,
    /// The error `status` shows, when it shows one: a screen reader says it at once rather
    /// than when it's idle. Kept as the text, so a later message isn't taken for an error.
    status_alert: String,
    /// The app clock at the last frame.
    now: f64,
    temp: PathBuf,
    inbox: Receiver<Msg>,
    outbox: Sender<Msg>,
    waker: Option<Waker>,
    /// What the user picked in Opções.
    settings: settings::Settings,
    /// Shared concurrency caps across all upload and download batches.
    upload_slots: Arc<work::Slots>,
    download_slots: Arc<work::Slots>,
    /// Copies and moves under way, each with its bar.
    transfers: Vec<transfers::Transfer>,
    /// The transfer queue's right-click menu: the row it was opened on, and where.
    transfer_menu: Option<(Option<Arc<ops::Progress>>, rune::PointerPress)>,
    /// How many rows were still waiting or running at the last frame.
    transfers_live: usize,
    /// The looks the transfer queue was last built with: while they stay, its live values
    /// are enough (see [`transfers::refresh`]).
    queue_looks: Vec<transfers::Look>,
    /// The queue's foot, as live values.
    queue_totals: transfers::Totals,
    /// Windows' own icons, as Explorer shows them.
    icons: sysicons::SysIcons,
    /// The listing version each pane's icons were last asked for.
    icons_for: [u64; MAX_PANES],
    /// State of the independent "Registrar conta" window when open.
    account_form: Option<Box<servers::Form>>,
    account_win: Option<rune::AppHandle<State>>,
    main_win: Option<rune::AppHandle<State>>,
    account_dirty: bool,
    /// Whether the account window was last asked to show (and not closed since).
    account_shown: bool,
    /// Closes asked by [`State::close_account_form`] whose `on_close` hasn't run yet: that
    /// `on_close` leaves alone a form opened again in between.
    account_closes: u32,
    /// Whether the account window's title says Editar conta (`Some(true)`) or Registrar conta.
    account_titled: Option<bool>,
    main_dirty: bool,
    account_synced_font: Option<settings::TextFont>,
    account_synced_render: Option<settings::TextRender>,
    is_placeholder: bool,
    /// Built-in updater state (latest release, download progress, staged file).
    updater: updater::UpdateState,
}

/// Receipt or error passed via `--update-receipt` / `--update-error` on relaunch after an update.
static STARTUP_UPDATE_NOTICE: std::sync::Mutex<Option<Result<String, String>>> = std::sync::Mutex::new(None);

/// Whether the main window was created for Mica (Opções, Experimental): only then can Windows
/// show it, and Opções says whether it did.
pub(crate) static STARTED_WITH_MICA: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Whether Windows has shown Mica behind the main window at least once: then a moment without it
/// is a request still on its way, not a refusal.
pub(crate) static MICA_SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Reiniciar (Opções): once the app has ended, it opens again (see `relaunch`).
pub(crate) static RESTART: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Opens this same program again, with the same arguments, for Reiniciar. Called as the app ends,
/// after the caches are saved, so the new copy starts from them.
fn relaunch() {
    let started = std::env::current_exe().and_then(|exe| std::process::Command::new(exe).args(std::env::args_os().skip(1)).spawn());
    if let Err(e) = started {
        openexplorer_gdrive::log(&format!("erro: reiniciar o Open Explorer: {e}"));
    }
}

/// How long the window's background takes to fade in over Mica before Mica goes, in seconds:
/// longer than the colour spring takes to settle.
const MICA_FADE: f64 = 0.4;
thread_local! {
    /// When Mica, turned off in Opções, leaves the main window.
    static MICA_OFF_AT: std::cell::Cell<Option<f64>> = const { std::cell::Cell::new(None) };
    /// The material last asked for on the main window, so `poll` asks again only when Opções
    /// changes it.
    static MICA_ASKED: std::cell::Cell<Option<settings::Material>> = const { std::cell::Cell::new(None) };
}
static ACCOUNT_WAKER: std::sync::LazyLock<std::sync::Mutex<Option<Waker>>> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

thread_local! {
    static SHARED_STATE: std::cell::RefCell<Option<State>> = const { std::cell::RefCell::new(None) };
    static PENDING_MAIN_FOCUS: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

impl State {
    /// Zero-IO stand-in swapped into the idle window host between turns (`App::turn_scope`).
    fn placeholder() -> State {
        let (outbox, inbox) = channel();
        let loc = Location::Mock(Vec::new());
        State {
            main_tab: MainTab::Explore,
            view_menu_open: false,
            view_mode_menu: None,
            grid_cols: [1; MAX_PANES],
            list_rows: [1; MAX_PANES],
            accounts_filter: None,
            accounts_search: rune::TextEdit::new(""),
            panes: std::array::from_fn(|_| Pane::new(loc.clone())),
            tabs: std::array::from_fn(|_| tabs::PaneTabs::initial()),
            show_tree: [true; MAX_PANES],
            menu: None,
            tree_menu: None,
            renamed: Vec::new(),
            lasso: None,
            views: Default::default(),
            zoom: Default::default(),
            kbd_sel: Default::default(),
            lasso_fade: None,
            row_reorder: Default::default(),
            col_drag: None,
            dialog: None,
            quit_asked: false,
            quit_app: false,
            popup: None,
            account_popup: None,
            acrylic: false,
            clash_waiting: Default::default(),
            name_edit: rune::TextEdit::new(""),
            renaming: None,
            search: std::array::from_fn(|_| rune::TextEdit::new("")),
            address: Default::default(),
            history: Default::default(),
            address_menu: None,
            typed: Default::default(),
            searched: Default::default(),
            found: Default::default(),
            installing_finder: false,
            found_again: [0.0; MAX_PANES],
            props_seq: 0,
            clip: None,
            undo: Default::default(),
            local_drives: Vec::new(),
            places: Default::default(),
            tree: tree::Tree::new(std::iter::empty()),
            layout: Default::default(),
            live: Default::default(),
            cache: cache::Cache::memory_only(),
            mock: MockDrive::empty(),
            accounts: Vec::new(),
            servers: servers::Servers {
                list: Vec::new(),
                pool: Arc::default(),
                store: openexplorer_remote::Store::at(PathBuf::new()),
                mega: openexplorer_mega::Store::at(PathBuf::new()),
                mega_quotas: Default::default(),
                mega_watch: Default::default(),
                dropbox: openexplorer_dropbox::store::Store::at(PathBuf::new()),
                dropbox_accounts: Vec::new(),
                dropbox_quotas: Default::default(),
                dropbox_watch: Default::default(),
                dropbox_login: None,
                b2: openexplorer_b2::Store::at(PathBuf::new()),
                b2_watch: Default::default(),
                onedrive: onedrive::Side {
                    store: openexplorer_onedrive::Store::at(PathBuf::new()),
                    accounts: Vec::new(),
                    quotas: HashMap::new(),
                    watch: HashMap::new(),
                    login: None,
                },
                telegram: openexplorer_telegram::Store::at(PathBuf::new()),
                telegram_watch: Default::default(),
                attempts: 0,
            },
            drives: HashMap::new(),
            quotas: HashMap::new(),
            watch: HashMap::new(),
            login: None,
            logins: 0,
            active: 0,
            typeahead: Default::default(),
            page_rows: [0; MAX_PANES],
            last_drag: None,
            autoscroll: None,
            edges: Default::default(),
            status: String::new(),
            status_seen: String::new(),
            status_until: 0.0,
            status_alert: String::new(),
            now: 0.0,
            temp: PathBuf::new(),
            inbox,
            outbox,
            waker: None,
            settings: Default::default(),
            upload_slots: Arc::new(work::Slots::new(usize::from(settings::Settings::default().uploads))),
            download_slots: Arc::new(work::Slots::new(usize::from(settings::Settings::default().downloads))),
            transfers: Vec::new(),
            transfer_menu: None,
            transfers_live: 0,
            queue_looks: Vec::new(),
            queue_totals: Default::default(),
            icons: Default::default(),
            icons_for: [u64::MAX; MAX_PANES],
            account_form: None,
            account_win: None,
            main_win: None,
            account_dirty: false,
            account_shown: false,
            account_closes: 0,
            account_titled: None,
            main_dirty: false,
            account_synced_font: None,
            account_synced_render: None,
            is_placeholder: true,
            updater: updater::UpdateState::idle(),
        }
    }

    /// Shows `form` in the account window, unless a login or check is under way there: its answer
    /// is matched to that form, so the window only comes to the front.
    pub fn open_account_form(&mut self, form: servers::Form) {
        if !self.account_form.as_ref().is_some_and(|f| f.checking.is_some()) {
            self.account_form = Some(Box::new(form));
        }
        self.show_account_form();
    }

    /// Brings the account window to the front with the form it holds.
    pub fn show_account_form(&mut self) {
        self.account_dirty = true;
        if let Some(w) = &self.account_win {
            self.account_shown = true;
            w.open();
            w.wake();
        }
    }

    /// Drops the form and closes the account window. The window's `on_close` runs later, maybe
    /// after Adicionar conta opened a new form, so it's told this close was already handled.
    pub fn close_account_form(&mut self) {
        self.account_form = None;
        self.account_dirty = true;
        if let Some(w) = &self.account_win
            && std::mem::take(&mut self.account_shown)
        {
            self.account_closes += 1;
            w.close();
        }
    }

    /// The account window's `on_close`: a close the window's own button asked for drops the
    /// form (and a Telegram login waiting on it); one [`State::close_account_form`] asked for
    /// already did, and a form opened since is kept.
    fn account_window_closed(&mut self) {
        if self.account_closes > 0 {
            self.account_closes -= 1;
            return;
        }
        self.account_shown = false;
        telegram::abort_pending();
        self.account_form = None;
    }

    pub fn notify_main(&mut self) {
        self.main_dirty = true;
        if let Some(w) = &self.main_win {
            w.wake();
        }
    }

    pub fn notify_account(&mut self) {
        self.account_dirty = true;
        if let Some(w) = &self.account_win {
            w.wake();
        }
    }
}

thread_local! {
    /// The stand-in the window whose turn it is gave up, handed to the next `leave_turn`. Rune
    /// runs a turn for every event-loop callback of each window, so building a fresh stand-in
    /// each time cost more than a frame.
    static SPARE_PLACEHOLDER: std::cell::RefCell<Option<State>> = const { std::cell::RefCell::new(None) };
}

fn enter_turn(s: &mut State) {
    SHARED_STATE.with(|cell| {
        if let Some(shared) = cell.borrow_mut().take() {
            let stand_in = std::mem::replace(s, shared);
            SPARE_PLACEHOLDER.with(|spare| *spare.borrow_mut() = Some(stand_in));
        }
    });
}

fn leave_turn(s: &mut State) {
    if !s.is_placeholder {
        let stand_in = SPARE_PLACEHOLDER.with(|spare| spare.borrow_mut().take()).unwrap_or_else(State::placeholder);
        SHARED_STATE.with(|cell| {
            *cell.borrow_mut() = Some(std::mem::replace(s, stand_in));
        });
    }
}

fn init() -> State {
    let (outbox, inbox) = channel();
    let settings = settings::Settings::load();
    // Tests read the Portuguese words, whatever the machine's language.
    if !cfg!(test) {
        settings.language.apply();
    }
    let mock = MockDrive::new();
    // Both panes open on the Desktop, at the top, as a first run does: nothing of where the last
    // run was is kept. The simulated drive stays in the tree, for trying things out.
    let places = places::places();
    let start = places.desktop.as_ref().map_or_else(Location::home, |(dir, _)| Location::Local(dir.clone()));
    // The folder is read once: every pane, shown or not, starts with that listing.
    let mut first = Pane::new(start.clone());
    first.load_now(&mock, settings.show_hidden);
    let mut panes: [Pane; MAX_PANES] = std::array::from_fn(|_| {
        let mut pane = Pane::new(start.clone());
        pane.free = first.free;
        pane.set_entries(first.error.clone().map_or_else(|| Ok(first.entries.clone()), Err));
        pane
    });
    panes[0] = first;
    let accounts = store::accounts();
    let tree = tree::Tree::new(accounts.iter().map(|a| a.email.clone()));
    let mut state = State {
        main_tab: MainTab::Explore,
        view_menu_open: false,
        view_mode_menu: None,
        grid_cols: [1; MAX_PANES],
        list_rows: [1; MAX_PANES],
        accounts_filter: None,
        accounts_search: rune::TextEdit::new(""),
        panes,
        tabs: std::array::from_fn(|_| tabs::PaneTabs::initial()),
        show_tree: [true; MAX_PANES],
        menu: None,
        tree_menu: None,
        renamed: Vec::new(),
        lasso: None,
        views: Default::default(),
        zoom: Default::default(),
        kbd_sel: Default::default(),
        lasso_fade: None,
        row_reorder: Default::default(),
        col_drag: None,
        dialog: None,
        quit_asked: false,
        quit_app: false,
        popup: None,
        account_popup: None,
        acrylic: settings.acrylic_window(),
        clash_waiting: Default::default(),
        name_edit: rune::TextEdit::new(""),
        renaming: None,
        search: std::array::from_fn(|_| rune::TextEdit::new("")),
        address: Default::default(),
        history: history::History::load(),
        address_menu: None,
        typed: Default::default(),
        searched: Default::default(),
        found: Default::default(),
        installing_finder: false,
        found_again: [0.0; MAX_PANES],
        props_seq: 0,
        clip: None,
        undo: Default::default(),
        local_drives: model::local_drives(),
        places,
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
        typeahead: Default::default(),
        page_rows: [0; MAX_PANES],
        last_drag: None,
        autoscroll: None,
        edges: Default::default(),
        status: String::new(),
        status_seen: String::new(),
        status_until: 0.0,
        status_alert: String::new(),
        now: 0.0,
        temp: std::env::temp_dir().join("OpenExplorer-drag"),
        inbox,
        outbox,
        waker: None,
        upload_slots: Arc::new(work::Slots::new(usize::from(settings.uploads))),
        download_slots: Arc::new(work::Slots::new(usize::from(settings.downloads))),
        settings,
        transfers: Vec::new(),
        transfer_menu: None,
        transfers_live: 0,
        queue_looks: Vec::new(),
        queue_totals: Default::default(),
        icons: Default::default(),
        icons_for: [u64::MAX; MAX_PANES],
        account_form: None,
        account_win: None,
        main_win: None,
        account_dirty: false,
        account_shown: false,
        account_closes: 0,
        account_titled: None,
        main_dirty: false,
        account_synced_font: None,
        account_synced_render: None,
        is_placeholder: false,
        updater: updater::UpdateState::new(),
    };
    if let Some(notice) = STARTUP_UPDATE_NOTICE.lock().unwrap_or_else(|e| e.into_inner()).take() {
        match notice {
            Ok(ver) => {
                updater::clear_staged();
                openexplorer_gdrive::log(&format!("atualização concluída: v{ver}"));
                state.status = tr!("Open Explorer atualizado para a versão {ver}.", "Open Explorer updated to version {ver}.");
            }
            Err(code) => {
                openexplorer_gdrive::log(&format!("erro atualização: {code}"));
                fail(&mut state, updater::error_text(&code));
            }
        }
    }
    state
}

fn setup(s: &mut State, waker: Waker) {
    let fixed = tree::fixed_icons(&s.places, &s.local_drives).into_iter().chain(sysicons::home_key());
    s.icons.start(rune::ImageCache::new(waker.clone()), s.outbox.clone(), waker.clone(), fixed);
    let combined: Waker = Arc::new(move || {
        waker();
        if let Some(aw) = ACCOUNT_WAKER.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            aw();
        }
    });
    s.waker = Some(combined);
    // Each tree opens down to the folder its pane starts in.
    for p in 0..MAX_PANES {
        tree::follow(s, p);
    }
    // Check for updates in the background (at most once per 24 h automatically).
    if !rune::is_headless() && s.updater.should_auto_check(updater::now_unix()) {
        actions::check_updates(s, false);
    }
}

fn init_account_window() -> State {
    if rune::is_headless() {
        let mut s = init();
        servers::ask_add(&mut s);
        s
    } else {
        State::placeholder()
    }
}

fn setup_account_window(_: &mut State, waker: Waker) {
    *ACCOUNT_WAKER.lock().unwrap_or_else(|e| e.into_inner()) = Some(waker);
}

fn poll_account_window(s: &mut State, _now: f64, tx: &mut Tx) -> bool {
    if s.is_placeholder {
        return false;
    }
    if s.account_synced_font != Some(s.settings.font) {
        s.account_synced_font = Some(s.settings.font);
        let (sans, mono) = s.settings.font.families();
        tx.set_font_families(sans, mono);
    }
    if s.account_synced_render != Some(s.settings.text_render) {
        s.account_synced_render = Some(s.settings.text_render);
        tx.set_font_smoothing(s.settings.text_render.smoothing());
    }
    // The title the taskbar and Alt+Tab show follows the one drawn in the window.
    let editing = s.account_form.as_ref().is_some_and(|f| f.editing.is_some());
    if s.account_titled != Some(editing) {
        s.account_titled = Some(editing);
        tx.set_title(ui::account_window_title(editing));
    }
    std::mem::take(&mut s.account_dirty)
}

fn account_window_key(s: &mut State, ev: &KeyEvent, _: &mut Tx) -> bool {
    if s.is_placeholder {
        return false;
    }
    if ev.key == Key::Escape && ev.mods == Mods::default() {
        if let Some(f) = &mut s.account_form {
            if f.open_group.is_some() {
                f.open_group = None;
                return true;
            }
            if f.step == servers::Step::Configure && f.editing.is_none() && f.checking.is_none() {
                servers::back_to_pick(s);
                return true;
            }
        }
        telegram::abort_pending();
        s.close_account_form();
        return true;
    }
    false
}

/// Runs `f`, turning a panic into an error that names it (and logging it), so background work
/// that hits a bug still ends with a message instead of never answering. `doing` says what it
/// was ("listar", "conectar").
fn catch_panic<T>(doing: &str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|p| {
        let what = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
        openexplorer_gdrive::log(&format!("pânico ao {doing}: {what}"));
        Err(tr!("Erro interno ao {doing} ({what}). Detalhes no log.", "Internal error trying to {doing} ({what}). Details in the log."))
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
///
/// Each message the notice starts for is handed to `say` once, for a screen reader: at once
/// (assertive) when it's the error [`State::status_alert`] names, else when the reader is idle.
/// The notice itself isn't a live region (see `ui::view`), so nothing says it twice.
fn notice(s: &mut State, now: f64, say: &mut dyn FnMut(String, LiveRegion)) -> bool {
    s.now = now;
    if s.status != s.status_seen {
        s.status_seen = s.status.clone();
        s.status_until = now + NOTICE_SECS;
        if !s.status.is_empty() {
            let alert = s.status == s.status_alert;
            say(s.status.clone(), if alert { LiveRegion::Assertive } else { LiveRegion::Polite });
        }
        s.status_alert.clear();
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

/// Shows `msg` in the notice as an error (see [`State::status_alert`]).
fn fail(s: &mut State, msg: String) {
    s.status_alert = msg.clone();
    s.status = msg;
}

/// Seconds a message stays on screen.
const NOTICE_SECS: f64 = 5.0;

/// How long the keys must pause before what's typed is searched, in seconds.
const TYPING_PAUSE: f64 = 0.12;
/// How often a search is asked again while a disk is still being read, in seconds.
const FOUND_AGAIN: f64 = 1.5;

fn poll(s: &mut State, now: f64, tx: &mut Tx) -> bool {
    if s.is_placeholder {
        return false;
    }
    if let Some(focused) = PENDING_MAIN_FOCUS.with(|c| c.take()) {
        accounts::window_focus(s, focused, tx);
    }
    // Mica follows Opções while the window lives; turned on again, it comes back only in a window
    // made for it (only its creation makes a window transparent). Turned off, the view paints the
    // window's own background at once and fades it in over Mica, and Mica goes once that's done:
    // taking it away first left a frame with nothing behind the window, a flash.
    // Another material takes the old one's place at once: the view stays transparent meanwhile.
    let want = (s.settings.mica && STARTED_WITH_MICA.load(Ordering::Relaxed)).then_some(s.settings.material);
    let asked = MICA_ASKED.with(std::cell::Cell::get);
    if let Some(material) = want {
        MICA_OFF_AT.with(|t| t.set(None));
        if asked != Some(material) {
            MICA_ASKED.with(|a| a.set(Some(material)));
            tx.set_window_backdrop(material.backdrop());
        }
    } else if asked.is_some() {
        let at = MICA_OFF_AT.with(|t| {
            let at = t.get().unwrap_or(now + MICA_FADE);
            t.set(Some(at));
            at
        });
        if now >= at {
            MICA_OFF_AT.with(|t| t.set(None));
            MICA_ASKED.with(|a| a.set(None));
            tx.set_window_backdrop(rune::WindowBackdrop::None);
        } else {
            tx.wake_at(at);
        }
    }
    // Each listing's file types get their icons asked for once.
    let versions = s.panes.each_ref().map(|p| p.version);
    if s.icons_for != versions {
        let relisted: Vec<usize> = (0..MAX_PANES).filter(|&p| s.icons_for[p] != versions[p]).collect();
        s.icons_for = versions;
        let keys = s.panes.iter().flat_map(|p| sysicons::keys_of(&p.entries, &p.loc)).collect();
        s.icons.show(keys);
        // The trees take in the folders a new listing shows.
        for p in relisted {
            tree::learn(s, p);
        }
    }
    // The 256 px icons only while a pane in view shows Large or Extra large icons.
    let big = s.panes[..s.settings.pane_layout.count()].iter().any(|p| matches!(p.view_mode, model::ViewMode::LargeIcons | model::ViewMode::ExtraLargeIcons));
    s.icons.set_big(big);
    let mut changed = std::mem::take(&mut s.main_dirty) | s.icons.settle();
    // List scrolled sideways to columns its view didn't build.
    for view in &s.views {
        changed |= view.columns_stale.replace(false);
    }
    changed |= notice(s, now, &mut |text, how| tx.announce(text, how));
    // Smooth Ctrl+wheel / pinch zoom, keyboard selection glide, and sort row reorder animations.
    let mut anim_wake = false;
    for p in 0..MAX_PANES {
        if s.zoom[p].tick(now) {
            changed = true;
            if s.zoom[p].is_animating() {
                anim_wake = true;
            }
        }
        if s.kbd_sel[p].tick(now) {
            changed = true;
            if s.kbd_sel[p].is_active() {
                anim_wake = true;
            }
        }
        if s.row_reorder[p].tick(now) {
            changed = true;
            if s.row_reorder[p].is_active() {
                anim_wake = true;
            }
        }
    }
    if let Some(fade) = s.lasso_fade {
        changed = true;
        if fade.done(now) {
            s.lasso_fade = None;
        } else {
            anim_wake = true;
        }
    }
    // Clipboard Copy/Cut icon wiggle: it runs for a moment after the pick-up, and wakes only
    // while copied/cut items are visible in an open pane. A copy stays picked up after Colar,
    // so a wiggle without an end would redraw the window for as long as it shows them.
    if s.main_tab == MainTab::Explore
        && s.settings.clipboard_animated()
        && let Some(clip) = &s.clip
        && !clip.items.is_empty()
        && now - clip.picked_at < anim::WIGGLE_TOTAL
        && {
            let ids: std::collections::HashSet<&str> = clip.items.iter().map(|c| c.id.as_str()).collect();
            (0..s.settings.pane_layout.count()).any(|p| !s.tabs[p].is_picker() && s.panes[p].loc == clip.from && s.panes[p].entries.iter().any(|e| ids.contains(e.id.as_str())))
        }
    {
        changed = true;
        anim_wake = true;
    }
    if anim_wake {
        tx.wake_at(now + 1.0 / 60.0);
    }
    // A rectangle selection held past the list's edge scrolls it.
    changed |= lasso::step(s, now, tx);
    // A local pane searches the computer as its field is typed in, once the keys pause.
    for p in 0..MAX_PANES {
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
    // Rows moving on change only their live values; the queue is built again when a row comes,
    // goes or changes stage.
    let live = s.transfers.iter().filter(|t| !t.progress.over()).count();
    let looks = transfers::looks(&s.transfers);
    transfers::refresh(&s.transfers, &s.queue_totals, transfers::bar_width(s.layout.queue_cols[5]));
    changed |= looks != s.queue_looks || live != s.transfers_live || s.transfers.len() != rows;
    s.queue_looks = looks;
    s.transfers_live = live;
    changed |= actions::quit_question_done(s);
    while let Ok(msg) = s.inbox.try_recv() {
        changed = true;
        match msg {
            Msg::Opened { email, drive } => keep_drive(s, email, drive),
            Msg::TreeKids { id, hidden, result } => tree::arrived(s, id, hidden, result),
            Msg::Servers { found, done } => tree::servers_found(s, found, done),
            Msg::ServerChecked { attempt, result, secret } => servers::checked(s, attempt, result, secret),
            Msg::Listed { pane, request, part, result, at } => {
                if s.live[pane].load(Ordering::Relaxed) != request {
                    continue;
                }
                let (first, more) = match part {
                    Part::Cached => (true, true),
                    Part::Page { first, more } => (first, more),
                };
                let ok = result.is_ok();
                let p = &mut s.panes[pane];
                // The selection (the folder just left, say) may be on a page still to come, and
                // so may the cursor's item.
                let held = (p.selected.clone(), p.cursor.clone(), p.anchor.clone());
                match result {
                    r if first => p.set_entries(r),
                    Ok(page) => p.append_entries(page),
                    // A later page, or the refresh of a cached listing, failed: keep what the pane
                    // shows and say so.
                    Err(e) => fail(s, tr!("Não consegui terminar a listagem, mostrando o que chegou: {e}", "Couldn't finish the listing, showing what arrived: {e}")),
                }
                let p = &mut s.panes[pane];
                p.loading = ok && more;
                if p.loading {
                    (p.selected, p.cursor, p.anchor) = held;
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
                    let entries = Arc::new(s.panes[pane].entries.clone());
                    s.cache.put(k, entries, at);
                }
            }
            Msg::Unchanged { pane, request, listing } => {
                if s.live[pane].load(Ordering::Relaxed) != request {
                    continue;
                }
                // The pane already shows the cached listing: it only stops loading.
                let p = &mut s.panes[pane];
                p.loading = false;
                p.prune_selection();
                drop_stale_rename(s, pane);
                if let Some(k) = cache::key(&s.panes[pane].loc) {
                    s.cache.keep(k, listing);
                }
            }
            Msg::Done { reload, result } => {
                changed_here(s, &reload);
                let ok = result.is_ok();
                match &result {
                    Ok(msg) => openexplorer_gdrive::log(&format!("operação: {msg}")),
                    Err(msg) => openexplorer_gdrive::log(&format!("erro: {msg}")),
                }
                if let Some(actions::Dialog::Options(opt)) = &mut s.dialog {
                    opt.refresh_logs();
                }
                match result {
                    Ok(msg) => s.status = msg,
                    Err(msg) => fail(s, msg),
                }
                for (parent, from, to) in std::mem::take(&mut s.renamed) {
                    if !reload.contains(&parent) {
                        s.renamed.push((parent, from, to));
                    } else if ok {
                        tabs::repoint(s, &from, Some(&to));
                    }
                }
                for p in 0..MAX_PANES {
                    if reload.contains(&s.panes[p].loc) {
                        load(s, p);
                    }
                }
                tabs::changed(s, &reload);
                // A folder no pane shows (one changed from the tree's menu) is read again for
                // the tree; a pane's listing tells the tree itself.
                for l in &reload {
                    if !s.panes.iter().any(|p| &p.loc == l) {
                        tree::reread(s, l);
                    }
                }
                // An upload, a copy or a delete there changes what the account uses.
                let mut touched: Vec<&String> = reload.iter().filter_map(|l| if let Location::Drive { email, .. } = l { Some(email) } else { None }).collect();
                touched.dedup();
                for email in touched {
                    ask_quota(s, email);
                }
                // So does one in a cloud account: each asked once.
                let mut seen = std::collections::HashSet::new();
                let clouds: Vec<openexplorer_remote::Server> = reload
                    .iter()
                    .filter_map(|l| match l {
                        Location::Server { server, .. } if accounts::Account::of(server).is_some() && seen.insert(server.id.clone()) => Some((**server).clone()),
                        _ => None,
                    })
                    .collect();
                for sv in clouds {
                    accounts::ask_quota(s, &sv);
                }
            }
            Msg::Undo(answer) => undo::arrived(s, answer),
            Msg::Mega(answer) => mega::arrived(s, answer, tx, now),
            Msg::Dropbox(answer) => dropbox::arrived(s, answer, tx, now),
            Msg::B2(answer) => b2::arrived(s, answer),
            Msg::OneDrive(answer) => onedrive::arrived(s, answer, tx, now),
            Msg::Telegram(answer) => telegram::arrived(s, answer),
            Msg::Caches(answer) => caches::arrived(s, answer),
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
                match result {
                    Ok(m) => s.status = m,
                    Err(m) => fail(s, m),
                }
                for p in 0..MAX_PANES {
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
                    s.notify_account();
                } else if result.is_err() {
                    continue;
                }
                match result {
                    Ok((account, drive)) => {
                        openexplorer_gdrive::log(&format!("conta Google conectada: {}", account.email));
                        s.status = tr!("Conta conectada: {}", "Account connected: {}", account.email);
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
                        fail(s, e);
                    }
                }
            }
            Msg::Reached { pane, moves, result } => {
                // The pane moved on while the place was looked for.
                if s.panes[pane].moves != moves {
                    continue;
                }
                s.history.reached(pane, moves, result.is_ok());
                match result {
                    Ok(to) => {
                        s.status.clear();
                        actions::arrive(s, pane, to);
                    }
                    Err(e) => fail(s, e),
                }
            }
            Msg::Update(ans) => match ans {
                updater::Answer::Checked { manual, result } => {
                    s.updater.checking = false;
                    s.updater.checked_once = true;
                    let now_unix = updater::now_unix();
                    s.updater.last_checked = Some(now_unix);
                    updater::record_check(now_unix);
                    match result {
                        Ok(Some(rel)) => {
                            openexplorer_gdrive::log(&format!("atualização disponível: v{}", rel.version));
                            let already_staged_same = matches!(
                                &s.updater.download,
                                updater::DownloadState::Ready(st) if st.version == rel.version
                            );
                            // A download or install under way stays: a second one would write
                            // the same file.
                            let busy = matches!(s.updater.download, updater::DownloadState::Downloading { .. } | updater::DownloadState::Installing);
                            s.updater.release = Some(rel);
                            if !already_staged_same && !busy {
                                s.updater.download = updater::DownloadState::Idle;
                                if s.updater.support.is_ok()
                                    && s.updater.offer(s.settings.auto_update) == Some(updater::Offer::Automatic)
                                {
                                    actions::download_update(s);
                                }
                            }
                        }
                        Ok(None) => {
                            s.updater.check_error = None;
                            if manual {
                                s.status = tr!(
                                    "Você já está na versão mais recente (v{}).",
                                    "You're already on the latest version (v{}).",
                                    updater::VERSION
                                );
                            }
                        }
                        Err(e) => {
                            openexplorer_gdrive::log(&format!("erro verificar atualizações: {e}"));
                            s.updater.check_error = Some(e.clone());
                            if manual {
                                fail(s, e);
                            }
                        }
                    }
                }
                updater::Answer::Progress { received, total } => {
                    s.updater.download = updater::DownloadState::Downloading { received, total };
                }
                updater::Answer::Downloaded(result) => match result {
                    Ok(staged) => {
                        openexplorer_gdrive::log(&format!("atualização baixada e verificada: v{}", staged.version));
                        s.updater.download = updater::DownloadState::Ready(staged);
                    }
                    Err(e) => {
                        openexplorer_gdrive::log(&format!("erro baixar atualização: {e}"));
                        s.updater.download = updater::DownloadState::Failed(e);
                    }
                },
            },
        }
    }
    // Accounts on screen are asked what changed elsewhere, every few seconds; after the
    // messages, so a listing that just ended is read again here if changes came during it.
    changed |= watch::tick(s, now);
    accounts::tick(s, now);
    if let Some(at) = accounts::next_ask(s) {
        tx.wake_at(at);
    }
    for p in 0..MAX_PANES {
        // The address field closes when the keyboard leaves it (a click elsewhere).
        if s.address[p].as_ref().is_some_and(|a| !a.focused) {
            s.address[p] = None;
            changed = true;
        }
        // Mostrar na pasta: the item is selected and brought into view once its folder lists it.
        let pane = &mut s.panes[p];
        let Some((id, name)) = pane.reveal.clone() else { continue };
        if let Some(i) = pane.index_of(&id) {
            pane.reveal = None;
            pane.selected = [id.clone()].into();
            pane.cursor = Some(id.clone());
            pane.anchor = Some(id.clone());
            reveal(s, p, i, &id, tx);
            changed = true;
        } else if !pane.loading {
            pane.reveal = None;
            if pane.error.is_none() {
                s.status = tr!("“{name}” não está mais nesta pasta.", "“{name}” is no longer in this folder.");
                changed = true;
            }
        }
    }
    // A tree shows its keyboard row's ring while it has focus, however it got it (Tab from the
    // search field, a click), and stops once focus left it.
    for p in 0..MAX_PANES {
        let on = tx.focused() == Some(ui::tree_key(p).as_str());
        if s.tree.focused[p] != on {
            tree::focus(s, p, on);
            changed = true;
        }
    }
    // Each tree brings the folder its pane shows into view, once that's in the tree.
    for p in 0..MAX_PANES {
        if tree::following(s, p) {
            let lines = tree::lines(s, p);
            if let Some(key) = tree::to_reveal(s, p, &lines) {
                tx.scroll_into_view_in(ui::tree_key(p), format!("t:{key}"));
                changed = true;
            }
        }
    }
    // A drag of files held near the edge of a list or tree scrolls it.
    autoscroll::tick(&mut s.autoscroll, &s.edges, |p| (ui::list_key(p), ui::tree_key(p)), ui::HEADER_H, now, tx);
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

/// Whether the focused element (its key, as [`rune::Tx::focused`] names it) is the name field
/// of a rename in place (see `ui::rename_field`).
fn rename_focused(focused: &str) -> bool {
    focused == "rename-input" || focused.ends_with("/rename-input")
}

fn row_key(id: &str) -> String {
    format!("r:{id}")
}

// ---------------------------------------------------------------------------------------------
// Accounts

/// Opens a stored account: reads the client file and the credential store, so it runs off the
/// UI thread.
fn open_drive(email: &str) -> Result<Arc<Drive>, String> {
    // The client it signed in with: its refresh token is that client's.
    let cfg = ClientConfig::for_account(&store::client_of(email))?;
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
    tx.open_url(&pending.url).map_err(|e| tr!("Não consegui abrir o navegador: {e:?}", "Couldn't open the browser: {e:?}"))?;
    s.logins += 1;
    let attempt = s.logins;
    s.login = Some(attempt);
    s.status = t("Faça o login no navegador que abriu. Esperando o Google…", "Sign in in the browser that opened. Waiting for Google…").into();
    background(s, move |emit| {
        let result = catch_panic(t("conectar", "connect"), || {
            let tokens = pending.finish(&cfg)?;
            let client_id = cfg.client_id.clone();
            let drive = Drive::from_login(cfg, &tokens)?;
            let who = drive.about()?;
            let account = StoredAccount { email: who.email, name: who.name, client_id };
            store::save(account.clone(), drive.refresh_token())?;
            Ok((account, Arc::new(drive)))
        });
        emit(Msg::LoggedIn { pane: p, attempt, result });
    });
    Ok(())
}

fn disconnect(s: &mut State, email: &str) {
    if let Err(e) = store::remove(email) {
        fail(s, e);
        return;
    }
    s.drives.remove(email);
    s.quotas.remove(email);
    s.cache.forget(email);
    s.tree.forget(email);
    s.watch.remove(email);
    s.accounts.retain(|a| a.email != email);
    for p in 0..MAX_PANES {
        let gone = |l: &Location| matches!(l, Location::Drive { email: e, .. } if e == email);
        s.panes[p].back.retain(|l| !gone(l));
        s.panes[p].forward.retain(|l| !gone(l));
        if !s.tabs[p].is_picker() && matches!(&s.panes[p].loc, Location::Drive { email: e, .. } if e == email) {
            switch(s, p, Location::Mock(Vec::new()));
        }
    }
    tabs::purge_locations(s, |l| matches!(l, Location::Drive { email: e, .. } if e == email), &Location::Mock(Vec::new()));
    s.status = tr!("Conta desconectada: {email}", "Account disconnected: {email}");
}

/// Asks Drive how much of `email`'s storage is used, on a background thread; the status bar
/// keeps showing the last answer meanwhile.
fn ask_quota(s: &mut State, email: &str) {
    if !s.accounts.iter().any(|a| a.email == email) {
        return;
    }
    s.quotas.entry(email.to_string()).or_insert(None);
    let who = email.to_string();
    drive_task(s, email, t("ler a cota", "read the storage quota"), |d| d.quota(), move |result| Msg::Quota { email: who, result });
}

/// Ensures every connected cloud account has requested its storage quota at least once.
pub(crate) fn ensure_all_quotas(s: &mut State) {
    let emails: Vec<String> = s.accounts.iter().filter(|a| !s.quotas.contains_key(&a.email)).map(|a| a.email.clone()).collect();
    for email in emails {
        ask_quota(s, &email);
    }
    let pending: Vec<openexplorer_remote::Server> = s.servers.list.iter().filter(|sv| accounts::quota_unasked(s, sv)).cloned().collect();
    for sv in pending {
        accounts::ask_quota(s, &sv);
    }
}

/// Refreshes storage quotas for all connected cloud accounts.
pub(crate) fn refresh_all_quotas(s: &mut State) {
    let emails: Vec<String> = s.accounts.iter().map(|a| a.email.clone()).collect();
    for email in emails {
        ask_quota(s, &email);
    }
    let clouds: Vec<openexplorer_remote::Server> = s.servers.list.iter().filter(|sv| accounts::Account::of(sv).is_some()).cloned().collect();
    for sv in clouds {
        accounts::ask_quota(s, &sv);
    }
}

pub(crate) fn set_main_tab(s: &mut State, tab: MainTab) {
    s.main_tab = tab;
    s.view_menu_open = false;
    s.view_mode_menu = None;
    if tab == MainTab::Accounts {
        ensure_all_quotas(s);
    }
}

/// Switches between 1, 2, and 4 visible panes in `Explorar`, persisting the choice.
pub(crate) fn set_pane_layout(s: &mut State, layout: settings::PaneLayout) {
    let before = s.settings.pane_layout.count();
    s.settings.pane_layout = layout;
    s.settings.save();
    s.view_menu_open = false;
    s.view_mode_menu = None;
    s.menu = None;
    s.tree_menu = None;
    s.address_menu = None;
    let count = layout.count();
    if s.active >= count {
        s.active = 0;
    }
    // A pane shown again is read again: while hidden it wasn't watched (B2's polling skips it).
    for p in before..count {
        if !s.tabs[p].is_picker() {
            load(s, p);
        }
    }
    if s.renaming.as_ref().is_some_and(|(p, _)| *p >= count) {
        s.renaming = None;
    }
    if s.lasso.as_ref().is_some_and(|l| l.pane >= count) {
        s.lasso = None;
    }
    s.main_tab = MainTab::Explore;
}

/// Sets the file display mode (Extra large / Large / Medium icons, List, Details) for pane `p`.
pub(crate) fn set_view_mode(s: &mut State, p: usize, mode: model::ViewMode) {
    s.active = p;
    s.panes[p].view_mode = mode;
    s.view_mode_menu = None;
    if s.lasso.as_ref().is_some_and(|l| l.pane == p) {
        s.lasso = None;
    }
}

/// Cycles pane `p` directly to the next file display mode.
pub(crate) fn cycle_view_mode(s: &mut State, p: usize) {
    let next = s.panes[p].view_mode.next();
    set_view_mode(s, p, next);
}

/// Opens `loc` in pane `p` (0 = left, 1 = right) and switches to `Explorar`.
pub(crate) fn open_account_in_pane(s: &mut State, p: usize, loc: Location) {
    s.main_tab = MainTab::Explore;
    if s.tabs[p].is_picker() {
        tabs::open_new_tab(s, p, loc);
    } else {
        navigate(s, p, loc);
    }
}

// ---------------------------------------------------------------------------------------------
// Navigation

/// Lists the pane's current folder again, whole: Atualizar, and every reload after a change.
fn load(s: &mut State, p: usize) {
    list(s, p, true);
}

/// Atualizar: [`load`], with a Dropbox or OneDrive folder read whole rather than from its kept
/// listing.
fn refresh(s: &mut State, p: usize) {
    dropbox::forget_shown(s, p);
    onedrive::forget_shown(s, p);
    load(s, p);
}

/// The app changed the folders at `reload` itself, and Drive's change log may not show it yet:
/// their cached listings aren't trusted on the log's word until read again.
fn changed_here(s: &mut State, reload: &[Location]) {
    for k in reload.iter().filter_map(cache::key) {
        s.cache.unverify(&k);
    }
}

/// For a Drive folder at `loc`: whether its cached listing may be kept on the change log's word
/// (only on opening it, not `fresh`), and whether a fresh listing of it records where in the
/// log it's valid (any [`watch::Provable`] folder).
fn checks(s: &State, loc: &Location, fresh: bool) -> (Option<watch::Provable>, Option<watch::Provable>) {
    let provable = watch::provable(&s.cache, loc);
    (provable.clone().filter(|_| !fresh), provable)
}

/// Seconds since 1970 by the system clock.
fn now_secs() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Lists the pane's current folder: in place for local and simulated folders, in the
/// background for Drive. Unless `fresh`, a Drive folder whose cached listing the change log
/// proves unchanged keeps it instead of being read again.
fn list(s: &mut State, p: usize, fresh: bool) {
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
                let result = catch_panic(t("pesquisar", "search"), || {
                    finder::search_elsewhere(&f, show_hidden, &alive, &mut |entries, more| {                        emit(Msg::Listed { pane: p, request, part: Part::Page { first, more }, result: Ok(entries), at: None });                        first = false;
                    })
                });
                if let Err(e) = result {
                    emit(Msg::Listed { pane: p, request, part: Part::Page { first, more: false }, result: Err(e), at: None });
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
            let result = catch_panic(t("listar", "list"), || model::read_local(&dir, show_hidden).map_err(|e| format!("{}: {e}", dir.display())));
            emit(Msg::Listed { pane: p, request, part: Part::WHOLE, result, at: None });
        });
        return;
    }
    // A cloud account's storage, the first time one of its folders opens.
    if let Location::Server { server, .. } = &s.panes[p].loc
        && accounts::quota_unasked(s, server)
    {
        let sv = (**server).clone();
        accounts::ask_quota(s, &sv);
    }
    if accounts::load(s, p, request, fresh) {
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
    let in_memory = key.as_ref().and_then(|k| s.cache.get(k)).cloned();
    // A folder in My Drive can keep its cached listing when the change log proves it unchanged
    // (see [`watch::verdict`]); such a listing then records where in the log it's valid. Atualizar
    // and every reload after a change read it whole all the same.
    let (check, provable) = checks(s, &s.panes[p].loc, fresh);
    let watched = provable.as_ref().and_then(|_| watch::token(s, &email));
    let pane = &mut s.panes[p];
    let mut quiet = in_memory.is_some();
    if let Some(listing) = &in_memory {
        pane.set_entries(Ok(listing.entries.to_vec()));
    }
    pane.loading = true;
    pane.error = None;
    let live = s.live[p].clone();
    let folder = path.last().map(|(id, _)| id.clone());
    background(s, move |emit| {
        let listed = |part, result| Msg::Listed { pane: p, request, part, result, at: None };
        // Else one from an earlier run, read here off the UI thread.
        let mut cached = in_memory;
        if !quiet && let Some(listing) = key.as_ref().and_then(cache::load_disk) {
            emit(listed(Part::Cached, Ok(listing.entries.to_vec())));
            quiet = true;
            cached = Some(listing);
        }
        let drive = match account.open(emit) {
            Ok(d) => d,
            Err(e) => return emit(listed(Part::Page { first: !quiet, more: false }, Err(e))),
        };
        let started = now_secs();
        let mut token = None;
        if let Some(check) = &check {
            let verdict = catch_panic(t("ler as mudanças do Drive", "read the changes in Drive"), || {
                Ok(watch::verdict(cached.as_ref(), check, started, |token, pages| drive.changes_within(token, pages), || drive.root_id()))
            });
            match (verdict, cached.take()) {
                (Ok(watch::Verdict::Unchanged(pos)), Some(listing)) => {
                    openexplorer_gdrive::log(&format!("  sem mudanças: {} itens mantidos do cache", listing.entries.len()));
                    return emit(Msg::Unchanged { pane: p, request, listing: cache::Listing { pos: Some(pos), ..listing } });
                }
                (Ok(watch::Verdict::Fetch(t)), _) => token = t,
                _ => {}
            }
        }
        drop(cached);
        // Where the listing about to be read is valid: a point in the log from before its first
        // page, so whatever changes while it's read is still ahead of it in the log.
        if provable.is_some() {
            token = token.or(watched).or_else(|| drive.start_page_token().ok());
        }
        let at = token.map(|token| cache::Pos { token, fetched: started });
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
                    emit(Msg::Listed { pane: p, request, part: Part::WHOLE, result: Ok(std::mem::take(&mut gathered)), at: at.clone() });
                }
            } else {
                emit(Msg::Listed { pane: p, request, part: Part::Page { first, more }, result: Ok(entries.collect()), at: if more { None } else { at.clone() } });
                first = false;
            }
            live.load(Ordering::Relaxed) == request
        };
        // A panic here (a bad value from Drive, say) must not leave the pane loading forever.
        let result = catch_panic(t("listar", "list"), || match (&space, folder.as_deref()) {
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
                        name: if d.hidden { tr!("{} (oculto)", "{} (hidden)", d.name) } else { d.name },
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
    list(s, p, false);
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

/// The mouse's side buttons over pane `p`: Voltar and Avançar, as in Explorer. Other extra
/// buttons (the wheel's press) are left to the system.
fn side_button(s: &mut State, p: usize, button: rune::PointerButton) -> bool {
    let side = matches!(button, rune::PointerButton::Back | rune::PointerButton::Forward);
    if s.tabs[p].is_picker() {
        return side;
    }
    match button {
        rune::PointerButton::Back => go_back(s, p),
        rune::PointerButton::Forward => go_forward(s, p),
        _ => return false,
    }
    true
}

/// The side buttons anywhere else in the window: the active pane's, while Explorar shows.
fn side_button_in_window(s: &mut State, button: rune::PointerButton) -> bool {
    let p = s.active;
    s.main_tab == MainTab::Explore && side_button(s, p, button)
}

fn open(s: &mut State, p: usize, id: &str) {
    actions::remember_search(s, p);
    let pane = &mut s.panes[p];
    let Some(e) = pane.entry(id).cloned() else { return };
    if !e.is_dir {
        return actions::open_file(s, p, &e);
    }
    let loc = folder_in(&pane.loc, &e);
    navigate(s, p, loc);
}

/// Where folder `e` of `at` opens.
pub(crate) fn folder_in(at: &Location, e: &Entry) -> Location {
    let mut loc = at.clone();
    match &mut loc {
        // A folder found opens where it is.
        Location::Found(_) => loc = Location::Local(PathBuf::from(&e.id)),
        Location::Local(dir) => dir.push(&e.name),
        Location::Mock(segs) => segs.push(e.name.clone()),
        Location::Drive { space: space @ Space::SharedDrives, .. } => *space = Space::Shared { id: e.id.clone(), name: e.name.clone() },
        Location::Drive { path, .. } => path.push((e.target.clone().unwrap_or_else(|| e.id.clone()), e.name.clone())),
        // A folder found opens where it is.
        Location::Server { server, .. } => {
            let segs = servers::segs_of(at, e);
            loc = servers::folder(server, segs);
        }
    }
    loc
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
    // A click on another row ends a rename in place, as leaving the field does in Explorer: the
    // name typed is kept (a name that can't be taken keeps the field open, and the click waits).
    if s.renaming.is_some() {
        actions::commit_rename(s);
        if s.renaming.is_some() {
            return;
        }
    }
    s.active = p;
    s.kbd_sel[p].stop();
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
    if ev.is_command('t') {
        tabs::open_picker(s, p);
        return true;
    }
    if ev.is_command('w') && !s.tabs[p].is_picker() {
        tabs::close_active_tab(s, p);
        return true;
    }
    if s.tabs[p].is_picker() {
        if matches!(ev.key, Key::Escape) {
            if s.tabs[p].picker.filter_open {
                s.tabs[p].picker.filter_open = false;
            } else {
                tabs::cancel_picker(s, p);
            }
            return true;
        }
        return false;
    }
    // Keys belong to the name field while renaming. Once the focus left it (a click in a row,
    // in the other pane), the rename ends as leaving the field does, and the key goes on: else
    // both panes' keys would stay with a field no one is typing in.
    if s.renaming.is_some() {
        if tx.focused().is_some_and(rename_focused) {
            return false;
        }
        actions::commit_rename(s);
        if s.renaming.is_some() {
            return false;
        }
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
    let page = keyboard::page(s.page_rows[p]) as isize;
    let flow = s.views[p].flow.get();
    let (across, down) = flow.steps();
    let owns = keyboard::list_has_keys(s, p, tx.focused());
    // Any other key ends a name being typed: Enter into a folder and a letter starts anew.
    let plain = !(ev.mods.cmd || ev.mods.ctrl || ev.mods.alt);
    if !(plain && matches!(ev.key, Key::Text(_))) {
        s.typeahead.clear();
    }
    let to = match &ev.key {
        Key::ContextMenu | Key::Function(10) if menu_key(ev) => {
            menu_from_keys(s, p, tx);
            return true;
        }
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
        // F5 reloads the focused pane's current folder, as in Explorer.
        Key::Function(5) => {
            refresh(s, p);
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
        // Ctrl+Space adds the cursor's item to the selection or takes it away, as in Explorer.
        _ if ev.is_command(' ') => {
            let pane = &mut s.panes[p];
            if let Some(c) = pane.cursor.clone()
                && pane.index_of(&c).is_some()
            {
                if !pane.selected.remove(&c) {
                    pane.selected.insert(c.clone());
                }
                pane.anchor = Some(c);
            }
            return true;
        }
        // Typing a name's start goes to it.
        Key::Text(t) if plain && owns => {
            let Some(query) = s.typeahead.push(p, t, s.now) else { return false };
            if let Some(i) = keyboard::find(&s.panes[p].entries, at, query) {
                let id = s.panes[p].entries[i].id.clone();
                click(s, p, &id, Mods::default());
                reveal(s, p, i, &id, tx);
            }
            return true;
        }
        // Ctrl+0 resets list zoom; Ctrl+= / Ctrl++ zooms in; Ctrl+- zooms out.
        _ if ev.is_command('0') => {
            if s.zoom[p].reset(s.now, s.settings.zoom_animated()) && s.zoom[p].is_animating() {
                tx.wake_at(s.now + 1.0 / 60.0);
            }
            return true;
        }
        _ if ev.is_command('=') || ev.is_command('+') => {
            if s.zoom[p].step_by(1.0, s.now, s.settings.zoom_animated()) && s.zoom[p].is_animating() {
                tx.wake_at(s.now + 1.0 / 60.0);
            }
            return true;
        }
        _ if ev.is_command('-') => {
            if s.zoom[p].step_by(-1.0, s.now, s.settings.zoom_animated()) && s.zoom[p].is_animating() {
                tx.wake_at(s.now + 1.0 / 60.0);
            }
            return true;
        }
        // Esc lets go of the selection (the cursor stays), or clears a pending copy/cut.
        Key::Escape if owns && !s.panes[p].selected.is_empty() => {
            s.panes[p].selected.clear();
            return true;
        }
        Key::Escape if owns && s.clip.is_some() => {
            s.clip = None;
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
        // The arrows follow the items as the view laid them out: tiles in rows, List's
        // columns, Details' rows (where Left and Right don't move).
        Key::Left if flow.sideways() => step(-across),
        Key::Right if flow.sideways() => step(across),
        Key::Up => step(-down),
        Key::Down => step(down),
        Key::PageUp => step(-page),
        Key::PageDown => step(page),
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
        // Back to the tree beside the files, as in Explorer.
        Key::Tab if ev.mods.shift => {
            tree::focus(s, p, true);
            tx.focus(ui::tree_key(p), true);
            return true;
        }
        Key::Tab => {
            let count = s.settings.pane_layout.count();
            if count > 1 {
                let next = (p + 1) % count;
                s.active = next;
                tx.focus(pane_key(next), true);
            }
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
            refresh(s, p);
            return true;
        }
        _ => return false,
    };
    if let Some(i) = to {
        let prev_single = (!ev.mods.cmd && !ev.mods.shift && s.panes[p].selected.len() == 1)
            .then_some(at)
            .flatten();
        let id = s.panes[p].entries[i].id.clone();
        // With Ctrl the cursor moves alone and the selection stays, as in Explorer.
        if ev.mods.cmd && !ev.mods.shift {
            s.active = p;
            s.kbd_sel[p].stop();
            s.panes[p].cursor = Some(id.clone());
        } else {
            click(s, p, &id, Mods { shift: ev.mods.shift, ..Mods::default() });
            if let Some(from_i) = prev_single
                && from_i != i
                && s.settings.selection_animated()
            {
                s.kbd_sel[p].start(from_i, i, s.now);
                tx.wake_at(s.now + 1.0 / 60.0);
            }
        }
        reveal(s, p, i, &id, tx);
    }
    true
}

/// Brings item `i` (id `id`) of pane `p` into view. List builds only the columns in sight, so its
/// items are reached by scrolling to where they sit; the other views reveal the row (or the
/// virtual line) that holds them.
fn reveal(s: &State, p: usize, i: usize, id: &str, tx: &mut Tx) {
    let flow = s.views[p].flow.get();
    if flow.rows {
        tx.scroll_into_view_in(pane_key(p), flow.reveal_key(i, row_key(id)));
    } else if let (_, Some([x, y])) = lasso::menu_point(&s.views[p], Some(i)) {
        tx.scroll_to_instant(ui::list_key(p), x, y);
    }
}

/// The Menu key, or Shift+F10, with nothing else held: the context menu from the keyboard.
fn menu_key(ev: &KeyEvent) -> bool {
    let m = ev.mods;
    let plain = !m.cmd && !m.ctrl && !m.alt && !m.logo;
    plain && (ev.key == Key::ContextMenu || (ev.key == Key::Function(10) && m.shift))
}

/// Opens pane `p`'s context menu as a right click would: on the selection, under the cursor's
/// row (or the first selected row, when the cursor is elsewhere), or on the folder at the top
/// of the list when nothing is selected. The pane's focus is a parent of the list, so Rune's
/// own handling of these keys never reaches the list's right-click handler.
fn menu_from_keys(s: &mut State, p: usize, tx: &mut Tx) {
    s.active = p;
    let pane = &s.panes[p];
    let row = if pane.selected.is_empty() {
        None
    } else {
        let cursor = pane.cursor.as_deref().filter(|c| pane.selected.contains(*c)).and_then(|c| pane.index_of(c));
        cursor.or_else(|| pane.entries.iter().position(|e| pane.selected.contains(&e.id)))
    };
    let (at, scroll) = lasso::menu_point(&s.views[p], row);
    if let Some([x, y]) = scroll {
        tx.scroll_to_instant(ui::list_key(p), x, y);
    }
    s.menu = Some((p, at));
}

fn set_sort(s: &mut State, p: usize, col: SortCol) {
    s.kbd_sel[p].stop();
    let animate = s.settings.selection_animated() && !s.panes[p].entries.is_empty();
    let (old_indices, vis_start, vis_end) = if animate {
        let scroll = s.views[p].scroll.get();
        let row_h = s.zoom[p].row_height().max(1.0);
        let first = (scroll.y / row_h).floor().max(0.0) as usize;
        let vis_count = ((scroll.h / row_h).ceil().max(24.0) as usize).min(120);
        let len = s.panes[p].entries.len();
        let win_start = first.saturating_sub(40);
        let win_end = (first + vis_count + 40).min(len);
        let mut map = HashMap::with_capacity(win_end.saturating_sub(win_start));
        for (i, e) in s.panes[p].entries[win_start..win_end].iter().enumerate() {
            map.insert(e.id.clone(), win_start + i);
        }
        (map, win_start, win_end)
    } else {
        (HashMap::new(), 0, 0)
    };
    let pane = &mut s.panes[p];
    pane.sort = if pane.sort.0 == col { (col, !pane.sort.1) } else { (col, true) };
    pane.sort_entries();
    if animate && !old_indices.is_empty() {
        let end = vis_end.min(pane.entries.len());
        let mut deltas = HashMap::new();
        for (offset, e) in pane.entries[vis_start..end].iter().enumerate() {
            let new_i = vis_start + offset;
            if let Some(&old_i) = old_indices.get(&e.id)
                && old_i != new_i
            {
                let d = (old_i as f32 - new_i as f32).clamp(-24.0, 24.0);
                deltas.insert(e.id.clone(), d);
            }
        }
        s.row_reorder[p].start(deltas, s.now);
    } else {
        s.row_reorder[p].clear();
    }
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
        dragout::Ended::Outside => s.status = t("Baixando para onde os itens foram soltos: o Windows mostra o andamento.", "Downloading to where the items were dropped: Windows shows the progress.").into(),
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
        s.panes[0].set_entries(Err(finder::missing().into()));
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
        Location::Found(_) | Location::Server { find: Some(_), .. } => {
            s.status = t("Os resultados da pesquisa não são uma pasta: solte os arquivos numa pasta.", "Search results aren't a folder: drop the files in a folder.").into();
            return;
        }
        Location::Local(dir) if network::is_server(dir) => {
            s.status = t("Aqui estão as pastas compartilhadas do computador: solte os arquivos numa delas.", "These are the computer's shared folders: drop the files in one of them.").into();
            return;
        }
        Location::Server { .. } => {
            let real: Vec<PathBuf> = files.iter().filter(|f| !f.starts_with(&s.temp)).cloned().collect();
            if real.is_empty() {
                s.status = nothing_to_upload();
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
                s.status = nothing_to_upload();
                return;
            }
            let left_out = files.len() - real.len();
            actions::upload(s, real, &loc, false, vec![loc.clone()]);
            if left_out > 0 {
                s.status = tr!("Itens do drive simulado não são enviados ({left_out}).", "Items of the simulated drive aren't uploaded ({left_out}).");
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
                s.status = tr!("Ignorados (já estão aqui): {skipped}.", "Skipped (already here): {skipped}.");
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
    s.status = tr!("Enviados (simulado): {done} · ignorados (já existem): {skipped}", "Uploaded (simulated): {done} · skipped (already there): {skipped}");
    load(s, p);
}

/// Files dropped on a server or Drive folder that are only the simulated drive's placeholders.
fn nothing_to_upload() -> String {
    t("Nada para enviar: arraste arquivos ou pastas do computador.", "Nothing to upload: drag files or folders from the computer.").into()
}

/// The clash dialog over a fake account's Drive folder, for sample files in %TEMP%.
fn clash_scene(s: &mut State, all: bool) {
    let dir = std::env::temp_dir().join("openexplorer-clash-scene");
    let _ = std::fs::create_dir_all(dir.join("Projetos"));
    for name in ["Relatório.pdf", "foto.jpg", "notas.txt"] {
        let _ = std::fs::write(dir.join(name), name);
    }
    let to = Location::Drive { email: "voce@gmail.com".into(), space: Space::MyDrive, path: vec![("x".into(), "Trabalho".into())] };
    s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into(), client_id: String::new() });
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

/// Opções open on Cache, its sizes fixed, then `then` applied to the page.
fn cache_scene(s: &mut State, then: fn(&mut caches::Page)) {
    let mut v = settings::OptionsView::open(settings::OptionsTab::Cache);
    v.cache.usage = Some(caches::Usage { drive: 3_565_158, mega: 48_211_968, dropbox: 1_204_224, b2: 1_204_224, onedrive: 1_310_720, telegram: 524_288 });
    then(&mut v.cache);
    s.dialog = Some(actions::Dialog::Options(Box::new(v)));
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

/// Rune's own labels in Portuguese while the interface is; English keeps Rune's. macOS writes
/// its menu items in title case ("Selecionar Tudo", "Ocultar Outros"), Windows and Linux in
/// sentence case ("Selecionar tudo"): each gets its system's.
fn rune_labels(label: rune::Label<'_>) -> Option<String> {
    rune_labels_on(label, cfg!(target_os = "macos"))
}

fn rune_labels_on(label: rune::Label<'_>, mac: bool) -> Option<String> {
    use rune::Label as L;
    if openexplorer_i18n::english() {
        return None;
    }
    let pick = |sentence: &str, title: &str| if mac { title.to_string() } else { sentence.to_string() };
    Some(match label {
        L::Cut => "Recortar".into(),
        L::Copy => "Copiar".into(),
        L::Paste => "Colar".into(),
        L::SelectAll => pick("Selecionar tudo", "Selecionar Tudo"),
        L::Undo => "Desfazer".into(),
        L::Redo => "Refazer".into(),
        L::LearnSpelling => pick("Aprender ortografia", "Aprender Ortografia"),
        L::NoGuesses => pick("Nenhuma sugestão", "Nenhuma Sugestão"),
        L::Spellcheck => pick("Verificação ortográfica", "Verificação Ortográfica"),
        L::SpellcheckLanguages => pick("Idiomas da verificação ortográfica", "Idiomas da Verificação Ortográfica"),
        L::Minimize => "Minimizar".into(),
        L::Maximize => "Maximizar".into(),
        L::Restore => "Restaurar".into(),
        L::Zoom => "Zoom".into(),
        L::Close => "Fechar".into(),
        L::BringAllToFront => pick("Trazer todas para a frente", "Trazer Todas para a Frente"),
        L::StartDictation => pick("Iniciar ditado…", "Iniciar Ditado…"),
        L::EmojiAndSymbols => pick("Emoji e símbolos", "Emoji e Símbolos"),
        L::About(app) => format!("Sobre o {app}"),
        L::Services => "Serviços".into(),
        L::Hide(app) => format!("Ocultar {app}"),
        L::HideOthers => pick("Ocultar outros", "Ocultar Outros"),
        L::ShowAll => pick("Mostrar tudo", "Mostrar Tudo"),
        L::Quit(app) => format!("Encerrar {app}"),
        L::MoreNestedLines(1) => "mais 1 linha aninhada".into(),
        L::MoreNestedLines(n) => format!("mais {n} linhas aninhadas"),
        L::OpenLink => pick("Abrir link", "Abrir Link"),
        L::CopyLink => pick("Copiar link", "Copiar Link"),
        L::SaveImageAs => pick("Salvar imagem como…", "Salvar Imagem Como…"),
        L::CopyImage => pick("Copiar imagem", "Copiar Imagem"),
        L::CopyImageAddress => pick("Copiar endereço da imagem", "Copiar Endereço da Imagem"),
        // A dictionary's or a menu's own name, and labels Rune adds later: as Rune has them.
        _ => return None,
    })
}

fn main() {
    // `openexplorer index …`: this executable as the search service (Windows starts it so), no
    // window at all.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == openexplorer_index::cli::ARG) {
        std::process::exit(openexplorer_index::cli::run(&args[1..]));
    }
    // `openexplorer --warm-gpu-cache`: the setup runs it once, hidden, as the person who
    // installs. Direct3D keeps the compiled pipelines per user and per exe path, so the first
    // launch reaches the GPU as fast as later ones (Rune: 1 to 2 s sooner). No window.
    if args.first().is_some_and(|a| a == "--warm-gpu-cache") {
        std::process::exit(match rune::warm_gpu_cache() {
            Ok(()) => 0,
            Err(e) => {
                openexplorer_gdrive::log(&format!("warm_gpu_cache: {e}"));
                1
            }
        });
    }
    let cli = updater::intercept_cli(args);
    if let Some(notice) = cli.notice {
        *STARTUP_UPDATE_NOTICE.lock().unwrap_or_else(|e| e.into_inner()) = Some(notice);
    }
    let initial = settings::Settings::load();
    // Rune's own words (a text field's menu, the window buttons' names for screen readers) in
    // the app's language.
    rune::set_labels(rune_labels);
    // Apply an update downloaded in an earlier run before opening the window. Not when this start
    // is the helper reporting that installing it failed: trying again at once would bring the
    // same refusal (a declined UAC prompt, a locked file) back at every start, the window never
    // opening. That update goes; the dialog can fetch and install it again.
    let install_failed = matches!(STARTUP_UPDATE_NOTICE.lock().unwrap_or_else(|e| e.into_inner()).as_ref(), Some(Err(_)));
    if install_failed {
        updater::clear_staged();
    } else if !rune::is_headless() && updater::install_kept(initial.auto_update) {
        std::process::exit(0);
    }
    accounts::start();
    let main_spec = AppSpec::<State> {
        min_size: Some((900.0, 420.0)),
        families: initial.font.families(),
        setup: Some(setup),
        poll: Some(poll),
        scenes: &[
            ("stress", |s| switch(s, 1, Location::Mock(vec!["Teste 50.000 arquivos".into()]))),
            // How a connected account looks, without a network: a fake account and a listing.
            ("account", |s| {
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into(), client_id: String::new() });
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
            // Opções open on each sidebar tab.
            ("options", |s| s.dialog = Some(actions::Dialog::Options(Box::new(settings::OptionsView::open(settings::OptionsTab::General))))),
            ("options-transfers", |s| s.dialog = Some(actions::Dialog::Options(Box::new(settings::OptionsView::open(settings::OptionsTab::Transfers))))),
            ("options-experimental", |s| s.dialog = Some(actions::Dialog::Options(Box::new(settings::OptionsView::open(settings::OptionsTab::Experimental))))),
            // Opções › Cache with fixed sizes (not the real folders'), as measured, asking to
            // clear, and after a clear that left a file in use.
            ("options-cache", |s| cache_scene(s, |_| {})),
            ("options-cache-confirm", |s| cache_scene(s, |p| p.confirming = true)),
            ("options-cache-error", |s| {
                cache_scene(s, |p| {
                    p.usage = Some(caches::Usage { drive: 0, mega: 48_211_968, dropbox: 0, b2: 0, onedrive: 0, telegram: 0 });
                    p.error = Some(tr!(
                        "Não foi possível apagar {}: O arquivo já está sendo usado por outro processo. (os error 32). Feche o que estiver usando o arquivo e tente de novo.",
                        "Couldn't delete {}: The process cannot access the file because it is being used by another process. (os error 32). Close whatever is using the file and try again.",
                        r"C:\Users\voce\AppData\Local\OpenExplorer\mega\3f9a0c1b.tree"
                    ));
                })
            }),
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
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into(), client_id: String::new() });
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
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into(), client_id: String::new() });
                s.panes[1].go(Location::Drive { email: "voce@gmail.com".into(), space: Space::MyDrive, path: vec![("x".into(), "Projetos".into())] });
                s.panes[1].set_entries(Ok(MockDrive::new().list(&["Documentos".into()])));
                s.address_menu = Some((1, rune::PointerPress { x: 820.0, y: 92.0 }));
            }),
            // A search of the computer with results, a disk still being read; and one without
            // the service installed.
            ("found", |s| found_scene(s, true)),
            // The same, with the experimental highlighter on its matches.
            ("found-marker", |s| {
                s.settings.search_marker = true;
                found_scene(s, true);
            }),
            ("found-missing", |s| found_scene(s, false)),
            // The Acrylic background where the system draws none (a headless run): Rune's
            // near-opaque neutral tint shows around the panes and through the folder trees.
            ("acrylic", |s| s.acrylic = true),
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
                p(3).end(Err("Copiar \"Episódio 07.mkv\": Não há espaço suficiente no disco.".into()));
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
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into(), client_id: String::new() });
                s.panes[1].go(Location::drive("voce@gmail.com", Space::Trash));
                s.panes[1].set_entries(Ok(view_sample()));
                s.panes[1].selected = ["f2".to_string()].into();
                s.menu = Some((1, rune::PointerPress { x: 900.0, y: 200.0 }));
            }),
            // Excluir permanentemente asking to confirm.
            ("purge", |s| {
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into(), client_id: String::new() });
                s.panes[1].go(Location::drive("voce@gmail.com", Space::Trash));
                s.panes[1].set_entries(Ok(view_sample()));
                s.panes[1].selected = ["f2".to_string(), "f3".to_string()].into();
                actions::ask_delete(s, 1);
            }),
            // Esvaziar lixeira asking to confirm.
            ("empty-trash", |s| {
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into(), client_id: String::new() });
                s.panes[1].go(Location::drive("voce@gmail.com", Space::Trash));
                s.panes[1].set_entries(Ok(view_sample()));
                actions::ask_empty_trash(s, 1);
            }),
            // Com estrela with the menu on a starred file, and the views in the tree.
            ("starred", |s| {
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into(), client_id: String::new() });
                s.panes[1].go(Location::drive("voce@gmail.com", Space::Starred));
                s.panes[1].set_entries(Ok(view_sample().into_iter().map(|e| Entry { starred: true, ..e }).collect()));
                s.panes[1].selected = ["f3".to_string()].into();
                s.menu = Some((1, rune::PointerPress { x: 900.0, y: 200.0 }));
            }),
            // An account's storage in its status bar, without a network.
            ("quota", |s| {
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into(), client_id: String::new() });
                s.quotas.insert("voce@gmail.com".into(), Some(Quota { limit: Some(16_106_127_360), usage: 2_898_000_000, in_drive: 2_100_000_000, in_trash: 12_000_000 }));
                s.panes[1].go(Location::drive("voce@gmail.com", Space::MyDrive));
                s.panes[1].set_entries(Ok(MockDrive::new().list(&["Documentos".into()])));
            }),
            // A MEGA account's Cloud Drive with its storage, without a network.
            ("mega", |s| {
                let sv = openexplorer_remote::Server::mega("voce@exemplo.com");
                s.servers.list.push(sv.clone());
                s.servers.mega_quotas.insert(sv.id.clone(), Some(openexplorer_mega::Quota { total: 21_474_836_480, used: 5_798_205_849, in_rubbish: 1_073_741_824 }));
                s.panes[1].go(mega::cloud(&sv));
                s.panes[1].set_entries(Ok(MockDrive::new().list(&["Documentos".into()])));
                s.active = 1;
            }),
            // Adding a MEGA account that asks for its two-factor code.
            // A Dropbox account's top with its storage, without a network.
            ("dropbox", |s| {
                let sv = openexplorer_remote::Server::dropbox("voce@exemplo.com");
                s.servers.list.push(sv.clone());
                s.servers.dropbox_quotas.insert(sv.id.clone(), Some(openexplorer_dropbox::Quota { used: 1_288_490_189, total: 2_199_023_255_552, team: false }));
                s.panes[1].go(dropbox::top(&sv));
                s.panes[1].set_entries(Ok(MockDrive::new().list(&["Documentos".into()])));
                s.active = 1;
            }),
            // A Dropbox folder's deleted items, two ticked.
            ("dropbox-deleted", |s| {
                let sv = openexplorer_remote::Server::dropbox("voce@exemplo.com");
                s.servers.list.push(sv.clone());
                s.panes[1].go(servers::folder(&sv, vec!["Documentos".into()]));
                s.panes[1].set_entries(Ok(MockDrive::new().list(&["Documentos".into()])));
                s.active = 1;
                let d = |name: &str, dir, size, when| openexplorer_dropbox::Deleted { name: name.into(), path: format!("/Documentos/{name}"), dir, rev: None, size, when };
                let items = vec![
                    d("Relatório final.docx", Some(false), Some(48_213), Some(1_763_560_800)),
                    d("orçamento 2026.xlsx", Some(false), Some(12_904), Some(1_763_474_400)),
                    d("Fotos da viagem", Some(true), None, None),
                    d("rascunho.txt", None, None, None),
                ];
                s.dialog = Some(actions::Dialog::DropboxDeleted(Box::new(dropbox::DeletedView { server: sv, path: "/Documentos".into(), place: "Documentos".into(), picked: vec![true, false, true, false], items: Some(Ok(items)) })));
            }),
            // A B2 key's bucket, without a network.
            ("b2", |s| {
                let sv = openexplorer_remote::Server::b2("0051a2b3c4d5e6f0000000001", "B2 · fotos-familia");
                s.servers.list.push(sv.clone());
                s.panes[1].go(servers::folder(&sv, vec!["fotos-familia".into()]));
                s.panes[1].set_entries(Ok(MockDrive::new().list(&["Documentos".into()])));
                s.active = 1;
            }),
            // A B2 bucket's unfinished uploads, without a network.
            ("b2-unfinished", b2::unfinished_scene),
            // A Telegram account's channel as folders, without a network.
            ("telegram", |s| {
                let sv = openexplorer_remote::Server::telegram(123456789, "Você");
                s.servers.list.push(sv.clone());
                s.panes[1].go(servers::folder(&sv, vec!["Documentos".into()]));
                s.panes[1].set_entries(Ok(MockDrive::new().list(&["Documentos".into()])));
                s.active = 1;
            }),
            // A OneDrive account's files with its storage, without a network.
            ("onedrive", |s| {
                let sv = openexplorer_remote::Server::onedrive("b!exemplo", "voce@exemplo.com");
                s.servers.list.push(sv.clone());
                s.servers.onedrive.quotas.insert(sv.id.clone(), Some(openexplorer_onedrive::Quota { total: 1_099_511_627_776, used: 214_748_364_800, remaining: 884_763_262_976, deleted: 1_073_741_824, state: "normal".into() }));
                s.panes[1].go(onedrive::files(&sv));
                s.panes[1].set_entries(Ok(MockDrive::new().list(&["Documentos".into()])));
                s.active = 1;
            }),
            // A work OneDrive account's Lixeira, with where each item came from, without a network.
            ("onedrive-bin", |s| onedrive::scene_bin(s, true)),
            // The same Lixeira before SharePoint's permission is granted.
            ("onedrive-bin-consent", |s| onedrive::scene_bin(s, false)),
            // The question Copiar link asks on a OneDrive file with no link.
            ("onedrive-link", |s| {
                let sv = openexplorer_remote::Server::onedrive("b!exemplo", "voce@exemplo.com");
                if let Some(item) = MockDrive::new().list(&["Documentos".into()]).into_iter().find(|e| !e.is_dir) {
                    s.dialog = Some(actions::Dialog::OneDriveLink(Box::new(onedrive::MakeLink { server: sv, path: "/Meus arquivos/x".into(), item })));
                }
            }),
            // Compartilhar on a file shared with a few people and the link, without a network.
            ("share", |s| {
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into(), client_id: String::new() });
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
                s.accounts.push(StoredAccount { email: "voce@gmail.com".into(), name: "Você".into(), client_id: String::new() });
                s.panes[1].go(Location::drive("voce@gmail.com", Space::SharedDrives));
                let d = |id: &str, name: &str| Entry { id: id.into(), name: name.into(), is_dir: true, size: None, modified: None, target: None, mime: None, md5: None, starred: false };
                s.panes[1].set_entries(Ok(vec![d("0AAa", "Equipe de design"), d("0AAb", "Financeiro"), d("0AAc", "Projetos 2026")]));
            }),
            ("accounts-hub", accounts_hub_scene),
            ("accounts-hub-empty", |s| {
                s.accounts.clear();
                s.servers.list.clear();
                s.main_tab = MainTab::Accounts;
            }),
            ("accounts-hub-light", |s| {
                accounts_hub_scene(s);
                s.settings.theme = settings::Theme::Light;
            }),
            ("update-available", |s| {
                s.updater.support = Ok(updater::InstallKind::WindowsInstalled { install_dir: PathBuf::from(r"C:\Program Files\OpenExplorer") });
                s.updater.release = Some(updater::Release {
                    version: "0.5.0".into(),
                    url: "https://github.com/orezraey/OpenExplorer/releases/tag/v0.5.0".into(),
                    notes: "- Atualização integrada no aplicativo\n- Melhorias de desempenho na listagem".into(),
                    asset_name: "OpenExplorer-0.5.0-windows-x64-setup.exe".into(),
                    download_url: "https://example.invalid/setup.exe".into(),
                    size: 18_400_000,
                    sha256: None,
                    signed: None,
                });
                s.updater.download = updater::DownloadState::Idle;
                s.dialog = Some(actions::Dialog::Update);
            }),
            ("update-downloading", |s| {
                s.updater.support = Ok(updater::InstallKind::WindowsInstalled { install_dir: PathBuf::from(r"C:\Program Files\OpenExplorer") });
                s.updater.release = Some(updater::Release {
                    version: "0.5.0".into(),
                    url: "https://github.com/orezraey/OpenExplorer/releases/tag/v0.5.0".into(),
                    notes: String::new(),
                    asset_name: "OpenExplorer-0.5.0-windows-x64-setup.exe".into(),
                    download_url: "https://example.invalid/setup.exe".into(),
                    size: 18_400_000,
                    sha256: None,
                    signed: None,
                });
                s.updater.download = updater::DownloadState::Downloading { received: 11_200_000, total: 18_400_000 };
                s.dialog = Some(actions::Dialog::Update);
            }),
            ("update-ready", |s| {
                s.updater.support = Ok(updater::InstallKind::WindowsInstalled { install_dir: PathBuf::from(r"C:\Program Files\OpenExplorer") });
                s.updater.release = Some(updater::Release {
                    version: "0.5.0".into(),
                    url: "https://github.com/orezraey/OpenExplorer/releases/tag/v0.5.0".into(),
                    notes: "- Atualização integrada no aplicativo\n- Melhorias de desempenho na listagem".into(),
                    asset_name: "OpenExplorer-0.5.0-windows-x64-setup.exe".into(),
                    download_url: "https://example.invalid/setup.exe".into(),
                    size: 18_400_000,
                    sha256: None,
                    signed: None,
                });
                s.updater.download = updater::DownloadState::Ready(updater::Staged {
                    version: "0.5.0".into(),
                    file_path: PathBuf::from("staged.exe"),
                    sha256: "0".repeat(64),
                    install_kind: updater::InstallKind::WindowsInstalled { install_dir: PathBuf::from(r"C:\Program Files\OpenExplorer") },
                    release_url: "https://github.com/orezraey/OpenExplorer/releases/tag/v0.5.0".into(),
                    signed: Default::default(),
                });
                s.dialog = Some(actions::Dialog::Update);
            }),
        ],
        ..AppSpec::new("Open Explorer", (1280.0, 760.0), init, ui::view)
    };
    let account_app = rune::App::new(AppSpec::<State> {
        min_size: Some((680.0, 440.0)),
        families: initial.font.families(),
        setup: Some(setup_account_window),
        poll: Some(poll_account_window),
        on_key: Some(account_window_key),
        scenes: &[
            ("add-account", |s| servers::ask_add(s)),
            ("gdrive-menu", |s| {
                let mut f = servers::Form::new();
                f.open_group = Some(servers::Provider::GoogleDrive);
                s.open_account_form(f);
            }),
            ("onedrive-menu", |s| {
                let mut f = servers::Form::new();
                f.open_group = Some(servers::Provider::OneDrive);
                s.open_account_form(f);
            }),
            ("add-account-nas", |s| {
                let mut f = servers::Form::new();
                f.category = servers::Category::Nas;
                s.open_account_form(f);
            }),
            ("add-account-search", |s| {
                let mut f = servers::Form::new();
                f.filter = rune::TextEdit::new("drive");
                s.open_account_form(f);
            }),
            ("add-gdrive", |s| {
                let mut f = servers::Form::for_variant(servers::ProviderVariant::GoogleDriveCustom);
                f.custom_oauth = true;
                s.open_account_form(f);
            }),
            ("add-nextcloud", |s| {
                let mut f = servers::Form::for_provider(servers::Provider::Nextcloud);
                f.host = rune::TextEdit::new("cloud.exemplo.com");
                f.user = rune::TextEdit::new("ana");
                s.open_account_form(f);
            }),
            ("add-server", |s| {
                let mut f = servers::Form::for_provider(servers::Provider::Sftp);
                f.host = rune::TextEdit::new("nas.local");
                f.user = rune::TextEdit::new("ana");
                f.password = rune::TextEdit::new("segredo").secure();
                s.open_account_form(f);
            }),
            // Plain FTP, with its warning; and a NAS's own certificate, turned down: the form shows it
            // and asks to trust it.
            ("add-server-ftp", |s| {
                let mut f = servers::Form::for_provider(servers::Provider::Ftp);
                f.protocol = openexplorer_remote::Protocol::Ftp;
                f.host = rune::TextEdit::new("192.168.0.10");
                f.error = Some("192.168.0.10 recusou o usuário ou a senha.".into());
                s.open_account_form(f);
            }),
            ("add-server-cert", |s| {
                let mut f = servers::Form::for_provider(servers::Provider::WebDav);
                f.protocol = openexplorer_remote::Protocol::Davs;
                f.host = rune::TextEdit::new("nas.local");
                f.user = rune::TextEdit::new("ana");
                f.untrusted = Some(openexplorer_remote::Rejected {
                    fingerprint: "75:25:66:B8:60:50:B3:6B:5E:1A:58:FE:38:DE:1A:DB:F5:D6:86:41:DC:8C:9F:50:75:A6:E5:33:69:5A:A6:36".into(),
                    why: "não foi emitido por uma autoridade conhecida (é um certificado próprio)".into(),
                });
                s.open_account_form(f);
            }),
            // Adding a Dropbox account before its app is set up.
            ("dropbox-add", |s| {
                let mut f = servers::Form::for_provider(servers::Provider::Dropbox);
                f.custom_oauth = true;
                f.error = Some("App do Dropbox não configurado.".into());
                s.open_account_form(f);
            }),
            ("mega-add", |s| {
                let mut f = servers::Form::for_provider(servers::Provider::Mega);
                f.user = rune::TextEdit::new("voce@exemplo.com");
                f.password = rune::TextEdit::new("segredo").secure();
                f.needs_code = true;
                f.error = Some(openexplorer_mega::Refused::NeedsCode.text());
                s.open_account_form(f);
            }),
            // Adding a B2 key, turned down.
            ("b2-add", |s| {
                let mut f = servers::Form::for_provider(servers::Provider::B2);
                f.user = rune::TextEdit::new("0051a2b3c4d5e6f0000000001");
                f.password = rune::TextEdit::new("K005abcdefghijklmnopqrstuvwxyz0").secure();
                f.error = Some(openexplorer_i18n::t("O ID da chave ou a chave de aplicação estão errados (ou a chave foi apagada).", "The key ID or the application key is wrong (or the key was deleted).").into());
                s.open_account_form(f);
            }),
            // Connecting a Telegram account: its first step, its code, its password.
            ("telegram-add", |s| {
                let mut f = servers::Form::for_provider(servers::Provider::Telegram);
                f.client_id = rune::TextEdit::new("1234567");
                f.client_secret = rune::TextEdit::new("0123456789abcdef0123456789abcdef").secure();
                f.user = rune::TextEdit::new("+55 11 99999-8888");
                s.open_account_form(f);
            }),
            ("telegram-code", |s| {
                let mut f = servers::Form::for_provider(servers::Provider::Telegram);
                f.user = rune::TextEdit::new("+55 11 99999-8888");
                f.telegram = telegram::Stage::Code;
                f.error = Some(openexplorer_i18n::t("O código não confere.", "The code doesn't match.").into());
                s.open_account_form(f);
            }),
            ("telegram-password", |s| {
                let mut f = servers::Form::for_provider(servers::Provider::Telegram);
                f.telegram = telegram::Stage::Password { hint: "gato".into() };
                f.password = rune::TextEdit::new("segredo").secure();
                s.open_account_form(f);
            }),
            // Adding a OneDrive account before an app registration is set up.
            ("onedrive-add", |s| {
                let mut f = servers::Form::for_provider(servers::Provider::OneDrive);
                f.custom_oauth = true;
                f.error = Some(t("Cliente OAuth do OneDrive não configurado.", "OneDrive OAuth client not set up.").into());
                s.open_account_form(f);
            }),
        ],
        ..AppSpec::new("Registrar conta", (980.0, 640.0), init_account_window, ui::account_window_view)
    })
    .windowless()
    .titlebar(rune::Titlebar::new(36.0))
    .font_smoothing(initial.text_render.smoothing())
    .turn_scope(enter_turn, leave_turn)
    // Linux has no system pop-up menu: the fields' menu is drawn in the window, as in the main
    // one. Rune dismisses it when the window loses focus: it goes.
    .on_popup_menu(|s, request, tx| {
        if s.is_placeholder {
            tx.resolve_popup_menu(None);
        } else {
            s.account_popup = Some(request);
        }
    })
    .on_focus(|s, focused, _| {
        if !focused && !s.is_placeholder {
            s.account_popup = None;
        }
    })
    .on_close(|s, _| {
        if !s.is_placeholder {
            s.account_popup = None;
            s.account_window_closed();
        }
    });

    let mut main_app = rune::App::new(main_spec)
        .titlebar(rune::Titlebar::new(34.0))
        .font_smoothing(initial.text_render.smoothing())
        .args(rune_args(cli.args.into_iter()))
        .turn_scope(enter_turn, leave_turn)
        // B2 folders and Telegram accounts on screen look for changes only while the window is in
        // front.
        .on_focus(|s, focused, tx| {
            if s.is_placeholder {
                PENDING_MAIN_FOCUS.with(|c| c.set(Some(focused)));
            } else {
                // Rune dismisses a Linux in-window menu when the window loses focus: it goes.
                if !focused {
                    s.popup = None;
                }
                accounts::window_focus(s, focused, tx);
            }
        })
        // The app ends without returning from `run`: what the cloud accounts changed since their
        // trees and listings were last kept is kept here, so the next start has less to catch up on.
        .on_quit(|_, _| {
            accounts::save_caches();
            if !updater::run_pending_handoff() && RESTART.load(Ordering::Relaxed) {
                relaunch();
            }
        })
        .on_close_request(|s, _| actions::close_requested(s))
        // macOS: ⌘Q, Encerrar in the Dock, logging out. Elsewhere Rune never calls it.
        .on_quit_request(actions::quit_requested)
        // Linux has no system pop-up menu: a text field's is drawn in the window.
        .on_popup_menu(|s, request, _| s.popup = Some(request))
        // It opens where it was closed, as Explorer's does.
        .on_window(window_place::created)
        .on_window_geometry(|_, _, _| {
            window_place::resized();
            false
        })
        .on_close(|s, _| window_place::remember(s));
    let place = window_place::options(initial.window);
    // Experimental: Mica shows through a transparent window, which only its creation sets.
    let mica = initial.mica && cfg!(windows);
    STARTED_WITH_MICA.store(mica, Ordering::Relaxed);
    MICA_ASKED.with(|a| a.set(mica.then_some(initial.material)));
    // Experimental too, off Windows: the system's material behind the page and the folder
    // trees, which also needs a transparent window.
    let acrylic = initial.acrylic_window();
    if place.is_some() || mica || acrylic {
        let maximized = place.is_some() && initial.window.is_some_and(|w| w.maximized);
        main_app = main_app.window_options(rune::WindowOptions { transparent: mica || acrylic, ..place.unwrap_or_default() }).maximized(maximized);
    }
    if mica {
        main_app = main_app.window_backdrop(initial.material.backdrop());
    }
    // The material's tint follows the app's theme, not the system's.
    if mica || acrylic {
        main_app = main_app.color_scheme(initial.theme.color_scheme());
    }
    // Experimental: content that keeps up with a drag of the left or top edge. Rune ignores it
    // in a transparent window, so not with Mica or Acrylic.
    if initial.resize_sync && cfg!(windows) {
        main_app = main_app.resize_sync(rune::ResizeSync::On);
    }

    let (account_handle, _) = main_app.window(rune::windows::WindowSpec::from_app("add-account", account_app));
    let main_handle = main_app.handle();
    main_app
        .init(move || {
            let mut s = init();
            s.account_win = Some(account_handle);
            s.main_win = Some(main_handle);
            s
        })
        .run();
}

/// Central de Contas with connected cloud accounts and servers, without a network.
fn accounts_hub_scene(s: &mut State) {
    s.accounts = vec![
        StoredAccount { email: "caua@gmail.com".into(), name: "Cauã Silva".into(), client_id: String::new() },
    ];
    s.quotas.insert(
        "caua@gmail.com".into(),
        Some(Quota { limit: Some(16_106_127_360), usage: 3_113_851_289, in_drive: 2_400_000_000, in_trash: 18_000_000 }),
    );
    let od_acc = openexplorer_onedrive::StoredAccount {
        id: "b!exemplo".into(),
        email: "caua@outlook.com".into(),
        name: "Cauã Silva".into(),
        business: false,
    };
    let od_sv = onedrive::server_of(&od_acc);
    s.servers.onedrive.accounts = vec![od_acc];
    s.servers.onedrive.quotas.insert(
        od_sv.id.clone(),
        Some(openexplorer_onedrive::Quota {
            total: 1_099_511_627_776,
            used: 214_748_364_800,
            remaining: 884_763_262_976,
            deleted: 1_073_741_824,
            state: "normal".into(),
        }),
    );
    let mega_sv = openexplorer_remote::Server::mega("caua@mega.nz");
    s.servers.mega_quotas.insert(
        mega_sv.id.clone(),
        Some(openexplorer_mega::Quota { total: 21_474_836_480, used: 5_798_205_849, in_rubbish: 536_870_912 }),
    );
    let sftp_sv = openexplorer_remote::Server {
        id: "nas-home".into(),
        name: "NAS de Casa".into(),
        protocol: openexplorer_remote::Protocol::Sftp,
        brand: None,
        host: "192.168.0.10".into(),
        port: 22,
        user: "caua".into(),
        root: "/mnt/storage".into(),
        key: None,
        host_key: None,
        cert: None,
        connections: 4,
        implicit: false,
    };
    s.servers.list = vec![od_sv, mega_sv, sftp_sv];
    s.panes[1].go(Location::drive("caua@gmail.com", Space::MyDrive));
    s.main_tab = MainTab::Accounts;
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

    /// The tests that read or write `RESTART`, shared by the whole process, run one at a time.
    static RESTART_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn restart_tests() -> std::sync::MutexGuard<'static, ()> {
        RESTART_TESTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A close the app asked for, whose `on_close` comes after Adicionar conta opened a new
    /// form, keeps that form; the window's own close button drops it.
    #[test]
    fn account_close_keeps_a_form_opened_since() {
        let mut s = init();
        s.account_closes = 1;
        s.account_form = Some(Box::new(servers::Form::new()));
        s.account_window_closed();
        assert!(s.account_form.is_some() && s.account_closes == 0);
        s.account_window_closed();
        assert!(s.account_form.is_none());
    }

    /// Closing the window asks only while a transfer runs, over the dialog open, which stays;
    /// a second close goes through and stops the transfers, and the question goes away by
    /// itself once they end.
    #[test]
    fn closing_asks_only_while_transfers_run() {
        let _one_at_a_time = restart_tests();
        let mut s = init();
        assert_eq!(actions::close_requested(&mut s), rune::CloseResponse::Close);
        let end = || transfers::End::local(std::path::Path::new(r"C:\x"));
        let batch = transfers::queue(&mut s.transfers, None, transfers::Op::Copy, [(end(), end())]);
        s.dialog = Some(actions::Dialog::EmptyTrash { email: "ana@gmail.com".into() });
        assert_eq!(actions::close_requested(&mut s), rune::CloseResponse::Defer);
        assert!(s.quit_asked && matches!(s.dialog, Some(actions::Dialog::EmptyTrash { .. })));
        assert!(!actions::quit_question_done(&mut s) && s.quit_asked);
        actions::keep_running(&mut s);
        assert!(!s.quit_asked && matches!(s.dialog, Some(actions::Dialog::EmptyTrash { .. })));

        assert_eq!(actions::close_requested(&mut s), rune::CloseResponse::Defer);
        assert_eq!(actions::close_requested(&mut s), rune::CloseResponse::Close);
        assert!(!s.quit_asked && !actions::transfers_running(&s));
        drop(batch);

        let other = transfers::queue(&mut s.transfers, None, transfers::Op::Copy, [(end(), end())]);
        assert_eq!(actions::close_requested(&mut s), rune::CloseResponse::Defer);
        drop(other);
        assert!(actions::quit_question_done(&mut s) && !s.quit_asked);
        assert_eq!(actions::close_requested(&mut s), rune::CloseResponse::Close);

        // Reiniciar put off by a transfer asks the same question; once the transfer ends by itself
        // and the question goes, a later plain close doesn't open the app again.
        let last = transfers::queue(&mut s.transfers, None, transfers::Op::Copy, [(end(), end())]);
        actions::restart(&mut s, &mut Tx::default());
        assert!(s.quit_asked && actions::restarting());
        drop(last);
        assert!(actions::quit_question_done(&mut s) && !actions::restarting());
    }

    #[test]
    fn quitting_the_app_asks_like_closing() {
        let _one_at_a_time = restart_tests();
        let mut s = init();
        let mut tx = Tx::default();
        assert_eq!(actions::quit_requested(&mut s, &mut tx), rune::QuitResponse::Quit);
        let end = || transfers::End::local(std::path::Path::new(r"C:\x"));
        let batch = transfers::queue(&mut s.transfers, None, transfers::Op::Copy, [(end(), end())]);
        assert_eq!(actions::quit_requested(&mut s, &mut tx), rune::QuitResponse::Defer);
        assert!(s.quit_asked && s.quit_app);
        actions::keep_running(&mut s);
        assert!(!s.quit_asked && !s.quit_app);

        // Asked by a quit, answered with Fechar mesmo assim: the transfers stop.
        assert_eq!(actions::quit_requested(&mut s, &mut tx), rune::QuitResponse::Defer);
        actions::quit_anyway(&mut s, &mut tx);
        assert!(!s.quit_asked && !s.quit_app && !actions::transfers_running(&s));
        drop(batch);

        // A quit asked again while the question shows quits; a window's close asking after a
        // quit did is a close again.
        let other = transfers::queue(&mut s.transfers, None, transfers::Op::Copy, [(end(), end())]);
        assert_eq!(actions::quit_requested(&mut s, &mut tx), rune::QuitResponse::Defer);
        assert_eq!(actions::close_requested(&mut s), rune::CloseResponse::Close);
        assert!(!s.quit_app);
        drop(other);
        let last = transfers::queue(&mut s.transfers, None, transfers::Op::Copy, [(end(), end())]);
        assert_eq!(actions::quit_requested(&mut s, &mut tx), rune::QuitResponse::Defer);
        assert_eq!(actions::quit_requested(&mut s, &mut tx), rune::QuitResponse::Quit);
        assert!(!s.quit_asked && !s.quit_app && !actions::transfers_running(&s));
        drop(last);

        // ⌘Q while Reiniciar asks quits, and the app doesn't open again.
        let again = transfers::queue(&mut s.transfers, None, transfers::Op::Copy, [(end(), end())]);
        actions::restart(&mut s, &mut tx);
        assert!(s.quit_asked && actions::restarting());
        assert_eq!(actions::quit_requested(&mut s, &mut tx), rune::QuitResponse::Quit);
        assert!(!s.quit_asked && !actions::restarting());
        drop(again);
    }

    #[test]
    fn runes_words_in_portuguese() {
        // Tests run in Portuguese (`openexplorer_i18n`); English keeps Rune's own words.
        assert_eq!(rune_labels(rune::Label::Cut).as_deref(), Some("Recortar"));
        assert_eq!(rune_labels_on(rune::Label::SelectAll, false).as_deref(), Some("Selecionar tudo"));
        // macOS writes its menu items in title case.
        assert_eq!(rune_labels_on(rune::Label::SelectAll, true).as_deref(), Some("Selecionar Tudo"));
        assert_eq!(rune_labels_on(rune::Label::HideOthers, true).as_deref(), Some("Ocultar Outros"));
        assert_eq!(rune_labels(rune::Label::Quit("Open Explorer")).as_deref(), Some("Encerrar Open Explorer"));
        assert_eq!(rune_labels(rune::Label::MoreNestedLines(3)).as_deref(), Some("mais 3 linhas aninhadas"));
        assert_eq!(rune_labels(rune::Label::Language("pt-BR")), None);
    }

    /// What a running transfer queue costs the window: 40 downloads advancing, each tick
    /// through `poll`, the view and a frame, as the real loop does (no GPU). Run it with
    /// `cargo test --release -p openexplorer -- --ignored --nocapture queue_progress_cost`.
    #[test]
    #[ignore = "benchmark"]
    fn queue_progress_cost() {
        use std::sync::atomic::Ordering::Relaxed;
        let mut s = init();
        let end = |i: usize| transfers::End::local(std::path::Path::new(&format!(r"C:\Dados\Arquivo {i}.bin")));
        let batch = transfers::queue(&mut s.transfers, None, transfers::Op::Download, (0..40).map(|i| (end(i), end(i))));
        for t in &s.transfers {
            t.progress.size(1 << 34);
            t.progress.begin();
        }
        let rows: Vec<_> = s.transfers.iter().map(|t| t.progress.clone()).collect();
        let mut rt = rune::Runtime::new(s, ui::view, (1600.0, 1000.0), rune::Env::new(false, 1.0));
        let mut now = 0.0;
        let mut tick = |rt: &mut rune::Runtime<State>| {
            for p in &rows {
                p.bytes.fetch_add(3 << 20, Relaxed);
            }
            now += 0.1;
            rt.update_if(|s, tx| poll(s, now, tx));
            rt.frame(0.1);
        };
        for _ in 0..50 {
            tick(&mut rt);
        }
        let mut runs = Vec::new();
        for _ in 0..9 {
            let started = std::time::Instant::now();
            for _ in 0..200 {
                tick(&mut rt);
            }
            runs.push(started.elapsed().as_secs_f64() * 1e6 / 200.0);
        }
        runs.sort_by(f64::total_cmp);
        println!("queue tick: {:.0} µs best, {:.0} µs median of 9 runs of 200", runs[0], runs[4]);
        drop(batch);
    }

    /// The mouse's side buttons step through a pane's history as Voltar and Avançar do; the
    /// wheel's press is left alone.
    #[test]
    fn side_buttons_go_back_and_forward() {
        let mut s = init();
        let start = s.panes[1].loc.clone();
        let there = Location::Mock(vec!["Documentos".into()]);
        navigate(&mut s, 1, there.clone());
        s.active = 0;
        assert!(side_button(&mut s, 1, rune::PointerButton::Back));
        assert_eq!((&s.panes[1].loc, s.active), (&start, 1));
        assert!(side_button(&mut s, 1, rune::PointerButton::Forward));
        assert_eq!(s.panes[1].loc, there);
        assert!(!side_button(&mut s, 1, rune::PointerButton::Middle));
        assert_eq!(s.panes[1].loc, there);
        // Over the transfer queue or the toolbar: the active pane; not while Contas shows.
        s.main_tab = MainTab::Accounts;
        assert!(!side_button_in_window(&mut s, rune::PointerButton::Back));
        s.main_tab = MainTab::Explore;
        assert!(side_button_in_window(&mut s, rune::PointerButton::Back));
        assert_eq!(s.panes[1].loc, start);
    }

    #[test]
    fn accounts_hub_lists_filters_opens_panes_and_removes() {
        let mut s = init();
        s.accounts = vec![StoredAccount { email: "ana@gmail.com".into(), name: "Ana Souza".into(), client_id: String::new() }];
        s.quotas.insert(
            "ana@gmail.com".into(),
            Some(Quota { limit: Some(15_000_000_000), usage: 5_000_000_000, in_drive: 4_000_000_000, in_trash: 0 }),
        );
        let sftp = openexplorer_remote::Server {
            id: "s1".into(),
            name: "NAS Escritório".into(),
            protocol: openexplorer_remote::Protocol::Sftp,
            brand: None,
            host: "10.0.0.5".into(),
            port: 22,
            user: "ana".into(),
            root: "/dados".into(),
            key: None,
            host_key: None,
            cert: None,
            connections: 4,
            implicit: false,
        };
        s.servers.list = vec![sftp];
        set_main_tab(&mut s, MainTab::Accounts);

        let cards = ui::hub_cards(&s);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].drive_name, "GoogleDrive");
        assert_eq!(cards[0].user_name.as_deref(), Some("Ana Souza"));
        assert_eq!(cards[0].email_or_addr, "ana@gmail.com");
        assert!(matches!(cards[0].quota, ui::HubQuota::Known { total: 15_000_000_000, free: 10_000_000_000, .. }));
        assert!(cards[0].matches_search("souza"));
        assert!(!cards[0].matches_search("10.0.0.5"));

        assert_eq!(cards[1].drive_name, "SFTP");
        assert_eq!(cards[1].user_name.as_deref(), Some("NAS Escritório"));
        assert_eq!(cards[1].email_or_addr, "ana@10.0.0.5");
        assert!(cards[1].edit_id.is_some());

        // Opening in left pane switches back to Explorar and navigates pane 0.
        open_account_in_pane(&mut s, 0, cards[0].open_loc.clone());
        assert_eq!(s.main_tab, MainTab::Explore);
        assert_eq!(s.active, 0);
        assert_eq!(s.panes[0].loc, Location::drive("ana@gmail.com", Space::MyDrive));
    }

    /// What one window turn costs in the move of the shared state: Rune runs a turn for every
    /// event-loop callback of each window (events, wakes, every frame of an animation).
    #[test]
    #[ignore = "benchmark: cargo test --release -p openexplorer turn_swap_cost -- --ignored --nocapture"]
    fn turn_swap_cost() {
        let (mut main_win, mut account_win) = (init(), State::placeholder());
        let rounds = 500;
        let started = std::time::Instant::now();
        for _ in 0..rounds {
            leave_turn(&mut main_win);
            enter_turn(&mut account_win);
            leave_turn(&mut account_win);
            enter_turn(&mut main_win);
        }
        let per_turn = started.elapsed().as_secs_f64() * 1e6 / f64::from(2 * rounds);
        println!("one turn (enter + leave): {per_turn:.2} µs");
        assert!(!main_win.is_placeholder && account_win.is_placeholder);
    }

    /// F2, then a click on another row: the rename ends (the name kept as it was) and the click
    /// selects, so the keys aren't left with a field no one is typing in. Before, the rename
    /// stayed open and both panes' arrows went to it.
    #[test]
    fn a_click_on_another_row_ends_a_rename() {
        let mut s = init();
        s.panes[0].go(Location::Mock(Vec::new()));
        s.panes[0].set_entries(Ok(MockDrive::new().list(&[])));
        let ids: Vec<String> = s.panes[0].entries.iter().map(|e| e.id.clone()).collect();
        let (a, b) = (&ids[0], &ids[1]);
        let item = s.panes[0].entry(a).cloned().unwrap();
        s.name_edit = actions::name_field(&item);
        s.renaming = Some((0, item));
        click(&mut s, 0, b, Mods::default());
        assert!(s.renaming.is_none());
        assert_eq!(s.panes[0].selected.iter().collect::<Vec<_>>(), [b]);
        // A name that can't be taken keeps the field, and the click waits.
        let item = s.panes[0].entry(a).cloned().unwrap();
        s.name_edit = rune::TextEdit::new("");
        s.renaming = Some((0, item));
        click(&mut s, 0, a, Mods::default());
        assert!(s.renaming.is_some());
        assert_eq!(s.panes[0].selected.iter().collect::<Vec<_>>(), [b]);
        assert!(rename_focused("rename-input") && !rename_focused("pane-0"));
    }

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

    /// The median of 9 runs of `per_run` calls to `step`, in µs per call, and the best.
    fn bench_runs(per_run: usize, mut step: impl FnMut()) -> (f64, f64) {
        for _ in 0..per_run / 4 {
            step();
        }
        let mut runs = Vec::new();
        for _ in 0..9 {
            let started = std::time::Instant::now();
            for _ in 0..per_run {
                step();
            }
            runs.push(started.elapsed().as_secs_f64() * 1e6 / per_run as f64);
        }
        runs.sort_by(f64::total_cmp);
        (runs[0], runs[4])
    }

    /// What List view costs the window while transfers run: both panes in List on a folder of
    /// 1 999 items (just under the count past which List goes virtual), 40 downloads advancing,
    /// each tick through `poll`, the view and a frame (no GPU). Run it with
    /// `cargo test --release -p openexplorer -- --ignored --nocapture list_view_progress_cost`.
    #[test]
    #[ignore = "benchmark"]
    fn list_view_progress_cost() {
        use std::sync::atomic::Ordering::Relaxed;
        let mut s = init();
        for p in 0..2 {
            s.panes[p].set_entries(Ok(entries(0, 1999)));
            s.panes[p].view_mode = model::ViewMode::List;
        }
        let end = |i: usize| transfers::End::local(std::path::Path::new(&format!(r"C:\Dados\Arquivo {i}.bin")));
        let batch = transfers::queue(&mut s.transfers, None, transfers::Op::Download, (0..40).map(|i| (end(i), end(i))));
        for t in &s.transfers {
            t.progress.size(1 << 34);
            t.progress.begin();
        }
        let rows: Vec<_> = s.transfers.iter().map(|t| t.progress.clone()).collect();
        let mut rt = rune::Runtime::new(s, ui::view, (1600.0, 1000.0), rune::Env::new(false, 1.0));
        let mut now = 0.0;
        let (best, median) = bench_runs(100, || {
            for p in &rows {
                p.bytes.fetch_add(3 << 20, Relaxed);
            }
            now += 0.1;
            rt.update_if(|s, tx| poll(s, now, tx));
            rt.frame(0.1);
        });
        println!("List view tick: {best:.0} µs best, {median:.0} µs median of 9 runs of 100");
        drop(batch);
    }

    /// The hover highlight follows the pointer to the row under it and fades when it leaves the
    /// rows, without the view running for it.
    #[test]
    fn the_hover_highlight_follows_the_pointer() {
        let mut s = init();
        s.panes[0].set_entries(Ok(entries(0, 200)));
        let mut rt = rune::Runtime::new(s, ui::view, (1600.0, 1000.0), rune::Env::new(false, 1.0));
        for _ in 0..3 {
            rt.frame(1.0 / 60.0);
        }
        let mut rect = [0.0; 4];
        rt.update(|s, _| rect = s.views[0].rect);
        let pill = |rt: &rune::Runtime<State>| rt.elements().into_iter().find(|e| e.key.as_deref() == Some("hover-pill")).map(|e| (e.rect, e.opacity));
        rt.pointer_move(rect[0] + 60.0, rect[1] + lasso::HEADER_H + 5.5 * ROW_H);
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
        let (at, shown) = pill(&rt).expect("the hover highlight is there");
        assert!((at[1] - (rect[1] + lasso::HEADER_H + 5.0 * ROW_H)).abs() < 1.0, "on row 5: {at:?} in {rect:?}");
        assert!(shown > 0.99, "{shown}");
        rt.pointer_leave();
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
        assert!(pill(&rt).expect("still there").1 < 0.01);
    }

    /// List builds the columns in sight; scrolled sideways, the view builds the ones it reaches.
    #[test]
    fn list_builds_the_columns_it_scrolls_to() {
        let mut s = init();
        s.panes[0].set_entries(Ok(entries(0, 1999)));
        s.panes[0].view_mode = model::ViewMode::List;
        let mut rt = rune::Runtime::new(s, ui::view, (1600.0, 1000.0), rune::Env::new(false, 1.0));
        for _ in 0..3 {
            rt.frame(1.0 / 60.0);
        }
        let has = |rt: &rune::Runtime<State>, key: &str| rt.elements().iter().any(|e| e.key.as_deref() == Some(key));
        assert!(has(&rt, "lcol-0") && !has(&rt, "lcol-40"));
        rt.update(|_, tx| tx.scroll_to_instant(ui::list_key(0), 40.0 * 238.0, 0.0));
        for i in 0..6 {
            rt.update_if(|s, tx| poll(s, 1.0 + f64::from(i), tx));
            rt.frame(1.0 / 60.0);
        }
        assert!(has(&rt, "lcol-40") && !has(&rt, "lcol-0"));
    }

    /// What the pointer costs moving down a Details list with the hover animation on (the
    /// default): one row further each frame, as a quick sweep crosses them. Run it with
    /// `cargo test --release -p openexplorer -- --ignored --nocapture hover_sweep_cost`.
    #[test]
    #[ignore = "benchmark"]
    fn hover_sweep_cost() {
        let mut s = init();
        for p in 0..2 {
            s.panes[p].set_entries(Ok(entries(0, 2000)));
        }
        let mut rt = rune::Runtime::new(s, ui::view, (1600.0, 1000.0), rune::Env::new(false, 1.0));
        for _ in 0..3 {
            rt.frame(1.0 / 60.0);
        }
        let mut rect = [0.0; 4];
        rt.update(|s, _| rect = s.views[0].rect);
        assert!(rect[3] > 200.0, "the list was laid out: {rect:?}");
        let rows = ((rect[3] - lasso::HEADER_H) / ROW_H) as usize - 1;
        let mut i = 0;
        let (best, median) = bench_runs(300, || {
            i += 1;
            let y = rect[1] + lasso::HEADER_H + (i % rows) as f32 * ROW_H + ROW_H / 2.0;
            rt.pointer_move(rect[0] + 60.0, y);
            rt.frame(1.0 / 60.0);
        });
        println!("hover, one row a frame: {best:.0} µs best, {median:.0} µs median of 9 runs of 300");
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
            s.outbox.send(Msg::Listed { pane: 1, request: 7, part, result: Ok(entries(from, *n)), at: None }).unwrap();
            from += n;
        }
        poll(&mut s, 0.0, &mut Tx::default());
        assert_eq!(s.panes[1].entries.len(), 5720);
        assert!(!s.panes[1].loading);
    }

    /// A fresh listing's last page keeps where in the change log it's valid; a cached listing
    /// the log proved unchanged ends the loading without touching what the pane shows, and an
    /// answer for a folder left since is dropped.
    #[test]
    fn listings_keep_their_position_and_an_unchanged_one_just_stops_loading() {
        let mut s = init();
        s.cache = cache::Cache::memory_only();
        let loc = Location::Drive { email: "t@t".into(), space: Space::MyDrive, path: vec![("pasta".into(), "Pasta".into())] };
        let k = cache::key(&loc).unwrap();
        s.panes[1].go(loc);
        s.live[1].store(7, Ordering::Relaxed);
        let at = |token: &str| Some(cache::Pos { token: token.into(), fetched: 1 });
        s.outbox.send(Msg::Listed { pane: 1, request: 7, part: Part::Page { first: true, more: true }, result: Ok(entries(0, 1000)), at: None }).unwrap();
        s.outbox.send(Msg::Listed { pane: 1, request: 7, part: Part::Page { first: false, more: false }, result: Ok(entries(1000, 5)), at: at("t1") }).unwrap();
        poll(&mut s, 0.0, &mut Tx::default());
        assert_eq!(s.cache.get(&k).map(|l| (l.entries.len(), l.pos.clone())), Some((1005, at("t1"))));
        // Opened again: the cached listing shows, then the log proves it unchanged.
        s.panes[1].loading = true;
        s.live[1].store(8, Ordering::Relaxed);
        let listing = cache::Listing { pos: at("t2"), ..s.cache.get(&k).unwrap().clone() };
        s.outbox.send(Msg::Unchanged { pane: 1, request: 7, listing: listing.clone() }).unwrap();
        poll(&mut s, 0.0, &mut Tx::default());
        assert!(s.panes[1].loading, "an answer for an older request is dropped");
        s.outbox.send(Msg::Unchanged { pane: 1, request: 8, listing }).unwrap();
        poll(&mut s, 0.0, &mut Tx::default());
        assert!(!s.panes[1].loading);
        assert_eq!(s.panes[1].entries.len(), 1005);
        assert_eq!(s.cache.get(&k).unwrap().pos, at("t2"));
        // The app changed the folder itself: on reopening it's read whole.
        let here = s.panes[1].loc.clone();
        changed_here(&mut s, &[here]);
        assert_eq!(s.cache.get(&k).unwrap().pos, None);
    }

    /// Atualizar and every reload read a Drive folder whole; only opening it may skip that.
    #[test]
    fn only_opening_a_folder_may_keep_its_cached_listing() {
        let mut s = init();
        s.cache = cache::Cache::memory_only();
        let loc = Location::Drive { email: "t@t".into(), space: Space::MyDrive, path: vec![] };
        assert!(checks(&s, &loc, false).0.is_some());
        let (check, provable) = checks(&s, &loc, true);
        assert!(check.is_none() && provable.is_some(), "a refresh still records where its listing is valid");
        let shared = Location::Drive { email: "t@t".into(), space: Space::Shared { id: "0A".into(), name: "E".into() }, path: vec![] };
        assert!(checks(&s, &shared, false).0.is_none());
    }

    /// What a screen reader hears from the notice at app clock `now`.
    fn heard(s: &mut State, now: f64) -> Vec<(String, LiveRegion)> {
        let mut said = Vec::new();
        notice(s, now, &mut |text, how| said.push((text, how)));
        said
    }

    /// Each message the notice starts for is said once: an error at once, anything else when
    /// the reader is idle. Asked again while it shows, it isn't repeated; once it has gone, the
    /// same words are said again.
    #[test]
    fn each_notice_is_announced_once() {
        let mut s = init();
        s.cache = cache::Cache::memory_only();
        let polite = |m: &str| vec![(m.to_string(), LiveRegion::Polite)];
        s.status = "Copiado: 3 itens.".into();
        assert_eq!(heard(&mut s, 0.0), polite("Copiado: 3 itens."));
        assert_eq!(heard(&mut s, 0.1), []);
        s.status = "Copiado: 3 itens.".into();
        assert_eq!(heard(&mut s, 1.0), []);
        // An operation that failed interrupts; the message after it doesn't.
        // (`Msg::Done` does this with its error; sent through `poll` it would write the log.)
        fail(&mut s, "Sem permissão para excluir.".into());
        assert_eq!(heard(&mut s, 1.6), [("Sem permissão para excluir.".to_string(), LiveRegion::Assertive)]);
        s.status = "Movido: 1 item.".into();
        assert_eq!(heard(&mut s, 2.0), polite("Movido: 1 item."));
        // Gone after its time, then said again.
        assert_eq!(heard(&mut s, 2.0 + NOTICE_SECS), []);
        assert!(s.status.is_empty());
        s.status = "Movido: 1 item.".into();
        assert_eq!(heard(&mut s, 10.0), polite("Movido: 1 item."));
    }

    /// Shift+F10 and the Menu key open the list's context menu: on the folder at the top of
    /// the rows with nothing selected, else under the cursor's row. F10 alone, or with Ctrl,
    /// doesn't.
    #[test]
    fn the_keyboard_opens_the_context_menu() {
        let mut s = init();
        s.cache = cache::Cache::memory_only();
        s.panes[1].set_entries(Ok(entries(0, 50)));
        s.panes[1].selected.clear();
        let h = lasso::HEADER_H + 10.0 * ROW_H;
        s.views[1].rect = [400.0, 100.0, 600.0, h];
        s.views[1].scroll.set(lasso::Scroll { x: 0.0, y: 0.0, max_x: 0.0, max_y: 40.0 * ROW_H + 2.0 * ROW_H, h });
        let mut tx = Tx::default();
        let ev = |key, mods| KeyEvent { key, mods };
        let shift = Mods { shift: true, ..Mods::default() };
        assert!(!key(&mut s, 1, &ev(Key::Function(10), Mods::default()), &mut tx));
        assert!(!key(&mut s, 1, &ev(Key::ContextMenu, Mods { cmd: true, ..Mods::default() }), &mut tx));
        assert!(s.menu.is_none());
        let top = 100.0 + lasso::HEADER_H;
        s.active = 0;
        assert!(key(&mut s, 1, &ev(Key::Function(10), shift), &mut tx));
        assert_eq!(s.menu, Some((1, rune::PointerPress { x: 424.0, y: top })));
        assert_eq!(s.active, 1);
        s.menu = None;
        click(&mut s, 1, "f2", Mods::default());
        click(&mut s, 1, "f4", Mods { cmd: true, ..Mods::default() });
        assert!(key(&mut s, 1, &ev(Key::ContextMenu, Mods::default()), &mut tx));
        assert_eq!(s.menu, Some((1, rune::PointerPress { x: 424.0, y: top + 5.0 * ROW_H })));
        // The cursor left the selection: the menu goes under the first selected row.
        s.panes[1].cursor = Some("f9".into());
        assert!(key(&mut s, 1, &ev(Key::ContextMenu, Mods::default()), &mut tx));
        assert_eq!(s.menu, Some((1, rune::PointerPress { x: 424.0, y: top + 3.0 * ROW_H })));
    }

    #[test]
    fn finished_downloads_stay_until_cleared_unless_auto_clear_is_on() {
        let mut s = init();
        s.settings = settings::Settings::default();
        assert!(!s.settings.clear_completed);

        let end = |name: &str| transfers::End::local(std::path::Path::new(name));
        let batch = transfers::queue(
            &mut s.transfers,
            None,
            transfers::Op::Download,
            [(end(r"C:\a.zip"), end(r"D:\a.zip")), (end(r"C:\b.zip"), end(r"D:\b.zip"))],
        );
        s.transfers[0].progress.begin();
        s.transfers[0].progress.end(Ok(()));
        s.transfers[1].progress.begin();
        s.transfers[1].progress.end(Err("falha".into()));

        // By default, completed transfers stay in the queue across `poll` and show the clear button.
        let mut tx = Tx::default();
        poll(&mut s, 0.1, &mut tx);
        assert_eq!(s.transfers.len(), 2);

        let mut rt = rune::Runtime::new(s, ui::view, (1280.0, 800.0), rune::Env::new(false, 1.0));
        rt.frame(1.0 / 60.0);
        assert!(rt.elements().into_iter().any(|e| e.key.as_deref() == Some("queue-clear")));

        // Manual clear removes finished rows from the queue.
        rt.update(|s, _| {
            actions::clear_transfers(s);
            assert!(s.transfers.is_empty());

            // With `clear_completed` enabled, `poll` removes Done rows and keeps Failed ones.
            let batch2 = transfers::queue(
                &mut s.transfers,
                None,
                transfers::Op::Download,
                [(end(r"C:\c.zip"), end(r"D:\c.zip")), (end(r"C:\d.zip"), end(r"D:\d.zip"))],
            );
            s.transfers[0].progress.begin();
            s.transfers[0].progress.end(Ok(()));
            s.transfers[1].progress.begin();
            s.transfers[1].progress.end(Err("falha".into()));
            s.settings.clear_completed = true;
            poll(s, 0.2, &mut Tx::default());
            assert_eq!(s.transfers.len(), 1);
            assert_eq!(s.transfers[0].progress.stage(), ops::Stage::Failed);
            drop(batch2);
        });

        // The auto-clear option lives in Options -> Transfers, not General.
        rt.update(|s, _| {
            s.dialog = Some(actions::Dialog::Options(Box::new(settings::OptionsView::open(settings::OptionsTab::General))));
        });
        rt.frame(1.0 / 60.0);
        assert!(!rt.elements().into_iter().any(|e| e.key.as_deref() == Some("opt-clear-done")));

        rt.update(|s, _| {
            s.dialog = Some(actions::Dialog::Options(Box::new(settings::OptionsView::open(settings::OptionsTab::Transfers))));
        });
        rt.frame(1.0 / 60.0);
        assert!(rt.elements().into_iter().any(|e| e.key.as_deref() == Some("opt-clear-done")));

        drop(batch);
    }
}
