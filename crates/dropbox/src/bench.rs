//! What opening a Dropbox folder costs, with and without the kept listings, against the
//! stand-in for the API on this machine (`fake.rs`): requests, bytes of JSON received, time.
//! Run with `cargo test -p openexplorer-dropbox --lib reopen_costs -- --ignored --nocapture`.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::account::Account;
use crate::fake::Fake;

/// Requests, JSON bytes and milliseconds that `work` took on `a`.
fn measure(a: &Account, work: impl FnOnce()) -> (u64, u64, f64) {
    let (r, b) = (a.api.requests.load(Ordering::Relaxed), a.api.received.load(Ordering::Relaxed));
    let t = Instant::now();
    work();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    (a.api.requests.load(Ordering::Relaxed) - r, a.api.received.load(Ordering::Relaxed) - b, ms)
}

fn mean(runs: &[(u64, u64, f64)]) -> (u64, u64, f64) {
    // Counts of the last run (the first also takes the log's cursor); time averaged.
    let n = runs.len() as f64;
    let last = runs[runs.len() - 1];
    (last.0, last.1, runs.iter().map(|r| r.2).sum::<f64>() / n)
}

#[test]
#[ignore = "a benchmark: run it by name"]
fn reopen_costs() {
    println!("items  | read whole (no cache): req / JSON bytes / ms | reopen, log read: req / bytes / ms | reopen, watcher waiting | after a restart (disk)");
    for items in [10usize, 1_000, 10_000] {
        let fake = Fake::start();
        fake.put_many("/Fotos", items);
        let dir = std::env::temp_dir().join(format!("oe-dropbox-bench-{}-{items}", std::process::id()));
        let file = crate::cache::file_in(&dir, "ana@exemplo.com");
        let (mut whole, mut log, mut watched, mut restart, mut opened) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for _ in 0..5 {
            let a = fake.account_kept(&file);
            // Today's cost of every open without a cache: the whole folder.
            whole.push(measure(&a, || assert_eq!(a.reload("/Fotos").unwrap().len(), items)));
            // Reopened, nothing changed since the log was last read: the log read from its cursor.
            a.catch_up().unwrap();
            a.forget_fresh();
            log.push(measure(&a, || assert_eq!(a.list("/Fotos").unwrap().len(), items)));
            // Reopened while the longpoll waits: nothing asked.
            let w = std::sync::Arc::new(a);
            let waiter = w.clone();
            let poll = std::thread::spawn(move || waiter.watch(30));
            std::thread::sleep(Duration::from_millis(200));
            w.forget_fresh();
            watched.push(measure(&w, || assert_eq!(w.list("/Fotos").unwrap().len(), items)));
            w.save_cache();
            fake.put_file("/outra/acorda.txt", b"x");
            poll.join().unwrap().unwrap();
            // A new run: the kept file (opened with the account), then the log since.
            let t = Instant::now();
            let b = fake.account_kept(&file);
            opened.push(t.elapsed().as_secs_f64() * 1000.0);
            restart.push(measure(&b, || assert_eq!(b.list("/Fotos").unwrap().len(), items)));
        }
        let f = |(r, b, ms): (u64, u64, f64)| format!("{r} / {b} / {ms:.1}");
        let open_ms = opened.iter().sum::<f64>() / opened.len() as f64;
        println!("{items:>6} | {} | {} | {} | {} (+{open_ms:.1} ms opening: who the account is, the sealed file)", f(mean(&whole)), f(mean(&log)), f(mean(&watched)), f(mean(&restart)));
        let _ = std::fs::remove_dir_all(dir);
    }
}
