//! Dropbox's metadata as the crate keeps it: one [`Entry`] per file or folder, read from the
//! `FileMetadata` / `FolderMetadata` / `DeletedMetadata` objects every route answers with.
//!
//! Dropbox paths are case-insensitive: `path_lower` names an item, `path_display` is how it's
//! shown. The account's root is the empty path `""`, never `/`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A file or a folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    #[serde(rename = "n")]
    pub name: String,
    /// `path_lower`: what names the item.
    #[serde(rename = "l")]
    pub lower: String,
    #[serde(rename = "d", default, skip_serializing_if = "std::ops::Not::not")]
    pub dir: bool,
    #[serde(rename = "s", default)]
    pub size: u64,
    /// Seconds since 1970: the file's own time (`client_modified`), else when it reached Dropbox.
    #[serde(rename = "m", default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<i64>,
    /// The file's revision, to restore it or read exactly this version.
    #[serde(rename = "r", default, skip_serializing_if = "String::is_empty")]
    pub rev: String,
    /// Dropbox's `content_hash` (see [`crate::content`]).
    #[serde(rename = "h", default, skip_serializing_if = "String::is_empty")]
    pub hash: String,
}

/// What one entry of a listing or of the change log says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// The item is there, as it is now.
    Is(Entry),
    /// Nothing is at this path (lowercased) any more.
    Gone(String),
}

impl Change {
    pub fn lower(&self) -> &str {
        match self {
            Change::Is(e) => &e.lower,
            Change::Gone(l) => l,
        }
    }
}

/// Reads a metadata object. `None` for what has no path (an item not mounted in this account).
///
/// A route declared to answer `Metadata` tags it (`"file"`, `"folder"`, `"deleted"`); one
/// declared `FileMetadata` or `FolderMetadata` (upload, restore, a new folder) doesn't, by
/// Stone's JSON rules, and a batch entry carries its own tag (`"success"`) instead: then what it
/// is shows in its fields, a file's `rev` and `size`.
pub fn change(v: &Value) -> Option<Change> {
    let s = |k: &str| v.get(k).and_then(Value::as_str);
    let lower = s("path_lower")?.to_string();
    let name = s("name").map(str::to_string).unwrap_or_else(|| last(&lower).to_string());
    let tag = match s(".tag") {
        Some(t @ ("file" | "folder" | "deleted")) => t,
        _ if v.get("rev").is_some() || v.get("size").is_some() => "file",
        _ => "folder",
    };
    Some(match tag {
        "deleted" => Change::Gone(lower),
        "folder" => Change::Is(Entry { name, lower, dir: true, size: 0, modified: None, rev: String::new(), hash: String::new() }),
        "file" => Change::Is(Entry {
            name,
            lower,
            dir: false,
            size: v.get("size").and_then(Value::as_u64).unwrap_or(0),
            modified: s("client_modified").or(s("server_modified")).and_then(seconds),
            rev: s("rev").unwrap_or("").into(),
            hash: s("content_hash").unwrap_or("").into(),
        }),
        _ => return None,
    })
}

/// [`change`] of something that must be there: a file or a folder.
pub fn entry(v: &Value) -> Option<Entry> {
    match change(v)? {
        Change::Is(e) => Some(e),
        Change::Gone(_) => None,
    }
}

/// The last segment of a path.
pub fn last(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The folder a lowercased path is in: `"/a/b"` → `"/a"`, `"/a"` → `""`.
pub fn parent(lower: &str) -> &str {
    match lower.rfind('/') {
        Some(i) => &lower[..i],
        None => "",
    }
}

/// The API's path for an app path: `/` (the top) is `""`; anything else as it is, without a
/// trailing `/`.
pub fn api_path(path: &str) -> String {
    let p = path.trim_end_matches('/');
    if p.is_empty() { String::new() } else if p.starts_with('/') { p.to_string() } else { format!("/{p}") }
}

/// The key a folder's listing is kept under: its API path, lowercased.
pub fn key(path: &str) -> String {
    api_path(path).to_lowercase()
}

/// Seconds since 1970 of `2015-05-12T15:50:38Z`.
pub fn seconds(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| text.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Days from 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// `2015-05-12T15:50:38Z` of seconds since 1970.
pub fn timestamp(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn metadata_reads_as_entries_and_changes() {
        let file = json!({".tag": "file", "name": "Prime_Numbers.txt", "path_lower": "/homework/math/prime_numbers.txt", "path_display": "/Homework/math/Prime_Numbers.txt", "id": "id:a4ayc_80_OEAAAAAAAAAXw", "client_modified": "2015-05-12T15:50:38Z", "server_modified": "2015-05-12T15:50:38Z", "rev": "a1c10ce0dd78", "size": 7212, "content_hash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"});
        let e = entry(&file).unwrap();
        assert_eq!((e.name.as_str(), e.lower.as_str(), e.dir, e.size, e.rev.as_str()), ("Prime_Numbers.txt", "/homework/math/prime_numbers.txt", false, 7212, "a1c10ce0dd78"));
        assert_eq!(e.modified, Some(1_431_445_838));
        let dir = json!({".tag": "folder", "name": "math", "path_lower": "/homework/math", "id": "id:x"});
        assert!(entry(&dir).unwrap().dir);
        assert_eq!(change(&json!({".tag": "deleted", "name": "x", "path_lower": "/a/x"})), Some(Change::Gone("/a/x".into())));
        // Not mounted here: no path.
        assert_eq!(change(&json!({".tag": "folder", "name": "fora"})), None);
        // Untagged, as upload and restore answer, or tagged as a batch entry.
        let mut bare = file.clone();
        bare.as_object_mut().unwrap().remove(".tag");
        assert_eq!(entry(&bare), Some(e.clone()));
        bare[".tag"] = json!("success");
        assert_eq!(entry(&bare), Some(e));
        assert!(entry(&json!({"name": "math", "path_lower": "/homework/math", "id": "id:x"})).unwrap().dir);
    }

    #[test]
    fn paths_map_to_the_api_and_back() {
        assert_eq!(api_path("/"), "");
        assert_eq!(api_path(""), "");
        assert_eq!(api_path("/Fotos/2024/"), "/Fotos/2024");
        assert_eq!(key("/Fotos/Ação"), "/fotos/ação");
        assert_eq!(parent("/a/b"), "/a");
        assert_eq!(parent("/a"), "");
        assert_eq!(last("/a/b.txt"), "b.txt");
    }

    #[test]
    fn times_go_both_ways() {
        for s in [0, 1_431_445_838, 951_782_400 /* 2000-02-29 */, 4_102_444_800] {
            assert_eq!(seconds(&timestamp(s)), Some(s));
        }
        assert_eq!(timestamp(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(seconds("nada"), None);
    }
}
