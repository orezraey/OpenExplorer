//! Telegram accounts in the panes. Each account shows in the tree under Contas and opens like a
//! server (see `servers.rs`): its files live in a private channel of the account, one message
//! each (see `openexplorer_telegram`). Copying, moving, renaming, new folders, searching and
//! dragging go through the servers' code, with the account as the connection
//! (`openexplorer_remote`'s `telegram.rs`). What only Telegram has lives here: the login, in
//! steps (the app each person registers at my.telegram.org, the phone, the code Telegram sends,
//! the two-step password), and removing an account.
//!
//! Deleting is for good, after asking, as on a server: Telegram has no bin (Telegram-Drive does
//! the same).

use std::sync::Mutex;

use openexplorer_i18n::{t, tr};
use openexplorer_remote::Server;
use openexplorer_telegram as telegram;
use rune::{El, TextEdit, Tx, col, row, text};
use rune_kit::{Button, ButtonVariant, Kit};

use crate::model::Location;
use crate::servers::{Form, folder};
use crate::{Msg, State, background, catch_panic};

/// The accounts' list, beside the app's other settings.
pub fn store() -> telegram::Store {
    telegram::Store::at(openexplorer_gdrive::config_dir().join("telegram-accounts.json"))
}

/// The servers the connected accounts show as.
pub fn servers(store: &telegram::Store) -> Vec<Server> {
    store.accounts().into_iter().map(|a| Server::telegram(a.user_id, &a.name)).collect()
}

/// Where each account's tree is kept, sealed (see `openexplorer_telegram::cache`).
pub fn cache_dir() -> std::path::PathBuf {
    openexplorer_gdrive::local_dir().join("telegram")
}

/// The user id a Telegram server stands for.
fn user_id(server: &Server) -> Option<i64> {
    server.user.parse().ok()
}

// ---------------------------------------------------------------------------------------------
// Connecting

/// Where the login in the dialog stands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Stage {
    /// The app's api_id and api_hash and the phone.
    #[default]
    Start,
    /// The code Telegram sent.
    Code,
    /// The two-step verification password, with the hint the person set.
    Password { hint: String },
}

/// The login under way between its steps: each step runs on a thread of its own.
static PENDING: Mutex<Option<telegram::Login>> = Mutex::new(None);

fn pending() -> std::sync::MutexGuard<'static, Option<telegram::Login>> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Counts the logins dropped (Recomeçar, Cancelar, another dialog): a step that was running then
/// doesn't put its login back, it ends with it.
static DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Keeps `login` for the next step, unless the login was dropped since step `generation` began
/// (then it ends here, on this background thread).
fn keep(login: telegram::Login, generation: u64) {
    let mut slot = pending();
    if DROPPED.load(std::sync::atomic::Ordering::SeqCst) == generation && slot.is_none() {
        *slot = Some(login);
    } else {
        drop(slot);
        drop(login);
    }
}

/// What a step of the login brought back.
pub enum Outcome {
    CodeSent,
    Password(String),
    Done(telegram::StoredAccount),
}

/// What background work on Telegram brought back.
pub enum Answer {
    /// Step `attempt` of the Adicionar conta dialog's login ended.
    Step { attempt: u64, result: Result<Outcome, String> },
    /// Account `user_id` asked its channel what changed: whether anything did.
    Polled { user_id: i64, result: Result<bool, String> },
}

pub fn arrived(s: &mut State, a: Answer) {
    match a {
        Answer::Step { attempt, result } => stepped(s, attempt, result),
        Answer::Polled { user_id, result } => polled(s, user_id, result),
    }
}

/// Drops the login under way, on a thread: it holds a runtime, which mustn't end on the UI's.
fn drop_pending() {
    DROPPED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if let Some(old) = pending().take() {
        std::thread::spawn(move || drop(old));
    }
}

/// The dialog set for Telegram, at the first step.
pub fn prepare(f: &mut Form) {
    f.host = TextEdit::new("telegram.org");
    f.telegram = Stage::Start;
    f.client_id = TextEdit::new("");
    f.client_secret = TextEdit::new("").secure();
    f.client_id.focused = true;
    drop_pending();
}

/// Drops any Telegram login waiting for a code or password when the account window closes.
pub fn abort_pending() {
    drop_pending();
}

/// Cancelar, or the window closed: the login under way goes too.
fn cancel(s: &mut State, _: &mut Tx) {
    drop_pending();
    s.close_account_form();
}

/// Back to the first step, the login dropped.
fn restart(s: &mut State) {
    if let Some(f) = &mut s.account_form {
        f.telegram = Stage::Start;
        f.checking = None;
        f.error = None;
    }
    drop_pending();
}

/// Conectar (or Entrar) in the window: the step the login is at, on a thread.
pub fn submit(s: &mut State) {
    let Some(f) = &mut s.account_form else { return };
    if f.checking.is_some() {
        return;
    }
    let attempt = s.servers.attempts + 1;
    let generation = DROPPED.load(std::sync::atomic::Ordering::SeqCst);
    let work: Box<dyn FnOnce() -> Result<Outcome, String> + Send> = match f.telegram.clone() {
        Stage::Start => {
            let Ok(api_id) = f.client_id.text.trim().parse::<i32>() else {
                f.error = Some(t("O api_id é um número (em my.telegram.org › API development tools).", "The api_id is a number (on my.telegram.org › API development tools).").into());
                return;
            };
            let api_hash = f.client_secret.text.trim().to_string();
            let phone = f.user.text.trim().to_string();
            if phone.chars().filter(char::is_ascii_digit).count() < 6 {
                f.error = Some(t("Digite o telefone da conta, com o código do país (+55 …).", "Enter the account's phone, with the country code (+1 …).").into());
                return;
            }
            Box::new(move || {
                let login = telegram::Login::begin(api_id, &api_hash, &phone)?;
                drop(pending().take());
                keep(login, generation);
                Ok(Outcome::CodeSent)
            })
        }
        Stage::Code => {
            let code = f.code.text.trim().to_string();
            if code.is_empty() {
                f.error = Some(t("Digite o código que o Telegram mandou.", "Enter the code Telegram sent.").into());
                return;
            }
            Box::new(move || step(generation, |l| l.code(&code)))
        }
        Stage::Password { .. } => {
            let password = f.password.text.clone();
            if password.is_empty() {
                f.error = Some(t("Digite a senha da verificação em duas etapas.", "Enter the two-step verification password.").into());
                return;
            }
            Box::new(move || step(generation, |l| l.password(&password)))
        }
    };
    s.servers.attempts = attempt;
    f.checking = Some(attempt);
    f.error = None;
    background(s, move |emit| {
        let result = catch_panic(t("entrar no Telegram", "sign in to Telegram"), work);
        emit(Msg::Telegram(Answer::Step { attempt, result }));
    });
}

/// A step of the login under way; the login stays for the next try or step unless it ended.
fn step(generation: u64, run: impl FnOnce(&mut telegram::Login) -> Result<telegram::Step, String>) -> Result<Outcome, String> {
    let mut login = pending().take().ok_or_else(|| t("O login expirou. Comece de novo.", "The login expired. Start again.").to_string())?;
    match run(&mut login) {
        Ok(telegram::Step::Done(_, stored)) => Ok(Outcome::Done(stored)),
        Ok(telegram::Step::Password { hint }) => {
            keep(login, generation);
            Ok(Outcome::Password(hint))
        }
        Err(e) => {
            keep(login, generation);
            Err(e)
        }
    }
}

fn stepped(s: &mut State, attempt: u64, result: Result<Outcome, String>) {
    // Signed in: the account is kept even if the window closed meanwhile, or it would be
    // connected with nothing in the app to remove it by.
    if let Ok(Outcome::Done(stored)) = result {
        return connected(s, stored);
    }
    let Some(f) = &mut s.account_form else { return };
    if f.checking != Some(attempt) {
        return;
    }
    f.checking = None;
    match result {
        Err(e) => {
            openexplorer_gdrive::log(&format!("telegram: login: {e}"));
            f.error = Some(e);
        }
        Ok(Outcome::CodeSent) => {
            f.telegram = Stage::Code;
            f.code = TextEdit::new("");
            f.code.focused = true;
            f.error = None;
        }
        Ok(Outcome::Password(hint)) => {
            f.telegram = Stage::Password { hint };
            f.password = TextEdit::new("").secure();
            f.password.focused = true;
            f.error = None;
        }
        Ok(Outcome::Done(_)) => unreachable!("handled above"),
    }
    s.notify_account();
}

fn connected(s: &mut State, stored: telegram::StoredAccount) {
    let name = stored.name.clone();
    if let Err(e) = s.servers.telegram.add(stored.clone()) {
        crate::fail(s, e);
        return;
    }
    openexplorer_gdrive::log(&format!("telegram: conta conectada: {name}"));
    if s.account_form.as_ref().is_some_and(|f| f.provider == crate::servers::Provider::Telegram) {
        s.close_account_form();
    }
    let server = Server::telegram(stored.user_id, &name);
    s.servers.list.retain(|sv| sv.id != server.id);
    s.servers.list.push(server.clone());
    s.servers.pool.forget(&server.id);
    s.tree.server_changed(&server.id);
    s.tree.server_opened(&server.id);
    let p = s.active;
    crate::navigate(s, p, folder(&server, Vec::new()));
    s.status = tr!("Conta do Telegram conectada: {name}", "Telegram account connected: {name}");
}

/// Remover conta: the session ends on Telegram's side and leaves the credential store, the tree
/// leaves the disk; the channel and its files stay in the account. Whether it left the list.
pub fn remove(s: &mut State, server: &Server) -> bool {
    let Some(id) = user_id(server) else { return false };
    if let Err(e) = s.servers.telegram.remove(id) {
        crate::fail(s, e);
        return false;
    }
    telegram::disconnect(id);
    true
}

/// What the Excluir question says in a Telegram account.
pub fn delete_note() -> &'static str {
    t(
        "O Telegram não tem lixeira: os itens saem do canal de armazenamento para sempre.",
        "Telegram has no bin: the items leave the storage channel for good.",
    )
}

/// The folders inside `dir` as the account's tree holds them, with no request.
pub fn kept_folders(server: &Server, dir: &str, shown: impl Fn(&str) -> bool) -> Option<Vec<String>> {
    let items = telegram::cached(user_id(server)?)?.kept(dir)?;
    Some(items.into_iter().filter(|i| i.is_dir() && shown(&i.name)).map(|i| i.name).collect())
}

// ---------------------------------------------------------------------------------------------
// Changes made elsewhere

/// How often an account a pane shows asks its channel what changed (one small request), and
/// how long after a failed asking.
const EVERY: f64 = 15.0;
const RETRY: f64 = 60.0;

/// When each account a pane shows next asks what changed, while the window has the focus.
#[derive(Default)]
pub struct Watch {
    away: bool,
    /// Account → (when it asks next, app clock; asking now).
    turns: std::collections::HashMap<i64, (f64, bool)>,
    /// Accounts whose change log brought news the panes don't show yet: shown at the next tick
    /// the account's tree is free.
    unshown: std::collections::HashSet<i64>,
}

/// The Telegram accounts the panes show (not a search's results).
fn on_screen(s: &State) -> Vec<i64> {
    let mut ids: Vec<i64> = s
        .panes
        .iter()
        .filter_map(|p| match &p.loc {
            Location::Server { server, find: None, .. } if server.is_telegram() => user_id(server),
            _ => None,
        })
        .collect();
    ids.dedup();
    ids
}

/// The window gained or lost the focus. Back in front, each account asks at once.
pub fn window_focus(s: &mut State, focused: bool) {
    let w = &mut s.servers.telegram_watch;
    w.away = !focused;
    if focused {
        for turn in w.turns.values_mut() {
            turn.0 = 0.0;
        }
    }
}

/// Each account on screen whose turn came asks its channel what changed, on a thread.
pub fn tick(s: &mut State, now: f64) {
    let ids = on_screen(s);
    let w = &mut s.servers.telegram_watch;
    w.turns.retain(|id, _| ids.contains(id));
    w.unshown.retain(|id| ids.contains(id));
    let unshown: Vec<i64> = w.unshown.iter().copied().collect();
    for id in unshown {
        if (0..crate::layout::MAX_PANES).all(|p| reload(s, p, id)) {
            s.servers.telegram_watch.unshown.remove(&id);
        }
    }
    let w = &mut s.servers.telegram_watch;
    if w.away {
        return;
    }
    let mut due = Vec::new();
    for id in ids {
        let turn = w.turns.entry(id).or_insert((now + EVERY, false));
        if !turn.1 && now >= turn.0 {
            turn.1 = true;
            due.push(id);
        }
    }
    for id in due {
        background(s, move |emit| {
            // Only an account already open: opening one is the listing's job.
            let result = match telegram::cached(id) {
                Some(a) => catch_panic(t("ver o que mudou no Telegram", "see what changed on Telegram"), || a.refresh()),
                None => Ok(false),
            };
            emit(Msg::Telegram(Answer::Polled { user_id: id, result }));
        });
    }
}

/// When the next asking is due; every second while Telegram makes a transfer wait, so its row
/// counts down.
pub fn next_ask(s: &State) -> Option<f64> {
    let w = &s.servers.telegram_watch;
    let asks = if w.away { None } else { w.turns.values().filter(|t| !t.1).map(|t| t.0).reduce(f64::min) };
    let retry = (!w.unshown.is_empty()).then_some(s.now + 0.25);
    let waiting = s.servers.list.iter().filter(|sv| sv.is_telegram()).filter_map(user_id).any(|id| telegram::cached(id).is_some_and(|a| a.waiting_until().is_some()));
    let tick = waiting.then_some(s.now + 1.0);
    [asks, tick, retry].into_iter().flatten().reduce(f64::min)
}

fn polled(s: &mut State, id: i64, result: Result<bool, String>) {
    let now = s.now;
    if let Some(turn) = s.servers.telegram_watch.turns.get_mut(&id) {
        turn.1 = false;
        turn.0 = now + if result.is_ok() { EVERY } else { RETRY };
    }
    match result {
        Err(e) => openexplorer_gdrive::log(&format!("telegram: ver o que mudou: {e}")),
        Ok(false) => {}
        Ok(true) => {
            if !(0..crate::layout::MAX_PANES).all(|p| reload(s, p, id)) {
                s.servers.telegram_watch.unshown.insert(id);
            }
        }
    }
}

/// Shows pane `p` again from the account's tree, if it shows account `id`: the tree is already
/// current, so no request. The selection stays, entries being matched by name. `false` only when
/// it should and can't yet: the tree is busy (being brought up to date on another thread).
fn reload(s: &mut State, p: usize, id: i64) -> bool {
    let Location::Server { server, path, find: None } = &s.panes[p].loc else { return true };
    if !server.is_telegram() || user_id(server) != Some(id) || s.panes[p].loading {
        return true;
    }
    let dir = server.path(path);
    let Some(account) = telegram::cached(id) else { return true };
    let Some(items) = account.kept(&dir) else { return false };
    let show_hidden = s.settings.show_hidden;
    let entries = items
        .into_iter()
        .filter(|i| show_hidden || !i.name.starts_with('.'))
        .map(|i| {
            let modified = (i.modified > 0).then(|| std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(i.modified as u64));
            crate::servers::entry(openexplorer_remote::Item { is_dir: i.is_dir(), size: (!i.is_dir()).then_some(i.size), modified, name: i.name, link: false })
        })
        .collect();
    s.panes[p].set_entries(Ok(entries));
    crate::drop_stale_rename(s, p);
    true
}

/// What a running row of the queue says while Telegram makes its account wait: "Aguardando o
/// Telegram · 0:42". `None` for a row that isn't on a Telegram account, or one not waiting.
pub fn waiting_note(from: &str, to: &str) -> Option<String> {
    let id = [from, to].into_iter().find_map(|end| {
        let (_, rest) = end.rsplit_once("[telegram://")?;
        rest.trim_end_matches(']').parse::<i64>().ok()
    })?;
    let until = telegram::cached(id)?.waiting_until()?;
    let left = (until - telegram::account::now()).max(0) as f64;
    Some(tr!("Aguardando o Telegram · {}", "Waiting for Telegram · {}", crate::transfers::hms(left)))
}

// ---------------------------------------------------------------------------------------------
// The dialog

pub fn summary() -> (&'static str, &'static str) {
    (t("Canal privado na sua conta · api_id seu, de my.telegram.org", "A private channel in your account · your own api_id from my.telegram.org"), "MTProto")
}

/// The step of the login the window is at: its fields, what to do, and the button.
pub fn form(k: &Kit, f: &Form, banner: El, eye: El) -> (El, El) {
    fn part(s: &mut State, pick: fn(&mut Form) -> &mut TextEdit) -> &mut TextEdit {
        match &mut s.account_form {
            Some(f) => pick(f),
            None => &mut s.name_edit,
        }
    }
    let labeled = |key: &'static str, label: &'static str, input: El| col().key(key).w_full().gap(4.0).child(text(label).font(k.ty.text_small_medium).color(k.c.text_base)).child(input);
    let note = |words: String| text(words).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0);
    let checking = f.checking.is_some();
    let mut inner = col().w_full().gap(12.0).child(banner);
    let label = match &f.telegram {
        Stage::Start => {
            let site = Button::new(t("Abrir my.telegram.org", "Open my.telegram.org"))
                .icon(&crate::icons::EXTERNAL_LINK)
                .variant(ButtonVariant::Neutral)
                .on_press_tx(|s: &mut State, tx: &mut Tx| {
                    if let Err(e) = tx.open_url("https://my.telegram.org/apps") {
                        crate::fail(s, tr!("Não consegui abrir o navegador: {e:?}", "Couldn't open the browser: {e:?}"));
                    }
                })
                .build(k).cursor(rune::Cursor::Pointer);
            let api_id = rune_kit::TextInput::new(&f.client_id)
                .width(None)
                .placeholder("1234567")
                .on_edit(|s: &mut State| part(s, |f| &mut f.client_id))
                .on_submit(|s: &mut State, _: &mut Tx| submit(s))
                .build(k);
            let api_hash = rune_kit::TextInput::new(&f.client_secret)
                .width(None)
                .placeholder(t("32 letras e números", "32 letters and digits"))
                .on_edit(|s: &mut State| part(s, |f| &mut f.client_secret))
                .on_submit(|s: &mut State, _: &mut Tx| submit(s))
                .build(k);
            let phone = rune_kit::TextInput::new(&f.user)
                .width(None)
                .placeholder("+55 11 99999-8888")
                .on_edit(|s: &mut State| part(s, |f| &mut f.user))
                .on_submit(|s: &mut State, _: &mut Tx| submit(s))
                .build(k);
            inner = inner
                .child(note(
                    t(
                        "O Open Explorer usa a API do Telegram para guardar arquivos num canal privado da sua conta, chamado \"Open Explorer\", que só você vê: cada pasta e cada arquivo é uma mensagem dele. Arquivos acima de 2 GB (4 GB com Premium) vão em partes, juntadas de volta ao baixar.",
                        "Open Explorer uses the Telegram API to keep files in a private channel of your account, named \"Open Explorer\", that only you see: each folder and each file is one of its messages. Files over 2 GB (4 GB with Premium) go in parts, joined again when downloaded.",
                    )
                    .into(),
                ))
                .child(
                    col()
                        .key("telegram-app")
                        .w_full()
                        .padding(12.0)
                        .gap(8.0)
                        .radius(k.radius.md)
                        .fill(k.c.bg_base)
                        .border(1.0, k.c.border_weak)
                        .child(row().w_full().center_y().child(text(t("Seu app do Telegram", "Your Telegram app")).font(k.ty.text_small_medium).color(k.c.text_base).grow(1.0)).child(site))
                        .child(
                            text(t(
                                "Entre em my.telegram.org com o seu número, abra API development tools e crie um app (qualquer nome, sem a palavra Telegram; plataforma Desktop). Copie o api_id e o api_hash para cá. Cada pessoa usa o próprio.",
                                "Sign in at my.telegram.org with your number, open API development tools and create an app (any name without the word Telegram; platform Desktop). Copy the api_id and api_hash here. Each person uses their own.",
                            ))
                            .font(k.ty.caption())
                            .color(k.c.text_muted)
                            .wrap()
                            .w_full()
                            .min_w(0.0),
                        )
                        .child(labeled("tg-api-id", "api_id", api_id))
                        .child(labeled("tg-api-hash", "api_hash", api_hash)),
                )
                .child(labeled("tg-phone", t("Telefone da conta", "The account's phone"), phone));
            t("Enviar código", "Send code")
        }
        Stage::Code => {
            let code = rune_kit::TextInput::new(&f.code)
                .width(None)
                .placeholder("12345")
                .on_edit(|s: &mut State| part(s, |f| &mut f.code))
                .on_submit(|s: &mut State, _: &mut Tx| submit(s))
                .build(k);
            inner = inner
                .child(note(tr!(
                    "O Telegram mandou um código para {} — no app do Telegram, em outro aparelho conectado (ou por SMS).",
                    "Telegram sent a code for {} — in the Telegram app, on another device that's signed in (or by SMS).",
                    f.user.text.trim()
                )))
                .child(labeled("tg-code", t("Código", "Code"), code));
            t("Entrar", "Sign in")
        }
        Stage::Password { hint } => {
            let pw = rune_kit::TextInput::new(&f.password)
                .width(None)
                .revealed(f.reveal_password)
                .trailing(eye)
                .on_edit(|s: &mut State| part(s, |f| &mut f.password))
                .on_submit(|s: &mut State, _: &mut Tx| submit(s))
                .build(k);
            let words = if hint.is_empty() {
                t("Esta conta tem verificação em duas etapas: digite a senha dela.", "This account has two-step verification: enter its password.").to_string()
            } else {
                tr!("Esta conta tem verificação em duas etapas: digite a senha dela (dica: {hint}).", "This account has two-step verification: enter its password (hint: {hint}).")
            };
            inner = inner.child(note(words)).child(labeled("tg-password", t("Senha", "Password"), pw));
            t("Entrar", "Sign in")
        }
    };
    inner = inner.child(note(
        t(
            "A sessão fica no cofre de credenciais do Windows, nunca em arquivo; o api_hash e o código não saem deste computador a não ser para o Telegram.",
            "The session is kept in Windows' credential store, never in a file; the api_hash and the code leave this computer only for Telegram.",
        )
        .into(),
    ));
    if let Some(e) = &f.error {
        inner = inner.child(text(e.clone()).key("error").font(k.ty.text_small).color(k.c.danger.fg).wrap().w_full().min_w(0.0));
    }
    let go = if checking {
        Button::new(t("Aguarde…", "Wait…")).variant(ButtonVariant::Submit).disabled(true).build(k).cursor(rune::Cursor::Pointer)
    } else {
        Button::new(label).variant(ButtonVariant::Submit).on_press(submit).build(k).cursor(rune::Cursor::Pointer).default_button()
    };
    let back = if f.telegram == Stage::Start {
        Button::new(t("Voltar", "Back")).icon(&crate::icons::BACK).variant(ButtonVariant::Neutral).disabled(checking).on_press(crate::servers::back_to_pick).build(k).cursor(rune::Cursor::Pointer)
    } else {
        Button::new(t("Recomeçar", "Start over")).icon(&crate::icons::BACK).variant(ButtonVariant::Neutral).disabled(checking).on_press(restart).build(k).cursor(rune::Cursor::Pointer)
    };
    let actions = row()
        .key("actions")
        .w_full()
        .gap(8.0)
        .center_y()
        .child(back)
        .child(rune::el().grow(1.0))
        .child(Button::new(t("Cancelar", "Cancel")).variant(ButtonVariant::Neutral).on_press_tx(cancel).build(k).cursor(rune::Cursor::Pointer))
        .child(go);
    (inner, actions)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use openexplorer_telegram::fake::Fake;

    use super::*;
    use crate::actions::{self, Clip, ClipMode};
    use crate::model::{Entry, Location};

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

    /// A state whose only server is an account on a channel in memory, open as `id`.
    fn on_fake(id: i64) -> (Arc<Fake>, Server, State) {
        let fake = Arc::new(Fake::new());
        telegram::open::adopt(id, Arc::new(telegram::Account::open(id, "Ana", Box::new(fake.clone()), None)));
        let server = Server::telegram(id, "Ana");
        let mut s = crate::init();
        s.cache = crate::cache::Cache::memory_only();
        s.servers.list = vec![server.clone()];
        s.settings.clear_completed = false;
        (fake, server, s)
    }

    fn open(s: &mut State, p: usize, loc: Location) {
        crate::switch(s, p, loc);
        s.status = "listado".into();
        settle(s);
    }

    fn names(s: &State, p: usize) -> Vec<String> {
        let mut n: Vec<String> = s.panes[p].entries.iter().map(|e| e.name.clone()).collect();
        n.sort();
        n
    }

    /// Picks `wanted` in pane 0 and pastes them into pane 1, then waits.
    fn paste(s: &mut State, wanted: &[&str], mode: ClipMode) {
        let items: Vec<Entry> = s.panes[0].entries.iter().filter(|e| wanted.contains(&e.name.as_str())).cloned().collect();
        assert_eq!(items.len(), wanted.len(), "{wanted:?} in {:?}", names(s, 0));
        s.status.clear();
        s.clip = Some(Clip { mode, from: s.panes[0].loc.clone(), items, picked_at: 0.0 });
        actions::paste(s, 1);
        settle(s);
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oe-app-telegram-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Projeto/src")).unwrap();
        std::fs::write(dir.join("Projeto/leia-me.txt"), "olá").unwrap();
        std::fs::write(dir.join("Projeto/vazio.txt"), "").unwrap();
        for i in 0..14 {
            std::fs::write(dir.join(format!("Projeto/src/m{i:02}.rs")), format!("fn m{i}() {{}}")).unwrap();
        }
        dir
    }

    #[test]
    fn a_folder_goes_up_moves_copies_and_is_deleted_for_good() {
        let (fake, sv, mut s) = on_fake(7001);
        let dir = scratch("tudo");
        open(&mut s, 0, Location::Local(dir.clone()));
        open(&mut s, 1, folder(&sv, Vec::new()));

        // Up: the folders made, the small files in groups, the empty one as a text message.
        paste(&mut s, &["Projeto"], ClipMode::Copy);
        assert!(!s.status.contains("erro"), "{}", s.status);
        open(&mut s, 1, folder(&sv, vec!["Projeto".into()]));
        assert_eq!(names(&s, 1), ["leia-me.txt", "src", "vazio.txt"]);
        open(&mut s, 1, folder(&sv, vec!["Projeto".into(), "src".into()]));
        assert_eq!(s.panes[1].entries.len(), 14);
        assert!(fake.groups.load(Ordering::SeqCst) >= 2, "small files go in groups");
        std::fs::remove_dir_all(&dir).unwrap();

        // Within the account: a copy on Telegram's side, then a move.
        telegram::cached(7001).unwrap().mkdir("/Destino").unwrap();
        open(&mut s, 0, folder(&sv, Vec::new()));
        open(&mut s, 1, folder(&sv, vec!["Destino".into()]));
        paste(&mut s, &["Projeto"], ClipMode::Copy);
        assert!(!s.status.contains("erro"), "{}", s.status);
        open(&mut s, 1, folder(&sv, vec!["Destino".into(), "Projeto".into(), "src".into()]));
        assert_eq!(s.panes[1].entries.len(), 14);
        assert!(fake.copies.load(Ordering::SeqCst) >= 14, "no bytes sent again");
        open(&mut s, 0, folder(&sv, vec!["Projeto".into()]));
        open(&mut s, 1, folder(&sv, Vec::new()));
        paste(&mut s, &["leia-me.txt"], ClipMode::Cut);
        assert!(!s.status.contains("erro"), "{}", s.status);
        open(&mut s, 0, folder(&sv, vec!["Projeto".into()]));
        assert_eq!(names(&s, 0), ["src", "vazio.txt"]);

        // Excluir asks to delete for good: Telegram has no bin.
        open(&mut s, 0, folder(&sv, Vec::new()));
        let id = s.panes[0].entries.iter().find(|e| e.name == "Destino").unwrap().id.clone();
        s.panes[0].selected = [id].into();
        actions::ask_delete(&mut s, 0);
        assert!(matches!(s.dialog, Some(actions::Dialog::Purge { .. })), "Excluir is for good");
        actions::purge(&mut s);
        settle(&mut s);
        open(&mut s, 0, folder(&sv, Vec::new()));
        assert_eq!(names(&s, 0), ["Projeto", "leia-me.txt"]);
        // Projeto, src, vazio.txt, the 14 files, and leia-me.txt at the top.
        assert_eq!(fake.messages().len(), 18);
    }

    #[test]
    fn changes_made_elsewhere_show_by_themselves_while_the_window_is_in_front() {
        let (fake, sv, mut s) = on_fake(7003);
        open(&mut s, 1, folder(&sv, Vec::new()));
        assert!(s.panes[1].entries.is_empty());
        // From a phone: a document forwarded to the channel.
        fake.post("encaminhado", Some(("doc.pdf", b"pdf")));
        let asks = |s: &State| s.servers.telegram_watch.turns.values().filter(|t| t.1).count();
        // In the background nothing is asked, however long.
        window_focus(&mut s, false);
        crate::poll(&mut s, 1000.0, &mut rune::Tx::default());
        assert_eq!(asks(&s), 0);
        assert!(s.panes[1].entries.is_empty());
        // Back in front, the account asks at once, and the pane shows what came.
        window_focus(&mut s, true);
        let start = Instant::now();
        while !s.panes[1].entries.iter().any(|e| e.name == "doc.pdf") {
            crate::poll(&mut s, 1000.0, &mut rune::Tx::default());
            assert!(start.elapsed() < Duration::from_secs(10), "the change never showed");
            std::thread::sleep(Duration::from_millis(20));
        }
        // Its next turn is a while off.
        assert!(next_ask(&s).is_some_and(|at| at >= 1000.0 + EVERY - 1.0));
        // Nothing waits on Telegram: a row going there says nothing special.
        assert_eq!(waiting_note(r"C:\a.txt", "Ana\\Fotos [telegram://7003]"), None);
        // Telegram asks to wait 42 s: the row says so, and the frame wakes each second.
        fake.waiting.store(telegram::account::now() + 42, Ordering::SeqCst);
        let note = waiting_note(r"C:\a.txt", "Ana\\Fotos [telegram://7003]").unwrap();
        assert!(note.starts_with("Aguardando o Telegram"), "{note}");
        assert!(next_ask(&s).is_some_and(|at| at <= s.now + 1.0));
        assert_eq!(waiting_note(r"C:\a.txt", r"D:\b.txt"), None);
    }

    #[test]
    fn the_login_dialog_walks_its_steps() {
        let (_, _, mut s) = on_fake(7002);
        let mut f = crate::servers::Form::for_provider(crate::servers::Provider::Telegram);
        f.checking = Some(5);
        s.open_account_form(f);
        let stage = |s: &State| match &s.account_form {
            Some(f) => (f.telegram.clone(), f.checking, f.error.clone()),
            None => panic!("the dialog closed"),
        };
        let wait = |s: &mut State, n: u64| {
            if let Some(f) = &mut s.account_form {
                f.checking = Some(n);
            }
        };
        // A stale answer changes nothing.
        stepped(&mut s, 4, Ok(Outcome::CodeSent));
        assert_eq!(stage(&s), (Stage::Start, Some(5), None));
        stepped(&mut s, 5, Ok(Outcome::CodeSent));
        assert_eq!(stage(&s), (Stage::Code, None, None));
        wait(&mut s, 6);
        stepped(&mut s, 6, Err("O código não confere.".into()));
        assert_eq!(stage(&s), (Stage::Code, None, Some("O código não confere.".into())));
        wait(&mut s, 7);
        stepped(&mut s, 7, Ok(Outcome::Password("gato".into())));
        assert_eq!(stage(&s).0, Stage::Password { hint: "gato".into() });
        // Signed in with the dialog closed meanwhile: the account is still kept.
        s.close_account_form();
        let dir = std::env::temp_dir().join(format!("oe-app-telegram-login-{}", std::process::id()));
        s.servers.telegram = telegram::Store::at(dir.join("telegram-accounts.json"));
        let stored = telegram::StoredAccount { user_id: 7002, name: "Ana".into(), channel_id: 1, channel_hash: 2, premium: false };
        stepped(&mut s, 8, Ok(Outcome::Done(stored)));
        assert_eq!(s.servers.telegram.accounts().len(), 1);
        assert!(s.servers.list.iter().any(|sv| sv.id == "telegram:7002"));
        assert!(s.status.contains("conectada"), "{}", s.status);
        let _ = std::fs::remove_dir_all(dir);
    }
}
