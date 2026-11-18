//! Asks the running index service, as the app does, and times the round trip.
//! `cargo run -p openexplorer-index --release --example index_ask -- <palavras> [pasta]`
//! (on Windows with `OE_INDEX_CONSOLE=1` when the service runs as `openexplorer index console`;
//! on Linux it starts the service when it isn't running).

fn main() {
    #[cfg(any(windows, target_os = "linux"))]
    {
        use openexplorer_index::proto::Request;
        let mut args = std::env::args().skip(1);
        let text = args.next().unwrap_or_default();
        let req = Request { text, within: args.next(), limit: 100_000, ..Request::default() };
        for _ in 0..3 {
            let t = std::time::Instant::now();
            #[cfg(windows)]
            let answer = openexplorer_index::pipe::ask(&req);
            #[cfg(target_os = "linux")]
            let answer = openexplorer_index::unix::ask(&req);
            match answer {
                Ok(a) => {
                    println!("{} encontrados, {} recebidos em {:.1} ms (busca no serviço {:.1} ms)", a.header.total, a.rows.len(), t.elapsed().as_secs_f64() * 1000.0, a.header.took_us as f64 / 1000.0);
                    for v in &a.header.volumes {
                        println!("  {} {} itens{}{}", v.root, v.files, if v.indexing { " (indexando)" } else { "" }, v.took_ms.map_or(String::new(), |ms| format!(", lido em {:.1} s", ms as f64 / 1000.0)));
                    }
                    for r in a.rows.iter().take(3) {
                        println!("  {}", r.path);
                    }
                }
                Err(e) => println!("erro: {e}"),
            }
        }
    }
}
