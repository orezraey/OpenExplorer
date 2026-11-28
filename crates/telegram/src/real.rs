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
//! Requests that carry file parts (both ways) share one budget per account, and go over
//! connections of their own to the DC, as in Telegram's own apps (TDLib, Android, Desktop): each
//! is a session of its own over the account's key. A download goes over one to the DC's media
//! address, an upload is shared out among several to the DC's own (`lanes` has which, and what
//! each was measured to bring). The extra connections run on copies of the session that save
//! nothing, made once the account's own connection holds an authorized key for that DC. One that
//! breaks is dropped, and that DC gets one fewer for a while. Messages are sent one at a time per account:
//! Telegram paces them anyway, and several at once only collect several waits.

use std::collections::HashMap;
use std::future::Future;
use std::io::{Read, Write};
use std::net::SocketAddrV4;
use std::num::NonZeroU32;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use grammers_client::media::{Downloadable, InputMedia, Media, Uploaded};
use grammers_client::message::InputMessage;
use grammers_client::{Client, InvocationError};
use grammers_mtsender::{ConnectionParams, RetryContext, RetryPolicy, SenderPool, SenderPoolHandle};
use grammers_session::types::{PeerAuth, PeerId, PeerKind, PeerRef};
use grammers_tl_types as tl;
use openexplorer_i18n::{t, tr};
use tokio::sync::{Mutex as AsyncMutex, Semaphore, mpsc};

use crate::account::now;
use crate::backend::{Backend, Changes, Paged, Progress, Small};
use crate::budget::Budget;
use crate::chats::{self, ChatFile, ChatRef, Cursor, Kind, Month, Page};
use crate::lanes::{self, Lanes, Link, Tuning};
use crate::places::{self, Paging, Places};
use crate::session::KeyringSession;
use crate::tree::{Doc, Msg};
use crate::{CANCELLED, Result};

/// A file part, both ways: Telegram's largest for a download that grammers handles.
pub const PART: usize = 512 * 1024;

/// Above this a file goes in "big file" parts.
const BIG: u64 = 10 * 1024 * 1024;
/// The connections and the file parts in flight of a Premium account and of a free one (`lanes`
/// has what each was measured to bring). Four connections to the DC's own address fill a
/// 31 MiB/s line with an upload, one to its media address brings a download faster than any
/// other number that lasted, and 32 parts in flight bought nothing over 16 on it. Telegram's
/// own apps open more (8 each way with Premium): here 8 to the media address were fast for one
/// run and closed by Telegram in every run after, and 32 parts in flight over 4 or 8 to the
/// DC's own address were refused (`-429`). Why isn't known; those runs came after others that
/// had asked for more still. A free account wasn't measured: it gets the same, and Telegram
/// caps its speed anyway (FLOOD_PREMIUM_WAIT).
pub const PREMIUM: Tuning = Tuning { general: 4, media: 1, parts: 16 };
pub const FREE: Tuning = Tuning { general: 4, media: 1, parts: 16 };
/// Parts of one upload in flight.
const UPLOAD_AT_ONCE: usize = 4;
/// How long a DC keeps the fewer connections it was left with when one broke: a short network
/// drop breaks them all, and the speed should come back after it.
const CEILING_FOR: Duration = Duration::from_secs(60);
/// How long a file's place counts as known (see `places`), and how many are kept. Telegram's
/// references to a file last hours; a download that starts on one this old still ends in time.
const PLACE_FRESH: Duration = Duration::from_secs(10 * 60);
const PLACES: usize = 20_000;
/// What a part answers when Telegram no longer takes the file's place: never shown as it is.
const STALE: &str = "FILE_REFERENCE";
/// How long a connection has to answer its probe, a request of a few bytes sent when it's made
/// and again when a request on it was cut: connecting and that answer take half a second from
/// far away. An address that swallows the connection would else hold the parts sent there for
/// minutes.
const FIRST_ANSWER: Duration = Duration::from_secs(15);
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

/// How every connection of ours introduces itself and retries.
fn params() -> ConnectionParams {
    ConnectionParams { device_model: "Open Explorer".into(), app_version: env!("CARGO_PKG_VERSION").into(), retry_policy: Box::new(Policy), ..Default::default() }
}

/// A runtime and a client over a session: what a login and an open account both run on.
pub struct Conn {
    pub rt: tokio::runtime::Runtime,
    pub client: Client,
    pub session: Arc<KeyringSession>,
    pub api_id: i32,
    /// The handle of the client's own connections, for file parts to share them out.
    main: SenderPoolHandle,
}

impl Conn {
    pub fn start(api_id: i32, session: Arc<KeyringSession>) -> Result<Conn> {
        // Each connection decrypts on its own task: a thread for each, up to the cores.
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get().clamp(2, 8));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .thread_name("telegram")
            .enable_all()
            .build()
            .map_err(|e| tr!("Não consegui iniciar a conexão com o Telegram: {e}", "Couldn't start the connection to Telegram: {e}"))?;
        let (client, main) = {
            let _in = rt.enter();
            let SenderPool { runner, handle, .. } = SenderPool::with_configuration(session.clone(), api_id, params());
            rt.spawn(runner.run());
            let main = handle.thin.clone();
            (Client::new(handle), main)
        };
        Ok(Conn { rt, client, session, api_id, main })
    }
}

impl Conn {
    /// A handle that ends the account's own connection to a DC, as Telegram may at any moment:
    /// the live tests, to see the requests on it asked again.
    pub fn breaker(&self) -> impl Fn(i32) + Send + Sync + 'static {
        let main = self.main.clone();
        move |dc| {
            main.disconnect_from_dc(dc);
        }
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

/// A connection that ended with the request on it (Telegram may close one at any moment): asked
/// again, it goes over a new one.
fn dropped(e: &InvocationError) -> bool {
    matches!(e, InvocationError::Dropped(_) | InvocationError::Io(_) | InvocationError::Transport(_))
}

/// How many times a request whose connection ended is asked again, a pause of 1, 2, then 4
/// seconds before each: some 30 s in all, for a connection busy with many file parts.
const DROPS: u32 = 10;

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
    /// The file parts in flight, both ways together: one budget for the account's life, made
    /// bigger or smaller in place when tuned.
    budget: Budget,
    sends: Semaphore,
    /// The DCs this session was authorized in, besides its own.
    authorized: AsyncMutex<Vec<i32>>,
    session: Arc<KeyringSession>,
    api_id: i32,
    /// The connections file parts go over.
    lanes: Lanes<SenderPoolHandle>,
    media: AsyncMutex<MediaList>,
}

/// Each DC's media address, once Telegram's list of them was asked; and when asking failed
/// last, not to ask with every part.
#[derive(Default)]
struct MediaList {
    known: Option<HashMap<i32, SocketAddrV4>>,
    failed: Option<Instant>,
}

impl Link for SenderPoolHandle {
    fn quit(&self) {
        SenderPoolHandle::quit(self);
    }
}

/// Each DC's address kept for files, from Telegram's list of its DCs' addresses: an IPv4 one
/// that takes a plain connection, and not a CDN's (those hold other files, under other keys).
pub fn media_addresses(options: &[tl::enums::DcOption]) -> HashMap<i32, SocketAddrV4> {
    let mut out = HashMap::new();
    for tl::enums::DcOption::Option(o) in options {
        if o.media_only && !o.cdn && !o.ipv6 && !o.tcpo_only
            && let (Ok(ip), Ok(port)) = (o.ip_address.parse(), u16::try_from(o.port))
        {
            out.entry(o.id).or_insert(SocketAddrV4::new(ip, port));
        }
    }
    out
}

/// Counts a request as in flight on a lane until dropped, also when its task is aborted.
struct Busy<'a>(&'a AtomicUsize);

impl<'a> Busy<'a> {
    fn on(count: &'a AtomicUsize) -> Busy<'a> {
        count.fetch_add(1, Ordering::SeqCst);
        Busy(count)
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// An error after which an extra connection is no use: it ended, or its key isn't known there.
/// One whose request was only cut (`Io`) may come back: it leaves the round and is probed.
fn broken(e: &InvocationError) -> bool {
    matches!(e, InvocationError::Dropped(_) | InvocationError::Transport(_) | InvocationError::Authentication(_) | InvocationError::Deserialize(_))
        || matches!(e, InvocationError::Rpc(r) if r.name == "AUTH_KEY_UNREGISTERED")
}

/// How many waits Telegram asked for in this process, and their seconds: for logs and tests.
pub static WAITS: AtomicUsize = AtomicUsize::new(0);
pub static WAITED_SECS: AtomicUsize = AtomicUsize::new(0);
/// Of those, how many were FLOOD_PREMIUM_WAIT: Telegram's cap on a free account's speed.
pub static PREMIUM_WAITS: AtomicUsize = AtomicUsize::new(0);
/// The requests asked again because their connection ended: for logs and tests.
pub static DROPPED_CALLS: AtomicUsize = AtomicUsize::new(0);
/// The extra connections dropped in this process, and why: for logs and tests.
pub static DROPPED_LANES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
/// The transport-level refusals (`-429`) in this process, on any connection: for logs and tests.
pub static REFUSALS: AtomicUsize = AtomicUsize::new(0);
/// What happened lately to the connections that carry file parts, and when: for the benchmark.
pub static EVENTS: std::sync::Mutex<Vec<(Instant, String)>> = std::sync::Mutex::new(Vec::new());

fn note(event: String) {
    let mut events = EVENTS.lock().unwrap_or_else(|e| e.into_inner());
    // An app open for days keeps the last few hundred, not all.
    if events.len() >= 400 {
        events.drain(..200);
    }
    events.push((Instant::now(), event));
}

/// The requests made to ask where files are kept (see `places`): for the benchmark.
pub static PLACE_ASKS: AtomicUsize = AtomicUsize::new(0);

/// A flag nothing sets: for requests no one cancels.
static NEVER: AtomicBool = AtomicBool::new(false);

impl Shared {
    /// Runs `call` until it answers, waiting out FLOOD_WAITs and `-429`s, and asking again (up to
    /// [`DROPS`] times) when its connection ended; `cancel` stops a wait.
    async fn patient<T, F, Fut>(&self, cancel: &AtomicBool, call: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = std::result::Result<T, InvocationError>>,
    {
        self.patient_with(cancel, DROPS, call).await
    }

    /// [`Shared::patient`] for a message sent: one whose connection ended may have arrived, and
    /// sent again would be there twice, so it isn't asked again.
    async fn patient_send<T, F, Fut>(&self, cancel: &AtomicBool, call: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = std::result::Result<T, InvocationError>>,
    {
        self.patient_with(cancel, 0, call).await
    }

    async fn patient_with<T, F, Fut>(&self, cancel: &AtomicBool, drops: u32, mut call: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = std::result::Result<T, InvocationError>>,
    {
        let mut busy = 0u32;
        let mut ended = 0u32;
        loop {
            match call().await {
                Ok(v) => return Ok(v),
                Err(InvocationError::Rpc(e)) if e.code == 420 => {
                    if e.name.starts_with("FLOOD_PREMIUM_WAIT") {
                        PREMIUM_WAITS.fetch_add(1, Ordering::Relaxed);
                    }
                    let secs = e.value.unwrap_or(5) as u64;
                    if secs > LONGEST_WAIT {
                        return Err(tr!("O Telegram pediu para esperar {} antes de continuar.", "Telegram asked to wait {} before going on.", span(secs)));
                    }
                    self.wait(secs, cancel).await?;
                }
                Err(e) if too_many(&e) && busy < 6 => {
                    REFUSALS.fetch_add(1, Ordering::Relaxed);
                    note("the account's own connection was refused (-429)".to_string());
                    busy += 1;
                    self.wait(1 << busy.min(4), cancel).await?;
                }
                Err(e) if dropped(&e) && ended < drops => {
                    DROPPED_CALLS.fetch_add(1, Ordering::Relaxed);
                    note(format!("a request's connection ended: {e}"));
                    // A pause of ours, not a wait Telegram asked for: the queue doesn't show it.
                    let end = Instant::now() + Duration::from_secs(1 << ended.min(2));
                    while Instant::now() < end {
                        if cancel.load(Ordering::SeqCst) {
                            return Err(CANCELLED.to_string());
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    ended += 1;
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
        // Exported bytes are taken once: a retry exports anew.
        let export = tl::functions::auth::ExportAuthorization { dc_id: dc };
        self.patient(&NEVER, || async {
            let tl::enums::auth::ExportedAuthorization::Authorization(a) = self.client.invoke(&export).await?;
            self.client.invoke_in_dc(dc, &tl::functions::auth::ImportAuthorization { id: a.id, bytes: a.bytes }).await
        })
        .await?;
        done.push(dc);
        Ok(())
    }

    /// The media address of `dc`, if it has one: Telegram's list is asked once, and again a
    /// while after asking failed (the downloads meanwhile go the DC's own way).
    async fn media_of(&self, dc: i32) -> Option<SocketAddrV4> {
        let mut list = self.media.lock().await;
        if list.known.is_none() && list.failed.is_none_or(|at| at.elapsed() >= CEILING_FOR) {
            // The parts of a transfer all wait here for the one that asks.
            match self.client.invoke(&tl::functions::help::GetConfig {}).await {
                Ok(tl::enums::Config::Config(c)) => list.known = Some(media_addresses(&c.dc_options)),
                Err(_) => list.failed = Some(Instant::now()),
            }
        }
        list.known.as_ref()?.get(&dc).copied()
    }

    /// A connection of its own to `dc`, at `address` rather than where the account's goes.
    fn open(&self, dc: i32, address: Option<SocketAddrV4>) -> SenderPoolHandle {
        let copy = self.session.unsaved_copy();
        if let Some(address) = address {
            copy.set_address(dc, address);
        }
        let SenderPool { runner, handle, .. } = SenderPool::with_configuration(Arc::new(copy), self.api_id, params());
        tokio::spawn(runner.run());
        note(format!("DC {dc}: a connection opened to {}", if address.is_some() { "its media address" } else { "its own address" }));
        handle.thin.clone()
    }

    /// Opens the connections a file part to `dc` goes over (a download's when `down`), the ones
    /// `lanes` says are missing, once the account's own holds a key there that's authorized (its
    /// own DC, or one it imported its authorization into): the copies then carry that key and
    /// need nothing of their own.
    async fn grow(self: &Arc<Self>, dc: i32, down: bool) {
        let media = if down && self.lanes.takes_media(dc) { self.media_of(dc).await } else { None };
        let Some(kind) = self.lanes.to_open(dc, down, media.is_some()) else { return };
        if !self.session.has_key(dc) || (dc != self.session.home() && !self.authorized.lock().await.contains(&dc)) {
            return;
        }
        let address = media.filter(|_| kind == lanes::Kind::Media);
        for lane in self.lanes.grow(dc, kind, || self.open(dc, address)) {
            self.probe(dc, lane);
        }
    }

    /// Asks `lane`, a connection to `dc` being made or one whose request was cut, something
    /// small: once it answers, parts go over it; if it doesn't within [`FIRST_ANSWER`], it's
    /// dropped. Any answer of Telegram's counts but that the key isn't known there.
    fn probe(self: &Arc<Self>, dc: i32, lane: Arc<lanes::Lane<SenderPoolHandle>>) {
        let s = self.clone();
        tokio::spawn(async move {
            let asked = lane.link.invoke_in_dc(dc, &tl::functions::help::GetNearestDc {});
            let why = match tokio::time::timeout(FIRST_ANSWER, asked).await {
                Ok(Ok(_)) => None,
                Ok(Err(InvocationError::Rpc(e))) if e.name != "AUTH_KEY_UNREGISTERED" => None,
                Ok(Err(e)) => Some(e.to_string()),
                Err(_) => Some("no answer".to_string()),
            };
            match why {
                None => s.lanes.answered(dc, &lane),
                Some(why) => s.dropped(dc, &lane, &why),
            }
        });
    }

    /// Drops `lane`, an extra connection to `dc` that broke, and notes it the first time.
    fn dropped(&self, dc: i32, lane: &Arc<lanes::Lane<SenderPoolHandle>>, why: &str) {
        let busy = lane.busy.load(Ordering::SeqCst);
        if self.lanes.drop_lane(dc, lane) {
            DROPPED_LANES.lock().unwrap_or_else(|e| e.into_inner()).push(format!("DC {dc}: {why}"));
            note(format!("DC {dc}: connection dropped {:.1} s after it opened, {busy} requests on it: {why}", lane.age().as_secs_f64()));
        }
    }

    /// Sends a file-part request to `dc` (a download's when `down`) over the connection `lanes`
    /// picks. An extra connection that broke is dropped, one whose request was cut leaves the
    /// round until it answers a probe, and either way the request goes over another: in the
    /// end the account's own, whose errors come back as they are.
    async fn call_in<R: tl::RemoteCall>(self: &Arc<Self>, dc: i32, down: bool, request: &R) -> std::result::Result<R::Return, InvocationError> {
        loop {
            let lane = self.lanes.pick(dc, down);
            let answer = {
                let _busy = Busy::on(&lane.busy);
                lane.link.invoke_in_dc(dc, request).await
            };
            // The account's key answered there: it's authorized, also when that was done in an
            // earlier run, so the extra connections may go there too.
            if answer.is_ok() && lane.main && dc != self.session.home() {
                let mut done = self.authorized.lock().await;
                if !done.contains(&dc) {
                    done.push(dc);
                }
            }
            match &answer {
                Err(e) if !lane.main && broken(e) => {
                    if too_many(e) {
                        REFUSALS.fetch_add(1, Ordering::Relaxed);
                        self.lanes.refused(dc);
                    }
                    self.dropped(dc, &lane, &e.to_string());
                }
                Err(InvocationError::Io(e)) if !lane.main => {
                    if self.lanes.falter(&lane) {
                        note(format!("DC {dc}: a request on a connection was cut ({e}): asking whether it still answers"));
                        self.probe(dc, lane.clone());
                    }
                }
                _ => return answer,
            }
        }
    }

    /// One part of a file, from wherever Telegram keeps it.
    async fn part(self: &Arc<Self>, location: &tl::enums::InputFileLocation, dc: &mut i32, offset: i64, cancel: &AtomicBool) -> Result<Vec<u8>> {
        loop {
            let _slot = self.budget.take().await;
            let request = tl::functions::upload::GetFile { precise: true, cdn_supported: false, location: location.clone(), offset, limit: PART as i32 };
            let here = *dc;
            self.grow(here, true).await;
            let got = self
                .patient(cancel, || async {
                    match self.call_in(here, true, &request).await {
                        // Moves, missing authorizations and a place gone stale come back to be
                        // handled below.
                        Err(InvocationError::Rpc(e)) if e.code == 303 || e.name == "AUTH_KEY_UNREGISTERED" || e.name.starts_with("FILE_REFERENCE_") => Ok(Err(e)),
                        other => other.map(Ok),
                    }
                })
                .await?;
            match got {
                Ok(tl::enums::upload::File::File(f)) => return Ok(f.bytes),
                Ok(tl::enums::upload::File::CdnRedirect(_)) => return Err(t("O Telegram mandou baixar de uma CDN, o que o Open Explorer ainda não faz.", "Telegram sent the download to a CDN, which Open Explorer doesn't do yet.").into()),
                Err(e) if e.code == 303 => *dc = e.value.ok_or("FILE_MIGRATE")? as i32,
                Err(e) if e.name.starts_with("FILE_REFERENCE_") => return Err(STALE.into()),
                Err(_) => self.authorize_in(here).await?,
            }
        }
    }
}

/// Where Telegram keeps a message's file, as a download asks for it.
#[derive(Clone)]
struct Place {
    location: tl::enums::InputFileLocation,
    dc: i32,
    size: u64,
}

pub struct Real {
    conn: Conn,
    shared: Arc<Shared>,
    premium: bool,
    /// The places of the files asked about lately, by chat and message.
    places: Places<(PeerId, i32), Place>,
    /// Per channel, one download asks for a page at a time: those waiting find their file's
    /// place came with its answer.
    finding: std::sync::Mutex<HashMap<PeerId, Arc<AsyncMutex<()>>>>,
    /// Whether a page pays in each chat.
    paging: Paging<PeerId>,
    /// The account's own user, as Saved Messages' files are fetched by: asked once.
    me: std::sync::OnceLock<PeerRef>,
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

/// The kinds of file a chat's folders show, each searched apart: Telegram's search takes one.
const CHAT_FILTERS: [tl::enums::MessagesFilter; 4] = [
    tl::enums::MessagesFilter::InputMessagesFilterPhotoVideo,
    tl::enums::MessagesFilter::InputMessagesFilterDocument,
    tl::enums::MessagesFilter::InputMessagesFilterMusic,
    tl::enums::MessagesFilter::InputMessagesFilterRoundVoice,
];

/// How many pages of 100 one search of a chat reads at most: a month of a busy channel.
const SEARCH_PAGES: usize = 50;

/// The peer a chat is asked about as: Saved Messages as the session's own user.
fn chat_peer(at: &ChatRef) -> PeerRef {
    let id = match at.kind {
        Kind::Saved => PeerId::self_user(),
        Kind::User => PeerId::user_unchecked(at.id),
        Kind::Group => PeerId::chat_unchecked(at.id),
        Kind::Channel => PeerId::channel_unchecked(at.id),
    };
    PeerRef { id, auth: PeerAuth::from_hash(at.hash) }
}

/// A chat's file, from its message; `None` for a message with none, a sticker or a GIF.
fn chat_file_of(raw: &tl::enums::Message) -> Option<ChatFile> {
    let tl::enums::Message::Message(m) = raw else { return None };
    let date = m.date as i64;
    match m.media.as_ref()? {
        tl::enums::MessageMedia::Photo(_) => {
            let doc = doc_of(&m.media)?;
            Some(ChatFile { msg: m.id, name: chats::file_name(chats::Media::Photo, "", None, "", date), size: doc.size, date })
        }
        tl::enums::MessageMedia::Document(d) => {
            let Some(tl::enums::Document::Document(doc)) = &d.document else { return None };
            let (mut given, mut media, mut music) = ("", chats::Media::Document, None);
            for a in &doc.attributes {
                match a {
                    tl::enums::DocumentAttribute::Filename(f) => given = &f.file_name,
                    tl::enums::DocumentAttribute::Audio(a) if a.voice => media = chats::Media::Voice,
                    tl::enums::DocumentAttribute::Audio(a) => {
                        media = chats::Media::Music;
                        music = Some((a.performer.as_deref().unwrap_or(""), a.title.as_deref().unwrap_or("")));
                    }
                    tl::enums::DocumentAttribute::Video(v) if v.round_message => media = chats::Media::Round,
                    tl::enums::DocumentAttribute::Video(_) if media == chats::Media::Document => media = chats::Media::Video,
                    tl::enums::DocumentAttribute::Sticker(_) | tl::enums::DocumentAttribute::Animated => return None,
                    _ => {}
                }
            }
            Some(ChatFile { msg: m.id, name: chats::file_name(media, given, music, &doc.mime_type, date), size: doc.size as u64, date })
        }
        _ => None,
    }
}

/// A user's name as their chat's folder shows it; empty for a deleted account.
fn user_name(u: &tl::types::User) -> String {
    if u.deleted {
        return String::new();
    }
    let name = format!("{} {}", u.first_name.as_deref().unwrap_or(""), u.last_name.as_deref().unwrap_or(""));
    let name = name.trim();
    if name.is_empty() { u.username.as_deref().map(|n| format!("@{n}")).unwrap_or_default() } else { name.to_string() }
}

/// The messages of an answer to a search or a history request.
fn messages_of(answer: tl::enums::messages::Messages) -> Vec<tl::enums::Message> {
    match answer {
        tl::enums::messages::Messages::Messages(m) => m.messages,
        tl::enums::messages::Messages::Slice(m) => m.messages,
        tl::enums::messages::Messages::ChannelMessages(m) => m.messages,
        tl::enums::messages::Messages::NotModified(_) => Vec::new(),
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
        let tuning = if premium { PREMIUM } else { FREE };
        let shared = Arc::new(Shared {
            client: conn.client.clone(),
            channel,
            waits: std::sync::Mutex::new(Vec::new()),
            budget: Budget::new(tuning.parts),
            sends: Semaphore::new(1),
            authorized: AsyncMutex::new(Vec::new()),
            session: conn.session.clone(),
            api_id: conn.api_id,
            lanes: Lanes::new(conn.main.clone(), tuning, CEILING_FOR),
            media: AsyncMutex::default(),
        });
        Real { conn, shared, premium, places: Places::new(PLACE_FRESH, PLACES), finding: std::sync::Mutex::default(), paging: Paging::default(), me: std::sync::OnceLock::new() }
    }

    pub fn session(&self) -> &Arc<KeyringSession> {
        &self.conn.session
    }

    /// Parts of one upload in flight: [`UPLOAD_AT_ONCE`] a connection, within the budget.
    fn upload_at_once(&self) -> usize {
        let s = &self.shared;
        (UPLOAD_AT_ONCE * s.lanes.general()).min(s.budget.size()).max(1)
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

    /// The message `id` of `peer`, as Telegram has it now (with a fresh file reference).
    async fn fetch(&self, peer: PeerRef, id: i32) -> Result<grammers_client::message::Message> {
        let s = &self.shared;
        let ids = [id];
        let got = s.patient(&NEVER, || s.client.get_messages_by_id(peer, &ids)).await?;
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
            while set.len() >= self.upload_at_once() {
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
                let _slot = s.budget.take().await;
                let home = s.session.home();
                s.grow(home, false).await;
                let ok = s
                    .patient(&cancel, || async {
                        if big {
                            s.call_in(home, false, &tl::functions::upload::SaveBigFilePart { file_id, file_part: part, file_total_parts: parts, bytes: buf.clone() }).await
                        } else {
                            s.call_in(home, false, &tl::functions::upload::SaveFilePart { file_id, file_part: part, bytes: buf.clone() }).await
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
        let m = s.patient_send(cancel, || s.client.send_message(s.channel, message())).await?;
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
        let changes = self.block(async {
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
        });
        // A message sent again, edited or deleted may hold another file now, or none: a place
        // kept for it would bring the old one (see `places`).
        match &changes {
            Ok(Changes::Since { upserts, deleted, .. }) => {
                let channel = self.shared.channel.id;
                for id in upserts.iter().map(|m| m.id).chain(deleted.iter().copied()) {
                    self.places.forget(&(channel, id));
                }
            }
            Ok(Changes::TooLong) => self.places.clear(),
            Err(_) => {}
        }
        changes
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
                    None => Ok(msg_of(&self.fetch(self.shared.channel, id).await?.raw)),
                },
                Err(name) if name == "MESSAGE_NOT_MODIFIED" => Ok(msg_of(&self.fetch(self.shared.channel, id).await?.raw)),
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
            let sent = s.patient_send(&NEVER, || s.client.send_album(s.channel, docs.iter().map(|(c, d)| InputMedia::new().caption(c.as_str()).media(d.clone())).collect())).await?;
            let msgs: Vec<Msg> = sent.into_iter().flatten().filter_map(|m| msg_of(&m.raw)).collect();
            if msgs.len() != files.len() {
                return Err(tr!("O Telegram devolveu {} de {} arquivos do grupo.", "Telegram returned {} of the group's {} files.", msgs.len(), files.len()));
            }
            Ok(msgs)
        })
    }

    fn copy(&self, id: i32, caption: &str) -> Result<Msg> {
        self.block(async {
            let m = self.fetch(self.shared.channel, id).await?;
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
        self.download_in(self.shared.channel, id, out, progress)
    }

    fn has_chats(&self) -> bool {
        true
    }

    fn chats(&self, after: Option<&Cursor>, patient: bool) -> Result<Paged> {
        self.block(self.chats_page(after, patient))
    }

    fn chat_months(&self, chat: &ChatRef) -> Result<Vec<Month>> {
        self.block(async {
            let s = &self.shared;
            let peer: tl::enums::InputPeer = chat_peer(chat).into();
            let request = tl::functions::messages::GetSearchCounters { peer: peer.clone(), saved_peer_id: None, top_msg_id: None, filters: CHAT_FILTERS.to_vec() };
            let counters = s.patient(&NEVER, || s.client.invoke(&request)).await?;
            let mut months = Vec::new();
            for tl::enums::messages::SearchCounter::Counter(c) in counters {
                if c.count == 0 {
                    continue;
                }
                match self.calendar(&peer, &c.filter).await {
                    Ok(m) => months.extend(m),
                    // A kind the calendar doesn't take: its files, read through.
                    Err(_) => months.extend(self.search(&peer, &c.filter, 0, 0).await?.iter().filter_map(chat_file_of).map(|f| Month::of(f.date))),
                }
            }
            Ok(months)
        })
    }

    fn chat_files(&self, chat: &ChatRef, month: Month) -> Result<Vec<ChatFile>> {
        self.block(async {
            let peer: tl::enums::InputPeer = chat_peer(chat).into();
            let (start, end) = month.range();
            let mut out = Vec::new();
            for f in &CHAT_FILTERS {
                let raw = self.search(&peer, f, start, end).await?;
                out.extend(raw.iter().filter_map(chat_file_of).filter(|f| f.date >= start && f.date < end));
            }
            // A message listed with another size than its kept place has got another file.
            let peer = match chat.kind {
                Kind::Saved => self.me.get().map(|me| me.id),
                _ => Some(chat_peer(chat).id),
            };
            if let Some(peer) = peer {
                for f in &out {
                    self.places.keep_if(&(peer, f.msg), |place| place.size == f.size);
                }
            }
            Ok(out)
        })
    }

    fn chat_download(&self, chat: &ChatRef, msg: i32, out: &mut dyn Write, progress: Progress) -> Result<()> {
        // A message is fetched by its chat's own id: Saved Messages by the account's user, not
        // the stand-in for "myself" the searches take.
        let peer = match chat.kind {
            Kind::Saved => match self.me.get() {
                Some(me) => *me,
                None => {
                    let me = self.myself()?;
                    *self.me.get_or_init(|| me)
                }
            },
            _ => chat_peer(chat),
        };
        self.download_in(peer, msg, out, progress)
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

    fn connections_open(&self) -> Vec<(i32, usize)> {
        self.shared.lanes.open()
    }

    fn tune(&self, tuning: Tuning) {
        let s = &self.shared;
        s.budget.resize(tuning.parts);
        s.lanes.tune(tuning.general, tuning.media);
    }
}

impl Real {
    /// The account's own user, asked of Telegram.
    fn myself(&self) -> Result<PeerRef> {
        self.block(async {
            let s = &self.shared;
            let request = tl::functions::users::GetUsers { id: vec![tl::enums::InputUser::UserSelf] };
            let users = s.patient(&NEVER, || s.client.invoke(&request)).await?;
            users
                .into_iter()
                .find_map(|u| match u {
                    tl::enums::User::User(u) => Some(PeerRef { id: PeerId::user_unchecked(u.id), auth: PeerAuth::from_hash(u.access_hash.unwrap_or(0)) }),
                    _ => None,
                })
                .ok_or_else(|| t("O Telegram não disse quem é a conta.", "Telegram didn't say who the account is.").to_string())
        })
    }

    /// A page of the account's chats from `after` (see [`Backend::chats`]).
    async fn chats_page(&self, after: Option<&Cursor>, patient: bool) -> Result<Paged> {
        let s = &self.shared;
        // The archive is a list of its own: the main one shows it as one entry ("Arquivadas").
        let archived = after.is_some_and(|c| c.archived);
        let from = after.filter(|c| c.id != 0);
        let request = tl::functions::messages::GetDialogs {
            exclude_pinned: false,
            folder_id: archived.then_some(1),
            offset_date: from.map_or(0, |c| c.date),
            offset_id: from.map_or(0, |c| c.id),
            offset_peer: from.map_or(tl::enums::InputPeer::Empty, |c| chat_peer(&c.peer).into()),
            limit: 100,
            hash: 0,
        };
        let answer = if patient {
            s.patient(&NEVER, || s.client.invoke(&request)).await?
        } else {
            match s.client.invoke(&request).await {
                Ok(a) => a,
                Err(InvocationError::Rpc(e)) if e.code == 420 => return Ok(Paged::WaitUntil(now() + e.value.unwrap_or(5) as i64)),
                Err(e) => return Err(said(e)),
            }
        };
        let (dialogs, messages, chats, users, whole) = match answer {
            tl::enums::messages::Dialogs::Dialogs(d) => (d.dialogs, d.messages, d.chats, d.users, true),
            tl::enums::messages::Dialogs::Slice(d) => (d.dialogs, d.messages, d.chats, d.users, false),
            tl::enums::messages::Dialogs::NotModified(_) => return Ok(Paged::Page(Page { chats: Vec::new(), next: None })),
        };
        let full = dialogs.len() >= request.limit as usize;
        let storage = s.channel.id.bare_id_unchecked();
        let mut out = Vec::new();
        let mut last = None;
        for d in &dialogs {
            let tl::enums::Dialog::Dialog(d) = d else { continue };
            let date = messages
                .iter()
                .find_map(|m| match m {
                    tl::enums::Message::Message(m) if m.id == d.top_message && m.peer_id == d.peer => Some(m.date),
                    tl::enums::Message::Service(m) if m.id == d.top_message && m.peer_id == d.peer => Some(m.date),
                    _ => None,
                })
                .unwrap_or(0);
            // Where the listing goes on from, and the chat's name if it shows.
            let (at, shown): (ChatRef, Option<String>) = match &d.peer {
                tl::enums::Peer::User(p) => match users.iter().find_map(|u| match u {
                    tl::enums::User::User(u) if u.id == p.user_id => Some(u),
                    _ => None,
                }) {
                    Some(u) => (ChatRef { kind: if u.is_self { Kind::Saved } else { Kind::User }, id: u.id, hash: u.access_hash.unwrap_or(0) }, Some(user_name(u))),
                    None => (ChatRef { kind: Kind::User, id: p.user_id, hash: 0 }, None),
                },
                tl::enums::Peer::Chat(p) => match chats.iter().find(|c| c.id() == p.chat_id) {
                    // A group that became a supergroup shows as that.
                    Some(tl::enums::Chat::Chat(c)) if c.migrated_to.is_none() => (ChatRef { kind: Kind::Group, id: c.id, hash: 0 }, Some(c.title.clone())),
                    _ => (ChatRef { kind: Kind::Group, id: p.chat_id, hash: 0 }, None),
                },
                tl::enums::Peer::Channel(p) => match chats.iter().find(|c| c.id() == p.channel_id) {
                    Some(tl::enums::Chat::Channel(c)) => (ChatRef { kind: Kind::Channel, id: c.id, hash: c.access_hash.unwrap_or(0) }, (c.id != storage).then(|| c.title.clone())),
                    Some(tl::enums::Chat::ChannelForbidden(c)) => (ChatRef { kind: Kind::Channel, id: c.id, hash: c.access_hash }, None),
                    _ => (ChatRef { kind: Kind::Channel, id: p.channel_id, hash: 0 }, None),
                },
            };
            last = Some(Cursor { date, id: d.top_message, peer: at, archived });
            if let Some(name) = shown {
                out.push(chats::Chat { at, name, date: date as i64 });
            }
        }
        let next = if whole || !full { None } else { last };
        Ok(Paged::Page(Page { chats: out, next }))
    }

    /// The months with files of kind `filter` in `peer`, from Telegram's calendar of them: a day
    /// a period. A day that runs over two of the computer's months (Telegram's days may not be
    /// its) is placed by the dates of its first and last message.
    async fn calendar(&self, peer: &tl::enums::InputPeer, filter: &tl::enums::MessagesFilter) -> Result<Vec<Month>> {
        let s = &self.shared;
        let mut out = Vec::new();
        let mut across: Vec<(i32, i32, Month, Month)> = Vec::new();
        let mut offset_id = 0;
        for _ in 0..200 {
            let request = tl::functions::messages::GetSearchResultsCalendar { peer: peer.clone(), saved_peer_id: None, filter: filter.clone(), offset_id, offset_date: 0 };
            let tl::enums::messages::SearchResultsCalendar::Calendar(c) = s.patient(&NEVER, || s.client.invoke(&request)).await?;
            let mut lowest = i32::MAX;
            for tl::enums::SearchResultsCalendarPeriod::Period(p) in &c.periods {
                let (first, last) = (Month::of(p.date as i64), Month::of(p.date as i64 + 86_399));
                if first == last {
                    out.push(first);
                } else {
                    across.push((p.min_msg_id, p.max_msg_id, first, last));
                }
                lowest = lowest.min(p.min_msg_id);
            }
            if c.periods.is_empty() || lowest <= c.min_msg_id || (offset_id != 0 && lowest >= offset_id) {
                break;
            }
            offset_id = lowest;
        }
        let ids: Vec<i32> = across.iter().flat_map(|a| [a.0, a.1]).collect();
        let dates = self.dates_of(peer, &ids).await?;
        for (min, max, first, last) in across {
            match (dates.get(&min), dates.get(&max)) {
                (Some(&a), Some(&b)) => {
                    out.push(Month::of(a));
                    out.push(Month::of(b));
                }
                _ => out.extend([first, last]),
            }
        }
        Ok(out)
    }

    /// When each of messages `ids` of `peer` was sent, for those still there.
    async fn dates_of(&self, peer: &tl::enums::InputPeer, ids: &[i32]) -> Result<HashMap<i32, i64>> {
        let s = &self.shared;
        let mut out = HashMap::new();
        for chunk in ids.chunks(100) {
            let id: Vec<tl::enums::InputMessage> = chunk.iter().map(|&id| tl::types::InputMessageId { id }.into()).collect();
            let answer = match peer {
                tl::enums::InputPeer::Channel(c) => {
                    let request = tl::functions::channels::GetMessages { channel: tl::types::InputChannel { channel_id: c.channel_id, access_hash: c.access_hash }.into(), id };
                    s.patient(&NEVER, || s.client.invoke(&request)).await?
                }
                _ => {
                    let request = tl::functions::messages::GetMessages { id };
                    s.patient(&NEVER, || s.client.invoke(&request)).await?
                }
            };
            for m in messages_of(answer) {
                match m {
                    tl::enums::Message::Message(m) => out.insert(m.id, m.date as i64),
                    tl::enums::Message::Service(m) => out.insert(m.id, m.date as i64),
                    tl::enums::Message::Empty(_) => None,
                };
            }
        }
        Ok(out)
    }

    /// The messages of kind `filter` in `peer` sent from `from` and before `before` (0: no bound),
    /// newest first, up to [`SEARCH_PAGES`] pages. Telegram's search finds nothing when given
    /// both its date bounds, so it's given the end only and read back to the start.
    async fn search(&self, peer: &tl::enums::InputPeer, filter: &tl::enums::MessagesFilter, from: i64, before: i64) -> Result<Vec<tl::enums::Message>> {
        let s = &self.shared;
        let mut out = Vec::new();
        let mut offset_id = 0;
        for _ in 0..SEARCH_PAGES {
            let request = tl::functions::messages::Search {
                peer: peer.clone(),
                q: String::new(),
                from_id: None,
                saved_peer_id: None,
                saved_reaction: None,
                top_msg_id: None,
                filter: filter.clone(),
                min_date: 0,
                max_date: before.max(0) as i32,
                offset_id,
                add_offset: 0,
                limit: 100,
                max_id: 0,
                min_id: 0,
                hash: 0,
            };
            let page = messages_of(s.patient(&NEVER, || s.client.invoke(&request)).await?);
            let n = page.len();
            let Some(last) = page.last().map(|m| m.id()) else { break };
            let date = |m: &tl::enums::Message| match m {
                tl::enums::Message::Message(m) => m.date as i64,
                tl::enums::Message::Service(m) => m.date as i64,
                tl::enums::Message::Empty(_) => i64::MAX,
            };
            let past = page.iter().any(|m| date(m) < from);
            out.extend(page.into_iter().filter(|m| date(m) >= from));
            if n < 100 || last == offset_id || past {
                break;
            }
            offset_id = last;
        }
        Ok(out)
    }

    /// The place of the file of message `id` of `peer`, asked of Telegram; `cancel` ends a wait
    /// Telegram asks for. In a channel where pages pay (see `places`), the page of history
    /// around the message is asked for and the places of the files there are kept: one download
    /// of the channel asks at a time, so the others find theirs came with its answer. Anywhere
    /// else the message alone is asked for, each download on its own.
    async fn find(&self, peer: PeerRef, id: i32, cancel: &AtomicBool) -> Result<Place> {
        let s = &self.shared;
        if peer.id.kind() == PeerKind::Channel {
            let turn = self.finding.lock().unwrap_or_else(|e| e.into_inner()).entry(peer.id).or_default().clone();
            let _turn = turn.lock().await;
            if let Some(place) = self.places.get(&(peer.id, id)) {
                self.paging.served(peer.id);
                return Ok(place);
            }
            if self.paging.asks_page(peer.id) {
                PLACE_ASKS.fetch_add(1, Ordering::Relaxed);
                let request = tl::functions::messages::GetHistory { peer: peer.into(), offset_id: places::page_from(id), offset_date: 0, add_offset: 0, limit: places::PAGE, max_id: 0, min_id: 0, hash: 0 };
                let page = messages_of(s.patient(cancel, || s.client.invoke(&request)).await?);
                // Kept before the turn passes on: the next download looks there first.
                return self.keep(peer, id, &page);
            }
        }
        PLACE_ASKS.fetch_add(1, Ordering::Relaxed);
        let ids = [id];
        let alone: Vec<tl::enums::Message> = s.patient(cancel, || s.client.get_messages_by_id(peer, &ids)).await?.into_iter().flatten().map(|m| m.raw).collect();
        self.keep(peer, id, &alone)
    }

    /// Keeps the places of the files of `messages`, of `peer`; that of message `id` among them.
    fn keep(&self, peer: PeerRef, id: i32, messages: &[tl::enums::Message]) -> Result<Place> {
        let mut found = None;
        for m in messages {
            let tl::enums::Message::Message(m) = m else { continue };
            let place = m.media.clone().and_then(Media::from_raw).and_then(|media| Some(Place { location: media.to_raw_input_location()?, dc: media.dc_id()?, size: media.size().unwrap_or(0) as u64 }));
            if m.id == id {
                found = Some(place.clone().ok_or_else(|| t("A mensagem não tem arquivo, ou o Telegram não disse onde ele está.", "The message has no file, or Telegram didn't say where it is.").to_string()));
            }
            if let Some(place) = place {
                self.places.put((peer.id, m.id), place);
            }
        }
        found.unwrap_or_else(|| Err(t("O arquivo não existe mais no Telegram.", "The file doesn't exist on Telegram anymore.").into()))
    }

    /// [`Real::find`], with `progress` asked meanwhile whether to go on: Cancelar works while a
    /// download waits its turn to ask, or waits out a wait Telegram asked for.
    async fn find_watched(&self, peer: PeerRef, id: i32, progress: Progress<'_>) -> Result<Place> {
        let cancel = AtomicBool::new(false);
        let find = self.find(peer, id, &cancel);
        tokio::pin!(find);
        loop {
            tokio::select! {
                found = &mut find => return found,
                () = tokio::time::sleep(Duration::from_millis(250)) => {
                    if !progress(0) {
                        return Err(CANCELLED.to_string());
                    }
                }
            }
        }
    }

    /// Writes the file of message `id` of `peer` to `out`: from the place kept for it, or asked
    /// now; a kept one that Telegram no longer takes is asked anew, once.
    fn download_in(&self, peer: PeerRef, id: i32, out: &mut dyn Write, progress: Progress) -> Result<()> {
        self.block(async {
            let key = (peer.id, id);
            let kept = self.places.get(&key);
            let place = match &kept {
                Some(place) => {
                    self.paging.served(peer.id);
                    place.clone()
                }
                None => self.find_watched(peer, id, &mut *progress).await?,
            };
            let mut written = 0;
            let mut result = self.bring(&place, &mut *out, &mut *progress, &mut written).await;
            if places::asks_again(kept.is_some(), result.as_ref().is_err_and(|e| e == STALE), written) {
                self.places.forget(&key);
                let place = self.find_watched(peer, id, &mut *progress).await?;
                result = self.bring(&place, out, progress, &mut written).await;
            }
            result.map_err(|e| if e == STALE { t("O Telegram deixou de aceitar o endereço deste arquivo no meio do download. Tente de novo.", "Telegram stopped taking this file's address halfway through the download. Try again.").into() } else { e })
        })
    }

    /// Writes the file at `place` to `out`, its parts fetched several at a time and written in
    /// order; `written` counts the bytes that reached `out`.
    async fn bring(&self, place: &Place, out: &mut dyn Write, progress: Progress<'_>, written: &mut u64) -> Result<()> {
        let (location, size, dc) = (place.location.clone(), place.size, place.dc);
        let count = size.div_ceil(PART as u64) as usize;
        // Each part waits its turn in the account's budget; no more than twice that many
        // parts wait in memory to be written in order.
        let at_once = self.shared.budget.size();
        let window = Arc::new(Semaphore::new(at_once * 2));
        let next = Arc::new(AtomicUsize::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..at_once.min(count.max(1)) {
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
        let mut at = 0usize;
        let failed = 'run: loop {
            match tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
                Ok(Some((i, bytes, held))) => {
                    waiting.insert(i, (bytes, held));
                    while let Some((bytes, _held)) = waiting.remove(&at) {
                        if let Err(e) = out.write_all(&bytes) {
                            break 'run Some(tr!("Não consegui gravar o arquivo: {e}", "Couldn't write the file: {e}"));
                        }
                        *written += bytes.len() as u64;
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
            if !progress(*written) {
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
        if *written != size {
            let written = *written;
            return Err(tr!("O download parou em {written} de {size} bytes.", "The download stopped at {written} of {size} bytes."));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(id: i32, ip: &str, flags: &[&str]) -> tl::enums::DcOption {
        let has = |f: &str| flags.contains(&f);
        tl::types::DcOption { ipv6: has("ipv6"), media_only: has("media_only"), tcpo_only: has("tcpo_only"), cdn: has("cdn"), r#static: has("static"), this_port_only: false, id, ip_address: ip.into(), port: 443, secret: None }.into()
    }

    #[test]
    fn a_dcs_media_address_is_the_plain_ipv4_one_kept_for_files() {
        // As Telegram lists them: each DC's own address, the one for proxies, the IPv6 ones and
        // the media ones; a CDN's and one that takes only obfuscated connections among them.
        let options = [
            option(1, "149.154.175.51", &[]),
            option(1, "149.154.175.55", &["static"]),
            option(1, "2001:b28:f23d:f001::a", &["ipv6"]),
            option(1, "2001:b28:f23d:f001::7", &["ipv6", "media_only"]),
            option(1, "149.154.175.211", &["media_only"]),
            option(2, "149.154.162.1", &["media_only", "tcpo_only"]),
            option(2, "149.154.162.123", &["media_only"]),
            option(3, "149.154.175.100", &[]),
            option(4, "149.154.167.9", &["media_only", "cdn"]),
            option(5, "not an address", &["media_only"]),
        ];
        let media = media_addresses(&options);
        assert_eq!(media.get(&1), Some(&"149.154.175.211:443".parse().unwrap()));
        assert_eq!(media.get(&2), Some(&"149.154.162.123:443".parse().unwrap()));
        // No media address: its downloads go over the connections to its own.
        assert_eq!((media.get(&3), media.get(&4), media.get(&5)), (None, None, None));
    }

    #[test]
    fn an_extra_connection_that_ended_is_dropped_and_one_only_cut_is_not() {
        // Cut: it leaves the round and is probed (`lanes`), for it may answer again.
        assert!(!broken(&InvocationError::Io(std::io::Error::from(std::io::ErrorKind::ConnectionReset))));
        assert!(broken(&InvocationError::Dropped("gone")));
    }
}
