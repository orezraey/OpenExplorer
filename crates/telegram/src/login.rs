//! Connecting an account: the app registered by the person at my.telegram.org (api_id and
//! api_hash), the phone, the code Telegram sends to their other devices and, with two-step
//! verification on, the password. Then the storage channel: found again by its mark when an
//! earlier Open Explorer (on this PC or another) made one, else created.
//!
//! Each step blocks on a request to Telegram; the app runs them on background threads.

use std::sync::Arc;

use std::time::Duration;

use grammers_client::{InvocationError, SignInError};
use grammers_client::client::{LoginToken, PasswordToken};
use grammers_client::message::InputMessage;
use grammers_session::types::PeerRef;
use grammers_tl_types as tl;
use openexplorer_i18n::{t, tr};

use crate::Account;
use crate::real::{Conn, said};
use crate::records::{CHANNEL_MARK, CHANNEL_POST_TAG, TAG};
use crate::session::{self, KeyringSession};
use crate::store::StoredAccount;
use crate::{Result, open};

/// What the storage channel is called when created; the person may rename it.
pub const CHANNEL_TITLE: &str = "Open Explorer";

/// A login under way.
pub struct Login {
    conn: Option<Conn>,
    api_id: i32,
    api_hash: String,
    token: Option<LoginToken>,
    password: Option<PasswordToken>,
}

/// Where a login stands after a step.
pub enum Step {
    /// The account has two-step verification: its password is next. `hint` is the hint the
    /// person set, maybe empty.
    Password { hint: String },
    /// Connected.
    Done(Arc<Account>, StoredAccount),
}

fn login_error(e: grammers_client::InvocationError) -> String {
    use grammers_client::InvocationError::Rpc;
    match &e {
        Rpc(r) if r.name == "API_ID_INVALID" || r.name == "API_ID_PUBLISHED_FLOOD" => {
            t("O api_id ou o api_hash não confere. Copie os dois de my.telegram.org › API development tools.", "The api_id or api_hash doesn't match. Copy both from my.telegram.org › API development tools.").into()
        }
        Rpc(r) if r.name == "PHONE_NUMBER_INVALID" => t("O Telegram não reconhece esse número. Escreva com o código do país, como +55 11 99999-8888.", "Telegram doesn't know that number. Write it with the country code, like +1 555 123 4567.").into(),
        Rpc(r) if r.name == "PHONE_NUMBER_BANNED" => t("Esse número está bloqueado no Telegram.", "That number is banned on Telegram.").into(),
        Rpc(r) if r.name == "PHONE_CODE_EXPIRED" => t("O código expirou. Comece de novo para receber outro.", "The code expired. Start again to get another.").into(),
        Rpc(r) if r.code == 420 => tr!("O Telegram pediu para esperar {} s antes de tentar de novo.", "Telegram asked to wait {} s before trying again.", r.value.unwrap_or(0)),
        _ => said(e),
    }
}

fn sign_in_error(e: SignInError) -> String {
    match e {
        SignInError::SignUpRequired => t("Esse número não tem conta no Telegram. Crie a conta no app do Telegram primeiro.", "That number has no Telegram account. Create it in the Telegram app first.").into(),
        SignInError::InvalidCode => t("O código não confere.", "The code doesn't match.").into(),
        SignInError::InvalidPassword(_) => t("A senha não confere.", "The password doesn't match.").into(),
        SignInError::PasswordRequired(_) => t("A conta pede a senha da verificação em duas etapas.", "The account asks for its two-step verification password.").into(),
        SignInError::Other(e) => login_error(e),
    }
}

impl Login {
    /// Starts a login for `phone`: Telegram sends a code to the account's other devices (or by
    /// SMS).
    pub fn begin(api_id: i32, api_hash: &str, phone: &str) -> Result<Login> {
        if api_id <= 0 || api_hash.trim().len() != 32 || !api_hash.trim().bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(t("O api_id é um número e o api_hash tem 32 letras e números (de my.telegram.org › API development tools).", "The api_id is a number and the api_hash is 32 letters and digits (from my.telegram.org › API development tools).").into());
        }
        let phone: String = phone.chars().filter(|c| c.is_ascii_digit() || *c == '+').collect();
        let conn = Conn::start(api_id, Arc::new(KeyringSession::blank()))?;
        let token = conn.rt.block_on(conn.client.request_login_code(&phone, api_hash.trim())).map_err(login_error)?;
        Ok(Login { conn: Some(conn), api_id, api_hash: api_hash.trim().into(), token: Some(token), password: None })
    }

    fn conn(&self) -> Result<&Conn> {
        self.conn.as_ref().ok_or_else(|| t("Esse login já terminou.", "That login has already ended.").into())
    }

    /// The code Telegram sent.
    pub fn code(&mut self, code: &str) -> Result<Step> {
        let code: String = code.chars().filter(char::is_ascii_digit).collect();
        let token = self.token.as_ref().ok_or_else(|| t("Esse login não espera um código agora.", "That login isn't waiting for a code now.").to_string())?;
        let conn = self.conn()?;
        match conn.rt.block_on(conn.client.sign_in(token, &code)) {
            Ok(_) => self.finish(),
            Err(SignInError::PasswordRequired(p)) => {
                let hint = p.hint().unwrap_or("").to_string();
                self.password = Some(p);
                Ok(Step::Password { hint })
            }
            Err(e) => Err(sign_in_error(e)),
        }
    }

    /// The two-step verification password.
    pub fn password(&mut self, password: &str) -> Result<Step> {
        let p = self.password.take().ok_or_else(|| t("Esse login não espera uma senha agora.", "That login isn't waiting for a password now.").to_string())?;
        let conn = self.conn()?;
        match conn.rt.block_on(conn.client.check_password(p, password.as_bytes())) {
            Ok(_) => self.finish(),
            Err(SignInError::InvalidPassword(p)) => {
                self.password = Some(p);
                Err(t("A senha não confere.", "The password doesn't match.").into())
            }
            Err(e) => Err(sign_in_error(e)),
        }
    }

    /// Signed in: the session goes to the credential store, the channel is found or made, and the
    /// account opens.
    fn finish(&mut self) -> Result<Step> {
        let conn = self.conn.take().ok_or_else(|| t("Esse login já terminou.", "That login has already ended.").to_string())?;
        let mut saved = None;
        match self.settle(&conn, &mut saved) {
            Ok(stored) => {
                let account = open::adopt_conn(conn, &stored)?;
                Ok(Step::Done(account, stored))
            }
            // Signed in but not set up: the session ends rather than stay among the account's
            // devices with nothing in the app to remove it by.
            Err(e) => {
                conn.session.detach();
                let _ = conn.rt.block_on(conn.client.sign_out());
                if let Some(id) = saved {
                    session::forget(id);
                }
                Err(e)
            }
        }
    }

    /// Who signed in and where their channel is; only then is the session kept, so a login that
    /// fails before (a channel not found in time) leaves the credential store as it was, an
    /// account connected before under the same user included. `saved`: the user id whose entries
    /// this login wrote, for a failure after to take them back.
    fn settle(&self, conn: &Conn, saved: &mut Option<i64>) -> Result<StoredAccount> {
        let me = conn.rt.block_on(conn.client.get_me()).map_err(said)?;
        let user_id = me.id().bare_id_unchecked();
        let premium = matches!(&me.raw, tl::enums::User::User(u) if u.premium);
        let channel = match conn.rt.block_on(find_channel(conn))? {
            Some(c) => c,
            None => conn.rt.block_on(create_channel(conn))?,
        };
        let stored = StoredAccount { user_id, name: me.full_name(), channel_id: channel.id.bare_id_unchecked(), channel_hash: channel.auth.hash(), premium };
        *saved = Some(user_id);
        conn.session.keep_as(user_id)?;
        session::set_api(user_id, self.api_id, &self.api_hash)?;
        Ok(stored)
    }
}

/// The channel an Open Explorer made for this account: a broadcast channel the person created
/// whose description holds the mark (the oldest, if several).
///
/// Found by Telegram's search over all the account's messages, a request or two, not by listing
/// its conversations: that listing goes 100 a request and Telegram paces it hard, so an account
/// with a few thousand conversations waited minutes. A channel shows up in the search by the
/// post made when it was created ([`CHANNEL_POST_TAG`]) or by its records ([`TAG`]); one that has
/// neither (emptied, and its first post deleted) is still found among the 100 most recent
/// conversations. The search finds hashtags only: `#oe1-canal`, in the description, is not one.
pub async fn find_channel(conn: &Conn) -> Result<Option<PeerRef>> {
    let mut waits = 0;
    let mut pause = async |e: InvocationError| -> Result<()> {
        match e {
            InvocationError::Rpc(r) if r.code == 420 && r.value.unwrap_or(0) <= 120 && waits < 20 => {
                waits += 1;
                tokio::time::sleep(Duration::from_secs(r.value.unwrap_or(5) as u64 + 1)).await;
                Ok(())
            }
            e => Err(login_error(e)),
        }
    };
    let mine = |c: &tl::types::Channel| c.creator && c.broadcast;

    let mut found: Vec<tl::types::Channel> = Vec::new();
    let keep = |chats: Vec<tl::enums::Chat>, found: &mut Vec<tl::types::Channel>| {
        for c in chats {
            if let tl::enums::Chat::Channel(c) = c
                && mine(&c)
                && !found.iter().any(|f| f.id == c.id)
            {
                found.push(c);
            }
        }
    };
    for tag in [CHANNEL_POST_TAG, TAG] {
        let request = tl::functions::messages::SearchGlobal {
            broadcasts_only: true,
            groups_only: false,
            users_only: false,
            folder_id: None,
            community: None,
            q: tag.into(),
            filter: tl::enums::MessagesFilter::InputMessagesFilterEmpty,
            min_date: 0,
            max_date: 0,
            offset_rate: 0,
            offset_peer: tl::enums::InputPeer::Empty,
            offset_id: 0,
            limit: 100,
        };
        let chats = loop {
            match conn.client.invoke(&request).await {
                Ok(tl::enums::messages::Messages::Messages(m)) => break m.chats,
                Ok(tl::enums::messages::Messages::Slice(m)) => break m.chats,
                Ok(tl::enums::messages::Messages::ChannelMessages(m)) => break m.chats,
                Ok(tl::enums::messages::Messages::NotModified(_)) => break Vec::new(),
                Err(e) => pause(e).await?,
            }
        };
        keep(chats, &mut found);
    }
    if found.is_empty() {
        let request = tl::functions::messages::GetDialogs { exclude_pinned: false, folder_id: None, offset_date: 0, offset_id: 0, offset_peer: tl::enums::InputPeer::Empty, limit: 100, hash: 0 };
        let chats = loop {
            match conn.client.invoke(&request).await {
                Ok(tl::enums::messages::Dialogs::Dialogs(d)) => break d.chats,
                Ok(tl::enums::messages::Dialogs::Slice(d)) => break d.chats,
                Ok(tl::enums::messages::Dialogs::NotModified(_)) => break Vec::new(),
                Err(e) => pause(e).await?,
            }
        };
        keep(chats, &mut found);
    }
    found.sort_by_key(|c| c.id);
    for c in found {
        if let Some(p) = marked(conn, c, &mut pause).await? {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

/// `c`, if its description holds the mark.
async fn marked(conn: &Conn, c: tl::types::Channel, pause: &mut impl AsyncFnMut(InvocationError) -> Result<()>) -> Result<Option<PeerRef>> {
    let peer = PeerRef::from(c);
    let request = tl::functions::channels::GetFullChannel { channel: peer.into() };
    let tl::enums::messages::ChatFull::Full(full) = loop {
        match conn.client.invoke(&request).await {
            Ok(f) => break f,
            Err(e) => pause(e).await?,
        }
    };
    Ok(match full.full_chat {
        tl::enums::ChatFull::ChannelFull(f) if f.about.contains(CHANNEL_MARK) => Some(peer),
        _ => None,
    })
}

async fn create_channel(conn: &Conn) -> Result<PeerRef> {
    let about = format!("Armazenamento do Open Explorer: cada mensagem é uma pasta ou um arquivo. Apagar ou editar mensagens aqui muda os arquivos. / Open Explorer's storage. {CHANNEL_MARK}");
    let updates = conn
        .client
        .invoke(&tl::functions::channels::CreateChannel { broadcast: true, megagroup: false, for_import: false, forum: false, title: CHANNEL_TITLE.into(), about, geo_point: None, address: None, ttl_period: None })
        .await
        .map_err(said)?;
    let chats = match updates {
        tl::enums::Updates::Updates(u) => u.chats,
        tl::enums::Updates::Combined(u) => u.chats,
        _ => Vec::new(),
    };
    let channel = chats
        .into_iter()
        .find_map(|c| match c {
            tl::enums::Chat::Channel(c) => Some(PeerRef::from(c)),
            _ => None,
        })
        .ok_or_else(|| tr!("O Telegram não devolveu o canal criado.", "Telegram didn't return the channel it created."))?;
    // Its first post, for a later login's search to find it by (see `find_channel`).
    let post = format!("Open Explorer: este canal guarda os seus arquivos. Não apague esta mensagem. / Open Explorer keeps your files here. Don't delete this message.
{CHANNEL_POST_TAG}");
    conn.client.send_message(channel, InputMessage::new().text(post)).await.map_err(said)?;
    Ok(channel)
}
