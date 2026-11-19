//! Backblaze B2 for Open Explorer: connecting an application key, the buckets it reaches and
//! their folders, new folders, renaming and moving, deleting (hiding, or every version), copying
//! on B2's side, downloads that pick up where a dropped connection left them, and uploads, big
//! ones in parts sent side by side and resumed after a failure.
//!
//! Everything here blocks; the app runs it on background threads.
//!
//! # Why our own client, on the native API
//!
//! - Backblaze publishes no Rust SDK (its SDKs are Python's `b2sdk` and Java's; the Go one,
//!   `blazer`, is archived). The B2 crates on crates.io are either years old (`backblaze-b2`,
//!   2017; `raze`, 2021) or async on tokio (`opendal-service-b2`, `b2_backblaze`), which the app
//!   doesn't run.
//! - B2 also speaks S3, and `rust-s3` has a blocking flavour. But through S3 a hide is a delete
//!   marker, versions are opaque version ids, there's no `recommendedPartSize`, a key's bucket and
//!   prefix restrictions aren't told up front, and server-side copies of big files are still to
//!   be put together by hand; it would also bring attohttpc and an XML stack beside ureq.
//! - So this is a client of the native API, blocking on ureq like the Google Drive and MEGA
//!   crates, from crate versions Cargo.lock already had.
//!
//! It speaks API **v4**, not v3: keys made for several buckets (`allowed.buckets`, a list) are
//! refused by v1 to v3 with `401 unsupported`, and v4 is the version Backblaze documents now.
//!
//! # Folders, versions and hidden files
//!
//! B2 has buckets of files whose names may hold `/`; there are no folders and no change log.
//! The top lists the buckets the key reaches; a folder is a prefix up to a `/`, listed with
//! `b2_list_file_names` and the `/` delimiter. An empty folder is a zero-byte `.bzEmpty` file in
//! it, as B2's web interface makes (and hides). Renaming or moving is a copy on B2's side
//! (`b2_copy_file`, or `b2_copy_part` for files of 5 GB or more) and then the old name retired.
//!
//! Every name keeps versions. The panes show only each name's current version (what
//! `b2_list_file_names` gives). Deleting hides (`b2_hide_file`): the name leaves the listing and
//! what it held stays as a hidden version until the bucket's lifecycle rules delete it (never,
//! with B2's default "keep all versions"), so it can be brought back by deleting the hide marker
//! (Desfazer). "Delete every version" deletes each version of the name for good. A rename
//! retires the old name by deleting the version just copied when it was the name's only one
//! (nothing of its history is lost), and by hiding it otherwise.
//!
//! See [`cache`] for how listings are kept and when they're read again, and [`watch`] for the
//! readings the app makes on its own so changes made elsewhere show.

pub mod account;
pub mod api;
pub mod cache;
pub mod download;
pub mod notes;
pub mod store;
pub mod upload;
pub mod watch;

#[cfg(test)]
mod bench;
#[cfg(test)]
mod flows;
#[cfg(test)]
pub(crate) mod mock;

pub use account::{Account, Bucket, Fresh, Item, Rule, cached, connect, disconnect, reach, save_caches};
pub use watch::Polled;
pub use api::{Allowed, set_log};
pub use cache::set_dir as set_cache_dir;
pub use notes::set_file as set_notes_file;
pub use store::Store;
pub use upload::{Unfinished, Uploads};

/// Errors are shown to the user as they are, so they're plain sentences in the interface's
/// language (see `openexplorer_i18n`).
pub type Result<T> = std::result::Result<T, String>;

/// The error of a transfer stopped by Cancelar.
pub const CANCELLED: &str = "Cancelado.";

/// Called with the bytes moved so far; `false` stops the transfer with [`CANCELLED`].
pub type Progress<'a> = &'a mut dyn FnMut(u64) -> bool;
