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
use rune::{El, Tx, col, row, text};
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

/// Lists pane `p`'s B2 folder on a thread. A listing kept from before shows at once, and B2 is
/// always asked again behind it (B2 can't say what changed, and listing is free; see
/// `openexplorer_b2::watch`): the pane changes only if B2's answer differs, so nothing blinks
/// and the selection and the scroll stay. `_fresh` (Atualizar) changes nothing here: every
/// opening reads. A search goes the servers' way.
pub fn load(s: &mut State, p: usize, request: u64, _fresh: bool) {
    let Location::Server { server, path, find: None } = s.panes[p].loc.clone() else {
        return crate::servers::load(s, p, request);
    };
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
        let Some(kept) = account.cached(&dir) else {
            let result = catch_panic(t("listar", "list"), || account.list_fresh(&dir).map(|items| entries(items, show_hidden)));
            return emit(listed(Part::WHOLE, result));
        };
        emit(listed(Part::Cached, Ok(entries(kept.items.clone(), show_hidden))));
        drop(kept);
        match catch_panic(t("listar", "list"), || account.reread(&dir).map(Some)) {
            Ok(Some(b2::Fresh::Changed(items))) => emit(listed(Part::WHOLE, Ok(entries(items, show_hidden)))),
            Ok(_) => emit(Msg::B2(Answer::Same { pane: p, request })),
            // The kept listing stays on screen; the error is said.
            Err(e) => emit(listed(Part::Page { first: false, more: false }, Err(e))),
        }
    });
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
    panes: [Option<Turn>; 2],
}

struct Turn {
    loc: Location,
    /// When it's read next (app clock, seconds).
    next: f64,
    busy: bool,
}

/// The window gained or lost the focus. Back in front, a folder whose turn came meanwhile is
/// read at once.
pub fn window_focus(s: &mut State, focused: bool, _: &mut Tx) {
    s.servers.b2_watch.away = !focused;
}

/// How long until a folder of `items` names is read again.
fn every(items: usize) -> f64 {
    b2::watch::every(items).as_secs_f64()
}

/// Reads again each pane's B2 folder whose turn came.
pub fn tick(s: &mut State, now: f64) {
    for p in 0..2 {
        let shows = matches!(&s.panes[p].loc, Location::Server { server, find: None, .. } if server.is_b2());
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
    s.panes[pane].set_entries(Ok(entries(items, show_hidden)));
    crate::drop_stale_rename(s, pane);
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
}

pub fn arrived(s: &mut State, a: Answer) {
    match a {
        Answer::Connected { attempt, result } => connected(s, attempt, result),
        Answer::Same { pane, request } => same(s, pane, request),
        Answer::Polled { pane, request, loc, result } => polled(s, pane, request, *loc, result),
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
    let Some(Dialog::Server(f)) = &mut s.dialog else { return };
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
    let Some(Dialog::Server(f)) = &mut s.dialog else { return };
    if f.checking != Some(attempt) {
        return;
    }
    f.checking = None;
    let (key_id, name, lacks) = match result {
        Ok(r) => r,
        Err(e) => {
            openexplorer_gdrive::log(&format!("falha ao conectar B2: {e}"));
            f.error = Some(e);
            return;
        }
    };
    if let Err(e) = s.servers.b2.add(&key_id, &name) {
        f.error = Some(e);
        return;
    }
    openexplorer_gdrive::log(&format!("chave B2 conectada: {name}"));
    s.dialog = None;
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
        for (i, (path, is_dir)) in paths.iter().enumerate() {
            let since = a.hide(path, *is_dir).map_err(|e| if i == 0 { e } else { tr!("{e} (já excluídos: {i})", "{e} (already deleted: {i})") })?;
            rec.add([Step::B2Unhide { server: at_server.clone(), path: path.clone(), is_dir: *is_dir, since }]);
        }
        Ok(tr!("Excluído do B2 (escondido): {}.", "Deleted from B2 (hidden): {}.", crate::actions::count(paths.len())))
    });
}

/// Excluir todas as versões, from the menu: asks first.
pub fn ask_purge(s: &mut State, p: usize, _: &mut Tx) {
    let items = selected(s, p);
    if !items.is_empty() && at(&s.panes[p].loc).is_some() && !items_fixed(&s.panes[p].loc) {
        s.dialog = Some(Dialog::Purge { pane: p, items });
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
            let since = b2::reach(&server.user)?.hide(path, *is_dir)?;
            Ok(Step::B2Unhide { server: server.clone(), path: path.clone(), is_dir: *is_dir, since })
        }
        other => Ok(other.clone()),
    }
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
pub fn form(k: &Kit, f: &Form, banner: El, eye: El, scroll: impl FnOnce(El) -> El, close: fn(&mut State), layer: &mut Vec<El>) {
    fn part(s: &mut State, pick: fn(&mut Form) -> &mut rune::TextEdit) -> &mut rune::TextEdit {
        match &mut s.dialog {
            Some(Dialog::Server(f)) => pick(f),
            _ => &mut s.name_edit,
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
        .px(16.0)
        .pb(4.0)
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
        Button::new(t("Conectando…", "Connecting…")).variant(ButtonVariant::Submit).disabled(true).build(k)
    } else {
        Button::new(t("Conectar", "Connect")).variant(ButtonVariant::Submit).on_press(submit).build(k).default_button()
    };
    let actions = row()
        .key("actions")
        .w_full()
        .px(16.0)
        .pt(12.0)
        .pb(16.0)
        .gap(8.0)
        .center_y()
        .child(Button::new(t("Voltar", "Back")).icon(&crate::icons::BACK).variant(ButtonVariant::Neutral).disabled(checking).on_press(crate::servers::back_to_pick).build(k))
        .child(rune::el().grow(1.0))
        .child(Button::new(t("Cancelar", "Cancel")).variant(ButtonVariant::Neutral).on_press(close).build(k))
        .child(connect);
    let body = col().key("b2-content").w_full().child(scroll(inner)).child(actions);
    Modal::new("server", t("Conectar Backblaze B2", "Connect Backblaze B2")).open(true).child(body).width(540.0).fit().on_close(close).build(k, layer);
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_key_lacking_capabilities_says_what_it_cant_do() {
        let all = b2::Allowed { buckets: None, capabilities: ["listBuckets", "listFiles", "readFiles", "writeFiles", "deleteFiles"].map(String::from).to_vec(), name_prefix: None };
        assert!(missing(&all).is_empty());
        let read_only = b2::Allowed { capabilities: ["listBuckets", "listFiles", "readFiles"].map(String::from).to_vec(), ..all };
        assert_eq!(missing(&read_only).len(), 2);
    }
}
