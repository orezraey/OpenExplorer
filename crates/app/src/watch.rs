//! Keeps what the panes and trees show of Google Drive current with what changes elsewhere: the
//! website, a phone, someone sharing a folder. Each account a pane shows is asked what changed
//! (Drive's change log) every [`EVERY`] seconds, a request of a few hundred bytes, and only the
//! folders whose listing would now differ are read again.

use std::collections::{HashMap, HashSet};

use openexplorer_gdrive::Change;
use openexplorer_i18n::t;

use crate::model::{Entry, Location, Pane, Space};
use crate::{Msg, State, drive_task, entry_of};

/// How often an account a pane shows is asked what changed, in seconds.
pub const EVERY: f64 = 20.0;
/// After a failed ask (no connection, say), how long until the next one.
const RETRY: f64 = 60.0;

/// Where an account's change log was read up to.
#[derive(Default)]
pub struct Watch {
    /// `None` until the first answer, and again once Google turned the last one down.
    token: Option<String>,
    /// Meu Drive's real top folder id, which changes name instead of `"root"`.
    root: Option<String>,
    busy: bool,
    /// When to ask next, on the app clock.
    next: f64,
    /// The token was turned down: what changed since it is unknown, so the next start reads
    /// the panes showing the account again.
    resync: bool,
    /// Per pane, the place it was still listing when changes came: its listing may have
    /// fetched the changed folder before they happened, so it's read again once done there.
    redo: [Option<Location>; 2],
}

/// What an ask brought.
pub enum Fetched {
    /// The first ask: where the log is now, and Meu Drive's id. Nothing to apply.
    Start { token: String, root: String },
    Changes { changes: Vec<Change>, token: String },
}

/// Asks each account a pane shows what changed, once its time has come, and reads again the
/// panes that were still listing when changes came (see [`Watch::redo`]).
pub fn tick(s: &mut State, now: f64) -> bool {
    let accounts = &s.accounts;
    s.watch.retain(|email, _| accounts.iter().any(|a| &a.email == email));
    let mut reloaded = false;
    for p in 0..2 {
        let pane = &s.panes[p];
        let mut again = false;
        for w in s.watch.values_mut() {
            if w.redo[p].as_ref().is_some_and(|l| *l != pane.loc) {
                w.redo[p] = None;
            } else if w.redo[p].is_some() && !pane.loading {
                w.redo[p] = None;
                again = true;
            }
        }
        if again {
            crate::load(s, p);
            reloaded = true;
        }
    }
    let shown: HashSet<String> = s.accounts.iter().map(|a| a.email.clone()).filter(|e| s.panes.iter().any(|p| shows(p, e))).collect();
    for email in shown {
        let w = s.watch.entry(email.clone()).or_default();
        if w.busy || now < w.next {
            continue;
        }
        w.busy = true;
        let token = w.token.clone();
        let to = email.clone();
        drive_task(
            s,
            &email,
            t("ler as mudanças do Drive", "read the changes in Drive"),
            move |d| match token {
                None => Ok(Fetched::Start { token: d.start_page_token()?, root: d.root_id()? }),
                Some(t) => d.changes(&t).map(|(changes, token)| Fetched::Changes { changes, token }),
            },
            move |result| Msg::Changes { email: to, result },
        );
    }
    reloaded
}

/// An ask for `email` came back: the panes and the tree take in what changed, and the next ask
/// is set.
pub fn arrived(s: &mut State, email: String, result: Result<Fetched, String>, now: f64) {
    // Disconnected meanwhile.
    let Some(w) = s.watch.get_mut(&email) else { return };
    w.busy = false;
    let mut resync = false;
    let changes = match result {
        Err(e) => {
            openexplorer_gdrive::log(&format!("mudanças de {email}: {e}"));
            // No connection keeps the token: the changes wait in Drive's log for the next ask.
            // A token Google turns down is gone, and so is what changed since it.
            if ["HTTP 400", "HTTP 404", "HTTP 410"].iter().any(|code| e.contains(code)) {
                w.token = None;
                w.resync = true;
            }
            w.next = now + RETRY;
            wake_in(s, RETRY);
            return;
        }
        Ok(Fetched::Start { token, root }) => {
            (w.token, w.root) = (Some(token), Some(root));
            resync = std::mem::take(&mut w.resync);
            Vec::new()
        }
        Ok(Fetched::Changes { changes, token }) => {
            w.token = Some(token);
            changes
        }
    };
    w.next = now + EVERY;
    let root = w.root.clone();
    wake_in(s, EVERY);
    if resync {
        for p in 0..2 {
            if shows(&s.panes[p], &email) {
                crate::load(s, p);
            }
        }
    }
    if changes.is_empty() {
        return;
    }
    openexplorer_gdrive::log(&format!("mudanças de {email}: {} itens", changes.len()));
    for p in 0..2 {
        if !shows(&s.panes[p], &email) {
            continue;
        }
        if s.panes[p].loading {
            let loc = s.panes[p].loc.clone();
            if let Some(w) = s.watch.get_mut(&email) {
                w.redo[p] = Some(loc);
            }
        } else if stale(&s.panes[p], &changes, root.as_deref()) {
            crate::load(s, p);
        }
    }
    crate::tree::drive_changed(s, &email, &changes, root.as_deref());
}

/// Where `email`'s change log was last read up to, if it was: no later than now, so a listing
/// started after this is valid at it (see [`crate::cache::Pos`]).
pub fn token(s: &State, email: &str) -> Option<String> {
    s.watch.get(email).and_then(|w| w.token.clone())
}

/// How long a listing proven unchanged by the change log alone is trusted before it's read
/// whole again anyway, in seconds: a day.
pub const MAX_AGE: u64 = 24 * 60 * 60;

/// A folder whose cached listing the change log can vouch for: one in My Drive, reached through
/// real folders. Changes to shared drives' files and to files only shared with the account
/// aren't all sure to be in the account's log, so nothing reached through a view, a shortcut or
/// a shared drive is.
#[derive(Clone, Debug)]
pub struct Provable {
    /// The folder's id; `None` for My Drive's top, whose real id Drive is asked for.
    pub folder: Option<String>,
    /// The folders above it, from the top: a change to any of them (trashed, moved, deleted,
    /// unshared) may change what it lists without a change to each item in it.
    pub above: Vec<String>,
}

/// Whether `loc` is a [`Provable`] folder: in My Drive, and each folder on its path a real
/// folder (not a shortcut) in the listing of the one above, as kept in `cache`.
pub fn provable(cache: &crate::cache::Cache, loc: &Location) -> Option<Provable> {
    let Location::Drive { email, space: Space::MyDrive, path } = loc else { return None };
    for i in 0..path.len() {
        let parent = Location::Drive { email: email.clone(), space: Space::MyDrive, path: path[..i].to_vec() };
        let listing = cache.get(&crate::cache::key(&parent)?)?;
        let id = path[i].0.as_str();
        if !listing.entries.iter().any(|e| e.id == id && e.is_dir && e.target.is_none()) {
            return None;
        }
    }
    let mut ids: Vec<String> = path.iter().map(|(id, _)| id.clone()).collect();
    let folder = ids.pop();
    Some(Provable { folder, above: ids })
}

/// What opening a cached folder comes to.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// The change log shows nothing that would change the listing: it stands, valid at the
    /// new position.
    Unchanged(crate::cache::Pos),
    /// The folder must be listed again; the token, when the log was read, is where the new
    /// listing will be valid at.
    Fetch(Option<String>),
}

/// Decides whether `cached`, the listing of `folder`, still stands, reading the change log from
/// its position with `changes(token, pages)` (as [`openexplorer_gdrive::Drive::changes_within`])
/// and My Drive's top id with `root`. Anything in doubt is a [`Verdict::Fetch`]: no listing or
/// position, one older than [`MAX_AGE`], a token Google turns down, more changes than reading
/// the folder whole would cost, or any change to the folder, the folders above it, or what it
/// holds. `now` is in seconds since 1970.
pub fn verdict(
    cached: Option<&crate::cache::Listing>,
    folder: &Provable,
    now: u64,
    changes: impl FnOnce(&str, usize) -> Result<Option<(Vec<Change>, String)>, String>,
    root: impl FnOnce() -> Result<String, String>,
) -> Verdict {
    let Some((listing, pos)) = cached.and_then(|l| Some((l, l.pos.as_ref()?))) else { return Verdict::Fetch(None) };
    if now < pos.fetched || now - pos.fetched > MAX_AGE {
        return Verdict::Fetch(None);
    }
    // Never more pages of changes than of the listing they'd spare (1,000 items each).
    let pages = listing.entries.len().div_ceil(1000).max(1);
    let (changes, next) = match changes(&pos.token, pages) {
        Ok(Some(read)) => read,
        Ok(None) => return Verdict::Fetch(None),
        Err(e) => {
            openexplorer_gdrive::log(&format!("mudanças desde a listagem guardada: {e}"));
            return Verdict::Fetch(None);
        }
    };
    let folder_id = match &folder.folder {
        Some(f) => f.clone(),
        None => match root() {
            Ok(r) => r,
            Err(_) => return Verdict::Fetch(Some(next)),
        },
    };
    let touched = changes.iter().any(|c| c.file_id == folder_id || folder.above.contains(&c.file_id));
    let shown: HashMap<&str, &Entry> = listing.entries.iter().map(|e| (e.id.as_str(), e)).collect();
    if touched || folder_stale(&shown, &folder_id, &changes) {
        return Verdict::Fetch(Some(next));
    }
    Verdict::Unchanged(crate::cache::Pos { token: next, fetched: pos.fetched })
}

/// Wakes the app in `secs`, for the next ask: nothing else may draw a frame meanwhile.
fn wake_in(s: &State, secs: f64) {
    if let Some(wake) = s.waker.clone() {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs_f64(secs + 0.1));
            wake();
        });
    }
}

/// Whether pane `pane` shows a place in `email`'s Drive.
fn shows(pane: &Pane, email: &str) -> bool {
    matches!(&pane.loc, Location::Drive { email: e, .. } if e == email)
}

/// Whether `changes` make what `pane` shows (a place in their account's Drive) differ from a
/// fresh listing.
fn stale(pane: &Pane, changes: &[Change], root: Option<&str>) -> bool {
    let Location::Drive { space, path, .. } = &pane.loc else { return false };
    // Looked up by id once: a folder can hold tens of thousands, and a backlog of changes as many.
    let shown: HashMap<&str, &Entry> = pane.entries.iter().map(|e| (e.id.as_str(), e)).collect();
    let folder = path.last().map(|(id, _)| id.as_str());
    match (space, folder) {
        (Space::SharedDrives, _) => changes.iter().any(|c| c.drive),
        // Search results stay as found; a trashed folder's contents change only by the actions here.
        (Space::Search(_), None) | (Space::Trash, Some(_)) => false,
        (Space::Starred, None) => view_stale(&shown, changes, |c| c.file.as_ref().is_some_and(|f| f.starred) && !c.trashed),
        (Space::Trash, None) => view_stale(&shown, changes, |c| c.trashed),
        // What Recentes holds follows from what was opened or changed: any change can move it.
        (Space::Recent, None) => changes.iter().any(|c| !c.drive),
        // A file newly shared with the account has no folder of its own there.
        (Space::SharedWithMe, None) => changes.iter().any(|c| shown.contains_key(c.file_id.as_str()) || (c.file.is_some() && c.parents.is_empty() && !c.trashed)),
        (Space::MyDrive, None) => root.is_some_and(|r| folder_stale(&shown, r, changes)),
        (Space::Shared { id, .. }, None) => folder_stale(&shown, id, changes),
        (_, Some(f)) => folder_stale(&shown, f, changes),
    }
}

/// Whether the listing of folder `folder`, `shown` by id, differs from what `changes` say it
/// holds: an item gone from it, one new in it, or one changed (renamed, starred, a new version).
fn folder_stale(shown: &HashMap<&str, &Entry>, folder: &str, changes: &[Change]) -> bool {
    view_stale(shown, changes, |c| !c.trashed && c.parents.iter().any(|p| p == folder))
}

/// Whether a list of files, `shown` by id, differs from what `changes` say, `holds` telling
/// whether a changed file belongs in it now.
fn view_stale(shown: &HashMap<&str, &Entry>, changes: &[Change], holds: impl Fn(&Change) -> bool) -> bool {
    changes.iter().filter(|c| !c.drive).any(|c| {
        let now = c.file.as_ref().filter(|_| holds(c)).map(|f| entry_of(f.clone()));
        match (shown.get(c.file_id.as_str()), now) {
            (None, None) => false,
            (Some(shown), Some(now)) => **shown != now,
            _ => true,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openexplorer_gdrive::DriveFile;

    fn file(id: &str, name: &str) -> DriveFile {
        DriveFile { id: id.into(), name: name.into(), is_dir: false, size: Some(3), modified: None, mime: "text/plain".into(), target: None, starred: false, md5: None }
    }

    fn change(f: DriveFile, parents: &[&str]) -> Change {
        Change { file_id: f.id.clone(), file: Some(f), parents: parents.iter().map(|p| p.to_string()).collect(), trashed: false, drive: false }
    }

    fn pane(space: Space, path: &[&str], files: &[DriveFile]) -> Pane {
        let path = path.iter().map(|id| (id.to_string(), id.to_string())).collect();
        let mut p = Pane::new(Location::Drive { email: "a@b".into(), space, path });
        p.set_entries(Ok(files.iter().cloned().map(entry_of).collect()));
        p.loading = false;
        p
    }

    #[test]
    fn a_folder_is_read_again_only_when_its_listing_would_differ() {
        let shown = pane(Space::MyDrive, &["pasta"], &[file("a", "a.txt")]);
        let is = |changes: &[Change]| stale(&shown, changes, Some("raiz"));
        // The same file as shown (the app's own upload, already listed): nothing to do.
        assert!(!is(&[change(file("a", "a.txt"), &["pasta"])]));
        // Renamed, moved out, trashed, deleted for good, or new here.
        assert!(is(&[change(file("a", "b.txt"), &["pasta"])]));
        assert!(is(&[change(file("a", "a.txt"), &["outra"])]));
        assert!(is(&[Change { trashed: true, ..change(file("a", "a.txt"), &["pasta"]) }]));
        assert!(is(&[Change { file_id: "a".into(), file: None, parents: Vec::new(), trashed: false, drive: false }]));
        assert!(is(&[change(file("n", "novo.txt"), &["pasta"])]));
        // Elsewhere: not this folder's business; another account's pane isn't asked.
        assert!(!is(&[change(file("x", "x.txt"), &["outra"])]));
        assert!(shows(&shown, "a@b") && !shows(&shown, "c@d"));
    }

    fn listing(files: &[DriveFile], token: Option<&str>, fetched: u64) -> crate::cache::Listing {
        crate::cache::Listing {
            entries: std::sync::Arc::new(files.iter().cloned().map(entry_of).collect()),
            pos: token.map(|t| crate::cache::Pos { token: t.into(), fetched }),
        }
    }

    const NOW: u64 = 1_800_000_000;

    /// The verdict on `cached` for folder `pasta` (under `avo`, `pai`), the change log answering
    /// `log`; also whether the log was read at all.
    fn judge(cached: Option<&crate::cache::Listing>, log: Result<Option<Vec<Change>>, String>) -> (Verdict, bool) {
        let folder = Provable { folder: Some("pasta".into()), above: vec!["avo".into(), "pai".into()] };
        let mut asked = false;
        let v = verdict(
            cached,
            &folder,
            NOW,
            |token, pages| {
                asked = true;
                assert_eq!((token, pages), ("t1", 1));
                log.map(|l| l.map(|c| (c, "t2".to_string())))
            },
            || panic!("only My Drive's top needs its id"),
        );
        (v, asked)
    }

    #[test]
    fn an_unchanged_folder_keeps_its_listing_and_moves_its_position() {
        let cached = listing(&[file("a", "a.txt")], Some("t1"), NOW - 60);
        let moved = Verdict::Unchanged(crate::cache::Pos { token: "t2".into(), fetched: NOW - 60 });
        // Nothing changed; changes elsewhere; the same file changed back to as listed.
        assert_eq!(judge(Some(&cached), Ok(Some(vec![]))), (moved.clone(), true));
        assert_eq!(judge(Some(&cached), Ok(Some(vec![change(file("x", "x.txt"), &["outra"])]))).0, moved.clone());
        assert_eq!(judge(Some(&cached), Ok(Some(vec![change(file("a", "a.txt"), &["pasta"])]))).0, moved);
    }

    #[test]
    fn a_changed_folder_is_read_again_from_where_the_log_was_read() {
        let cached = listing(&[file("a", "a.txt")], Some("t1"), NOW - 60);
        let again = |log: Vec<Change>| judge(Some(&cached), Ok(Some(log))).0;
        let fetch = Verdict::Fetch(Some("t2".into()));
        assert_eq!(again(vec![change(file("n", "novo.txt"), &["pasta"])]), fetch);
        assert_eq!(again(vec![change(file("a", "b.txt"), &["pasta"])]), fetch);
        assert_eq!(again(vec![Change { file_id: "a".into(), file: None, parents: Vec::new(), trashed: false, drive: false }]), fetch);
        // The folder itself or one above it changed (trashed, moved, unshared): its items may
        // not each show a change.
        assert_eq!(again(vec![Change { trashed: true, ..change(file("pasta", "Pasta"), &["pai"]) }]), fetch);
        assert_eq!(again(vec![Change { file_id: "avo".into(), file: None, parents: Vec::new(), trashed: false, drive: false }]), fetch);
    }

    #[test]
    fn without_a_trusted_position_the_folder_is_read_again() {
        let fresh = listing(&[file("a", "a.txt")], Some("t1"), NOW - 60);
        // No listing, or one without a position (from before positions, or unverified): the
        // log isn't even read.
        assert_eq!(judge(None, Ok(Some(vec![]))), (Verdict::Fetch(None), false));
        assert_eq!(judge(Some(&listing(&[], None, NOW)), Ok(Some(vec![]))), (Verdict::Fetch(None), false));
        // Too old, or from a clock that has gone back.
        assert_eq!(judge(Some(&listing(&[], Some("t1"), NOW - MAX_AGE - 1)), Ok(Some(vec![]))), (Verdict::Fetch(None), false));
        assert_eq!(judge(Some(&listing(&[], Some("t1"), NOW + 60)), Ok(Some(vec![]))), (Verdict::Fetch(None), false));
        // A token Google turns down, no connection, or more changes than the listing's pages.
        assert_eq!(judge(Some(&fresh), Err("Mudanças no Drive: HTTP 410: token expirado".into())), (Verdict::Fetch(None), true));
        assert_eq!(judge(Some(&fresh), Err("sem conexão".into())), (Verdict::Fetch(None), true));
        assert_eq!(judge(Some(&fresh), Ok(None)), (Verdict::Fetch(None), true));
    }

    #[test]
    fn my_drive_s_top_is_judged_by_its_real_id_and_a_big_folder_reads_more_pages() {
        let top = Provable { folder: None, above: Vec::new() };
        let files: Vec<DriveFile> = (0..2500).map(|i| file(&format!("f{i}"), "x")).collect();
        let cached = listing(&files, Some("t1"), NOW);
        let log = |root: Result<String, String>, changes: Vec<Change>| {
            verdict(
                Some(&cached),
                &top,
                NOW,
                |_, pages| {
                    assert_eq!(pages, 3);
                    Ok(Some((changes, "t2".into())))
                },
                || root,
            )
        };
        assert!(matches!(log(Ok("raiz".into()), vec![change(file("x", "x"), &["outra"])]), Verdict::Unchanged(_)));
        assert_eq!(log(Ok("raiz".into()), vec![change(file("n", "n"), &["raiz"])]), Verdict::Fetch(Some("t2".into())));
        assert_eq!(log(Err("sem conexão".into()), vec![]), Verdict::Fetch(Some("t2".into())));
    }

    #[test]
    fn only_my_drive_folders_reached_through_real_folders_are_provable() {
        let mut cache = crate::cache::Cache::memory_only();
        let at = |space: Space, path: &[&str]| Location::Drive { email: "a@b".into(), space, path: path.iter().map(|id| (id.to_string(), id.to_string())).collect() };
        let folder = |id: &str| Entry { is_dir: true, mime: None, ..entry_of(file(id, id)) };
        let put = |cache: &mut crate::cache::Cache, path: &[&str], entries: Vec<Entry>| cache.put(crate::cache::key(&at(Space::MyDrive, path)).unwrap(), std::sync::Arc::new(entries), None);
        // My Drive's top always is; a folder in it once the top's listing shows it a folder.
        assert!(provable(&cache, &at(Space::MyDrive, &[])).is_some_and(|p| p.folder.is_none()));
        assert!(provable(&cache, &at(Space::MyDrive, &["fotos"])).is_none());
        let shortcut = Entry { target: Some("alvo".into()), ..folder("atalho") };
        put(&mut cache, &[], vec![folder("fotos"), shortcut, entry_of(file("doc", "doc"))]);
        let p = provable(&cache, &at(Space::MyDrive, &["fotos"])).unwrap();
        assert_eq!((p.folder.as_deref(), p.above.len()), (Some("fotos"), 0));
        // Deeper: each folder on the way must be known.
        assert!(provable(&cache, &at(Space::MyDrive, &["fotos", "2024"])).is_none());
        put(&mut cache, &["fotos"], vec![folder("2024")]);
        let p = provable(&cache, &at(Space::MyDrive, &["fotos", "2024"])).unwrap();
        assert_eq!((p.folder.as_deref(), p.above), (Some("2024"), vec!["fotos".to_string()]));
        // A shortcut's target (maybe in a shared drive, maybe only shared with the account), a
        // file, a shared drive, a view, or a folder opened from one: the log may not cover it.
        assert!(provable(&cache, &at(Space::MyDrive, &["alvo"])).is_none());
        assert!(provable(&cache, &at(Space::MyDrive, &["doc"])).is_none());
        assert!(provable(&cache, &at(Space::Shared { id: "0A".into(), name: "E".into() }, &[])).is_none());
        assert!(provable(&cache, &at(Space::Starred, &["fotos"])).is_none());
        assert!(provable(&cache, &at(Space::SharedWithMe, &[])).is_none());
        assert!(provable(&cache, &Location::Mock(Vec::new())).is_none());
    }

    #[test]
    fn meu_drive_s_top_and_the_views_follow_their_own_rules() {
        let top = pane(Space::MyDrive, &[], &[]);
        assert!(stale(&top, &[change(file("n", "n.txt"), &["raiz"])], Some("raiz")));
        assert!(!stale(&top, &[change(file("n", "n.txt"), &["raiz"])], None));
        let starred = pane(Space::Starred, &[], &[]);
        let star = DriveFile { starred: true, ..file("s", "s.txt") };
        assert!(stale(&starred, &[change(star, &["x"])], None));
        assert!(!stale(&starred, &[change(file("s", "s.txt"), &["x"])], None));
        let drives = pane(Space::SharedDrives, &[], &[]);
        assert!(stale(&drives, &[Change { file_id: "0A".into(), file: None, parents: Vec::new(), trashed: false, drive: true }], None));
        assert!(!stale(&drives, &[change(file("n", "n.txt"), &["raiz"])], None));
    }
}
