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
            assert_eq!(a.list("/fotos/fotos").unwrap().len(), items);
            warm.push(t.elapsed());
        }
        let warm_req = m.lock().requests;
        a.lists.save_if_changed();
        for _ in 0..5 {
            let t = Instant::now();
            let b = Account::new(m.api(), Some(file.clone()));
            assert_eq!(b.list("/fotos/fotos").unwrap().len(), items);
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
