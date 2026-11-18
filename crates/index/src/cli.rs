//! The search service's commands. They aren't a program of their own: the app's executable runs
//! them when its first argument is [`ARG`] (`openexplorer index …`), so one file is both the app
//! and the service, as `Everything.exe -svc` is Everything's.
//!
//! - Windows: `index service` is what Windows starts (the service itself); `index install
//!   [--report <arquivo>]` and `index uninstall`, as an administrator (the app asks Windows);
//!   `index console [--walk]`, the service in this window, to try it; `--walk` lists folders
//!   instead of reading the MFT, which needs no administrator (the app then needs
//!   `OE_INDEX_CONSOLE=1`).
//! - Linux: `index serve`, the service (the app starts it when it isn't running); `index
//!   install` and `index uninstall`: start it at login through systemd, or stop that. No root.

/// The first argument that makes the app's executable the search service.
pub const ARG: &str = "index";

/// Attaches to the parent terminal in release builds (`windows_subsystem = "windows"`) when
/// neither stdout nor stderr is already redirected, so CLI commands like `openexplorer index
/// console --walk` and `openexplorer index uninstall` print to PowerShell / cmd.
#[cfg(windows)]
fn attach_parent_console() {
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    const STD_OUTPUT_HANDLE: u32 = (-11i32) as u32;
    const STD_ERROR_HANDLE: u32 = (-12i32) as u32;
    const INVALID_HANDLE: isize = -1;
    unsafe extern "system" {
        fn AttachConsole(dwProcessId: u32) -> i32;
        fn GetStdHandle(nStdHandle: u32) -> isize;
    }
    unsafe {
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        let err = GetStdHandle(STD_ERROR_HANDLE);
        let has_out = out != 0 && out != INVALID_HANDLE;
        let has_err = err != 0 && err != INVALID_HANDLE;
        if !has_out && !has_err {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

/// Runs the command in `args` (what follows [`ARG`]) and returns the process's exit code.
#[cfg(windows)]
pub fn run(args: &[String]) -> i32 {
    use crate::service;
    let sub = args.first().map(String::as_str);
    if sub != Some("service") {
        attach_parent_console();
    }
    if matches!(sub, Some("help" | "--help" | "-h")) {
        println!("Uso: openexplorer index install | uninstall | console [--walk]");
        return 0;
    }
    let result = match sub {
        None | Some("service") => service::dispatch().map_err(|e| format!("{e}\nUso: openexplorer index install | uninstall | console [--walk]")),
        Some("install") => service::install(),
        Some("uninstall") => service::uninstall(),
        Some("console") => service::run(args.iter().any(|a| a == "--walk")),
        Some(other) => Err(format!("Comando desconhecido: {other}\nUso: openexplorer index install | uninstall | console [--walk]")),
    };
    let Err(e) = result else { return 0 };
    // Run elevated, this must not write through a link someone left at that path: only a new
    // file is written.
    if let Some(i) = args.iter().position(|a| a == "--report")
        && let Some(path) = args.get(i + 1)
        && let Ok(mut f) = std::fs::OpenOptions::new().write(true).create_new(true).open(path)
    {
        use std::io::Write;
        let _ = f.write_all(e.as_bytes());
    }
    eprintln!("{e}");
    1
}

/// Runs the command in `args` (what follows [`ARG`]) and returns the process's exit code.
#[cfg(target_os = "linux")]
pub fn run(args: &[String]) -> i32 {
    use crate::unix;
    if matches!(args.first().map(String::as_str), Some("help" | "--help" | "-h")) {
        println!("Uso: openexplorer index [serve] | install | uninstall");
        return 0;
    }
    let result = match args.first().map(String::as_str) {
        None | Some("serve" | "console") => unix::run(),
        Some("install") => unix::install(),
        Some("uninstall") => unix::uninstall(),
        Some(other) => Err(format!("Comando desconhecido: {other}\nUso: openexplorer index [serve] | install | uninstall")),
    };
    let Err(e) = result else { return 0 };
    eprintln!("{e}");
    1
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn run(_: &[String]) -> i32 {
    eprintln!("O serviço de pesquisa só existe no Windows e no Linux.");
    1
}
