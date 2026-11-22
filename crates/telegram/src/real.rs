//! The channel on Telegram, through grammers: what [`Backend`] asks, as MTProto requests.
//!
//! # Waiting
//!
//! Telegram paces an account with FLOOD_WAIT ("wait N seconds"). grammers waits out the short
//! ones (a few seconds) by itself; the longer ones come back here and are waited out in slices,
//! so a transfer can be cancelled meanwhile and the app can show the wait
//! ([`Backend::waiting_until`]). A transport-level refusal (`-429`, too many requests at once)
//! is retried after a growing pause.
//!
//! # How much at once
//!
//! Requests that carry file parts (both ways) share one budget per account, [`PARTS_AT_ONCE`]:
//! a dozen gave ~37 MiB/s on a real account, more got `-429`. Messages are sent one at a time
//! per account: Telegram paces them anyway, and several at once only collect several waits.

use std::future::Future;
use std::io::{Read, Write};
use std::num::NonZeroU32;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use grammers_client::media::{Downloadable, InputMedia, Media, Uploaded};
use grammers_client::message::InputMessage;
use grammers_client::{Client, InvocationError};
use grammers_mtsender::{ConnectionParams, RetryContext, RetryPolicy, SenderPool};
use grammers_session::types::PeerRef;
use grammers_tl_types as tl;
use openexplorer_i18n::{t, tr};
use tokio::sync::{Mutex as AsyncMutex, Semaphore, mpsc};

use crate::account::now;
use crate::backend::{Backend, Changes, Progress, Small};
use crate::session::KeyringSession;
use crate::tree::{Doc, Msg};
use crate::{CANCELLED, Result};

/// A file part, both ways: Telegram's largest for a download that grammers handles.
pub const PART: usize = 512 * 1024;
/// Above this a file goes in "big file" parts.
const BIG: u64 = 10 * 1024 * 1024;
/// File-part requests in flight per account.
pub const PARTS_AT_ONCE: usize = 12;
/// Parts of one upload in flight.
const UPLOAD_AT_ONCE: usize = 4;
/// Waits longer than this fail rather than hold a transfer for so long.
const LONGEST_WAIT: u64 = 15 * 60;
/// FLOOD_WAITs this short grammers waits out by itself.
const SHORT_WAIT: u32 = 3;

/// What grammers retries on its own: a dropped connection, and short waits.
struct Policy;

impl RetryPolicy for Policy {
    fn should_retry(&self, ctx: &RetryContext) -> ControlFlow<(), Duration> {
        match &ctx.error {
            InvocationError::Rpc(e) if e.code == 420 && e.value.is_some_and(|s| s <= SHORT_WAIT) && ctx.fail_count <= NonZeroU32::new(3).unwrap() => {
                ControlFlow::Continue(Duration::from_secs(e.value.unwrap_or(1) as u64))
            }
            InvocationError::Io(_) if ctx.fail_count <= NonZeroU32::new(3).unwrap() => ControlFlow::Continue(Duration::from_secs(1)),
            // Telegram's own trouble (RPC_CALL_FAIL, a timeout inside): it says to ask again.
            InvocationError::Rpc(e) if e.code == 500 && ctx.fail_count <= NonZeroU32::new(3).unwrap() => ControlFlow::Continue(Duration::from_secs(2)),
            _ => ControlFlow::Break(()),
        }
    }
}

/// A runtime and a client over a session: what a login and an open account both run on.
pub struct Conn {
    pub rt: tokio::runtime::Runtime,
    pub client: Client,
    pub session: Arc<KeyringSession>,
}

impl Conn {
    pub fn start(api_id: i32, session: Arc<KeyringSession>) -> Result<Conn> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("telegram")
            .enable_all()
            .build()
            .map_err(|e| tr!("Não consegui iniciar a conexão com o Telegram: {e}", "Couldn't start the connection to Telegram: {e}"))?;
        let client = {
            let _in = rt.enter();
            let params = ConnectionParams { device_model: "Open Explorer".into(), app_version: env!("CARGO_PKG_VERSION").into(), retry_policy: Box::new(Policy), ..Default::default() };
            let SenderPool { runner, handle, .. } = SenderPool::with_configuration(session.clone(), api_id, params);
            rt.spawn(runner.run());
            Client::new(handle)
        };
        Ok(Conn { rt, client, session })
    }
}

/// An error of Telegram's in the user's words.
pub fn said(e: InvocationError) -> String {
    match e {
        InvocationError::Rpc(r) if r.name == "AUTH_KEY_UNREGISTERED" || r.name == "SESSION_REVOKED" || r.name == "USER_DEACTIVATED" => {
            t("A sessão do Telegram foi encerrada (por outro aparelho ou pelo próprio Telegram). Remova a conta e conecte de novo.", "The Telegram session was ended (from another device or by Telegram). Remove the account and connect it again.").into()
        }
        InvocationError::Rpc(r) if r.name == "CHANNEL_INVALID" || r.name == "CHANNEL_PRIVATE" => {
            t("O canal de armazenamento não está mais acessível (foi apagado?). Remova a conta e conecte de novo.", "The storage channel can't be reached anymore (was it deleted?). Remove the account and connect it again.").into()
        }
        InvocationError::Rpc(r) => tr!("O Telegram recusou o pedido ({}).", "Telegram refused the request ({}).", r.name),
        InvocationError::Io(e) => tr!("Sem conexão com o Telegram: {e}", "No connection to Telegram: {e}"),
        e => tr!("Falha na conexão com o Telegram: {e}", "The connection to Telegram failed: {e}"),
    }
}

fn too_many(e: &InvocationError) -> bool {
    e.to_string().contains("negative length -429")
}

/// `90` → `1 min 30 s`.
fn span(secs: u64) -> String {
    match (secs / 60, secs % 60) {
        (0, s) => format!("{s} s"),
        (m, 0) => format!("{m} min"),
        (m, s) => format!("{m} min {s} s"),
    }
}

/// What the tasks of one account share.
struct Shared {
    client: Client,
    channel: PeerRef,
    /// Until when (seconds since 1970) each request waiting now waits.
    waits: std::sync::Mutex<Vec<i64>>,
    parts: Semaphore,
    sends: Semaphore,
    /// The DCs this session was authorized in, besides its own.
    authorized: AsyncMutex<Vec<i32>>,
}

/// How many waits Telegram asked for in this process, and their seconds: for logs and tests.
pub static WAITS: AtomicUsize = AtomicUsize::new(0);
pub static WAITED_SECS: AtomicUsize = AtomicUsize::new(0);

/// A flag nothing sets: for requests no one cancels.
static NEVER: AtomicBool = AtomicBool::new(false);

impl Shared {
    /// Runs `call` until it answers, waiting out FLOOD_WAITs and `-429`s; `cancel` stops a wait.
    async fn patient<T, F, Fut>(&self, cancel: &AtomicBool, mut call: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = std::result::Result<T, InvocationError>>,
    {
        let mut busy = 0u32;
        loop {
            match call().await {
                Ok(v) => return Ok(v),
                Err(InvocationError::Rpc(e)) if e.code == 420 => {
                    let secs = e.value.unwrap_or(5) as u64;
                    if secs > LONGEST_WAIT {
                        return Err(tr!("O Telegram pediu para esperar {} antes de continuar.", "Telegram asked to wait {} before going on.", span(secs)));
                    }
                    self.wait(secs, cancel).await?;
                }
                Err(e) if too_many(&e) && busy < 6 => {
                    busy += 1;
                    self.wait(1 << busy.min(4), cancel).await?;
                }
                Err(e) => return Err(said(e)),
            }
        }
    }

    async fn wait(&self, secs: u64, cancel: &AtomicBool) -> Result<()> {
        WAITS.fetch_add(1, Ordering::Relaxed);
        WAITED_SECS.fetch_add(secs as usize, Ordering::Relaxed);
        let until = now() + secs as i64;
        self.waits.lock().unwrap_or_else(|e| e.into_inner()).push(until);
        let end = Instant::now() + Duration::from_secs(secs);
        let result = loop {
            if cancel.load(Ordering::SeqCst) {
                break Err(CANCELLED.to_string());
            }
            if Instant::now() >= end {
                break Ok(());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        };
        let mut waits = self.waits.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = waits.iter().position(|w| *w == until) {
            waits.swap_remove(i);
        }
        result
    }

    /// Lets this session download from DC `dc`, as grammers does for its own downloads.
    async fn authorize_in(&self, dc: i32) -> Result<()> {
        let mut done = self.authorized.lock().await;
        if done.contains(&dc) {
            return Ok(());
        }
        let tl::enums::auth::ExportedAuthorization::Authorization(a) = self.client.invoke(&tl::functions::auth::ExportAuthorization { dc_id: dc }).await.map_err(said)?;
        self.client.invoke_in_dc(dc, &tl::functions::auth::ImportAuthorization { id: a.id, bytes: a.bytes }).await.map_err(said)?;
        done.push(dc);
        Ok(())
    }

    /// One part of a file, from wherever Telegram keeps it.
    async fn part(&self, location: &tl::enums::InputFileLocation, dc: &mut i32, offset: i64, cancel: &AtomicBool) -> Result<Vec<u8>> {
        loop {
            let _slot = self.parts.acquire().await.map_err(|e| e.to_string())?;
            let request = tl::functions::upload::GetFile { precise: true, cdn_supported: false, location: location.clone(), offset, limit: PART as i32 };
            let here = *dc;
            let got = self
                .patient(cancel, || async {
                    match self.client.invoke_in_dc(here, &request).await {
                        // Moves and missing authorizations come back to be handled below.
                        Err(InvocationError::Rpc(e)) if e.code == 303 || e.name == "AUTH_KEY_UNREGISTERED" => Ok(Err(e)),
                        other => other.map(Ok),
                    }
                })
                .await?;
            match got {
                Ok(tl::enums::upload::File::File(f)) => return Ok(f.bytes),
                Ok(tl::enums::upload::File::CdnRedirect(_)) => return Err(t("O Telegram mandou baixar de uma CDN, o que o Open Explorer ainda não faz.", "Telegram sent the download to a CDN, which Open Explorer doesn't do yet.").into()),
                Err(e) if e.code == 303 => *dc = e.value.ok_or("FILE_MIGRATE")? as i32,
                Err(_) => self.authorize_in(here).await?,
            }
        }
    }
}

pub struct Real {
    conn: Conn,
    shared: Arc<Shared>,
    premium: bool,
}

/// A message's document, or photo: a photo is read as its largest size, which is what a download
/// of it brings (grammers' `Photo` picks the same).
fn doc_of(media: &Option<tl::enums::MessageMedia>) -> Option<Doc> {
    match media.as_ref()? {
        tl::enums::MessageMedia::Document(d) => {
            let Some(tl::enums::Document::Document(doc)) = &d.document else { return None };
            let name = doc
                .attributes
                .iter()
                .find_map(|a| match a {
                    tl::enums::DocumentAttribute::Filename(f) => Some(f.file_name.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            Some(Doc { size: doc.size as u64, name, photo: false })
        }
        tl::enums::MessageMedia::Photo(p) => {
            let Some(tl::enums::Photo::Photo(photo)) = &p.photo else { return None };
            let size = photo
                .sizes
                .iter()
                .map(|s| match s {
                    tl::enums::PhotoSize::Size(s) => s.size as u64,
                    tl::enums::PhotoSize::Progressive(s) => s.sizes.iter().copied().max().unwrap_or(0) as u64,
                    _ => 0,
                })
                .max()
                .unwrap_or(0);
            Some(Doc { size, name: String::new(), photo: true })
        }
        _ => None,
    }
}

/// A message as the tree needs it; `None` for an empty or service message.
fn msg_of(raw: &tl::enums::Message) -> Option<Msg> {
    match raw {
        tl::enums::Message::Message(m) => Some(Msg { id: m.id, text: m.message.clone(), date: m.date as i64, doc: doc_of(&m.media) }),
        _ => None,
    }
}

/// The messages an answer to a send or an edit carries.
fn sent_in(updates: &tl::enums::Updates) -> Vec<Msg> {
    let list: &[tl::enums::Update] = match updates {
        tl::enums::Updates::Updates(u) => &u.updates,
        tl::enums::Updates::Combined(u) => &u.updates,
        tl::enums::Updates::UpdateShort(u) => std::slice::from_ref(&u.update),
        _ => &[],
    };
    list.iter()
        .filter_map(|u| match u {
            tl::enums::Update::NewChannelMessage(u) => msg_of(&u.message),
            tl::enums::Update::EditChannelMessage(u) => msg_of(&u.message),
            _ => None,
        })
        .collect()
}

/// The type Telegram's apps show a document as, from its extension (they preview images, play
/// audio and video); Open Explorer gets the same bytes back whatever it is.
fn mime_of(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "ogg" | "oga" => "audio/ogg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "txt" | "log" | "md" | "csv" => "text/plain",
        _ => "application/octet-stream",
    }
}

fn random_i64() -> i64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("the OS gives random bytes");
    i64::from_le_bytes(b)
}

fn read_full(src: &mut dyn Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match src.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

impl Real {
    /// The account behind `conn`, storing in `channel`.
    pub fn new(conn: Conn, channel: PeerRef, premium: bool) -> Real {
        let shared = Arc::new(Shared {
            client: conn.client.clone(),
            channel,
            waits: std::sync::Mutex::new(Vec::new()),
            parts: Semaphore::new(PARTS_AT_ONCE),
            sends: Semaphore::new(1),
            authorized: AsyncMutex::new(Vec::new()),
        });
        Real { conn, shared, premium }
    }

    pub fn session(&self) -> &Arc<KeyringSession> {
        &self.conn.session
    }

    fn block<F: Future>(&self, f: F) -> F::Output {
        self.conn.rt.block_on(f)
    }

    async fn pts(&self) -> Result<i32> {
        let s = &self.shared;
        let request = tl::functions::channels::GetFullChannel { channel: s.channel.into() };
        let tl::enums::messages::ChatFull::Full(f) = s.patient(&NEVER, || s.client.invoke(&request)).await?;
        match f.full_chat {
            tl::enums::ChatFull::ChannelFull(c) => Ok(c.pts),
            _ => Err(t("O canal de armazenamento não é um canal.", "The storage channel isn't a channel.").into()),
        }
    }

    /// The message `id`, as Telegram has it now (with a fresh file reference).
    async fn fetch(&self, id: i32) -> Result<grammers_client::message::Message> {
        let s = &self.shared;
        let ids = [id];
        let got = s.patient(&NEVER, || s.client.get_messages_by_id(s.channel, &ids)).await?;
        got.into_iter()
            .flatten()
            .find(|m| !matches!(m.raw, tl::enums::Message::Empty(_)))
            .ok_or_else(|| t("O arquivo não existe mais no Telegram.", "The file doesn't exist on Telegram anymore.").into())
    }

    /// Sends `len` bytes of `src` in parts, [`UPLOAD_AT_ONCE`] at a time.
    async fn upload(&self, name: &str, src: &mut dyn Read, len: u64, progress: Progress<'_>) -> Result<Uploaded> {
        let parts = len.div_ceil(PART as u64).max(1);
        let limit = if self.premium { 8000 } else { 4000 };
        if parts > limit {
            return Err(tr!("\"{name}\" passa do limite de arquivo do Telegram.", "\"{name}\" is over Telegram's file limit."));
        }
        let (big, parts) = (len > BIG, parts as i32);
        let file_id = random_i64();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut set = tokio::task::JoinSet::new();
        let mut done = 0u64;
        let stop = |set: &mut tokio::task::JoinSet<Result<usize>>, cancel: &AtomicBool| {
            cancel.store(true, Ordering::SeqCst);
            set.abort_all();
            Err(CANCELLED.to_string())
        };
        for part in 0..parts {
            // Never past `len`: a file growing meanwhile fails at once, not after every part.
            let mut buf = vec![0u8; PART.min((len - part as u64 * PART as u64) as usize)];
            let n = read_full(src, &mut buf).map_err(|e| tr!("Não consegui ler o arquivo: {e}", "Couldn't read the file: {e}"))?;
            buf.truncate(n);
            while set.len() >= UPLOAD_AT_ONCE {
                match tokio::time::timeout(Duration::from_millis(250), set.join_next()).await {
                    Ok(Some(r)) => done += r.map_err(|e| e.to_string())?? as u64,
                    Ok(None) => {}
                    Err(_) => {}
                }
                if !progress(done) {
                    return stop(&mut set, &cancel);
                }
            }
            let (s, cancel) = (self.shared.clone(), cancel.clone());
            set.spawn(async move {
                let _slot = s.parts.acquire().await.map_err(|e| e.to_string())?;
                let ok = s
                    .patient(&cancel, || async {
                        if big {
                            s.client.invoke(&tl::functions::upload::SaveBigFilePart { file_id, file_part: part, file_total_parts: parts, bytes: buf.clone() }).await
                        } else {
                            s.client.invoke(&tl::functions::upload::SaveFilePart { file_id, file_part: part, bytes: buf.clone() }).await
                        }
                    })
                    .await?;
                if ok { Ok(n) } else { Err(t("O Telegram recusou uma parte do arquivo.", "Telegram refused a part of the file.").to_string()) }
            });
        }
        while !set.is_empty() {
            if let Ok(Some(r)) = tokio::time::timeout(Duration::from_millis(250), set.join_next()).await {
                done += r.map_err(|e| e.to_string())?? as u64;
            }
            if !progress(done) {
                return stop(&mut set, &cancel);
            }
        }
        if done != len {
            return Err(tr!("O arquivo mudou durante o envio ({done} de {len} bytes).", "The file changed while being sent ({done} of {len} bytes)."));
        }
        let raw: tl::enums::InputFile = if big {
            tl::types::InputFileBig { id: file_id, parts, name: name.into() }.into()
        } else {
            tl::types::InputFile { id: file_id, parts, name: name.into(), md5_checksum: String::new() }.into()
        };
        Ok(Uploaded { raw })
    }

    /// Sends a message of the account's, one at a time.
    async fn send(&self, cancel: &AtomicBool, message: impl Fn() -> InputMessage) -> Result<Msg> {
        let s = &self.shared;
        let _turn = s.sends.acquire().await.map_err(|e| e.to_string())?;
        let m = s.patient(cancel, || s.client.send_message(s.channel, message())).await?;
        msg_of(&m.raw).ok_or_else(|| t("O Telegram não devolveu a mensagem enviada.", "Telegram didn't return the message sent.").into())
    }
}

impl Backend for Real {
    fn history(&self) -> Result<(Vec<Msg>, i32)> {
        self.block(async {
            let pts = self.pts().await?;
            let s = &self.shared;
            let mut it = s.client.iter_messages(s.channel);
            let mut out = Vec::new();
            loop {
                let next = loop {
                    match it.next().await {
                        Err(InvocationError::Rpc(e)) if e.code == 420 => s.wait(e.value.unwrap_or(5) as u64, &NEVER).await?,
                        other => break other.map_err(said)?,
                    }
                };
                let Some(m) = next else { break };
                out.extend(msg_of(&m.raw));
            }
            Ok((out, pts))
        })
    }

    fn changes(&self, pts: i32) -> Result<Changes> {
        self.block(async {
            let s = &self.shared;
            let mut upserts: Vec<Msg> = Vec::new();
            let mut deleted: Vec<i32> = Vec::new();
            let mut pts = pts;
            for _ in 0..1000 {
                let request = tl::functions::updates::GetChannelDifference { force: false, channel: s.channel.into(), filter: tl::enums::ChannelMessagesFilter::Empty, pts, limit: 100 };
                match s.patient(&NEVER, || s.client.invoke(&request)).await? {
                    tl::enums::updates::ChannelDifference::Empty(d) => return Ok(Changes::Since { upserts, deleted, pts: d.pts }),
                    tl::enums::updates::ChannelDifference::TooLong(_) => return Ok(Changes::TooLong),
                    tl::enums::updates::ChannelDifference::Difference(d) => {
                        let mut seen: Vec<Msg> = d.new_messages.iter().filter_map(msg_of).collect();
                        for u in &d.other_updates {
                            match u {
                                tl::enums::Update::NewChannelMessage(u) => seen.extend(msg_of(&u.message)),
                                tl::enums::Update::EditChannelMessage(u) => seen.extend(msg_of(&u.message)),
                                tl::enums::Update::DeleteChannelMessages(u) => {
                                    upserts.retain(|m| !u.messages.contains(&m.id));
                                    deleted.extend(&u.messages);
                                }
                                _ => {}
                            }
                        }
                        for m in seen {
                            if !deleted.contains(&m.id) {
                                upserts.retain(|u| u.id != m.id);
                                upserts.push(m);
                            }
                        }
                        pts = d.pts;
                        if d.r#final {
                            return Ok(Changes::Since { upserts, deleted, pts });
                        }
                    }
                }
            }
            Ok(Changes::TooLong)
        })
    }

    fn send_text(&self, text: &str) -> Result<Msg> {
        self.block(self.send(&NEVER, || InputMessage::new().text(text)))
    }

    fn edit(&self, id: i32, text: &str) -> Result<Option<Msg>> {
        self.block(async {
            let s = &self.shared;
            let request = tl::functions::messages::EditMessage {
                no_webpage: true,
                invert_media: false,
                peer: s.channel.into(),
                id,
                message: Some(text.into()),
                media: None,
                reply_markup: None,
                entities: None,
                schedule_date: None,
                schedule_repeat_period: None,
                quick_reply_shortcut_id: None,
                rich_message: None,
            };
            let answer = s
                .patient(&NEVER, || async {
                    match s.client.invoke(&request).await {
                        Err(InvocationError::Rpc(e)) if e.name == "MESSAGE_EDIT_TIME_EXPIRED" || e.name == "MESSAGE_NOT_MODIFIED" => Ok(Err(e.name)),
                        other => other.map(Ok),
                    }
                })
                .await?;
            match answer {
                Ok(updates) => match sent_in(&updates).into_iter().find(|m| m.id == id) {
                    Some(m) => Ok(Some(m)),
                    None => Ok(msg_of(&self.fetch(id).await?.raw)),
                },
                Err(name) if name == "MESSAGE_NOT_MODIFIED" => Ok(msg_of(&self.fetch(id).await?.raw)),
                Err(_) => Ok(None),
            }
        })
    }

    fn send_file(&self, caption: &str, name: &str, src: &mut dyn Read, len: u64, progress: Progress) -> Result<Msg> {
        self.block(async {
            let up = self.upload(name, src, len, &mut *progress).await?;
            // The last part is sent; what's left is one message, which a wait may delay: Cancelar
            // still works then.
            let cancel = AtomicBool::new(false);
            let watch = async {
                loop {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    if !progress(len) {
                        cancel.store(true, Ordering::SeqCst);
                    }
                }
            };
            tokio::select! {
                sent = self.send(&cancel, || InputMessage::new().text(caption).file(up.clone())) => sent,
                () = watch => unreachable!("the watch never ends"),
            }
        })
    }

    fn send_group(&self, files: Vec<Small>) -> Result<Vec<Msg>> {
        self.block(async {
            // Each file is uploaded and made a document first, so a wait before the group goes
            // retries only the group, not files already used.
            let s = &self.shared;
            let mut docs = Vec::new();
            for f in &files {
                let up = self.upload(&f.name, &mut &f.bytes[..], f.bytes.len() as u64, &mut |_| true).await?;
                let raw: tl::enums::InputMedia = tl::types::InputMediaUploadedDocument {
                    nosound_video: false,
                    force_file: true,
                    spoiler: false,
                    file: up.raw,
                    thumb: None,
                    mime_type: mime_of(&f.name).into(),
                    attributes: vec![tl::types::DocumentAttributeFilename { file_name: f.name.clone() }.into()],
                    stickers: None,
                    ttl_seconds: None,
                    video_cover: None,
                    video_timestamp: None,
                }
                .into();
                let media = s.patient(&NEVER, || s.client.upload_media(s.channel, raw.clone())).await?;
                let Some(Media::Document(d)) = media else {
                    return Err(tr!("O Telegram não aceitou \"{}\" como arquivo.", "Telegram didn't take \"{}\" as a file.", f.name));
                };
                docs.push((f.caption.clone(), tl::enums::InputMedia::from(d.to_raw_input_media())));
            }
            let _turn = s.sends.acquire().await.map_err(|e| e.to_string())?;
            let sent = s.patient(&NEVER, || s.client.send_album(s.channel, docs.iter().map(|(c, d)| InputMedia::new().caption(c.as_str()).media(d.clone())).collect())).await?;
            let msgs: Vec<Msg> = sent.into_iter().flatten().filter_map(|m| msg_of(&m.raw)).collect();
            if msgs.len() != files.len() {
                return Err(tr!("O Telegram devolveu {} de {} arquivos do grupo.", "Telegram returned {} of the group's {} files.", msgs.len(), files.len()));
            }
            Ok(msgs)
        })
    }

    fn copy(&self, id: i32, caption: &str) -> Result<Msg> {
        self.block(async {
            let m = self.fetch(id).await?;
            let media = m.media().ok_or_else(|| t("A mensagem não tem arquivo.", "The message has no file.").to_string())?;
            self.send(&NEVER, || InputMessage::new().text(caption).copy_media(&media)).await
        })
    }

    fn delete(&self, ids: &[i32]) -> Result<()> {
        self.block(async {
            let s = &self.shared;
            for chunk in ids.chunks(100) {
                s.patient(&NEVER, || s.client.delete_messages(s.channel, chunk)).await?;
            }
            Ok(())
        })
    }

    fn download(&self, id: i32, out: &mut dyn Write, progress: Progress) -> Result<()> {
        self.block(async {
            let m = self.fetch(id).await?;
            let media: Media = m.media().ok_or_else(|| t("A mensagem não tem arquivo.", "The message has no file.").to_string())?;
            let location = media.to_raw_input_location().ok_or_else(|| t("O Telegram não deu o local do arquivo.", "Telegram didn't give the file's location.").to_string())?;
            let size = media.size().unwrap_or(0) as u64;
            let dc = media.dc_id().ok_or_else(|| t("O Telegram não disse onde o arquivo está.", "Telegram didn't say where the file is.").to_string())?;
            let count = size.div_ceil(PART as u64) as usize;
            // Each part waits its turn in the account's budget; no more than twice that many
            // parts wait in memory to be written in order.
            let window = Arc::new(Semaphore::new(PARTS_AT_ONCE * 2));
            let next = Arc::new(AtomicUsize::new(0));
            let cancel = Arc::new(AtomicBool::new(false));
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut tasks = tokio::task::JoinSet::new();
            for _ in 0..PARTS_AT_ONCE.min(count.max(1)) {
                let (s, location, window, next, cancel, tx) = (self.shared.clone(), location.clone(), window.clone(), next.clone(), cancel.clone(), tx.clone());
                tasks.spawn(async move {
                    let mut dc = dc;
                    loop {
                        let held = window.clone().acquire_owned().await.map_err(|e| e.to_string())?;
                        let i = next.fetch_add(1, Ordering::SeqCst);
                        if i >= count {
                            return Ok(());
                        }
                        let bytes = s.part(&location, &mut dc, (i * PART) as i64, &cancel).await?;
                        if tx.send((i, bytes, held)).is_err() {
                            return Ok(());
                        }
                    }
                });
            }
            drop(tx);
            let mut waiting = std::collections::BTreeMap::new();
            let (mut at, mut written) = (0usize, 0u64);
            let failed = 'run: loop {
                match tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
                    Ok(Some((i, bytes, held))) => {
                        waiting.insert(i, (bytes, held));
                        while let Some((bytes, _held)) = waiting.remove(&at) {
                            if let Err(e) = out.write_all(&bytes) {
                                break 'run Some(tr!("Não consegui gravar o arquivo: {e}", "Couldn't write the file: {e}"));
                            }
                            written += bytes.len() as u64;
                            at += 1;
                        }
                    }
                    Ok(None) => break None,
                    Err(_) => {}
                }
                // A part that failed never arrives: its error ends the download.
                while let Some(r) = tasks.try_join_next() {
                    match r {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => break 'run Some(e),
                        Err(e) => break 'run Some(e.to_string()),
                    }
                }
                if !progress(written) {
                    break Some(CANCELLED.to_string());
                }
            };
            if let Some(e) = failed {
                cancel.store(true, Ordering::SeqCst);
                tasks.abort_all();
                return Err(e);
            }
            while let Some(r) = tasks.join_next().await {
                r.map_err(|e| e.to_string())??;
            }
            if written != size {
                return Err(tr!("O download parou em {written} de {size} bytes.", "The download stopped at {written} of {size} bytes."));
            }
            Ok(())
        })
    }

    fn max_file(&self) -> u64 {
        (if self.premium { 8000 } else { 4000 }) * PART as u64
    }

    fn waiting_until(&self) -> Option<i64> {
        // The longest of the waits under way: when the account sends freely again.
        self.shared.waits.lock().unwrap_or_else(|e| e.into_inner()).iter().copied().max().filter(|u| *u > now())
    }

    fn sign_out(&self) {
        self.conn.session.detach();
        let _ = self.block(self.conn.client.sign_out());
    }

    fn detach(&self) {
        self.conn.session.detach();
    }
}
