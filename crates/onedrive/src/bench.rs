//! What opening a OneDrive folder costs in requests, bytes and time, with and without the kept
//! listings, against the stand-in for Graph (`fake.rs`), which answers as Graph does for the
//! fields asked (`$select`), 1,000 items a page. Bytes are the JSON bodies, uncompressed.
//! Run with `cargo test -p openexplorer-onedrive --lib bench -- --ignored --nocapture`.

use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::account::{Kind, on};
use crate::fake::{Fake, ROOT};

const RUNS: usize = 5;

fn fill(fake: &Fake, items: usize) -> String {
    fake.with(|st| {
        let dir = st.add(ROOT, "Fotos", true, &[]);
        for i in 0..items {
            st.add(&dir, &format!("IMG_2024{:02}{:02}_{i:06}.jpg", 1 + i % 12, 1 + i % 28), false, &[]);
        }
        dir
    })
}

/// Requests, bytes and milliseconds per run of `run`, after one run to warm up.
fn measure(fake: &Fake, mut run: impl FnMut()) -> (usize, usize, f64) {
    run();
    let (r0, b0) = (fake.requests(), fake.count.bytes.load(Ordering::SeqCst));
    let started = Instant::now();
    for _ in 0..RUNS {
        run();
    }
    let ms = started.elapsed().as_secs_f64() * 1000.0 / RUNS as f64;
    ((fake.requests() - r0) / RUNS, (fake.count.bytes.load(Ordering::SeqCst) - b0) / RUNS, ms)
}

#[test]
#[ignore]
fn bench() {
    println!("itens | caso | pedidos | bytes | ms");
    for items in [10, 1_000, 10_000] {
        let fake = Fake::start();
        let dir = fill(&fake, items);
        let a = on(fake.ends(), Kind::Personal, None).unwrap();
        // Without kept listings: the whole folder, page by page, at every opening.
        let full = measure(&fake, || {
            assert_eq!(a.list_fresh(&dir).len(), items);
        });
        // Kept and unchanged: one delta request, then the listing from memory.
        a.list("/Meus arquivos/Fotos").unwrap();
        let kept = measure(&fake, || {
            assert!(a.catch_up().unwrap().is_empty());
            assert_eq!(a.list("/Meus arquivos/Fotos").unwrap().len(), items);
        });
        // A start: with no tree kept, the position, the top's and the folder's listings; with
        // one on disk, one delta request.
        let dir_path = std::env::temp_dir().join(format!("oe-onedrive-bench-{}-{items}", std::process::id()));
        let keep = || Some((crate::cache::file_in(&dir_path, crate::fake::DRIVE), crate::cache::Secret::of(crate::fake::DRIVE, &[3; 32])));
        let cold = measure(&fake, || {
            let b = on(fake.ends(), Kind::Personal, None).unwrap();
            assert_eq!(b.list("/Meus arquivos/Fotos").unwrap().len(), items);
        });
        {
            let b = on(fake.ends(), Kind::Personal, keep()).unwrap();
            b.list("/Meus arquivos/Fotos").unwrap();
            b.save_cache();
        }
        let warm = measure(&fake, || {
            let b = on(fake.ends(), Kind::Personal, keep()).unwrap();
            assert!(b.started_from_cache());
            assert_eq!(b.list("/Meus arquivos/Fotos").unwrap().len(), items);
            b.forgotten.store(true, Ordering::SeqCst);
        });
        let _ = std::fs::remove_dir_all(&dir_path);
        for (case, (req, bytes, ms)) in [
            ("reabrir: listagem inteira", full),
            ("reabrir: guardada, sem mudanças", kept),
            ("início + abrir: sem árvore guardada", cold),
            ("início + abrir: árvore do disco", warm),
        ] {
            println!("{items} | {case} | {req} | {bytes} | {ms:.2}");
        }
    }
}
