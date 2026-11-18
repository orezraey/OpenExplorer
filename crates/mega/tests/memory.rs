//! Benchmark of the memory the decrypted tree keeps per node, and of the time it takes to read
//! a listing and to list and resolve a big folder; ignored unless asked for:
//!
//! ```text
//! cargo test -p openexplorer-mega --release --test memory -- --ignored --nocapture --test-threads 1
//! ```
//!
//! The listing is shaped like a real account's `f` answer: folders nested a few levels deep,
//! nine files for each folder, names of real lengths, files with a fingerprint (`c`), 32-byte
//! file keys and 16-byte folder keys under the master key, and one folder of many files. The
//! bytes are counted by this binary's own allocator: what the tree holds once the listing is
//! gone, and the most held at once while reading it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::time::Instant;

use openexplorer_mega::crypto::{attr_key, b64, ecb_encrypt, encrypt_attrs, fingerprint, merge_file_key};
use openexplorer_mega::nodes::{Keys, Tree};
use serde_json::{Map, Value, json};

struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);
static BLOCKS: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grow(layout.size() as isize, 1);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        grow(-(layout.size() as isize), -1);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            grow(new_size as isize - layout.size() as isize, 0);
        }
        p
    }
}

fn grow(bytes: isize, blocks: isize) {
    let now = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    BLOCKS.fetch_add(blocks, Ordering::Relaxed);
    PEAK.fetch_max(now.max(0) as usize, Ordering::Relaxed);
}

#[global_allocator]
static COUNTING: Counting = Counting;

fn live() -> (isize, isize) {
    (LIVE.load(Ordering::Relaxed), BLOCKS.load(Ordering::Relaxed))
}

/// A small fixed-seed generator, so every run builds the same listing.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn bytes<const N: usize>(&mut self) -> [u8; N] {
        std::array::from_fn(|_| self.next() as u8)
    }
}

const USER: &str = "USERHANDLE0";

/// Node `i`'s handle: 6 bytes in base64, as MEGA's are.
fn handle(i: usize) -> String {
    b64(&(i as u64).to_le_bytes()[..6])
}

const WORDS: [&str; 12] = ["Relatório", "fotos", "Projeto", "backup", "contrato", "Viagem", "notas", "orçamento", "Música", "final", "cópia", "planilha"];
const EXTS: [&str; 8] = [".jpg", ".pdf", ".docx", ".mp4", ".xlsx", ".png", ".txt", ".zip"];

fn file_name(r: &mut Rng, i: usize) -> String {
    match r.below(3) {
        0 => format!("IMG_2024{:02}{:02}_{:06}.jpg", 1 + r.below(12), 1 + r.below(28), i % 1_000_000),
        1 => format!("{} {} {i}{}", WORDS[r.below(12)], WORDS[r.below(12)], EXTS[r.below(8)]),
        _ => format!("{} - {} {} (versão {}){}", WORDS[r.below(12)], WORDS[r.below(12)], WORDS[r.below(12)], i % 97, EXTS[r.below(8)]),
    }
}

fn folder_name(r: &mut Rng, i: usize) -> String {
    format!("{} {} {i}", WORDS[r.below(12)], WORDS[r.below(12)])
}

fn raw(r: &mut Rng, master: &[u8; 16], h: &str, p: &str, file: bool, name: String) -> Value {
    let key: Vec<u8> = if file { merge_file_key(&r.bytes::<16>(), &r.bytes::<8>(), &r.bytes::<8>()).to_vec() } else { r.bytes::<16>().to_vec() };
    let mut attrs = Map::new();
    attrs.insert("n".into(), name.into());
    if file {
        attrs.insert("c".into(), fingerprint(&r.bytes::<16>(), 1_700_000_000 + r.below(50_000_000) as i64).into());
    }
    let a = encrypt_attrs(&attr_key(&key), &attrs);
    let mut k = key;
    ecb_encrypt(master, &mut k);
    json!({"h": h, "p": p, "t": if file { 0 } else { 1 }, "a": a, "k": format!("{USER}:{}", b64(&k)), "u": USER, "ts": 1_600_000_000 + r.below(100_000_000), "s": if file { r.below(10_000_000) } else { 0 }})
}

/// A listing of about `n` nodes, the big folder's handle and its items' names.
fn listing(n: usize, big: usize, master: &[u8; 16]) -> (Value, String, Vec<String>) {
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let root = handle(0);
    let mut f = vec![json!({"h": root, "p": "", "t": 2, "a": "", "u": USER, "ts": 1}), json!({"h": handle(1), "p": "", "t": 4, "a": "", "u": USER, "ts": 1})];
    let mut next = 2;
    let mut folders = vec![root.clone()];
    let rest = n - big - 1;
    // One folder in ten; each hangs from an earlier folder, the first ones from the root.
    for _ in 0..rest / 10 {
        let p = if folders.len() < 20 { root.clone() } else { folders[folders.len() / 2 + r.below(folders.len() / 2)].clone() };
        let h = handle(next);
        let name = folder_name(&mut r, next);
        f.push(raw(&mut r, master, &h, &p, false, name));
        folders.push(h);
        next += 1;
    }
    while next < rest + 2 {
        let p = folders[r.below(folders.len())].clone();
        let name = file_name(&mut r, next);
        f.push(raw(&mut r, master, &handle(next), &p, true, name));
        next += 1;
    }
    let big_h = handle(next);
    f.push(raw(&mut r, master, &big_h, &root, false, "Muitos arquivos".into()));
    next += 1;
    let mut names = Vec::with_capacity(big);
    for i in 0..big {
        let name = format!("IMG_{i:07}.jpg");
        f.push(raw(&mut r, master, &handle(next), &big_h, true, name.clone()));
        names.push(name);
        next += 1;
    }
    (json!({"f": f, "ok": [], "sn": "SN000000001"}), big_h, names)
}

fn measure(n: usize, big: usize) {
    let master = [7u8; 16];
    let (l, big_h, names) = listing(n, big, &master);
    let nodes = l["f"].as_array().map_or(0, Vec::len);
    let mut keys = Keys { user: USER.into(), master, rsa: None, shares: HashMap::new() };

    let (before, blocks_before) = live();
    PEAK.store(before.max(0) as usize, Ordering::Relaxed);
    let started = Instant::now();
    let tree = Tree::from_listing(&mut keys, &l);
    let built = started.elapsed();
    let peak = PEAK.load(Ordering::Relaxed) as isize - before;
    let (after, blocks_after) = live();
    let (bytes, blocks) = (after - before, blocks_after - blocks_before);
    drop(l);

    let started = Instant::now();
    let listed = tree.children(&big_h).len();
    let listing_time = started.elapsed();
    let started = Instant::now();
    for name in &names {
        assert!(tree.child_named(&big_h, name).is_some());
    }
    let resolving = started.elapsed();
    assert_eq!(listed, big);
    assert!(tree.get(&handle(0)).is_some());

    eprintln!(
        "{nodes} nodes: tree {:.1} MB ({} bytes/node, {:.1} allocations/node), {:.1} MB more at the peak of reading; read in {} ms; {big}-item folder listed in {} ms, every item resolved in {} ms",
        bytes as f64 / 1e6,
        bytes / nodes as isize,
        blocks as f64 / nodes as f64,
        peak as f64 / 1e6,
        built.as_millis(),
        listing_time.as_millis(),
        resolving.as_millis(),
    );
    drop(tree);
}

#[test]
#[ignore = "benchmark"]
fn tree_memory_100_thousand() {
    measure(100_000, 5_000);
}

#[test]
#[ignore = "benchmark"]
fn tree_memory_1_million() {
    measure(1_000_000, 50_000);
}
