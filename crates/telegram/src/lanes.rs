//! The connections to a DC that carry file parts, and which one a part goes over.
//!
//! A DC has its own address, where the account's connection is, and most have another kept for
//! files ("media only"), which is where Telegram's own apps download from (TDLib, Android,
//! Desktop). A download goes over a connection to the media address once one answers; an upload,
//! or a download from a DC with no media address, is shared out among the connections to the
//! DC's own address, the account's included. What `real` adds is the network: here is only who
//! carries what, so the tests can play Telegram.
//!
//! A connection carries parts only while it answers. A new one is asked something small first
//! (`real` does: its probe), and takes its share once that came back; one whose request was cut
//! leaves the round the same way, until it answers again. So a part never waits behind a
//! connection being made, nor behind an address that stopped answering, and how long a
//! connection gets to answer doesn't depend on how long a part takes on a slow line.
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
    /// The account's own connection: never dropped, always in the round.
    pub main: bool,
    pub kind: Kind,
    /// It answers: parts go over it. Not yet for one being made, nor for one whose request was
    /// cut, until it answers the probe.
    ready: AtomicBool,
    /// It answered at some point.
    ever: AtomicBool,
    born: Instant,
}

impl<L> Lane<L> {
    /// Whether parts go over it now.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    pub fn age(&self) -> Duration {
        self.born.elapsed()
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
    /// Per DC and address, how many connections in a row were dropped without ever answering.
    unanswered: HashMap<(i32, Kind), u32>,
}

/// The longest a DC goes without a connection that keeps failing to answer, in rests: each one
/// dropped before it ever answered doubles the rest, up to this. An address that can't be
/// reached from this network (the media one may be closed where the DC's own is open) is then
/// tried seldom, and one that a short network drop caught being made, again soon.
pub const LONGEST_REST: u32 = 16;

pub struct Lanes<L> {
    main: L,
    state: Mutex<State<L>>,
    general: AtomicUsize,
    media: AtomicUsize,
    /// How long a DC keeps the fewer connections it was left with when one broke: a short
    /// network drop breaks them all, and the speed should come back after it.
    rest: Duration,
}

impl<L: Link + Clone> Lanes<L> {
    /// The lanes of an account whose own connection is `main`; `rest` is how long a DC goes
    /// with one connection fewer after one broke.
    pub fn new(main: L, tuning: Tuning, rest: Duration) -> Lanes<L> {
        Lanes {
            main,
            state: Mutex::new(State { lanes: HashMap::new(), ceiling: HashMap::new(), unanswered: HashMap::new() }),
            general: AtomicUsize::new(tuning.general.max(1)),
            media: AtomicUsize::new(tuning.media),
            rest,
        }
    }

    fn state(&self) -> MutexGuard<'_, State<L>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lane(&self, link: L, kind: Kind, main: bool) -> Arc<Lane<L>> {
        Arc::new(Lane { link, busy: AtomicUsize::new(0), main, kind, ready: AtomicBool::new(main), ever: AtomicBool::new(main), born: Instant::now() })
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
    /// and no other: until that one answers its parts go over the account's own. The others to
    /// the DC's own address open for an upload, for a DC with no media address, or when the
    /// media one broke.
    pub fn to_open(&self, dc: i32, down: bool, has_media: bool) -> Option<Kind> {
        let kind = if down && has_media && self.takes_media(dc) { Kind::Media } else { Kind::General };
        (self.count(dc, kind) < self.wanted(dc, kind)).then_some(kind)
    }

    /// Opens connections to `dc`'s `kind` of address up to the number wanted, each made by
    /// `open`. The new ones, for the caller to probe: none carries a part before it
    /// [answered](Lanes::answered).
    pub fn grow(&self, dc: i32, kind: Kind, mut open: impl FnMut() -> L) -> Vec<Arc<Lane<L>>> {
        let want = self.wanted(dc, kind);
        let mut st = self.state();
        let lanes = st.lanes.entry((dc, kind)).or_insert_with(|| (kind == Kind::General).then(|| self.lane(self.main.clone(), kind, true)).into_iter().collect());
        let mut new = Vec::new();
        while lanes.len() < want {
            let lane = self.lane(open(), kind, false);
            lanes.push(lane.clone());
            new.push(lane);
        }
        new
    }

    /// The connection a file part to `dc` goes over, among those that answer. A download
    /// (`media`) takes the least busy one to the media address; anything else, and a download
    /// while none answers there, the least busy one to the DC's own address.
    pub fn pick(&self, dc: i32, media: bool) -> Arc<Lane<L>> {
        let mut st = self.state();
        let least_busy = |lanes: &Vec<Arc<Lane<L>>>| lanes.iter().filter(|l| l.is_ready()).min_by_key(|l| l.busy.load(Ordering::SeqCst)).cloned();
        if media && let Some(lane) = st.lanes.get(&(dc, Kind::Media)).and_then(least_busy) {
            return lane;
        }
        let lanes = st.lanes.entry((dc, Kind::General)).or_insert_with(|| vec![self.lane(self.main.clone(), Kind::General, true)]);
        least_busy(lanes).expect("the account's own connection is always in the round")
    }

    /// `lane`, a connection to `dc`, answered: parts go over it from now on.
    pub fn answered(&self, dc: i32, lane: &Lane<L>) {
        lane.ready.store(true, Ordering::SeqCst);
        if !lane.ever.swap(true, Ordering::SeqCst) {
            self.state().unanswered.remove(&(dc, lane.kind));
        }
    }

    /// A request on `lane` was cut: no part goes over it until it answers again. Whether this
    /// was the first to say so, for the caller to probe it once (never, for the account's own).
    pub fn falter(&self, lane: &Lane<L>) -> bool {
        !lane.main && lane.ready.swap(false, Ordering::SeqCst)
    }

    /// Drops an extra connection that broke; its DC takes one fewer to that address for a
    /// while: a rest, and twice as long each time one in a row went without ever answering (up
    /// to [`LONGEST_REST`] rests). Whether it was still there: every request on it comes here,
    /// the first drops it.
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
        let rests = if lane.ever.load(Ordering::SeqCst) {
            1
        } else {
            let in_a_row = st.unanswered.entry((dc, lane.kind)).or_insert(0);
            *in_a_row += 1;
            1u32.checked_shl(*in_a_row - 1).unwrap_or(LONGEST_REST).min(LONGEST_REST)
        };
        // Never sooner than a rest already set: one connection that broke after working doesn't
        // cut short the long rest of an address that doesn't answer.
        let until = Instant::now() + self.rest * rests;
        let until = st.ceiling.get(&(dc, lane.kind)).map_or(until, |(_, set)| until.max(*set));
        st.ceiling.insert((dc, lane.kind), (left, until));
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
        st.unanswered.clear();
    }

    /// The connections to the DC's own address the account wants: what an upload shares its
    /// parts among.
    pub fn general(&self) -> usize {
        self.general.load(Ordering::SeqCst)
    }

    /// Per DC, how many connections carry file parts now or are being made to, to either
    /// address.
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

    /// Opens the connections of `dc` to its `kind` of address, each answering its probe.
    fn open(l: &Lanes<Fake>, dc: i32, kind: Kind) -> Vec<Arc<Lane<Fake>>> {
        let new = l.grow(dc, kind, Fake::default);
        for lane in &new {
            l.answered(dc, lane);
        }
        new
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

    #[test]
    fn a_download_goes_over_the_media_connection_once_it_answers() {
        let l = lanes(4, 1);
        let new = l.grow(1, Kind::Media, Fake::default);
        assert_eq!(new.len(), 1);
        // Still being made: no part waits for it, all go over the account's own.
        assert!(send(&l, 1, true, 16).iter().all(|x| x.main));
        l.answered(1, &new[0]);
        // It answered: every part of a download goes over it, none over the account's own.
        assert!(send(&l, 1, true, 16).iter().all(|x| x.kind == Kind::Media && !x.main));
        // An upload never does.
        assert!(send(&l, 1, false, 8).iter().all(|x| x.kind == Kind::General));
    }

    #[test]
    fn a_dc_with_no_media_connection_shares_downloads_among_its_own() {
        let l = lanes(4, 1);
        assert_eq!(open(&l, 3, Kind::General).len(), 3);
        let sent = send(&l, 3, true, 16);
        let mut per: Vec<usize> = sent.iter().map(|x| Arc::as_ptr(x) as usize).collect();
        per.sort();
        per.dedup();
        assert_eq!(per.len(), 4, "four connections carry the parts");
        assert!(sent.iter().all(|x| x.busy.load(Ordering::SeqCst) == 4), "four parts each");
    }

    #[test]
    fn a_connection_being_made_takes_no_part_until_it_answers() {
        let l = lanes(4, 0);
        let new = l.grow(1, Kind::General, Fake::default);
        assert_eq!(new.len(), 3);
        // However long connecting takes, and however slow the line: the parts don't go there.
        assert!(send(&l, 1, false, 16).iter().all(|x| x.main));
        l.answered(1, &new[0]);
        let sent = send(&l, 1, false, 4);
        assert!(sent.iter().all(|x| Arc::ptr_eq(x, &new[0])), "the one that answered takes the next ones: it has none yet");
        assert!(!new[1].is_ready() && !new[2].is_ready());
    }

    #[test]
    fn a_connection_whose_request_was_cut_leaves_the_round_until_it_answers_again() {
        let l = lanes(4, 1);
        let media = open(&l, 1, Kind::Media).remove(0);
        assert_eq!(l.pick(1, true).kind, Kind::Media);
        // Every part on it reports the cut: the first is told to probe it.
        assert!(l.falter(&media));
        assert!(!l.falter(&media));
        // Meanwhile the download goes on over the account's own, and no other connection opens:
        // the media one is still there.
        assert!(send(&l, 1, true, 16).iter().all(|x| x.main));
        assert_eq!(l.to_open(1, true, true), None);
        // It answers the probe: back in the round.
        l.answered(1, &media);
        assert_eq!(l.pick(1, true).kind, Kind::Media);
        // The account's own never leaves it.
        let main = l.pick(1, false);
        assert!(main.main && !l.falter(&main) && l.pick(1, false).main);
    }

    #[test]
    fn a_connection_that_broke_goes_and_its_dc_takes_one_fewer_for_a_while() {
        let l = Lanes::new(Fake::default(), Tuning { general: 4, media: 1, parts: 16 }, Duration::from_millis(80));
        open(&l, 1, Kind::General);
        open(&l, 1, Kind::Media);
        let media = l.pick(1, true);
        assert_eq!(media.kind, Kind::Media);
        // Every request on it reports the break: the first drops it, once.
        assert!(l.drop_lane(1, &media));
        assert!(!l.drop_lane(1, &media));
        assert_eq!(media.link.0.load(Ordering::SeqCst), 1, "ended once");
        // The download goes on over the DC's own connections, and no new media one opens yet.
        assert_eq!(l.pick(1, true).kind, Kind::General);
        assert_eq!((l.wanted(1, Kind::Media), l.grow(1, Kind::Media, Fake::default).len()), (0, 0));
        // The DC's own address still has its four, and another DC is as it was.
        assert_eq!((l.wanted(1, Kind::General), l.wanted(2, Kind::Media)), (4, 1));
        // After a while it's tried again.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!((l.wanted(1, Kind::Media), l.grow(1, Kind::Media, Fake::default).len()), (1, 1));
    }

    #[test]
    fn an_address_that_never_answers_is_tried_less_and_less_often() {
        let l = Lanes::new(Fake::default(), Tuning { general: 4, media: 1, parts: 16 }, Duration::from_millis(60));
        // The probe of a new connection got no answer: one rest, as for any that broke (a short
        // network drop may have caught it being made).
        let first = l.grow(1, Kind::Media, Fake::default).remove(0);
        assert!(l.drop_lane(1, &first));
        assert_eq!((l.wanted(1, Kind::Media), l.to_open(1, true, true)), (0, Some(Kind::General)));
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(l.to_open(1, true, true), Some(Kind::Media));
        // Again no answer: twice as long.
        let second = l.grow(1, Kind::Media, Fake::default).remove(0);
        assert!(l.drop_lane(1, &second));
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(l.wanted(1, Kind::Media), 0, "one rest isn't enough the second time");
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(l.wanted(1, Kind::Media), 1);
        // One answers at last, then breaks: counted from the start again.
        let third = l.grow(1, Kind::Media, Fake::default).remove(0);
        l.answered(1, &third);
        assert!(l.drop_lane(1, &third));
        std::thread::sleep(Duration::from_millis(80));
        let fourth = l.grow(1, Kind::Media, Fake::default).remove(0);
        assert!(l.drop_lane(1, &fourth));
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(l.wanted(1, Kind::Media), 1, "one rest, not four");
    }

    #[test]
    fn a_long_rest_isnt_cut_short_by_another_connection_breaking() {
        let l = Lanes::new(Fake::default(), Tuning { general: 4, media: 0, parts: 16 }, Duration::from_millis(60));
        // Of the three new ones, two answer and one never does: one rest.
        let new = l.grow(1, Kind::General, Fake::default);
        l.answered(1, &new[1]);
        l.answered(1, &new[2]);
        assert!(l.drop_lane(1, &new[0]));
        std::thread::sleep(Duration::from_millis(80));
        // The one opened in its place doesn't answer either: two rests this time.
        let again = l.grow(1, Kind::General, Fake::default).remove(0);
        assert!(l.drop_lane(1, &again));
        // One that had answered breaks right after: its one rest doesn't replace the two.
        assert!(l.drop_lane(1, &new[1]));
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(l.wanted(1, Kind::General), 2, "past one rest, still the two that are left");
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(l.wanted(1, Kind::General), 4);
    }

    #[test]
    fn a_download_opens_the_media_connection_alone_and_the_dcs_own_when_it_is_out() {
        let l = lanes(4, 1);
        // A download from a DC with a media address: that connection, and then nothing more.
        assert_eq!(l.to_open(1, true, true), Some(Kind::Media));
        let media = open(&l, 1, Kind::Media).remove(0);
        assert_eq!(l.to_open(1, true, true), None, "not the three to the DC's own address");
        assert_eq!(l.open(), [(1, 1)]);
        // A DC with none: its own address, as an upload anywhere.
        assert_eq!((l.to_open(3, true, false), l.to_open(1, false, true)), (Some(Kind::General), Some(Kind::General)));
        // The media one broke: the download gets the DC's own connections meanwhile.
        assert!(l.drop_lane(1, &media));
        assert_eq!(l.to_open(1, true, true), Some(Kind::General));
        assert_eq!(l.grow(1, Kind::General, Fake::default).len(), 3);
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
