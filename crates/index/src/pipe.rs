//! The named pipe between the app and the service. Only local, signed-in users can open it,
//! and the app checks that whoever serves it is the service Windows started, not a program
//! that took the pipe's name first.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use openexplorer_i18n::{t, tr};
use windows::Win32::Foundation::{ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, FlushFileBuffers, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW};
use windows::core::{HSTRING, w};

use crate::index::Index;
use crate::proto::{self, Answer, PIPE, Request};

/// Full access for the system, administrators and the pipe's owner (the service; whoever runs
/// `console`), who opens each new instance; for signed-in users, reading and writing data only
/// (0x12008b: FILE_GENERIC_READ | FILE_WRITE_DATA). Not GENERIC_WRITE: for a pipe it includes
/// FILE_CREATE_PIPE_INSTANCE, which would let any user serve the pipe's name too.
const SDDL: windows::core::PCWSTR = w!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)(A;;0x12008b;;;AU)");
/// What the app asks of the pipe: GENERIC_READ | FILE_WRITE_DATA, all the DACL gives users.
const CLIENT_ACCESS: u32 = 0x8000_0000 | 0x2;
/// Connections answered at once; more are closed at once, so clients that never write can't
/// pile up threads.
const MAX_CLIENTS: usize = 32;
/// Pipe buffers, each way.
const BUFFER: u32 = 1 << 16;
/// `SECURITY_IDENTIFICATION`: whoever serves the pipe may learn who we are, not act as us.
const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;

/// Answers searches from `index`, a thread per connection, until the process ends.
pub fn serve(index: Arc<Index>) -> Result<(), String> {
    let mut sd = PSECURITY_DESCRIPTOR::default();
    unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(SDDL, SDDL_REVISION_1, &mut sd, None) }.map_err(|e| tr!("Permissões do pipe: {e}", "Pipe permissions: {e}"))?;
    let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0, bInheritHandle: false.into() };
    let name = HSTRING::from(PIPE);
    let clients = Arc::new(AtomicUsize::new(0));
    let mut first = true;
    let result = loop {
        // The first instance must be ours alone: if the name is taken, another program has it.
        let mode = if first { PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE } else { PIPE_ACCESS_DUPLEX };
        let h = unsafe { CreateNamedPipeW(&name, mode, PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS, PIPE_UNLIMITED_INSTANCES, BUFFER, BUFFER, 0, Some(&sa)) };
        if h == INVALID_HANDLE_VALUE {
            break Err(tr!("Criar o pipe: {}", "Create the pipe: {}", windows::core::Error::from_thread()));
        }
        first = false;
        let connected = match unsafe { ConnectNamedPipe(h, None) } {
            Ok(()) => true,
            Err(e) => e.code() == ERROR_PIPE_CONNECTED.to_hresult(),
        };
        // SAFETY: `h` is a pipe handle we own; the File closes it.
        let file = unsafe { File::from_raw_handle(h.0) };
        if !connected || clients.load(Ordering::Relaxed) >= MAX_CLIENTS {
            continue;
        }
        clients.fetch_add(1, Ordering::Relaxed);
        let (index, clients) = (index.clone(), clients.clone());
        std::thread::spawn(move || {
            answer(file, &index);
            clients.fetch_sub(1, Ordering::Relaxed);
        });
    };
    unsafe { LocalFree(Some(HLOCAL(sd.0))) };
    result
}

fn answer(file: File, index: &Index) {
    let answer = match proto::read_request(&mut BufReader::new(&file)) {
        Ok(req) => index.answer(&req),
        Err(e) => Answer { header: proto::Header { error: Some(e), ..Default::default() }, rows: Vec::new() },
    };
    let _ = proto::write_answer(&mut BufWriter::with_capacity(BUFFER as usize, &file), &answer);
    let h = HANDLE(file.as_raw_handle());
    unsafe {
        let _ = FlushFileBuffers(h);
        let _ = DisconnectNamedPipe(h);
    }
}

/// Asks the service. An error when it isn't running (or its pipe is held by someone else).
pub fn ask(req: &Request) -> Result<Answer, String> {
    let file = open()?;
    if std::env::var_os("OE_INDEX_CONSOLE").is_none() {
        let mut pid = 0;
        unsafe { GetNamedPipeServerProcessId(HANDLE(file.as_raw_handle()), &mut pid) }.map_err(|e| tr!("Pipe do índice: {e}", "Index pipe: {e}"))?;
        if crate::service::process_id() != Some(pid) {
            return Err(t("O pipe do índice não é do serviço do Open Explorer.", "The index pipe doesn't belong to the Open Explorer service.").into());
        }
    }
    proto::write_request(&mut &file, req).map_err(|e| tr!("Falar com o serviço: {e}", "Talking to the service: {e}"))?;
    proto::read_answer(&mut BufReader::with_capacity(BUFFER as usize, &file))
}

fn open() -> Result<File, String> {
    for _ in 0..3 {
        match OpenOptions::new().access_mode(CLIENT_ACCESS).security_qos_flags(SECURITY_IDENTIFICATION).open(PIPE) {
            Ok(f) => return Ok(f),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => {
                let _ = unsafe { WaitNamedPipeW(&HSTRING::from(PIPE), 2000) };
            }
            Err(e) => return Err(tr!("O serviço de pesquisa não está rodando ({e}).", "The search service isn't running ({e}).")),
        }
    }
    Err(t("O serviço de pesquisa está ocupado.", "The search service is busy.").into())
}
