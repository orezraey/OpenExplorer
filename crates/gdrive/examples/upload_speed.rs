//! Measures upload speed to Google Drive: one file alone, and several at once.
//!
//! Uploads a test file several times into a new folder "OpenExplorer - teste de velocidade" in
//! the first connected account's My Drive, prints each run's speed, then moves that folder to
//! the Drive trash.
//!
//! `cargo run -p openexplorer-gdrive --release --example upload_speed -- [MB]`

use std::io::Write;
use std::time::Instant;

use openexplorer_gdrive::{ClientConfig, Drive, store};

fn main() {
    let mb: u64 = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(500);
    let cfg = ClientConfig::load().expect("cliente do Google");
    let account = store::accounts().into_iter().next().expect("uma conta conectada");
    let token = store::refresh_token(&account.email).expect("token da conta");
    let drive = || Drive::new(cfg.clone(), token.clone());

    // Bytes that don't repeat, so nothing on the way can shrink them.
    let file = std::env::temp_dir().join(format!("openexplorer-speed-{mb}.bin"));
    if std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0) != mb << 20 {
        let mut f = std::fs::File::create(&file).unwrap();
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut block = vec![0u8; 1 << 20];
        for _ in 0..mb {
            for b in block.as_chunks_mut::<8>().0 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                b.copy_from_slice(&x.to_le_bytes());
            }
            f.write_all(&block).unwrap();
        }
    }

    let folder = drive().create_folder("OpenExplorer - teste de velocidade", "root").expect("pasta de teste");
    println!("conta {}, pasta {}, arquivo {mb} MB", account.email, folder.id);
    for at_once in [1, 2, 4] {
        let t = Instant::now();
        let results: Vec<_> = std::thread::scope(|scope| {
            let runs: Vec<_> = (0..at_once)
                .map(|i| {
                    let (file, folder, drive) = (&file, &folder.id, drive());
                    scope.spawn(move || drive.upload(file, folder, &format!("teste {at_once} juntos {i}.bin"), &mut |_| true))
                })
                .collect();
            runs.into_iter().map(|r| r.join().unwrap()).collect()
        });
        let secs = t.elapsed().as_secs_f64();
        let ok = results.iter().filter(|r| r.is_ok()).count();
        println!("{at_once} arquivo(s) juntos: {:6.1} MB/s no total ({secs:.1} s, {ok}/{at_once} ok)", (mb * at_once as u64) as f64 / secs);
        for e in results.iter().filter_map(|r| r.as_ref().err()) {
            println!("   erro: {e}");
        }
    }
    match drive().trash(&folder.id) {
        Ok(_) => println!("pasta de teste movida para a lixeira do Drive"),
        Err(e) => println!("não consegui mover a pasta de teste para a lixeira: {e}"),
    }
}
