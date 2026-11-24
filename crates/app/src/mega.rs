//! MEGA accounts in the panes. Each account shows in the tree under Contas, beside the Google
//! Drive accounts and the servers, and opens like a server (see `servers.rs`): its top holds
//! Drive na nuvem, Lixeira and Compartilhados comigo. Listing, copying to and from anywhere,
//! moving, renaming, new folders, searching and dragging go through the servers' code, with the
//! account as the connection (`openexplorer_remote`'s `mega.rs`). What only MEGA has lives here:
//! adding an account (with its two-factor code), the Rubbish Bin and Restaurar, public links,
//! the storage in the status bar, and folders that refresh when something changes elsewhere.

use openexplorer_i18n::{t, tr};
use openexplorer_mega::{self as mega, Account, Area, Changes, Node, Refused};
use openexplorer_remote::Server;
use rune::{El, Tx, col, row, text};
use rune_kit::{Button, ButtonVariant, Dialog as Modal, Kit};

use crate::actions::{Dialog, selected};
use crate::model::{Entry, Location};
use crate::servers::{folder, path_of};
use crate::undo::{self, Step};
use crate::{Msg, State, background, catch_panic};

/// The accounts' list, beside the app's other settings.
pub fn store() -> mega::Store {
    mega::Store::at(openexplorer_gdrive::config_dir().join("mega-accounts.json"))
}

/// The servers the connected accounts show as.
pub fn servers(store: &mega::Store) -> Vec<Server> {
    store.accounts().into_iter().map(|a| Server::mega(&a.email)).collect()
}

/// The MEGA account a location is in, and the path below its top.
pub fn at(loc: &Location) -> Option<(&Server, &[String])> {
    match loc {
        Location::Server { server, path, .. } if server.is_mega() => Some((server, path)),
        _ => None,
    }
}

/// The area a path below an account's top starts in.
pub fn area_of(path: &[String]) -> Option<Area> {
    let first = path.first()?;
    Area::ALL.into_iter().find(|a| {
        let both: [&str; 2] = match a {
            Area::Cloud => ["Drive na nuvem", "Cloud Drive"],
            Area::Rubbish => ["Lixeira", "Rubbish Bin"],
            Area::Incoming => ["Compartilhados comigo", "Incoming shares"],
        };
        both.contains(&first.as_str())
    })
}

/// Whether `loc` is in a MEGA account's Rubbish Bin (deleting there is for good).
pub fn in_bin(loc: &Location) -> bool {
    at(loc).is_some_and(|(_, path)| area_of(path) == Some(Area::Rubbish))
}

/// Whether deleting in `loc` is for good: in the Rubbish Bin, and inside a folder shared with
/// the account (what's there belongs to whoever shares it).
pub fn deletes_for_good(loc: &Location) -> bool {
    at(loc).is_some_and(|(_, path)| match area_of(path) {
        Some(Area::Rubbish) => true,
        Some(Area::Incoming) => path.len() >= 2,
        _ => false,
    })
}

/// The Rubbish Bin itself, where Restaurar, Excluir permanentemente and Esvaziar apply.
pub fn bin_top(loc: &Location) -> bool {
    matches!(loc, Location::Server { server, path, find: None } if server.is_mega() && path.len() == 1 && area_of(path) == Some(Area::Rubbish))
}

/// Where a MEGA location doesn't take new items: the account's top, Compartilhados comigo
/// itself, and the Rubbish Bin. `None` where it does (or it isn't MEGA).
pub fn why_not(loc: &Location) -> Option<String> {
    let (_, path) = at(loc)?;
    match (area_of(path), path.len()) {
        (_, 0) => Some(t("O topo da conta MEGA só tem Drive na nuvem, Lixeira e Compartilhados comigo: abra uma delas.", "The MEGA account's top only holds Cloud Drive, Rubbish Bin and Incoming shares: open one of them.").into()),
        (Some(Area::Incoming), 1) => Some(t("Compartilhados comigo lista as pastas que outras pessoas compartilham: abra uma delas.", "Incoming shares lists the folders other people share: open one of them.").into()),
        (Some(Area::Rubbish), _) => Some(t("A Lixeira do MEGA não recebe itens: exclua para mandar algo para ela, ou use Restaurar.", "The MEGA Rubbish Bin doesn't take items: delete something to send it there, or use Restore.").into()),
        _ => None,
    }
}

/// What the Excluir question says of where the items go.
pub fn delete_note() -> &'static str {
    t("Os itens vão para a Lixeira do MEGA, de onde podem ser restaurados.", "The items go to the MEGA Rubbish Bin, where they can be restored.")
}

/// What the Excluir permanentemente question says in `loc`: in the Rubbish Bin, or in a folder
/// shared with the account (deleting there is for good too).
pub fn purge_note(loc: &Location) -> &'static str {
    if in_bin(loc) {
        t("Os itens serão apagados para sempre da Lixeira do MEGA. Isso não pode ser desfeito.", "The items will be deleted from the MEGA Rubbish Bin for good. This can't be undone.")
    } else {
        t(
            "Itens de uma pasta compartilhada com você pertencem a quem compartilhou: serão apagados para sempre, sem passar pela Lixeira. Isso não pode ser desfeito.",
            "Items in a folder shared with you belong to whoever shared it: they'll be deleted for good, skipping the Rubbish Bin. This can't be undone.",
        )
    }
}

/// Whether the items `loc` lists can be renamed, cut or deleted: not the three areas, nor the
/// shared folders themselves.
pub fn items_fixed(loc: &Location) -> bool {
    matches!(at(loc), Some((_, path)) if path.is_empty() || (path.len() == 1 && area_of(path) == Some(Area::Incoming)))
}

/// Cloud Drive's top, where a new account opens.
pub fn cloud(server: &Server) -> Location {
    folder(server, vec![Area::Cloud.label().to_string()])
}

/// The panes' places on account `id`, reloaded after a change there.
fn panes_on(s: &State, id: &str) -> Vec<Location> {
    s.panes.iter().map(|p| p.loc.clone()).filter(|l| matches!(l, Location::Server { server, .. } if server.id == id)).collect()
}

/// Runs `work` with the account on a thread, then `done` with the result.
fn task<T: Send + 'static>(s: &State, server: &Server, doing: &'static str, work: impl FnOnce(&Account) -> Result<T, String> + Send + 'static, done: impl FnOnce(Result<T, String>) -> Msg + Send + 'static) {
    let email = server.user.clone();
    background(s, move |emit| {
        let result = catch_panic(doing, || mega::reach(&email).and_then(|a| work(&a)));
        emit(done(result));
    });
}

/// What background work on MEGA brought back.
pub enum Answer {
    /// The Adicionar conta dialog's login `attempt` ended.
    Connected { attempt: u64, result: Result<String, Refused> },
    Quota { id: String, result: Result<mega::Quota, String> },
    /// What changed in account `id` since it was last asked.
    Changes { id: String, result: Result<Changes, String> },
    /// What Copiar link found: the item's link, or `None` when it has none yet.
    Link { server: Box<Server>, path: String, item: Entry, result: Result<Option<String>, String> },
    /// A link made (`made`) or removed for the item named `name`.
    Linked { name: String, result: Result<Option<String>, String> },
}

pub fn arrived(s: &mut State, a: Answer, tx: &mut Tx, now: f64) {
    match a {
        Answer::Connected { attempt, result } => connected(s, attempt, result),
        Answer::Quota { id, result } => match result {
            Ok(q) => {
                s.servers.mega_quotas.insert(id, Some(q));
            }
            Err(e) => openexplorer_gdrive::log(&format!("cota do MEGA {id}: {e}")),
        },
        Answer::Changes { id, result } => changes_arrived(s, id, result, now),
        Answer::Link { server, path, item, result } => match result {
            Ok(Some(link)) => {
                tx.copy(link);
                s.status = tr!("Link copiado: qualquer pessoa com o link pode abrir \"{}\".", "Link copied: anyone with the link can open \"{}\".", item.name);
            }
            Ok(None) if item.is_dir => s.status = folder_link_note().into(),
            Ok(None) => s.dialog = Some(Dialog::MegaLink(Box::new(MakeLink { server: *server, path, item }))),
            Err(e) => crate::fail(s, e),
        },
        Answer::Linked { name, result } => match result {
            Ok(Some(link)) => {
                tx.copy(link);
                s.status = tr!("Link criado e copiado: qualquer pessoa com o link pode abrir \"{name}\".", "Link made and copied: anyone with the link can open \"{name}\".");
            }
            Ok(None) => s.status = tr!("Link removido: \"{name}\" não abre mais pelo link.", "Link removed: \"{name}\" no longer opens from the link."),
            Err(e) => crate::fail(s, e),
        },
    }
}

fn folder_link_note() -> &'static str {
    t(
        "Esta pasta não tem link público. Links de pasta são criados no site do MEGA; depois o Open Explorer copia e remove o link.",
        "This folder has no public link. Folder links are made on the MEGA website; then Open Explorer copies and removes the link.",
    )
}

// ---------------------------------------------------------------------------------------------
// Adding and removing an account

/// Conectar in the Adicionar conta dialog with MEGA picked: logs in on a thread.
pub fn submit(s: &mut State) {
    let Some(f) = &mut s.account_form else { return };
    if f.checking.is_some() {
        return;
    }
    let email = f.user.text.trim().to_string();
    let password = f.password.text.clone();
    let code = f.code.text.trim().to_string();
    if email.is_empty() || !email.contains('@') {
        f.error = Some(t("Digite o e-mail da conta MEGA.", "Enter the MEGA account's e-mail.").into());
        return;
    }
    if password.is_empty() {
        f.error = Some(t("Digite a senha da conta MEGA.", "Enter the MEGA account's password.").into());
        return;
    }
    if f.needs_code && code.is_empty() {
        f.error = Some(t("Digite o código de 6 dígitos do app autenticador.", "Enter the 6-digit code from the authenticator app.").into());
        return;
    }
    s.servers.attempts += 1;
    let attempt = s.servers.attempts;
    f.checking = Some(attempt);
    f.error = None;
    background(s, move |emit| {
        let code = (!code.is_empty()).then_some(code);
        let result = std::panic::catch_unwind(|| mega::connect(&email, &password, code.as_deref()).map(|a| a.email.clone()))
            .unwrap_or_else(|_| Err(Refused::Failed(t("Erro interno ao conectar ao MEGA. Detalhes no log.", "Internal error connecting to MEGA. Details in the log.").into())));
        emit(Msg::Mega(Answer::Connected { attempt, result }));
    });
}

fn connected(s: &mut State, attempt: u64, result: Result<String, Refused>) {
    let Some(f) = &mut s.account_form else { return };
    if f.checking != Some(attempt) {
        return;
    }
    f.checking = None;
    let email = match result {
        Ok(e) => e,
        Err(Refused::NeedsCode) => {
            f.needs_code = true;
            f.code = rune::TextEdit::new("");
            f.code.focused = true;
            f.error = Some(Refused::NeedsCode.text());
            s.notify_account();
            return;
        }
        Err(r) => {
            openexplorer_gdrive::log(&format!("falha ao conectar MEGA: {}", r.text()));
            if r == Refused::WrongCode {
                f.code = rune::TextEdit::new("");
                f.code.focused = true;
            }
            f.error = Some(r.text());
            s.notify_account();
            return;
        }
    };
    if let Err(e) = s.servers.mega.add(&email) {
        f.error = Some(e);
        s.notify_account();
        return;
    }
    openexplorer_gdrive::log(&format!("conta MEGA conectada: {email}"));
    s.close_account_form();
    let server = Server::mega(&email);
    if !s.servers.list.iter().any(|sv| sv.id == server.id) {
        s.servers.list.push(server.clone());
    }
    // Connections kept from an earlier session of this account (one that ended) go.
    s.servers.pool.forget(&server.id);
    s.tree.server_changed(&server.id);
    s.tree.server_opened(&server.id);
    s.status = tr!("Conta MEGA conectada: {email}", "MEGA account connected: {email}");
    let p = s.active;
    crate::navigate(s, p, cloud(&server));
}

/// Remover conta: the session ends on MEGA's side and leaves the credential store. Whether the
/// account left the list.
pub fn remove(s: &mut State, server: &Server) -> bool {
    if let Err(e) = s.servers.mega.remove(&server.user) {
        crate::fail(s, e);
        return false;
    }
    mega::disconnect(&server.user);
    s.servers.mega_quotas.remove(&server.id);
    s.servers.mega_watch.remove(&server.id);
    true
}

// ---------------------------------------------------------------------------------------------
// The Rubbish Bin

/// The node a listed item is, checked to still be what the pane shows (a twin renamed on the
/// website could otherwise take its name).
fn node_of(a: &Account, loc: &Location, e: &Entry) -> Result<Node, String> {
    let path = path_of(loc, e).ok_or(t("Isso não é um item do MEGA.", "That isn't a MEGA item."))?;
    let n = a.node(&path)?;
    let same = n.kind.is_dir() == e.is_dir && (e.is_dir || e.size.is_none_or(|s| s == n.size));
    if !same {
        return Err(tr!("“{}” mudou no MEGA: atualize a pasta e tente de novo.", "“{}” changed on MEGA: refresh the folder and try again.", e.name));
    }
    Ok(n)
}

/// The nodes the selected items are, all found before any changes: twins' names shift as
/// one of them goes, so a name looked up after that could name another node.
fn nodes_of(a: &Account, loc: &Location, items: &[Entry]) -> Result<Vec<Node>, String> {
    items.iter().map(|e| node_of(a, loc, e)).collect()
}

/// Excluir outside the Rubbish Bin: the items go to it, as on the website.
pub fn trash(s: &mut State, loc: &Location, items: Vec<Entry>) {
    let Some((server, _)) = at(loc) else { return };
    let server = server.clone();
    let reload = panes_on(s, &server.id);
    let n = items.len();
    let (at_server, loc) = (server.clone(), loc.clone());
    undo::mega_job(s, &server, reload, move |n| tr!("mover {} para a Lixeira do MEGA", "move {} to the MEGA Rubbish Bin", crate::actions::count(n)), move |a, rec| {
        for (i, node) in nodes_of(a, &loc, &items)?.into_iter().enumerate() {
            a.trash(&node).map_err(|err| if i == 0 { err } else { tr!("{err} (já na Lixeira: {i})", "{err} (already in the Rubbish Bin: {i})") })?;
            rec.add([Step::MegaRestore { server: at_server.clone(), node: node.handle.to_string(), name: node.name }]);
        }
        Ok(tr!("Movido para a Lixeira do MEGA: {}.", "Moved to the MEGA Rubbish Bin: {}.", crate::actions::count(n)))
    });
}

/// Excluir permanentemente, confirmed: each item is found by what the pane shows (a twin in the
/// bin can't take its place) and deleted for good.
pub fn purge(s: &mut State, loc: &Location, items: Vec<Entry>) {
    let Some((server, _)) = at(loc) else { return };
    let server = server.clone();
    let reload = panes_on(s, &server.id);
    let (n, loc) = (items.len(), loc.clone());
    task(
        s,
        &server,
        t("excluir do MEGA", "delete from MEGA"),
        move |a| {
            for (i, node) in nodes_of(a, &loc, &items)?.iter().enumerate() {
                a.delete(node).map_err(|err| if i == 0 { err } else { tr!("{err} (já excluídos para sempre: {i})", "{err} (already deleted for good: {i})") })?;
            }
            Ok(tr!("Excluído para sempre: {}.", "Deleted for good: {}.", crate::actions::count(n)))
        },
        move |result| Msg::Done { reload, result },
    );
}

/// Restaurar in the Rubbish Bin: each item goes back to the folder it was in (or Cloud Drive's
/// top when that's gone).
pub fn restore(s: &mut State, p: usize) {
    let loc = s.panes[p].loc.clone();
    let items = selected(s, p);
    let (Some((server, _)), false) = (at(&loc), items.is_empty()) else { return };
    let server = server.clone();
    let reload = panes_on(s, &server.id);
    let n = items.len();
    let at_server = server.clone();
    undo::mega_job(s, &server, reload, move |n| tr!("restaurar {}", "restore {}", crate::actions::count(n)), move |a, rec| {
        for node in nodes_of(a, &loc, &items)? {
            a.restore(&node)?;
            rec.add([Step::MegaTrash { server: at_server.clone(), node: node.handle.to_string(), name: node.name }]);
        }
        Ok(tr!("Restaurado: {}.", "Restored: {}.", crate::actions::count(n)))
    });
    s.status = tr!("Restaurando {}…", "Restoring {}…", crate::actions::count(n));
}

/// Esvaziar lixeira, confirmed: everything in the Rubbish Bin is deleted for good.
pub fn empty_bin(s: &mut State, id: &str) {
    let Some(server) = s.servers.list.iter().find(|sv| sv.id == id).cloned() else { return };
    let reload = panes_on(s, &server.id);
    task(
        s,
        &server,
        t("esvaziar a Lixeira do MEGA", "empty the MEGA Rubbish Bin"),
        |a| {
            // By handle: twins' shown names shift as one of them goes.
            let items = a.list(&format!("/{}", Area::Rubbish.label()))?;
            for h in items.iter().filter_map(|it| it.handle.as_deref()) {
                a.delete(&a.node(&mega::handle_path(h))?)?;
            }
            Ok(tr!("Lixeira do MEGA esvaziada: {}.", "MEGA Rubbish Bin emptied: {}.", crate::actions::count(items.len())))
        },
        move |result| Msg::Done { reload, result },
    );
    s.status = t("Esvaziando a Lixeira do MEGA…", "Emptying the MEGA Rubbish Bin…").into();
}

// ---------------------------------------------------------------------------------------------
// Public links

/// Copiar link, asked before a link is made: the item becomes open to anyone with it.
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
    if in_bin(&loc) {
        s.status = t("Restaure o item da Lixeira antes de compartilhar.", "Restore the item from the Rubbish Bin before sharing it.").into();
        return None;
    }
    let path = path_of(&loc, item)?;
    Some((server.clone(), path, item.clone()))
}

/// Copiar link: the item's link to the clipboard; with none yet, asks before making one.
pub fn copy_link(s: &mut State, p: usize, _: &mut Tx) {
    let Some((server, path, item)) = one_item(s, p) else { return };
    s.status = tr!("Buscando o link de \"{}\"…", "Getting the link for \"{}\"…", item.name);
    let (sv, at_path, it) = (server.clone(), path.clone(), item.clone());
    task(s, &server, t("copiar o link", "copy the link"), move |a| Ok(a.link_of(&a.node(&path)?)), move |result| Msg::Mega(Answer::Link { server: Box::new(sv), path: at_path, item: it, result }));
}

/// Criar link e copiar, from the question.
pub fn make_link(s: &mut State) {
    let Some(Dialog::MegaLink(m)) = s.dialog.take() else { return };
    let MakeLink { server, path, item } = *m;
    s.status = tr!("Criando o link de \"{}\"…", "Making the link for \"{}\"…", item.name);
    let name = item.name.clone();
    task(s, &server, t("criar o link", "make the link"), move |a| a.create_link(&a.node(&path)?).map(Some), move |result| Msg::Mega(Answer::Linked { name, result }));
}

/// Remover link: whoever has the link can't open the item any more.
pub fn remove_link(s: &mut State, p: usize, _: &mut Tx) {
    let Some((server, path, item)) = one_item(s, p) else { return };
    let name = item.name.clone();
    task(s, &server, t("remover o link", "remove the link"), move |a| a.remove_link(&a.node(&path)?).map(|_| None), move |result| Msg::Mega(Answer::Linked { name, result }));
}

pub fn link_dialog(k: &Kit, m: &MakeLink, layer: &mut Vec<El>) {
    let note = tr!(
        "\"{}\" ainda não tem link público. Criar um link deixa qualquer pessoa que o tenha ver e baixar o arquivo. Dá para remover depois em Remover link.",
        "\"{}\" has no public link yet. Making one lets anyone who has it see and download the file. You can remove it later with Remove link.",
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
                .child(Button::new(t("Cancelar", "Cancel")).variant(ButtonVariant::Neutral).on_press(close).build(k).cursor(rune::Cursor::Pointer))
                .child(Button::new(t("Criar link e copiar", "Make link and copy")).variant(ButtonVariant::Submit).on_press(make_link).build(k).cursor(rune::Cursor::Pointer).default_button()),
        );
    Modal::new("mega-link", t("Criar link público?", "Make a public link?")).open(true).child(body).width(480.0).fit().on_close(close).build(k, layer);
}

// ---------------------------------------------------------------------------------------------
// Storage

/// Asks for account `server`'s storage (unless the first answer is still on its way); the
/// status bar keeps showing the last answer meanwhile.
pub fn ask_quota(s: &mut State, server: &Server) {
    match s.servers.mega_quotas.get(&server.id) {
        Some(None) => return,
        Some(Some(_)) => {}
        None => {
            s.servers.mega_quotas.insert(server.id.clone(), None);
        }
    }
    let id = server.id.clone();
    task(s, server, t("ler a cota", "read the storage quota"), |a| a.quota(), move |result| Msg::Mega(Answer::Quota { id, result }));
}

/// The storage the status bar shows beside a MEGA location.
pub fn quota_of(s: &State, loc: &Location) -> Option<mega::Quota> {
    let (server, _) = at(loc)?;
    s.servers.mega_quotas.get(&server.id).copied().flatten()
}

pub fn quota_label(q: &mega::Quota) -> String {
    tr!("{} livres de {}", "{} free of {}", crate::model::format_size(q.total.saturating_sub(q.used)), crate::ui::whole_size(q.total))
}

pub fn quota_help(q: mega::Quota) -> String {
    tr!(
        "Usado: {} de {}\nNa Lixeira: {}",
        "Used: {} of {}\nIn the Rubbish Bin: {}",
        crate::model::format_size(q.used),
        crate::ui::whole_size(q.total),
        crate::model::format_size(q.in_rubbish)
    )
}

// ---------------------------------------------------------------------------------------------
// Changes made elsewhere

/// How often an account a pane shows is asked what changed, in seconds.
pub const EVERY: f64 = 15.0;
/// After a failed ask, how long until the next one.
const RETRY: f64 = 60.0;

/// When each account shown is asked next, and whether an ask is out.
#[derive(Default)]
pub struct Watch {
    next: f64,
    busy: bool,
}

/// Asks each MEGA account a pane shows what changed, once its time has come.
pub fn tick(s: &mut State, now: f64) {
    if s.servers.mega_watch.is_empty() && !s.panes.iter().any(|p| at(&p.loc).is_some()) {
        return;
    }
    let mut shown: Vec<Server> = s.panes.iter().filter_map(|p| at(&p.loc).map(|(sv, _)| sv.clone())).collect();
    shown.dedup_by(|a, b| a.id == b.id);
    s.servers.mega_watch.retain(|id, _| shown.iter().any(|sv| sv.id == *id));
    for server in shown {
        // Only an account already open: opening one is the listing's job.
        if mega::cached(&server.user).is_none() {
            continue;
        }
        let w = s.servers.mega_watch.entry(server.id.clone()).or_insert(Watch { next: now + EVERY, busy: false });
        if w.busy || now < w.next {
            continue;
        }
        w.busy = true;
        let id = server.id.clone();
        task(s, &server, t("ver o que mudou no MEGA", "see what changed on MEGA"), |a| a.poll(), move |result| Msg::Mega(Answer::Changes { id, result }));
    }
}

/// When the next ask is due, for the frame loop to wake then.
pub fn next_ask(s: &State) -> Option<f64> {
    s.servers.mega_watch.values().filter(|w| !w.busy).map(|w| w.next).reduce(f64::min)
}

fn changes_arrived(s: &mut State, id: String, result: Result<Changes, String>, now: f64) {
    let Some(w) = s.servers.mega_watch.get_mut(&id) else { return };
    w.busy = false;
    let changes = match result {
        Ok(c) => {
            w.next = now + EVERY;
            c
        }
        Err(e) => {
            w.next = now + RETRY;
            openexplorer_gdrive::log(&format!("mudanças do MEGA {id}: {e}"));
            return;
        }
    };
    if changes.is_empty() {
        return;
    }
    let Some(server) = s.servers.list.iter().find(|sv| sv.id == id).cloned() else { return };
    let Some(account) = mega::cached(&server.user) else { return };
    let stale = |path: &[String]| match account.folder_handle(&server.path(path)) {
        Some(h) => changes.touches(&h),
        // The top and Compartilhados comigo: a share may have come or gone.
        None => changes.all || path.len() <= 1,
    };
    for p in 0..2 {
        let reload = matches!(&s.panes[p].loc, Location::Server { server: sv, path, find: None } if sv.id == id && stale(path));
        if reload && !s.panes[p].loading {
            crate::load(s, p);
        }
    }
    crate::tree::server_folders_changed(s, &id, &stale);
    // What changed may change what the account uses.
    ask_quota(s, &server);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_path(path: &[&str]) -> Location {
        folder(&Server::mega("ana@exemplo.com"), path.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn areas_read_in_either_language() {
        assert!(bin_top(&at_path(&["Lixeira"])) && bin_top(&at_path(&["Rubbish Bin"])));
        assert!(!bin_top(&at_path(&["Lixeira", "velha"])) && in_bin(&at_path(&["Lixeira", "velha"])));
        assert!(!in_bin(&at_path(&["Drive na nuvem"])));
        assert!(deletes_for_good(&at_path(&["Lixeira", "x"])) && deletes_for_good(&at_path(&["Compartilhados comigo", "Deles"])));
        assert!(!deletes_for_good(&at_path(&["Drive na nuvem"])) && !deletes_for_good(&at_path(&["Compartilhados comigo"])));
        assert!(why_not(&at_path(&[])).is_some());
        assert!(why_not(&at_path(&["Compartilhados comigo"])).is_some());
        assert!(why_not(&at_path(&["Compartilhados comigo", "Deles"])).is_none());
        assert!(why_not(&at_path(&["Lixeira", "x"])).is_some());
        assert!(why_not(&at_path(&["Cloud Drive", "Fotos"])).is_none());
        assert!(items_fixed(&at_path(&[])) && items_fixed(&at_path(&["Incoming shares"])) && !items_fixed(&at_path(&["Drive na nuvem"])));
        // Not MEGA at all.
        assert!(why_not(&Location::home()).is_none() && !in_bin(&Location::home()));
    }

    #[test]
    fn accounts_show_as_servers_of_their_own() {
        let sv = Server::mega("ana@exemplo.com");
        assert!(sv.is_mega());
        assert_eq!(sv.id, "mega:ana@exemplo.com");
        assert_eq!(sv.address(), "mega://ana@exemplo.com");
        assert_eq!(cloud(&sv), at_path(&["Drive na nuvem"]));
    }

    #[test]
    fn the_storage_reads_free_of_total() {
        let q = mega::Quota { total: 21_474_836_480, used: 5_368_709_120, in_rubbish: 1_073_741_824 };
        assert_eq!(quota_label(&q), "15,0 GB livres de 20 GB");
        assert!(quota_help(q).contains("Lixeira: 1,0 GB"));
    }
}
