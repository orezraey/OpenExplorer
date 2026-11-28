//! Telegram for Open Explorer: a private channel of the person's own account as storage, the
//! way Telegram-Drive does it. Each folder is a text message and each file a message with a
//! document; the tree is read from the channel's history (`records` has the format, `tree` the
//! view), kept on disk sealed, and brought up to date with the channel's change log.
//!
//! The account's other chats show beside it, read-only, in a folder of their own (`chats`):
//! each chat a folder of months, each month the files sent then.
//!
//! Everything here blocks, like the other cloud crates: the app runs it on background threads.
//! grammers is async, so each open account keeps a tokio runtime of its own and blocks on it.
//!
//! # Limits Telegram sets (measured on a real account, `poc/telegram/PLANO.md`)
//!
//! - Sending messages has a pace: past a few a second Telegram answers FLOOD_WAIT with a wait of
//!   some seconds. More at once doesn't help; small files go ten to a message group instead.
//! - A message's file is at most 4000 parts of 512 KiB (about 2000 MiB), 8000 with Premium; a
//!   bigger file goes in several messages (`records`, `account`).
//! - Parts go 512 KiB a request, over connections of their own. A download goes over one to
//!   the DC's media address, as in Telegram's own apps: 42 to 50 MiB/s on a Premium account,
//!   against 24 to 29 over four to the DC's own address. An upload is shared out among those
//!   four, each good for some 7 MiB/s (see `lanes` and `real`).
//! - A file's place is asked with the page of history around it, so a folder of small files
//!   comes down with one request a file, not two (see `places`).

pub mod account;
pub mod backend;
pub mod budget;
pub mod cache;
pub mod chats;
pub mod lanes;
pub mod login;
pub mod open;
pub mod places;
pub mod real;
pub mod records;
pub mod session;
pub mod store;
pub mod tree;

/// A channel in memory: the crate's tests, and the app's with `fake`.
#[cfg(any(test, feature = "fake"))]
pub mod fake;

pub use account::Account;
pub use login::{Login, Step};
pub use open::{cached, disconnect, reach, reach_id, save_caches, set_store};
pub use store::{Store, StoredAccount};
pub use tree::{Found, Item};

/// Errors are shown to the user as they are, so they're plain sentences in the interface's
/// language (see `openexplorer_i18n`).
pub type Result<T> = std::result::Result<T, String>;

/// The error of a transfer stopped by Cancelar.
pub const CANCELLED: &str = "Cancelado.";
