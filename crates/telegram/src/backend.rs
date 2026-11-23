//! What an account asks of its channel, apart from how: Telegram through grammers (`real`), or
//! the in-memory stand-in the tests use (`fake`). The tree, the paths and the rules about names
//! live above this, in `account`, the same over both.

use std::io::{Read, Write};

use crate::Result;
use crate::chats::{ChatFile, ChatRef, Cursor, Month, Page};
use crate::tree::Msg;

/// How a transfer reports the bytes done so far and hears whether to go on: `false` stops it
/// with [`crate::CANCELLED`]. It's also asked, with the same count, while a transfer waits out a
/// FLOOD_WAIT, so Cancelar works then too.
pub type Progress<'a> = &'a mut dyn FnMut(u64) -> bool;

/// What changed in the channel since a position of its change log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Changes {
    /// The messages sent or edited (as they are now) and those deleted; the log's position now.
    Since { upserts: Vec<Msg>, deleted: Vec<i32>, pts: i32 },
    /// Too much changed to tell: read the whole history again.
    TooLong,
}

/// A small file sent with others in one message group.
pub struct Small {
    pub caption: String,
    pub name: String,
    pub bytes: Vec<u8>,
}

/// At most this many files go in one message group (Telegram's limit for an album).
pub const GROUP: usize = 10;

/// A page of the listing of chats, or when Telegram lets it be asked again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Paged {
    Page(Page),
    /// Seconds since 1970.
    WaitUntil(i64),
}

/// What a backend with no chats answers (`chats`).
fn no_chats() -> String {
    openexplorer_i18n::t("Esta conta não lê conversas.", "This account doesn't read chats.").into()
}

pub trait Backend: Send + Sync {
    /// Every message of the channel and the change log's position, taken before reading, so
    /// nothing that changes meanwhile is missed (applying a change twice changes nothing).
    fn history(&self) -> Result<(Vec<Msg>, i32)>;

    /// What changed since `pts`.
    fn changes(&self, pts: i32) -> Result<Changes>;

    fn send_text(&self, text: &str) -> Result<Msg>;

    /// Replaces a message's text or a document's caption. `None` when Telegram no longer lets
    /// that message be edited: the caller sends it again instead.
    fn edit(&self, id: i32, text: &str) -> Result<Option<Msg>>;

    /// Sends `len` bytes of `src` as a document named `name`.
    fn send_file(&self, caption: &str, name: &str, src: &mut dyn Read, len: u64, progress: Progress) -> Result<Msg>;

    /// Sends up to [`GROUP`] small files in one message group: fewer messages, so fewer waits.
    fn send_group(&self, files: Vec<Small>) -> Result<Vec<Msg>>;

    /// Sends message `id`'s document again with another caption, on Telegram's side: no bytes
    /// travel.
    fn copy(&self, id: i32, caption: &str) -> Result<Msg>;

    fn delete(&self, ids: &[i32]) -> Result<()>;

    /// Writes message `id`'s document to `out`.
    fn download(&self, id: i32, out: &mut dyn Write, progress: Progress) -> Result<()>;

    /// The largest file the account can send.
    fn max_file(&self) -> u64;

    /// Whether the account shows its chats (`chats`): a real account does; the fake, once it has
    /// some.
    fn has_chats(&self) -> bool {
        false
    }

    /// A page of the account's chats, the most recent first, from `after` (the first page with
    /// `None`), leaving out the storage channel. `patient`: a FLOOD_WAIT is waited out; else its
    /// end comes back as [`Paged::WaitUntil`].
    fn chats(&self, _after: Option<&Cursor>, _patient: bool) -> Result<Paged> {
        Err(no_chats())
    }

    /// The months with files in `chat` (see `chats`).
    fn chat_months(&self, _chat: &ChatRef) -> Result<Vec<Month>> {
        Err(no_chats())
    }

    /// The files sent in `chat` in `month`.
    fn chat_files(&self, _chat: &ChatRef, _month: Month) -> Result<Vec<ChatFile>> {
        Err(no_chats())
    }

    /// Writes the file of message `msg` of `chat` to `out`.
    fn chat_download(&self, _chat: &ChatRef, _msg: i32, _out: &mut dyn Write, _progress: Progress) -> Result<()> {
        Err(no_chats())
    }

    /// Until when (seconds since 1970) Telegram asked to wait, if it is asking now.
    fn waiting_until(&self) -> Option<i64> {
        None
    }

    /// Ends the session on Telegram's side (the account was removed).
    fn sign_out(&self) {}

    /// Saves the session nowhere from now on (the account is being removed).
    fn detach(&self) {}

    /// How many connections per DC carry file parts, and how many parts may be in flight for the
    /// account: the tests and benchmarks, to compare.
    fn tune(&self, _connections: usize, _parts: usize) {}

    /// Per DC, how many connections carry file parts now.
    fn connections_open(&self) -> Vec<(i32, usize)> {
        Vec::new()
    }
}
