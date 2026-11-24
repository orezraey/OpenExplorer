//! OneDrive accounts (personal and work or school) in the panes. Each account shows in the tree
//! under Contas, beside the Google Drive and MEGA accounts and the servers, and opens like a
//! server (see `servers.rs`): its top holds Meus arquivos, and on a work or school account the
//! Lixeira. Listing, copying to and from anywhere, moving, renaming, new folders, searching and
//! dragging go through the servers' code, with the account as the connection
//! (`openexplorer_remote`'s `onedrive.rs`). What only OneDrive has lives here: signing in
//! through the browser, deleting to its recycle bin (undone by restoring), the Lixeira with
//! Restaurar, Excluir permanentemente and Esvaziar, sharing links, the storage in the status bar,
//! and folders that refresh when something changes elsewhere.
//!
//! Graph has no way to list a personal account's recycle bin, so it shows no Lixeira: deleted
//! items come back with Desfazer or from the website. A work or school account's comes through
//! SharePoint (see `openexplorer_onedrive::bin`), once its permission is granted: until then the
//! Lixeira says what to add to the app registration and offers to authorize it in the browser.

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
    /// The list as the store keeps it, read once: dialogs ask it every frame.
    pub accounts: Vec<StoredAccount>,
    /// Each account's storage, by server id: `None` while the answer is on its way.
    pub quotas: HashMap<String, Option<Quota>>,
    /// When each account a pane shows is next asked what changed.
    pub watch: HashMap<String, Watch>,
    /// The sign-in waiting for the browser, by number.
    pub login: Option<u64>,
}

impl Side {
    pub fn load() -> Side {
        let store = od::Store::at(openexplorer_gdrive::config_dir().join("onedrive-accounts.json"));
        Side { accounts: store.accounts(), store, quotas: HashMap::new(), watch: HashMap::new(), login: None }
    }

    /// The servers the connected accounts show as.
    pub fn servers(&self) -> Vec<Server> {
        self.accounts.iter().map(server_of).collect()
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

/// Where a OneDrive location doesn't take new items: the account's top, and the Lixeira. `None`
/// where it does (or it isn't OneDrive).
pub fn why_not(loc: &Location) -> Option<String> {
    let (_, path) = at(loc)?;
    if path.is_empty() {
        return Some(t("O topo da conta OneDrive só tem as pastas dela: abra uma.", "The OneDrive account's top only holds its folders: open one.").into());
    }
    in_bin(loc).then(|| od::bin::fixed().into())
}

fn is_bin(path: &[String]) -> bool {
    path.first().and_then(|p| od::Area::of(p)) == Some(od::Area::Bin)
}

/// Whether `loc` is in a OneDrive account's Lixeira.
pub fn in_bin(loc: &Location) -> bool {
    at(loc).is_some_and(|(_, path)| is_bin(path))
}

/// The Lixeira itself, where Restaurar, Excluir permanentemente and Esvaziar apply.
pub fn bin_top(loc: &Location) -> bool {
    matches!(loc, Location::Server { server, path, find: None } if server.is_onedrive() && path.len() == 1 && is_bin(path))
}

/// Whether the tree's node for `path` of `server` is a OneDrive Lixeira: its own icon, and no
/// arrow (folders in it don't open).
pub fn bin_node(server: &Server, path: &[String]) -> bool {
    server.is_onedrive() && path.len() == 1 && is_bin(path)
}

/// What the Excluir permanentemente question says in the Lixeira; `None` elsewhere (the
/// servers' words apply).
pub fn purge_note(loc: &Location) -> Option<&'static str> {
    in_bin(loc).then(|| t("Os itens serão apagados para sempre da lixeira do OneDrive. Isso não pode ser desfeito.", "The items will be deleted from the OneDrive recycle bin for good. This can't be undone."))
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

fn entry_of(i: od::Item) -> Entry {
    let modified = i.modified.filter(|t| *t > 0).map(|t| std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(t as u64));
    crate::servers::entry(openexplorer_remote::Item { name: i.name, is_dir: i.is_dir, size: i.size, modified, link: false })
}

/// Lists pane `p`'s folder in Meus arquivos on a thread: what the kept tree holds shows at once,
/// then the folder as the delta feed brings it (no request while the change watcher keeps the
/// tree current), and then the folders inside it are read ahead, so opening one is immediate.
/// `false` where the servers' listing does it (searches, the Lixeira, the account's top).
pub fn load(s: &mut State, p: usize, request: u64) -> bool {
    let Location::Server { server, path, find: None } = s.panes[p].loc.clone() else { return false };
    if path.first().and_then(|f| od::Area::of(f)) != Some(od::Area::Files) {
        return false;
    }
    let show_hidden = s.settings.show_hidden;
    let pane = &mut s.panes[p];
    pane.loading = true;
    pane.error = None;
    pane.free = None;
    background(s, move |emit| {
        let listed = |part, result| Msg::Listed { pane: p, request, part, result, at: None };
        let shown = |items: Vec<od::Item>| items.into_iter().filter(|i| show_hidden || !i.name.starts_with('.')).map(entry_of).collect::<Vec<_>>();
        let dir = server.path(&path);
        let account = match catch_panic(t("abrir o OneDrive", "open OneDrive"), || od::reach(&server.user)) {
            Ok(a) => a,
            Err(e) => return emit(listed(crate::Part::WHOLE, Err(e))),
        };
        let kept = account.kept_list(&dir);
        if let Some(items) = &kept {
            emit(listed(crate::Part::Cached, Ok(shown(items.clone()))));
        }
        match catch_panic(t("listar", "list"), || account.list(&dir)) {
            Ok(items) => emit(listed(crate::Part::WHOLE, Ok(shown(items)))),
            // The kept listing stays on screen; the error is said.
            Err(e) if kept.is_some() => emit(listed(crate::Part::Page { first: false, more: false }, Err(e))),
            Err(e) => return emit(listed(crate::Part::WHOLE, Err(e))),
        }
        let _ = catch_panic(t("ler as pastas adiante", "read folders ahead"), || {
            account.prefetch(&dir);
            Ok(())
        });
    });
    true
}

/// Atualizar: the folder pane `p` shows is read whole at its next opening, and the delta feed
/// asked, whatever the tree holds.
pub fn forget_shown(s: &State, p: usize) {
    if let Some((server, path)) = at(&s.panes[p].loc)
        && let Some(a) = od::cached(&server.user)
    {
        a.forget(&server.path(path));
    }
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
    /// The sign-in that grants SharePoint for account `id` (its Lixeira) ended.
    BinGranted { id: String, result: Result<(), String> },
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
        Answer::BinGranted { id, result } => match result {
            Ok(()) => {
                s.status = t("Lixeira autorizada.", "Recycle bin authorized.").into();
                for p in 0..s.panes.len() {
                    if matches!(&s.panes[p].loc, Location::Server { server, .. } if server.id == id) {
                        crate::load(s, p);
                    }
                }
            }
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
    match &mut s.account_form {
        Some(f) => &mut f.client_id,
        None => &mut s.name_edit,
    }
}

/// Conectar no navegador: the registration typed (kept for next time) or saved, then the
/// browser opens Microsoft's sign-in and a thread waits for it to come back.
pub fn submit(s: &mut State, tx: &mut Tx) {
    let Some(f) = &mut s.account_form else { return };
    f.error = None;
    let typed = f.client_id.text.trim().to_string();
    // Pessoal or For Business picks who signs in, also with a Client ID typed here; a
    // registration that names its own tenant keeps it.
    let tenant_override = f.onedrive_tenant;
    let mut cfg = if !typed.is_empty() {
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
    if let Some(tenant) = tenant_override
        && cfg.tenant == "common"
    {
        cfg.tenant = tenant.into();
    }
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
    s.close_account_form();
    s.status = t("Faça o login no navegador que abriu. Esperando a Microsoft…", "Sign in in the browser that opened. Waiting for Microsoft…").into();
    s.notify_main();
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
        s.notify_account();
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
    s.servers.onedrive.accounts = s.servers.onedrive.store.accounts();
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
    s.servers.onedrive.accounts.retain(|a| a.id != server.user);
    s.servers.onedrive.quotas.remove(&server.id);
    s.servers.onedrive.watch.remove(&server.id);
    true
}

/// The OneDrive step of Registrar conta: what's needed, then the browser.
pub fn configure(s: &State, k: &Kit, f: &Form, banner: El, close: fn(&mut State, &mut Tx)) -> (El, El) {
    let muted = |words: String| text(words).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0);
    let intro = match f.onedrive_tenant {
        Some("consumers") => t(
            "Ao continuar, o navegador abre a página da Microsoft para entrar com sua conta pessoal e autorizar o acesso aos seus arquivos do OneDrive.",
            "When you continue, the browser opens Microsoft's page to sign in with your personal account and allow access to your OneDrive files.",
        ),
        Some("organizations") => t(
            "Ao continuar, o navegador abre a página da Microsoft para entrar com sua conta corporativa ou de estudante (OneDrive for Business) e autorizar o acesso.",
            "When you continue, the browser opens Microsoft's page to sign in with your work or school account (OneDrive for Business) and allow access.",
        ),
        _ => t(
            "Ao continuar, o navegador abre a página da Microsoft para você escolher a conta (pessoal, corporativa ou de estudante) e autorizar o acesso aos seus arquivos do OneDrive.",
            "When you continue, the browser opens Microsoft's page so you can pick the account (personal, work or school) and allow access to your OneDrive files.",
        ),
    };
    let mut inner = col().key("onedrive-content").w_full().gap(12.0).child(banner).child(muted(intro.into()));
    let connected: Vec<&str> = s.servers.onedrive.accounts.iter().map(|a| a.email.as_str()).collect();
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
        .gap(8.0)
        .center_y()
        .child(Button::new(t("Voltar", "Back")).icon(&crate::icons::BACK).variant(ButtonVariant::Neutral).on_press(crate::servers::back_to_pick).build(k))
        .child(rune::el().grow(1.0))
        .child(Button::new(t("Cancelar", "Cancel")).variant(ButtonVariant::Neutral).on_press_tx(close).build(k))
        .child(Button::new(t("Conectar no navegador", "Connect in browser")).variant(ButtonVariant::Submit).on_press_tx(submit).build(k).default_button());
    (inner, actions)
}

// ---------------------------------------------------------------------------------------------
// The recycle bin

fn is_business(s: &State, server: &Server) -> bool {
    s.servers.onedrive.accounts.iter().any(|a| a.id == server.user && a.business)
}

/// Excluir: the items go to OneDrive's recycle bin, from where Desfazer brings them back where
/// they were (on a work or school account, through the Lixeira's SharePoint permission).
pub fn trash(s: &mut State, loc: &Location, items: Vec<Entry>) {
    let Some((server, _)) = at(loc) else { return };
    let server = server.clone();
    let reload = panes_on(s, &server.id);
    let paths: Vec<String> = items.iter().filter_map(|e| path_of(loc, e)).collect();
    let n = paths.len();
    let id = server.user.clone();
    undo::local_job(s, reload, move |n| tr!("mover {} para a lixeira do OneDrive", "move {} to the OneDrive recycle bin", crate::actions::count(n)), move |rec| {
        let a = od::reach(&id)?;
        let (gone, err) = a.trash_many(&paths);
        // Only what can come back is undone: on a work account, what was found in the bin.
        let back: Vec<&(String, String)> = gone.iter().filter(|(item, _)| a.can_restore(item)).collect();
        for (item, name) in &back {
            rec.add([Step::OneDriveRestore { server: server.clone(), id: item.clone(), name: name.clone() }]);
        }
        let stuck = gone.len() - back.len();
        match err {
            Some(e) if gone.is_empty() => Err(e),
            Some(e) => Err(tr!("{e} (já na lixeira: {})", "{e} (already in the recycle bin: {})", gone.len())),
            None if stuck > 0 => Ok(tr!(
                "Movido para a lixeira do OneDrive: {}. Desfazer não traz de volta {}: autorize a Lixeira da conta, ou restaure pelo site.",
                "Moved to the OneDrive recycle bin: {}. Undo can't bring back {}: authorize the account's recycle bin, or restore on the website.",
                crate::actions::count(n),
                crate::actions::count(stuck)
            )),
            None => Ok(tr!("Movido para a lixeira do OneDrive: {}.", "Moved to the OneDrive recycle bin: {}.", crate::actions::count(n))),
        }
    });
}

/// The names the selected items have in the Lixeira (the names shown: twins have their own).
fn bin_names(items: &[Entry]) -> Vec<String> {
    items.iter().map(|e| e.name.clone()).collect()
}

/// Restaurar in the Lixeira: each item goes back to the folder it was deleted from.
pub fn restore(s: &mut State, p: usize) {
    let loc = s.panes[p].loc.clone();
    let items = selected(s, p);
    let (Some((server, _)), false) = (at(&loc), items.is_empty()) else { return };
    let server = server.clone();
    let reload = panes_on(s, &server.id);
    let names = bin_names(&items);
    let n = names.len();
    task(
        s,
        &server,
        t("restaurar da lixeira do OneDrive", "restore from the OneDrive recycle bin"),
        move |a| match a.bin_restore(&names) {
            (done, None) => Ok(tr!("Restaurado: {}.", "Restored: {}.", crate::actions::count(done.len()))),
            (done, Some(e)) if done.is_empty() => Err(e),
            (done, Some(e)) => Err(tr!("{e} (já restaurados: {})", "{e} (already restored: {})", done.len())),
        },
        move |result| Msg::Done { reload, result },
    );
    s.status = tr!("Restaurando {}…", "Restoring {}…", crate::actions::count(n));
}

/// Excluir permanentemente in the Lixeira, confirmed: the items are deleted for good.
pub fn purge(s: &mut State, loc: &Location, items: Vec<Entry>) {
    let Some((server, _)) = at(loc) else { return };
    if !in_bin(loc) {
        return;
    }
    let server = server.clone();
    let reload = panes_on(s, &server.id);
    let names = bin_names(&items);
    task(
        s,
        &server,
        t("excluir da lixeira do OneDrive", "delete from the OneDrive recycle bin"),
        move |a| match a.bin_purge(&names) {
            (done, None) => Ok(tr!("Excluído permanentemente: {}.", "Deleted forever: {}.", crate::actions::count(done.len()))),
            (done, Some(e)) if done.is_empty() => Err(e),
            (done, Some(e)) => Err(tr!("{e} (já excluídos para sempre: {})", "{e} (already deleted forever: {})", done.len())),
        },
        move |result| Msg::Done { reload, result },
    );
}

/// Esvaziar lixeira, confirmed: everything in it, as read now, deleted for good.
pub fn empty_bin(s: &mut State, id: &str) {
    let Some(server) = s.servers.list.iter().find(|sv| sv.id == id).cloned() else { return };
    let reload = panes_on(s, &server.id);
    task(
        s,
        &server,
        t("esvaziar a lixeira do OneDrive", "empty the OneDrive recycle bin"),
        |a| a.bin_empty().map(|n| tr!("Lixeira do OneDrive esvaziada: {}.", "OneDrive recycle bin emptied: {}.", crate::actions::count(n))),
        move |result| Msg::Done { reload, result },
    );
    s.status = t("Esvaziando a lixeira do OneDrive…", "Emptying the OneDrive recycle bin…").into();
}

/// Autorizar no navegador: the sign-in that grants the app SharePoint for the account (what
/// the registration lists for it), then the Lixeira read again.
pub fn authorize_bin(s: &mut State, p: usize, tx: &mut Tx) {
    let Some((server, _)) = at(&s.panes[p].loc).map(|(sv, path)| (sv.clone(), path.to_vec())) else { return };
    // The account is open: the Lixeira just asked SharePoint, and found the site.
    let Some(account) = od::cached(&server.user) else { return };
    let login = match account.bin_grant() {
        Ok(l) => l,
        Err(e) => return crate::fail(s, e),
    };
    if let Err(e) = tx.open_url(&login.url) {
        return crate::fail(s, tr!("Não consegui abrir o navegador: {e:?}", "Couldn't open the browser: {e:?}"));
    }
    s.status = t("Autorize no navegador que abriu. Esperando a Microsoft…", "Authorize in the browser that opened. Waiting for Microsoft…").into();
    let id = server.id.clone();
    background(s, move |emit| {
        let result = catch_panic(t("autorizar a lixeira do OneDrive", "authorize the OneDrive recycle bin"), || account.bin_granted(&login));
        emit(Msg::OneDrive(Answer::BinGranted { id, result }));
    });
}

/// What the Lixeira shows when SharePoint's permission is missing, in place of the error: what to
/// add to the registration, Microsoft's own words, and the button that grants it.
pub fn bin_notice(s: &State, k: &Kit, p: usize) -> Option<El> {
    let loc = &s.panes[p].loc;
    let (server, _) = at(loc)?;
    if !bin_top(loc) || s.panes[p].error.is_none() {
        return None;
    }
    let od::BinAccess::NeedsConsent(why) = od::bin::access(&server.user) else { return None };
    let small = |words: String, color| text(words).font(k.ty.text_small).color(color).wrap().w_full().min_w(0.0);
    let portal = Button::new(t("Abrir o portal do Entra", "Open the Entra portal")).icon(&crate::icons::EXTERNAL_LINK).variant(ButtonVariant::Neutral).on_press_tx(|s: &mut State, tx: &mut Tx| {
        if let Err(e) = tx.open_url(PORTAL) {
            crate::fail(s, tr!("Não consegui abrir o navegador: {e:?}", "Couldn't open the browser: {e:?}"));
        }
    });
    let grant = Button::new(t("Autorizar no navegador", "Authorize in browser")).variant(ButtonVariant::Submit).on_press_tx(move |s: &mut State, tx: &mut Tx| authorize_bin(s, p, tx));
    // As wide as the pane shows, not the columns (which may run past it): the words wrap.
    let shown_w = s.views[p].rect[2];
    Some(
        col()
            .key("onedrive-bin-consent")
            .w_full()
            .max_w(if shown_w > 0.0 { shown_w.max(320.0) } else { 460.0 })
            .grow(1.0)
            .padding(16.0)
            .gap(12.0)
            .child(text(t("A Lixeira desta conta precisa de autorização", "This account's recycle bin needs authorization")).font(k.ty.text_small_medium).color(k.c.text_base))
            .child(small(od::bin::consent_help(), k.c.text_muted))
            .children((!why.is_empty()).then(|| small(tr!("A Microsoft disse: {why}", "Microsoft said: {why}"), k.c.text_faint)))
            .child(row().gap(8.0).child(grant.build(k)).child(portal.build(k))),
    )
}

/// The Lixeira's Data and Tipo columns: when each item was deleted, and where from.
pub fn bin_columns(loc: &Location) -> Option<(&'static str, &'static str)> {
    bin_top(loc).then(|| (t("Excluído em", "Date deleted"), t("Local original", "Original location")))
}

/// Where a Lixeira item was deleted from, for its Local original column.
pub fn origin_of(loc: &Location, e: &Entry) -> Option<String> {
    let (server, _) = at(loc)?;
    if !bin_top(loc) {
        return None;
    }
    let b = od::bin::shown_item(&server.user, &e.name)?;
    Some(if b.second_stage { tr!("{} (2º estágio)", "{} (second stage)", b.origin) } else { b.origin })
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
        t(
            "Os itens vão para a lixeira do OneDrive, de onde Desfazer ou a Lixeira da conta os trazem de volta (depois de autorizada).",
            "The items go to the OneDrive recycle bin, from where Undo or the account's recycle bin brings them back (once authorized).",
        )
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

/// The scenes `onedrive-bin` (`granted`) and `onedrive-bin-consent`: a work account's Lixeira
/// in the right pane, without a network.
pub fn scene_bin(s: &mut State, granted: bool) {
    let drive = "b!trabalho";
    let account = StoredAccount { id: drive.into(), email: "voce@contoso.com".into(), name: "Você".into(), business: true };
    let sv = server_of(&account);
    s.servers.onedrive.accounts.push(account);
    s.servers.list.push(sv.clone());
    s.servers.onedrive.quotas.insert(sv.id.clone(), Some(Quota { total: 1_099_511_627_776, used: 214_748_364_800, remaining: 884_763_262_976, deleted: 52_428_800, state: "normal".into() }));
    s.panes[1].go(folder(&sv, vec![od::Area::Bin.label().to_string()]));
    s.active = 1;
    // Narrow columns, so Excluído em and Local original show beside the tree.
    s.layout.columns[1] = [190.0, 70.0, 120.0, 210.0];
    s.layout.tree[1] = 120.0;
    if !granted {
        od::bin::show(drive, Vec::new(), od::BinAccess::NeedsConsent("AADSTS65001: The user or administrator has not consented to use the application with ID '0b1c2d3e-4f50-6172-8394-a5b6c7d8e9f0'.".into()));
        s.panes[1].set_entries(Err(od::bin::consent_help()));
        return;
    }
    let day = 86_400;
    let now = 1_795_000_000;
    let item = |name: &str, origin: &str, ago: i64, size: u64, is_dir: bool, second_stage: bool| od::BinItem {
        id: format!("{:08x}-0000-4000-8000-000000000000", ago),
        name: name.into(),
        leaf: name.into(),
        dir: String::new(),
        origin: origin.into(),
        deleted: now - ago,
        size,
        is_dir,
        second_stage,
    };
    let items = vec![
        item("Relatório trimestral.docx", "Meus arquivos/Trabalho", 3_600, 284_311, false, false),
        item("Fotos do evento", "Meus arquivos", day, 0, true, false),
        item("orçamento.xlsx", "Meus arquivos/Trabalho/2026", 2 * day, 51_200, false, false),
        item("orçamento (2).xlsx", "Meus arquivos/Trabalho", 9 * day, 48_930, false, false),
        item("notas.txt", "Meus arquivos/Pessoal", 40 * day, 1_204, false, true),
    ];
    let entries = items
        .iter()
        .map(|b| Entry {
            id: b.name.clone(),
            name: b.name.clone(),
            is_dir: b.is_dir,
            size: (!b.is_dir).then_some(b.size),
            modified: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(b.deleted as u64)),
            target: None,
            mime: None,
            md5: None,
            starred: false,
        })
        .collect();
    od::bin::show(drive, items, od::BinAccess::Ready);
    s.panes[1].set_entries(Ok(entries));
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
    fn the_lixeira_restores_and_purges_but_takes_nothing() {
        assert!(bin_top(&at_path(&["Lixeira"])) && bin_top(&at_path(&["Recycle bin"])));
        assert!(!bin_top(&at_path(&["Lixeira", "Fotos"])) && in_bin(&at_path(&["Lixeira", "Fotos"])));
        assert!(!in_bin(&at_path(&["Meus arquivos", "Lixeira"])));
        assert!(why_not(&at_path(&["Lixeira"])).is_some() && why_not(&at_path(&["Lixeira", "Fotos"])).is_some());
        assert!(!items_fixed(&at_path(&["Lixeira"])));
        assert!(bin_columns(&at_path(&["Lixeira"])).is_some() && bin_columns(&at_path(&["Meus arquivos"])).is_none());
        let sv = Server::onedrive("d1", "ana@exemplo.com");
        assert!(bin_node(&sv, &["Lixeira".into()]) && !bin_node(&sv, &["Meus arquivos".into()]) && !bin_node(&Server::mega("a@b.c"), &["Lixeira".into()]));
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
