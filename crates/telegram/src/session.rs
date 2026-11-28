//! An account's grammers session with no file on disk: the authorization keys (full access to the
//! account) go to the OS credential store, like the other clouds' tokens. What else a session
//! holds (peers met, update state) stays in memory and comes back by itself next run.
//!
//! grammers' own `SqliteSession` writes the keys to a file in the clear, which is why this exists.
//!
//! # In the credential store
//!
//! Service `OpenExplorer Telegram`, one entry per account named by its user id:
//!
//! ```text
//! format (1) | home DC (i32 LE) | n (1) | n × (DC id i32 | IPv4 4+2 | IPv6 16+2 | auth key 256)
//! ```
//!
//! Only the DCs with a key: about 290 bytes each, 1426 with every DC (the store takes 2560). The
//! app's api_id and api_hash, which each person registers at my.telegram.org, are kept beside it
//! in `<user id>#api` as `id:hash`.
//!
//! A login has no user id until it ends, so its session saves nothing until [`KeyringSession::keep_as`].

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};
use std::sync::Mutex;

use grammers_session::types::{ChannelState, DcOption, PeerId, PeerInfo, UpdateState, UpdatesState};
use grammers_session::{BoxFuture, Session, SessionData};
use openexplorer_i18n::{t, tr};

use crate::Result;

const SERVICE: &str = "OpenExplorer Telegram";
const FORMAT: u8 = 1;

#[derive(Debug)]
pub struct Error(String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn entry(name: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, name).map_err(|e| tr!("Cofre de credenciais indisponível: {e}", "Credential store unavailable: {e}"))
}

/// The api_id and api_hash kept for `user_id`.
pub fn api(user_id: i64) -> Result<(i32, String)> {
    let gone = || t("O acesso desta conta do Telegram não está no cofre do sistema. Remova a conta e conecte de novo.", "This Telegram account's access isn't in the system's credential store. Remove the account and connect it again.").to_string();
    let s = entry(&format!("{user_id}#api"))?.get_password().map_err(|_| gone())?;
    let (id, hash) = s.split_once(':').ok_or_else(gone)?;
    Ok((id.parse().map_err(|_| gone())?, hash.to_string()))
}

pub fn set_api(user_id: i64, api_id: i32, api_hash: &str) -> Result<()> {
    entry(&format!("{user_id}#api"))?
        .set_password(&format!("{api_id}:{api_hash}"))
        .map_err(|e| tr!("Não consegui guardar o acesso no cofre do sistema: {e}", "Couldn't keep the access in the system's credential store: {e}"))
}

/// Forgets `user_id`'s session and api keys.
pub fn forget(user_id: i64) {
    for name in [user_id.to_string(), format!("{user_id}#api")] {
        if let Ok(e) = entry(&name) {
            let _ = e.delete_credential();
        }
    }
}

pub struct KeyringSession {
    data: Mutex<SessionData>,
    /// The entry it saves to; none during a login.
    name: Mutex<Option<String>>,
}

impl KeyringSession {
    /// A session for a login: nothing saved until [`KeyringSession::keep_as`].
    pub fn blank() -> KeyringSession {
        KeyringSession { data: Mutex::new(SessionData::default()), name: Mutex::new(None) }
    }

    /// `user_id`'s session from the credential store.
    pub fn load(user_id: i64) -> Result<KeyringSession> {
        let name = user_id.to_string();
        let bytes = entry(&name)?.get_secret().map_err(|_| {
            t("A sessão desta conta do Telegram não está no cofre do sistema. Remova a conta e conecte de novo.", "This Telegram account's session isn't in the system's credential store. Remove the account and connect it again.").to_string()
        })?;
        let (home, dcs) = decode(&bytes).ok_or_else(|| t("A sessão guardada desta conta do Telegram não se lê. Remova a conta e conecte de novo.", "This Telegram account's kept session doesn't read. Remove the account and connect it again.").to_string())?;
        let mut data = SessionData { home_dc: home, ..SessionData::default() };
        for dc in dcs {
            data.dc_options.insert(dc.id, dc);
        }
        Ok(KeyringSession { data: Mutex::new(data), name: Mutex::new(Some(name)) })
    }

    /// A session from bytes in the credential store's format, saving nothing until
    /// [`KeyringSession::keep_as`]: the live tests, over the proof of concept's session.
    pub fn from_bytes(bytes: &[u8]) -> Option<KeyringSession> {
        let (home, dcs) = decode(bytes)?;
        let mut data = SessionData { home_dc: home, ..SessionData::default() };
        for dc in dcs {
            data.dc_options.insert(dc.id, dc);
        }
        Some(KeyringSession { data: Mutex::new(data), name: Mutex::new(None) })
    }

    /// Saves this session as `user_id`'s from now on.
    pub fn keep_as(&self, user_id: i64) -> Result<()> {
        *self.name.lock().unwrap_or_else(|e| e.into_inner()) = Some(user_id.to_string());
        let data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        self.save(&data).map_err(|e| e.0)
    }

    /// The same keys in a session that saves nothing: what an extra connection to a DC runs on
    /// (see `real`), so it never writes over the account's entry, and a key it might make alone
    /// (a DC this session had none for) stays its own.
    pub fn unsaved_copy(&self) -> KeyringSession {
        let data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let copy = SessionData { home_dc: data.home_dc, dc_options: data.dc_options.clone(), ..SessionData::default() };
        KeyringSession { data: Mutex::new(copy), name: Mutex::new(None) }
    }

    /// Connects to DC `dc` at `address` from now on: the DC's media address, for a copy that
    /// carries downloads (see `real`).
    pub fn set_address(&self, dc: i32, address: SocketAddrV4) {
        if let Some(o) = self.data.lock().unwrap_or_else(|e| e.into_inner()).dc_options.get_mut(&dc) {
            o.ipv4 = address;
        }
    }

    /// This session's own DC.
    pub fn home(&self) -> i32 {
        self.data.lock().unwrap_or_else(|e| e.into_inner()).home_dc
    }

    /// Whether this session holds a key for DC `dc`.
    pub fn has_key(&self, dc: i32) -> bool {
        self.data.lock().unwrap_or_else(|e| e.into_inner()).dc_options.get(&dc).is_some_and(|o| o.auth_key.is_some())
    }

    /// Saves nothing from now on: a session being ended after its account was removed.
    pub fn detach(&self) {
        *self.name.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// The home DC's authorization key: what the tree's file on disk is sealed with.
    pub fn auth_key(&self) -> Option<[u8; 256]> {
        let data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        data.dc_options.get(&data.home_dc)?.auth_key
    }

    fn save(&self, data: &SessionData) -> std::result::Result<(), Error> {
        let Some(name) = self.name.lock().unwrap_or_else(|e| e.into_inner()).clone() else { return Ok(()) };
        let bytes = encode(data);
        entry(&name).map_err(Error)?.set_secret(&bytes).map_err(|e| Error(tr!("Não consegui guardar a sessão do Telegram: {e}", "Couldn't keep the Telegram session: {e}")))
    }
}

fn encode(data: &SessionData) -> Vec<u8> {
    let mut dcs: Vec<&DcOption> = data.dc_options.values().filter(|dc| dc.auth_key.is_some()).collect();
    dcs.sort_by_key(|dc| dc.id);
    let mut out = vec![FORMAT];
    out.extend(data.home_dc.to_le_bytes());
    out.push(dcs.len() as u8);
    for dc in dcs {
        out.extend(dc.id.to_le_bytes());
        out.extend(dc.ipv4.ip().octets());
        out.extend(dc.ipv4.port().to_le_bytes());
        out.extend(dc.ipv6.ip().octets());
        out.extend(dc.ipv6.port().to_le_bytes());
        out.extend(dc.auth_key.expect("filtered above"));
    }
    out
}

fn decode(b: &[u8]) -> Option<(i32, Vec<DcOption>)> {
    let mut r = b;
    let mut take = |n: usize| -> Option<&[u8]> {
        let (head, tail) = r.split_at_checked(n)?;
        r = tail;
        Some(head)
    };
    if take(1)?[0] != FORMAT {
        return None;
    }
    let home = i32::from_le_bytes(take(4)?.try_into().ok()?);
    let n = take(1)?[0];
    let mut dcs = Vec::new();
    for _ in 0..n {
        let id = i32::from_le_bytes(take(4)?.try_into().ok()?);
        let v4: [u8; 4] = take(4)?.try_into().ok()?;
        let p4 = u16::from_le_bytes(take(2)?.try_into().ok()?);
        let v6: [u8; 16] = take(16)?.try_into().ok()?;
        let p6 = u16::from_le_bytes(take(2)?.try_into().ok()?);
        let key: [u8; 256] = take(256)?.try_into().ok()?;
        dcs.push(DcOption { id, ipv4: SocketAddrV4::new(Ipv4Addr::from(v4), p4), ipv6: SocketAddrV6::new(Ipv6Addr::from(v6), p6, 0, 0), auth_key: Some(key) });
    }
    Some((home, dcs))
}

impl Session for KeyringSession {
    type Error = Error;

    fn home_dc_id(&self) -> std::result::Result<i32, Error> {
        Ok(self.data.lock().unwrap_or_else(|e| e.into_inner()).home_dc)
    }

    fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, std::result::Result<(), Error>> {
        Box::pin(async move {
            let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
            data.home_dc = dc_id;
            self.save(&data)
        })
    }

    fn dc_option(&self, dc_id: i32) -> std::result::Result<Option<DcOption>, Error> {
        Ok(self.data.lock().unwrap_or_else(|e| e.into_inner()).dc_options.get(&dc_id).cloned())
    }

    fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, std::result::Result<(), Error>> {
        let dc_option = dc_option.clone();
        Box::pin(async move {
            let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
            data.dc_options.insert(dc_option.id, dc_option);
            self.save(&data)
        })
    }

    fn peer(&self, peer: PeerId) -> BoxFuture<'_, std::result::Result<Option<PeerInfo>, Error>> {
        Box::pin(async move { Ok(self.data.lock().unwrap_or_else(|e| e.into_inner()).peer_infos.get(&peer).cloned()) })
    }

    fn cache_peer(&self, peer: PeerInfo) -> BoxFuture<'_, std::result::Result<(), Error>> {
        Box::pin(async move {
            self.data.lock().unwrap_or_else(|e| e.into_inner()).peer_infos.entry(peer.id()).or_insert_with(|| peer.clone()).extend_info(&peer);
            Ok(())
        })
    }

    fn updates_state(&self) -> BoxFuture<'_, std::result::Result<UpdatesState, Error>> {
        Box::pin(async move { Ok(self.data.lock().unwrap_or_else(|e| e.into_inner()).updates_state.clone()) })
    }

    fn set_update_state(&self, update: UpdateState) -> BoxFuture<'_, std::result::Result<(), Error>> {
        Box::pin(async move {
            let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
            let s = &mut data.updates_state;
            match update {
                UpdateState::All(all) => *s = all,
                UpdateState::Primary { pts, date, seq } => {
                    s.pts = pts;
                    s.date = date;
                    s.seq = seq;
                }
                UpdateState::Secondary { qts } => s.qts = qts,
                UpdateState::Channel { id, pts } => {
                    s.channels.retain(|c| c.id != id);
                    s.channels.push(ChannelState { id, pts });
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_comes_back_the_same_and_fits_the_store() {
        let mut data = SessionData::default();
        let first = *data.dc_options.keys().min().unwrap();
        data.home_dc = first;
        data.dc_options.get_mut(&first).unwrap().auth_key = Some([7; 256]);
        let bytes = encode(&data);
        let (home, dcs) = decode(&bytes).unwrap();
        assert_eq!(home, first);
        assert_eq!(dcs, vec![data.dc_options[&first].clone()]);
        assert!(decode(&bytes[..bytes.len() - 1]).is_none());
        for dc in data.dc_options.values_mut() {
            dc.auth_key = Some([9; 256]);
        }
        assert!(encode(&data).len() <= 2560);
    }
}
