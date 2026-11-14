//! `openexplorer-index`: the search index service.
//!
//! - started by Windows: the service itself;
//! - `install [--report <arquivo>]`, `uninstall`: as an administrator (the app asks Windows);
//! - `console [--walk]`: the service in this window, to try it; `--walk` lists folders instead of
//!   reading the MFT, which needs no administrator (the app then needs `OE_INDEX_CONSOLE=1`).

#[cfg(windows)]
fn main() {
    use openexplorer_index::service;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("install") => service::install(),
        Some("uninstall") => service::uninstall(),
        Some("console") => service::run(args.iter().any(|a| a == "--walk")),
        Some(other) => Err(format!("Comando desconhecido: {other}")),
        None => service::dispatch().map_err(|e| format!("{e}\nUso: openexplorer-index install | uninstall | console [--walk]")),
    };
    if let Err(e) = result {
        // Run elevated, this must not write through a link someone left at that path: only a
        // new file is written.
        if let Some(i) = args.iter().position(|a| a == "--report")
            && let Some(path) = args.get(i + 1)
            && let Ok(mut f) = std::fs::OpenOptions::new().write(true).create_new(true).open(path)
        {
            use std::io::Write;
            let _ = f.write_all(e.as_bytes());
        }
        eprintln!("{e}");
        std::process::exit(1);
    }
}

/// `openexplorer-index [serve]`: the service (the app starts it when it isn't running);
/// `install`, `uninstall`: start it at login through systemd, or stop that. No root needed.
#[cfg(target_os = "linux")]
fn main() {
    use openexplorer_index::unix;
    let result = match std::env::args().nth(1).as_deref() {
        None | Some("serve" | "console") => unix::run(),
        Some("install") => unix::install(),
        Some("uninstall") => unix::uninstall(),
        Some(other) => Err(format!("Comando desconhecido: {other}\nUso: openexplorer-index [serve] | install | uninstall")),
    };
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
fn main() {
    eprintln!("O serviço de índice só existe no Windows e no Linux.");
    std::process::exit(1);
}
