//! The Windows service: installing it (once, as an administrator), running it, and asking
//! Windows how it is. It runs as LocalSystem, from a copy in Program Files that only
//! administrators can change: a system service must never run a file a user could swap.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use windows_service::service::{ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::index::{Index, Vol};
use crate::proto::SERVICE;
use crate::{ntfs, pipe, walk};

const DISPLAY_NAME: &str = "Open Explorer — índice de arquivos";
const DESCRIPTION: &str = "Mantém o índice dos nomes de arquivos dos discos, lido da MFT do NTFS, que a pesquisa do Open Explorer consulta.";
const EXE: &str = "openexplorer-index.exe";

/// How the service is, for the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Missing,
    Stopped,
    Starting,
    Running,
}

pub fn state() -> State {
    let Ok(m) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) else { return State::Missing };
    let Ok(s) = m.open_service(SERVICE, ServiceAccess::QUERY_STATUS) else { return State::Missing };
    match s.query_status().map(|st| st.current_state) {
        Ok(ServiceState::Running) => State::Running,
        Ok(ServiceState::StartPending | ServiceState::ContinuePending) => State::Starting,
        _ => State::Stopped,
    }
}

/// The running service's process, to check who serves the pipe.
pub fn process_id() -> Option<u32> {
    let m = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
    m.open_service(SERVICE, ServiceAccess::QUERY_STATUS).ok()?.query_status().ok()?.process_id
}

/// Where the service runs from.
fn installed_exe() -> Result<PathBuf, String> {
    let pf = std::env::var_os("ProgramFiles").ok_or("ProgramFiles não está definido.")?;
    Ok(Path::new(&pf).join("OpenExplorer").join(EXE))
}

fn manager(access: ServiceManagerAccess) -> Result<ServiceManager, String> {
    ServiceManager::local_computer(None::<&str>, access).map_err(|e| format!("Gerenciador de serviços: {e}"))
}

/// Stops `s` and waits for it, up to 20 seconds.
fn stop(s: &windows_service::service::Service) {
    if s.stop().is_err() {
        return;
    }
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(20) && s.query_status().is_ok_and(|st| st.current_state != ServiceState::Stopped) {
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Installs (or updates) and starts the service: this program copied to Program Files and
/// registered to start with Windows. Needs an administrator.
pub fn install() -> Result<(), String> {
    let m = manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::START | ServiceAccess::CHANGE_CONFIG;
    let existing = m.open_service(SERVICE, access).ok();
    if let Some(s) = &existing {
        stop(s);
    }
    let src = std::env::current_exe().map_err(|e| e.to_string())?;
    let dst = installed_exe()?;
    if src != dst {
        std::fs::create_dir_all(dst.parent().unwrap()).map_err(|e| format!("Criar {}: {e}", dst.display()))?;
        // The old process can take a moment to let go of the file.
        let mut copied = std::fs::copy(&src, &dst);
        for _ in 0..20 {
            if copied.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
            copied = std::fs::copy(&src, &dst);
        }
        copied.map_err(|e| format!("Copiar o serviço para {}: {e}", dst.display()))?;
    }
    let info = ServiceInfo {
        name: SERVICE.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: dst,
        launch_arguments: vec![],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    let s = match existing {
        Some(s) => {
            s.change_config(&info).map_err(|e| format!("Atualizar o serviço: {e}"))?;
            s
        }
        None => m.create_service(&info, access).map_err(|e| format!("Criar o serviço: {e}"))?,
    };
    let _ = s.set_description(DESCRIPTION);
    s.start(&[] as &[&std::ffi::OsStr]).map_err(|e| format!("Iniciar o serviço: {e}"))
}

/// Stops and removes the service and its copy in Program Files. Needs an administrator.
pub fn uninstall() -> Result<(), String> {
    let m = manager(ServiceManagerAccess::CONNECT)?;
    let s = m.open_service(SERVICE, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE).map_err(|e| format!("Abrir o serviço: {e}"))?;
    stop(&s);
    s.delete().map_err(|e| format!("Remover o serviço: {e}"))?;
    drop(s);
    let exe = installed_exe()?;
    let _ = std::fs::remove_file(&exe);
    let _ = std::fs::remove_dir(exe.parent().unwrap());
    Ok(())
}

/// Runs `openexplorer-index install` as an administrator (Windows asks the user), from the
/// folder the app runs from, and waits for it.
pub fn install_elevated() -> Result<(), String> {
    use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    use windows::core::{HSTRING, PCWSTR, w};
    let exe = std::env::current_exe().map_err(|e| e.to_string())?.with_file_name(EXE);
    if !exe.exists() {
        return Err(format!("{} não foi encontrado ao lado do Open Explorer.", exe.display()));
    }
    let report = std::env::temp_dir().join(format!("openexplorer-index-install-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&report);
    let (file, params) = (HSTRING::from(exe.as_os_str()), HSTRING::from(format!("install --report \"{}\"", report.display())));
    let mut info = SHELLEXECUTEINFOW { cbSize: size_of::<SHELLEXECUTEINFOW>() as u32, fMask: SEE_MASK_NOCLOSEPROCESS, lpVerb: w!("runas"), lpFile: PCWSTR(file.as_ptr()), lpParameters: PCWSTR(params.as_ptr()), nShow: SW_HIDE.0, ..Default::default() };
    if let Err(e) = unsafe { ShellExecuteExW(&mut info) } {
        return Err(if e.code() == ERROR_CANCELLED.to_hresult() { "A instalação foi cancelada.".into() } else { format!("Pedir permissão de administrador: {e}") });
    }
    let mut code = 1u32;
    unsafe {
        if WaitForSingleObject(info.hProcess, INFINITE) == WAIT_OBJECT_0 {
            let _ = GetExitCodeProcess(info.hProcess, &mut code);
        }
        let _ = CloseHandle(info.hProcess);
    }
    let said = std::fs::read_to_string(&report).unwrap_or_default();
    let _ = std::fs::remove_file(&report);
    if code == 0 { Ok(()) } else { Err(if said.is_empty() { format!("A instalação falhou (código {code}).") } else { said }) }
}

/// Indexes every NTFS volume of a fixed disk, each on its own thread, and serves searches.
/// `walk` lists folders instead of reading MFTs, which needs no administrator: for trying it.
pub fn run(walk: bool) -> Result<(), String> {
    let vols: Vec<(char, Arc<Vol>)> = ntfs::volumes().into_iter().map(|l| (l, Vol::new(&format!("{l}:\\")))).collect();
    for (letter, vol) in &vols {
        let (letter, vol) = (*letter, vol.clone());
        std::thread::spawn(move || {
            if walk {
                let t = Instant::now();
                let tree = walk::walk(&format!("{letter}:\\"));
                vol.ready(tree, t.elapsed().as_millis() as u64);
            } else {
                crate::index::follow(vol, letter);
            }
        });
    }
    pipe::serve(Arc::new(Index::new(vols.into_iter().map(|(_, v)| v).collect())))
}

static STATUS: OnceLock<ServiceStatusHandle> = OnceLock::new();

fn report(state: ServiceState) {
    if let Some(h) = STATUS.get() {
        let accepted = if state == ServiceState::Running { ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN } else { ServiceControlAccept::empty() };
        let _ = h.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accepted,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(5),
            process_id: None,
        });
    }
}

windows_service::define_windows_service!(ffi_service_main, service_main);

fn service_main(_args: Vec<OsString>) {
    let handler = |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            // Nothing to save: the index is read again at the next start.
            report(ServiceState::Stopped);
            std::process::exit(0);
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let Ok(h) = service_control_handler::register(SERVICE, handler) else { return };
    let _ = STATUS.set(h);
    report(ServiceState::Running);
    let _ = run(false);
    report(ServiceState::Stopped);
}

/// Hands this process to Windows' service manager; fails when not started by it.
pub fn dispatch() -> Result<(), String> {
    windows_service::service_dispatcher::start(SERVICE, ffi_service_main).map_err(|e| e.to_string())
}
