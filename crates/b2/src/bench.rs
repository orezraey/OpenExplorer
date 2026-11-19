//! What the listing cache and the parallel parts save, against the stand-in for B2 on this
//! machine ([`crate::mock`]), which answers each request after a set delay (a round trip to B2)
//! and reads each upload connection at a set speed.
//!
//! `cargo test -p openexplorer-b2 --release --lib bench -- --ignored --nocapture --test-threads=1`

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::account::Account;
use crate::cache;
use crate::mock::Mock;
use crate::upload::Uploads;

/// A round trip to B2 from a home connection in Brazil, roughly.
const LATENCY_MS: u64 = 150;

fn folder(m: &Mock, items: usize) {
    let mut st = m.lock();
    for i in 0..items {
        let name = format!("fotos/IMG_{:06}_{:04}.jpg", (i * 7919) % 1_000_000, i);
        let id = st.add("bkt_fotos", &name, b"");
        let v = st.versions.iter_mut().find(|v| v.id == id).unwrap();
        v.data = std::sync::Arc::new(Vec::new());
    }
}

fn mean(runs: &[Duration]) -> f64 {
    runs.iter().map(Duration::as_secs_f64).sum::<f64>() / runs.len() as f64 * 1000.0
}

/// Opening a folder of `items` files: read from B2 each time (no cache), from the memory kept
/// this run, and from the disk after a restart.
#[test]
#[ignore]
fn bench_reopen() {
    println!("\nReopening a folder, B2 answering after {LATENCY_MS} ms; mean of 5\n");
    println!("{:>7} | {:<34} | {:<22} | {:<22}", "items", "no cache: requests / Class C / ms", "memory: requests / ms", "disk: requests / ms");
    for items in [10, 1_000, 10_000, 25_000] {
        let m = Mock::start();
        folder(&m, items);
        m.faults.latency_ms.store(LATENCY_MS, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("oe-b2-bench-{}-{items}", std::process::id()));
        let file = cache::file_in(&dir, crate::mock::KEY_ID);
        let a = Account::new(m.api(), Some(file.clone()));
        a.buckets_fresh().unwrap();
        a.api.auth().unwrap();
        let (mut cold, mut warm, mut disk) = (Vec::new(), Vec::new(), Vec::new());
        m.reset_counts();
        for _ in 0..5 {
            let t = Instant::now();
            assert_eq!(a.list_fresh("/fotos/fotos").unwrap().len(), items);
            cold.push(t.elapsed());
        }
        let (cold_req, listed) = { let st = m.lock(); (st.requests / 5, st.listed / 5) };
        m.reset_counts();
        for _ in 0..5 {
            let t = Instant::now();
            assert_eq!(a.cached("/fotos/fotos").unwrap().items.len(), items);
            warm.push(t.elapsed());
        }
        let warm_req = m.lock().requests;
        a.lists.save_if_changed();
        for _ in 0..5 {
            let t = Instant::now();
            let b = Account::new(m.api(), Some(file.clone()));
            assert_eq!(b.cached("/fotos/fotos").unwrap().items.len(), items);
            disk.push(t.elapsed());
        }
        let disk_req = m.lock().requests;
        let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
        println!(
            "{items:>7} | {:>2} / {:>3} / {:>9.1} ms             | {} / {:>8.2} ms         | {} / {:>8.2} ms ({} KB file)",
            cold_req,
            listed.div_ceil(1000).max(1),
            mean(&cold),
            warm_req,
            mean(&warm),
            disk_req,
            mean(&disk),
            size / 1024
        );
        let _ = std::fs::remove_dir_all(dir);
    }
    // maxFileCount: what a high count saves on a big folder (requests; billing is per name).
    let m = Mock::start();
    folder(&m, 10_000);
    m.faults.latency_ms.store(LATENCY_MS, Ordering::SeqCst);
    let a = m.account();
    a.buckets_fresh().unwrap();
    let bucket = a.bucket("fotos").unwrap();
    for page in [1_000u32, crate::account::PAGE] {
        m.reset_counts();
        let t = Instant::now();
        let mut start: Option<String> = None;
        let mut got = 0;
        loop {
            let mut body = serde_json::json!({"bucketId": bucket.id, "prefix": "fotos/", "delimiter": "/", "maxFileCount": page});
            if let Some(s) = &start {
                body["startFileName"] = s.clone().into();
            }
            let v = a.api.run("b2_list_file_names", &body).unwrap();
            got += v["files"].as_array().unwrap().len();
            match v["nextFileName"].as_str() {
                Some(n) => start = Some(n.to_string()),
                None => break,
            }
        }
        assert_eq!(got, 10_000);
        let (req, listed) = { let st = m.lock(); (st.requests, st.listed) };
        println!("10,000 items, maxFileCount {page:>5}: {req:>2} requests, {} Class C transactions, {:.0} ms", listed.div_ceil(1000), t.elapsed().as_secs_f64() * 1000.0);
    }
}

/// Uploading a 32 MiB file with each connection read at 8 MB/s: in one request, in parts one at
/// a time, in parts side by side.
#[test]
#[ignore]
fn bench_upload() {
    let len = 32 << 20;
    let data: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
    let speed = 8_000_000u64;
    println!("\nUploading 32 MiB, each connection read at {} MB/s, B2 answering after {LATENCY_MS} ms; mean of 3\n", speed / 1_000_000);
    let run = |label: &str, recommended: u64, parallel: usize| {
        let mut times = Vec::new();
        let mut requests = 0;
        for _ in 0..3 {
            let m = Mock::start();
            {
                let mut st = m.lock();
                st.recommended = recommended;
            }
            m.faults.latency_ms.store(LATENCY_MS, Ordering::SeqCst);
            m.faults.upload_speed.store(speed, Ordering::SeqCst);
            let mut a = m.account();
            a.parallel = parallel;
            a.buckets_fresh().unwrap();
            m.reset_counts();
            let t = Instant::now();
            a.upload(&mut Uploads::default(), "/fotos/grande.bin", &mut &data[..], len as u64, &mut |_| true).unwrap();
            times.push(t.elapsed());
            requests = m.lock().requests;
        }
        let ms = mean(&times);
        println!("{label:<44} {requests:>3} requests, {:>7.0} ms, {:>5.1} MB/s", ms, len as f64 / ms / 1000.0);
    };
    // Up to a part's size (32 MiB at most) a file goes in one request.
    run("one request (b2_upload_file)", 100_000_000, 1);
    run("large file, 8 MiB parts, 1 at a time", 8 << 20, 1);
    run("large file, 8 MiB parts, 4 at a time", 8 << 20, 4);
    run("large file, 5 MiB parts, 4 at a time", 5 << 20, 4);
}

/// Opening a folder the way the app's pane does (`crates/app/src/b2.rs`, `load`): what it shows
/// at once, and what it shows once B2 answered. Gives the items shown last, the ms until the
/// first show and until the last.
fn open_like_the_pane(a: &Account, path: &str) -> (Vec<crate::Item>, f64, f64) {
    let t = Instant::now();
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
    let kept = a.cached(path);
    let first = kept.as_ref().map(|_| ms(t));
    let items = match (a.reread(path).unwrap(), kept) {
        (crate::Fresh::Same, Some(kept)) => kept.items.clone(),
        (crate::Fresh::Same, None) => unreachable!("nothing kept is never the same"),
        (crate::Fresh::Changed(items), _) => items,
    };
    let last = ms(t);
    (items, first.unwrap_or(last), last)
}

/// How soon a change made elsewhere (the website, another computer) shows, and what checking
/// costs: reopening a folder whose kept listing is a minute or ten minutes old, after a file
/// was added elsewhere; and asking about a free name before writing to it.
#[test]
#[ignore]
fn bench_freshness() {
    println!("\nA file added elsewhere, then the folder reopened; B2 answering after {LATENCY_MS} ms\n");
    println!("{:>7} | {:>13} | {:>8} | {:>14} | {:>14} | the new file shows", "items", "listing age", "requests", "first show ms", "last show ms");
    for items in [1_000, 25_000] {
        for age in [60u64, 600] {
            let m = Mock::start();
            folder(&m, items);
            let a = m.account();
            a.buckets_fresh().unwrap();
            a.api.auth().unwrap();
            a.list_fresh("/fotos/fotos").unwrap();
            // The kept listing is `age` seconds old, and a file came in from elsewhere since.
            let kept = a.cached("/fotos/fotos").unwrap();
            a.lists.put("fotos", "fotos/", kept.items.clone(), cache::now() - age);
            m.lock().add("bkt_fotos", "fotos/de-outro-lugar.jpg", b"x");
            m.faults.latency_ms.store(LATENCY_MS, Ordering::SeqCst);
            m.reset_counts();
            let (shown, first, last) = open_like_the_pane(&a, "/fotos/fotos");
            let seen = shown.iter().any(|i| i.name == "de-outro-lugar.jpg");
            println!("{items:>7} | {age:>11} s | {:>8} | {first:>14.1} | {last:>14.1} | {}", m.lock().requests, if seen { "yes" } else { "no" });
        }
    }
    // Before writing: is the name free? (Nova pasta, a copy or move's target.)
    let m = Mock::start();
    folder(&m, 1_000);
    let a = m.account();
    a.buckets_fresh().unwrap();
    a.api.auth().unwrap();
    a.list_fresh("/fotos/fotos").unwrap();
    m.lock().add("bkt_fotos", "fotos/livre.jpg", b"x");
    m.faults.latency_ms.store(LATENCY_MS, Ordering::SeqCst);
    for (label, path) in [("a name taken elsewhere a moment ago", "/fotos/fotos/livre.jpg"), ("a free name", "/fotos/fotos/nada.jpg")] {
        m.reset_counts();
        let t = Instant::now();
        let found = a.stat(path).unwrap().is_some();
        println!("stat, {label:<36}: {} requests, {:>6.1} ms, says {}", m.lock().requests, t.elapsed().as_secs_f64() * 1000.0, if found { "taken" } else { "free" });
    }
    // Moving a file within the bucket, both folders listed a moment before.
    a.list_fresh("/fotos").unwrap();
    m.reset_counts();
    let t = Instant::now();
    a.rename("/fotos/fotos/livre.jpg", "/fotos/movido.jpg").unwrap();
    println!("moving a file in the bucket                : {} requests, {:>6.1} ms", m.lock().requests, t.elapsed().as_secs_f64() * 1000.0);
}

/// Searching a bucket of 200 folders of 20 files: folder by folder, as the servers' search reads
/// any server (one listing a folder), against one walk of every name.
#[test]
#[ignore]
fn bench_search() {
    let m = Mock::start();
    {
        let mut st = m.lock();
        for f in 0..200 {
            for i in 0..20 {
                st.add("bkt_fotos", &format!("album {f:03}/sub/IMG_{i:04}.jpg"), b"");
            }
        }
    }
    let a = m.account();
    a.buckets_fresh().unwrap();
    a.api.auth().unwrap();
    m.faults.latency_ms.store(LATENCY_MS, Ordering::SeqCst);
    println!("\nSearching a bucket of 200 folders (each with a subfolder) of 20 files; B2 answering after {LATENCY_MS} ms\n");
    m.reset_counts();
    let t = Instant::now();
    let mut todo = std::collections::VecDeque::from([String::from("/fotos")]);
    let mut names = 0;
    while let Some(dir) = todo.pop_front() {
        for it in a.list(&dir).unwrap() {
            names += 1;
            if it.is_dir {
                todo.push_back(format!("{dir}/{}", it.name));
            }
        }
    }
    println!("folder by folder: {names} names, {} requests, {:.0} ms", m.lock().requests, t.elapsed().as_secs_f64() * 1000.0);
    m.reset_counts();
    let t = Instant::now();
    let mut files = 0;
    a.walk("/fotos", &mut |page| {
        files += page.len();
        true
    })
    .unwrap();
    println!("one walk:         {files} files, {} requests, {:.0} ms", m.lock().requests, t.elapsed().as_secs_f64() * 1000.0);
}

/// While a pane stays on a folder: how soon a change made elsewhere shows, and what the
/// readings cost an hour.
#[test]
#[ignore]
fn bench_pane_left_open() {
    println!("\nA pane left on a folder, a file added elsewhere; B2 answering after {LATENCY_MS} ms\n");
    for items in [1_000, 25_000] {
        let m = Mock::start();
        folder(&m, items);
        let a = m.account();
        a.buckets_fresh().unwrap();
        a.api.auth().unwrap();
        a.list_fresh("/fotos/fotos").unwrap();
        m.faults.latency_ms.store(LATENCY_MS, Ordering::SeqCst);
        m.reset_counts();
        let t = Instant::now();
        assert_eq!(a.poll("/fotos/fotos"), crate::Polled::Same);
        let same = (m.lock().requests, t.elapsed().as_secs_f64() * 1000.0);
        m.lock().add("bkt_fotos", "fotos/de-outro-lugar.jpg", b"x");
        m.reset_counts();
        let t = Instant::now();
        assert!(matches!(a.poll("/fotos/fotos"), crate::Polled::Changed(_)));
        let changed = (m.lock().requests, t.elapsed().as_secs_f64() * 1000.0);
        let every = crate::watch::every(items).as_secs();
        println!(
            "{items:>7} items: read again every {every} s; unchanged {} requests {:.1} ms, changed {} requests {:.1} ms; shows within {every} s + {:.0} ms; {} requests an hour",
            same.0,
            same.1,
            changed.0,
            changed.1,
            changed.1,
            3600 / every * u64::from(crate::watch::cost(items))
        );
    }
}
