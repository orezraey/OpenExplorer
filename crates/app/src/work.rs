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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
}
