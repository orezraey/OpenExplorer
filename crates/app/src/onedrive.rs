//! OneDrive accounts (personal and work or school) in the panes. Each account shows in the tree
//! under Contas, beside the Google Drive and MEGA accounts and the servers, and opens like a
//! server (see `servers.rs`): its top holds Meus arquivos. Listing, copying to and from
//! anywhere, moving, renaming, new folders, searching and dragging go through the servers'
//! code, with the account as the connection (`openexplorer_remote`'s `onedrive.rs`). What only
//! OneDrive has lives here: signing in through the browser, deleting to its recycle bin (undone
//! by restoring, on personal accounts), sharing links, the storage in the status bar, and
//! folders that refresh when something changes elsewhere.
//!
//! Graph has no way to list OneDrive's recycle bin, so no Lixeira shows in the tree: deleted
//! items come back with Desfazer (personal accounts) or from the website.

use std::collections::HashMap;

use openexplorer_i18n::{t, tr};
use openexplorer_onedrive::{self as od, Account, Changes, Kind, Quota, StoredAccount};
use openexplorer_remote::Server;
use rune::{El, TextEdit, Tx, col, row, text};
use rune_kit::{Button, ButtonVariant, Dialog as Modal, Kit};

use crate::actions::{Dialog, selected};
use crate::model::{Entry, Location};
use crate::servers::{Form, folder, path_of};
use crate::undo::{self, Step};
use crate::{Msg, State, background, catch_panic};

/// Where the app registration's portal is.
const PORTAL: &str = "https://entra.microsoft.com/#view/Microsoft_AAD_RegisteredApps/ApplicationsListBlade";

/// The OneDrive accounts' state beside the servers'.
pub struct Side {
    pub store: od::Store,
    /// Each account's storage, by server id: `None` while the answer is on its way.
    pub quotas: HashMap<String, Option<Quota>>,
    /// When each account a pane shows is next asked what changed.
    pub watch: HashMap<String, Watch>,
    /// The sign-in waiting for the browser, by number.
    pub login: Option<u64>,
}

impl Side {
    pub fn load() -> Side {
        Side { store: od::Store::at(openexplorer_gdrive::config_dir().join("onedrive-accounts.json")), quotas: HashMap::new(), watch: HashMap::new(), login: None }
    }

    /// The servers the connected accounts show as.
    pub fn servers(&self) -> Vec<Server> {
        self.store.accounts().iter().map(server_of).collect()
    }
}

pub fn server_of(a: &StoredAccount) -> Server {
    Server::onedrive(&a.id, &a.email)
}

/// The OneDrive account a location is in, and the path below its top.
pub fn at(loc: &Location) -> Option<(&Server, &[String])> {
    match loc {
        Location::Server { server, path, .. } if server.is_onedrive() => Some((server, path)),
        _ => None,
    }
}

/// Where a OneDrive location doesn't take new items: the account's top. `None` where it does
/// (or it isn't OneDrive).
pub fn why_not(loc: &Location) -> Option<String> {
    let (_, path) = at(loc)?;
    path.is_empty().then(|| t("O topo da conta OneDrive só tem Meus arquivos: abra essa pasta.", "The OneDrive account's top only holds My files: open that folder.").into())
}

/// Whether the items `loc` lists can be renamed, cut or deleted: not Meus arquivos itself.
pub fn items_fixed(loc: &Location) -> bool {
    at(loc).is_some_and(|(_, path)| path.is_empty())
}

/// Meus arquivos, where a new account opens.
pub fn files(server: &Server) -> Location {
    folder(server, vec![od::Area::Files.label().to_string()])
}

fn panes_on(s: &State, id: &str) -> Vec<Location> {
    s.panes.iter().map(|p| p.loc.clone()).filter(|l| matches!(l, Location::Server { server, .. } if server.id == id)).collect()
}

/// Runs `work` with the account on a thread, then `done` with the result.
fn task<T: Send + 'static>(s: &State, server: &Server, doing: &'static str, work: impl FnOnce(&Account) -> Result<T, String> + Send + 'static, done: impl FnOnce(Result<T, String>) -> Msg + Send + 'static) {
    let id = server.user.clone();
    background(s, move |emit| {
        let result = catch_panic(doing, || od::reach(&id).and_then(|a| work(&a)));
        emit(done(result));
    });
}

/// What background work on OneDrive brought back.
pub enum Answer {
    /// The sign-in `attempt` ended.
    Connected { attempt: u64, result: Result<StoredAccount, String> },
    Quota { id: String, result: Result<Quota, String> },
    /// What changed in account `id` since it was last asked.
    Changes { id: String, result: Result<Changes, String> },
    /// What Copiar link found: the item's link, or `None` when it has none yet.
    Link { server: Box<Server>, path: String, item: Entry, result: Result<Option<String>, String> },
    /// A link made (`Some`) or removed for the item named `name`.
    Linked { name: String, result: Result<Option<String>, String> },
}

pub fn arrived(s: &mut State, a: Answer, tx: &mut Tx, now: f64) {
    match a {
        Answer::Connected { attempt, result } => connected(s, attempt, result),
        Answer::Quota { id, result } => match result {
            Ok(q) => {
                s.servers.onedrive.quotas.insert(id, Some(q));
            }
            Err(e) => openexplorer_gdrive::log(&format!("cota do OneDrive {id}: {e}")),
        },
        Answer::Changes { id, result } => changes_arrived(s, id, result, now),
        Answer::Link { server, path, item, result } => match result {
            Ok(Some(link)) => {
                tx.copy(link);
                s.status = tr!("Link copiado: quem tiver o link pode abrir \"{}\".", "Link copied: whoever has the link can open \"{}\".", item.name);
            }
            Ok(None) => s.dialog = Some(Dialog::OneDriveLink(Box::new(MakeLink { server: *server, path, item }))),
            Err(e) => crate::fail(s, e),
        },
        Answer::Linked { name, result } => match result {
            Ok(Some(link)) => {
                tx.copy(link);
                s.status = tr!("Link criado e copiado: quem tiver o link pode abrir \"{name}\".", "Link made and copied: whoever has the link can open \"{name}\".");
            }
            Ok(None) => s.status = tr!("Link removido: \"{name}\" não abre mais pelo link.", "Link removed: \"{name}\" no longer opens from the link."),
            Err(e) => crate::fail(s, e),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Signing in and removing an account

/// Picking OneDrive in Adicionar conta: whether an app registration is set up was read once.
pub fn prepare(f: &mut Form) {
    f.custom_oauth = od::ClientConfig::load().is_err();
    f.client_id = TextEdit::new("");
    f.client_id.focused = f.custom_oauth;
}

fn client_field(s: &mut State) -> &mut TextEdit {
    match &mut s.dialog {
        Some(Dialog::Server(f)) => &mut f.client_id,
        _ => &mut s.name_edit,
    }
}

/// Conectar no navegador: the registration typed (kept for next time) or saved, then the
/// browser opens Microsoft's sign-in and a thread waits for it to come back.
pub fn submit(s: &mut State, tx: &mut Tx) {
    let Some(Dialog::Server(f)) = &mut s.dialog else { return };
    f.error = None;
    let typed = f.client_id.text.trim().to_string();
    let cfg = if !typed.is_empty() {
        match od::ClientConfig::parse(&typed).and_then(|c| c.save().map(|_| c)) {
            Ok(c) => c,
            Err(e) => {
                f.error = Some(e);
                return;
            }
        }
    } else {
        match od::ClientConfig::load() {
            Ok(c) => c,
            Err(e) => {
                f.custom_oauth = true;
                f.client_id.focused = true;
                f.error = Some(e);
                return;
            }
        }
    };
    let pending = match od::oauth::begin(&cfg, &od::graph::Endpoints::default()) {
        Ok(p) => p,
        Err(e) => {
            f.error = Some(e.text);
            return;
        }
    };
    if let Err(e) = tx.open_url(&pending.url) {
        f.error = Some(tr!("Não consegui abrir o navegador: {e:?}", "Couldn't open the browser: {e:?}"));
        return;
    }
    s.servers.attempts += 1;
    let attempt = s.servers.attempts;
    s.servers.onedrive.login = Some(attempt);
    s.dialog = None;
    s.status = t("Faça o login no navegador que abriu. Esperando a Microsoft…", "Sign in in the browser that opened. Waiting for Microsoft…").into();
    background(s, move |emit| {
        let result = catch_panic(t("conectar ao OneDrive", "connect to OneDrive"), || {
            let a = od::connect(&pending)?;
            Ok(StoredAccount { id: a.meta.id.clone(), email: a.meta.email.clone(), name: a.meta.name.clone(), business: a.meta.kind == Kind::Business })
        });
        emit(Msg::OneDrive(Answer::Connected { attempt, result }));
    });
}

fn connected(s: &mut State, attempt: u64, result: Result<StoredAccount, String>) {
    if s.servers.onedrive.login == Some(attempt) {
        s.servers.onedrive.login = None;
    }
    let account = match result {
        Ok(a) => a,
        Err(e) => {
            openexplorer_gdrive::log(&format!("falha ao conectar OneDrive: {e}"));
            return crate::fail(s, e);
        }
    };
    if let Err(e) = s.servers.onedrive.store.add(account.clone()) {
        od::disconnect(&account.id);
        return crate::fail(s, e);
    }
    openexplorer_gdrive::log(&format!("conta OneDrive conectada: {} ({})", account.email, if account.business { "corporativa" } else { "pessoal" }));
    let server = server_of(&account);
    match s.servers.list.iter_mut().find(|sv| sv.id == server.id) {
        Some(sv) => *sv = server.clone(),
        None => s.servers.list.push(server.clone()),
    }
    s.servers.pool.forget(&server.id);
    s.servers.onedrive.quotas.remove(&server.id);
    s.tree.server_changed(&server.id);
    s.tree.server_opened(&server.id);
    s.status = tr!("Conta OneDrive conectada: {}", "OneDrive account connected: {}", account.email);
    let p = s.active;
    crate::navigate(s, p, files(&server));
}

/// Remover conta: its tokens leave the credential store and its kept tree goes. Whether the
/// account left the list.
pub fn remove(s: &mut State, server: &Server) -> bool {
    if let Err(e) = s.servers.onedrive.store.remove(&server.user) {
        crate::fail(s, e);
        return false;
    }
    od::disconnect(&server.user);
    s.servers.onedrive.quotas.remove(&server.id);
    s.servers.onedrive.watch.remove(&server.id);
    true
}

/// The OneDrive step of Adicionar conta: what's needed, then the browser.
pub fn configure(s: &State, k: &Kit, f: &Form, layer: &mut Vec<El>) {
    let close = |s: &mut State| s.dialog = None;
    let muted = |words: String| text(words).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0);
    let mut inner = col().key("onedrive-content").w_full().px(16.0).pb(4.0).gap(12.0).child(muted(
        t(
            "Ao continuar, o navegador abre a página da Microsoft para você escolher a conta (pessoal, corporativa ou de estudante) e autorizar o acesso aos seus arquivos do OneDrive.",
            "When you continue, the browser opens Microsoft's page so you can pick the account (personal, work or school) and allow access to your OneDrive files.",
        )
        .into(),
    ));
    let connected: Vec<String> = s.servers.onedrive.store.accounts().into_iter().map(|a| a.email).collect();
    if !connected.is_empty() {
        inner = inner.child(muted(tr!("Já conectadas: {}", "Already connected: {}", connected.join(", "))));
    }
    if f.custom_oauth {
        let input = rune_kit::TextInput::new(&f.client_id)
            .width(None)
            .label(t("ID do aplicativo (cliente)", "Application (client) ID"))
            .placeholder("00000000-0000-0000-0000-000000000000")
            .on_edit(client_field)
            .on_submit(|s: &mut State, tx: &mut Tx| submit(s, tx));
        let portal = Button::new(t("Abrir o portal do Entra", "Open the Entra portal"))
            .icon(&crate::icons::EXTERNAL_LINK)
            .variant(ButtonVariant::Neutral)
            .on_press_tx(|s: &mut State, tx: &mut Tx| {
                if let Err(e) = tx.open_url(PORTAL) {
                    crate::fail(s, tr!("Não consegui abrir o navegador: {e:?}", "Couldn't open the browser: {e:?}"));
                }
            })
            .build(k);
        inner = inner.child(
            col()
                .key("onedrive-client")
                .w_full()
                .padding(12.0)
                .gap(8.0)
                .radius(k.radius.md)
                .fill(k.c.bg_base)
                .border(1.0, k.c.border_weak)
                .child(row().w_full().center_y().child(text(t("Registro do aplicativo", "App registration")).font(k.ty.text_small_medium).color(k.c.text_base).grow(1.0)).child(portal))
                .child(text(od::registration_help()).font(k.ty.caption()).color(k.c.text_muted).wrap().w_full().min_w(0.0))
                .child(input.build(k)),
        );
    }
    if let Some(e) = &f.error {
        inner = inner.child(text(e.clone()).key("error").font(k.ty.text_small).color(k.c.danger.fg).wrap().w_full().min_w(0.0));
    }
    let actions = row()
        .key("actions")
        .w_full()
        .px(16.0)
        .pt(12.0)
        .pb(16.0)
        .gap(8.0)
        .center_y()
        .child(Button::new(t("Voltar", "Back")).icon(&crate::icons::BACK).variant(ButtonVariant::Neutral).on_press(crate::servers::back_to_pick).build(k))
        .child(rune::el().grow(1.0))
        .child(Button::new(t("Cancelar", "Cancel")).variant(ButtonVariant::Neutral).on_press(close).build(k))
        .child(Button::new(t("Conectar no navegador", "Connect in browser")).variant(ButtonVariant::Submit).on_press_tx(submit).build(k).default_button());
    let body = col().key("onedrive-dialog").w_full().child(inner).child(actions);
    Modal::new("server", t("Conectar OneDrive", "Connect OneDrive")).open(true).child(body).width(560.0).fit().on_close(close).build(k, layer);
}

// ---------------------------------------------------------------------------------------------
// The recycle bin

fn is_business(s: &State, server: &Server) -> bool {
    s.servers.onedrive.store.find(&server.user).is_some_and(|a| a.business)
}

/// Excluir: the items go to OneDrive's recycle bin; on a personal account Desfazer brings
/// them back where they were.
pub fn trash(s: &mut State, loc: &Location, items: Vec<Entry>) {
    let Some((server, _)) = at(loc) else { return };
    let server = server.clone();
    let reload = panes_on(s, &server.id);
    let business = is_business(s, &server);
    let paths: Vec<String> = items.iter().filter_map(|e| path_of(loc, e)).collect();
    let n = paths.len();
    let id = server.user.clone();
    undo::local_job(s, reload, move |n| tr!("mover {} para a lixeira do OneDrive", "move {} to the OneDrive recycle bin", crate::actions::count(n)), move |rec| {
        let a = od::reach(&id)?;
        let (gone, err) = a.trash_many(&paths);
        // On a work or school account Graph can't bring them back: nothing to undo.
        if !business {
            for (item, name) in &gone {
                rec.add([Step::OneDriveRestore { server: server.clone(), id: item.clone(), name: name.clone() }]);
            }
        }
        match err {
            Some(e) if gone.is_empty() => Err(e),
            Some(e) => Err(tr!("{e} (já na lixeira: {})", "{e} (already in the recycle bin: {})", gone.len())),
            None if business => Ok(tr!("Movido para a lixeira do OneDrive: {}. Para restaurar, use a lixeira no site.", "Moved to the OneDrive recycle bin: {}. To restore, use the recycle bin on the website.", crate::actions::count(n))),
            None => Ok(tr!("Movido para a lixeira do OneDrive: {}.", "Moved to the OneDrive recycle bin: {}.", crate::actions::count(n))),
        }
    });
}

/// Desfazer's steps: item `id` of the account `server` stands for, back from the recycle bin
/// or into it again.
pub fn restore_id(server: &Server, id: &str) -> Result<(), String> {
    od::reach(&server.user)?.restore(id)
}

pub fn trash_id(server: &Server, id: &str) -> Result<(), String> {
    od::reach(&server.user)?.trash_id(id)
}

/// What the delete question says happens to the items.
pub fn delete_note(s: &State, loc: &Location) -> Option<&'static str> {
    let (server, _) = at(loc)?;
    Some(if is_business(s, server) {
        t("Os itens vão para a lixeira do OneDrive; numa conta corporativa ou de estudante, restaure pela lixeira no site.", "The items go to the OneDrive recycle bin; on a work or school account, restore them from the recycle bin on the website.")
    } else {
        t("Os itens vão para a lixeira do OneDrive, de onde Desfazer os traz de volta.", "The items go to the OneDrive recycle bin, from where Undo brings them back.")
    })
}

// ---------------------------------------------------------------------------------------------
// Sharing links

/// Copiar link, asked before a link is made: the item opens to anyone with it.
pub struct MakeLink {
    pub server: Server,
    pub path: String,
    pub item: Entry,
}

fn one_item(s: &mut State, p: usize) -> Option<(Server, String, Entry)> {
    let loc = s.panes[p].loc.clone();
    let items = selected(s, p);
    let (Some((server, _)), [item]) = (at(&loc), items.as_slice()) else {
        if items.len() > 1 {
            s.status = t("Selecione um item só.", "Select just one item.").into();
        }
        return None;
    };
    let path = path_of(&loc, item)?;
    Some((server.clone(), path, item.clone()))
}

/// Copiar link: the item's link to the clipboard; with none yet, asks before making one.
pub fn copy_link(s: &mut State, p: usize, _: &mut Tx) {
    let Some((server, path, item)) = one_item(s, p) else { return };
    s.status = tr!("Buscando o link de \"{}\"…", "Getting the link for \"{}\"…", item.name);
    let (sv, at_path, it) = (server.clone(), path.clone(), item.clone());
    task(s, &server, t("copiar o link", "copy the link"), move |a| a.link_of(&path), move |result| Msg::OneDrive(Answer::Link { server: Box::new(sv), path: at_path, item: it, result }));
}

pub fn make_link(s: &mut State) {
    let Some(Dialog::OneDriveLink(m)) = s.dialog.take() else { return };
    let MakeLink { server, path, item } = *m;
    s.status = tr!("Criando o link de \"{}\"…", "Making the link for \"{}\"…", item.name);
    let name = item.name.clone();
    task(s, &server, t("criar o link", "make the link"), move |a| a.create_link(&path).map(Some), move |result| Msg::OneDrive(Answer::Linked { name, result }));
}

/// Remover link: whoever has a link to the item can't open it any more.
pub fn remove_link(s: &mut State, p: usize, _: &mut Tx) {
    let Some((server, path, item)) = one_item(s, p) else { return };
    let name = item.name.clone();
    task(s, &server, t("remover o link", "remove the link"), move |a| a.remove_link(&path).map(|_| None), move |result| Msg::OneDrive(Answer::Linked { name, result }));
}

pub fn link_dialog(k: &Kit, m: &MakeLink, layer: &mut Vec<El>) {
    let note = tr!(
        "\"{}\" ainda não tem link. Criar um link deixa quem o tiver ver e baixar o item (numa conta corporativa, se a organização não permitir links abertos, só pessoas dela). Dá para remover depois em Remover link.",
        "\"{}\" has no link yet. Making one lets whoever has it see and download the item (on a work account whose organization allows no open links, only people in it). You can remove it later with Remove link.",
        m.item.name
    );
    let close = |s: &mut State| s.dialog = None;
    let body = col()
        .key("content")
        .w_full()
        .px(16.0)
        .pb(16.0)
        .gap(12.0)
        .child(text(note).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0))
        .child(
            row()
                .key("actions")
                .w_full()
                .pt(8.0)
                .gap(8.0)
                .justify_end()
                .child(Button::new(t("Cancelar", "Cancel")).variant(ButtonVariant::Neutral).on_press(close).build(k))
                .child(Button::new(t("Criar link e copiar", "Make link and copy")).variant(ButtonVariant::Submit).on_press(make_link).build(k).default_button()),
        );
    Modal::new("onedrive-link", t("Criar link de compartilhamento?", "Make a sharing link?")).open(true).child(body).width(480.0).fit().on_close(close).build(k, layer);
}

// ---------------------------------------------------------------------------------------------
// Storage

/// Asks for the account's storage (unless the first answer is still on its way).
pub fn ask_quota(s: &mut State, server: &Server) {
    match s.servers.onedrive.quotas.get(&server.id) {
        Some(None) => return,
        Some(Some(_)) => {}
        None => {
            s.servers.onedrive.quotas.insert(server.id.clone(), None);
        }
    }
    let id = server.id.clone();
    task(s, server, t("ler a cota", "read the storage quota"), |a| a.quota(), move |result| Msg::OneDrive(Answer::Quota { id, result }));
}

/// What the status bar shows beside a OneDrive location: the words, how full, the details.
pub fn quota_bar(s: &State, loc: &Location) -> Option<(String, Option<f64>, String)> {
    let (server, _) = at(loc)?;
    let q = s.servers.onedrive.quotas.get(&server.id).cloned().flatten()?;
    let label = tr!("{} livres de {}", "{} free of {}", crate::model::format_size(q.remaining.min(q.total.saturating_sub(q.used))), crate::ui::whole_size(q.total));
    let help = tr!(
        "Usado: {} de {}\nNa lixeira: {}",
        "Used: {} of {}\nIn the recycle bin: {}",
        crate::model::format_size(q.used),
        crate::ui::whole_size(q.total),
        crate::model::format_size(q.deleted)
    );
    Some((label, (q.total > 0).then(|| q.used as f64 / q.total as f64), help))
}

// ---------------------------------------------------------------------------------------------
// Changes made elsewhere

/// How often an account a pane shows is asked what changed, in seconds.
pub const EVERY: f64 = 15.0;
const RETRY: f64 = 60.0;

#[derive(Default)]
pub struct Watch {
    next: f64,
    busy: bool,
}

/// Asks each OneDrive account a pane shows what changed (one delta request), once its time
/// has come.
pub fn tick(s: &mut State, now: f64) {
    if s.servers.onedrive.watch.is_empty() && !s.panes.iter().any(|p| at(&p.loc).is_some()) {
        return;
    }
    let mut shown: Vec<Server> = s.panes.iter().filter_map(|p| at(&p.loc).map(|(sv, _)| sv.clone())).collect();
    shown.dedup_by(|a, b| a.id == b.id);
    s.servers.onedrive.watch.retain(|id, _| shown.iter().any(|sv| sv.id == *id));
    for server in shown {
        // Only an account already open: opening one is the listing's job.
        if od::cached(&server.user).is_none() {
            continue;
        }
        let w = s.servers.onedrive.watch.entry(server.id.clone()).or_insert(Watch { next: now + EVERY, busy: false });
        if w.busy || now < w.next {
            continue;
        }
        w.busy = true;
        let id = server.id.clone();
        task(s, &server, t("ver o que mudou no OneDrive", "see what changed on OneDrive"), |a| a.poll(), move |result| Msg::OneDrive(Answer::Changes { id, result }));
    }
}

/// When the next ask is due, for the frame loop to wake then.
pub fn next_ask(s: &State) -> Option<f64> {
    s.servers.onedrive.watch.values().filter(|w| !w.busy).map(|w| w.next).reduce(f64::min)
}

fn changes_arrived(s: &mut State, id: String, result: Result<Changes, String>, now: f64) {
    let Some(w) = s.servers.onedrive.watch.get_mut(&id) else { return };
    w.busy = false;
    let changes = match result {
        Ok(c) => {
            w.next = now + EVERY;
            c
        }
        Err(e) => {
            w.next = now + RETRY;
            openexplorer_gdrive::log(&format!("mudanças do OneDrive {id}: {e}"));
            return;
        }
    };
    if changes.is_empty() {
        return;
    }
    let Some(server) = s.servers.list.iter().find(|sv| sv.id == id).cloned() else { return };
    let Some(account) = od::cached(&server.user) else { return };
    let stale = |path: &[String]| match account.folder_id(&server.path(path)) {
        Some(f) => changes.touches(&f),
        None => changes.all,
    };
    for p in 0..2 {
        let reload = matches!(&s.panes[p].loc, Location::Server { server: sv, path, find: None } if sv.id == id && stale(path));
        if reload && !s.panes[p].loading {
            crate::load(s, p);
        }
    }
    crate::tree::server_folders_changed(s, &id, &stale);
    ask_quota(s, &server);
}

/// A OneDrive address (`onedrive://ana@exemplo.com/Meus arquivos/…`): the e-mail is the whole
/// authority, and only reaches an account already connected.
pub fn address(rest: &str) -> Option<(String, String)> {
    let (email, path) = rest.split_once('/').map_or((rest, ""), |(e, p)| (e, p));
    let email = email.trim().to_lowercase();
    (!email.is_empty()).then(|| (email, format!("/{}", path.trim_start_matches('/'))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_path(path: &[&str]) -> Location {
        folder(&Server::onedrive("d1", "ana@exemplo.com"), path.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn the_top_takes_nothing_and_my_files_does() {
        assert!(why_not(&at_path(&[])).is_some() && items_fixed(&at_path(&[])));
        assert!(why_not(&at_path(&["Meus arquivos"])).is_none() && !items_fixed(&at_path(&["Meus arquivos"])));
        assert!(why_not(&Location::home()).is_none() && at(&Location::home()).is_none());
    }

    #[test]
    fn accounts_show_as_servers_by_their_email() {
        let sv = Server::onedrive("b!Xyz", "ana@exemplo.com");
        assert!(sv.is_onedrive() && !sv.is_mega());
        assert_eq!(sv.id, "onedrive:b!Xyz");
        assert_eq!(sv.address(), "onedrive://ana@exemplo.com");
        assert_eq!(files(&sv), folder(&sv, vec!["Meus arquivos".into()]));
        assert_eq!(address("Ana@Exemplo.com/Meus arquivos/Fotos"), Some(("ana@exemplo.com".into(), "/Meus arquivos/Fotos".into())));
        assert_eq!(address("ana@exemplo.com"), Some(("ana@exemplo.com".into(), "/".into())));
    }
}
