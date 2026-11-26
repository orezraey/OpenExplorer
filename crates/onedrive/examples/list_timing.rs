//! How long each page of a OneDrive folder's listing takes to come, read twice (the second
//! time from what the first kept, if anything).
//!
//! `cargo run -p openexplorer-onedrive --release --example list_timing -- <drive id> <path>`

use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let id = args.next().expect("the drive's id, as onedrive-accounts.json names it");
    let path = args.next().expect("the folder's path, e.g. \"/Meus arquivos/Fotos\"");
    let account = openexplorer_onedrive::reach(&id).unwrap_or_else(|e| panic!("reaching the account: {e}"));
    for round in 1..=2 {
        let started = Instant::now();
        let mut total = 0;
        account
            .list_paged(&path, &mut |items, more| {
                total += items.len();
                println!("  round {round}: {} items ({} .. {}) at {:.3} s{}", items.len(), items.first().map_or("", |i| i.name.as_str()), items.last().map_or("", |i| i.name.as_str()), started.elapsed().as_secs_f64(), if more { "" } else { " (last)" });
                true
            })
            .unwrap_or_else(|e| panic!("listing: {e}"));
        println!("round {round}: {total} items in {:.3} s", started.elapsed().as_secs_f64());
    }
}
