//! What reopening a cached Drive folder costs in requests, bytes and time, against a stand-in
//! for the Drive API on this machine that answers as Drive does (JSON gzipped when asked).
//! Run with `cargo test -p openexplorer-gdrive --offline --lib reopen_costs -- --ignored --nocapture`.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::api::Drive;

/// Pseudo-random characters from `alphabet`, as unique ids and checksums are: they don't
/// compress, so the gzipped sizes aren't flattered.
fn noise(seed: usize, len: usize, alphabet: &[u8]) -> String {
    let mut x = (seed as u64).wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            alphabet[(x % alphabet.len() as u64) as usize] as char
        })
        .collect()
}

/// One file as Drive lists it: a typical photo, every field the app asks for.
fn file(i: usize) -> serde_json::Value {
    const ID: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    serde_json::json!({
        "id": format!("1{}", noise(i, 32, ID)),
        "name": format!("IMG_2024{:02}{:02}_{:06}.jpg", 1 + i % 12, 1 + i % 28, (i * 7919) % 1_000_000),
        "mimeType": "image/jpeg",
        "starred": false,
        "modifiedTime": format!("2024-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z", 1 + i % 12, 1 + i % 28, i % 24, i % 60, (i * 7) % 60, (i * 13) % 1000),
        "md5Checksum": noise(i + 1_000_000, 32, b"0123456789abcdef"),
        "size": format!("{}", 1_000_000 + (i * 104_729) % 5_000_000),
    })
}

#[derive(Default)]
struct Count {
    requests: AtomicUsize,
    wire: AtomicUsize,
    json: AtomicUsize,
}

/// Serves a folder of `items` files (1,000 a page), the change log (nothing new) and My Drive's
/// id, counting each answer.
fn serve(items: usize) -> (Drive, Arc<Count>, Arc<Mutex<bool>>) {
    let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.local_addr().unwrap());
    let count = Arc::new(Count::default());
    let stop = Arc::new(Mutex::new(false));
    let (c, st) = (count.clone(), stop.clone());
    std::thread::spawn(move || {
        for conn in server.incoming() {
            if *st.lock().unwrap() {
                return;
            }
            let mut s = BufReader::new(conn.unwrap());
            let mut line = String::new();
            s.read_line(&mut line).unwrap();
            let mut gzip = false;
            loop {
                let mut h = String::new();
                s.read_line(&mut h).unwrap();
                if h.trim().is_empty() {
                    break;
                }
                gzip |= h.to_ascii_lowercase().starts_with("accept-encoding:") && h.contains("gzip");
            }
            let target = line.split(' ').nth(1).unwrap_or_default();
            let page = target.split(['?', '&']).find_map(|kv| kv.strip_prefix("pageToken=p")).and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
            let body = if target.starts_with("/files/root") {
                serde_json::json!({ "id": "0AMeuDrive" })
            } else if target.starts_with("/changes") {
                serde_json::json!({ "newStartPageToken": "t2", "changes": [] })
            } else {
                let from = page * 1000;
                let to = (from + 1000).min(items);
                let mut v = serde_json::json!({ "files": (from..to).map(file).collect::<Vec<_>>() });
                if to < items {
                    v["nextPageToken"] = format!("p{}", page + 1).into();
                }
                v
            };
            // Drive answers pretty-printed JSON.
            let json = serde_json::to_vec_pretty(&body).unwrap();
            let (bytes, enc) = if gzip {
                let mut z = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                z.write_all(&json).unwrap();
                (z.finish().unwrap(), "Content-Encoding: gzip\r\n")
            } else {
                (json.clone(), "")
            };
            c.requests.fetch_add(1, Ordering::Relaxed);
            c.json.fetch_add(json.len(), Ordering::Relaxed);
            c.wire.fetch_add(bytes.len(), Ordering::Relaxed);
            let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{enc}Content-Length: {}\r\nConnection: close\r\n\r\n", bytes.len());
            let out = s.get_mut();
            out.write_all(head.as_bytes()).unwrap();
            out.write_all(&bytes).unwrap();
            let _ = out.read(&mut [0; 0]);
        }
    });
    let mut d = Drive::for_tests(String::new());
    d.api_url = base;
    (d, count, stop)
}

fn measure(items: usize, run: impl Fn(&Drive)) -> (usize, usize, usize, f64) {
    const RUNS: usize = 5;
    let (d, count, stop) = serve(items);
    // Once to warm up; then the average of a few.
    run(&d);
    let before = (count.requests.load(Ordering::Relaxed), count.wire.load(Ordering::Relaxed), count.json.load(Ordering::Relaxed));
    let started = Instant::now();
    for _ in 0..RUNS {
        run(&d);
    }
    let ms = started.elapsed().as_secs_f64() * 1000.0 / RUNS as f64;
    *stop.lock().unwrap() = true;
    let _ = std::net::TcpStream::connect(d.api_url.trim_start_matches("http://"));
    let per = |now: usize, then: usize| (now - then) / RUNS;
    (per(count.requests.load(Ordering::Relaxed), before.0), per(count.wire.load(Ordering::Relaxed), before.1), per(count.json.load(Ordering::Relaxed), before.2), ms)
}

#[test]
#[ignore]
fn reopen_costs() {
    println!("itens | caso | pedidos | bytes (gzip) | bytes (JSON) | ms");
    for items in [10, 1_000, 10_000] {
        // Today: the whole folder again, page by page, each page parsed into files.
        let list = measure(items, |d| {
            let mut n = 0;
            d.list("pasta", None, |files, _| {
                n += files.len();
                true
            })
            .unwrap();
            assert_eq!(n, items);
        });
        // Proven unchanged: one read of the change log, no more pages than the listing's.
        let pages = items.div_ceil(1000).max(1);
        let check = measure(items, |d| {
            let (changes, _) = d.changes_within("t1", pages).unwrap().unwrap();
            assert!(changes.is_empty());
        });
        for (case, (req, wire, json, ms)) in [("antes: listagem inteira", list), ("depois, sem mudanças: log de mudanças", check)] {
            println!("{items} | {case} | {req} | {wire} | {json} | {ms:.2}");
        }
    }
}
