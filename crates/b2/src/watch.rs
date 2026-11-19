//! Reading the folders on screen again on the app's own, so what changes elsewhere (the website,
//! another computer, a sync tool) shows without Atualizar.
//!
//! # Why reading again, and not being told
//!
//! B2 has no change log nor any "what changed since" call. Its Event Notifications only POST to
//! a webhook (`targetType` "webhook" is the only one, the URL must be HTTPS and not Backblaze's;
//! the feature is turned on by asking Backblaze's support), and a desktop app has no public URL
//! to be called on. So the app reads again, and B2 lists for free: Backblaze's transaction
//! pricing page puts `b2_list_file_names` (class C) among the calls at no cost; only class D,
//! the notifications themselves, is billed.
//!
//! # How often, at most
//!
//! - A folder a pane shows is read again every [`EVERY`] while the window has the focus, longer
//!   for big folders: [`every`] grows with the requests one reading takes (10,000 names each).
//! - Each key reads on its own at most [`MOST_PER_HOUR`] requests an hour and [`MOST_PER_DAY`] a
//!   day ([`Budget`]), whatever the panes show; past them it waits for the hour or the day to
//!   turn. Opening a folder and Atualizar aren't counted: they are the user's.
//! - A 429 or a 503 to any call holds these readings for its `Retry-After` (or [`crate::api::PAUSE`]),
//!   and they're never sent again on their own: they wait for their next turn.
//! - A cap the account set itself (`transaction_cap_exceeded`, Caps & Alerts) stops them until a
//!   reading the user asks for goes through.

use std::time::Duration;

use crate::account::{Item, PAGE};

/// How often a folder of up to 10,000 names is read again while it shows.
pub const EVERY: Duration = Duration::from_secs(45);
/// The most requests a key's own readings make in an hour. A pane on a folder takes about 80
/// an hour, whatever its size ([`every`]), so both panes on the same key fit, with room.
pub const MOST_PER_HOUR: u32 = 200;
/// The most requests a key's own readings make in a day (UTC): both panes left on the key for
/// some 12 hours, one for a day.
pub const MOST_PER_DAY: u32 = 2_000;

/// The requests one reading of a folder of `items` names takes.
pub fn cost(items: usize) -> u32 {
    items.div_ceil(PAGE as usize).max(1) as u32
}

/// How long until a folder of `items` names is read again: [`EVERY`] for each request a reading
/// takes, so a big folder costs no more an hour than a small one.
pub fn every(items: usize) -> Duration {
    EVERY * cost(items).min(20)
}

/// What a reading the app made on its own came to.
#[derive(Clone, Debug, PartialEq)]
pub enum Polled {
    /// The folder is as kept.
    Same,
    /// It changed: its items now.
    Changed(Vec<Item>),
    /// Not read: B2 asked for a pause, or the hour's or the day's requests are spent. Seconds
    /// until it may be read again.
    Wait(u64),
    /// Not read: the account's own cap was reached (the words say which); the app's own readings
    /// stop until one the user asks for goes through.
    Stopped(String),
    /// Read, and B2 answered with an error.
    Failed(String),
}

/// One key's account of its own readings.
#[derive(Debug, Default)]
pub struct Budget {
    hour: u64,
    in_hour: u32,
    day: u64,
    in_day: u32,
    capped: Option<String>,
}

impl Budget {
    /// Counts `cost` requests at `now` (seconds since 1970) if the hour and the day have room;
    /// else what to do instead.
    pub fn take(&mut self, now: u64, cost: u32) -> Result<(), Polled> {
        if let Some(why) = &self.capped {
            return Err(Polled::Stopped(why.clone()));
        }
        let (hour, day) = (now / 3600, now / 86_400);
        if hour != self.hour {
            (self.hour, self.in_hour) = (hour, 0);
        }
        if day != self.day {
            (self.day, self.in_day) = (day, 0);
        }
        if self.in_day + cost > MOST_PER_DAY {
            return Err(Polled::Wait((day + 1) * 86_400 - now));
        }
        if self.in_hour + cost > MOST_PER_HOUR {
            return Err(Polled::Wait((hour + 1) * 3600 - now));
        }
        self.in_hour += cost;
        self.in_day += cost;
        Ok(())
    }

    /// The account's cap was reached: the app's own readings stop.
    pub fn cap(&mut self, why: String) {
        self.capped = Some(why);
    }

    /// A reading the user asked for went through: no cap stands in the way any more.
    pub fn uncap(&mut self) {
        self.capped = None;
    }

    /// Requests counted this hour and this day.
    pub fn spent(&self) -> (u32, u32) {
        (self.in_hour, self.in_day)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readings_stop_at_the_hours_and_the_days_budget() {
        let mut b = Budget::default();
        // 10:00 UTC on some day.
        let t0 = 20_000 * 86_400 + 10 * 3600;
        for i in 0..MOST_PER_HOUR {
            assert_eq!(b.take(t0 + u64::from(i) * 10, 1), Ok(()), "request {i}");
        }
        // The hour is spent: wait for 11:00.
        let at = t0 + 1200;
        assert_eq!(b.take(at, 1), Err(Polled::Wait(t0 + 3600 - at)));
        // A new hour has room again, until the day's budget runs out.
        let mut now = t0 + 3600;
        let mut taken = MOST_PER_HOUR;
        while taken < MOST_PER_DAY {
            match b.take(now, 1) {
                Ok(()) => taken += 1,
                Err(Polled::Wait(w)) => now += w,
                Err(other) => panic!("{other:?}"),
            }
        }
        let next_day = (now / 86_400 + 1) * 86_400;
        assert_eq!(b.take(now, 1), Err(Polled::Wait(next_day - now)));
        assert_eq!(b.take(next_day, 1), Ok(()));
        assert_eq!(b.spent(), (1, 1));
    }

    /// Both panes on folders of the same key, read again on their turns, for an hour: none is
    /// held by the budget.
    #[test]
    fn two_panes_on_one_key_fit_in_the_hours_budget() {
        let mut b = Budget::default();
        let t0 = 20_000 * 86_400 + 10 * 3600;
        for (items, offset) in [(500usize, 0u64), (25_000, 7)] {
            let step = every(items).as_secs();
            let mut now = t0 + offset;
            while now < t0 + 3600 {
                assert_eq!(b.take(now, cost(items)), Ok(()), "{items} items at {}", now - t0);
                now += step;
            }
        }
        assert!(b.spent().0 <= MOST_PER_HOUR, "{:?}", b.spent());
    }

    #[test]
    fn a_cap_stops_readings_until_the_user_reads() {
        let mut b = Budget::default();
        b.cap("cap".into());
        assert_eq!(b.take(1, 1), Err(Polled::Stopped("cap".into())));
        b.uncap();
        assert_eq!(b.take(1, 1), Ok(()));
    }

    #[test]
    fn big_folders_are_read_less_often() {
        assert_eq!(cost(0), 1);
        assert_eq!(cost(10_000), 1);
        assert_eq!(cost(25_000), 3);
        assert_eq!(every(500), EVERY);
        assert_eq!(every(25_000), EVERY * 3);
        // A folder of a million names: at most every 15 minutes.
        assert_eq!(every(1_000_000), EVERY * 20);
    }
}
