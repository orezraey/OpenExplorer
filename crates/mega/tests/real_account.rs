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

fn login() -> Option<Arc<Account>> {
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
    // One small file, if the top has one: decrypted and checked against its MAC.
    if let Some(f) = items.iter().find(|i| !i.is_dir && i.size.is_some_and(|s| s < 5 << 20)) {
        let mut out = Vec::new();
        a.download(&format!("{cloud}/{}", f.name), &mut out, &mut |_| true).unwrap();
        assert_eq!(out.len() as u64, f.size.unwrap());
    }
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
