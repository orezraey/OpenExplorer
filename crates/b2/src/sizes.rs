//! How much each bucket and each folder in it holds, which B2's API doesn't say (its website
//! does): every version in the bucket listed (`b2_list_file_versions`, 10,000 a request, free as
//! every listing) and summed folder by folder, as the B2 command line's `get-bucket --showSize`
//! does. Two sums each: the bytes of the current files (what the panes show), and of every
//! version kept, hidden ones too (what the website shows and Backblaze bills).
//!
//! Kept per bucket for [`FRESH_FOR`]; the app's own changes in a bucket mark its sums stale, and
//! they're summed again at its next opening, while the old ones still show.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use openexplorer_i18n::tr;
use serde_json::{Value, json};

use crate::Result;
use crate::account::{Account, EMPTY_MARKER, PAGE, Version, Within, within};

/// How long a bucket's sums stand before an opening sums it again.
pub const FRESH_FOR: Duration = Duration::from_secs(10 * 60);
/// Pages of versions summed at most (10,000 each): past it the bucket is too big to sum on
/// every opening, and no size is shown.
const MOST_PAGES: usize = 300;

/// A bucket's sums.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sizes {
    /// By folder (`a`, `a/b`, without the trailing slash): bytes of its current files and of
    /// every version under it.
    folders: HashMap<String, (u64, u64)>,
    /// The whole bucket's.
    pub current: u64,
    pub stored: u64,
    /// Current files.
    pub files: u64,
}

impl Sizes {
    /// The sums of the folder whose names start with `prefix` (`a/b/`, `""` for the bucket):
    /// current files' bytes, every version's.
    pub fn of(&self, prefix: &str) -> Option<(u64, u64)> {
        let folder = prefix.trim_end_matches('/');
        if folder.is_empty() { Some((self.current, self.stored)) } else { self.folders.get(folder).copied() }
    }

    /// Adds version `v`, `newest` when it's its name's newest.
    fn add(&mut self, v: &Version, newest: bool) {
        let current = if newest && v.action == "upload" { v.size } else { 0 };
        let stored = if v.action == "folder" { 0 } else { v.size };
        if current == 0 && stored == 0 && !(newest && v.action == "upload") {
            return;
        }
        self.current += current;
        self.stored += stored;
        if newest && v.action == "upload" && !v.name.ends_with(EMPTY_MARKER) {
            self.files += 1;
        }
        for (i, _) in v.name.match_indices('/') {
            let sums = self.folders.entry(v.name[..i].to_string()).or_default();
            sums.0 += current;
            sums.1 += stored;
        }
    }
}

/// A bucket's sums as kept, and whether they still stand.
#[derive(Clone)]
pub(crate) struct Kept {
    sizes: Arc<Sizes>,
    at: Instant,
    stale: bool,
}

impl Account {
    /// Bucket `bucket`'s sums as last made, however old: what shows at once.
    pub fn kept_sizes(&self, bucket: &str) -> Option<Arc<Sizes>> {
        self.sizes.lock().unwrap_or_else(|e| e.into_inner()).get(bucket).map(|k| k.sizes.clone())
    }

    /// Bucket `bucket`'s sums: the kept ones while they stand, else summed now (one at a time
    /// per key: a second asker waits and takes the first one's).
    pub fn sizes(&self, bucket: &str) -> Result<Arc<Sizes>> {
        let standing = |me: &Account| me.sizes.lock().unwrap_or_else(|e| e.into_inner()).get(bucket).filter(|k| !k.stale && k.at.elapsed() < FRESH_FOR).map(|k| k.sizes.clone());
        if let Some(s) = standing(self) {
            return Ok(s);
        }
        let _one = self.sizing.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = standing(self) {
            return Ok(s);
        }
        let started = Instant::now();
        let sizes = Arc::new(self.sum(bucket)?);
        crate::api::log(&format!("B2: tamanhos de {bucket} somados em {} ms ({} arquivos)", started.elapsed().as_millis(), sizes.files));
        self.sizes.lock().unwrap_or_else(|e| e.into_inner()).insert(bucket.to_string(), Kept { sizes: sizes.clone(), at: Instant::now(), stale: false });
        Ok(sizes)
    }

    /// The app changed something in `bucket`: its sums are summed again at its next opening.
    pub(crate) fn sizes_changed(&self, bucket: &str) {
        if let Some(k) = self.sizes.lock().unwrap_or_else(|e| e.into_inner()).get_mut(bucket) {
            k.stale = true;
        }
    }

    /// Every version of `bucket` (within the key's prefix, for a key limited to one), summed.
    fn sum(&self, bucket: &str) -> Result<Sizes> {
        let b = self.bucket(bucket)?;
        let auth = self.api.auth()?;
        let prefix = match (within(auth.allowed.name_prefix.as_deref(), ""), auth.allowed.name_prefix.as_deref()) {
            (Within::Inside, _) | (_, None) => String::new(),
            (_, Some(np)) => np.to_string(),
        };
        let mut sizes = Sizes::default();
        let mut last = String::new();
        let mut start: Option<(String, Option<String>)> = None;
        for _ in 0..MOST_PAGES {
            let mut body = json!({"bucketId": b.id, "prefix": prefix, "maxFileCount": PAGE});
            if let Some((name, id)) = &start {
                body["startFileName"] = name.clone().into();
                if let Some(id) = id {
                    body["startFileId"] = id.clone().into();
                }
            }
            let v = self.api.run("b2_list_file_versions", &body)?;
            // Each name's versions come together, newest first.
            for ver in v.get("files").and_then(Value::as_array).into_iter().flatten().filter_map(Version::of) {
                let newest = ver.name != last;
                if newest {
                    last.clone_from(&ver.name);
                }
                sizes.add(&ver, newest);
            }
            match v.get("nextFileName").and_then(Value::as_str) {
                Some(n) => start = Some((n.to_string(), v.get("nextFileId").and_then(Value::as_str).map(str::to_string))),
                None => return Ok(sizes),
            }
        }
        Err(tr!("O bucket “{bucket}” tem versões demais para somar o tamanho.", "The bucket “{bucket}” has too many versions to add up its size."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(name: &str, action: &str, size: u64) -> Version {
        Version { id: String::new(), name: name.into(), action: action.into(), size, sha1: None, content_type: None, info: Default::default(), uploaded: 0 }
    }

    #[test]
    fn folders_sum_their_current_files_and_every_version() {
        let mut s = Sizes::default();
        // a/b/x.jpg: current 10, an older version 4; a/y.txt hidden (its last upload 7 kept);
        // c/.bzEmpty, an empty folder.
        for (ver, newest) in [
            (v("a/b/x.jpg", "upload", 10), true),
            (v("a/b/x.jpg", "upload", 4), false),
            (v("a/y.txt", "hide", 0), true),
            (v("a/y.txt", "upload", 7), false),
            (v("c/.bzEmpty", "upload", 0), true),
            (v("z.bin", "upload", 100), true),
        ] {
            s.add(&ver, newest);
        }
        assert_eq!(s.of(""), Some((110, 121)));
        assert_eq!(s.of("a/"), Some((10, 21)));
        assert_eq!(s.of("a/b"), Some((10, 14)));
        assert_eq!(s.of("c/"), Some((0, 0)));
        assert_eq!(s.of("nada/"), None);
        assert_eq!(s.files, 2);
    }
}
