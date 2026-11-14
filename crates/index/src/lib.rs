//! Open Explorer's index of the computer's files, Everything's way: on Windows each NTFS
//! volume's MFT read whole into memory and kept current from its USN journal, by a Windows
//! service (it needs an administrator's access to the volumes), asked over a named pipe; on
//! Linux the folders listed as the user and followed with inotify, by the user's own service,
//! asked over a Unix socket (see [`unix`]).

pub mod fold;
pub mod index;
pub mod proto;
pub mod tree;
pub mod walk;

#[cfg(windows)]
pub mod ntfs;
#[cfg(windows)]
pub mod pipe;
#[cfg(windows)]
pub mod service;
#[cfg(target_os = "linux")]
pub mod unix;
