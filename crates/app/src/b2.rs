//! Backblaze B2 keys in the panes. Each key shows in the tree under Contas, beside the Google
//! Drive and MEGA accounts and the servers, and opens like a server (see `servers.rs`): its top
//! lists the buckets the key reaches. Copying to and from anywhere, moving, renaming, new
//! folders, searching and dragging go through the servers' code, with the key's account as the
//! connection (`openexplorer_remote`'s `b2.rs`). What only B2 has lives here: adding a key,
//! listings shown from the cache at once (`openexplorer_b2::cache`), deleting by hiding (with
//! Desfazer) or every version for good, and `b2://bucket/…` addresses.

use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use openexplorer_b2 as b2;
use openexplorer_i18n::{t, tr};
use openexplorer_remote::{Protocol, Server};
use rune::{El, ScrollSpec, Tx, col, row, text};
use rune_kit::{Button, ButtonVariant, Dialog as Modal, Kit};

use crate::actions::{Dialog, selected};
use crate::model::{Entry, Location};
use crate::servers::{Address, Form, folder, path_of};
use crate::undo::{Rec, Step};
use crate::{Msg, Part, State, background, catch_panic};

/// The keys' list, beside the app's other settings.
pub fn store() -> b2::Store {
    b2::Store::at(openexplorer_gdrive::config_dir().join("b2-accounts.json"))
}

/// The servers the connected keys show as.
pub fn servers(store: &b2::Store) -> Vec<Server> {
    store.accounts().into_iter().map(|a| Server::b2(&a.key_id, &a.name)).collect()
}

/// Which file on this computer each large upload sends (see `openexplorer_b2::notes`), beside
/// the keys' list.
pub fn notes_file() -> std::path::PathBuf {
    openexplorer_gdrive::config_dir().join("b2-uploads.json")
}

/// Where the listings are kept, sealed (see `openexplorer_b2::cache`).
pub fn cache_dir() -> std::path::PathBuf {
    openexplorer_gdrive::local_dir().join("b2")
}

/// The B2 key a location is in, and the path below its top (the bucket first).
pub fn at(loc: &Location) -> Option<(&Server, &[String])> {
    match loc {
        Location::Server { server, path, .. } if server.is_b2() => Some((server, path)),
        _ => None,
    }
}

/// The top of a key, where only buckets are: nothing new goes there. `None` elsewhere.
pub fn why_not(loc: &Location) -> Option<String> {
    match at(loc) {
        Some((_, [])) => Some(t("O topo da conta B2 só lista os buckets: abra um deles. Buckets são criados no site da Backblaze.", "The B2 account's top only lists the buckets: open one of them. Buckets are made on the Backblaze website.").into()),
        _ => None,
    }
}

/// Whether the items `loc` lists can't be renamed, cut or deleted: the buckets.
pub fn items_fixed(loc: &Location) -> bool {
    matches!(at(loc), Some((_, [])))
}

/// The key's top, where a new key opens.
pub fn top(server: &Server) -> Location {
    folder(server, Vec::new())
}

/// The panes' places on key `id`, reloaded after a change there.
fn panes_on(s: &State, id: &str) -> Vec<Location> {
    s.panes.iter().map(|p| p.loc.clone()).filter(|l| matches!(l, Location::Server { server, .. } if server.id == id)).collect()
}

fn entry(i: b2::Item) -> Entry {
    let modified = i.modified.filter(|t| *t > 0).map(|t| SystemTime::UNIX_EPOCH + Duration::from_millis(t as u64));
    crate::servers::entry(openexplorer_remote::Item { name: i.name, is_dir: i.is_dir, size: i.size, modified, link: false })
}

// ---------------------------------------------------------------------------------------------
// Listing

fn entries(items: Vec<b2::Item>, show_hidden: bool) -> Vec<Entry> {
    items.into_iter().filter(|i| show_hidden || !i.name.starts_with('.')).map(entry).collect()
}

/// The bucket and the folder prefix (`a/b/`, `""` at the bucket's top) of a B2 path; `None` at
/// the key's top, the list of buckets.
fn bucket_of(dir: &str) -> Option<(&str, String)> {
    let rel = dir.trim_matches('/');
    match rel.split_once('/') {
        Some((b, p)) => Some((b, format!("{p}/"))),
        None if rel.is_empty() => None,
        None => Some((rel, String::new())),
    }
}

/// Gives the folders (or the buckets) among `entries`, listed at `dir`, the size the key summed
/// for them (see `openexplorer_b2::sizes`): the bytes of the current files under each. Those not
/// summed yet keep none.
fn fill(entries: &mut [Entry], account: &b2::Account, dir: &str) {
    let at = bucket_of(dir);
    for e in entries.iter_mut().filter(|e| e.is_dir) {
        let size = match &at {
            None => account.kept_sizes(&e.name).map(|s| s.current),
            Some((b, prefix)) => account.kept_sizes(b).and_then(|s| s.of(&format!("{prefix}{}/", e.name))).map(|(current, _)| current),
        };
        if size.is_some() {
            e.size = size;
        }
    }
}

/// Sums the sizes of the buckets `dir` shows or is in (one listing of every version each, kept
/// a while; see `openexplorer_b2::sizes`), then has the panes on the key show them.
fn sum_sizes(account: &b2::Account, dir: &str, emit: &dyn Fn(Msg)) {
    let buckets: Vec<String> = match bucket_of(dir) {
        Some((b, _)) => vec![b.to_string()],
        None => account.known_buckets().into_iter().map(|b| b.name).collect(),
    };
    let mut any = false;
    for b in buckets {
        match catch_panic(t("somar o tamanho do bucket", "add up the bucket's size"), || account.sizes(&b)) {
            Ok(_) => any = true,
            Err(e) => openexplorer_gdrive::log(&format!("B2: tamanho de {b}: {e}")),
        }
    }
    if any {
        emit(Msg::B2(Answer::Sized { key_id: account.key_id.clone() }));
    }
}

/// The sums of key `key_id` arrived: its panes' folders show their sizes.
fn sized(s: &mut State, key_id: &str) {
    let Some(account) = b2::cached(key_id) else { return };
    for p in 0..s.panes.len() {
        let Location::Server { server, path, find: None } = &s.panes[p].loc else { continue };
        if !server.is_b2() || server.user != key_id || s.panes[p].loading {
            continue;
        }
        let dir = server.path(path);
        let mut entries = s.panes[p].entries.clone();
        fill(&mut entries, &account, &dir);
        if entries != s.panes[p].entries {
            s.panes[p].set_entries(Ok(entries));
        }
    }
}

/// What the status bar shows inside a B2 bucket: what its current files take, and in the tip
/// what every version kept takes (what the website shows and Backblaze bills).
pub fn quota_bar(loc: &Location) -> Option<(String, Option<f64>, String)> {
    let Location::Server { server, path, .. } = loc else { return None };
    let dir = server.path(path);
    let (bucket, _) = bucket_of(&dir)?;
    let sums = b2::cached(&server.user)?.kept_sizes(bucket)?;
    let label = tr!("{} no bucket", "{} in the bucket", crate::model::format_size(sums.current));
    let help = tr!(
        "{bucket}: {} arquivos, {} nos arquivos atuais; {} guardados contando as versões escondidas (o que a Backblaze cobra).",
        "{bucket}: {} files, {} in the current files; {} stored counting hidden versions (what Backblaze bills).",
        openexplorer_i18n::grouped(sums.files),
        crate::model::format_size(sums.current),
        crate::model::format_size(sums.stored)
    );
    Some((label, None, help))
}

/// Lists pane `p`'s B2 folder on a thread. A listing kept from before shows at once, and B2 is
/// always asked again behind it (B2 can't say what changed, and listing is free; see
/// `openexplorer_b2::watch`): the pane changes only if B2's answer differs, so nothing blinks
/// and the selection and the scroll stay. `_fresh` (Atualizar) changes nothing here: every
/// opening reads. A search in a bucket reads its whole tree at once ([`search`]); at the key's
/// top it goes the servers' way.
pub fn load(s: &mut State, p: usize, request: u64, _fresh: bool) {
    let Location::Server { server, path, find } = s.panes[p].loc.clone() else { return };
    if let Some(text) = find {
        if path.is_empty() {
            return crate::servers::load(s, p, request);
        }
        return search(s, p, request, server.path(&path), server.user.clone(), text);
    }
    // The pane's next reading on the app's own comes a full turn after this one.
    s.servers.b2_watch.panes[p] = None;
    let show_hidden = s.settings.show_hidden;
    let pane = &mut s.panes[p];
    pane.loading = true;
    pane.error = None;
    pane.free = None;
    background(s, move |emit| {
        let listed = |part, result| Msg::Listed { pane: p, request, part, result, at: None };
        let dir = server.path(&path);
        let account = match b2::reach(&server.user) {
            Ok(a) => a,
            Err(e) => return emit(listed(Part::WHOLE, Err(e))),
        };
        // Once the folder shows, the folders inside it are read ahead: expanding one in the tree
        // or opening it is then immediate.
        let ahead = || {
            let _ = catch_panic(t("ler as pastas adiante", "read folders ahead"), || {
                account.prefetch(&dir);
                Ok(())
            });
        };
        let sized = |items: Vec<b2::Item>| {
            let mut e = entries(items, show_hidden);
            fill(&mut e, &account, &dir);
            e
        };
        let Some(kept) = account.cached(&dir) else {
            let result = catch_panic(t("listar", "list"), || account.list_fresh(&dir).map(sized));
            let ok = result.is_ok();
            emit(listed(Part::WHOLE, result));
            if ok {
                ahead();
                sum_sizes(&account, &dir, &emit);
            }
            return;
        };
        emit(listed(Part::Cached, Ok(sized(kept.items.clone()))));
        drop(kept);
        match catch_panic(t("listar", "list"), || account.reread(&dir).map(Some)) {
            Ok(Some(b2::Fresh::Changed(items))) => emit(listed(Part::WHOLE, Ok(sized(items)))),
            Ok(_) => emit(Msg::B2(Answer::Same { pane: p, request })),
            // The kept listing stays on screen; the error is said.
            Err(e) => emit(listed(Part::Page { first: false, more: false }, Err(e))),
        }
        ahead();
        sum_sizes(&account, &dir, &emit);
    });
}

/// Searches the B2 folder `dir` of key `key_id` and every folder below it for names holding all
/// of `text`'s words: one walk of every name under it (10,000 a request), not a listing a folder
/// as on other servers. Folders are found by the names under them. Each result's id is its path
/// from `dir`, as the servers' search gives it; results come in pages as they're found.
fn search(s: &mut State, p: usize, request: u64, dir: String, key_id: String, text: String) {
    let show_hidden = s.settings.show_hidden;
    let live = s.live[p].clone();
    let pane = &mut s.panes[p];
    pane.loading = true;
    pane.error = None;
    pane.free = None;
    background(s, move |emit| {
        let alive = || live.load(Ordering::Relaxed) == request;
        let mut first = true;
        let mut page = |entries: Vec<Entry>, more: bool| {
            emit(Msg::Listed { pane: p, request, part: Part::Page { first, more }, result: Ok(entries), at: None });
            first = false;
        };
        let result = catch_panic(t("pesquisar", "search"), || {
            let account = b2::reach(&key_id)?;
            let words = crate::finder::words(&text);
            let mut pager = crate::finder::Pager::new(&mut page);
            let mut found = Found { words, show_hidden, folders: Default::default(), buf: Vec::new() };
            account.walk(&dir, &mut |files| {
                found.page(files, &mut |e| pager.push(e));
                alive() && !pager.full()
            })?;
            pager.flush(false);
            Ok(())
        });
        if let Err(e) = result {
            emit(Msg::Listed { pane: p, request, part: Part::Page { first, more: false }, result: Err(e), at: None });
        }
    });
}

/// What a search found so far: the words, and the folders already given.
struct Found {
    words: Vec<Vec<u8>>,
    show_hidden: bool,
    folders: std::collections::HashSet<String>,
    buf: Vec<u8>,
}

impl Found {
    /// The results among a page of files (paths below the folder searched): the folders on
    /// their way, once each, and the files.
    fn page(&mut self, files: Vec<(String, b2::Item)>, out: &mut dyn FnMut(Entry)) {
        for (rel, item) in files {
            let parts: Vec<&str> = rel.split('/').collect();
            // A name B2 holds but no folder can show ("a//b").
            if parts.iter().any(|c| c.is_empty()) {
                continue;
            }
            let (name, above) = parts.split_last().expect("one part at least");
            // A hidden folder hides what's in it.
            let mut hidden = false;
            for (i, folder) in above.iter().enumerate() {
                hidden |= !self.show_hidden && folder.starts_with('.');
                if hidden {
                    break;
                }
                let id = parts[..=i].join("/");
                if self.folders.insert(id.clone()) && crate::finder::matches_in(&self.words, folder, &mut self.buf) {
                    out(Entry { id, ..entry(b2::Item { name: folder.to_string(), is_dir: true, size: None, modified: None, id: None, info: None }) });
                }
            }
            let shown = !hidden && *name != b2::account::EMPTY_MARKER && (self.show_hidden || !name.starts_with('.'));
            if shown && crate::finder::matches_in(&self.words, name, &mut self.buf) {
                out(Entry { id: rel.clone(), ..entry(item) });
            }
        }
    }
}

/// B2 listed the folder just as the pane shows it: it only stops loading.
fn same(s: &mut State, pane: usize, request: u64) {
    if s.live[pane].load(Ordering::Relaxed) != request {
        return;
    }
    let p = &mut s.panes[pane];
    p.loading = false;
    p.prune_selection();
    crate::drop_stale_rename(s, pane);
}

// ---------------------------------------------------------------------------------------------
// Changes made elsewhere

/// How long after a failed reading the next one comes, in seconds.
const RETRY: f64 = 120.0;

/// The panes' B2 folders, read again on the app's own while the window has the focus, so
/// what changes elsewhere shows (see `openexplorer_b2::watch` for why and how often at most).
#[derive(Default)]
pub struct Watch {
    /// The window lost the focus: nothing is read until it's back.
    away: bool,
    panes: [Option<Turn>; crate::layout::MAX_PANES],
}

struct Turn {
    loc: Location,
    /// When it's read next (app clock, seconds).
    next: f64,
    busy: bool,
}

/// The window gained or lost the focus. Back in front, a folder whose turn came meanwhile is
/// read at once.
pub fn window_focus(s: &mut State, focused: bool, tx: &mut Tx) {
    s.servers.b2_watch.away = !focused;
    // While away no wake was asked for: the next frame looks at the turns.
    if focused {
        tx.wake_after(Duration::ZERO);
    }
}

/// How long until a folder of `items` names is read again.
fn every(items: usize) -> f64 {
    b2::watch::every(items).as_secs_f64()
}

/// Reads again each pane's B2 folder whose turn came.
pub fn tick(s: &mut State, now: f64) {
    // A pane the layout hides isn't read: it is once it shows again.
    let visible = s.settings.pane_layout.count();
    for p in 0..crate::layout::MAX_PANES {
        let shows = p < visible && matches!(&s.panes[p].loc, Location::Server { server, find: None, .. } if server.is_b2());
        if !shows {
            s.servers.b2_watch.panes[p] = None;
            continue;
        }
        let (loc, items, loading) = (&s.panes[p].loc, s.panes[p].entries.len(), s.panes[p].loading);
        let w = &mut s.servers.b2_watch;
        let turn = match &mut w.panes[p] {
            Some(t) if t.loc == *loc => t,
            slot => slot.insert(Turn { loc: loc.clone(), next: now + every(items), busy: false }),
        };
        if w.away || turn.busy || loading || now < turn.next {
            continue;
        }
        turn.busy = true;
        poll(s, p);
    }
}

/// When the next reading is due, for the frame loop to wake then.
pub fn next_ask(s: &State) -> Option<f64> {
    let w = &s.servers.b2_watch;
    if w.away {
        return None;
    }
    w.panes.iter().flatten().filter(|t| !t.busy).map(|t| t.next).reduce(f64::min)
}

/// Reads pane `p`'s folder again on a thread, without touching the pane until B2's answer
/// differs from what it shows.
fn poll(s: &State, p: usize) {
    let request = s.live[p].load(Ordering::Relaxed);
    let loc = s.panes[p].loc.clone();
    let Location::Server { server, path, .. } = &loc else { return };
    let (key_id, dir) = (server.user.clone(), server.path(path));
    background(s, move |emit| {
        // Only a key already open: opening one is the listing's job.
        let result = match b2::cached(&key_id) {
            Some(a) => catch_panic(t("ver o que mudou no B2", "see what changed on B2"), || Ok(a.poll(&dir))),
            None => Ok(b2::Polled::Same),
        };
        emit(Msg::B2(Answer::Polled { pane: p, request, loc: Box::new(loc), result }));
    });
}

fn polled(s: &mut State, pane: usize, request: u64, loc: Location, result: Result<b2::Polled, String>) {
    let now = s.now;
    let items = s.panes[pane].entries.len();
    let wait = match &result {
        Ok(b2::Polled::Same | b2::Polled::Changed(_)) => every(items),
        Ok(b2::Polled::Wait(secs)) => (*secs as f64).max(1.0),
        Ok(b2::Polled::Stopped(_) | b2::Polled::Failed(_)) | Err(_) => RETRY,
    };
    match &result {
        Ok(b2::Polled::Wait(secs)) => openexplorer_gdrive::log(&format!("B2: releitura de {} adiada {secs} s", loc_text(&loc))),
        Ok(b2::Polled::Stopped(e) | b2::Polled::Failed(e)) | Err(e) => openexplorer_gdrive::log(&format!("B2: releitura de {}: {e}", loc_text(&loc))),
        _ => {}
    }
    if let Some(t) = &mut s.servers.b2_watch.panes[pane]
        && t.loc == loc
    {
        t.busy = false;
        t.next = now + wait;
    }
    let Ok(b2::Polled::Changed(items)) = result else { return };
    // The pane moved on, or is loading anew, meanwhile: what it shows next comes from that.
    if s.live[pane].load(Ordering::Relaxed) != request || s.panes[pane].loc != loc || s.panes[pane].loading {
        return;
    }
    // The selection, the cursor and the scroll stay: entries are matched by name.
    let show_hidden = s.settings.show_hidden;
    let mut shown = entries(items, show_hidden);
    if let (Some(account), Location::Server { server, path, .. }) = (b2::cached(&server_user(&loc)), &loc) {
        fill(&mut shown, &account, &server.path(path));
    }
    s.panes[pane].set_entries(Ok(shown));
    crate::drop_stale_rename(s, pane);
}

fn server_user(loc: &Location) -> String {
    match loc {
        Location::Server { server, .. } => server.user.clone(),
        _ => String::new(),
    }
}

fn loc_text(loc: &Location) -> String {
    match at(loc) {
        Some((_, path)) => address_text(path),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// Adding and removing a key

/// What background work on B2 brought back.
pub enum Answer {
    /// The Adicionar conta dialog's check `attempt` ended: the key ID, its name, what it can't do.
    Connected { attempt: u64, result: Result<(String, String, Vec<&'static str>), String> },
    /// Listing `request` of pane `pane` found the folder as kept, which the pane shows already.
    Same { pane: usize, request: u64 },
    /// A reading on the app's own of what pane `pane` showed at `request`, at `loc`.
    Polled { pane: usize, request: u64, loc: Box<Location>, result: Result<b2::Polled, String> },
    /// The unfinished uploads of `bucket` on key `key_id`.
    Unfinished { key_id: String, bucket: String, result: Result<Vec<b2::Unfinished>, String> },
    /// Cancelling some of them ended: how many went.
    Cancelled { key_id: String, bucket: String, result: Result<usize, String> },
    /// Key `key_id` summed the sizes of buckets its panes show.
    Sized { key_id: String },
}

pub fn arrived(s: &mut State, a: Answer) {
    match a {
        Answer::Connected { attempt, result } => connected(s, attempt, result),
        Answer::Same { pane, request } => same(s, pane, request),
        Answer::Polled { pane, request, loc, result } => polled(s, pane, request, *loc, result),
        Answer::Unfinished { key_id, bucket, result } => unfinished_listed(s, &key_id, &bucket, result),
        Answer::Cancelled { key_id, bucket, result } => uploads_cancelled(s, &key_id, &bucket, result),
        Answer::Sized { key_id } => sized(s, &key_id),
    }
}

/// What the app does with each capability a key may lack, in the user's words.
fn missing(allowed: &b2::Allowed) -> Vec<&'static str> {
    let wants: [(&str, &'static str); 4] = [
        ("listFiles", t("listar arquivos (listFiles)", "list files (listFiles)")),
        ("readFiles", t("baixar (readFiles)", "download (readFiles)")),
        ("writeFiles", t("enviar, criar pastas, renomear e excluir (writeFiles)", "upload, make folders, rename and delete (writeFiles)")),
        ("deleteFiles", t("excluir todas as versões (deleteFiles)", "delete every version (deleteFiles)")),
    ];
    wants.into_iter().filter(|(cap, _)| !allowed.can(cap)).map(|(_, words)| words).collect()
}

/// Conectar in the Adicionar conta dialog with B2 picked: authorizes the key on a thread.
pub fn submit(s: &mut State) {
    let Some(f) = &mut s.account_form else { return };
    if f.checking.is_some() {
        return;
    }
    let key_id = f.user.text.trim().to_string();
    let key = f.password.text.trim().to_string();
    if key_id.is_empty() {
        f.error = Some(t("Digite o ID da chave (keyID).", "Enter the key ID (keyID).").into());
        return;
    }
    if key.is_empty() {
        f.error = Some(t("Digite a chave de aplicação (applicationKey).", "Enter the application key (applicationKey).").into());
        return;
    }
    s.servers.attempts += 1;
    let attempt = s.servers.attempts;
    f.checking = Some(attempt);
    f.error = None;
    background(s, move |emit| {
        let result = catch_panic(t("conectar ao B2", "connect to B2"), || {
            let a = b2::connect(&key_id, &key)?;
            let lacks = a.api().known_auth().map(|auth| missing(&auth.allowed)).unwrap_or_default();
            Ok((a.key_id.clone(), a.label()?, lacks))
        });
        emit(Msg::B2(Answer::Connected { attempt, result }));
    });
}

fn connected(s: &mut State, attempt: u64, result: Result<(String, String, Vec<&'static str>), String>) {
    let Some(f) = &mut s.account_form else { return };
    if f.checking != Some(attempt) {
        return;
    }
    f.checking = None;
    let (key_id, name, lacks) = match result {
        Ok(r) => r,
        Err(e) => {
            openexplorer_gdrive::log(&format!("falha ao conectar B2: {e}"));
            f.error = Some(e);
            s.notify_account();
            return;
        }
    };
    if let Err(e) = s.servers.b2.add(&key_id, &name) {
        f.error = Some(e);
        s.notify_account();
        return;
    }
    openexplorer_gdrive::log(&format!("chave B2 conectada: {name}"));
    s.close_account_form();
    let server = Server::b2(&key_id, &name);
    s.servers.list.retain(|sv| sv.id != server.id);
    s.servers.list.push(server.clone());
    s.servers.pool.forget(&server.id);
    s.tree.server_changed(&server.id);
    s.tree.server_opened(&server.id);
    let p = s.active;
    crate::navigate(s, p, top(&server));
    s.status = if lacks.is_empty() {
        tr!("Conta B2 conectada: {name}", "B2 account connected: {name}")
    } else {
        tr!("Conta B2 conectada: {name}. Esta chave não permite: {}.", "B2 account connected: {name}. This key doesn't allow: {}.", lacks.join("; "))
    };
}

/// Remover conta: the application key leaves the credential store and its listings the disk.
/// Whether the key left the list.
pub fn remove(s: &mut State, server: &Server) -> bool {
    if let Err(e) = s.servers.b2.remove(&server.user) {
        crate::fail(s, e);
        return false;
    }
    b2::disconnect(&server.user);
    true
}

// ---------------------------------------------------------------------------------------------
// Deleting

/// Runs `work` with the key's account on a thread, recording what Desfazer takes back.
fn job(s: &State, server: &Server, reload: Vec<Location>, what: impl FnOnce(usize) -> String + Send + 'static, work: impl FnOnce(&b2::Account, &Rec) -> Result<String, String> + Send + 'static) {
    let key_id = server.user.clone();
    background(s, move |emit| {
        let rec = Rec::default();
        let result = catch_panic(t("operar no B2", "work on B2"), || b2::reach(&key_id).and_then(|a| work(&a, &rec)));
        emit(crate::undo::did(&rec, what, reload, result));
    });
}

/// How the bucket of `loc` keeps what's deleted, in words for the Excluir dialog.
pub fn delete_note(loc: &Location) -> String {
    let kept = at(loc).and_then(|(server, path)| b2::cached(&server.user).and_then(|a| a.hidden_kept(&server.path(path))));
    match kept {
        Some(Some(days)) => tr!(
            "Os itens ficam escondidos no B2 e as regras deste bucket os apagam de vez em {days} dia(s); até lá, Desfazer os traz de volta. Para apagar já, use Excluir todas as versões.",
            "The items are hidden on B2 and this bucket's rules delete them for good in {days} day(s); until then, Undo brings them back. To delete them now, use Delete every version."
        ),
        Some(None) => t(
            "Os itens ficam escondidos no B2. Este bucket guarda as versões antigas para sempre, então elas continuam ocupando (e custando) espaço; Desfazer os traz de volta. Para liberar o espaço, use Excluir todas as versões.",
            "The items are hidden on B2. This bucket keeps old versions forever, so they still take (and cost) space; Undo brings them back. To free the space, use Delete every version.",
        )
        .into(),
        None => t(
            "Os itens ficam escondidos no B2, como versões antigas, pelo tempo que as regras do bucket mandarem; Desfazer os traz de volta.",
            "The items are hidden on B2, as old versions, for as long as the bucket's rules say; Undo brings them back.",
        )
        .into(),
    }
}

/// Excluir: each item is hidden (see `openexplorer_b2`'s docs), to be brought back by Desfazer.
pub fn hide(s: &mut State, loc: &Location, items: Vec<Entry>) {
    let Some((server, _)) = at(loc) else { return };
    let server = server.clone();
    let reload = panes_on(s, &server.id);
    let paths: Vec<(String, bool)> = items.iter().filter_map(|e| path_of(loc, e).map(|p| (p, e.is_dir))).collect();
    let at_server = server.clone();
    job(s, &server, reload, |n| tr!("excluir {} do B2", "delete {} from B2", crate::actions::count(n)), move |a, rec| {
        hide_each(&paths, |path, is_dir| a.hide(path, is_dir), |path, is_dir, since| rec.add([Step::B2Unhide { server: at_server.clone(), path: path.to_string(), is_dir, since }]))
    });
}

/// Hides each of `paths` in turn, `record`ing how to bring back what each hid, the part of a
/// folder hidden before an error included: what's gone from the pane always has its Desfazer.
fn hide_each(paths: &[(String, bool)], hide: impl Fn(&str, bool) -> Result<i64, b2::HideStopped>, mut record: impl FnMut(&str, bool, i64)) -> Result<String, String> {
    for (i, (path, is_dir)) in paths.iter().enumerate() {
        match hide(path, *is_dir) {
            Ok(since) => record(path, *is_dir, since),
            Err(stopped) => {
                let e = stopped.error;
                let e = if i == 0 { e } else { tr!("{e} (já excluídos: {i})", "{e} (already deleted: {i})") };
                if stopped.since.is_none() && i == 0 {
                    return Err(e);
                }
                if let Some(since) = stopped.since {
                    record(path, *is_dir, since);
                }
                return Err(tr!("{e} — Desfazer traz de volta o que já foi excluído.", "{e} — Undo brings back what was already deleted."));
            }
        }
    }
    Ok(tr!("Excluído do B2 (escondido): {}.", "Deleted from B2 (hidden): {}.", crate::actions::count(paths.len())))
}

/// Excluir todas as versões, from the menu: asks first.
pub fn ask_purge(s: &mut State, p: usize, _: &mut Tx) {
    let items = selected(s, p);
    if !items.is_empty() && at(&s.panes[p].loc).is_some() && !items_fixed(&s.panes[p].loc) {
        s.dialog = Some(Dialog::Purge { loc: s.panes[p].loc.clone(), items });
    }
}

pub fn purge_note() -> &'static str {
    t(
        "Todas as versões destes itens serão apagadas para sempre do B2, inclusive as escondidas e os envios inacabados. Isso não pode ser desfeito.",
        "Every version of these items will be deleted from B2 for good, hidden ones and unfinished uploads included. This can't be undone.",
    )
}

/// Excluir todas as versões, confirmed.
pub fn purge(s: &mut State, loc: &Location, items: Vec<Entry>) {
    let Some((server, _)) = at(loc) else { return };
    let server = server.clone();
    let reload = panes_on(s, &server.id);
    let paths: Vec<(String, bool)> = items.iter().filter_map(|e| path_of(loc, e).map(|p| (p, e.is_dir))).collect();
    let key_id = server.user.clone();
    background(s, move |emit| {
        let result = catch_panic(t("excluir do B2", "delete from B2"), || {
            let a = b2::reach(&key_id)?;
            let mut versions = 0;
            for (i, (path, is_dir)) in paths.iter().enumerate() {
                versions += a.purge(path, *is_dir).map_err(|e| if i == 0 { e } else { tr!("{e} (já excluídos para sempre: {i})", "{e} (already deleted for good: {i})") })?;
            }
            Ok(tr!("Excluído para sempre do B2: {} ({versions} versões).", "Deleted from B2 for good: {} ({versions} versions).", crate::actions::count(paths.len())))
        });
        emit(Msg::Done { reload, result });
    });
}

// ---------------------------------------------------------------------------------------------
// Desfazer

fn short(path: &str) -> String {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path).to_string()
}

/// Whether undoing (or redoing) `step` can go ahead: what it brings back is still hidden, or
/// what it hides is still there.
pub fn check(step: &Step) -> Result<(), String> {
    match step {
        Step::B2Unhide { server, path, is_dir, since } => {
            if b2::reach(&server.user)?.hidden_since(path, *is_dir, *since)?.is_empty() {
                return Err(tr!("“{}” não está mais escondido no B2 (foi enviado de novo ou apagado de vez).", "“{}” is no longer hidden on B2 (it was uploaded again or deleted for good).", short(path)));
            }
            Ok(())
        }
        Step::B2Hide { server, path, .. } => match b2::reach(&server.user)?.stat(path)? {
            Some(_) => Ok(()),
            None => Err(tr!("“{}” não está mais no B2.", "“{}” is no longer on B2.", short(path))),
        },
        _ => Ok(()),
    }
}

/// Makes `step`'s change, giving the step that takes it back.
pub fn run(step: &Step) -> Result<Step, String> {
    match step {
        Step::B2Unhide { server, path, is_dir, since } => {
            b2::reach(&server.user)?.unhide(path, *is_dir, *since)?;
            Ok(Step::B2Hide { server: server.clone(), path: path.clone(), is_dir: *is_dir })
        }
        Step::B2Hide { server, path, is_dir } => {
            let a = b2::reach(&server.user)?;
            match a.hide(path, *is_dir) {
                Ok(since) => Ok(Step::B2Unhide { server: server.clone(), path: path.clone(), is_dir: *is_dir, since }),
                // The step stays to be tried again whole, and its Desfazer brings back from one
                // time on: what this try hid comes back first, or it would be left out of it.
                Err(stopped) => match stopped.since.map(|since| a.unhide(path, *is_dir, since)) {
                    Some(Err(back)) => Err(tr!(
                        "{} (e parte de “{}” ficou escondida: {back})",
                        "{} (and part of “{}” stayed hidden: {back})",
                        stopped.error,
                        short(path)
                    )),
                    _ => Err(stopped.error),
                },
            }
        }
        other => Ok(other.clone()),
    }
}

// ---------------------------------------------------------------------------------------------
// Envios inacabados

/// The Envios inacabados dialog of a bucket: the large uploads that stopped midway (B2 keeps
/// their parts, billed as storage, until each is finished or cancelled), what B2 holds of each,
/// and Retomar, Cancelar envio or Cancelar todos.
pub struct Unfinished {
    pub server: Server,
    pub bucket: String,
    /// `None` while B2 is asked the first time.
    pub rows: Option<Result<Vec<b2::Unfinished>, String>>,
    /// B2 is being asked, or a cancel is on its way.
    pub busy: bool,
    /// Cancelar todos was pressed once: pressed again, it cancels.
    pub confirm_all: bool,
}

/// The bucket the menu's Envios inacabados opens: the one the pane is in, or at the key's top
/// the one bucket selected.
pub fn menu_bucket(s: &State, p: usize) -> Option<(Server, String)> {
    let (server, path) = at(&s.panes[p].loc)?;
    match path.first() {
        Some(bucket) => Some((server.clone(), bucket.clone())),
        None => match selected(s, p).as_slice() {
            [one] if one.is_dir => Some((server.clone(), one.name.clone())),
            _ => None,
        },
    }
}

/// Envios inacabados…, from the menu.
pub fn ask_unfinished(s: &mut State, p: usize, _: &mut Tx) {
    let Some((server, bucket)) = menu_bucket(s, p) else { return };
    s.dialog = Some(Dialog::B2Unfinished(Box::new(Unfinished { server, bucket, rows: None, busy: false, confirm_all: false })));
    list_unfinished(s);
}

/// Asks B2 for the open dialog's unfinished uploads, on a thread.
fn list_unfinished(s: &mut State) {
    let Some(Dialog::B2Unfinished(d)) = &mut s.dialog else { return };
    d.busy = true;
    d.confirm_all = false;
    let (key_id, bucket) = (d.server.user.clone(), d.bucket.clone());
    background(s, move |emit| {
        let result = catch_panic(t("ver os envios inacabados", "see the unfinished uploads"), || b2::reach(&key_id)?.unfinished_uploads(&bucket));
        emit(Msg::B2(Answer::Unfinished { key_id, bucket, result }));
    });
}

/// The open dialog, if it's of `bucket` on key `key_id`.
fn unfinished_of<'a>(s: &'a mut State, key_id: &str, bucket: &str) -> Option<&'a mut Unfinished> {
    match &mut s.dialog {
        Some(Dialog::B2Unfinished(d)) if d.server.user == key_id && d.bucket == bucket => Some(d),
        _ => None,
    }
}

fn unfinished_listed(s: &mut State, key_id: &str, bucket: &str, result: Result<Vec<b2::Unfinished>, String>) {
    if let Some(d) = unfinished_of(s, key_id, bucket) {
        d.rows = Some(result);
        d.busy = false;
    }
}

/// Cancels the uploads `ids` of the open dialog's bucket on a thread: their parts are deleted.
fn cancel_uploads(s: &mut State, ids: Vec<String>) {
    let Some(Dialog::B2Unfinished(d)) = &mut s.dialog else { return };
    if d.busy || ids.is_empty() {
        return;
    }
    d.busy = true;
    let (key_id, bucket) = (d.server.user.clone(), d.bucket.clone());
    background(s, move |emit| {
        let result = catch_panic(t("cancelar os envios", "cancel the uploads"), || b2::reach(&key_id)?.cancel_unfinished(&ids));
        emit(Msg::B2(Answer::Cancelled { key_id, bucket, result }));
    });
}

fn uploads_cancelled(s: &mut State, key_id: &str, bucket: &str, result: Result<usize, String>) {
    match result {
        Ok(n) => s.status = tr!("Envios inacabados cancelados no B2: {n}. As partes enviadas foram apagadas.", "Unfinished uploads cancelled on B2: {n}. The parts sent were deleted."),
        Err(e) => crate::fail(s, e),
    }
    if let Some(d) = unfinished_of(s, key_id, bucket) {
        d.busy = false;
        list_unfinished(s);
    }
}

/// Cancelar todos: the first press asks, the second cancels every upload not being sent now.
fn cancel_all_uploads(s: &mut State) {
    let Some(Dialog::B2Unfinished(d)) = &mut s.dialog else { return };
    if !d.confirm_all {
        d.confirm_all = true;
        return;
    }
    let ids = match &d.rows {
        Some(Ok(rows)) => rows.iter().filter(|r| !r.sending).map(|r| r.id.clone()).collect(),
        _ => Vec::new(),
    };
    cancel_uploads(s, ids);
}

/// Retomar: sends the file on this computer the upload was sending to the same name again, in
/// the queue; the parts B2 holds aren't sent again (see `openexplorer_b2::upload`).
fn resume_upload(s: &mut State, id: &str) {
    let Some(Dialog::B2Unfinished(d)) = &s.dialog else { return };
    let Some(Ok(rows)) = &d.rows else { return };
    let Some(row) = rows.iter().find(|r| r.id == id) else { return };
    let (Some(local), Some(size)) = (row.local.clone(), row.size) else { return };
    let (server, bucket, name) = (d.server.clone(), d.bucket.clone(), row.name.clone());
    s.dialog = None;
    let mut dirs: Vec<String> = std::iter::once(bucket.clone()).chain(name.split('/').map(str::to_string)).collect();
    let file = dirs.pop().unwrap_or_default();
    let into = folder(&server, dirs);
    let reload = panes_on(s, &server.id);
    let batch = crate::transfers::queue(&mut s.transfers, s.waker.clone(), crate::transfers::Op::Upload, [(crate::transfers::End::local(&local), crate::servers::end(&into, Some(&file)))]);
    let (pool, path) = (s.servers.pool.clone(), format!("/{bucket}/{name}"));
    s.upload_slots.set_limit(usize::from(s.settings.uploads));
    let slots = s.upload_slots.clone();
    background(s, move |emit| {
        let result = catch_panic(t("retomar o envio", "resume the upload"), || {
            batch.rows()[0].size(size);
            let _slot = (!batch.rows()[0].over()).then(|| slots.acquire());
            batch.run(|_, p| {
                crate::servers::with(&pool, &server, |c| {
                    let mut last = 0;
                    c.upload_file(&path, &local, &mut |n| {
                        p.reached(&mut last, n);
                        !p.cancelled()
                    })
                })
            })
        });
        emit(Msg::Done { reload, result });
    });
}

fn day_and_time(ms: i64) -> String {
    let t = SystemTime::UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64);
    crate::model::format_time(t)
}

/// What a row says under its name.
fn unfinished_detail(r: &b2::Unfinished) -> String {
    let sent = crate::model::format_size(r.sent);
    let started = day_and_time(r.started);
    let parts = if r.parts == 1 { t("1 parte", "1 part").to_string() } else { tr!("{} partes", "{} parts", r.parts) };
    let mut out = match r.size {
        Some(size) => tr!("{sent} de {} no B2 ({parts}) · começado em {started}", "{sent} of {} on B2 ({parts}) · started {started}", crate::model::format_size(size)),
        None => tr!("{sent} no B2 ({parts}) · começado em {started}", "{sent} on B2 ({parts}) · started {started}"),
    };
    out.push_str(if r.sending {
        t(" · enviando agora", " · being sent now")
    } else if r.local.is_some() {
        t(" · deste computador, pode ser retomado", " · from this computer, can be resumed")
    } else if r.size.is_some() {
        t(" · do Open Explorer em outro computador (ou o arquivo mudou)", " · from Open Explorer on another computer (or the file changed)")
    } else {
        t(" · de outro programa", " · from another program")
    });
    out
}

pub fn unfinished_dialog(k: &Kit, d: &Unfinished, layer: &mut Vec<El>) {
    let close = |s: &mut State| s.dialog = None;
    let small = |s: String| text(s).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0);
    let mut inner = col().key("content").w_full().px(16.0).gap(12.0).child(small(
        t(
            "Envios grandes que pararam no meio. As partes já enviadas ficam no B2, ocupando (e custando) espaço, até o envio terminar ou ser cancelado. Cancelar apaga as partes; Retomar envia só o que falta. Um envio que outro computador esteja fazendo agora também aparece aqui: não o cancele.",
            "Large uploads that stopped midway. The parts sent stay on B2, taking (and costing) space, until the upload finishes or is cancelled. Cancel deletes the parts; Resume sends only what's missing. An upload another computer is making right now shows here too: don't cancel it.",
        )
        .into(),
    ));
    match d.rows.as_ref().map(|r| r.as_deref()) {
        None => inner = inner.child(small(t("Perguntando ao B2…", "Asking B2…").into())),
        Some(Err(e)) => inner = inner.child(text(e.to_string()).key("error").font(k.ty.text_small).color(k.c.danger.fg).wrap().w_full().min_w(0.0)),
        Some(Ok([])) => inner = inner.child(small(t("Nenhum envio inacabado neste bucket.", "No unfinished uploads in this bucket.").into())),
        Some(Ok(rows)) => {
            let mut list = col().w_full().gap(10.0);
            for r in rows {
                let (id, again) = (r.id.clone(), r.id.clone());
                let mut line = row()
                    .key(format!("u:{}", r.id))
                    .w_full()
                    .gap(8.0)
                    .center_y()
                    .child(col().grow(1.0).min_w(0.0).gap(2.0).child(text(r.name.clone()).font(k.ty.text_small_medium).color(k.c.text_base).wrap().w_full().min_w(0.0)).child(small(unfinished_detail(r))));
                if r.local.is_some() && !r.sending {
                    line = line.child(Button::new(t("Retomar", "Resume")).variant(ButtonVariant::Neutral).disabled(d.busy).on_press(move |s: &mut State| resume_upload(s, &again)).build(k).cursor(rune::Cursor::Pointer));
                }
                line = line.child(Button::new(t("Cancelar envio", "Cancel upload")).variant(ButtonVariant::Neutral).disabled(d.busy || r.sending).on_press(move |s: &mut State| cancel_uploads(s, vec![id.clone()])).build(k).cursor(rune::Cursor::Pointer));
                list = list.child(line);
            }
            inner = inner.child(col().key("unfinished-rows").w_full().max_h(320.0).min_h(0.0).scroll(ScrollSpec { stick_bottom: false }).child(list));
        }
    }
    let cancellable = match &d.rows {
        Some(Ok(rows)) => rows.iter().filter(|r| !r.sending).count(),
        _ => 0,
    };
    let all = if d.confirm_all {
        tr!("Confirmar: cancelar {cancellable}", "Confirm: cancel {cancellable}")
    } else {
        t("Cancelar todos", "Cancel all").to_string()
    };
    let actions = row()
        .key("actions")
        .w_full()
        .px(16.0)
        .pt(12.0)
        .pb(16.0)
        .gap(8.0)
        .center_y()
        .child(Button::new(t("Atualizar", "Refresh")).variant(ButtonVariant::Neutral).disabled(d.busy).on_press(list_unfinished).build(k).cursor(rune::Cursor::Pointer))
        .child(rune::el().grow(1.0))
        .child(Button::new(all).variant(ButtonVariant::Danger).disabled(d.busy || cancellable == 0).on_press(cancel_all_uploads).build(k).cursor(rune::Cursor::Pointer))
        .child(Button::new(t("Fechar", "Close")).variant(ButtonVariant::Neutral).on_press(close).build(k).cursor(rune::Cursor::Pointer).default_button());
    let title = tr!("Envios inacabados · {}", "Unfinished uploads · {}", d.bucket);
    Modal::new("b2-unfinished", title).open(true).child(col().w_full().child(inner).child(actions)).width(620.0).fit().on_close(close).build(k, layer);
}

/// The headless scene `b2-unfinished`: a bucket's dialog with an upload of this computer, one
/// being sent and one of another program, without a network.
pub fn unfinished_scene(s: &mut State) {
    let server = Server::b2("0051a2b3c4d5e6f0000000001", "B2 · fotos-familia");
    s.servers.list.push(server.clone());
    s.panes[1].go(folder(&server, vec!["fotos-familia".into()]));
    s.active = 1;
    let day = 1_763_560_000_000;
    let row = |id: &str, name: &str, sent: u64, parts: usize, size: Option<u64>, sending: bool, local: Option<&str>| b2::Unfinished {
        id: id.into(),
        name: name.into(),
        started: day,
        sent,
        parts,
        size,
        sending,
        local: local.map(std::path::PathBuf::from),
    };
    let rows = vec![
        row("4_za", "videos/ferias-2025.mp4", 1_610_612_736, 48, Some(4_294_967_296), false, Some("C:\\Users\\voce\\Videos\\ferias-2025.mp4")),
        row("4_zb", "backup/fotos.zip", 335_544_320, 10, Some(2_147_483_648), true, None),
        row("4_zc", "arquivo-antigo.tar", 104_857_600, 1, None, false, None),
    ];
    s.dialog = Some(Dialog::B2Unfinished(Box::new(Unfinished { server, bucket: "fotos-familia".into(), rows: Some(Ok(rows)), busy: false, confirm_all: false })));
}

// ---------------------------------------------------------------------------------------------
// Addresses

/// The address bar's text for a B2 location: `b2://bucket/folder`.
pub fn address_text(path: &[String]) -> String {
    format!("b2://{}", path.join("/"))
}

/// What `b2://bucket/folder` says (the part after `b2://` is `rest`).
pub fn address(rest: &str, decode: impl Fn(&str) -> String) -> Address {
    Address { protocol: Protocol::B2, user: String::new(), password: None, host: "backblazeb2.com".into(), port: None, path: format!("/{}", decode(rest).trim_start_matches('/')) }
}

/// The location a `b2://` address leads to: the key whose buckets (as last seen) hold the
/// address's bucket (bucket names are unique across B2), else the first key.
pub fn reach_address(servers: &[Server], a: &Address) -> Result<Location, String> {
    let keys: Vec<&Server> = servers.iter().filter(|sv| sv.is_b2()).collect();
    let Some(first) = keys.first() else { return Err(String::new()) };
    let segs: Vec<String> = a.path.split('/').filter(|s| !s.is_empty()).map(str::to_string).collect();
    let Some(bucket) = segs.first() else { return Ok(top(first)) };
    let knows = |sv: &&&Server| b2::cached(&sv.user).or_else(|| b2::reach(&sv.user).ok()).is_some_and(|acc| acc.known_buckets().iter().any(|b| &b.name == bucket));
    let sv = keys.iter().find(knows).unwrap_or(first);
    Ok(folder(sv, segs))
}

// ---------------------------------------------------------------------------------------------
// The Adicionar conta step

/// The step's banner line and badge.
pub fn summary() -> (&'static str, &'static str) {
    (t("Chave de aplicação · API nativa do B2", "Application key · B2 native API"), "B2")
}

/// The key ID and application key fields, what the key needs, and Conectar.
pub fn form(k: &Kit, f: &Form, banner: El, eye: El, close: fn(&mut State, &mut Tx)) -> (El, El) {
    fn part(s: &mut State, pick: fn(&mut Form) -> &mut rune::TextEdit) -> &mut rune::TextEdit {
        match &mut s.account_form {
            Some(f) => pick(f),
            None => &mut s.name_edit,
        }
    }
    let labeled = |key: &'static str, label: &'static str, input: El| col().key(key).w_full().gap(4.0).child(text(label).font(k.ty.text_small_medium).color(k.c.text_base)).child(input);
    let id_input = rune_kit::TextInput::new(&f.user)
        .width(None)
        .label(t("ID da chave (keyID)", "Key ID (keyID)"))
        .placeholder("0051a2b3c4d5e6f0000000001")
        .on_edit(|s: &mut State| part(s, |f| &mut f.user))
        .on_submit(|s: &mut State, _: &mut Tx| submit(s))
        .build(k);
    let key_input = rune_kit::TextInput::new(&f.password)
        .width(None)
        .label(t("Chave de aplicação (applicationKey)", "Application key (applicationKey)"))
        .revealed(f.reveal_password)
        .trailing(eye)
        .on_edit(|s: &mut State| part(s, |f| &mut f.password))
        .on_submit(|s: &mut State, _: &mut Tx| submit(s))
        .build(k);
    let note = |s: &'static str| text(s).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0);
    let mut inner = col()
        .w_full()
        .gap(12.0)
        .child(banner)
        .child(note(t(
            "Crie uma chave em Application Keys, no site da Backblaze: para todos os buckets ou só um (e, se quiser, só um prefixo). Ver e baixar pede listBuckets, listFiles e readFiles; enviar, criar pastas, renomear e excluir pede writeFiles; Excluir todas as versões pede deleteFiles.",
            "Make a key under Application Keys on the Backblaze website: for all buckets or just one (and, if you like, just a prefix). Seeing and downloading needs listBuckets, listFiles and readFiles; uploading, making folders, renaming and deleting needs writeFiles; Delete every version needs deleteFiles.",
        )))
        .child(labeled("b2-key-id", t("ID da chave (keyID)", "Key ID (keyID)"), id_input))
        .child(labeled("b2-key", t("Chave de aplicação (applicationKey)", "Application key (applicationKey)"), key_input))
        .child(note(t(
            "O Open Explorer guarda a chave no cofre de credenciais do sistema. O site só mostra a chave de aplicação uma vez, ao criá-la.",
            "Open Explorer keeps the key in the system's credential store. The website shows the application key only once, when it's made.",
        )));
    let checking = f.checking.is_some();
    if let Some(e) = &f.error {
        inner = inner.child(text(e.clone()).key("error").font(k.ty.text_small).color(k.c.danger.fg).wrap().w_full().min_w(0.0));
    }
    let connect = if checking {
        Button::new(t("Conectando…", "Connecting…")).variant(ButtonVariant::Submit).disabled(true).build(k).cursor(rune::Cursor::Pointer)
    } else {
        Button::new(t("Conectar", "Connect")).variant(ButtonVariant::Submit).on_press(submit).build(k).cursor(rune::Cursor::Pointer).default_button()
    };
    let actions = row()
        .key("actions")
        .w_full()
        .gap(8.0)
        .center_y()
        .child(Button::new(t("Voltar", "Back")).icon(&crate::icons::BACK).variant(ButtonVariant::Neutral).disabled(checking).on_press(crate::servers::back_to_pick).build(k).cursor(rune::Cursor::Pointer))
        .child(rune::el().grow(1.0))
        .child(Button::new(t("Cancelar", "Cancel")).variant(ButtonVariant::Neutral).on_press_tx(close).build(k).cursor(rune::Cursor::Pointer))
        .child(connect);
    (inner, actions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_names_its_bucket_and_folder_prefix() {
        assert_eq!(bucket_of("/"), None);
        assert_eq!(bucket_of("/fotos"), Some(("fotos", String::new())));
        assert_eq!(bucket_of("/fotos/2024/ferias/"), Some(("fotos", "2024/ferias/".to_string())));
    }

    fn at_path(path: &[&str]) -> Location {
        folder(&Server::b2("0051a2b3", "B2 · fotos"), path.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn the_top_lists_buckets_and_takes_nothing_new() {
        assert!(why_not(&at_path(&[])).is_some() && items_fixed(&at_path(&[])));
        assert!(why_not(&at_path(&["fotos"])).is_none() && !items_fixed(&at_path(&["fotos"])));
        assert!(why_not(&Location::home()).is_none() && !items_fixed(&Location::home()));
    }

    #[test]
    fn keys_show_as_servers_of_their_own_with_b2_addresses() {
        let sv = Server::b2("0051a2b3", "B2 · fotos");
        assert!(sv.is_b2() && !sv.is_mega());
        assert_eq!((sv.id.as_str(), sv.user.as_str(), sv.name.as_str()), ("b2:0051a2b3", "0051a2b3", "B2 · fotos"));
        assert_eq!(address_text(&["fotos".into(), "2024".into()]), "b2://fotos/2024");
        let a = address("fotos/2024%20ver%C3%A3o", |s| s.replace("%20", " ").replace("%C3%A3", "ã"));
        assert_eq!((a.protocol, a.path.as_str()), (Protocol::B2, "/fotos/2024 verão"));
        // No key: the address isn't reached.
        assert!(reach_address(&[], &a).is_err());
    }

    /// Excluir in a bucket asks as for a trash (what's hidden comes back with Desfazer), not
    /// "for good"; the top, which lists buckets, takes nothing new and its buckets stay.
    #[test]
    fn deleting_in_a_bucket_hides_and_the_top_holds_still() {
        let mut s = crate::init();
        s.cache = crate::cache::Cache::memory_only();
        let e = |id: &str| Entry { id: id.into(), name: id.into(), is_dir: false, size: Some(1), modified: None, target: None, mime: None, md5: None, starred: false };
        for path in [&["fotos"][..], &["fotos", "2024"]] {
            s.dialog = None;
            s.panes[1].go(at_path(path));
            s.panes[1].set_entries(Ok(vec![e("a.txt")]));
            s.panes[1].selected = ["a.txt".to_string()].into();
            crate::actions::ask_delete(&mut s, 1);
            assert!(matches!(&s.dialog, Some(Dialog::Delete { .. })), "{path:?} hides");
        }
        assert!(delete_note(&at_path(&["fotos"])).contains("Desfazer"));
        assert!(crate::actions::editable(&at_path(&["fotos"])) && !crate::actions::editable(&at_path(&[])));
        assert!(!crate::actions::items_editable(&at_path(&[])) && crate::actions::items_editable(&at_path(&["fotos"])));
        // The address bar's text for a folder.
        assert_eq!(crate::servers::address_text(&at_path(&["fotos", "2024"])), "b2://fotos/2024");
        let a = crate::servers::parse_address("b2://fotos/2024").unwrap();
        assert_eq!((a.protocol, a.path.as_str()), (Protocol::B2, "/fotos/2024"));
    }

    fn file(name: &str) -> Entry {
        Entry { id: name.into(), name: name.into(), is_dir: false, size: Some(1), modified: None, target: None, mime: None, md5: None, starred: false }
    }

    fn item(name: &str) -> b2::Item {
        b2::Item { name: name.into(), is_dir: false, size: Some(1), modified: None, id: None, info: None }
    }

    fn shown(s: &State, p: usize) -> Vec<String> {
        s.panes[p].entries.iter().map(|e| e.name.clone()).collect()
    }

    /// A search finds files and the folders on their way (once each, by name), not the empty
    /// folders' markers, nor what hidden folders hold unless hidden items show.
    #[test]
    fn a_search_finds_files_and_the_folders_on_their_way() {
        let files = || {
            ["fotos 2024/praia.jpg", "fotos 2024/praia (2).jpg", "fotos 2024/vazia/.bzEmpty", "outra/praia/x.txt", ".oculta/praia.jpg", "a//praia.jpg"].map(|r| (r.to_string(), item(r.rsplit('/').next().unwrap())))
        };
        let run = |text: &str, show_hidden: bool| {
            let mut found = Found { words: crate::finder::words(text), show_hidden, folders: Default::default(), buf: Vec::new() };
            let mut got = Vec::new();
            // Two pages: a folder met again in the second isn't given twice.
            found.page(files().to_vec(), &mut |e| got.push((e.id, e.is_dir)));
            found.page(files()[..2].to_vec(), &mut |e| got.push((e.id, e.is_dir)));
            got
        };
        let got = run("praia", false);
        assert_eq!(got, [("fotos 2024/praia.jpg".to_string(), false), ("fotos 2024/praia (2).jpg".into(), false), ("outra/praia".into(), true), ("fotos 2024/praia.jpg".into(), false), ("fotos 2024/praia (2).jpg".into(), false)]);
        assert_eq!(run("2024", false), [("fotos 2024".to_string(), true)]);
        assert!(run("bzempty", true).is_empty());
        assert!(run("praia", true).contains(&(".oculta/praia.jpg".to_string(), false)));
        assert!(run("vazia", false).contains(&("fotos 2024/vazia".to_string(), true)));
    }

    /// A reading on the app's own that found a change replaces the list in place, the selection
    /// kept; one for a place the pane left, or one older than its last load, changes nothing.
    #[test]
    fn a_change_elsewhere_shows_in_place_keeping_the_selection() {
        let mut s = crate::init();
        s.cache = crate::cache::Cache::memory_only();
        let loc = at_path(&["fotos"]);
        s.panes[1].go(loc.clone());
        s.panes[1].set_entries(Ok(vec![file("a.txt"), file("b.txt")]));
        s.panes[1].selected = ["b.txt".to_string()].into();
        let request = s.live[1].load(Ordering::Relaxed);
        let changed = |names: &[&str]| Ok(b2::Polled::Changed(names.iter().map(|n| item(n)).collect()));
        arrived(&mut s, Answer::Polled { pane: 1, request, loc: Box::new(loc.clone()), result: changed(&["b.txt", "c.txt"]) });
        assert_eq!(shown(&s, 1), ["b.txt", "c.txt"]);
        assert!(s.panes[1].selected.contains("b.txt") && !s.panes[1].loading);
        // From before the pane's last load, or of another place: dropped.
        arrived(&mut s, Answer::Polled { pane: 1, request: request + 1, loc: Box::new(loc.clone()), result: changed(&["x"]) });
        arrived(&mut s, Answer::Polled { pane: 1, request, loc: Box::new(at_path(&["docs"])), result: changed(&["y"]) });
        assert_eq!(shown(&s, 1), ["b.txt", "c.txt"]);
        // An opening that found the folder as kept only stops loading.
        s.panes[1].loading = true;
        arrived(&mut s, Answer::Same { pane: 1, request });
        assert!(!s.panes[1].loading && s.panes[1].selected.contains("b.txt"));
    }

    /// A folder is read again once its turn comes, one reading at a time, and not while the
    /// window is away; back in front, a turn that came meanwhile goes at once.
    #[test]
    fn readings_wait_their_turn_and_the_window_in_front() {
        let mut s = crate::init();
        s.cache = crate::cache::Cache::memory_only();
        let loc = at_path(&["fotos"]);
        s.panes[1].go(loc.clone());
        s.panes[1].set_entries(Ok(vec![file("a.txt")]));
        let every = b2::watch::EVERY.as_secs_f64();
        tick(&mut s, 0.0);
        assert_eq!(next_ask(&s), Some(every));
        tick(&mut s, every - 1.0);
        assert_eq!(next_ask(&s), Some(every), "not yet");
        let mut tx = rune::Tx::default();
        window_focus(&mut s, false, &mut tx);
        assert_eq!(next_ask(&s), None);
        tick(&mut s, every + 60.0);
        assert!(!s.servers.b2_watch.panes[1].as_ref().unwrap().busy, "nothing read while away");
        window_focus(&mut s, true, &mut tx);
        tick(&mut s, every + 61.0);
        assert!(s.servers.b2_watch.panes[1].as_ref().unwrap().busy, "read on coming back");
        assert_eq!(next_ask(&s), None, "one reading at a time");
        s.now = every + 62.0;
        let request = s.live[1].load(Ordering::Relaxed);
        arrived(&mut s, Answer::Polled { pane: 1, request, loc: Box::new(loc.clone()), result: Ok(b2::Polled::Wait(300)) });
        assert_eq!(next_ask(&s), Some(every + 62.0 + 300.0), "B2 asked for a pause");
        // Leaving the key forgets the turn.
        s.panes[1].go(Location::home());
        tick(&mut s, every + 63.0);
        assert_eq!(next_ask(&s), None);
    }

    /// Envios inacabados opens for the bucket a pane is in, or the one bucket selected at the
    /// top; Cancelar todos asks first, and leaves the upload being sent now.
    #[test]
    fn unfinished_uploads_open_for_the_bucket_and_cancel_all_asks_first() {
        let mut s = crate::init();
        s.cache = crate::cache::Cache::memory_only();
        s.panes[1].go(at_path(&["fotos", "2024"]));
        assert_eq!(menu_bucket(&s, 1).map(|(_, b)| b).as_deref(), Some("fotos"));
        s.panes[1].go(at_path(&[]));
        let bucket = |n: &str| Entry { is_dir: true, size: None, ..file(n) };
        s.panes[1].set_entries(Ok(vec![bucket("docs"), bucket("fotos")]));
        assert!(menu_bucket(&s, 1).is_none(), "which bucket?");
        s.panes[1].selected = ["fotos".to_string()].into();
        assert_eq!(menu_bucket(&s, 1).map(|(_, b)| b).as_deref(), Some("fotos"));
        assert!(menu_bucket(&s, 0).is_none(), "not on B2");
        unfinished_scene(&mut s);
        cancel_all_uploads(&mut s);
        let Some(Dialog::B2Unfinished(d)) = &s.dialog else { panic!("no dialog") };
        assert!(d.confirm_all && !d.busy, "the first press only asks");
        cancel_all_uploads(&mut s);
        let Some(Dialog::B2Unfinished(d)) = &s.dialog else { panic!("no dialog") };
        assert!(d.busy, "the second cancels");
    }

    /// Excluir that stops midway keeps the Desfazer of what it hid: the items before, and the
    /// part of the folder hidden before the error; nothing hidden, nothing to record.
    #[test]
    fn a_delete_that_stops_midway_keeps_the_undo_of_what_it_hid() {
        let paths: Vec<(String, bool)> = vec![("/fotos/a.txt".into(), false), ("/fotos/2024".into(), true), ("/fotos/b.txt".into(), false)];
        let hide = |path: &str, _: bool| match path {
            "/fotos/2024" => Err(b2::HideStopped { since: Some(20), error: "recusado".into() }),
            _ => Ok(10),
        };
        let mut got = Vec::new();
        let e = hide_each(&paths, hide, |p, d, since| got.push((p.to_string(), d, since))).unwrap_err();
        assert_eq!(got, [("/fotos/a.txt".to_string(), false, 10), ("/fotos/2024".to_string(), true, 20)]);
        assert!(e.starts_with("recusado") && e.contains("Desfazer"), "{e}");
        // Refused before hiding anything: the error alone.
        let mut got = Vec::new();
        let e = hide_each(&paths[1..2], |_, _| Err(b2::HideStopped { since: None, error: "recusado".into() }), |p, d, s| got.push((p.to_string(), d, s))).unwrap_err();
        assert!(got.is_empty());
        assert_eq!(e, "recusado");
        let mut got = Vec::new();
        assert!(hide_each(&paths, |_, _| Ok(5), |p, d, s| got.push((p.to_string(), d, s))).is_ok());
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn rows_say_what_b2_holds_and_who_started_them() {
        let mut s = crate::init();
        unfinished_scene(&mut s);
        let Some(Dialog::B2Unfinished(d)) = &s.dialog else { panic!("no dialog") };
        let Some(Ok(rows)) = &d.rows else { panic!("no rows") };
        let said: Vec<String> = rows.iter().map(unfinished_detail).collect();
        assert!(said[0].starts_with("1,5 GB de 4,0 GB no B2 (48 partes)") && said[0].ends_with("pode ser retomado"), "{}", said[0]);
        assert!(said[1].ends_with("enviando agora"));
        assert!(said[2].starts_with("100,0 MB no B2 (1 parte)") && said[2].ends_with("outro programa"), "{}", said[2]);
    }

    /// Retomar puts the file in the queue and closes the dialog; an upload with no file on this
    /// computer can't be taken up.
    #[test]
    fn resuming_queues_the_file_on_this_computer() {
        let mut s = crate::init();
        unfinished_scene(&mut s);
        resume_upload(&mut s, "4_zc");
        assert!(s.dialog.is_some() && s.transfers.is_empty());
        resume_upload(&mut s, "4_za");
        assert!(s.dialog.is_none());
        assert_eq!(s.transfers.len(), 1);
        assert!(s.transfers[0].from.text.ends_with("ferias-2025.mp4") && s.transfers[0].to.text.contains("fotos-familia\\videos\\ferias-2025.mp4"), "{}", s.transfers[0].to.text);
    }

    #[test]
    fn a_key_lacking_capabilities_says_what_it_cant_do() {
        let all = b2::Allowed { buckets: None, capabilities: ["listBuckets", "listFiles", "readFiles", "writeFiles", "deleteFiles"].map(String::from).to_vec(), name_prefix: None };
        assert!(missing(&all).is_empty());
        let read_only = b2::Allowed { capabilities: ["listBuckets", "listFiles", "readFiles"].map(String::from).to_vec(), ..all };
        assert_eq!(missing(&read_only).len(), 2);
    }
}
