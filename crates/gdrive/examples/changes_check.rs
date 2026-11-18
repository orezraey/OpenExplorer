//! Reads Drive's change log of the first connected account, changing nothing: where the log is
//! now, Meu Drive's real id, and what changed over the last stretch of it (counts by kind).
//!
//! `cargo run -p openexplorer-gdrive --example changes_check [quantos-pontos-atrás]`

use std::time::Instant;

use openexplorer_gdrive::{ClientConfig, Drive, store};

fn main() {
    let cfg = ClientConfig::load().expect("cliente do Google");
    let account = store::accounts().into_iter().next().expect("uma conta conectada");
    let d = Drive::new(cfg, store::refresh_token(&account.email).expect("token da conta"));
    let back: u64 = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(200);

    let t = Instant::now();
    let token = d.start_page_token().expect("ponto de início");
    println!("ponto atual: {token} ({} ms)", t.elapsed().as_millis());
    println!("Meu Drive: {}", if d.root_id().is_ok() { "id lido" } else { "falhou" });

    let t = Instant::now();
    let (none, after) = d.changes(&token).expect("mudanças desde agora");
    println!("desde agora: {} mudanças, próximo ponto {after} ({} ms)", none.len(), t.elapsed().as_millis());

    // Tokens are numbers in practice: a smaller one reads back over the latest changes.
    let Ok(n) = token.parse::<u64>() else { return println!("o ponto não é um número; fim") };
    let from = n.saturating_sub(back).max(1).to_string();
    let t = Instant::now();
    let (changes, next) = d.changes(&from).expect("mudanças recentes");
    let removed = changes.iter().filter(|c| c.file.is_none() && !c.drive).count();
    let trashed = changes.iter().filter(|c| c.trashed).count();
    let folders = changes.iter().filter(|c| c.file.as_ref().is_some_and(|f| f.is_dir)).count();
    let drives = changes.iter().filter(|c| c.drive).count();
    let no_parent = changes.iter().filter(|c| c.file.is_some() && c.parents.is_empty()).count();
    println!(
        "desde {from}: {} itens ({folders} pastas, {trashed} na lixeira, {removed} removidos, {drives} drives compartilhados, {no_parent} sem pasta), próximo ponto {next} ({} ms)",
        changes.len(),
        t.elapsed().as_millis()
    );
}
