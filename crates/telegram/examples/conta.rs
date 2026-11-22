//! A Telegram account connected the way the app will connect it, from a terminal: the login the
//! app's dialog will drive, the storage channel found or made, and what's in it.
//!
//! ```text
//! cargo run -p openexplorer-telegram --example conta -- entrar
//! cargo run -p openexplorer-telegram --example conta -- listar [pasta]
//! cargo run -p openexplorer-telegram --example conta -- sair
//! ```
//!
//! The account goes where the app keeps its accounts (`%APPDATA%\OpenExplorer\telegram-accounts.json`,
//! the session in the credential store), so the app finds it connected later. `sair` removes it:
//! the session ends on Telegram's side, the channel and its files stay.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use openexplorer_telegram::{Login, Step, Store, cache};

fn config_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("APPDATA").expect("APPDATA")).join("OpenExplorer")
}

fn ask(question: &str) -> String {
    print!("{question}");
    std::io::stdout().flush().unwrap();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).unwrap();
    line.trim().to_string()
}

fn main() {
    let store = Store::at(config_dir().join("telegram-accounts.json"));
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        cache::set_dir(Some(PathBuf::from(local).join("OpenExplorer").join("telegram")));
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("entrar") => enter(&store),
        Some("listar") => list(&store, args.get(1).map(String::as_str).unwrap_or("/")),
        Some("sair") => leave(&store),
        _ => Err("Uso: conta entrar | listar [pasta] | sair".into()),
    };
    openexplorer_telegram::save_caches();
    if let Err(e) = result {
        eprintln!("ERRO: {e}");
        std::process::exit(1);
    }
}

fn enter(store: &Store) -> Result<(), String> {
    println!("O api_id e o api_hash vêm de https://my.telegram.org › API development tools.");
    let api_id: i32 = ask("api_id: ").parse().map_err(|_| "o api_id é um número".to_string())?;
    let api_hash = rpassword::prompt_password("api_hash (não aparece ao digitar): ").map_err(|e| e.to_string())?;
    let phone = ask("Telefone, com o código do país (ex. +55 11 99999-8888): ");
    let mut login = Login::begin(api_id, &api_hash, &phone)?;
    let mut step = loop {
        match login.code(&ask("Código que o Telegram mandou: ")) {
            Ok(s) => break s,
            Err(e) => eprintln!("{e}"),
        }
    };
    loop {
        match step {
            Step::Done(account, stored) => {
                store.add(stored.clone())?;
                println!("Conectado como {}. Canal de armazenamento: {} (id {}).", account.name, openexplorer_telegram::login::CHANNEL_TITLE, stored.channel_id);
                return list(store, "/");
            }
            Step::Password { hint } => {
                let prompt = if hint.is_empty() { "Senha da verificação em duas etapas (não aparece): ".to_string() } else { format!("Senha da verificação em duas etapas (dica: {hint}; não aparece): ") };
                let pw = rpassword::prompt_password(prompt).map_err(|e| e.to_string())?;
                step = match login.password(&pw) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("{e}");
                        Step::Password { hint }
                    }
                };
            }
        }
    }
}

fn list(store: &Store, path: &str) -> Result<(), String> {
    let stored = store.accounts().into_iter().next().ok_or("Nenhuma conta do Telegram conectada: rode `entrar`.")?;
    let account = openexplorer_telegram::reach(&stored)?;
    let items = account.list(path)?;
    println!("{} › {path}: {} itens", account.name, items.len());
    for i in items {
        if i.is_dir() {
            println!("  📁 {}", i.name);
        } else {
            println!("  📄 {}  ({} bytes)", i.name, i.size);
        }
    }
    Ok(())
}

fn leave(store: &Store) -> Result<(), String> {
    let stored = store.accounts().into_iter().next().ok_or("Nenhuma conta do Telegram conectada.")?;
    openexplorer_telegram::disconnect(stored.user_id);
    store.remove(stored.user_id)?;
    // The session ends on a thread of its own: give it a moment before the process goes.
    std::thread::sleep(std::time::Duration::from_secs(3));
    println!("Conta {} removida; o canal e os arquivos continuam no Telegram.", stored.name);
    Ok(())
}
