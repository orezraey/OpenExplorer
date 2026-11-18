//! MEGA (mega.nz) for Open Explorer: logging in (with two-factor codes), the accounts and their
//! sessions, and the calls the panes need: listing, new folders, renaming, moving, the Rubbish
//! Bin and restoring from it, copying on MEGA's side, downloading and uploading, public links,
//! the storage quota, and the changes made elsewhere.
//!
//! Everything here blocks; the app runs it on background threads.
//!
//! # Why our own client
//!
//! MEGA encrypts everything on the client, so talking to it means doing its cryptography. Three
//! ways were weighed:
//!
//! - MEGA's official SDK (C++, BSD-2) does everything, but binding it means building it (and
//!   its cryptography and HTTP dependencies) with CMake and vcpkg on every CI target.
//! - The `mega` crate (0.8.0, MIT/Apache, last released October 2024) is async with reqwest and
//!   tokio beside the app's ureq, can't make or remove public links, copy on the server or
//!   restore from the Rubbish Bin, panics (`todo!`, `assert_eq!`) on some answers, and doesn't
//!   answer the proof of work (HTTP 402 `X-Hashcash`) MEGA now asks of every login, so it
//!   can't log in at all.
//! - Our own client of the JSON API, blocking on ureq like the Google Drive crate, built from
//!   crates Cargo.lock already had. This is it; the SDK's sources were the reference for what
//!   the `mega` crate doesn't do (hashcash, links, `rr`, `cr`, copies).

pub mod account;
pub mod cache;
pub mod api;
pub mod changes;
pub mod crypto;
pub mod download;
pub mod hashcash;
pub mod nodes;
pub mod quota;
pub mod session;
pub mod share;
pub mod store;
pub mod upload;

pub use account::{Account, Area, Item, Place, cached, connect, disconnect, handle_path, reach, save_caches};
pub use cache::set_dir as set_cache_dir;
pub use api::set_log;
pub use changes::Changes;
pub use nodes::{Kind, Node};
pub use quota::Quota;
pub use session::Refused;
pub use store::Store;

/// Errors are shown to the user as they are, so they're plain sentences in the interface's
/// language (see `openexplorer_i18n`).
pub type Result<T> = std::result::Result<T, String>;

/// The error of a transfer stopped by Cancelar.
pub const CANCELLED: &str = "Cancelado.";
