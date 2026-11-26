//! Dropbox accounts in the panes. Each account shows in the tree under Contas, beside the Google
//! Drive and MEGA accounts and the servers, and opens like a server (see `servers.rs`) at the
//! account's top, a team space's top for a team member (team folders beside their own).
//! Listing, copying to and from anywhere, moving, renaming, new folders, searching and dragging
//! go through the servers' code, with the account as the connection (`openexplorer_remote`'s
//! `dropbox.rs`; many items at once in Dropbox's batches). What only Dropbox has lives here:
//! adding an account (OAuth in the browser), deleting into Dropbox's deleted files and undoing
//! it, a folder's deleted items and restoring them, shared links, the storage in the status bar,
//! and folders that refresh when something changes elsewhere (a longpoll per account).

use std::sync::atomic::{AtomicU64, Ordering};

use openexplorer_dropbox::store::StoredAccount;
use openexplorer_dropbox::{self as dropbox, Account, AppConfig, Changes, Quota};
use openexplorer_i18n::{t, tr};
use openexplorer_remote::Server;
use rune::{El, ScrollSpec, Tx, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, Checkbox, Dialog as Modal, Kit};

use crate::actions::{Dialog, selected};
use crate::model::{Entry, Location};
use crate::servers::{Form, folder, path_of};
use crate::undo::{self, Step};
use crate::{Msg, State, background, catch_panic, icons};

/// The accounts' list, beside the app's other settings.
pub fn store() -> dropbox::store::Store {
    dropbox::store::Store::at(openexplorer_gdrive::config_dir().join("dropbox-accounts.json"))
}

/// The Dropbox account a location is in, and the path below its top.
pub fn at(loc: &Location) -> Option<(&Server, &[String])> {
    match loc {
        Location::Server { server, path, .. } if server.is_dropbox() => Some((server, path)),
        _ => None,
    }
}

/// The account's top, where a new account opens.
pub fn top(server: &Server) -> Location {
    folder(server, Vec::new())
}

/// Atualizar on pane `p`: a Dropbox folder's kept listing goes, so it's read whole (the reloads
/// after changes keep using it, brought up to date by the account's change log).
pub fn forget_shown(s: &State, p: usize) {
    if let Some((server, path)) = at(&s.panes[p].loc)
        && let Some(a) = dropbox::cached(&server.user)
    {
        a.forget(&server.path(path));
    }
}

/// The panes' places on account `id`, reloaded after a change there.
fn panes_on(s: &State, id: &str) -> Vec<Location> {
    s.panes.iter().map(|p| p.loc.clone()).filter(|l| matches!(l, Location::Server { server, .. } if server.id == id)).collect()
}

/// Runs `work` with the account on a thread, then `done` with the result.
fn task<T: Send + 'static>(s: &State, server: &Server, doing: &'static str, work: impl FnOnce(&Account) -> Result<T, String> + Send + 'static, done: impl FnOnce(Result<T, String>) -> Msg + Send + 'static) {
    let email = server.user.clone();
    background(s, move |emit| {
        let result = catch_panic(doing, || dropbox::reach(&email).and_then(|a| work(&a)));
        emit(done(result));
    });
}

/// What background work on Dropbox brought back.
pub enum Answer {
    /// A login in the browser ended: the account it opened.
    Connected { attempt: u64, result: Result<StoredAccount, String> },
    Quota { id: String, result: Result<Quota, String> },
    /// What changed in account `id` while its longpoll waited; `wait` is the [`Watch`]'s that
    /// sent it.
    Changes { id: String, wait: u64, result: Result<Changes, String> },
    /// What Copiar link found: the item's link, or `None` when it has none yet.
    Link { server: Box<Server>, path: String, item: Entry, result: Result<Option<String>, String> },
    /// A link made (`Some`) or taken away (`None`) for the item named `name`.
    Linked { name: String, result: Result<Option<String>, String> },
    /// What was deleted from the folder at `path` (Itens excluídos).
    Deleted { path: String, result: Result<Vec<dropbox::Deleted>, String> },
}

pub fn arrived(s: &mut State, a: Answer, tx: &mut Tx, now: f64) {
    match a {
        Answer::Connected { attempt, result } => {
            if s.servers.dropbox_login == Some(attempt) {
                s.servers.dropbox_login = None;
                s.notify_account();
            }
            connected(s, result)
        }
        Answer::Quota { id, result } => match result {
            Ok(q) => {
                s.servers.dropbox_quotas.insert(id, Some(q));
            }
            Err(e) => openexplorer_gdrive::log(&format!("cota do Dropbox {id}: {e}")),
        },
        Answer::Changes { id, wait, result } => changes_arrived(s, id, wait, result, now),
        Answer::Link { server, path, item, result } => match result {
            Ok(Some(link)) => {
                tx.copy(link);
                s.status = tr!("Link copiado: quem tiver o link pode abrir \"{}\".", "Link copied: anyone with the link can open \"{}\".", item.name);
            }
            Ok(None) => s.dialog = Some(Dialog::DropboxLink(Box::new(MakeLink { server: *server, path, item }))),
            Err(e) => crate::fail(s, e),
        },
        Answer::Linked { name, result } => match result {
            Ok(Some(link)) => {
                tx.copy(link);
                s.status = tr!("Link criado e copiado: quem tiver o link pode abrir \"{name}\".", "Link made and copied: anyone with the link can open \"{name}\".");
            }
            Ok(None) => s.status = tr!("Link removido: \"{name}\" não abre mais pelo link.", "Link removed: \"{name}\" no longer opens from the link."),
            Err(e) => crate::fail(s, e),
        },
        Answer::Deleted { path, result } => {
            if let Some(v) = view_mut(s)
                && v.path == path
            {
                v.picked = vec![false; result.as_ref().map_or(0, Vec::len)];
                v.items = Some(result);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Adding and removing an account

/// The login waiting for the browser now: a newer one stops it (and frees the port).
static LOGIN: AtomicU64 = AtomicU64::new(0);

/// The App key the dialog has, else the saved app: `None` (and the dialog asks for the key)
/// when there's neither.
fn app_of(f: &mut Form) -> Option<AppConfig> {
    let typed = f.client_id.text.trim().trim_start_matches('\u{feff}').to_string();
    if f.custom_oauth || !typed.is_empty() {
        let port = AppConfig::load().map(|c| c.port).unwrap_or(dropbox::DEFAULT_PORT);
        let parsed = if typed.starts_with('{') { AppConfig::parse(&typed) } else { AppConfig::parse(&serde_json::json!({"app_key": typed, "port": port}).to_string()) };
        let cfg = match parsed {
            Ok(c) => c,
            Err(e) => {
                f.error = Some(tr!("App key: {e}", "App key: {e}"));
                return None;
            }
        };
        if let Err(e) = cfg.save() {
            f.error = Some(e);
            return None;
        }
        return Some(cfg);
    }
    match AppConfig::load() {
        Ok(c) => Some(c),
        Err(e) => {
            f.custom_oauth = true;
            f.client_id.focused = true;
            f.error = Some(e);
            None
        }
    }
}

/// Conectar no navegador with Dropbox picked: opens Dropbox's authorization page and waits for
/// it on a thread; the dialog closes, as Google Drive's does.
pub fn submit(s: &mut State, tx: &mut Tx) {
    let Some(f) = &mut s.account_form else { return };
    f.error = None;
    let Some(cfg) = app_of(f) else { return };
    // An older login still waiting holds the port: it's stopped, and gives the port back
    // within a moment.
    let attempt = LOGIN.fetch_add(1, Ordering::SeqCst) + 1;
    let mut begun = dropbox::oauth::begin(&cfg);
    for _ in 0..10 {
        if begun.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        begun = dropbox::oauth::begin(&cfg);
    }
    let pending = match begun {
        Ok(p) => p,
        Err(e) => {
            if let Some(f) = &mut s.account_form {
                f.error = Some(e);
            }
            return;
        }
    };
    if let Err(e) = tx.open_url(&pending.url) {
        if let Some(f) = &mut s.account_form {
            f.error = Some(tr!("Não consegui abrir o navegador: {e:?}", "Couldn't open the browser: {e:?}"));
        }
        return;
    }
    s.servers.dropbox_login = Some(attempt);
    s.close_account_form();
    s.status = t("Entre no Dropbox no navegador que abriu e autorize o Open Explorer. Esperando o Dropbox…", "Sign in to Dropbox in the browser that opened and allow Open Explorer. Waiting for Dropbox…").into();
    s.notify_main();
    background(s, move |emit| {
        let result = catch_panic(t("conectar ao Dropbox", "connect to Dropbox"), || {
            let tokens = pending.finish_until(&cfg, &dropbox::api::Hosts::default().api, &|| LOGIN.load(Ordering::SeqCst) != attempt)?;
            let a = dropbox::connect(&cfg, &tokens)?;
            Ok(StoredAccount { email: a.email.clone(), name: a.name.clone(), account_id: a.account_id.clone() })
        });
        emit(Msg::Dropbox(Answer::Connected { attempt, result }));
    });
}

fn connected(s: &mut State, result: Result<StoredAccount, String>) {
    let account = match result {
        Ok(a) => a,
        Err(e) => {
            openexplorer_gdrive::log(&format!("falha ao conectar Dropbox: {e}"));
            return crate::fail(s, e);
        }
    };
    let email = account.email.clone();
    if let Err(e) = s.servers.dropbox.add(account.clone()) {
        return crate::fail(s, e);
    }
    match s.servers.dropbox_accounts.iter_mut().find(|a| a.email.eq_ignore_ascii_case(&email)) {
        Some(a) => *a = account,
        None => s.servers.dropbox_accounts.push(account),
    }
    openexplorer_gdrive::log(&format!("conta Dropbox conectada: {email}"));
    let server = Server::dropbox(&email);
    if !s.servers.list.iter().any(|sv| sv.id == server.id) {
        s.servers.list.push(server.clone());
    }
    s.servers.pool.forget(&server.id);
    s.tree.server_changed(&server.id);
    s.tree.server_opened(&server.id);
    s.status = tr!("Conta Dropbox conectada: {email}", "Dropbox account connected: {email}");
    let p = s.active;
    crate::navigate(s, p, top(&server));
}

/// Remover conta: the access is revoked on Dropbox's side and leaves the credential store.
/// Whether the account left the list.
pub fn remove(s: &mut State, server: &Server) -> bool {
    if let Err(e) = s.servers.dropbox.remove(&server.user) {
        crate::fail(s, e);
        return false;
    }
    s.servers.dropbox_accounts.retain(|a| !a.email.eq_ignore_ascii_case(&server.user));
    dropbox::disconnect(&server.user);
    s.servers.dropbox_quotas.remove(&server.id);
    s.servers.dropbox_watch.remove(&server.id);
    true
}

/// The Dropbox step of Registrar conta: the App key when none is set, then the browser.
pub fn account_step(s: &State, k: &Kit, f: &Form, banner: El, close: fn(&mut State, &mut Tx)) -> (El, El) {
    fn key_edit(s: &mut State) -> &mut rune::TextEdit {
        match &mut s.account_form {
            Some(f) => &mut f.client_id,
            None => &mut s.name_edit,
        }
    }
    let note = |words: &str| text(words.to_string()).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0);
    let mut inner = col().w_full().gap(12.0).child(banner).child(note(t(
        "Ao continuar, o navegador abre a página do Dropbox para você entrar e autorizar o Open Explorer. Contas de um time veem também as pastas da equipe.",
        "When you continue, the browser opens Dropbox's page so you can sign in and allow Open Explorer. Team accounts also see the team's folders.",
    )));
    let connected: Vec<&str> = s.servers.list.iter().filter(|sv| sv.is_dropbox()).map(|sv| sv.user.as_str()).collect();
    if !connected.is_empty() {
        inner = inner.child(
            row()
                .key("connected-dropbox")
                .w_full()
                .px(10.0)
                .py(7.0)
                .gap(6.0)
                .center_y()
                .radius(k.radius.sm)
                .fill(k.alpha(k.c.text_accent, if k.dark { 0.10 } else { 0.06 }))
                .child(el().size(6.0, 6.0).radius(3.0).fill(k.c.success.fg))
                .child(text(tr!("Já conectadas: {}", "Already connected: {}", connected.join(", "))).font(k.ty.text_xsmall).color(k.c.text_base).truncate().min_w(0.0).grow(1.0)),
        );
    }
    if f.custom_oauth {
        let port = AppConfig::load().map(|c| c.port).unwrap_or(dropbox::DEFAULT_PORT);
        let key_input = rune_kit::TextInput::new(&f.client_id)
            .width(None)
            .label(t("App key", "App key"))
            .placeholder(t("a App key do seu app (aba Settings)", "your app's App key (Settings tab)"))
            .on_edit(key_edit)
            .on_submit(submit);
        let console = Button::new(t("Abrir App Console", "Open App Console"))
            .icon(&icons::EXTERNAL_LINK)
            .variant(ButtonVariant::Neutral)
            .on_press_tx(|s: &mut State, tx: &mut Tx| {
                if let Err(e) = tx.open_url("https://www.dropbox.com/developers/apps") {
                    crate::fail(s, tr!("Não consegui abrir o navegador: {e:?}", "Couldn't open the browser: {e:?}"));
                }
            })
            .build(k).cursor(rune::Cursor::Pointer);
        let how = tr!(
            "Crie um app em Create app: Scoped access, Full Dropbox. Em Permissions marque files.metadata.read e .write, files.content.read e .write, sharing.read e .write e account_info.read, e clique em Submit. Em Settings › OAuth 2 › Redirect URIs adicione http://localhost:{port}. Depois cole a App key aqui (não é preciso o App secret).",
            "Create an app with Create app: Scoped access, Full Dropbox. On Permissions tick files.metadata.read and .write, files.content.read and .write, sharing.read and .write and account_info.read, then click Submit. On Settings › OAuth 2 › Redirect URIs add http://localhost:{port}. Then paste the App key here (the App secret isn't needed)."
        );
        inner = inner.child(
            col()
                .key("dropbox-app")
                .w_full()
                .padding(12.0)
                .gap(8.0)
                .radius(k.radius.md)
                .fill(k.c.bg_base)
                .border(1.0, k.c.border_weak)
                .child(row().w_full().center_y().child(text(t("App do Dropbox", "Dropbox app")).font(k.ty.text_small_medium).color(k.c.text_base).grow(1.0)).child(console))
                .child(text(how).font(k.ty.caption()).color(k.c.text_muted).wrap().w_full().min_w(0.0))
                .child(col().w_full().gap(4.0).child(text(t("App key", "App key")).font(k.ty.text_xsmall_medium).color(k.c.text_base)).child(key_input.build(k))),
        );
    } else {
        inner = inner.child(
            row()
                .key("dropbox-ready")
                .w_full()
                .px(12.0)
                .py(8.0)
                .gap(8.0)
                .center_y()
                .radius(k.radius.md)
                .fill(k.c.bg_base)
                .border(1.0, k.c.border_weak)
                .child(icon(k.glyph.lock, 15.0).color(k.c.success.fg))
                .child(text(t("App do Dropbox configurado e pronto para conectar.", "Dropbox app set up and ready to connect.")).font(k.ty.text_xsmall).color(k.c.text_muted).grow(1.0).min_w(0.0))
                .child(
                    Button::new(t("Alterar App key", "Change App key"))
                        .variant(ButtonVariant::GhostMuted)
                        .on_press(|s: &mut State| {
                            if let Some(f) = &mut s.account_form {
                                f.custom_oauth = true;
                                f.client_id.focused = true;
                            }
                        })
                        .build(k).cursor(rune::Cursor::Pointer),
                ),
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
        .child(Button::new(t("Voltar", "Back")).icon(&icons::BACK).variant(ButtonVariant::Neutral).on_press(crate::servers::back_to_pick).build(k).cursor(rune::Cursor::Pointer))
        .child(el().grow(1.0))
        .child(Button::new(t("Cancelar", "Cancel")).variant(ButtonVariant::Neutral).on_press_tx(close).build(k).cursor(rune::Cursor::Pointer))
        .child(Button::new(t("Conectar no navegador", "Connect in browser")).variant(ButtonVariant::Submit).on_press_tx(submit).build(k).cursor(rune::Cursor::Pointer).default_button());
    (inner, actions)
}

// ---------------------------------------------------------------------------------------------
// Deleting, and undoing it

/// What the Excluir question says before deleting in a Dropbox account.
pub fn delete_note() -> &'static str {
    t(
        "Os itens vão para os arquivos excluídos do Dropbox, de onde podem ser restaurados por um tempo (Desfazer, ou pelo site).",
        "The items go to Dropbox's deleted files, where they can be restored for a while (Undo, or on the website).",
    )
}

/// Excluir: the items go to Dropbox's deleted files (several in one batch), and Desfazer brings
/// them back.
pub fn delete(s: &mut State, loc: &Location, items: Vec<Entry>) {
    let Some((server, _)) = at(loc) else { return };
    let server = server.clone();
    let reload = panes_on(s, &server.id);
    let paths: Vec<(String, String)> = items.iter().filter_map(|e| path_of(loc, e).map(|p| (p, e.name.clone()))).collect();
    undo::local_job(s, reload, |n| tr!("excluir {} do Dropbox", "delete {} from Dropbox", crate::actions::count(n)), move |rec| {
        let a = dropbox::reach(&server.user)?;
        // The deletion's window on Dropbox's clock, for Desfazer to bring back what it took.
        let since = a.api.server_now() - 2;
        let results = match paths.as_slice() {
            [(one, _)] => vec![a.delete(one)],
            many => a.delete_many(&many.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>())?,
        };
        let until = a.api.server_now() + 1;
        let mut failed = Vec::new();
        let mut done = 0;
        for (r, (path, name)) in results.into_iter().zip(&paths) {
            match r {
                Ok(e) => {
                    done += 1;
                    rec.add([Step::DropboxRestore { server: server.clone(), path: path.clone(), name: name.clone(), rev: (!e.dir).then_some(e.rev), since, until }]);
                }
                Err(e) => failed.push(e),
            }
        }
        if failed.is_empty() {
            Ok(tr!("Excluído do Dropbox: {}. Dá para desfazer.", "Deleted from Dropbox: {}. It can be undone.", crate::actions::count(done)))
        } else {
            Err(failed.join("\n"))
        }
    });
}

/// Whether the item a Dropbox step changes is as it expects.
pub fn check(step: &Step) -> Result<(), String> {
    match step {
        Step::DropboxDelete { server, path, name } => match dropbox::reach(&server.user)?.stat(path)? {
            Some(_) => Ok(()),
            None => Err(tr!("“{name}” não está mais no Dropbox.", "“{name}” is no longer on Dropbox.")),
        },
        Step::DropboxRestore { server, path, name, .. } => match dropbox::reach(&server.user)?.stat(path)? {
            Some(_) => Err(tr!("já existe “{name}” no Dropbox, onde ele estava.", "“{name}” already exists on Dropbox, where it was.")),
            None => Ok(()),
        },
        _ => Ok(()),
    }
}

/// Makes a Dropbox step's change, giving the step that takes it back.
pub fn run(step: &Step) -> Result<Step, String> {
    match step {
        Step::DropboxDelete { server, path, name } => {
            let a = dropbox::reach(&server.user)?;
            let since = a.api.server_now() - 2;
            let e = a.delete(path)?;
            let until = a.api.server_now() + 1;
            Ok(Step::DropboxRestore { server: server.clone(), path: path.clone(), name: name.clone(), rev: (!e.dir).then_some(e.rev), since, until })
        }
        Step::DropboxRestore { server, path, name, rev, since, until } => {
            let a = dropbox::reach(&server.user)?;
            match rev {
                Some(rev) => {
                    a.restore(path, rev)?;
                }
                None => {
                    a.restore_folder(path, Some((*since, *until)))?;
                }
            }
            Ok(Step::DropboxDelete { server: server.clone(), path: path.clone(), name: name.clone() })
        }
        other => Ok(other.clone()),
    }
}

// ---------------------------------------------------------------------------------------------
// A folder's deleted items

/// Itens excluídos: what was deleted from a folder, as the Dropbox website's "deleted files"
/// shows it, to pick and bring back.
pub struct DeletedView {
    pub server: Server,
    /// The folder's path on the account.
    pub path: String,
    /// Its name, for the title.
    pub place: String,
    /// What was deleted there: `None` while it's asked.
    pub items: Option<Result<Vec<dropbox::Deleted>, String>>,
    /// Which of `items` are ticked.
    pub picked: Vec<bool>,
}

fn view_mut(s: &mut State) -> Option<&mut DeletedView> {
    match &mut s.dialog {
        Some(Dialog::DropboxDeleted(v)) => Some(v),
        _ => None,
    }
}

/// Whether pane `p` shows a Dropbox folder (not a search), where Itens excluídos applies.
pub fn shows_folder(s: &State, p: usize) -> bool {
    matches!(&s.panes[p].loc, Location::Server { server, find: None, .. } if server.is_dropbox())
}

/// Itens excluídos: the deleted items of the folder pane `p` shows, in a dialog.
pub fn show_deleted(s: &mut State, p: usize, _: &mut Tx) {
    if !shows_folder(s, p) {
        return;
    }
    let Some((server, path)) = at(&s.panes[p].loc) else { return };
    let (server, dir) = (server.clone(), server.path(path));
    let place = path.last().cloned().unwrap_or_else(|| server.name.clone());
    s.dialog = Some(Dialog::DropboxDeleted(Box::new(DeletedView { server: server.clone(), path: dir.clone(), place, items: None, picked: Vec::new() })));
    let at_path = dir.clone();
    task(s, &server, t("listar os itens excluídos", "list the deleted items"), move |a| a.deleted_in(&at_path), move |result| Msg::Dropbox(Answer::Deleted { path: dir, result }));
}

/// Restaurar, in the dialog: the items ticked come back (a folder with what its deletion took),
/// and Desfazer deletes them again.
pub fn restore_picked(s: &mut State) {
    let Some(Dialog::DropboxDeleted(v)) = s.dialog.take() else { return };
    let DeletedView { server, items, picked, .. } = *v;
    let chosen: Vec<dropbox::Deleted> = items.and_then(Result::ok).unwrap_or_default().into_iter().zip(&picked).filter(|(_, on)| **on).map(|(d, _)| d).collect();
    if chosen.is_empty() {
        return;
    }
    let reload = panes_on(s, &server.id);
    s.status = tr!("Restaurando {}…", "Restoring {}…", crate::actions::count(chosen.len()));
    undo::local_job(s, reload, |n| tr!("restaurar {} no Dropbox", "restore {} on Dropbox", crate::actions::count(n)), move |rec| {
        let a = dropbox::reach(&server.user)?;
        let (mut done, mut files, mut failed) = (0, 0, Vec::new());
        for d in &chosen {
            match a.restore_deleted(d) {
                Ok(n) => {
                    done += 1;
                    files += n;
                    rec.add([Step::DropboxDelete { server: server.clone(), path: d.path.clone(), name: d.name.clone() }]);
                }
                Err(e) => failed.push(e),
            }
        }
        match failed.first() {
            None if files == done => Ok(tr!("Restaurado no Dropbox: {}.", "Restored on Dropbox: {}.", crate::actions::count(done))),
            None => Ok(tr!("Restaurado no Dropbox: {} ({files} arquivos).", "Restored on Dropbox: {} ({files} files).", crate::actions::count(done))),
            Some(_) if done == 0 => Err(failed.join("\n")),
            Some(first) => Err(tr!("Restaurado: {}; {} falharam ({first})", "Restored: {}; {} failed ({first})", crate::actions::count(done), failed.len())),
        }
    });
}

/// What a deleted item's line says under its name.
fn deleted_detail(d: &dropbox::Deleted) -> String {
    let when = |w: i64| crate::model::format_time(std::time::UNIX_EPOCH + std::time::Duration::from_secs(w.max(0) as u64));
    match (d.dir, d.size, d.when) {
        (Some(true), ..) => t("Pasta", "Folder").into(),
        (Some(false), Some(size), Some(w)) => tr!("{} · excluído em {}", "{} · deleted on {}", crate::model::format_size(size), when(w)),
        (Some(false), Some(size), None) => crate::model::format_size(size),
        _ => t("Arquivo ou pasta", "File or folder").into(),
    }
}

pub fn deleted_dialog(k: &Kit, v: &DeletedView, layer: &mut Vec<El>) {
    let close = |s: &mut State| s.dialog = None;
    let muted = |words: String| text(words).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0);
    let mut body = col().key("content").w_full().px(16.0).pb(16.0).gap(10.0).child(muted(
        t(
            "Itens excluídos desta pasta que o Dropbox ainda guarda. Marque os que quer de volta: uma pasta volta com o que foi excluído junto com ela.",
            "Items deleted from this folder that Dropbox still keeps. Tick the ones you want back: a folder comes back with what was deleted along with it.",
        )
        .into(),
    ));
    let picked = v.picked.iter().filter(|p| **p).count();
    match &v.items {
        None => body = body.child(muted(t("Procurando os itens excluídos…", "Looking for the deleted items…").into())),
        Some(Err(e)) => body = body.child(text(e.clone()).key("error").font(k.ty.text_small).color(k.c.danger.fg).wrap().w_full().min_w(0.0)),
        Some(Ok(items)) if items.is_empty() => body = body.child(muted(t("Nada excluído aqui.", "Nothing deleted here.").into())),
        Some(Ok(items)) => {
            let all = picked == items.len();
            body = body.child(
                Checkbox::new(all)
                    .label(tr!("Todos ({})", "All ({})", items.len()))
                    .on_toggle(move |s: &mut State| {
                        if let Some(v) = view_mut(s) {
                            v.picked.iter_mut().for_each(|p| *p = !all);
                        }
                    })
                    .build(k)
                    .key("all"),
            );
            let mut list = col().key("deleted").w_full().gap(6.0);
            for (i, d) in items.iter().enumerate() {
                let on = v.picked.get(i).copied().unwrap_or(false);
                list = list.child(
                    Checkbox::new(on)
                        .label(d.name.clone())
                        .description(deleted_detail(d))
                        .on_toggle(move |s: &mut State| {
                            if let Some(p) = view_mut(s).and_then(|v| v.picked.get_mut(i)) {
                                *p = !*p;
                            }
                        })
                        .build(k)
                        .key(format!("deleted-{i}")),
                );
            }
            body = body.child(col().key("deleted-scroll").w_full().max_h(320.0).min_h(0.0).shrink().child(list.scroll(ScrollSpec { stick_bottom: false })));
        }
    }
    let label = if picked > 0 { tr!("Restaurar ({picked})", "Restore ({picked})") } else { t("Restaurar", "Restore").into() };
    body = body.child(
        row()
            .key("actions")
            .w_full()
            .pt(8.0)
            .gap(8.0)
            .justify_end()
            .child(Button::new(t("Fechar", "Close")).variant(ButtonVariant::Neutral).on_press(close).build(k).cursor(rune::Cursor::Pointer))
            .child(Button::new(label).variant(ButtonVariant::Submit).disabled(picked == 0).on_press(restore_picked).build(k).cursor(rune::Cursor::Pointer).default_button()),
    );
    let title = tr!("Itens excluídos de “{}”", "Deleted items of “{}”", v.place);
    Modal::new("dropbox-deleted", title).open(true).child(body).width(520.0).fit().on_close(close).build(k, layer);
}

// ---------------------------------------------------------------------------------------------
// Shared links

/// Copiar link, asked before a link is made: the item opens to anyone who has it.
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
    task(s, &server, t("copiar o link", "copy the link"), move |a| a.link_of(&path), move |result| Msg::Dropbox(Answer::Link { server: Box::new(sv), path: at_path, item: it, result }));
}

/// Criar link e copiar, from the question.
pub fn make_link(s: &mut State) {
    let Some(Dialog::DropboxLink(m)) = s.dialog.take() else { return };
    let MakeLink { server, path, item } = *m;
    s.status = tr!("Criando o link de \"{}\"…", "Making the link for \"{}\"…", item.name);
    let name = item.name.clone();
    task(s, &server, t("criar o link", "make the link"), move |a| a.create_link(&path).map(Some), move |result| Msg::Dropbox(Answer::Linked { name, result }));
}

/// Remover link: whoever has the link can't open the item any more.
pub fn remove_link(s: &mut State, p: usize, _: &mut Tx) {
    let Some((server, path, item)) = one_item(s, p) else { return };
    let name = item.name.clone();
    task(s, &server, t("remover o link", "remove the link"), move |a| a.remove_link(&path).map(|_| None), move |result| Msg::Dropbox(Answer::Linked { name, result }));
}

pub fn link_dialog(k: &Kit, m: &MakeLink, layer: &mut Vec<El>) {
    let note = tr!(
        "\"{}\" ainda não tem link. Criar um link deixa qualquer pessoa que o tenha ver e baixar o item. Dá para remover depois em Remover link.",
        "\"{}\" has no link yet. Making one lets anyone who has it see and download the item. You can remove it later with Remove link.",
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
    Modal::new("dropbox-link", t("Criar link compartilhado?", "Make a shared link?")).open(true).child(body).width(480.0).fit().on_close(close).build(k, layer);
}

// ---------------------------------------------------------------------------------------------
// Storage

/// Asks for account `server`'s storage (unless the first answer is still on its way).
pub fn ask_quota(s: &mut State, server: &Server) {
    match s.servers.dropbox_quotas.get(&server.id) {
        Some(None) => return,
        Some(Some(_)) => {}
        None => {
            s.servers.dropbox_quotas.insert(server.id.clone(), None);
        }
    }
    let id = server.id.clone();
    task(s, server, t("ler a cota", "read the storage quota"), |a| a.quota(), move |result| Msg::Dropbox(Answer::Quota { id, result }));
}

/// The storage the status bar shows beside a Dropbox location: its words, how full, the tip.
pub fn quota_status(s: &State, loc: &Location) -> Option<(String, Option<f64>, String)> {
    let (server, _) = at(loc)?;
    let q = s.servers.dropbox_quotas.get(&server.id).copied().flatten()?;
    Some((quota_label(&q), (q.total > 0).then(|| q.used as f64 / q.total as f64), quota_help(q)))
}

pub fn quota_label(q: &Quota) -> String {
    tr!("{} livres de {}", "{} free of {}", crate::model::format_size(q.total.saturating_sub(q.used)), crate::ui::whole_size(q.total))
}

pub fn quota_help(q: Quota) -> String {
    let used = tr!("Usado: {} de {}", "Used: {} of {}", crate::model::format_size(q.used), crate::ui::whole_size(q.total));
    if q.team { format!("{used}\n{}", t("Espaço do time", "Team space")) } else { used }
}

// ---------------------------------------------------------------------------------------------
// Changes made elsewhere

/// How long each longpoll waits for a change, in seconds (Dropbox takes 30 to 480).
const WAIT: u64 = 60;
/// After a failed wait, how long until the next one.
const RETRY: f64 = 60.0;

/// Numbers each wait sent, so an answer is told from a newer wait's.
static WAITS: AtomicU64 = AtomicU64::new(0);

/// When each account shown waits for changes next, and whether a wait is out (`wait`, its
/// number).
#[derive(Default)]
pub struct Watch {
    next: f64,
    busy: bool,
    wait: u64,
}

/// Keeps one longpoll out for each Dropbox account a pane shows (on a thread of its own, it
/// costs nothing while nothing changes). An account no pane shows any more keeps its entry
/// while its wait is out: coming back to it meanwhile sends no second one.
pub fn tick(s: &mut State, now: f64) {
    if s.servers.dropbox_watch.is_empty() && !s.panes.iter().any(|p| at(&p.loc).is_some()) {
        return;
    }
    let mut shown: Vec<Server> = s.panes.iter().filter_map(|p| at(&p.loc).map(|(sv, _)| sv.clone())).collect();
    shown.dedup_by(|a, b| a.id == b.id);
    s.servers.dropbox_watch.retain(|id, w| w.busy || shown.iter().any(|sv| sv.id == *id));
    for server in shown {
        // Only an account already open: opening one is the listing's job.
        if dropbox::cached(&server.user).is_none() {
            continue;
        }
        let w = s.servers.dropbox_watch.entry(server.id.clone()).or_insert(Watch { next: now, busy: false, wait: 0 });
        if w.busy || now < w.next {
            continue;
        }
        w.busy = true;
        w.wait = WAITS.fetch_add(1, Ordering::Relaxed) + 1;
        let (id, wait) = (server.id.clone(), w.wait);
        task(s, &server, t("esperar mudanças no Dropbox", "wait for changes on Dropbox"), |a| a.watch(WAIT), move |result| Msg::Dropbox(Answer::Changes { id, wait, result }));
    }
}

/// When the next wait is due, for the frame loop to wake then.
pub fn next_ask(s: &State) -> Option<f64> {
    s.servers.dropbox_watch.values().filter(|w| !w.busy).map(|w| w.next).reduce(f64::min)
}

fn changes_arrived(s: &mut State, id: String, wait: u64, result: Result<Changes, String>, now: f64) {
    // A wait from before the account was removed and added again isn't this entry's.
    let Some(w) = s.servers.dropbox_watch.get_mut(&id).filter(|w| w.wait == wait) else { return };
    w.busy = false;
    let changes = match result {
        Ok(c) => {
            w.next = now + c.backoff.unwrap_or(0) as f64;
            c
        }
        Err(e) => {
            w.next = now + RETRY;
            openexplorer_gdrive::log(&format!("mudanças do Dropbox {id}: {e}"));
            return;
        }
    };
    if changes.is_empty() {
        return;
    }
    let Some(server) = s.servers.list.iter().find(|sv| sv.id == id).cloned() else { return };
    let stale = |path: &[String]| changes.touches(&server.path(path));
    for p in 0..2 {
        let reload = matches!(&s.panes[p].loc, Location::Server { server: sv, path, find: None } if sv.id == id && stale(path));
        if reload && !s.panes[p].loading {
            crate::load(s, p);
        }
    }
    crate::tree::server_folders_changed(s, &id, &stale);
    ask_quota(s, &server);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_open_at_their_top() {
        let sv = Server::dropbox("ana@exemplo.com");
        assert_eq!(top(&sv), folder(&sv, Vec::new()));
        assert!(at(&top(&sv)).is_some() && at(&Location::home()).is_none());
    }

    #[test]
    fn an_account_address_reaches_its_folder_and_a_new_one_opens_the_sign_in() {
        let sv = Server::dropbox("ana@exemplo.com");
        let a = crate::servers::parse_address("dropbox://Ana@Exemplo.com/Fotos/Viagem%202024").unwrap();
        assert_eq!((a.protocol, a.user.as_str(), a.path.as_str()), (openexplorer_remote::Protocol::Dropbox, "ana@exemplo.com", "/Fotos/Viagem 2024"));
        let at = folder(&sv, vec!["Fotos".into(), "Viagem 2024".into()]);
        assert_eq!(crate::servers::reach(std::slice::from_ref(&sv), &a).unwrap(), at);
        assert_eq!(crate::servers::address_text(&at), "dropbox://ana@exemplo.com/Fotos/Viagem 2024");
        // Not connected: the dialog to add it, on Dropbox's step.
        assert_eq!(crate::servers::reach(&[], &a), Err(String::new()));
        assert_eq!(Form::of_address(&a).provider, crate::servers::Provider::Dropbox);
        // A MEGA address isn't taken for a Dropbox one.
        let m = crate::servers::parse_address("mega://ana@exemplo.com/Drive na nuvem").unwrap();
        assert!(crate::servers::reach(std::slice::from_ref(&sv), &m).is_err());
    }

    #[test]
    fn the_storage_reads_free_of_total() {
        let q = Quota { used: 5_368_709_120, total: 2_199_023_255_552, team: false };
        assert_eq!(quota_label(&q), "2,0 TB livres de 2 TB");
        assert!(!quota_help(q).contains("time"));
        assert!(quota_help(Quota { team: true, ..q }).contains("Espaço do time"));
    }

    #[test]
    fn deleted_items_say_what_they_are() {
        let d = |dir, size, when| dropbox::Deleted { name: "x".into(), path: "/x".into(), dir, rev: None, size, when };
        assert_eq!(deleted_detail(&d(Some(true), None, None)), "Pasta");
        assert!(deleted_detail(&d(Some(false), Some(2048), Some(1_700_000_000))).contains("excluído em"));
        assert_eq!(deleted_detail(&d(None, None, None)), "Arquivo ou pasta");
    }

    #[test]
    fn changes_name_the_folders_panes_show() {
        let sv = Server::dropbox("ana@exemplo.com");
        let c = Changes { folders: ["/fotos/2024".to_string()].into(), all: false, backoff: None };
        assert!(c.touches(&sv.path(&["Fotos".into(), "2024".into()])));
        assert!(!c.touches(&sv.path(&["Fotos".into()])));
    }
}

/// The panes' work on a Dropbox account in batches, as the panes do it (Colar, Recortar, an
/// upload), against the Dropbox crate's stand-in for the API: the account is the connection,
/// with its batch methods and `batches()` of (250, 8 MiB).
#[cfg(test)]
mod batches {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use openexplorer_dropbox::fake::Fake;

    use super::*;
    use crate::actions::{self, Clip, ClipMode};
    use crate::ops::Stage;

    /// Lets background jobs finish: polls until the status says how the last one went.
    fn settle(s: &mut State) {
        let start = Instant::now();
        loop {
            crate::poll(s, 0.0, &mut rune::Tx::default());
            let busy = s.panes.iter().any(|p| p.loading) || s.status.is_empty() || s.status.ends_with('…');
            if !busy {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(60), "stuck: {}", s.status);
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A state whose only server is an account on a new stand-in, open as `tag`'s e-mail.
    fn on_fake(tag: &str) -> (Fake, Server, State) {
        let fake = Fake::start();
        let email = format!("{tag}@lotes.teste");
        dropbox::account::adopt(&email, Arc::new(fake.account()));
        let server = Server::dropbox(&email);
        let mut s = crate::init();
        s.cache = crate::cache::Cache::memory_only();
        s.servers.list = vec![server.clone()];
        // Rows done stay in the queue, to be looked at.
        s.settings.clear_completed = false;
        (fake, server, s)
    }

    fn open(s: &mut State, p: usize, loc: Location) {
        crate::switch(s, p, loc);
        s.status = "listado".into();
        settle(s);
    }

    /// Picks `names` (all when empty) in pane 0's folder and pastes them into pane 1's, without
    /// waiting for the job.
    fn paste_now(s: &mut State, names: &[&str], mode: ClipMode) {
        let items: Vec<Entry> = s.panes[0].entries.iter().filter(|e| names.is_empty() || names.contains(&e.name.as_str())).cloned().collect();
        s.status.clear();
        s.clip = Some(Clip { mode, from: s.panes[0].loc.clone(), items, picked_at: 0.0 });
        actions::paste(s, 1);
    }

    /// The stages of the last `n` rows of the queue, in order.
    fn stages(s: &State, n: usize) -> Vec<Stage> {
        s.transfers[s.transfers.len() - n..].iter().map(|t| t.progress.stage()).collect()
    }

    fn count(stages: &[Stage], stage: Stage) -> usize {
        stages.iter().filter(|s| **s == stage).count()
    }

    #[test]
    fn moves_go_in_batches_of_what_the_account_takes_and_undo_one_by_one() {
        let (fake, sv, mut s) = on_fake("mover");
        for i in 0..300 {
            fake.put_file(&format!("/A/f{i:03}.txt"), b"a");
        }
        // Taken where f007 goes: that one fails alone.
        fake.put_file("/B/f007.txt", b"ja estava");
        open(&mut s, 0, folder(&sv, vec!["A".into()]));
        open(&mut s, 1, folder(&sv, vec!["B".into()]));
        paste_now(&mut s, &[], ClipMode::Cut);
        settle(&mut s);
        // 250 a batch, the rest in the next; each row with its own result.
        assert_eq!(fake.batch_sizes("files/move_batch_v2"), [250, 50]);
        let st = stages(&s, 300);
        assert_eq!((count(&st, Stage::Done), count(&st, Stage::Failed)), (299, 1), "{}", s.status);
        assert_eq!(st[7], Stage::Failed);
        assert!(s.transfers[s.transfers.len() - 300 + 7].progress.error().contains("Já existe"));
        assert!(s.status.contains("299") && s.status.contains("com erro: 1"), "{}", s.status);
        assert!(fake.has("/B/f000.txt") && fake.has("/B/f299.txt") && fake.has("/A/f007.txt"));
        assert_eq!(fake.content("/B/f007.txt"), b"ja estava");

        // Desfazer: a step per item moved, each back where it was; the one that failed stays.
        crate::undo::undo(&mut s, false);
        settle(&mut s);
        assert!(s.status.starts_with("Desfeito"), "{}", s.status);
        assert!((0..300).all(|i| fake.has(&format!("/A/f{i:03}.txt"))));
        assert!((0..300).filter(|i| *i != 7).all(|i| !fake.has(&format!("/B/f{i:03}.txt"))));
        assert_eq!(fake.content("/B/f007.txt"), b"ja estava");
    }

    #[test]
    fn copies_in_one_folder_take_free_names_and_rows_cancelled_meanwhile_stay_out() {
        let (fake, sv, mut s) = on_fake("copiar");
        for i in 0..300 {
            fake.put_file(&format!("/C/f{i:03}.txt"), format!("{i}").as_bytes());
        }
        open(&mut s, 0, folder(&sv, vec!["C".into()]));
        open(&mut s, 1, folder(&sv, vec!["C".into()]));
        // Each answer late, so the first batch is still out when Cancelar comes.
        fake.delay(150);
        paste_now(&mut s, &[], ClipMode::Copy);
        std::thread::sleep(Duration::from_millis(300));
        let rows = s.transfers.len() - 300;
        for t in &s.transfers[rows + 280..rows + 290] {
            t.progress.cancel();
        }
        settle(&mut s);
        fake.delay(0);
        assert_eq!(fake.batch_sizes("files/copy_batch_v2"), [250, 40]);
        let st = stages(&s, 300);
        assert_eq!((count(&st, Stage::Done), count(&st, Stage::Cancelled)), (290, 10), "{}", s.status);
        // The names the one-by-one path gives: "nome (2).ext".
        assert_eq!(fake.content("/C/f000 (2).txt"), b"0");
        assert_eq!(fake.content("/C/f299 (2).txt"), b"299");
        assert!(!fake.has("/C/f285 (2).txt"));
    }

    /// Leaving an account while its longpoll waits and coming back sends no second one; the
    /// wait's answer frees the entry, and one of an older wait changes nothing.
    #[test]
    fn one_longpoll_per_account_across_leaving_and_coming_back() {
        let (fake, sv, mut s) = on_fake("vigiar");
        fake.put_file("/W/a.txt", b"a");
        let local = local_tree("vigiar");
        open(&mut s, 1, folder(&sv, vec!["W".into()]));
        tick(&mut s, 0.0);
        let wait_for = |what: &dyn Fn() -> bool| {
            let start = Instant::now();
            while !what() {
                assert!(start.elapsed() < Duration::from_secs(20));
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        wait_for(&|| fake.count_since(&0, "files/list_folder/longpoll") == 1);
        let first = s.servers.dropbox_watch[&sv.id].wait;
        // Away, then back, while the wait is out.
        open(&mut s, 1, Location::Local(local.clone()));
        tick(&mut s, 1.0);
        assert!(s.servers.dropbox_watch[&sv.id].busy, "kept while its wait is out");
        open(&mut s, 1, folder(&sv, vec!["W".into()]));
        tick(&mut s, 2.0);
        assert_eq!(s.servers.dropbox_watch[&sv.id].wait, first, "no second wait sent");
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!((fake.count_since(&0, "files/list_folder/longpoll"), fake.most_longpolls()), (1, 1));
        // An answer of an older wait: ignored.
        arrived(&mut s, Answer::Changes { id: sv.id.clone(), wait: first - 1, result: Ok(Changes::default()) }, &mut rune::Tx::default(), 3.0);
        assert!(s.servers.dropbox_watch[&sv.id].busy);
        // A change elsewhere ends the wait, and the next one (the frame loop ticks) goes out alone.
        fake.put_file("/W/b.txt", b"b");
        let start = Instant::now();
        while s.servers.dropbox_watch[&sv.id].wait == first {
            assert!(start.elapsed() < Duration::from_secs(20), "{:?}", fake.calls_since(&0));
            crate::poll(&mut s, 4.0, &mut rune::Tx::default());
            tick(&mut s, 4.0);
            std::thread::sleep(Duration::from_millis(10));
        }
        wait_for(&|| fake.count_since(&0, "files/list_folder/longpoll") == 2);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(fake.count_since(&0, "files/list_folder/longpoll"), 2);
        assert_eq!(fake.most_longpolls(), 1);
        let _ = std::fs::remove_dir_all(local);
    }

    fn local_tree(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oe-app-dropbox-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, n: usize, seed: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, (0..n).map(|k| ((k + seed) % 251) as u8).collect::<Vec<u8>>()).unwrap();
    }

    #[test]
    fn uploads_go_together_up_to_what_the_account_takes_with_free_names() {
        let (fake, sv, mut s) = on_fake("enviar");
        let local = local_tree("enviar");
        for i in 0..300 {
            write(&local.join(format!("f{i:03}.bin")), 1000 + i, i);
        }
        // Too big for a batch: alone, in one request of its own.
        write(&local.join("grande.bin"), (8 << 20) + 10, 1);
        for i in 0..5 {
            write(&local.join(format!("Pasta/p{i}.bin")), 500, i);
        }
        // Taken on Dropbox already: the upload gets "f000 (2).bin", as one by one.
        fake.put_file("/U/f000.bin", b"outro");
        open(&mut s, 0, Location::Local(local.clone()));
        open(&mut s, 1, folder(&sv, vec!["U".into()]));
        paste_now(&mut s, &[], ClipMode::Copy);
        settle(&mut s);
        let st = stages(&s, 302);
        assert_eq!(count(&st, Stage::Done), 302, "{}", s.status);
        let finished = fake.batch_sizes("files/upload_session/finish_batch_v2");
        assert!(finished.iter().all(|n| *n <= 250), "{finished:?}");
        // The 300 rows, and the folder's five in a batch of their own; a run a worker was left
        // with a single row of goes in one request, as one by one, and so does the big one.
        let alone = fake.count_since(&0, "files/upload");
        assert_eq!(finished.iter().sum::<usize>() + alone, 306, "{finished:?} + {alone}");
        assert!(finished.len() < 20, "{finished:?}");
        assert_eq!(fake.content("/U/f000 (2).bin"), std::fs::read(local.join("f000.bin")).unwrap());
        assert_eq!(fake.content("/U/f000.bin"), b"outro");
        assert_eq!(fake.content("/U/f299.bin"), std::fs::read(local.join("f299.bin")).unwrap());
        assert_eq!(fake.content("/U/grande.bin"), std::fs::read(local.join("grande.bin")).unwrap());
        assert_eq!(fake.content("/U/Pasta/p4.bin"), std::fs::read(local.join("Pasta/p4.bin")).unwrap());
        // Each row's bar full.
        for t in &s.transfers[s.transfers.len() - 302..] {
            assert_eq!(t.progress.bytes.load(std::sync::atomic::Ordering::Relaxed), t.progress.total.load(std::sync::atomic::Ordering::Relaxed));
        }
        let _ = std::fs::remove_dir_all(local);
    }

    #[test]
    fn a_folder_cancelled_between_its_batches_goes_away_whole() {
        let (fake, sv, mut s) = on_fake("cancelar");
        let local = local_tree("cancelar");
        for i in 0..300 {
            write(&local.join(format!("Grande/g{i:03}.bin")), 200, i);
        }
        open(&mut s, 0, Location::Local(local.clone()));
        open(&mut s, 1, folder(&sv, vec!["V".into()]));
        fake.make_dir("/V");
        fake.delay(150);
        paste_now(&mut s, &["Grande"], ClipMode::Copy);
        // The folder made, its first 250 files arrived at Dropbox: Cancelar.
        let start = Instant::now();
        while fake.batch_sizes("files/upload_session/append_batch").is_empty() {
            assert!(start.elapsed() < Duration::from_secs(20));
            std::thread::sleep(Duration::from_millis(10));
        }
        s.transfers.last().unwrap().progress.cancel();
        settle(&mut s);
        fake.delay(0);
        assert_eq!(stages(&s, 1), [Stage::Cancelled], "{}", s.status);
        // The first batch committed, none after it, and what arrived taken away with the folder.
        assert_eq!(fake.batch_sizes("files/upload_session/append_batch"), [250]);
        assert_eq!(fake.batch_sizes("files/upload_session/finish_batch_v2"), [250]);
        assert!(!fake.has("/V/Grande"));
        let _ = std::fs::remove_dir_all(local);
    }
}
