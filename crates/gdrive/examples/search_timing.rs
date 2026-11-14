//! Times a Drive search against the first connected account, and the obvious alternative: listing
//! every folder of the scope and matching names locally. Read-only.
//!
//! `cargo run -p openexplorer-gdrive --release --example search_timing -- <palavras> [pasta na raiz]`
//!
//! Without a folder it searches My Drive and everything; the local matching then lists all of
//! My Drive, stopped after `OE_NAIVE_CAP` seconds (120 by default).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use openexplorer_gdrive::search::Within;
use openexplorer_gdrive::{ClientConfig, Drive, store};

/// Folders listed at once by the local matching, as the drag-out's listing does.
const LISTERS: usize = 6;

fn ours(d: &Drive, text: &str, within: &Within, label: &str) {
    let t = Instant::now();
    let (mut first, mut n) = (None, 0usize);
    d.search(text, within, |page, _| {
        if first.is_none() && !page.is_empty() {
            first = Some(t.elapsed());
        }
        n += page.len();
        true
    })
    .expect("pesquisa");
    let first = first.map_or("-".into(), |f| format!("{:.2} s", f.as_secs_f64()));
    println!("pesquisa  {label:<14} primeiro resultado {first:>8}  total {:>7.2} s  {n} resultados", t.elapsed().as_secs_f64());
}

/// The words of a name, close to how Drive's `name contains` sees them (not exactly: Drive's
/// count can differ a little), lowercased: split at anything not a letter or digit, and where
/// letters meet digits ("ps5a" is "ps", "5", "a"). A typed word must start one of them.
fn name_words(name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut last: Option<bool> = None;
    for c in name.to_lowercase().chars() {
        let class = c.is_alphanumeric().then(|| c.is_numeric());
        match class {
            Some(digit) if last == Some(digit) => out.last_mut().unwrap().push(c),
            Some(_) => out.push(c.to_string()),
            None => {}
        }
        last = class;
    }
    out
}

/// Lists the tree under `root` (LISTERS folders at a time) and keeps names with a word starting
/// with each typed one.
fn naive(d: &Drive, text: &str, root: &str, label: &str, cap: Duration) {
    let words: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
    let t = Instant::now();
    let queue = Mutex::new((VecDeque::from([root.to_string()]), 0usize));
    let ready = Condvar::new();
    let (found, seen) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let first = Mutex::new(None);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..LISTERS {
            s.spawn(|| loop {
                let folder = {
                    let mut q = queue.lock().unwrap();
                    loop {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        if let Some(f) = q.0.pop_front() {
                            q.1 += 1;
                            break f;
                        }
                        if q.1 == 0 {
                            ready.notify_all();
                            return;
                        }
                        q = ready.wait_timeout(q, Duration::from_millis(200)).unwrap().0;
                    }
                };
                let _ = d.list(&folder, None, |page, _| {
                    let mut sub = Vec::new();
                    for f in page {
                        seen.fetch_add(1, Ordering::Relaxed);
                        if std::env::var_os("OE_SHOW").is_some() && seen.load(Ordering::Relaxed) % 25 == 0 {
                            println!("  {}", f.name);
                        }
                        let parts = name_words(&f.name);
                        if words.iter().all(|w| parts.iter().any(|p| p.starts_with(w.as_str()))) {
                            found.fetch_add(1, Ordering::Relaxed);
                            first.lock().unwrap().get_or_insert(t.elapsed());
                        }
                        if f.is_dir {
                            sub.push(f.id);
                        }
                    }
                    queue.lock().unwrap().0.extend(sub);
                    ready.notify_all();
                    if t.elapsed() > cap {
                        stop.store(true, Ordering::Relaxed);
                    }
                    !stop.load(Ordering::Relaxed)
                });
                queue.lock().unwrap().1 -= 1;
                ready.notify_all();
            });
        }
    });
    let first = first.into_inner().unwrap().map_or("-".into(), |f| format!("{:.2} s", f.as_secs_f64()));
    let capped = if stop.load(Ordering::Relaxed) { " (parou no limite)" } else { "" };
    println!(
        "listando  {label:<14} primeiro resultado {first:>8}  total {:>7.2} s  {} resultados em {} itens{capped}",
        t.elapsed().as_secs_f64(),
        found.load(Ordering::Relaxed),
        seen.load(Ordering::Relaxed)
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let text = args.next().expect("uso: search_timing <palavras> [pasta na raiz]");
    let folder = args.next();
    let cap = Duration::from_secs(std::env::var("OE_NAIVE_CAP").ok().and_then(|v| v.parse().ok()).unwrap_or(120));
    let cfg = ClientConfig::load().expect("cliente do Google");
    let account = store::accounts().into_iter().next().expect("uma conta conectada");
    let d = Drive::new(cfg, store::refresh_token(&account.email).expect("token da conta"));
    match folder {
        Some(name) => {
            let mut root = Vec::new();
            d.list("root", None, |page, _| {
                root.extend(page);
                true
            })
            .expect("listar a raiz");
            let f = root.into_iter().find(|f| f.is_dir && f.name == name).expect("pasta na raiz");
            ours(&d, &text, &Within::Folder { id: f.id.clone(), shared_drive: None }, &name);
            naive(&d, &text, &f.id, &name, cap);
        }
        None => {
            ours(&d, &text, &Within::MyDrive, "Meu Drive");
            ours(&d, &text, &Within::Everything, "todo o Drive");
            naive(&d, &text, "root", "Meu Drive", cap);
        }
    }
}
