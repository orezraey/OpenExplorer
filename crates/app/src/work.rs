//! A list of work that workers share and add to as they go: a folder's contents join the list
//! once the folder is read, so files start moving from the first seconds instead of after the
//! whole tree is walked.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};

/// The shared list of work, and how many workers are busy (more work may still come from them).
pub struct Queue<W> {
    state: Mutex<(VecDeque<W>, usize)>,
    ready: Condvar,
}

impl<W> Queue<W> {
    pub fn new(first: impl IntoIterator<Item = W>) -> Queue<W> {
        Queue { state: Mutex::new((first.into_iter().collect(), 0)), ready: Condvar::new() }
    }

    /// The next piece of work, waiting while busy workers may still add some; `None` when all
    /// is done. Hold the returned [`Finished`] until the piece is done.
    pub fn next(&self) -> Option<(W, Finished<'_, W>)> {
        let mut s = self.state.lock().unwrap();
        loop {
            if let Some(w) = s.0.pop_front() {
                s.1 += 1;
                return Some((w, Finished(self)));
            }
            if s.1 == 0 {
                return None;
            }
            s = self.ready.wait(s).unwrap();
        }
    }

    /// Up to `max` more queued pieces that `want` picks, taken for a worker to do along with the
    /// piece it holds (and finished with it).
    pub fn take(&self, max: usize, want: impl Fn(&W) -> bool) -> Vec<W> {
        let mut s = self.state.lock().unwrap();
        let mut out = Vec::new();
        let mut i = 0;
        while i < s.0.len() && out.len() < max {
            if want(&s.0[i]) {
                out.extend(s.0.remove(i));
            } else {
                i += 1;
            }
        }
        out
    }

    pub fn add(&self, work: impl IntoIterator<Item = W>) {
        self.state.lock().unwrap().0.extend(work);
        self.ready.notify_all();
    }

    fn finished_one(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.1 -= 1;
        }
        self.ready.notify_all();
    }
}

/// Marks a worker's piece of work finished however it ends, a panic included, so the other
/// workers don't wait forever for work it would have added.
pub struct Finished<'a, W>(&'a Queue<W>);

impl<W> Drop for Finished<'_, W> {
    fn drop(&mut self) {
        self.0.finished_one();
    }
}

/// A shared limit on how many transfers run at the same time across all batches (uploads or
/// downloads), so starting a second batch while one is under way still honours the cap in
/// Opções instead of multiplying it.
pub struct Slots {
    state: Mutex<(usize, usize)>,
    ready: Condvar,
}

impl Slots {
    pub fn new(limit: usize) -> Slots {
        Slots { state: Mutex::new((0, limit.max(1))), ready: Condvar::new() }
    }

    /// Updates the cap (from Opções). Raising it wakes waiting workers at once; lowering it
    /// lets those already running finish before more start.
    pub fn set_limit(&self, limit: usize) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.1 = limit.max(1);
        self.ready.notify_all();
    }

    /// Waits until a slot is free and takes it. Drop the returned [`Slot`] when the work ends.
    pub fn acquire(&self) -> Slot<'_> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while s.0 >= s.1 {
            s = self.ready.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        s.0 += 1;
        Slot(self)
    }

    fn release(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.0 -= 1;
        self.ready.notify_one();
    }
}

/// Releases a slot taken from [`Slots`] however the work ends, a panic included.
pub struct Slot<'a>(&'a Slots);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A worker takes the queued pieces it can do along with its own, in order, as many as it
    /// asks, and the others stay.
    #[test]
    fn taking_more_leaves_the_rest_in_order() {
        let queue = Queue::new([1, 2, 3, 4, 5, 6]);
        let (first, done) = queue.next().unwrap();
        assert_eq!(first, 1);
        assert_eq!(queue.take(2, |n| n % 2 == 0), vec![2, 4]);
        drop(done);
        let rest: Vec<i32> = std::iter::from_fn(|| queue.next().map(|(n, _)| n)).collect();
        assert_eq!(rest, vec![3, 5, 6]);
    }

    /// Workers keep going while a busy one may still add work, and all stop once nothing is
    /// queued or running: every piece, nested ones included, is taken exactly once.
    #[test]
    fn the_queue_ends_only_when_all_work_is_done() {
        // A piece is its depth: 0 adds three 1s and five 2s, each 1 adds five 2s.
        let queue = Queue::new([0u8]);
        let taken = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    while let Some((depth, _finished)) = queue.next() {
                        taken.fetch_add(1, Ordering::Relaxed);
                        if depth < 2 {
                            // A pause first, so the other workers find the list empty while
                            // this one is still busy.
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            if depth == 0 {
                                queue.add([1, 1, 1]);
                            }
                            queue.add([2; 5]);
                        }
                    }
                });
            }
        });
        assert_eq!(taken.load(Ordering::Relaxed), 1 + 3 + 4 * 5);
    }

    /// Two concurrent batches sharing `Slots` never exceed the total cap (e.g. 2 × 4 workers
    /// on 2 × 10 items still run at most 4 at once), and raising the limit wakes waiting workers.
    #[test]
    fn slots_cap_concurrency_across_batches_and_update_live() {
        let slots = Slots::new(4);
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let done = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let (slots, active, peak, done) = (&slots, &active, &peak, &done);
            for _ in 0..2 {
                let queue = Queue::new(0..10);
                scope.spawn(move || {
                    std::thread::scope(|inner| {
                        for _ in 0..4 {
                            inner.spawn(|| {
                                while let Some((_, _finished)) = queue.next() {
                                    let _slot = slots.acquire();
                                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                                    peak.fetch_max(now, Ordering::SeqCst);
                                    std::thread::sleep(std::time::Duration::from_millis(15));
                                    active.fetch_sub(1, Ordering::SeqCst);
                                    done.fetch_add(1, Ordering::SeqCst);
                                }
                            });
                        }
                    });
                });
            }
        });
        assert_eq!(done.load(Ordering::SeqCst), 20);
        assert!(peak.load(Ordering::SeqCst) <= 4, "peak was {}", peak.load(Ordering::SeqCst));
        assert_eq!(peak.load(Ordering::SeqCst), 4);

        // Raising the limit wakes a blocked acquirer.
        let live = Slots::new(1);
        let first = live.acquire();
        let acquired = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let _second = live.acquire();
                acquired.store(1, Ordering::SeqCst);
            });
            std::thread::sleep(std::time::Duration::from_millis(20));
            assert_eq!(acquired.load(Ordering::SeqCst), 0);
            live.set_limit(2);
            std::thread::sleep(std::time::Duration::from_millis(20));
            assert_eq!(acquired.load(Ordering::SeqCst), 1);
            drop(first);
        });
    }
}
