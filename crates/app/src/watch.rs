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
