//! How many file parts an account has in flight at most: a number that holds while it changes.
//!
//! The parts of every transfer of an account wait here for a slot. The number can be set anew
//! (the benchmarks do, to compare settings) while parts are in flight: made bigger, the new
//! slots are there at once; made smaller, the parts in flight end as they are and their slots
//! go with them, so no new part gets in until the account is under the new number. One budget
//! stays for the account's life, so two settings never add up.

use std::sync::Mutex;

use tokio::sync::Semaphore;

struct State {
    size: usize,
    /// Slots taken away while in use: each goes when its part ends.
    owed: usize,
}

pub struct Budget {
    slots: Semaphore,
    state: Mutex<State>,
}

/// A part's place in the budget, until dropped.
pub struct Slot<'a>(&'a Budget);

impl Budget {
    /// A budget of `size` parts in flight (one at least).
    pub fn new(size: usize) -> Budget {
        let size = size.max(1);
        Budget { slots: Semaphore::new(size), state: Mutex::new(State { size, owed: 0 }) }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How many parts may be in flight.
    pub fn size(&self) -> usize {
        self.state().size
    }

    /// Waits for a slot.
    pub async fn take(&self) -> Slot<'_> {
        // Counted by hand from here: a slot given back may be one that was taken away.
        self.slots.acquire().await.expect("the budget is never closed").forget();
        Slot(self)
    }

    /// Sets how many parts may be in flight from now on (one at least).
    pub fn resize(&self, size: usize) {
        let size = size.max(1);
        let mut st = self.state();
        if size >= st.size {
            // What was still owed is forgiven first; the rest are new slots.
            let more = size - st.size;
            let forgiven = more.min(st.owed);
            st.owed -= forgiven;
            self.slots.add_permits(more - forgiven);
        } else {
            // The free ones go now, the ones in use as their parts end.
            let fewer = st.size - size;
            let mut gone = 0;
            while gone < fewer {
                let Ok(free) = self.slots.try_acquire() else { break };
                free.forget();
                gone += 1;
            }
            st.owed += fewer - gone;
        }
        st.size = size;
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut st = self.0.state();
        if st.owed > 0 {
            st.owed -= 1;
        } else {
            self.0.slots.add_permits(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn run<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(f)
    }

    /// Whether a part gets a slot now; it's given back at once.
    async fn gets_in(b: &Budget) -> bool {
        tokio::time::timeout(Duration::from_millis(30), b.take()).await.is_ok()
    }

    #[test]
    fn as_many_parts_fly_as_the_budget_says() {
        run(async {
            let b = Budget::new(3);
            let held = [b.take().await, b.take().await, b.take().await];
            assert!(!gets_in(&b).await, "a fourth waits");
            drop(held);
            assert!(gets_in(&b).await);
            assert_eq!((Budget::new(0).size(), b.size()), (1, 3));
        });
    }

    #[test]
    fn made_smaller_with_parts_in_flight_no_new_one_gets_in_until_under_the_new_number() {
        run(async {
            let b = Budget::new(4);
            let mut held = vec![b.take().await, b.take().await, b.take().await, b.take().await];
            b.resize(2);
            assert_eq!(b.size(), 2);
            // Two parts end: the account is at two, the new number, and none more gets in.
            held.truncate(2);
            assert!(!gets_in(&b).await, "four slots were never there together with the two");
            // A third ends: one below, one gets in.
            held.truncate(1);
            let one = b.take().await;
            assert!(!gets_in(&b).await);
            drop(one);
            drop(held);
            let two = [b.take().await, b.take().await];
            assert!(!gets_in(&b).await, "two it stays");
            drop(two);
        });
    }

    #[test]
    fn made_bigger_the_new_slots_are_there_at_once_and_only_those() {
        run(async {
            let b = Budget::new(2);
            let held = [b.take().await, b.take().await];
            b.resize(4);
            let more = [b.take().await, b.take().await];
            assert!(!gets_in(&b).await, "four in flight, not the two of before and four more");
            drop(held);
            drop(more);
            let all = [b.take().await, b.take().await, b.take().await, b.take().await];
            assert!(!gets_in(&b).await);
            drop(all);
        });
    }

    #[test]
    fn smaller_and_then_bigger_before_the_parts_end_comes_out_right() {
        run(async {
            let b = Budget::new(4);
            let mut held = vec![b.take().await, b.take().await, b.take().await, b.take().await];
            b.resize(1);
            b.resize(3);
            // Four in flight against three: one must end before any gets in.
            assert!(!gets_in(&b).await);
            held.truncate(3);
            assert!(!gets_in(&b).await, "three in flight is the number");
            held.truncate(2);
            assert!(gets_in(&b).await);
            // Free slots go at once when made smaller.
            drop(held);
            b.resize(1);
            let one = b.take().await;
            assert!(!gets_in(&b).await);
            drop(one);
        });
    }
}
