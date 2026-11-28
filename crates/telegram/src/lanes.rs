//! The connections to a DC that carry file parts, and which one a part goes over.
//!
//! A DC has its own address, where the account's connection is, and most have another kept for
//! files ("media only"), which is where Telegram's own apps download from (TDLib, Android,
//! Desktop). A download goes over a connection to the media address once one answers; an upload,
//! or a download from a DC with no media address, is shared out among the connections to the
//! DC's own address, the account's included. What `real` adds is the network: here is only who
//! carries what, so the tests can play Telegram.
//!
//! Measured on a real Premium account (`examples/telegram_bench.rs`, a 300 MiB file, home DC 1,
//! rounds after the first): four connections to the DC's own address brought 27 to 37 MiB/s, one
//! to the media address 41 to 51. More than one there bought nothing that lasted: two did 27 to
//! 31, eight did 46 to 54 in one run and in every run after it Telegram closed them seconds
//! after they opened (3 to 9 MiB/s). Uploads are the other way around: one connection carries
//! some 7 MiB/s whatever the address, four fill a 31 MiB/s line.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// A connection as the lanes need it: one that can be ended.
pub trait Link: Send + Sync {
    fn quit(&self);
}

/// Which address of the DC a connection goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// The DC's own: where the account's connection is.
    General,
    /// The one kept for files.
    Media,
}

/// One connection to a DC that file parts go over.
pub struct Lane<L> {
    pub link: L,
    /// The requests on it now.
    pub busy: AtomicUsize,
    /// The account's own connection: never dropped.
    pub main: bool,
    pub kind: Kind,
    /// It answered once: connected. Until then it takes one request at a time (which connects
    /// it), so parts don't wait behind a connection still being made.
    ready: AtomicBool,
    born: Instant,
}

impl<L> Lane<L> {
    /// It answered: from now on it takes its share.
    pub fn answered(&self) {
        self.ready.store(true, Ordering::SeqCst);
    }

    /// Whether it answered already.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    pub fn age(&self) -> Duration {
        self.born.elapsed()
    }

    fn free_to_start(&self) -> bool {
        self.ready.load(Ordering::SeqCst) || self.busy.load(Ordering::SeqCst) == 0
    }
}

/// How many connections a DC gets, and how many file parts may be in flight for the account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tuning {
    /// Connections to the DC's own address, the account's own among them.
    pub general: usize,
    /// Connections to the DC's media address, for downloads.
    pub media: usize,
    /// File-part requests in flight for the account, both ways together.
    pub parts: usize,
}

/// `4x16`: 4 connections to the DC's own address, 16 parts in flight, and the one to its media
/// address; `4x16m0` for none there, `4x32m8` for eight.
impl std::str::FromStr for Tuning {
    type Err = String;

    fn from_str(s: &str) -> Result<Tuning, String> {
        let bad = || format!("{s}: connections x parts, as in 4x16 or 4x16m1");
        let (general, rest) = s.split_once('x').ok_or_else(bad)?;
        let (parts, media) = rest.split_once('m').unwrap_or((rest, "1"));
        Ok(Tuning { general: general.parse().map_err(|_| bad())?, media: media.parse().map_err(|_| bad())?, parts: parts.parse().map_err(|_| bad())? })
    }
}

impl std::fmt::Display for Tuning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}m{}", self.general, self.parts, self.media)
    }
}

struct State<L> {
    lanes: HashMap<(i32, Kind), Vec<Arc<Lane<L>>>>,
    /// Per DC and address, the most connections it takes (one fewer each time one broke) and
    /// until when.
    ceiling: HashMap<(i32, Kind), (usize, Instant)>,
}

/// How many times longer a DC goes without a connection that never answered than without one
/// that broke after working: an address that can't be reached from this network (the media one
/// may be closed where the DC's own is open) isn't tried again and again.
pub const NEVER_ANSWERED: u32 = 10;

pub struct Lanes<L> {
    main: L,
    state: Mutex<State<L>>,
    general: AtomicUsize,
    media: AtomicUsize,
    /// How long a DC keeps the fewer connections it was left with when one broke: a short
    /// network drop breaks them all, and the speed should come back after it.
    ceiling_for: Duration,
}

impl<L: Link + Clone> Lanes<L> {
    /// The lanes of an account whose own connection is `main`.
    pub fn new(main: L, tuning: Tuning, ceiling_for: Duration) -> Lanes<L> {
        Lanes {
            main,
            state: Mutex::new(State { lanes: HashMap::new(), ceiling: HashMap::new() }),
            general: AtomicUsize::new(tuning.general.max(1)),
            media: AtomicUsize::new(tuning.media),
            ceiling_for,
        }
    }

    fn state(&self) -> MutexGuard<'_, State<L>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lane(&self, link: L, kind: Kind, main: bool) -> Arc<Lane<L>> {
        Arc::new(Lane { link, busy: AtomicUsize::new(0), main, kind, ready: AtomicBool::new(main), born: Instant::now() })
    }

    /// How many connections `dc` should have to its `kind` of address now.
    pub fn wanted(&self, dc: i32, kind: Kind) -> usize {
        let ceiling = self.state().ceiling.get(&(dc, kind)).filter(|(_, until)| Instant::now() < *until).map_or(usize::MAX, |(n, _)| *n);
        match kind {
            Kind::General => self.general.load(Ordering::SeqCst).min(ceiling).max(1),
            Kind::Media => self.media.load(Ordering::SeqCst).min(ceiling),
        }
    }

    /// How many connections `dc` has to its `kind` of address, the account's own counted.
    pub fn count(&self, dc: i32, kind: Kind) -> usize {
        let open = self.state().lanes.get(&(dc, kind)).map_or(0, Vec::len);
        if kind == Kind::General { open.max(1) } else { open }
    }

    /// Whether a download from `dc` may go over a connection to its media address now: when
    /// not, that address needn't be known.
    pub fn takes_media(&self, dc: i32) -> bool {
        self.wanted(dc, Kind::Media) > 0
    }

    /// The kind of connection to open for a file part to `dc` now, if one is missing. A
    /// download (`down`) from a DC with a media address (`has_media`) gets the connection there
    /// and no other: while that one is being made its parts go over the account's own. The
    /// others to the DC's own address open for an upload, for a DC with no media address, or
    /// when the media one broke.
    pub fn to_open(&self, dc: i32, down: bool, has_media: bool) -> Option<Kind> {
        let kind = if down && has_media && self.takes_media(dc) { Kind::Media } else { Kind::General };
        (self.count(dc, kind) < self.wanted(dc, kind)).then_some(kind)
    }

    /// Opens connections to `dc`'s `kind` of address up to the number wanted, each made by
    /// `open`; how many it opened.
    pub fn grow(&self, dc: i32, kind: Kind, mut open: impl FnMut() -> L) -> usize {
        let want = self.wanted(dc, kind);
        let mut st = self.state();
        let lanes = st.lanes.entry((dc, kind)).or_insert_with(|| (kind == Kind::General).then(|| self.lane(self.main.clone(), kind, true)).into_iter().collect());
        let before = lanes.len();
        while lanes.len() < want {
            lanes.push(self.lane(open(), kind, false));
        }
        lanes.len() - before
    }

    /// The connection a file part to `dc` goes over. A download (`media`) takes a connection to
    /// the media address when one answered already, or one still to be made that has nothing on
    /// it yet; anything else, and a download meanwhile, the least busy connection to the DC's
    /// own address.
    pub fn pick(&self, dc: i32, media: bool) -> Arc<Lane<L>> {
        let mut st = self.state();
        if media && let Some(lane) = st.lanes.get(&(dc, Kind::Media)).and_then(|l| l.iter().filter(|l| l.free_to_start()).min_by_key(|l| l.busy.load(Ordering::SeqCst))) {
            return lane.clone();
        }
        let lanes = st.lanes.entry((dc, Kind::General)).or_insert_with(|| vec![self.lane(self.main.clone(), Kind::General, true)]);
        lanes.iter().filter(|l| l.free_to_start()).min_by_key(|l| l.busy.load(Ordering::SeqCst)).expect("the account's own connection is always ready").clone()
    }

    /// Drops an extra connection that broke; its DC takes one fewer to that address for a
    /// while, [`NEVER_ANSWERED`] times longer when it never answered. Whether it was still
    /// there: every request on it comes here, the first drops it.
    pub fn drop_lane(&self, dc: i32, lane: &Arc<Lane<L>>) -> bool {
        if lane.main {
            return false;
        }
        let mut st = self.state();
        let Some(lanes) = st.lanes.get_mut(&(dc, lane.kind)) else { return false };
        let before = lanes.len();
        lanes.retain(|l| !Arc::ptr_eq(l, lane));
        if lanes.len() == before {
            return false;
        }
        let left = lanes.len();
        lane.link.quit();
        let rest = if lane.is_ready() { self.ceiling_for } else { self.ceiling_for * NEVER_ANSWERED };
        st.ceiling.insert((dc, lane.kind), (left, Instant::now() + rest));
        true
    }

    /// Sets how many connections each DC gets from now on; those beyond go now.
    pub fn tune(&self, general: usize, media: usize) {
        self.general.store(general.max(1), Ordering::SeqCst);
        self.media.store(media, Ordering::SeqCst);
        let mut st = self.state();
        for ((_, kind), lanes) in st.lanes.iter_mut() {
            let keep = if *kind == Kind::General { general.max(1) } else { media };
            while lanes.len() > keep {
                if let Some(lane) = lanes.pop() {
                    lane.link.quit();
                }
            }
        }
        st.ceiling.clear();
    }

    /// The connections to the DC's own address the account wants: what an upload shares its
    /// parts among.
    pub fn general(&self) -> usize {
        self.general.load(Ordering::SeqCst)
    }

    /// Per DC, how many connections carry file parts now, to either address.
    pub fn open(&self) -> Vec<(i32, usize)> {
        let mut per: HashMap<i32, usize> = HashMap::new();
        for ((dc, _), lanes) in self.state().lanes.iter() {
            *per.entry(*dc).or_default() += lanes.len();
        }
        let mut open: Vec<(i32, usize)> = per.into_iter().collect();
        open.sort();
        open
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connection that only counts how often it was ended.
    #[derive(Clone, Default)]
    struct Fake(Arc<AtomicUsize>);

    impl Link for Fake {
        fn quit(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn lanes(general: usize, media: usize) -> Lanes<Fake> {
        Lanes::new(Fake::default(), Tuning { general, media, parts: 16 }, Duration::from_secs(60))
    }

    /// Puts `n` requests on the lanes of `dc`, as parts sent at once; the lanes they went over.
    fn send(l: &Lanes<Fake>, dc: i32, media: bool, n: usize) -> Vec<Arc<Lane<Fake>>> {
        (0..n)
            .map(|_| {
                let lane = l.pick(dc, media);
                lane.busy.fetch_add(1, Ordering::SeqCst);
                lane
            })
            .collect()
    }

    fn answer(sent: &[Arc<Lane<Fake>>]) {
        for lane in sent {
            lane.busy.fetch_sub(1, Ordering::SeqCst);
            lane.answered();
        }
    }

    #[test]
    fn a_download_goes_over_the_media_connection_once_it_answers() {
        let l = lanes(4, 1);
        assert_eq!(l.grow(1, Kind::Media, Fake::default), 1);
        // Still being made: it takes one part, which connects it; the others don't wait for it.
        let first = send(&l, 1, true, 16);
        assert_eq!(first.iter().filter(|x| x.kind == Kind::Media).count(), 1);
        assert_eq!(first.iter().filter(|x| x.main).count(), 15);
        answer(&first);
        // It answered: every part of a download goes over it, none over the account's own.
        let next = send(&l, 1, true, 16);
        assert!(next.iter().all(|x| x.kind == Kind::Media && !x.main));
        // An upload never does.
        assert!(send(&l, 1, false, 8).iter().all(|x| x.kind == Kind::General));
    }

    #[test]
    fn a_dc_with_no_media_connection_shares_downloads_among_its_own() {
        let l = lanes(4, 1);
        assert_eq!(l.grow(3, Kind::General, Fake::default), 3);
        answer(&send(&l, 3, true, 4));
        let sent = send(&l, 3, true, 16);
        let mut per: Vec<usize> = sent.iter().map(|x| Arc::as_ptr(x) as usize).collect();
        per.sort();
        per.dedup();
        assert_eq!(per.len(), 4, "four connections carry the parts");
        assert!(sent.iter().all(|x| x.busy.load(Ordering::SeqCst) == 4), "four parts each");
    }

    #[test]
    fn a_connection_being_made_takes_one_part_until_it_answers() {
        let l = lanes(4, 0);
        l.grow(1, Kind::General, Fake::default);
        let sent = send(&l, 1, false, 16);
        // One to each of the three new ones, the rest to the account's own, which is connected.
        assert_eq!(sent.iter().filter(|x| !x.main).count(), 3);
        assert_eq!(sent.iter().filter(|x| x.main).count(), 13);
    }

    #[test]
    fn a_connection_that_broke_goes_and_its_dc_takes_one_fewer_for_a_while() {
        let l = Lanes::new(Fake::default(), Tuning { general: 4, media: 1, parts: 16 }, Duration::from_millis(80));
        l.grow(1, Kind::General, Fake::default);
        l.grow(1, Kind::Media, Fake::default);
        answer(&send(&l, 1, true, 1));
        let media = l.pick(1, true);
        assert_eq!(media.kind, Kind::Media);
        // Every request on it reports the break: the first drops it, once.
        assert!(l.drop_lane(1, &media));
        assert!(!l.drop_lane(1, &media));
        assert_eq!(media.link.0.load(Ordering::SeqCst), 1, "ended once");
        // The download goes on over the DC's own connections, and no new media one opens yet.
        assert_eq!(l.pick(1, true).kind, Kind::General);
        assert_eq!((l.wanted(1, Kind::Media), l.grow(1, Kind::Media, Fake::default)), (0, 0));
        // The DC's own address still has its four, and another DC is as it was.
        assert_eq!((l.wanted(1, Kind::General), l.wanted(2, Kind::Media)), (4, 1));
        // After a while it's tried again.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!((l.wanted(1, Kind::Media), l.grow(1, Kind::Media, Fake::default)), (1, 1));
    }

    #[test]
    fn a_connection_that_never_answered_isnt_tried_again_for_much_longer() {
        let l = Lanes::new(Fake::default(), Tuning { general: 4, media: 1, parts: 16 }, Duration::from_millis(40));
        l.grow(1, Kind::Media, Fake::default);
        // The part that would connect it got no answer: the address can't be reached.
        let media = l.pick(1, true);
        assert!(l.drop_lane(1, &media));
        // Well past the rest a broken connection gets, it still isn't opened again.
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!((l.wanted(1, Kind::Media), l.to_open(1, true, true)), (0, Some(Kind::General)));
        std::thread::sleep(Duration::from_millis(40 * NEVER_ANSWERED as u64 - 100));
        assert_eq!(l.to_open(1, true, true), Some(Kind::Media));
    }

    #[test]
    fn a_download_opens_the_media_connection_alone_and_the_dcs_own_when_it_is_out() {
        let l = lanes(4, 1);
        // A download from a DC with a media address: that connection, and then nothing more.
        assert_eq!(l.to_open(1, true, true), Some(Kind::Media));
        l.grow(1, Kind::Media, Fake::default);
        assert_eq!(l.to_open(1, true, true), None, "not the three to the DC's own address");
        assert_eq!(l.open(), [(1, 1)]);
        // A DC with none: its own address, as an upload anywhere.
        assert_eq!((l.to_open(3, true, false), l.to_open(1, false, true)), (Some(Kind::General), Some(Kind::General)));
        // The media one broke: the download gets the DC's own connections meanwhile.
        answer(&send(&l, 1, true, 1));
        let media = l.pick(1, true);
        assert!(l.drop_lane(1, &media));
        assert_eq!(l.to_open(1, true, true), Some(Kind::General));
        assert_eq!(l.grow(1, Kind::General, Fake::default), 3);
        assert_eq!(l.to_open(1, true, true), None);
        // Tuned to none there, a download never asks for the address.
        let none = lanes(4, 0);
        assert!(!none.takes_media(1) && l.takes_media(2));
        assert_eq!(none.to_open(1, true, true), Some(Kind::General));
    }

    #[test]
    fn the_accounts_own_connection_is_never_dropped() {
        let l = lanes(4, 1);
        let main = l.pick(1, false);
        assert!(main.main);
        assert!(!l.drop_lane(1, &main));
        assert!(l.pick(1, false).main);
        assert_eq!(l.open(), [(1, 1)]);
    }

    #[test]
    fn a_tuning_reads_from_its_short_form() {
        assert_eq!("4x16".parse(), Ok(Tuning { general: 4, media: 1, parts: 16 }));
        assert_eq!("8x32m0".parse(), Ok(Tuning { general: 8, media: 0, parts: 32 }));
        assert_eq!(Tuning { general: 1, media: 2, parts: 16 }.to_string(), "1x16m2");
        assert!("4".parse::<Tuning>().is_err() && "x16".parse::<Tuning>().is_err());
    }

    #[test]
    fn tuning_down_ends_the_connections_beyond() {
        let l = lanes(4, 1);
        let made = Fake::default();
        l.grow(1, Kind::General, || made.clone());
        l.grow(1, Kind::Media, || made.clone());
        assert_eq!(l.open(), [(1, 5)]);
        l.tune(2, 0);
        assert_eq!(l.open(), [(1, 2)]);
        assert_eq!(made.0.load(Ordering::SeqCst), 3, "two of the DC's own and the media one ended");
        assert_eq!((l.wanted(1, Kind::General), l.wanted(1, Kind::Media)), (2, 0));
        assert_eq!(l.pick(1, true).kind, Kind::General);
    }
}
