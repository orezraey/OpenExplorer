//! What the cloud account crates share: each keeps its account's tree or listings on disk
//! between runs, sealed ([`sealed`], with the compact body MEGA's and B2's use in [`varint`]),
//! runs work on several items at once ([`each`]) and reads the files it sends a part at a time
//! ([`fill`]). Each crate keeps its own folder, file names, body and key; only the plumbing is
//! here, so a fix to it reaches all of them.

pub mod sealed;
pub mod varint;

use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Runs `work` on each of `items`, `at_once` at a time, and gives back each result in the
/// items' order. With one at a time (or one item) it runs on this thread.
pub fn each<T: Sync, R: Send>(items: &[T], at_once: usize, work: impl Fn(&T) -> R + Sync) -> Vec<R> {
    each_until(items, at_once, work, |_| false).into_iter().map(|r| r.expect("every item ran")).collect()
}

/// [`each`], but no new item starts once a result `stop` picks came back: those not started are
/// `None`. What was already running ends and is kept.
pub fn each_until<T: Sync, R: Send>(items: &[T], at_once: usize, work: impl Fn(&T) -> R + Sync, stop: impl Fn(&R) -> bool + Sync) -> Vec<Option<R>> {
    let workers = at_once.min(items.len());
    if workers <= 1 {
        let mut out: Vec<Option<R>> = Vec::with_capacity(items.len());
        for item in items {
            let r = work(item);
            let stopped = stop(&r);
            out.push(Some(r));
            if stopped {
                break;
            }
        }
        out.resize_with(items.len(), || None);
        return out;
    }
    let next = AtomicUsize::new(0);
    let stopped = AtomicBool::new(false);
    let out: Mutex<Vec<Option<R>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while !stopped.load(Ordering::SeqCst) {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(item) = items.get(i) else { break };
                    let r = work(item);
                    stopped.fetch_or(stop(&r), Ordering::SeqCst);
                    out.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(r);
                }
            });
        }
    });
    out.into_inner().unwrap_or_else(|e| e.into_inner())
}

/// Reads from `src` until `buf` is full or the source ends: how many bytes it holds. A read
/// cut by a signal is tried again.
pub fn fill(src: &mut dyn Read, buf: &mut [u8]) -> std::io::Result<usize> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_keeps_the_order_and_runs_everything() {
        let items: Vec<u32> = (0..50).collect();
        assert_eq!(each(&items, 4, |i| i * 2), items.iter().map(|i| i * 2).collect::<Vec<_>>());
        assert_eq!(each(&items, 1, |i| i + 1).len(), 50);
        assert!(each(&[] as &[u32], 4, |i| *i).is_empty());
    }

    #[test]
    fn each_until_starts_nothing_after_a_stop() {
        let items: Vec<u32> = (0..50).collect();
        // One at a time: everything after the stop is left alone.
        let got = each_until(&items, 1, |i| *i, |r| *r == 10);
        assert_eq!(got.iter().flatten().count(), 11);
        assert!(got[11..].iter().all(Option::is_none));
        // Side by side: what was running ends, but far fewer than all go.
        let got = each_until(&items, 4, |i| *i, |r| *r == 2);
        assert!(got[2].is_some() && got.iter().flatten().count() < 50);
    }

    /// A reader that gives a few bytes at a time and is interrupted now and then.
    struct Trickle(Vec<u8>, usize);

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.1 += 1;
            if self.1.is_multiple_of(3) {
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            let n = buf.len().min(self.0.len()).min(2);
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0.drain(..n);
            Ok(n)
        }
    }

    #[test]
    fn fill_reads_until_full_or_the_end() {
        let mut buf = [0u8; 5];
        assert_eq!(fill(&mut Trickle(b"abcdefg".to_vec(), 0), &mut buf).unwrap(), 5);
        assert_eq!(&buf, b"abcde");
        assert_eq!(fill(&mut Trickle(b"xy".to_vec(), 0), &mut buf).unwrap(), 2);
    }
}
