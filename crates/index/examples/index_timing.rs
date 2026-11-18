//! How fast the index finds things on this computer's disks, built by walking them (no admin
//! needed; the service reads the MFT instead, with the same search).
//!
//! `cargo run -p openexplorer-index --release --example index_timing -- C:\ D:\`

use std::time::Instant;

use openexplorer_index::tree::{Query, Tree};
use openexplorer_index::walk;

const QUERIES: [&str; 6] = ["a", "pdf", "relatorio", "windows system32", ".dll", "xyzzy"];

fn main() {
    let roots: Vec<String> = std::env::args().skip(1).collect();
    let t = Instant::now();
    let trees: Vec<Tree> = roots.iter().map(|r| walk::walk(r)).collect();
    let files: usize = trees.iter().map(Tree::len).sum();
    let memory: usize = trees.iter().map(Tree::memory).sum();
    println!("índice: {files} itens em {:.1} s, {} MB", t.elapsed().as_secs_f64(), memory >> 20);
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    for q in QUERIES {
        let query = Query::new(q).unwrap();
        for threads in [1, cores] {
            let mut best = f64::MAX;
            let mut total = 0;
            for _ in 0..5 {
                let t = Instant::now();
                total = 0;
                let mut rows = 0;
                for tree in &trees {
                    let mut out = Vec::new();
                    total += tree.search(&query, None, false, 1000, threads, &mut out);
                    rows += out.iter().filter_map(|r| tree.row(*r)).count();
                }
                assert!(rows <= 1000 * trees.len());
                best = best.min(t.elapsed().as_secs_f64() * 1000.0);
            }
            println!("{q:<18} {threads:>2} threads  {best:>7.1} ms  {total} encontrados");
        }
    }
}
