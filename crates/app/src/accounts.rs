//! The cloud accounts that show in the panes as servers: MEGA, Dropbox, Backblaze B2,
//! OneDrive and Telegram. Their servers go through the servers' code (`servers.rs`) like any other, with the
//! account as the connection; what differs from one to another (where new items can't go, a
//! bin, how deleting works, the storage, a refresh from the change log, links) is asked here,
//! and here sent on to the account's own module (`mega.rs`, `dropbox.rs`, `b2.rs`,
//! `onedrive.rs`). The matches are exhaustive: a new kind of account fails to build until each
//! question has its answer for it.

use std::borrow::Cow;

use openexplorer_i18n::tr;
use openexplorer_remote::{Protocol, Server};
use rune::Tx;

use crate::model::{Entry, Location};
use crate::{State, b2, dropbox, mega, onedrive, telegram};

/// The kind of account a server is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Account {
    Mega,
    Dropbox,
    B2,
    OneDrive,
    Telegram,
}

/// A context menu item's action, on pane `p`.
pub type MenuAction = fn(&mut State, usize, &mut Tx);

impl Account {
    /// In the order they're asked each frame (see [`tick`]).
    pub const ALL: [Account; 5] = [Account::Mega, Account::Dropbox, Account::OneDrive, Account::B2, Account::Telegram];

    /// The account server `server` is, `None` for a server reached by a protocol of its own
    /// (SFTP, FTP, WebDAV).
    pub fn of(server: &Server) -> Option<Account> {
        match server.protocol {
            Protocol::Mega => Some(Account::Mega),
            Protocol::Dropbox => Some(Account::Dropbox),
            Protocol::B2 => Some(Account::B2),
            Protocol::OneDrive => Some(Account::OneDrive),
            Protocol::Telegram => Some(Account::Telegram),
            Protocol::Ftp | Protocol::Ftps | Protocol::Sftp | Protocol::Dav | Protocol::Davs => None,
        }
    }

    /// The account `loc` is in.
    pub fn at(loc: &Location) -> Option<Account> {
        match loc {
            Location::Server { server, .. } => Account::of(server),
            _ => None,
        }
    }

    /// The account a server's id names (see `Server::mega` and the others), for what keeps
    /// only the id.
    pub fn of_id(id: &str) -> Option<Account> {
        Account::ALL.into_iter().find(|a| id.starts_with(a.id_prefix()))
    }

    fn id_prefix(self) -> &'static str {
        match self {
            Account::Mega => "mega:",
            Account::Dropbox => "dropbox:",
            Account::B2 => "b2:",
            Account::OneDrive => "onedrive:",
            Account::Telegram => "telegram:",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Starting and closing

/// As the app starts: each crate logs to the app's log and keeps its sealed caches under
/// `%LOCALAPPDATA%\OpenExplorer` (see `caches.rs`).
pub fn start() {
    let local = openexplorer_gdrive::local_dir();
    openexplorer_mega::set_log(openexplorer_gdrive::log);
    // Each MEGA account's tree, sealed, so a start reads only what changed since.
    openexplorer_mega::set_cache_dir(Some(local.join("mega")));
    openexplorer_dropbox::set_log(openexplorer_gdrive::log);
    // Each Dropbox account's listings, sealed, so a folder opened before isn't read again.
    openexplorer_dropbox::cache::set_dir(Some(local.join("dropbox")));
    openexplorer_b2::set_log(openexplorer_gdrive::log);
    // Each B2 key's folder listings, sealed, so a folder seen before shows at once.
    openexplorer_b2::set_cache_dir(Some(b2::cache_dir()));
    // Which file each large B2 upload sends, to take one that stopped up again.
    openexplorer_b2::set_notes_file(Some(b2::notes_file()));
    // Each OneDrive account's listings and delta position, sealed, so a start reads one page.
    openexplorer_onedrive::set_log(openexplorer_gdrive::log);
    openexplorer_onedrive::set_cache_dir(Some(local.join("onedrive")));
    // Each Telegram account's tree, sealed, so a start asks the channel only what changed.
    openexplorer_telegram::cache::set_dir(Some(telegram::cache_dir()));
    openexplorer_telegram::set_store(telegram::store());
}

/// As the app closes: what each open account changed since it was last kept goes to disk.
pub fn save_caches() {
    for account in Account::ALL {
        match account {
            Account::Mega => openexplorer_mega::save_caches(),
            Account::Dropbox => openexplorer_dropbox::save_caches(),
            Account::OneDrive => openexplorer_onedrive::save_caches(),
            Account::B2 => openexplorer_b2::save_caches(),
            Account::Telegram => openexplorer_telegram::save_caches(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Places

/// Why `loc` doesn't take new items (its top, an area listing others' folders, a bin), `None`
/// where it does or isn't an account.
pub fn why_not(loc: &Location) -> Option<String> {
    match Account::at(loc)? {
        Account::Mega => mega::why_not(loc),
        Account::B2 => b2::why_not(loc),
        Account::OneDrive => onedrive::why_not(loc),
        Account::Dropbox | Account::Telegram => None,
    }
}

/// Whether the items `loc` lists can't be renamed, cut or deleted (the areas of the top, the
/// buckets, the folders shared with the account).
pub fn items_fixed(loc: &Location) -> bool {
    match Account::at(loc) {
        Some(Account::Mega) => mega::items_fixed(loc),
        Some(Account::B2) => b2::items_fixed(loc),
        Some(Account::OneDrive) => onedrive::items_fixed(loc),
        Some(Account::Dropbox | Account::Telegram) | None => false,
    }
}

/// Whether `loc` is an account's bin itself, where Restaurar, Excluir permanentemente and
/// Esvaziar apply.
pub fn bin_top(loc: &Location) -> bool {
    match Account::at(loc) {
        Some(Account::Mega) => mega::bin_top(loc),
        Some(Account::OneDrive) => onedrive::bin_top(loc),
        Some(Account::Dropbox | Account::B2 | Account::Telegram) | None => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Deleting

/// Whether Excluir in `loc` keeps what it deletes somewhere it comes back from (MEGA's Rubbish
/// Bin, Dropbox's deleted files, B2's hidden versions, OneDrive's recycle bin), rather than
/// deleting for good: not in MEGA's Rubbish Bin itself, nor in a folder shared with the account.
pub fn keeps_deleted(loc: &Location) -> bool {
    match Account::at(loc) {
        Some(Account::Mega) => !mega::deletes_for_good(loc),
        Some(Account::Dropbox | Account::B2 | Account::OneDrive) => true,
        // Telegram has no bin: the servers' deleting, after asking.
        Some(Account::Telegram) | None => false,
    }
}

/// Excluir (asked already) in account location `loc`; the items back where `loc` isn't an
/// account.
pub fn delete(s: &mut State, loc: &Location, items: Vec<Entry>) -> Result<(), Vec<Entry>> {
    match Account::at(loc) {
        Some(Account::Mega) => mega::trash(s, loc, items),
        Some(Account::Dropbox) => dropbox::delete(s, loc, items),
        Some(Account::B2) => b2::hide(s, loc, items),
        Some(Account::OneDrive) => onedrive::trash(s, loc, items),
        Some(Account::Telegram) | None => return Err(items),
    }
    Ok(())
}

/// Excluir permanentemente (asked already) where the account deletes for good its own way; the
/// items back where the servers' deleting does it (Dropbox, OneDrive outside its recycle bin, a
/// server).
pub fn purge(s: &mut State, loc: &Location, items: Vec<Entry>) -> Result<(), Vec<Entry>> {
    match Account::at(loc) {
        Some(Account::Mega) => mega::purge(s, loc, items),
        Some(Account::B2) => b2::purge(s, loc, items),
        Some(Account::OneDrive) if onedrive::in_bin(loc) => onedrive::purge(s, loc, items),
        Some(Account::OneDrive | Account::Dropbox | Account::Telegram) | None => return Err(items),
    }
    Ok(())
}

/// Restaurar in pane `p`, where it shows an account's bin; `false` elsewhere.
pub fn restore(s: &mut State, p: usize) -> bool {
    let loc = &s.panes[p].loc;
    if !bin_top(loc) {
        return false;
    }
    match Account::at(loc) {
        Some(Account::Mega) => mega::restore(s, p),
        Some(Account::OneDrive) => onedrive::restore(s, p),
        Some(Account::Dropbox | Account::B2 | Account::Telegram) | None => return false,
    }
    true
}

/// The account whose bin `loc` is, for Esvaziar lixeira.
pub fn bin_of(loc: &Location) -> Option<String> {
    match loc {
        Location::Server { server, .. } if bin_top(loc) => Some(server.id.clone()),
        _ => None,
    }
}

/// Esvaziar lixeira (asked already) of account `id`'s bin; `false` if `id` isn't an account
/// with one.
pub fn empty_bin(s: &mut State, id: &str) -> bool {
    match Account::of_id(id) {
        Some(Account::Mega) => mega::empty_bin(s, id),
        Some(Account::OneDrive) => onedrive::empty_bin(s, id),
        Some(Account::Dropbox | Account::B2 | Account::Telegram) | None => return false,
    }
    true
}

/// What the Excluir question says of where the items go, in an account.
pub fn delete_note(s: &State, loc: &Location) -> Option<Cow<'static, str>> {
    match Account::at(loc)? {
        Account::Mega => Some(mega::delete_note().into()),
        Account::Dropbox => Some(dropbox::delete_note().into()),
        Account::B2 => Some(b2::delete_note(loc).into()),
        Account::OneDrive => onedrive::delete_note(s, loc).map(Cow::from),
        Account::Telegram => Some(telegram::delete_note().into()),
    }
}

/// What the Excluir permanentemente question says, in an account that deletes for good its own
/// way; `None` where the servers' words apply.
pub fn purge_note(loc: &Location) -> Option<&'static str> {
    match Account::at(loc)? {
        Account::Mega => Some(mega::purge_note(loc)),
        Account::B2 => Some(b2::purge_note()),
        Account::OneDrive => onedrive::purge_note(loc),
        Account::Dropbox => None,
        Account::Telegram => Some(telegram::delete_note()),
    }
}

// ---------------------------------------------------------------------------------------------
// Links

/// Copiar link and Remover link for one item in `loc`, where the account makes links (not in a
/// bin).
pub fn links(loc: &Location) -> Option<(MenuAction, MenuAction)> {
    match Account::at(loc)? {
        Account::Mega if !mega::in_bin(loc) => Some((mega::copy_link, mega::remove_link)),
        Account::Dropbox => Some((dropbox::copy_link, dropbox::remove_link)),
        Account::OneDrive if !onedrive::in_bin(loc) => Some((onedrive::copy_link, onedrive::remove_link)),
        Account::Mega | Account::OneDrive | Account::B2 | Account::Telegram => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Removing

/// Forgets account `server`, of kind `account`: it leaves the account's own store, its secrets
/// the credential store, its kept tree or listings the disk. `false` if it couldn't (the failure
/// shown already).
pub fn remove(s: &mut State, account: Account, server: &Server) -> bool {
    match account {
        Account::Mega => mega::remove(s, server),
        Account::Dropbox => dropbox::remove(s, server),
        Account::B2 => b2::remove(s, server),
        Account::OneDrive => onedrive::remove(s, server),
        Account::Telegram => telegram::remove(s, server),
    }
}

/// The status bar's words once account `name`, of kind `account`, is removed.
pub fn removed(account: Account, name: &str) -> String {
    match account {
        Account::Mega => tr!("Conta MEGA removida: {name}", "MEGA account removed: {name}"),
        Account::Dropbox => tr!("Conta Dropbox removida: {name}", "Dropbox account removed: {name}"),
        Account::B2 => tr!("Conta B2 removida: {name}", "B2 account removed: {name}"),
        Account::OneDrive => tr!("Conta OneDrive removida: {name}", "OneDrive account removed: {name}"),
        Account::Telegram => tr!("Conta do Telegram removida: {name}. O canal e os arquivos continuam no Telegram.", "Telegram account removed: {name}. The channel and its files stay on Telegram."),
    }
}

// ---------------------------------------------------------------------------------------------
// Storage

/// Asks the storage account `server` uses (none for a B2 key, nor a server).
pub fn ask_quota(s: &mut State, server: &Server) {
    match Account::of(server) {
        Some(Account::Mega) => mega::ask_quota(s, server),
        Some(Account::Dropbox) => dropbox::ask_quota(s, server),
        Some(Account::OneDrive) => onedrive::ask_quota(s, server),
        Some(Account::B2 | Account::Telegram) | None => {}
    }
}

/// Whether account `server`'s storage was never asked: the first of its folders to open asks it.
pub fn quota_unasked(s: &State, server: &Server) -> bool {
    match Account::of(server) {
        Some(Account::Mega) => !s.servers.mega_quotas.contains_key(&server.id),
        Some(Account::Dropbox) => !s.servers.dropbox_quotas.contains_key(&server.id),
        Some(Account::OneDrive) => !s.servers.onedrive.quotas.contains_key(&server.id),
        Some(Account::B2 | Account::Telegram) | None => false,
    }
}

/// What the status bar shows beside an account's location: the words, how full, the details.
pub fn quota_bar(s: &State, loc: &Location) -> Option<(String, Option<f64>, String)> {
    match Account::at(loc)? {
        Account::Mega => mega::quota_of(s, loc).map(|q| (mega::quota_label(&q), (q.total > 0).then(|| q.used as f64 / q.total as f64), mega::quota_help(q))),
        Account::Dropbox => dropbox::quota_status(s, loc),
        Account::OneDrive => onedrive::quota_bar(s, loc),
        Account::B2 => b2::quota_bar(loc),
        Account::Telegram => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Listing and watching

/// The folders inside folder `path` of `server` as its account keeps them, with no request:
/// what the tree shows at once while it lists the folder behind it. `None` where nothing is kept
/// (or the account lists from memory anyway, as MEGA does).
pub fn kept_folders(server: &Server, path: &[String], show_hidden: bool) -> Option<Vec<String>> {
    let dir = server.path(path);
    let shown = |name: &str| show_hidden || !name.starts_with('.');
    match Account::of(server)? {
        Account::Mega => None,
        Account::B2 => {
            let kept = openexplorer_b2::cached(&server.user)?.cached(&dir)?;
            Some(kept.items.iter().filter(|i| i.is_dir && shown(&i.name)).map(|i| i.name.clone()).collect())
        }
        Account::OneDrive => Some(openexplorer_onedrive::cached(&server.user)?.kept_list(&dir)?.into_iter().filter(|i| i.is_dir && shown(&i.name)).map(|i| i.name).collect()),
        Account::Dropbox => Some(openexplorer_dropbox::cached(&server.user)?.kept(&dir)?.into_iter().filter(|e| e.dir && shown(&e.name)).map(|e| e.name).collect()),
        Account::Telegram => telegram::kept_folders(server, &dir, shown),
    }
}

/// Lists pane `p`'s folder where the account lists its own way (B2, from its kept listings);
/// `false` where the servers' listing does it.
pub fn load(s: &mut State, p: usize, request: u64, fresh: bool) -> bool {
    match Account::at(&s.panes[p].loc) {
        Some(Account::B2) => b2::load(s, p, request, fresh),
        Some(Account::OneDrive) => return onedrive::load(s, p, request),
        Some(Account::Mega | Account::Dropbox | Account::Telegram) | None => return false,
    }
    true
}

/// Each frame: the accounts on screen ask what changed elsewhere, when it's time.
pub fn tick(s: &mut State, now: f64) {
    for account in Account::ALL {
        match account {
            Account::Mega => mega::tick(s, now),
            Account::Dropbox => dropbox::tick(s, now),
            Account::OneDrive => onedrive::tick(s, now),
            Account::B2 => b2::tick(s, now),
            Account::Telegram => telegram::tick(s, now),
        }
    }
}

/// When the next of those asks is due, if one is.
pub fn next_ask(s: &State) -> Option<f64> {
    Account::ALL
        .into_iter()
        .filter_map(|account| match account {
            Account::Mega => mega::next_ask(s),
            Account::Dropbox => dropbox::next_ask(s),
            Account::OneDrive => onedrive::next_ask(s),
            Account::B2 => b2::next_ask(s),
            Account::Telegram => telegram::next_ask(s),
        })
        .reduce(f64::min)
}

/// The window gained or lost the focus: the accounts that look for changes only while it's in
/// front (B2, Telegram) stop, or catch up at once.
pub fn window_focus(s: &mut State, focused: bool, tx: &mut Tx) {
    b2::window_focus(s, focused, tx);
    telegram::window_focus(s, focused);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_account_is_told_by_its_server_and_its_id() {
        let servers = [Server::mega("a@b"), Server::dropbox("a@b"), Server::b2("000k", "chave"), Server::onedrive("d1", "a@b")];
        for (server, account) in servers.iter().zip([Account::Mega, Account::Dropbox, Account::B2, Account::OneDrive]) {
            assert_eq!(Account::of(server), Some(account));
            assert_eq!(Account::of_id(&server.id), Some(account), "{}", server.id);
            assert_eq!(Account::at(&crate::servers::folder(server, vec!["x".into()])), Some(account));
        }
        let sftp = Server { id: "s1".into(), protocol: Protocol::Sftp, ..Server::mega("x") };
        assert_eq!(Account::of(&sftp), None);
        assert_eq!(Account::of_id("a@gmail.com"), None);
        assert_eq!(Account::at(&Location::home()), None);
    }
}
