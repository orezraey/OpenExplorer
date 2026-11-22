//! What each message in the storage channel says about the tree: the one part of this crate that
//! can't change after a release, since it lives in people's channels, not in the app.
//!
//! # Format, version 1
//!
//! The last line of a message's text (a folder) or caption (a file) is its record; the lines
//! above are the item's name, so a person scrolling the channel in Telegram reads names:
//!
//! ```text
//! 📁 <name>                        a folder: a text message
//! #oe1 d <id> <parent>
//!
//! <name>                           a file: a message with a document
//! #oe1 f <parent> [<mtime>]
//!
//! <name>                           an empty file: a text message (Telegram takes no file of
//! #oe1 e <parent> <mtime>          0 bytes)
//!
//! <name>                           one part of a file too big for one message (Telegram
//! #oe1 p <parent> <mtime> <group> <index>/<count>    takes 2000 MiB, 4000 with Premium)
//! ```
//!
//! - `<id>` is a folder's own id, 16 hex digits drawn at random, never the message's id: a
//!   folder's record can be sent again (when an old message can't be edited) without losing
//!   what's in it.
//! - `<parent>` is the id of the folder an item is in, `0` for the top.
//! - `<group>` ties a file's parts together: 16 hex digits drawn at random. `<index>` counts
//!   from 1; the parts, in order, are the file's bytes. A file missing a part shows as incomplete.
//! - `<mtime>` is the file's modified time in seconds since 1970; without it (the proof of
//!   concept wrote none) the message's date stands in.
//! - Telegram trims spaces from the start of a message, so a name that starts with any (Windows
//!   allows it) has them in its record too, as a last field: `+` and their UTF-8 bytes as `%XX`
//!   (`#oe1 f 0 1700000000 +%20` for ` nota.txt`).
//! - A record of another version (`#oe2 …`) or kind belongs to a newer Open Explorer: the message
//!   is left out, not misread. A document with no record at all (sent by hand, forwarded from a
//!   phone) is a file at the top under the document's own name.
//!
//! The caption of a message holds 1024 characters (4096 with Premium); a name is at most 255
//! (Windows' limit), so a record always fits.

/// The tag that starts every record of this version.
pub const TAG: &str = "#oe1";
/// The parent of the items at the top.
pub const ROOT: &str = "0";
/// What a folder's text starts with.
const FOLDER_MARK: &str = "📁 ";
/// What the storage channel's description holds, to find it again from another PC.
pub const CHANNEL_MARK: &str = "#oe1-canal";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    Dir { id: String, parent: String, name: String },
    File { parent: String, name: String, mtime: Option<i64> },
    /// A file of 0 bytes, which Telegram can't hold as a document.
    Empty { parent: String, name: String, mtime: i64 },
    /// One part of a file split in several messages.
    Part { parent: String, name: String, mtime: i64, group: String, index: u32, count: u32 },
    /// A record of a newer version or an unknown kind: left out of the tree.
    Unknown,
    /// No record: a message written by hand.
    None,
}

/// A folder's text.
pub fn dir_text(name: &str, id: &str, parent: &str) -> String {
    format!("{FOLDER_MARK}{name}\n{TAG} d {id} {parent}{}", lead_field(name))
}

/// A file's caption.
pub fn file_text(name: &str, parent: &str, mtime: i64) -> String {
    format!("{name}\n{TAG} f {parent} {mtime}{}", lead_field(name))
}

/// A file part's caption: part `index` (from 1) of `count`.
pub fn part_text(name: &str, parent: &str, mtime: i64, group: &str, index: u32, count: u32) -> String {
    format!("{name}\n{TAG} p {parent} {mtime} {group} {index}/{count}{}", lead_field(name))
}

/// An empty file's text.
pub fn empty_text(name: &str, parent: &str, mtime: i64) -> String {
    format!("{name}\n{TAG} e {parent} {mtime}{}", lead_field(name))
}

/// ` +%20%20` for a name starting with two spaces; nothing for one starting otherwise.
fn lead_field(name: &str) -> String {
    let lead = &name[..name.len() - name.trim_start().len()];
    if lead.is_empty() {
        return String::new();
    }
    let hex: String = lead.bytes().map(|b| format!("%{b:02X}")).collect();
    format!(" +{hex}")
}

/// `+%20%20` → two spaces.
fn unlead(field: &str) -> Option<String> {
    let hex = field.strip_prefix('+')?;
    let mut bytes = Vec::new();
    let mut rest = hex;
    while !rest.is_empty() {
        let pair = rest.strip_prefix('%')?.get(..2)?;
        bytes.push(u8::from_str_radix(pair, 16).ok()?);
        rest = &rest[3..];
    }
    let lead = String::from_utf8(bytes).ok()?;
    (!lead.is_empty() && lead.trim_start().is_empty()).then_some(lead)
}

/// A name as written, its leading spaces (trimmed by Telegram or not) put back from the record.
fn named(body: &str, lead: &Option<String>) -> String {
    match lead {
        Some(lead) => format!("{lead}{}", body.trim_start()),
        None => body.to_string(),
    }
}

/// A new folder id: 16 hex digits.
pub fn new_id() -> String {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("the OS gives random bytes");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn is_id(s: &str) -> bool {
    s == ROOT || (s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// What a message's text says.
pub fn parse(text: &str) -> Record {
    let (body, last) = text.rsplit_once('\n').unwrap_or(("", text));
    let words: Vec<&str> = last.split(' ').collect();
    let Some(first) = words.first() else { return Record::None };
    if !first.starts_with("#oe") || first.len() < 4 || !first[3..].bytes().all(|b| b.is_ascii_digit()) {
        return Record::None;
    }
    if *first != TAG {
        return Record::Unknown;
    }
    if body.is_empty() {
        return Record::Unknown;
    }
    let (fields, lead) = match words.last() {
        Some(w) if w.starts_with('+') => match unlead(w) {
            Some(lead) => (&words[1..words.len() - 1], Some(lead)),
            None => return Record::Unknown,
        },
        _ => (&words[1..], None),
    };
    match *fields {
        ["d", id, parent] if is_id(id) && id != ROOT && is_id(parent) => {
            Record::Dir { id: id.into(), parent: parent.into(), name: named(body.strip_prefix(FOLDER_MARK).unwrap_or(body), &lead) }
        }
        ["f", parent] if is_id(parent) && lead.is_none() => Record::File { parent: parent.into(), name: body.into(), mtime: None },
        ["f", parent, mtime] if is_id(parent) => match mtime.parse() {
            Ok(m) => Record::File { parent: parent.into(), name: named(body, &lead), mtime: Some(m) },
            Err(_) => Record::Unknown,
        },
        ["e", parent, mtime] if is_id(parent) => match mtime.parse() {
            Ok(m) => Record::Empty { parent: parent.into(), name: named(body, &lead), mtime: m },
            Err(_) => Record::Unknown,
        },
        ["p", parent, mtime, group, of] if is_id(parent) => {
            let parsed = (|| {
                let (index, count) = of.split_once('/')?;
                let (index, count): (u32, u32) = (index.parse().ok()?, count.parse().ok()?);
                (is_id(group) && group != ROOT && (1..=count).contains(&index)).then_some(())?;
                Some(Record::Part { parent: parent.into(), name: named(body, &lead), mtime: mtime.parse().ok()?, group: group.into(), index, count })
            })();
            parsed.unwrap_or(Record::Unknown)
        }
        _ => Record::Unknown,
    }
}

/// Whether `name` can be an item's name: what Windows allows in a file name, as the files come
/// down to Windows folders.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 255
        && name != "."
        && name != ".."
        && !name.ends_with(' ')
        && !name.ends_with('.')
        && !name.chars().any(|c| c < ' ' || r#"<>:"/\|?*"#.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "00112233aabbccdd";

    #[test]
    fn records_come_back_as_written() {
        assert_eq!(parse(&dir_text("Fotos 2026", A, ROOT)), Record::Dir { id: A.into(), parent: ROOT.into(), name: "Fotos 2026".into() });
        assert_eq!(parse(&file_text("nota.txt", A, 1_700_000_000)), Record::File { parent: A.into(), name: "nota.txt".into(), mtime: Some(1_700_000_000) });
        assert_eq!(parse(&empty_text("vazio.txt", ROOT, 3)), Record::Empty { parent: ROOT.into(), name: "vazio.txt".into(), mtime: 3 });
        // What the proof of concept wrote.
        assert_eq!(parse(&format!("a.bin\n#oe1 f {A}")), Record::File { parent: A.into(), name: "a.bin".into(), mtime: None });
        assert_eq!(parse(&part_text(" grande.iso", A, 5, A, 2, 3)), Record::Part { parent: A.into(), name: " grande.iso".into(), mtime: 5, group: A.into(), index: 2, count: 3 });
        assert_eq!(parse(&format!("g\n#oe1 p {A} 5 {A} 4/3")), Record::Unknown);
        assert_eq!(
            parse(&format!("grande.iso\n#oe1 p {A} 5 {A} 2/3")),
            Record::Part { parent: A.into(), name: "grande.iso".into(), mtime: 5, group: A.into(), index: 2, count: 3 }
        );
    }

    #[test]
    fn a_newer_or_broken_record_is_left_out_and_plain_text_is_no_record() {
        assert_eq!(parse(&format!("x\n#oe2 f {A} 5")), Record::Unknown);
        assert_eq!(parse(&format!("x\n#oe1 z {A}")), Record::Unknown);
        assert_eq!(parse("x\n#oe1 f nothex 5"), Record::Unknown);
        assert_eq!(parse(&format!("x\n#oe1 f {A} amanhã")), Record::Unknown);
        assert_eq!(parse(&format!("#oe1 f {A} 5")), Record::Unknown);
        assert_eq!(parse(&format!("📁 x\n#oe1 d 0 {A}")), Record::Unknown);
        assert_eq!(parse("Open Explorer (PoC): mensagem de teste.\n#oe1 sonda"), Record::Unknown);
        assert_eq!(parse("olá"), Record::None);
        assert_eq!(parse("#oeste"), Record::None);
        assert_eq!(parse(""), Record::None);
    }

    #[test]
    fn leading_spaces_survive_telegram_trimming_them() {
        let trimmed = |t: String| t.trim_start().to_string();
        for name in [" nota.txt", "  \u{3000}dois.txt", "\u{a0}x"] {
            let want = Record::File { parent: A.into(), name: name.into(), mtime: Some(5) };
            assert_eq!(parse(&file_text(name, A, 5)), want);
            assert_eq!(parse(&trimmed(file_text(name, A, 5))), want);
            assert_eq!(parse(&trimmed(empty_text(name, A, 5))), Record::Empty { parent: A.into(), name: name.into(), mtime: 5 });
            assert_eq!(parse(&dir_text(name, A, ROOT)), Record::Dir { id: A.into(), parent: ROOT.into(), name: name.into() });
        }
        assert_eq!(file_text("nota.txt", A, 5), format!("nota.txt\n#oe1 f {A} 5"));
        assert_eq!(parse(&format!("x\n#oe1 f {A} 5 +%2")), Record::Unknown);
        assert_eq!(parse(&format!("x\n#oe1 f {A} 5 +%41")), Record::Unknown);
    }

    #[test]
    fn a_name_may_hold_new_lines_but_the_record_is_the_last_line() {
        assert_eq!(parse(&file_text("a\nb", ROOT, 1)), Record::File { parent: ROOT.into(), name: "a\nb".into(), mtime: Some(1) });
    }

    #[test]
    fn names_follow_windows() {
        assert!(valid_name("relatório final.pdf"));
        assert!(valid_name(&"a".repeat(255)));
        for bad in ["", ".", "..", "a/b", "a\\b", "a:b", "a?", "fim.", "fim ", "a\nb", &"a".repeat(256)] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn ids_are_sixteen_hex_digits() {
        let id = new_id();
        assert!(is_id(&id) && id != ROOT && id != new_id());
    }
}
