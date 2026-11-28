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
//! A page pays only where files sit together. In a busy chat each file may be a page of its
//! own, a hundred messages with their senders where one message would do: after pages in a row
//! that served no other download, a chat's files are asked about one by one, and a page is
//! tried again now and then ([`Paging`]).
//!
//! A place kept may have expired when used: Telegram says so, and the download asks again
//! ([`asks_again`]). And the message may have got another file meanwhile: a place is forgotten
//! when its message is seen changed ([`Places::keep_if`]).

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
        // A key kept already is renewed even when full: its place is the newer one.
        if kept.len() < self.most || kept.contains_key(&key) {
            kept.insert(key, (place, Instant::now()));
        }
    }

    pub fn forget(&self, key: &K) {
        self.kept().remove(key);
    }

    /// Forgets the place kept for `key` unless it's `same` as what the message has now: a
    /// message whose file was replaced would else download the old one.
    pub fn keep_if(&self, key: &K, same: impl FnOnce(&P) -> bool) {
        let mut kept = self.kept();
        if kept.get(key).is_some_and(|(place, _)| !same(place)) {
            kept.remove(key);
        }
    }

    /// Forgets every place: what the messages hold now isn't known anymore.
    pub fn clear(&self) {
        self.kept().clear();
    }
}

/// After this many pages in a row that served no other download, a chat's files are asked
/// about one by one.
pub const LONE_PAGES: u32 = 2;
/// After this many files asked about one by one, a page is tried again.
pub const ONE_BY_ONE: u32 = 30;

#[derive(Default)]
struct Run {
    /// A page was asked for in this chat before.
    paged: bool,
    /// Downloads that found their place kept since the last request.
    served: u32,
    /// Pages in a row that served no other download.
    lone: u32,
    /// Files asked about one by one since the last page.
    singly: u32,
}

/// Whether a page of history pays in each chat, from what the last ones brought.
pub struct Paging<C> {
    chats: Mutex<HashMap<C, Run>>,
}

impl<C: Hash + Eq> Default for Paging<C> {
    fn default() -> Paging<C> {
        Paging { chats: Mutex::new(HashMap::new()) }
    }
}

impl<C: Hash + Eq> Paging<C> {
    /// A download in `chat` found its file's place kept: the last page paid.
    pub fn served(&self, chat: C) {
        self.chats.lock().unwrap_or_else(|e| e.into_inner()).entry(chat).or_default().served += 1;
    }

    /// Whether the request about to be made in `chat` asks for the page around the file; else
    /// for the file's message alone.
    pub fn asks_page(&self, chat: C) -> bool {
        let mut chats = self.chats.lock().unwrap_or_else(|e| e.into_inner());
        let run = chats.entry(chat).or_default();
        if run.lone >= LONE_PAGES {
            run.singly += 1;
            if run.singly <= ONE_BY_ONE {
                return false;
            }
            // A page again: one more that serves nobody goes straight back to one by one.
            (run.singly, run.lone, run.served) = (0, LONE_PAGES - 1, 0);
            return true;
        }
        if run.paged {
            run.lone = if run.served == 0 { run.lone + 1 } else { 0 };
        }
        (run.paged, run.served) = (true, 0);
        if run.lone >= LONE_PAGES {
            run.singly = 1;
            return false;
        }
        true
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
    fn a_place_kept_already_is_renewed_even_when_no_more_fit() {
        let places: Places<i32, &str> = Places::new(Duration::from_secs(60), 2);
        places.put(1, "old");
        places.put(2, "b");
        // Full, all fresh: a new key waits, but a newer place for a key that's there goes in.
        places.put(3, "c");
        places.put(1, "new");
        assert_eq!((places.get(&1), places.get(&2), places.get(&3)), (Some("new"), Some("b"), None));
    }

    #[test]
    fn a_place_is_forgotten_when_its_message_has_another_file_now() {
        let places: Places<i32, u64> = Places::new(Duration::from_secs(60), 10);
        places.put(1, 4096);
        places.put(2, 4096);
        // Message 1 still has a file of 4096 bytes, message 2 one of 5000 now; 3 was never kept.
        places.keep_if(&1, |size| *size == 4096);
        places.keep_if(&2, |size| *size == 5000);
        places.keep_if(&3, |_| false);
        assert_eq!((places.get(&1), places.get(&2), places.get(&3)), (Some(4096), None, None));
        places.clear();
        assert_eq!(places.get(&1), None);
    }

    #[test]
    fn pages_that_serve_no_other_download_give_way_to_one_file_at_a_time() {
        let paging: Paging<&str> = Paging::default();
        // Files side by side: each page serves the downloads after it, so pages go on.
        for _ in 0..5 {
            assert!(paging.asks_page("folder"));
            for _ in 0..99 {
                paging.served("folder");
            }
        }
        // Files far apart: two pages that served nobody else, then one by one.
        assert!(paging.asks_page("busy group"));
        assert!(paging.asks_page("busy group"), "one lone page may be chance");
        for _ in 0..ONE_BY_ONE {
            assert!(!paging.asks_page("busy group"));
        }
        // A page is tried again after a while; serving nobody, it goes straight back.
        assert!(paging.asks_page("busy group"));
        assert!(!paging.asks_page("busy group"));
        // Each chat on its own.
        assert!(paging.asks_page("folder"));
    }

    #[test]
    fn a_page_that_serves_again_brings_the_pages_back() {
        let paging: Paging<i32> = Paging::default();
        assert!(paging.asks_page(7) && paging.asks_page(7) && !paging.asks_page(7));
        for _ in 1..ONE_BY_ONE {
            assert!(!paging.asks_page(7));
        }
        // The page tried again serves the next downloads: this part of the chat is dense.
        assert!(paging.asks_page(7));
        paging.served(7);
        assert!(paging.asks_page(7));
        assert!(paging.asks_page(7), "and one lone page after it is forgiven");
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
