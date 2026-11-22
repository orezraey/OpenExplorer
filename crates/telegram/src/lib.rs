//! Telegram for Open Explorer: a private channel of the person's own account as storage, the
//! way Telegram-Drive does it. Each folder is a text message and each file a message with a
//! document; the tree is read from the channel's history (`records` has the format, `tree` the
//! view), kept on disk sealed, and brought up to date with the channel's change log.
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
//! - Parts go 512 KiB a request. One connection to a DC carries only so much: like Telegram's own
//!   apps, an account spreads its parts over several (8 with Premium), which took a Premium
//!   account from ~17 to ~40 MiB/s down and from ~9 to ~33 up (see `real`).

pub mod account;
pub mod backend;
pub mod cache;
pub mod login;
pub mod open;
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
