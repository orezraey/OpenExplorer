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

/// What batches save, against the stand-in with each answer 30 ms late (a round trip):
/// requests and milliseconds, item by item as the app went (the "before", still what the
/// one-item calls do) and in batches. Mean of 3 runs, counts of the last.
/// Run with `cargo test -p openexplorer-dropbox --lib batch_costs -- --ignored --nocapture`.
#[test]
#[ignore = "a benchmark: run it by name"]
fn batch_costs() {
    const RTT: u64 = 30;
    const RUNS: usize = 3;
    let dir = std::env::temp_dir().join(format!("oe-dropbox-batch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let files: Vec<std::path::PathBuf> = (0..200)
        .map(|i| {
            let f = dir.join(format!("foto_{i:03}.jpg"));
            std::fs::write(&f, vec![(i % 251) as u8; 20_000 + i * 37]).unwrap();
            f
        })
        .collect();
    let row = |what: &str, before: &[(u64, u64, f64)], after: &[(u64, u64, f64)]| {
        let (b, a) = (mean(before), mean(after));
        println!("{what:<44} | {:>4} req {:>6.0} ms | {:>4} req {:>6.0} ms", b.0, b.2, a.0, a.2);
    };
    println!("{:<44} | before (one by one)  | after (batches)", "");

    // 200 small files into a folder. Before: four at a time (Uploads simultâneos), each asked
    // about first (is the same file there?) and sent in its own request, as the panes did.
    let (mut before, mut after) = (Vec::new(), Vec::new());
    for _ in 0..RUNS {
        for batched in [false, true] {
            let fake = Fake::start();
            fake.make_dir("/Destino");
            let a = std::sync::Arc::new(fake.account());
            a.list("/Destino").unwrap();
            a.forget_fresh();
            fake.delay(RTT);
            let next = std::sync::atomic::AtomicUsize::new(0);
            let m = measure(&a, || {
                if batched {
                    let pairs: Vec<(String, std::path::PathBuf)> = files.iter().map(|f| (format!("/Destino/{}", f.file_name().unwrap().to_string_lossy()), f.clone())).collect();
                    assert!(a.upload_many(&pairs, &mut |_, _| true).iter().all(Result::is_ok));
                    return;
                }
                std::thread::scope(|s| {
                    for _ in 0..4 {
                        s.spawn(|| {
                            while let Some(f) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                                let to = format!("/Destino/{}", f.file_name().unwrap().to_string_lossy());
                                // Asked about first, as the panes did then (none is there).
                                if a.stat(&to).unwrap().is_some() {
                                    continue;
                                }
                                let mut src = std::fs::File::open(f).unwrap();
                                let len = src.metadata().unwrap().len();
                                a.upload(&to, &mut src, len, &mut |_| true).unwrap();
                            }
                        });
                    }
                })
            });
            if batched { after.push(m) } else { before.push(m) }
        }
    }
    row("200 small files (20-27 KB) sent", &before, &after);

    // 50 items moved, then the 50 copied back, one call each in turn (Recortar and Colar).
    let (mut mb, mut ma, mut cb, mut ca) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for _ in 0..RUNS {
        for batched in [false, true] {
            let fake = Fake::start();
            for i in 0..50 {
                fake.put_file(&format!("/Origem/item_{i:02}.txt"), b"x");
            }
            fake.make_dir("/Movidos");
            let a = fake.account();
            fake.delay(RTT);
            let moves: Vec<(String, String)> = (0..50).map(|i| (format!("/Origem/item_{i:02}.txt"), format!("/Movidos/item_{i:02}.txt"))).collect();
            let copies: Vec<(String, String)> = moves.iter().map(|(f, t)| (t.clone(), f.clone())).collect();
            let m = measure(&a, || match batched {
                true => assert!(a.move_many(&moves).iter().all(Result::is_ok)),
                false => moves.iter().for_each(|(f, t)| drop(a.rename(f, t).unwrap())),
            });
            let c = measure(&a, || match batched {
                true => assert!(a.copy_many(&copies).iter().all(Result::is_ok)),
                false => copies.iter().for_each(|(f, t)| drop(a.copy(f, t).unwrap())),
            });
            if batched {
                ma.push(m);
                ca.push(c);
            } else {
                mb.push(m);
                cb.push(c);
            }
        }
    }
    row("50 items moved", &mb, &ma);
    row("50 items copied", &cb, &ca);

    // A 40 MiB file: before, one streamed request (which a 429 lost); now 8 MiB blocks.
    let big: Vec<u8> = (0..40usize << 20).map(|i| (i % 253) as u8).collect();
    let (mut bb, mut ba) = (Vec::new(), Vec::new());
    for _ in 0..RUNS {
        let fake = Fake::start();
        let a = fake.account();
        fake.delay(RTT);
        let commit = serde_json::json!({"path": "/um.bin", "mode": "overwrite", "autorename": false, "mute": true});
        bb.push(measure(&a, || drop(a.api.upload("files/upload", &commit, &mut crate::api::Body::Bytes(&big, None)).unwrap())));
        ba.push(measure(&a, || drop(a.upload("/blocos.bin", &mut &big[..], big.len() as u64, &mut |_| true).unwrap())));
    }
    row("a 40 MiB file sent", &bb, &ba);

    // Undoing a folder's deletion: 200 files and 10 empty folders in it, 20 files deleted from
    // it an hour before. Before: every deleted item asked about and restored one at a time
    // (the empty folders left out); after: a few at once, the empty folders made again.
    let (mut ub, mut ua) = (Vec::new(), Vec::new());
    for run in 0..RUNS * 2 {
        let batched = run % 2 == 1;
        let fake = Fake::start();
        for i in 0..220 {
            fake.put_file(&format!("/Pasta/{}/f{i:03}.txt", i % 5), b"conteudo");
        }
        for i in 0..10 {
            fake.make_dir(&format!("/Pasta/Vazia{i}"));
        }
        let a = fake.account();
        let old: Vec<String> = (200..220).map(|i| format!("/Pasta/{}/f{i:03}.txt", i % 5)).collect();
        assert!(a.delete_many(&old).unwrap().iter().all(Result::is_ok));
        for p in &old {
            fake.backdate(p, 3600);
        }
        let from = a.api.server_now() - 2;
        a.delete("/Pasta").unwrap();
        let to = a.api.server_now() + 1;
        fake.delay(RTT);
        if batched {
            ua.push(measure(&a, || assert_eq!(a.restore_folder("/Pasta", Some((from, to))).unwrap(), 200)));
            assert!(fake.has("/Pasta/Vazia3") && !fake.has(&old[0]));
        } else {
            ub.push(measure(&a, || assert_eq!(old_restore(&a, "/Pasta", from), 200)));
        }
    }
    row("undo of a folder's deletion (200 files)", &ub, &ua);
    let _ = std::fs::remove_dir_all(dir);
}

/// How the folder's undo went before: the folder made, what was deleted in it listed, then
/// each deleted item asked about and each file deleted since `since` restored, one at a time.
fn old_restore(a: &Account, path: &str, since: i64) -> usize {
    use serde_json::{Value, json};
    a.mkdir(path).unwrap();
    let page = a.api.rpc("files/list_folder", &json!({"path": path, "recursive": true, "include_deleted": true, "limit": 2000})).unwrap();
    let deleted: Vec<String> = page["entries"].as_array().unwrap().iter().filter(|e| e[".tag"] == "deleted").map(|e| e["path_display"].as_str().unwrap().to_string()).collect();
    let mut back = 0;
    for p in deleted {
        let Ok(v) = a.api.rpc("files/list_revisions", &json!({"path": p, "mode": "path", "limit": 1})) else { continue };
        let when = v.get("server_deleted").and_then(Value::as_str).and_then(crate::meta::seconds).unwrap_or(0);
        if v["is_deleted"].as_bool() == Some(true) && when >= since {
            a.restore(&p, v["entries"][0]["rev"].as_str().unwrap()).unwrap();
            back += 1;
        }
    }
    back
}
