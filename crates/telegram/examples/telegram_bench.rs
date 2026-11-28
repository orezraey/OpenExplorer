//! Transfer speeds on a real Telegram account, with no app on screen: the first account in the
//! app's list (`%APPDATA%\OpenExplorer\telegram-accounts.json`), over its session in the credential
//! store, read and never written. Everything it sends goes in a folder of its own in the storage
//! channel, deleted at the end.
//!
//! ```text
//! cargo run --release -p openexplorer-telegram --example telegram_bench -- roundtrip [options]
//!   --big <MiB>        size of the big file (300); 0 skips it
//!   --small <n>        how many small files (0: none)
//!   --small-kib <KiB>  size of each small file (16)
//!   --at-once <n>      small files moving at once, as the app's queue does (4)
//!   --rounds <n>       rounds, the variants interleaved in each (1)
//!   --variants <list>  connections x parts in flight, e.g. 4x16,8x32 (the account's own setting);
//!                      4x16m0 for no connection to the media address, 4x16m2 for two
//! ```
//!
//! It prints each measurement and, at the end, the median with the lowest and highest of each
//! variant, in MiB/s for the big file and files/s for the small ones.
//!
//! Go easy with it. Telegram remembers what an account asked for: after some 2,500 small files
//! sent in three hours of rounds it refused to take a message for 48 minutes, and after runs
//! with 64 parts in flight, or two of these at once, it closed connections and slowed the
//! account for minutes. So small files and rounds are off unless asked for, and what compares
//! two builds or two settings is a file that stays, downloaded with a pause between runs:
//!
//! ```text
//! telegram_bench put [--mib 300]                              sends "/OE bench keep/big.bin" once
//! telegram_bench get [--rounds 3] [--pause 0] [--tune 4x16m0] downloads it, checked, each round timed
//! telegram_bench clean                                        deletes the folder
//! telegram_bench put-small [--n 50] [--kib 16]                sends "/OE bench keep/small" once
//! telegram_bench get-small [--n 100] [--max-kib 256] [--at-once 4] [--dir <folder>] [--chat <k>]
//!                                                             downloads small files that are
//!                                                             there already (nothing is sent):
//!                                                             of the storage, of a folder, or
//!                                                             of the k-th month of a chat that
//!                                                             has that many (messages far apart);
//!                                                             prints counts and times only
//! telegram_bench chats [--n 5] [--max-mib 40]                 downloads a file from each of the
//!                                                             first chats that have one (other
//!                                                             DCs than the account's, often),
//!                                                             printing sizes and times only
//! telegram_bench probe                                        round-trip time, the DCs' addresses, Premium
//! ```

use std::io::Write;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use grammers_session::types::{PeerAuth, PeerId, PeerRef};
use grammers_tl_types as tl;
use openexplorer_telegram::lanes::Tuning;
use openexplorer_telegram::real::{Conn, DROPPED_CALLS, EVENTS, PLACE_ASKS, PREMIUM_WAITS, REFUSALS, Real, WAITED_SECS, WAITS};
use openexplorer_telegram::session::{self, KeyringSession};
use openexplorer_telegram::{Account, Found, Store, StoredAccount};

/// Bytes that don't repeat, the same for the same `n` and `seed`.
fn random(n: usize, seed: u64) -> Vec<u8> {
    let mut b = vec![0u8; n];
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15 ^ n as u64 ^ seed.wrapping_mul(0x2545_f491_4f6c_dd1d);
    for chunk in b.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let bytes = x.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    b
}

/// A place to download to that keeps nothing: each write is compared with what was sent.
struct Check<'a> {
    want: &'a [u8],
    at: usize,
    same: bool,
}

impl Write for Check<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let end = self.at + buf.len();
        self.same &= self.want.get(self.at..end) == Some(buf);
        self.at = end;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn arg<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// The first account of the app's list and a connection over its session.
fn connect() -> Result<(Conn, StoredAccount), String> {
    let dir = std::path::PathBuf::from(std::env::var_os("APPDATA").ok_or("APPDATA")?).join("OpenExplorer");
    let stored = Store::at(dir.join("telegram-accounts.json")).accounts().into_iter().next().ok_or("No Telegram account connected: connect one in the app, or run the `conta` example.")?;
    let (api_id, _) = session::api(stored.user_id)?;
    let bytes = keyring::Entry::new("OpenExplorer Telegram", &stored.user_id.to_string()).and_then(|e| e.get_secret()).map_err(|e| e.to_string())?;
    // From bytes: this run saves nothing to the credential store, where the app's session is.
    let session = Arc::new(KeyringSession::from_bytes(&bytes).ok_or("the kept session doesn't read")?);
    Ok((Conn::start(api_id, session)?, stored))
}

fn open() -> Result<(Account, bool), String> {
    let (conn, stored) = connect()?;
    let channel = PeerRef { id: PeerId::channel(stored.channel_id).ok_or("channel id")?, auth: PeerAuth::from_hash(stored.channel_hash) };
    Ok((Account::open(stored.user_id, "bench", Box::new(Real::new(conn, channel, stored.premium)), None), stored.premium))
}

/// What Telegram asked or refused so far, to tell what one step cost.
#[derive(Clone, Copy)]
struct Counts {
    waits: usize,
    secs: usize,
    premium: usize,
    refusals: usize,
    again: usize,
    at: Instant,
}

impl Counts {
    fn now() -> Counts {
        Counts { waits: WAITS.load(Relaxed), secs: WAITED_SECS.load(Relaxed), premium: PREMIUM_WAITS.load(Relaxed), refusals: REFUSALS.load(Relaxed), again: DROPPED_CALLS.load(Relaxed), at: Instant::now() }
    }

    /// What happened since `self` was taken: nothing said when nothing did.
    fn since(&self) -> String {
        let n = Counts::now();
        let mut out = String::new();
        if n.waits > self.waits || n.refusals > self.refusals || n.again > self.again {
            out = format!("; waits {} ({} s, {} of them FLOOD_PREMIUM_WAIT), -429 refusals {}, requests asked again {}", n.waits - self.waits, n.secs - self.secs, n.premium - self.premium, n.refusals - self.refusals, n.again - self.again);
        }
        for (at, event) in EVENTS.lock().unwrap().iter().filter(|(at, _)| *at >= self.at) {
            out.push_str(&format!("\n    {:>7.2} s  {event}", at.duration_since(self.at).as_secs_f64()));
        }
        out
    }
}

fn median(v: &[f64]) -> String {
    if v.is_empty() {
        return "-".into();
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    format!("{:.1} ({:.1}-{:.1})", s[s.len() / 2], s[0], s[s.len() - 1])
}

/// Runs `job` over `items`, `at_once` threads taking them in turn; the first error, if any.
fn each<T: Send>(items: Vec<T>, at_once: usize, job: impl Fn(T) -> Result<(), String> + Sync) -> Result<(), String> {
    let queue = Mutex::new(items);
    let failed = Mutex::new(None);
    std::thread::scope(|s| {
        for _ in 0..at_once.max(1) {
            s.spawn(|| {
                loop {
                    let Some(item) = queue.lock().unwrap().pop() else { break };
                    if let Err(e) = job(item) {
                        failed.lock().unwrap().get_or_insert(e);
                        break;
                    }
                }
            });
        }
    });
    failed.into_inner().unwrap().map_or(Ok(()), Err)
}

/// A small file to send: where to, its bytes and its modified time.
type SmallFile = (String, Vec<u8>, Option<i64>);

#[derive(Default, Clone)]
struct Speeds {
    up: Vec<f64>,
    down: Vec<f64>,
    small_up: Vec<f64>,
    small_down: Vec<f64>,
}

fn roundtrip(args: &[String]) -> Result<(), String> {
    let (big_mib, small_n, small_kib): (usize, usize, usize) = (arg(args, "--big", 300), arg(args, "--small", 0), arg(args, "--small-kib", 16));
    let (at_once, rounds): (usize, usize) = (arg(args, "--at-once", 4), arg(args, "--rounds", 1));
    let variants: Vec<Option<Tuning>> = match args.iter().position(|a| a == "--variants").and_then(|i| args.get(i + 1)) {
        Some(list) => list.split(',').map(|v| v.parse().map(Some)).collect::<Result<_, String>>()?,
        None => vec![None],
    };
    let (a, premium) = open()?;
    let top = format!("/OE bench {}", std::process::id());
    let t = Instant::now();
    a.mkdir(&top)?;
    println!("account open (premium: {premium}), tree read and folder made in {:.1} s", t.elapsed().as_secs_f64());
    let big = random(big_mib << 20, 1);
    let small: Vec<Vec<u8>> = (0..small_n).map(|i| random(small_kib << 10, 100 + i as u64)).collect();
    let mut speeds = vec![Speeds::default(); variants.len()];
    let run = |speeds: &mut Vec<Speeds>| -> Result<(), String> {
        for round in 1..=rounds {
            for (v, variant) in variants.iter().enumerate() {
                let label = variant.map_or("default".to_string(), |t| t.to_string());
                if let Some(tuning) = *variant {
                    a.tune(tuning);
                }
                if !big.is_empty() {
                    let path = format!("{top}/big {round} {v}.bin");
                    let (t, before) = (Instant::now(), Counts::now());
                    a.upload(&path, &mut &big[..], big.len() as u64, None, &mut |_| true)?;
                    let up = big_mib as f64 / t.elapsed().as_secs_f64();
                    let sent = before.since();
                    let mut check = Check { want: &big, at: 0, same: true };
                    let (t, before) = (Instant::now(), Counts::now());
                    a.download(&path, &mut check, &mut |_| true)?;
                    let down = big_mib as f64 / t.elapsed().as_secs_f64();
                    let got = before.since();
                    if !check.same || check.at != big.len() {
                        return Err(format!("{path} came back different ({} of {} bytes)", check.at, big.len()));
                    }
                    a.remove(&path, false)?;
                    println!("round {round}, {label}: {big_mib} MiB up {up:.1} MiB/s{sent}");
                    println!("round {round}, {label}: {big_mib} MiB down {down:.1} MiB/s; connections per DC {:?}{got}", a.connections_open());
                    speeds[v].up.push(up);
                    speeds[v].down.push(down);
                }
                if !small.is_empty() {
                    let dir = format!("{top}/small {round} {v}");
                    a.mkdir(&dir)?;
                    // As the app sends them: in groups of ten, `at_once` groups at a time.
                    let groups: Vec<Vec<SmallFile>> = small.chunks(10).enumerate().map(|(g, c)| c.iter().enumerate().map(|(i, b)| (format!("{dir}/s{:04}.bin", g * 10 + i), b.clone(), None)).collect()).collect();
                    let (t, before) = (Instant::now(), Counts::now());
                    each(groups, at_once, |g| a.upload_small(g).into_iter().collect::<Result<Vec<()>, String>>().map(|_| ()))?;
                    let up = small_n as f64 / t.elapsed().as_secs_f64();
                    let sent = before.since();
                    let (t, before) = (Instant::now(), Counts::now());
                    each((0..small_n).collect(), at_once, |i| {
                        let mut check = Check { want: &small[i], at: 0, same: true };
                        a.download(&format!("{dir}/s{i:04}.bin"), &mut check, &mut |_| true)?;
                        if check.same && check.at == small[i].len() { Ok(()) } else { Err(format!("small file {i} came back different")) }
                    })?;
                    let down = small_n as f64 / t.elapsed().as_secs_f64();
                    a.remove(&dir, true)?;
                    let got = before.since();
                    println!("round {round}, {label}: {small_n} files of {small_kib} KiB, {at_once} at once: up {up:.1} files/s{sent}");
                    println!("round {round}, {label}: {small_n} files of {small_kib} KiB, {at_once} at once: down {down:.1} files/s{got}");
                    speeds[v].small_up.push(up);
                    speeds[v].small_down.push(down);
                }
            }
        }
        Ok(())
    };
    let result = run(&mut speeds);
    let cleaned = a.remove(&top, true);
    println!("\nmedian (lowest-highest) of {rounds} rounds:");
    for (v, variant) in variants.iter().enumerate() {
        let label = variant.map_or("default".to_string(), |t| t.to_string());
        let s = &speeds[v];
        println!("  {label}: big up {} MiB/s, big down {} MiB/s, small up {} files/s, small down {} files/s", median(&s.up), median(&s.down), median(&s.small_up), median(&s.small_down));
    }
    println!("waits asked: {} ({} s; FLOOD_PREMIUM_WAIT: {}); -429 refusals: {}; requests asked again: {}", WAITS.load(Relaxed), WAITED_SECS.load(Relaxed), PREMIUM_WAITS.load(Relaxed), REFUSALS.load(Relaxed), DROPPED_CALLS.load(Relaxed));
    result.and(cleaned)
}

/// The folder and file `put` leaves for `get`.
const KEEP: &str = "/OE bench keep";

fn keep_file() -> String {
    format!("{KEEP}/big.bin")
}

fn tuned(a: &Account, args: &[String]) -> Result<String, String> {
    match args.iter().position(|a| a == "--tune").and_then(|i| args.get(i + 1)) {
        Some(v) => {
            let tuning: Tuning = v.parse()?;
            a.tune(tuning);
            Ok(tuning.to_string())
        }
        None => Ok("default".into()),
    }
}

fn put(args: &[String]) -> Result<(), String> {
    let mib: usize = arg(args, "--mib", 300);
    let (a, _) = open()?;
    let label = tuned(&a, args)?;
    if a.stat(KEEP)?.is_none() {
        a.mkdir(KEEP)?;
    }
    let big = random(mib << 20, 1);
    let (t, before) = (Instant::now(), Counts::now());
    a.upload(&keep_file(), &mut &big[..], big.len() as u64, None, &mut |_| true)?;
    println!("{label}: {mib} MiB up {:.1} MiB/s; connections per DC {:?}{}", mib as f64 / t.elapsed().as_secs_f64(), a.connections_open(), before.since());
    Ok(())
}

fn get(args: &[String]) -> Result<(), String> {
    let (rounds, pause): (usize, u64) = (arg(args, "--rounds", 3), arg(args, "--pause", 0));
    let (a, _) = open()?;
    let label = tuned(&a, args)?;
    let Some(Found::Item(item)) = a.stat(&keep_file())? else { return Err("no file to get: run `put` first".into()) };
    let big = random(item.size as usize, 1);
    let mib = item.size as f64 / 1048576.0;
    let mut speeds = Vec::new();
    for round in 1..=rounds {
        std::thread::sleep(std::time::Duration::from_secs(if round > 1 { pause } else { 0 }));
        let mut check = Check { want: &big, at: 0, same: true };
        let (t, before) = (Instant::now(), Counts::now());
        a.download(&keep_file(), &mut check, &mut |_| true)?;
        let speed = mib / t.elapsed().as_secs_f64();
        if !check.same || check.at != big.len() {
            return Err(format!("the file came back different ({} of {} bytes)", check.at, big.len()));
        }
        println!("round {round}, {label}: {mib:.0} MiB down {speed:.1} MiB/s; connections per DC {:?}{}", a.connections_open(), before.since());
        speeds.push(speed);
    }
    println!("{label}: down {} MiB/s, median (lowest-highest) of {rounds}", median(&speeds));
    Ok(())
}

/// A place to download to that only counts.
struct Count(u64);

impl Write for Count {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The folder of the `k`-th month of a chat with at least `n` files of at most `max` bytes,
/// among the newest months of the first chats.
fn chat_month(a: &Account, k: usize, n: usize, max: u64) -> Result<String, String> {
    let top = format!("/{}", openexplorer_telegram::chats::folder_name());
    let mut seen = 0;
    for chat in a.list(&top)?.iter().take(80) {
        let Ok(months) = a.list(&format!("{top}/{}", chat.name)) else { continue };
        for month in months.iter().take(3) {
            let dir = format!("{top}/{}/{}", chat.name, month.name);
            let Ok(files) = a.list(&dir) else { continue };
            if files.iter().filter(|f| f.size > 0 && f.size <= max).count() >= n {
                if seen == k {
                    return Ok(dir);
                }
                seen += 1;
            }
        }
    }
    Err(format!("only {seen} months of chats have {n} files that small"))
}

/// Sends `--n` files of `--kib` to a folder that stays, for `get-small --dir "/OE bench keep/small"`.
fn put_small(args: &[String]) -> Result<(), String> {
    let (n, kib): (usize, usize) = (arg(args, "--n", 50), arg(args, "--kib", 16));
    let (a, _) = open()?;
    let dir = format!("{KEEP}/small");
    for d in [KEEP, dir.as_str()] {
        if a.stat(d)?.is_none() {
            a.mkdir(d)?;
        }
    }
    let files: Vec<SmallFile> = (0..n).map(|i| (format!("{dir}/s{i:04}.bin"), random(kib << 10, 100 + i as u64), None)).collect();
    let (t, before) = (Instant::now(), Counts::now());
    a.upload_small(files).into_iter().collect::<Result<Vec<()>, String>>()?;
    println!("{n} files of {kib} KiB, one group at a time: up {:.1} files/s{}", n as f64 / t.elapsed().as_secs_f64(), before.since());
    Ok(())
}

/// Downloads up to `--n` files of at most `--max-kib` that are there already, `--at-once` at a
/// time: of the storage, folder by folder in the order listed; of `--dir` and the folders in
/// it; or of a month of a chat (`--chat k`), where the messages are far apart. Reads only; no
/// name is printed.
fn get_small(args: &[String]) -> Result<(), String> {
    let (n, max_kib, at_once): (usize, u64, usize) = (arg(args, "--n", 100), arg(args, "--max-kib", 256), arg(args, "--at-once", 4));
    let (a, _) = open()?;
    let start = match (args.iter().position(|a| a == "--dir").and_then(|i| args.get(i + 1)), args.iter().position(|a| a == "--chat").and_then(|i| args.get(i + 1))) {
        (Some(dir), _) => dir.clone(),
        (None, Some(k)) => chat_month(&a, k.parse().map_err(|_| "--chat takes a number")?, n, max_kib << 10)?,
        (None, None) => "/".to_string(),
    };
    let asks = PLACE_ASKS.load(Relaxed);
    let mut files: Vec<(String, u64)> = Vec::new();
    let mut folders = vec![start.clone()];
    while let Some(dir) = folders.pop() {
        if files.len() >= n {
            break;
        }
        for i in a.list(&dir)? {
            let path = format!("{}/{}", dir.trim_end_matches('/'), i.name);
            if i.is_dir() {
                if !a.passed_over(&path) && (start != "/" || !i.name.starts_with("OE bench")) {
                    folders.push(path);
                }
            } else if i.size > 0 && i.size <= max_kib << 10 && i.parts.is_empty() && !i.incomplete && files.len() < n {
                files.push((path, i.size));
            }
        }
    }
    if files.is_empty() {
        return Err("the storage has no file that small".into());
    }
    let (count, bytes) = (files.len(), files.iter().map(|f| f.1).sum::<u64>());
    let (t, before) = (Instant::now(), Counts::now());
    each(files, at_once, |(path, size)| {
        let mut got = Count(0);
        a.download(&path, &mut got, &mut |_| true)?;
        if got.0 == size { Ok(()) } else { Err(format!("a file of {size} bytes came as {}", got.0)) }
    })?;
    let secs = t.elapsed().as_secs_f64();
    println!("{count} files, {} KiB in all, {at_once} at once: down {:.1} files/s ({secs:.1} s), {} requests to find them{}", bytes >> 10, count as f64 / secs, PLACE_ASKS.load(Relaxed) - asks, before.since());
    Ok(())
}

/// Downloads the biggest file of at most `--max-mib` from the newest month of each of the first
/// `--n` chats that have one: files of other people are often in other DCs than the account's.
/// Reads only; no name is printed.
fn chats(args: &[String]) -> Result<(), String> {
    let (n, max_mib): (usize, u64) = (arg(args, "--n", 5), arg(args, "--max-mib", 40));
    let (a, _) = open()?;
    let top = format!("/{}", openexplorer_telegram::chats::folder_name());
    let mut done = 0;
    for (i, chat) in a.list(&top)?.iter().enumerate().take(60) {
        let Ok(months) = a.list(&format!("{top}/{}", chat.name)) else { continue };
        let Some(month) = months.first() else { continue };
        let Ok(files) = a.list(&format!("{top}/{}/{}", chat.name, month.name)) else { continue };
        let Some(file) = files.iter().filter(|f| f.size > 0 && f.size <= max_mib << 20).max_by_key(|f| f.size) else { continue };
        let path = format!("{top}/{}/{}/{}", chat.name, month.name, file.name);
        let mut got = Count(0);
        let (t, before) = (Instant::now(), Counts::now());
        match a.download(&path, &mut got, &mut |_| true) {
            Ok(()) if got.0 == file.size => println!("chat {i}: {:.1} MiB in {:.1} s = {:.1} MiB/s; connections per DC {:?}{}", file.size as f64 / 1048576.0, t.elapsed().as_secs_f64(), file.size as f64 / 1048576.0 / t.elapsed().as_secs_f64(), a.connections_open(), before.since()),
            Ok(()) => return Err(format!("chat {i}: a file of {} bytes came as {}", file.size, got.0)),
            Err(e) => return Err(format!("chat {i}: {e}")),
        }
        done += 1;
        if done == n {
            break;
        }
    }
    if done == 0 { Err("no chat with a file that size".into()) } else { Ok(()) }
}

fn clean() -> Result<(), String> {
    let (a, _) = open()?;
    if a.stat(KEEP)?.is_some() {
        a.remove(KEEP, true)?;
    }
    Ok(())
}

/// How long a request takes to come back, where the DCs are, and what the account is now.
fn probe() -> Result<(), String> {
    let (conn, stored) = connect()?;
    let ask = || conn.rt.block_on(conn.client.invoke(&tl::functions::help::GetNearestDc {})).map_err(|e| e.to_string());
    let tl::enums::NearestDc::Dc(near) = ask()?;
    let mut times: Vec<f64> = (0..10)
        .map(|_| {
            let t = Instant::now();
            ask().map(|_| t.elapsed().as_secs_f64() * 1000.0)
        })
        .collect::<Result<_, _>>()?;
    times.sort_by(f64::total_cmp);
    println!("this DC {}, nearest DC {}; a request's round trip: median {:.0} ms ({:.0}-{:.0})", near.this_dc, near.nearest_dc, times[5], times[0], times[9]);
    let users = conn.rt.block_on(conn.client.invoke(&tl::functions::users::GetUsers { id: vec![tl::enums::InputUser::UserSelf] })).map_err(|e| e.to_string())?;
    let premium = users.iter().any(|u| matches!(u, tl::enums::User::User(u) if u.premium));
    println!("Premium now: {premium}; in the app's list: {}", stored.premium);
    let tl::enums::Config::Config(config) = conn.rt.block_on(conn.client.invoke(&tl::functions::help::GetConfig {})).map_err(|e| e.to_string())?;
    for tl::enums::DcOption::Option(o) in &config.dc_options {
        let flags: Vec<&str> = [(o.ipv6, "ipv6"), (o.media_only, "media_only"), (o.tcpo_only, "tcpo_only"), (o.cdn, "cdn"), (o.r#static, "static")].into_iter().filter(|f| f.0).map(|f| f.1).collect();
        println!("  DC {} {}:{} {}", o.id, o.ip_address, o.port, flags.join(" "));
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("roundtrip") => roundtrip(&args[1..]),
        Some("put") => put(&args[1..]),
        Some("get") => get(&args[1..]),
        Some("clean") => clean(),
        Some("get-small") => get_small(&args[1..]),
        Some("put-small") => put_small(&args[1..]),
        Some("chats") => chats(&args[1..]),
        Some("probe") => probe(),
        _ => Err("Usage: telegram_bench roundtrip|put|get|put-small|get-small|chats|clean|probe (see the file's head for the options)".into()),
    };
    if let Err(e) = result {
        eprintln!("ERROR: {e}");
        std::process::exit(1);
    }
}
