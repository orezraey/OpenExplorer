//! Where Telegram keeps each message's file, remembered for a while.
//!
//! A download needs the file's place (its id, access hash and a reference that expires), which
//! comes with the message. Asking for the message costs a round trip before the first byte: for
//! a folder of small files, half of all the waiting. So a file in a channel is asked about with
//! the page of the channel's history around it ([`page_from`]), and the places of the files
//! there are kept here for some minutes: files sent together, as a folder's are, sit side by
//! side in the channel, and one request then serves a hundred downloads.
//!
//! The page is the history's and not the messages by their ids: a hundred ids in one request,
//! from four downloads at once, made Telegram ask for waits of half a minute (FLOOD_WAIT), and
//! one request of a hundred at a time still drew one of 4 s in three folders.
//!
//! A place kept may have expired when used: Telegram says so, and the download asks again
//! ([`asks_again`]).

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How many messages a page of history holds: Telegram's most.
pub const PAGE: i32 = 100;

/// Where the page of history asked for message `id` starts: the history is read from the
/// message before this one back, so `id` comes with about as many sent after it as before.
pub fn page_from(id: i32) -> i32 {
    id.saturating_add(PAGE / 2)
}

/// Whether a download that failed is tried again with the file's place asked anew: its place
/// was a kept one that Telegram no longer takes (`stale`), and nothing was written yet.
pub fn asks_again(kept: bool, stale: bool, written: u64) -> bool {
    kept && stale && written == 0
}

pub struct Places<K, P> {
    kept: Mutex<HashMap<K, (P, Instant)>>,
    fresh_for: Duration,
    /// Past this many, those no longer fresh go.
    most: usize,
}

impl<K: Hash + Eq, P: Clone> Places<K, P> {
    /// Places that count as known for `fresh_for`.
    pub fn new(fresh_for: Duration, most: usize) -> Places<K, P> {
        Places { kept: Mutex::new(HashMap::new()), fresh_for, most }
    }

    fn kept(&self) -> std::sync::MutexGuard<'_, HashMap<K, (P, Instant)>> {
        self.kept.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The place kept for `key`, if it's fresh still.
    pub fn get(&self, key: &K) -> Option<P> {
        self.kept().get(key).filter(|(_, at)| at.elapsed() < self.fresh_for).map(|(p, _)| p.clone())
    }

    pub fn put(&self, key: K, place: P) {
        let mut kept = self.kept();
        if kept.len() >= self.most {
            kept.retain(|_, (_, at)| at.elapsed() < self.fresh_for);
        }
        if kept.len() < self.most {
            kept.insert(key, (place, Instant::now()));
        }
    }

    pub fn forget(&self, key: &K) {
        self.kept().remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_asked_about_with_the_page_around_it() {
        // The page holds the hundred messages before 1050: 1000 is among them even when every
        // id from it up is taken, with those sent after it as well as before.
        let from = page_from(1000);
        assert_eq!(from, 1050);
        assert!((from - PAGE..from).contains(&1000) && (from - PAGE..from).contains(&1049) && (from - PAGE..from).contains(&951));
        assert_eq!(page_from(i32::MAX), i32::MAX);
    }

    #[test]
    fn a_place_is_known_while_fresh_and_asked_anew_after() {
        let places: Places<i32, &str> = Places::new(Duration::from_millis(60), 10);
        assert_eq!(places.get(&1), None);
        places.put(1, "a");
        places.put(2, "b");
        assert_eq!((places.get(&1), places.get(&2)), (Some("a"), Some("b")));
        places.forget(&1);
        assert_eq!((places.get(&1), places.get(&2)), (None, Some("b")));
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(places.get(&2), None, "no longer fresh");
        places.put(2, "c");
        assert_eq!(places.get(&2), Some("c"));
    }

    #[test]
    fn the_places_kept_stay_bounded() {
        let places: Places<i32, i32> = Places::new(Duration::from_millis(40), 3);
        for i in 0..5 {
            places.put(i, i);
        }
        assert_eq!((0..5).filter(|i| places.get(i).is_some()).count(), 3, "the newest wait while the old are fresh");
        std::thread::sleep(Duration::from_millis(60));
        places.put(7, 7);
        assert_eq!(places.get(&7), Some(7), "the stale ones made room");
        assert_eq!(places.kept().len(), 1);
    }

    #[test]
    fn only_a_kept_place_that_went_stale_before_any_byte_is_asked_again() {
        assert!(asks_again(true, true, 0));
        // A place just asked for that Telegram refuses won't get better by asking.
        assert!(!asks_again(false, true, 0));
        // Another failure isn't about the place.
        assert!(!asks_again(true, false, 0));
        // Bytes already written can't be taken back from where they went.
        assert!(!asks_again(true, true, 4096));
    }
}
