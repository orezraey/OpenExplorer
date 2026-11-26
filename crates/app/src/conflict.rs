//! Unified file conflict prompt and thread-safe batch resolution across all transfer paths.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use openexplorer_gdrive::DriveFile;
use openexplorer_remote::Item;

use crate::model::{Entry, Kind};
use crate::transfers::Op;

/// Metadata for either the source or destination item shown in the conflict dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInfo {
    pub name: String,
    pub place: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub kind: Kind,
}

impl FileInfo {
    /// Reads metadata for a local file or folder on disk.
    pub fn local(path: &Path) -> Option<FileInfo> {
        let meta = std::fs::metadata(path).ok()?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let place = path.parent().map(|p| p.display().to_string()).unwrap_or_default();
        let is_dir = meta.is_dir();
        let size = if is_dir { None } else { Some(meta.len()) };
        let modified = meta.modified().ok();
        let kind = kind_of(&name, is_dir, None);
        Some(FileInfo { name, place, is_dir, size, modified, kind })
    }

    /// Builds a [`FileInfo`] from a pane/folder [`Entry`].
    pub fn entry(e: &Entry, place: String) -> FileInfo {
        FileInfo {
            name: e.name.clone(),
            place,
            is_dir: e.is_dir,
            size: if e.is_dir { None } else { e.size },
            modified: e.modified,
            kind: e.kind(),
        }
    }

    /// Builds a [`FileInfo`] from a Google Drive [`DriveFile`].
    pub fn drive(f: &DriveFile, place: String) -> FileInfo {
        let is_dir = f.is_dir;
        let kind = kind_of(&f.name, is_dir, Some(&f.mime));
        FileInfo {
            name: f.name.clone(),
            place,
            is_dir,
            size: if is_dir { None } else { f.size },
            modified: f.modified,
            kind,
        }
    }

    /// Builds a [`FileInfo`] from a remote server [`Item`].
    pub fn remote(item: &Item, place: String) -> FileInfo {
        FileInfo {
            name: item.name.clone(),
            place,
            is_dir: item.is_dir,
            size: if item.is_dir { None } else { item.size },
            modified: item.modified,
            kind: kind_of(&item.name, item.is_dir, None),
        }
    }
}

/// Resolves an item's [`Kind`] from its name, folder flag and optional Drive MIME type.
pub fn kind_of(name: &str, is_dir: bool, mime: Option<&str>) -> Kind {
    Entry {
        id: String::new(),
        name: name.to_string(),
        is_dir,
        size: None,
        modified: None,
        target: None,
        mime: mime.map(str::to_string),
        md5: None,
        starred: false,
    }
    .kind()
}

/// What the user clicked in the conflict dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Overwrite / send new version.
    Replace,
    /// Skip this item, keeping destination untouched.
    Skip,
    /// Keep both files by numbering the incoming item.
    KeepBoth,
    /// Replace only when source is strictly newer than destination; otherwise skip.
    ReplaceNewer,
}

/// Reply sent from the UI back to the waiting worker thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    Choose { choice: Choice, apply_all: bool },
    Cancel,
}

/// Concrete per-item decision returned to a transfer worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Replace,
    Skip,
    KeepBoth,
    Cancel,
}

/// One conflict prompt shown in [`crate::actions::Dialog::Conflict`].
pub struct Prompt {
    pub op: Op,
    pub src: FileInfo,
    pub dst: FileInfo,
    pub renamed: String,
    pub may_have_more: bool,
    pub apply_all: bool,
    pub reply: Option<Sender<Reply>>,
    pub live: Option<Arc<AtomicBool>>,
}

impl Prompt {
    /// Whether the waiting worker gave up (e.g. its row was cancelled externally).
    pub fn is_stale(&self) -> bool {
        self.live.as_ref().is_some_and(|l| !l.load(Ordering::Relaxed))
    }

    /// Sends cancellation to the waiting worker, if any.
    pub fn send_cancel(self) {
        if let Some(tx) = self.reply {
            let _ = tx.send(Reply::Cancel);
        }
    }
}

/// Whether `src` is at least 1 second newer than `dst` (avoids sub-second FS/cloud rounding).
pub fn is_newer(src: Option<SystemTime>, dst: Option<SystemTime>) -> bool {
    let (Some(s), Some(d)) = (src, dst) else { return false };
    s.duration_since(d).is_ok_and(|diff| diff >= Duration::from_secs(1))
}

/// Maps a user [`Choice`] and timestamps to a concrete [`Decision`].
pub fn apply_choice(choice: Choice, src_mod: Option<SystemTime>, dst_mod: Option<SystemTime>) -> Decision {
    match choice {
        Choice::Replace => Decision::Replace,
        Choice::Skip => Decision::Skip,
        Choice::KeepBoth => Decision::KeepBoth,
        Choice::ReplaceNewer => {
            if is_newer(src_mod, dst_mod) {
                Decision::Replace
            } else {
                Decision::Skip
            }
        }
    }
}

/// Generates `"stem (2).ext"`, `"stem (3).ext"` (or `"Folder (2)"` for dirs) until `!is_taken`.
pub fn numbered_name(name: &str, is_dir: bool, is_taken: impl Fn(&str) -> bool) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && !is_dir => name.split_at(i),
        _ => (name, ""),
    };
    (2..).map(|k| format!("{stem} ({k}){ext}")).find(|n| !is_taken(n)).expect("some number is free")
}

/// Picks a free numbered path in `dir` that does not exist on disk and fails `extra_taken`.
pub fn numbered_local(dir: &Path, name: &str, is_dir: bool, extra_taken: &dyn Fn(&Path) -> bool) -> PathBuf {
    let candidate = numbered_name(name, is_dir, |n| {
        let p = dir.join(n);
        p.exists() || extra_taken(&p)
    });
    dir.join(candidate)
}

pub type AskFn = Arc<dyn Fn(Prompt) + Send + Sync>;

struct ResolverState {
    policy: Option<Choice>,
    cancelled: bool,
}

/// Batch-scoped conflict resolver shared across all worker threads of a transfer job.
#[derive(Clone)]
pub struct Resolver {
    op: Op,
    may_have_more: bool,
    ask: AskFn,
    cancel_batch: Option<Arc<dyn Fn() + Send + Sync>>,
    inner: Arc<Mutex<ResolverState>>,
}

impl Resolver {
    /// Creates a resolver that asks the UI via `ask` and cancels the whole batch on Cancel.
    pub fn new(op: Op, may_have_more: bool, ask: AskFn, cancel_batch: Option<Arc<dyn Fn() + Send + Sync>>) -> Resolver {
        Resolver {
            op,
            may_have_more,
            ask,
            cancel_batch,
            inner: Arc::new(Mutex::new(ResolverState { policy: None, cancelled: false })),
        }
    }

    /// Creates a resolver with a pre-selected policy (for tests without a UI loop).
    #[cfg(test)]
    pub fn fixed(choice: Choice) -> Resolver {
        Resolver {
            op: Op::Copy,
            may_have_more: true,
            ask: Arc::new(|_| {}),
            cancel_batch: None,
            inner: Arc::new(Mutex::new(ResolverState { policy: Some(choice), cancelled: false })),
        }
    }

    /// Resolves a conflict for one item, blocking until the user answers if no batch policy
    /// is active yet. Concurrent workers serialize cleanly so only one prompt opens at a time.
    pub fn resolve(&self, src: FileInfo, dst: FileInfo, renamed: String, is_cancelled: impl Fn() -> bool) -> Decision {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.cancelled || is_cancelled() {
            return Decision::Cancel;
        }
        if let Some(policy) = g.policy {
            return apply_choice(policy, src.modified, dst.modified);
        }
        let (tx, rx) = mpsc::channel();
        let live = Arc::new(AtomicBool::new(true));
        (self.ask)(Prompt {
            op: self.op,
            src: src.clone(),
            dst: dst.clone(),
            renamed,
            may_have_more: self.may_have_more,
            apply_all: false,
            reply: Some(tx),
            live: Some(live.clone()),
        });
        loop {
            match rx.recv_timeout(Duration::from_millis(120)) {
                Ok(Reply::Choose { choice, apply_all }) => {
                    if apply_all || choice == Choice::ReplaceNewer {
                        g.policy = Some(choice);
                    }
                    return apply_choice(choice, src.modified, dst.modified);
                }
                Ok(Reply::Cancel) | Err(RecvTimeoutError::Disconnected) => {
                    g.cancelled = true;
                    drop(g);
                    if let Some(cancel) = &self.cancel_batch {
                        cancel();
                    }
                    return Decision::Cancel;
                }
                Err(RecvTimeoutError::Timeout) => {
                    if is_cancelled() {
                        live.store(false, Ordering::Relaxed);
                        return Decision::Cancel;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn info(name: &str, modified: Option<SystemTime>) -> FileInfo {
        FileInfo {
            name: name.into(),
            place: "C:\\Temp".into(),
            is_dir: false,
            size: Some(1024),
            modified,
            kind: Kind::Text,
        }
    }

    #[test]
    fn newer_threshold_and_choices_work_as_expected() {
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let half_sec = base + Duration::from_millis(500);
        let two_secs = base + Duration::from_secs(2);

        assert!(!is_newer(Some(base), Some(base)));
        assert!(!is_newer(Some(half_sec), Some(base)));
        assert!(is_newer(Some(two_secs), Some(base)));
        assert!(!is_newer(Some(base), Some(two_secs)));
        assert!(!is_newer(None, Some(base)));
        assert!(!is_newer(Some(base), None));

        assert_eq!(apply_choice(Choice::Replace, Some(base), Some(two_secs)), Decision::Replace);
        assert_eq!(apply_choice(Choice::Skip, Some(two_secs), Some(base)), Decision::Skip);
        assert_eq!(apply_choice(Choice::KeepBoth, Some(base), Some(base)), Decision::KeepBoth);
        assert_eq!(apply_choice(Choice::ReplaceNewer, Some(two_secs), Some(base)), Decision::Replace);
        assert_eq!(apply_choice(Choice::ReplaceNewer, Some(base), Some(base)), Decision::Skip);
        assert_eq!(apply_choice(Choice::ReplaceNewer, Some(base), Some(two_secs)), Decision::Skip);
    }

    #[test]
    fn numbered_naming_keeps_extensions_for_files_only() {
        let taken = ["a.txt", "a (2).txt", "pasta", "pasta (2)"];
        assert_eq!(numbered_name("a.txt", false, |n| taken.contains(&n)), "a (3).txt");
        assert_eq!(numbered_name("pasta.v1", true, |n| n == "pasta.v1"), "pasta.v1 (2)");
        assert_eq!(numbered_name("pasta", true, |n| taken.contains(&n)), "pasta (3)");

        let dir = std::env::temp_dir().join(format!("oe-conflict-num-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ep.mp4"), "1").unwrap();
        std::fs::write(dir.join("ep (2).mp4"), "2").unwrap();
        let extra = dir.join("ep (3).mp4");
        assert_eq!(numbered_local(&dir, "ep.mp4", false, &|p| p == extra), dir.join("ep (4).mp4"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolver_serializes_threads_and_applies_batch_policy() {
        let prompts = Arc::new(AtomicUsize::new(0));
        let p_count = prompts.clone();
        let resolver = Resolver::new(
            Op::Download,
            true,
            Arc::new(move |prompt| {
                p_count.fetch_add(1, Ordering::Relaxed);
                if let Some(tx) = prompt.reply {
                    let _ = tx.send(Reply::Choose { choice: Choice::Skip, apply_all: true });
                }
            }),
            None,
        );
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let results: Vec<Decision> = std::thread::scope(|scope| {
            (0..4)
                .map(|i| {
                    let r = resolver.clone();
                    scope.spawn(move || {
                        let name = format!("f{i}.mp4");
                        r.resolve(info(&name, Some(t0)), info(&name, Some(t0)), format!("f{i} (2).mp4"), || false)
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect()
        });
        assert_eq!(prompts.load(Ordering::Relaxed), 1);
        assert_eq!(results, [Decision::Skip; 4]);
    }

    #[test]
    fn replace_newer_sets_policy_automatically_for_subsequent_files() {
        let prompts = Arc::new(AtomicUsize::new(0));
        let p_count = prompts.clone();
        let resolver = Resolver::new(
            Op::Copy,
            true,
            Arc::new(move |prompt| {
                p_count.fetch_add(1, Ordering::Relaxed);
                if let Some(tx) = prompt.reply {
                    let _ = tx.send(Reply::Choose { choice: Choice::ReplaceNewer, apply_all: false });
                }
            }),
            None,
        );
        let older = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let newer = SystemTime::UNIX_EPOCH + Duration::from_secs(200);
        let d1 = resolver.resolve(info("a.mp4", Some(newer)), info("a.mp4", Some(older)), "a (2).mp4".into(), || false);
        let d2 = resolver.resolve(info("b.mp4", Some(older)), info("b.mp4", Some(newer)), "b (2).mp4".into(), || false);
        assert_eq!(prompts.load(Ordering::Relaxed), 1);
        assert_eq!(d1, Decision::Replace);
        assert_eq!(d2, Decision::Skip);
    }

    #[test]
    fn cancel_marks_resolver_cancelled_and_invokes_batch_cancel() {
        let batch_cancelled = Arc::new(AtomicBool::new(false));
        let bc = batch_cancelled.clone();
        let resolver = Resolver::new(
            Op::Upload,
            true,
            Arc::new(move |prompt| {
                prompt.send_cancel();
            }),
            Some(Arc::new(move || bc.store(true, Ordering::Relaxed))),
        );
        let d1 = resolver.resolve(info("a.txt", None), info("a.txt", None), "a (2).txt".into(), || false);
        let d2 = resolver.resolve(info("b.txt", None), info("b.txt", None), "b (2).txt".into(), || false);
        assert_eq!(d1, Decision::Cancel);
        assert_eq!(d2, Decision::Cancel);
        assert!(batch_cancelled.load(Ordering::Relaxed));
    }
}
