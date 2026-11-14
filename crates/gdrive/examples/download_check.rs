//! Downloads one ordinary file and exports one Google Doc from the first connected account,
//! read-only: looks through My Drive's folders (breadth first, up to 200) for the smallest file
//! up to 50 MB and for a Google Docs file, fetches both into a new temp folder, prints what
//! arrived, and deletes the folder. With `--segments`, the file is instead the biggest up to
//! 300 MB, fetched in one stream and then in parallel segments, to compare.
//!
//! `cargo run -p openexplorer-gdrive --example download_check [-- --segments]`

use std::collections::VecDeque;
use std::time::Instant;

use openexplorer_gdrive::download::{export_format, local_name};
use openexplorer_gdrive::{ClientConfig, Drive, DriveFile, store};

fn main() {
    let segmented = std::env::args().any(|a| a == "--segments");
    let cfg = ClientConfig::load().expect("cliente do Google");
    let account = store::accounts().into_iter().next().expect("uma conta conectada");
    let token = store::refresh_token(&account.email).expect("token da conta");
    let drive = Drive::new(cfg, token);

    let (mut file, mut doc): (Option<DriveFile>, Option<DriveFile>) = (None, None);
    let mut folders = VecDeque::from(["root".to_string()]);
    let mut seen = 0;
    while let Some(id) = folders.pop_front() {
        seen += 1;
        drive
            .list(&id, None, |page, _| {
                for f in page {
                    if f.is_dir && f.target.is_none() {
                        folders.push_back(f.id.clone());
                    } else if export_format(&f.mime).is_some() && f.mime.ends_with("document") {
                        doc.get_or_insert(f);
                    } else if !f.mime.starts_with("application/vnd.google-apps.")
                        && f.size.is_some_and(|n| n > 0 && n <= if segmented { 300 << 20 } else { 50 << 20 })
                        && file.as_ref().is_none_or(|b| if segmented { f.size > b.size } else { f.size < b.size })
                    {
                        file = Some(f);
                    }
                }
                true
            })
            .expect("listar");
        if seen >= 200 || (doc.is_some() && file.is_some() && seen >= 20) {
            break;
        }
    }

    let dir = std::env::temp_dir().join(format!("openexplorer-download-check-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let runs: Vec<(DriveFile, bool)> = match segmented {
        true => file.into_iter().flat_map(|f| [(f.clone(), false), (f, true)]).collect(),
        false => [file, doc].into_iter().flatten().map(|f| (f, false)).collect(),
    };
    for (f, segmented) in runs {
        let to = dir.join(local_name(&f));
        let t = Instant::now();
        let got = drive.download(&f, &to, segmented, &mut |_| true);
        let secs = t.elapsed().as_secs_f64();
        match got {
            Ok(n) => {
                let meta = std::fs::metadata(&to).unwrap();
                let kept = meta.modified().ok() == f.modified;
                let speed = n as f64 / secs / 1_048_576.0;
                println!("ok: {} ({}) -> {n} bytes em {secs:.1} s ({speed:.1} MB/s, segmentos: {segmented}); data mantida: {kept}", local_name(&f), f.mime);
                // Google's own formats list the quota they use as their size, not the export's.
                if let (Some(size), false) = (f.size, f.mime.starts_with("application/vnd.google-apps.")) {
                    assert_eq!(meta.len(), size, "o arquivo baixado tem o tamanho do Drive");
                }
            }
            Err(e) => println!("erro: {}: {e}", f.name),
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
    println!("pasta temporária apagada ({} pastas olhadas)", seen);
}
