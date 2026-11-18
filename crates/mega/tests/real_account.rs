//! Against a real MEGA account; ignored unless asked for:
//!
//! ```text
//! MEGA_TEST_EMAIL=… MEGA_TEST_PASSWORD=… [MEGA_TEST_CODE=123456]
//! cargo test -p openexplorer-mega --test real_account -- --ignored --test-threads 1
//! ```
//!
//! `reads_an_account` only reads (listing, quota, changes, one small file). `works_in_a_folder_of_its_own`
//! also writes, only with `MEGA_TEST_WRITE=1`: everything happens inside a new folder
//! "OpenExplorer-teste-<number>" at Cloud Drive's top, deleted for good at the end. Neither
//! keeps the session anywhere; each ends it on MEGA's side.

use std::sync::Arc;

use openexplorer_mega::api::Api;
use openexplorer_mega::session::{self, Refused};
use openexplorer_mega::{Account, Area, Kind};

fn login_session() -> Option<(String, Api, session::Session)> {
    let (Ok(email), Ok(password)) = (std::env::var("MEGA_TEST_EMAIL"), std::env::var("MEGA_TEST_PASSWORD")) else {
        eprintln!("MEGA_TEST_EMAIL and MEGA_TEST_PASSWORD aren't set: skipped");
        return None;
    };
    openexplorer_mega::set_log(|l| eprintln!("{l}"));
    let code = std::env::var("MEGA_TEST_CODE").ok();
    let api = Api::default();
    let s = match session::login(&api, &email, &password, code.as_deref()) {
        Ok(s) => s,
        Err(Refused::NeedsCode) => panic!("the account asks for a two-factor code: set MEGA_TEST_CODE"),
        Err(e) => panic!("login: {}", e.text()),
    };
    Some((email, api, s))
}

fn login() -> Option<Arc<Account>> {
    let (email, api, s) = login_session()?;
    Some(Arc::new(Account::start(&email, api, s).expect("reading the account")))
}

/// Ends the session even when an assertion fails midway.
struct Logout(Arc<Account>);

impl Drop for Logout {
    fn drop(&mut self) {
        self.0.logout();
    }
}

#[test]
#[ignore = "needs a real MEGA account"]
fn reads_an_account() {
    let Some(a) = login() else { return };
    let _end = Logout(a.clone());
    let top: Vec<String> = a.list("/").unwrap().into_iter().map(|i| i.name).collect();
    assert_eq!(top, Area::ALL.map(|x| x.label().to_string()));
    let cloud = format!("/{}", Area::Cloud.label());
    let items = a.list(&cloud).unwrap();
    eprintln!("{} items at Cloud Drive's top", items.len());
    let q = a.quota().unwrap();
    assert!(q.total > 0 && q.used <= q.total.max(q.used));
    a.list(&format!("/{}", Area::Rubbish.label())).unwrap();
    a.list(&format!("/{}", Area::Incoming.label())).unwrap();
    a.poll().unwrap();
    // The saved session opens again, as the app does at every start, and reads the same tree.
    let api = Api::default();
    let resumed = session::resume(&api, &a.saved()).unwrap_or_else(|f| panic!("resume: {}", f.text()));
    let again = Account::start(&a.email, api, resumed).unwrap();
    // Same master key under the same session key, same session id.
    assert!(again.saved() == a.saved(), "the resumed session holds the same keys");
    assert_eq!(again.list(&cloud).unwrap().len(), items.len());
    // One small file, if the top has one: decrypted and checked against its MAC.
    if let Some(f) = items.iter().find(|i| !i.is_dir && i.size.is_some_and(|s| s < 5 << 20)) {
        let mut out = Vec::new();
        a.download(&format!("{cloud}/{}", f.name), &mut out, &mut |_| true).unwrap();
        assert_eq!(out.len() as u64, f.size.unwrap());
    }
}

/// The tree kept on disk (in a temporary folder, deleted at the end): the first start reads the
/// whole tree and keeps it; a second start of the same session, resumed as the app does at every
/// start, reads the kept tree and only the changes since. With `MEGA_TEST_WRITE=1`, another
/// session makes a folder between the two starts, and the second start must show it.
#[test]
#[ignore = "needs a real MEGA account"]
fn a_second_start_comes_from_the_kept_tree() {
    let Some((email, api, s)) = login_session() else { return };
    let dir = std::env::temp_dir().join(format!("oe-mega-real-cache-{}", std::process::id()));
    let file = openexplorer_mega::cache::file_in(&dir, &email);
    let started = std::time::Instant::now();
    let a = Arc::new(Account::start_kept(&email, api, s, Some(file.clone())).expect("reading the account"));
    let whole = started.elapsed();
    let _end = Logout(a.clone());
    assert!(!a.started_from_cache(), "nothing kept yet: the whole tree is read");
    assert!(file.exists(), "and kept");
    let cloud = format!("/{}", Area::Cloud.label());
    let writing = std::env::var("MEGA_TEST_WRITE").as_deref() == Ok("1");
    let made = writing.then(|| {
        let b = login().expect("a second session");
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let path = format!("{cloud}/OpenExplorer-cache-{stamp}");
        let made = b.mkdir(&path).unwrap();
        // Once the first session's channel brings it, MEGA has it for any session asking.
        // Polling doesn't keep the tree again this soon, so the file keeps the older number.
        for _ in 0..10 {
            if a.poll().map(|_| a.stat(&path).is_some()).unwrap_or(false) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        (b, path, made)
    });
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let api = Api::default();
        let resumed = session::resume(&api, &a.saved()).unwrap_or_else(|f| panic!("resume: {}", f.text()));
        let started = std::time::Instant::now();
        let again = Account::start_kept(&email, api, resumed, Some(file.clone())).expect("reading the account again");
        let cached = started.elapsed();
        eprintln!("start reading the whole tree: {} ms; from the kept tree: {} ms", whole.as_millis(), cached.as_millis());
        assert!(again.started_from_cache(), "the second start reads the kept tree");
        let names = |x: &Account| {
            let mut v: Vec<String> = x.list(&cloud).unwrap().into_iter().map(|i| i.name).collect();
            v.sort();
            v
        };
        if let Some((_, path, _)) = &made {
            assert!(again.stat(path).is_some(), "a folder made after the tree was kept is there");
        }
        assert_eq!(names(&again), names(&a));
        for area in [Area::Rubbish, Area::Incoming] {
            assert_eq!(again.list(&format!("/{}", area.label())).unwrap(), a.list(&format!("/{}", area.label())).unwrap());
        }
        // A sequence number MEGA never gave: the channel must turn it away, and the start read
        // the whole tree instead of showing the kept one.
        let api = Api::default();
        let resumed = session::resume(&api, &a.saved()).unwrap_or_else(|f| panic!("resume: {}", f.text()));
        let secret = openexplorer_mega::cache::Secret::of(&resumed.user, &resumed.master, &resumed.sek);
        let (mut kept, shares) = openexplorer_mega::cache::read(&file, &secret).expect("the kept tree");
        eprintln!("kept sequence number: {} characters", kept.sn.len());
        kept.sn = "A".repeat(kept.sn.len());
        openexplorer_mega::cache::write(&file, &secret, openexplorer_mega::cache::encode(&kept, &shares)).unwrap();
        let third = Account::start_kept(&email, api, resumed, Some(file.clone())).expect("reading the account a third time");
        assert!(!third.started_from_cache(), "a sequence number MEGA doesn't know isn't caught up from");
        assert_eq!(names(&third), names(&a));
    }));
    if let Some((b, _, made)) = made {
        b.delete(&made).unwrap();
        b.logout();
    }
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

/// A second session (the website, say) makes and deletes a folder; the first one's change
/// channel brings both.
#[test]
#[ignore = "needs a real MEGA account and MEGA_TEST_WRITE=1"]
fn sees_changes_made_elsewhere() {
    if std::env::var("MEGA_TEST_WRITE").as_deref() != Ok("1") {
        eprintln!("MEGA_TEST_WRITE isn't 1: skipped");
        return;
    }
    let (Some(a), Some(b)) = (login(), login()) else { return };
    let (_end_a, _end_b) = (Logout(a.clone()), Logout(b.clone()));
    let cloud = format!("/{}", Area::Cloud.label());
    let root = a.folder_handle(&cloud).unwrap();
    a.poll().unwrap();
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let dir = format!("{cloud}/OpenExplorer-outro-{stamp}");
    let made = b.mkdir(&dir).unwrap();
    let seen = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for _ in 0..10 {
            if a.poll().unwrap().touches(&root) {
                return a.stat(&dir).is_some();
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        false
    }));
    // Gone for good, whatever happened above.
    b.delete(&made).unwrap();
    assert!(seen.unwrap_or(false), "the other session's new folder arrives");
    let mut gone = false;
    for _ in 0..10 {
        a.poll().unwrap();
        if a.stat(&dir).is_none() {
            gone = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    assert!(gone, "and its deletion");
}

#[test]
#[ignore = "needs a real MEGA account and MEGA_TEST_WRITE=1"]
fn works_in_a_folder_of_its_own() {
    if std::env::var("MEGA_TEST_WRITE").as_deref() != Ok("1") {
        eprintln!("MEGA_TEST_WRITE isn't 1: skipped");
        return;
    }
    let Some(a) = login() else { return };
    let _end = Logout(a.clone());
    let cloud = format!("/{}", Area::Cloud.label());
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let dir = format!("{cloud}/OpenExplorer-teste-{stamp}");
    let folder = a.mkdir(&dir).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let data: Vec<u8> = (0..1_500_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let up = a.upload(&format!("{dir}/a.bin"), &mut &data[..], data.len() as u64, &mut |_| true).unwrap();
        assert_eq!((up.kind, up.size), (Kind::File, data.len() as u64));
        a.upload(&format!("{dir}/vazio.txt"), &mut &b""[..], 0, &mut |_| true).unwrap();
        let read = |path: &str| {
            let mut out = Vec::new();
            a.download(path, &mut out, &mut |_| true).unwrap();
            out
        };
        assert!(read(&format!("{dir}/a.bin")) == data, "a download gives back what was uploaded");
        assert!(read(&format!("{dir}/vazio.txt")).is_empty());

        a.copy(&format!("{dir}/a.bin"), &format!("{dir}/b.bin")).unwrap();
        assert!(read(&format!("{dir}/b.bin")) == data, "a copy on MEGA's side opens with the same key");
        a.mkdir(&format!("{dir}/sub")).unwrap();
        a.rename(&format!("{dir}/b.bin"), &format!("{dir}/sub/c.bin")).unwrap();
        assert!(a.stat(&format!("{dir}/sub/c.bin")).is_some() && a.stat(&format!("{dir}/b.bin")).is_none());

        let node = a.node(&format!("{dir}/a.bin")).unwrap();
        let url = a.create_link(&node).unwrap();
        assert!(url.starts_with("https://mega.nz/file/"), "{url}");
        a.remove_link(&node).unwrap();
        assert_eq!(a.link_of(&node), None);

        a.remove(&format!("{dir}/a.bin")).unwrap();
        assert!(a.stat(&format!("{dir}/a.bin")).is_none());
        let trashed = a.node(&openexplorer_mega::handle_path(&node.handle)).unwrap();
        let back = a.restore(&trashed).unwrap();
        assert_eq!(back.handle, folder.handle, "restored to the folder it was in");
        assert!(a.stat(&format!("{dir}/a.bin")).is_some());

        // What the channel brings of this test's own changes applies cleanly.
        a.poll().unwrap();
        assert!(a.stat(&format!("{dir}/sub/c.bin")).is_some());
    }));
    // Gone for good, whatever happened above.
    let f = a.node(&openexplorer_mega::handle_path(&folder.handle)).unwrap();
    a.delete(&f).unwrap();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
