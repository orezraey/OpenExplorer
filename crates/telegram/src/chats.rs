//! The account's chats as folders to look through and download from, apart from the storage
//! channel: Saved Messages, channels, groups and private chats, each a folder of months, each
//! month a folder of the files sent then (documents, photos, videos, music, voice notes and round
//! videos). Nothing here is written: a chat isn't the person's storage, so making, renaming,
//! sending or deleting there is refused ([`read_only`]); a file is opened, downloaded or copied
//! elsewhere.
//!
//! ```text
//! /Conversas do Telegram/Mensagens Salvas/2026-11/contrato.pdf
//! /Conversas do Telegram/Fulano/2026-10/Foto 2026-10-03 14.03.12.jpg
//! ```
//!
//! Telegram paces the listing of chats hard (100 a request, then waits of up to minutes), so the
//! first opening reads one page, the most recent 100, and the rest comes a page at a time each
//! time the app asks what changed ([`Chats::wanted_more`]), kept on disk with the tree (`cache`). A
//! chat's months come from Telegram's calendar of its files, and a month is one search per kind
//! of file: a chat with years of files opens as fast as one with a few.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use chrono::{Datelike, Local, TimeZone};
use openexplorer_i18n::{t, tr};
use serde::{Deserialize, Serialize};

use crate::backend::Paged;
use crate::tree::{Item, numbered, shown_name};

/// The names the folder of chats goes by at the top of the account: the interface's languages,
/// either leading there whatever the language now.
const FOLDER_NAMES: [&str; 2] = ["Conversas do Telegram", "Telegram chats"];
const SAVED_NAMES: [&str; 2] = ["Mensagens Salvas", "Saved Messages"];

/// How long a chat's months or a month's files are trusted before a listing asks again.
const FRESH: Duration = Duration::from_secs(60);
/// How often the most recent chats are read again once all were: new chats, new names.
const AGAIN: Duration = Duration::from_secs(300);

/// The folder of chats at the top of the account, in the interface's language.
pub fn folder_name() -> &'static str {
    t("Conversas do Telegram", "Telegram chats")
}

fn saved_name() -> &'static str {
    t("Mensagens Salvas", "Saved Messages")
}

fn is_one_of(names: &[&str], name: &str) -> bool {
    let low = name.to_lowercase();
    names.iter().any(|n| n.to_lowercase() == low)
}

/// Whether `name`, at the top of the account, is one of the folder of chats' names (unless the
/// storage has an item by it, which keeps it: see `Account`).
pub fn is_folder_name(name: &str) -> bool {
    is_one_of(&FOLDER_NAMES, name)
}

/// Whether `name` could be the folder of chats: one of its names, or one numbered as a twin of
/// them (`Conversas do Telegram (2)`), as it shows when the storage has an item by its name.
pub fn may_name_folder(name: &str) -> bool {
    if is_folder_name(name) {
        return true;
    }
    name.strip_suffix(')')
        .and_then(|n| n.rsplit_once(" ("))
        .is_some_and(|(base, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && is_folder_name(base))
}

/// The first folder of `path` and the rest (`/a/b/c` → `("a", ["b", "c"])`).
pub fn split_first(path: &str) -> Option<(&str, Vec<&str>)> {
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    let first = parts.next()?;
    Some((first, parts.collect()))
}

/// What anything that would change a chat answers.
pub fn read_only() -> String {
    t(
        "As conversas do Telegram são só leitura: dá para abrir, baixar e copiar os arquivos para outro lugar, não mudar nada nelas.",
        "Telegram chats are read-only: their files can be opened, downloaded and copied elsewhere, but nothing in them can be changed.",
    )
    .into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Kind {
    /// The account's own chat with itself.
    Saved,
    User,
    /// A small group (a "chat" in Telegram's terms).
    Group,
    /// A channel, or a group past the small kind (a "megagroup").
    Channel,
}

/// A chat as Telegram is asked about it: its kind, id and the access hash Telegram gave this
/// session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChatRef {
    pub kind: Kind,
    pub id: i64,
    pub hash: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chat {
    pub at: ChatRef,
    /// Its title, or the person's name.
    pub name: String,
    /// Its last message, seconds since 1970.
    pub date: i64,
}

/// Where the listing of chats goes on from: the last chat of a page and its last message, in the
/// main list or in the archive (which Telegram lists apart, after it). An `id` of 0 is the
/// start of that list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub date: i32,
    pub id: i32,
    pub peer: ChatRef,
    #[serde(default)]
    pub archived: bool,
}

impl Cursor {
    /// The start of the archive, read once the main list ends.
    pub fn archive() -> Cursor {
        Cursor { date: 0, id: 0, peer: SAVED, archived: true }
    }
}

/// One page of the listing of chats, and where the next starts (`None`: that was the last).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub chats: Vec<Chat>,
    pub next: Option<Cursor>,
}

/// A month, in the computer's time zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Month {
    pub year: i32,
    pub month: u32,
}

impl Month {
    /// `2026-11`: sorts as dates do.
    pub fn name(self) -> String {
        format!("{:04}-{:02}", self.year, self.month)
    }

    pub fn parse(name: &str) -> Option<Month> {
        let (y, m) = name.split_once('-')?;
        let (year, month) = (y.parse().ok()?, m.parse().ok()?);
        (y.len() == 4 && m.len() == 2 && (1..=12).contains(&month)).then_some(Month { year, month })
    }

    /// The month moment `ts` (seconds since 1970) falls in.
    pub fn of(ts: i64) -> Month {
        let d = Local.timestamp_opt(ts, 0).earliest().unwrap_or_else(|| Local.timestamp_opt(0, 0).earliest().expect("1970 exists"));
        Month { year: d.year(), month: d.month() }
    }

    /// Its first second and the next month's, seconds since 1970.
    pub fn range(self) -> (i64, i64) {
        let start = |y: i32, m: u32| Local.with_ymd_and_hms(y, m, 1, 0, 0, 0).earliest().map_or(0, |d| d.timestamp());
        let (ny, nm) = if self.month == 12 { (self.year + 1, 1) } else { (self.year, self.month + 1) };
        (start(self.year, self.month), start(ny, nm))
    }
}

/// What a file of a chat is, for its name when it was sent with none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Media {
    Document,
    Photo,
    Video,
    Round,
    Voice,
    Music,
}

/// A file of a chat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatFile {
    /// The message that holds it.
    pub msg: i32,
    pub name: String,
    pub size: u64,
    /// When it was sent, seconds since 1970.
    pub date: i64,
}

/// `video/mp4` → `mp4`; what a file sent with no name is saved as.
pub fn extension(mime: &str) -> &str {
    match mime {
        "image/jpeg" => "jpg",
        "video/quicktime" => "mov",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "audio/mp4" | "audio/x-m4a" | "audio/m4a" => "m4a",
        "audio/x-wav" | "audio/wav" => "wav",
        "audio/x-flac" | "audio/flac" => "flac",
        _ => match mime.split_once('/') {
            Some((_, sub)) if !sub.is_empty() && sub.len() <= 5 && sub.chars().all(|c| c.is_ascii_alphanumeric()) => sub,
            _ => "bin",
        },
    }
}

/// The name a chat's file shows as: the one it was sent with; else, for music, its artist and
/// title; else what it is and when it was sent (`Foto 2026-11-22 14.03.12.jpg`).
pub fn file_name(media: Media, given: &str, music: Option<(&str, &str)>, mime: &str, date: i64) -> String {
    if !given.trim().is_empty() {
        return given.to_string();
    }
    let ext = match media {
        Media::Photo => "jpg",
        Media::Voice if mime.is_empty() => "ogg",
        Media::Round if mime.is_empty() => "mp4",
        _ => extension(mime),
    };
    if media == Media::Music
        && let Some((performer, title)) = music
        && !title.trim().is_empty()
    {
        return if performer.trim().is_empty() { format!("{title}.{ext}") } else { format!("{performer} - {title}.{ext}") };
    }
    let what = match media {
        Media::Document => t("Arquivo", "File"),
        Media::Photo => t("Foto", "Photo"),
        Media::Video => t("Vídeo", "Video"),
        Media::Round => t("Vídeo redondo", "Round video"),
        Media::Voice => t("Áudio", "Voice"),
        Media::Music => t("Música", "Music"),
    };
    let when = Local.timestamp_opt(date, 0).earliest().map_or_else(|| date.to_string(), |d| d.format("%Y-%m-%d %H.%M.%S").to_string());
    format!("{what} {when}.{ext}")
}

/// The chats as kept on disk with the tree.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kept {
    pub list: Vec<Chat>,
    pub next: Option<Cursor>,
    pub complete: bool,
}

/// The ids of the folders the chats show as: never a folder id of the storage (16 letters).
const DIR_CHATS: &str = "\u{1}chats";
const DIR_CHAT: &str = "\u{1}chat";
const DIR_MONTH: &str = "\u{1}month";

/// The folder of chats as it shows at the top of the account, named `name`.
pub fn folder_item(name: String) -> Item {
    Item { name, dir: Some(DIR_CHATS.into()), parent: crate::records::ROOT.into(), ..Item::default() }
}

/// The chats of an account, as far as read. It never asks Telegram itself: it says what it
/// needs ([`Need`], [`Chats::wanted_more`]) and the account asks with no lock held, so a listing
/// waiting on Telegram never holds up another.
#[derive(Default)]
pub(crate) struct Chats {
    kept: Kept,
    /// The first page was read, or the chats came from disk.
    loaded: bool,
    /// The chats kept on disk were looked for.
    pub(crate) disk_tried: bool,
    /// When the most recent chats were last read.
    refreshed: Option<Instant>,
    /// Until when (seconds since 1970) Telegram asked the listing to wait.
    pub(crate) not_before: i64,
    /// The folder names, made from `kept.list` when first needed: (name, index in the list, or
    /// `None` for Saved Messages before the listing reached it).
    names: Option<Vec<(String, Option<usize>)>>,
    months: HashMap<ChatRef, (Vec<Month>, Instant)>,
    files: HashMap<(ChatRef, Month), (Vec<ChatFile>, Instant)>,
    /// Changed since kept on disk.
    pub(crate) dirty: bool,
    /// Someone looked at the chats this run: only then do the rest of them get read.
    opened: bool,
}

/// Saved Messages before the listing reached it: Telegram knows it by the session alone.
const SAVED: ChatRef = ChatRef { kind: Kind::Saved, id: 0, hash: 0 };

/// What a listing under the folder of chats needs before it can answer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Need {
    Ready(Vec<Item>),
    /// The first page of chats.
    First,
    Months(ChatRef),
    Files(ChatRef, Month),
    Missing,
    NotFolder,
}

impl Chats {
    /// The chats kept on disk.
    pub(crate) fn adopt(&mut self, kept: Kept) {
        if !self.loaded && !kept.list.is_empty() {
            self.kept = kept;
            self.loaded = true;
            self.names = None;
        }
    }

    pub(crate) fn kept(&self) -> &Kept {
        &self.kept
    }

    /// Adds or updates `chats`: how many were new, and whether a name shown changed.
    fn merge(&mut self, chats: Vec<Chat>) -> (usize, bool) {
        let (mut new, mut changed) = (0, false);
        for c in chats {
            match self.kept.list.iter_mut().find(|k| k.at.id == c.at.id && k.at.kind == c.at.kind) {
                Some(k) if *k == c => {}
                Some(k) => {
                    changed |= k.name != c.name;
                    *k = c;
                    self.dirty = true;
                }
                None => {
                    self.kept.list.push(c);
                    new += 1;
                    self.dirty = true;
                }
            }
        }
        if new > 0 || changed {
            self.names = None;
        }
        (new, new > 0 || changed)
    }

    /// The first page of chats, read because someone is looking.
    pub(crate) fn put_first(&mut self, p: Page) {
        if !self.loaded {
            self.kept.next = Some(p.next.unwrap_or_else(Cursor::archive));
            self.kept.complete = false;
            self.dirty = true;
        }
        self.merge(p.chats);
        self.loaded = true;
        self.refreshed = Some(Instant::now());
    }

    /// What reading more of the chats would ask for now: the next page (`Some(Some(cursor))`),
    /// the most recent again once all were read and a while went by (`Some(None)`), or nothing.
    /// Only after someone opened the chats this run, and not while Telegram asks to wait.
    pub(crate) fn wanted_more(&self) -> Option<Option<Cursor>> {
        if !self.opened || !self.loaded || crate::account::now() < self.not_before {
            return None;
        }
        if !self.kept.complete {
            return Some(self.kept.next.clone());
        }
        self.refreshed.is_none_or(|r| r.elapsed() >= AGAIN).then_some(None)
    }

    /// What Telegram answered to [`Chats::wanted_more`]'s `asked`: whether anything shown changed,
    /// and whether to go on reading. A page that brings no new chat, or leaves the listing where
    /// it was, ends it: the listing never goes round the same pages.
    pub(crate) fn put_more(&mut self, asked: Option<Cursor>, paged: Paged) -> (bool, bool) {
        let p = match paged {
            Paged::WaitUntil(until) => {
                self.not_before = until;
                return (false, false);
            }
            Paged::Page(p) => p,
        };
        let Some(asked) = asked else {
            self.refreshed = Some(Instant::now());
            return (self.merge(p.chats).1, false);
        };
        let stale = self.kept.next.as_ref() != Some(&asked);
        let next = p.next.clone();
        let (new, changed) = self.merge(p.chats);
        if !stale {
            // A page that brings no new chat, or stays where it was, ends its list; the end of
            // the main list goes on to the archive.
            let stuck = new == 0 || next.as_ref() == Some(&asked);
            self.kept.next = match (stuck, next) {
                (false, Some(n)) => Some(n),
                _ if !asked.archived => Some(Cursor::archive()),
                _ => None,
            };
            self.kept.complete = self.kept.next.is_none();
            self.dirty = true;
        }
        (changed, !self.kept.complete)
    }

    /// The folder names of the chats, each unique: Saved Messages first, the others in the order
    /// they were first read, so a twin read later is the one numbered and a name always leads
    /// to the same chat.
    fn names(&mut self) -> &[(String, Option<usize>)] {
        if self.names.is_none() {
            let saved = self.kept.list.iter().position(|c| c.at.kind == Kind::Saved);
            let mut out: Vec<(String, Option<usize>)> = vec![(saved_name().into(), saved)];
            let mut taken: HashSet<String> = SAVED_NAMES.iter().map(|n| n.to_lowercase()).collect();
            for (i, c) in self.kept.list.iter().enumerate() {
                if c.at.kind == Kind::Saved {
                    continue;
                }
                let base = if c.name.trim().is_empty() { t("Conta excluída", "Deleted account").to_string() } else { shown_name(&c.name) };
                let mut name = base.clone();
                let mut n = 1;
                while !taken.insert(name.to_lowercase()) {
                    n += 1;
                    name = numbered(&base, n, true);
                }
                out.push((name, Some(i)));
            }
            self.names = Some(out);
        }
        self.names.as_deref().unwrap_or_default()
    }

    /// The chat folder `name` names.
    pub(crate) fn chat(&mut self, name: &str) -> Option<ChatRef> {
        let low = name.to_lowercase();
        let saved = is_one_of(&SAVED_NAMES, name);
        let (_, at) = self.names().iter().find(|(n, _)| n.to_lowercase() == low || (saved && n == saved_name()))?.clone();
        Some(match at {
            Some(i) => self.kept.list[i].at,
            None => SAVED,
        })
    }

    fn chat_items(&mut self) -> Vec<Item> {
        let names = self.names().to_vec();
        names
            .into_iter()
            .map(|(name, at)| Item {
                modified: at.map_or(0, |i| self.kept.list[i].date),
                name,
                dir: Some(DIR_CHAT.into()),
                parent: DIR_CHATS.into(),
                ..Item::default()
            })
            .collect()
    }

    pub(crate) fn put_months(&mut self, chat: ChatRef, mut m: Vec<Month>) {
        m.sort();
        m.dedup();
        m.reverse();
        self.months.insert(chat, (m, Instant::now()));
    }

    pub(crate) fn put_files(&mut self, chat: ChatRef, month: Month, mut f: Vec<ChatFile>) {
        f.sort_by_key(|f| f.msg);
        f.dedup_by_key(|f| f.msg);
        self.files.insert((chat, month), (f, Instant::now()));
    }

    /// A month's files as items, each name unique (the older keeps the name, as in the storage).
    fn file_items(files: &[ChatFile]) -> Vec<Item> {
        let mut taken: HashSet<String> = HashSet::new();
        files
            .iter()
            .map(|f| {
                let base = shown_name(&f.name);
                let mut name = base.clone();
                let mut n = 1;
                while !taken.insert(name.to_lowercase()) {
                    n += 1;
                    name = numbered(&base, n, false);
                }
                Item { msg: f.msg, name, parent: DIR_MONTH.into(), size: f.size, modified: f.date, ..Item::default() }
            })
            .collect()
    }

    fn month_item(m: Month) -> Item {
        Item { name: m.name(), dir: Some(DIR_MONTH.into()), parent: DIR_CHAT.into(), modified: m.range().0, ..Item::default() }
    }

    /// The items of the folder `segs` names under the folder of chats, or what's needed first.
    /// `fresh`: what was read over a minute ago is read again.
    pub(crate) fn plan(&mut self, segs: &[&str], fresh: bool) -> Need {
        self.opened = true;
        if !self.loaded {
            return Need::First;
        }
        let young = |at: &Instant| !fresh || at.elapsed() < FRESH;
        match segs {
            [] => Need::Ready(self.chat_items()),
            [chat] => {
                let Some(at) = self.chat(chat) else { return Need::Missing };
                match self.months.get(&at) {
                    Some((m, when)) if young(when) => Need::Ready(m.iter().copied().map(Chats::month_item).collect()),
                    _ => Need::Months(at),
                }
            }
            [chat, month] => {
                let (Some(at), Some(m)) = (self.chat(chat), Month::parse(month)) else { return Need::Missing };
                match self.files.get(&(at, m)) {
                    Some((f, when)) if young(when) => Need::Ready(Chats::file_items(f)),
                    _ => Need::Files(at, m),
                }
            }
            _ => Need::NotFolder,
        }
    }

    /// The items of the folder `segs` names as known now, with no request; `None` if not read yet.
    pub(crate) fn kept_list(&mut self, segs: &[&str]) -> Option<Vec<Item>> {
        if !self.loaded {
            return None;
        }
        match segs {
            [] => Some(self.chat_items()),
            [chat] => {
                let at = self.chat(chat)?;
                Some(self.months.get(&at)?.0.iter().copied().map(Chats::month_item).collect())
            }
            [chat, month] => {
                let at = self.chat(chat)?;
                let m = Month::parse(month)?;
                Some(Chats::file_items(&self.files.get(&(at, m))?.0))
            }
            _ => None,
        }
    }
}

/// What the first listing of chats answers when Telegram asks it to wait.
pub(crate) fn wait_until(until: i64) -> String {
    tr!(
        "O Telegram pediu para esperar até {} antes de listar as conversas.",
        "Telegram asked to wait until {} before listing the chats.",
        Local.timestamp_opt(until, 0).earliest().map_or_else(String::new, |d| d.format("%H:%M:%S").to_string())
    )
}

pub(crate) fn not_folder(segs: &[&str]) -> String {
    tr!("{} não é uma pasta.", "{} isn't a folder.", shown(segs))
}

pub(crate) fn is_folder(segs: &[&str]) -> String {
    tr!("{} é uma pasta.", "{} is a folder.", shown(segs))
}

fn shown(segs: &[&str]) -> String {
    let mut s = format!("/{}", folder_name());
    for p in segs {
        s.push('/');
        s.push_str(p);
    }
    s
}

pub(crate) fn missing(segs: &[&str]) -> String {
    tr!("{} não existe no Telegram.", "{} doesn't exist on Telegram.", shown(segs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_may_be_in_the_chats_by_its_first_folder_in_either_language_or_numbered() {
        assert_eq!(split_first("/Conversas do Telegram/Ana/2026-11"), Some(("Conversas do Telegram", vec!["Ana", "2026-11"])));
        assert_eq!(split_first("/"), None);
        assert!(may_name_folder("telegram CHATS"));
        assert!(may_name_folder("Conversas do Telegram (2)"));
        assert!(may_name_folder("Telegram chats (12)"));
        assert!(!may_name_folder("Conversas do Telegram (x)"));
        assert!(!may_name_folder("Conversas do Telegram ()"));
        assert!(!may_name_folder("Fotos (2)"));
        assert!(!may_name_folder("Fotos"));
    }

    #[test]
    fn months_name_sort_and_bound_in_local_time() {
        let m = Month { year: 2026, month: 11 };
        assert_eq!(m.name(), "2026-11");
        assert_eq!(Month::parse("2026-11"), Some(m));
        assert_eq!(Month::parse("2026-13"), None);
        assert_eq!(Month::parse("26-11"), None);
        let (start, end) = m.range();
        assert_eq!(Month::of(start), m);
        assert_eq!(Month::of(end - 1), m);
        assert_eq!(Month::of(end), Month { year: 2026, month: 12 });
        assert_eq!(Month { year: 2026, month: 12 }.range().1, Month { year: 2027, month: 1 }.range().0);
    }

    #[test]
    fn a_file_with_no_name_is_named_by_what_it_is_and_when() {
        let date = Month { year: 2026, month: 11 }.range().0 + 22 * 86400 + 3600;
        assert_eq!(file_name(Media::Document, "contrato.pdf", None, "application/pdf", date), "contrato.pdf");
        assert_eq!(file_name(Media::Photo, "", None, "", date), "Foto 2026-11-23 01.00.00.jpg");
        assert_eq!(file_name(Media::Voice, "", None, "audio/ogg", date), "Áudio 2026-11-23 01.00.00.ogg");
        assert_eq!(file_name(Media::Round, "", None, "video/mp4", date), "Vídeo redondo 2026-11-23 01.00.00.mp4");
        assert_eq!(file_name(Media::Music, "", Some(("Artista", "Faixa")), "audio/mpeg", date), "Artista - Faixa.mp3");
        assert_eq!(file_name(Media::Music, "", Some(("", "Faixa")), "audio/mp4", date), "Faixa.m4a");
        assert_eq!(file_name(Media::Video, "", None, "video/quicktime", date), "Vídeo 2026-11-23 01.00.00.mov");
        assert_eq!(extension("application/x-weird+stuff"), "bin");
    }

    fn chat(kind: Kind, id: i64, name: &str) -> Chat {
        Chat { at: ChatRef { kind, id, hash: 0 }, name: name.into(), date: 0 }
    }

    #[test]
    fn chat_folders_are_unique_saved_first_and_numbered_in_the_order_read() {
        let mut c = Chats::default();
        c.merge(vec![chat(Kind::User, 9, "Ana"), chat(Kind::Channel, 3, "Ana"), chat(Kind::Saved, 1, "Eu"), chat(Kind::Group, 5, "a/b"), chat(Kind::User, 2, ""), chat(Kind::User, 4, "Mensagens salvas")]);
        let names: Vec<String> = c.names().iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(names, ["Mensagens Salvas", "Ana", "Ana (2)", "a_b", "Conta excluída", "Mensagens salvas (2)"]);
        assert_eq!(c.chat("ana").map(|r| r.id), Some(9));
        assert_eq!(c.chat("Ana (2)").map(|r| r.id), Some(3));
        assert_eq!(c.chat("Saved Messages").map(|r| r.kind), Some(Kind::Saved));
        // A twin read later, with a lower id, takes the number: the names keep their chats.
        c.merge(vec![chat(Kind::User, 1, "Ana")]);
        assert_eq!(c.chat("Ana").map(|r| r.id), Some(9));
        assert_eq!(c.chat("Ana (3)").map(|r| r.id), Some(1));
    }

    fn page(chats: Vec<Chat>, next: Option<i32>) -> Paged {
        let next = next.map(|id| Cursor { date: 0, id, peer: SAVED, archived: false });
        Paged::Page(Page { chats, next })
    }

    #[test]
    fn the_listing_ends_when_a_page_brings_nothing_new_or_stays_put() {
        let mut c = Chats::default();
        assert_eq!(c.wanted_more(), None, "not before someone opens the chats");
        assert_eq!(c.plan(&[], true), Need::First);
        let Paged::Page(first) = page(vec![chat(Kind::User, 1, "A")], Some(1)) else { unreachable!() };
        c.put_first(first);
        let at = c.wanted_more().expect("more to read").expect("the next page");
        assert_eq!(c.put_more(Some(at.clone()), page(vec![chat(Kind::User, 2, "B")], Some(2))), (true, true));
        let at = c.wanted_more().unwrap().unwrap();
        assert_eq!(c.put_more(Some(at), page(vec![chat(Kind::User, 2, "B")], Some(3))), (false, true), "nothing new: on to the archive");
        let at = c.wanted_more().unwrap().unwrap();
        assert_eq!(at, Cursor::archive());
        let archived = |id| Some(Cursor { date: 0, id, peer: SAVED, archived: true });
        assert_eq!(c.put_more(Some(at), Paged::Page(Page { chats: vec![chat(Kind::User, 3, "C")], next: archived(9) })), (true, true));
        let at = c.wanted_more().unwrap().unwrap();
        assert_eq!(c.put_more(Some(at), Paged::Page(Page { chats: vec![chat(Kind::User, 4, "D")], next: None })), (true, false));
        assert_eq!(c.wanted_more(), None, "all read, and the most recent read a moment ago");
        assert_eq!(c.put_more(None, Paged::WaitUntil(crate::account::now() + 60)), (false, false));
    }

    #[test]
    fn twin_files_in_a_month_are_numbered_oldest_first() {
        let f = |msg, name: &str| ChatFile { msg, name: name.into(), size: 1, date: 0 };
        let items = Chats::file_items(&[f(1, "a.pdf"), f(2, "A.pdf"), f(3, "b")]);
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["a.pdf", "A (2).pdf", "b"]);
    }
}
