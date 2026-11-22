//! The accounts open now, one per user id for the whole process: every pane and transfer on an
//! account shares its connection, its tree and its budget of requests.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex};

use grammers_session::types::{PeerAuth, PeerId, PeerRef};

use crate::account::Kept;
use crate::real::{Conn, Real};
use crate::session::{self, KeyringSession};
use crate::store::{Store, StoredAccount};
use crate::{Account, Result, cache};

static OPEN: LazyLock<Mutex<HashMap<i64, Arc<Account>>>> = LazyLock::new(Mutex::default);
/// The accounts' list the app keeps (`telegram-accounts.json`), for [`reach_id`].
static STORE: Mutex<Option<Store>> = Mutex::new(None);

/// Where the app keeps its accounts' list.
pub fn set_store(store: Store) {
    *STORE.lock().unwrap_or_else(|e| e.into_inner()) = Some(store);
}

/// The account `user_id`, open already or opened from the list set by [`set_store`]: what a
/// connection to a server named `telegram:<id>` reaches.
pub fn reach_id(user_id: i64) -> Result<Arc<Account>> {
    if let Some(open) = cached(user_id) {
        return Ok(open);
    }
    let stored = STORE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|s| s.get(user_id));
    let stored = stored.ok_or_else(|| openexplorer_i18n::t("Esta conta do Telegram não está mais na lista. Conecte de novo.", "This Telegram account isn't in the list anymore. Connect it again.").to_string())?;
    reach(&stored)
}
/// One lock per account, so two panes reaching it at once open it once.
static OPENING: LazyLock<Mutex<HashMap<i64, Arc<Mutex<()>>>>> = LazyLock::new(Mutex::default);

fn open_now() -> std::sync::MutexGuard<'static, HashMap<i64, Arc<Account>>> {
    OPEN.lock().unwrap_or_else(|e| e.into_inner())
}

/// The account open for `user_id`, if it is.
pub fn cached(user_id: i64) -> Option<Arc<Account>> {
    open_now().get(&user_id).cloned()
}

fn channel_of(a: &StoredAccount) -> Result<PeerRef> {
    let id = PeerId::channel(a.channel_id).ok_or_else(|| {
        openexplorer_i18n::tr!(
            "A lista de contas do Telegram tem um canal inválido ({}) para {}. Remova a conta e conecte de novo.",
            "The Telegram accounts' list has an invalid channel ({}) for {}. Remove the account and connect it again.",
            a.channel_id,
            a.name
        )
    })?;
    Ok(PeerRef { id, auth: PeerAuth::from_hash(a.channel_hash) })
}

fn kept_for(user_id: i64, session: &KeyringSession) -> Option<Kept> {
    let file = cache::file_of(user_id)?;
    let key = session.auth_key()?;
    Some(Kept { file, secret: cache::secret(user_id, &key) })
}

/// The account `a`: open already, or opened from its session in the credential store.
pub fn reach(a: &StoredAccount) -> Result<Arc<Account>> {
    if let Some(open) = cached(a.user_id) {
        return Ok(open);
    }
    let gate = OPENING.lock().unwrap_or_else(|e| e.into_inner()).entry(a.user_id).or_default().clone();
    let _held = gate.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(open) = cached(a.user_id) {
        return Ok(open);
    }
    channel_of(a)?;
    let session = Arc::new(KeyringSession::load(a.user_id)?);
    let (api_id, _) = session::api(a.user_id)?;
    let conn = Conn::start(api_id, session)?;
    adopt_conn(conn, a)
}

/// Opens account `a` over a connection already signed in (a login that just ended). An account
/// open for the same user before (signed in again) gives way: its session stops saving to the
/// credential store at once, where the new one now is, and ends on Telegram's side on a thread
/// of its own, so it doesn't linger among the account's devices.
pub(crate) fn adopt_conn(conn: Conn, a: &StoredAccount) -> Result<Arc<Account>> {
    let channel = channel_of(a)?;
    let kept = kept_for(a.user_id, &conn.session);
    let account = Arc::new(Account::open(a.user_id, &a.name, Box::new(Real::new(conn, channel, a.premium)), kept));
    if let Some(old) = open_now().insert(a.user_id, account.clone()) {
        old.forgotten.store(true, Ordering::SeqCst);
        old.detach();
        std::thread::spawn(move || old.sign_out());
    }
    Ok(account)
}

/// Makes `account` the one open for `user_id` (the app's tests, on the stand-in).
#[cfg(any(test, feature = "fake"))]
pub fn adopt(user_id: i64, account: Arc<Account>) {
    open_now().insert(user_id, account);
}

/// Keeps the tree of each open account that changed since last kept: the app calls it as it
/// closes.
pub fn save_caches() {
    let open: Vec<Arc<Account>> = open_now().values().cloned().collect();
    for a in open {
        a.save_cache_if_changed();
    }
}

/// Forgets account `user_id`: its session and api keys leave the credential store and its tree
/// the disk at once, and the session is ended on Telegram's side on a thread of its own (it then
/// no longer shows among the account's devices). The channel and its files stay.
pub fn disconnect(user_id: i64) {
    let open = open_now().remove(&user_id);
    if let Some(a) = &open {
        a.forgotten.store(true, Ordering::SeqCst);
        // Before its entries go: a save after would bring them back.
        a.detach();
    }
    if let Some(f) = cache::file_of(user_id) {
        cache::forget_file(&f);
    }
    let leaving = match (&open, session::api(user_id), KeyringSession::load(user_id)) {
        (None, Ok((api_id, _)), Ok(s)) => Some((api_id, s)),
        _ => None,
    };
    session::forget(user_id);
    std::thread::spawn(move || {
        if let Some(a) = open {
            a.sign_out();
        } else if let Some((api_id, s)) = leaving {
            s.detach();
            if let Ok(conn) = Conn::start(api_id, Arc::new(s)) {
                let _ = conn.rt.block_on(conn.client.sign_out());
            }
        }
    });
}
