//! Against a real Dropbox account; ignored unless asked for.
//!
//! First a refresh token, from the app registered in the Dropbox App Console (see the crate
//! docs), by signing in once in the browser:
//!
//! ```text
//! DROPBOX_TEST_APP_KEY=… [DROPBOX_TEST_PORT=53719]
//! cargo test -p openexplorer-dropbox --test real_account login -- --ignored --nocapture
//! ```
//!
//! It prints the refresh token. Then:
//!
//! ```text
//! DROPBOX_TEST_APP_KEY=… DROPBOX_TEST_REFRESH_TOKEN=… [DROPBOX_TEST_WRITE=1]
//! cargo test -p openexplorer-dropbox --test real_account -- --ignored --test-threads 1
//! ```
//!
//! `reads_an_account` only reads (the top, the storage, a cursor). `works_in_a_folder_of_its_own`
//! also writes, only with `DROPBOX_TEST_WRITE=1`: everything happens inside a new folder
//! "/OpenExplorer-teste-<number>", deleted at the end (Dropbox keeps it among the deleted files
//! for a while). Neither keeps anything on disk nor in the credential store.

use std::sync::Arc;
use std::time::Duration;

use openexplorer_dropbox::api::{Api, Hosts};
use openexplorer_dropbox::{Account, AppConfig};

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

fn account() -> Option<Arc<Account>> {
    let (Some(key), Some(token)) = (env("DROPBOX_TEST_APP_KEY"), env("DROPBOX_TEST_REFRESH_TOKEN")) else {
        eprintln!("DROPBOX_TEST_APP_KEY and DROPBOX_TEST_REFRESH_TOKEN aren't set: skipped");
        return None;
    };
    openexplorer_dropbox::set_log(|l| eprintln!("{l}"));
    Some(Arc::new(Account::open(Api::new(Hosts::default(), &key, &token), None).expect("opening the account")))
}

#[test]
#[ignore = "signs in through the browser"]
fn login() {
    let Some(key) = env("DROPBOX_TEST_APP_KEY") else { return };
    let port = env("DROPBOX_TEST_PORT").and_then(|p| p.parse().ok()).unwrap_or(openexplorer_dropbox::DEFAULT_PORT);
    let cfg = AppConfig { app_key: key, port };
    let pending = openexplorer_dropbox::oauth::begin(&cfg).unwrap();
    eprintln!("Open in the browser:\n{}\n(redirect URI registered in the app: {})", pending.url, cfg.redirect());
    let tokens = pending.finish(&cfg, &Hosts::default().api).unwrap();
    eprintln!("DROPBOX_TEST_REFRESH_TOKEN={}", tokens.refresh_token.unwrap());
}

#[test]
#[ignore = "needs a real Dropbox account"]
fn reads_an_account() {
    let Some(a) = account() else { return };
    eprintln!("{} ({}), team space: {}", a.email, a.name, a.team);
    let top = a.list("/").unwrap();
    eprintln!("{} items at the top", top.len());
    // Opened again: the kept listing and the log, not the whole folder.
    std::thread::sleep(Duration::from_secs(3));
    let before = a.api.requests.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(a.list("/").unwrap().len(), top.len());
    assert_eq!(a.api.requests.load(std::sync::atomic::Ordering::Relaxed) - before, 1);
    let q = a.quota().unwrap();
    assert!(q.total > 0 && q.used <= q.total.max(q.used));
    if let Some(f) = top.iter().find(|e| !e.dir) {
        assert_eq!(a.stat(&format!("/{}", f.name)).unwrap().unwrap().size, f.size);
    }
}

#[test]
#[ignore = "needs a real Dropbox account and DROPBOX_TEST_WRITE=1"]
fn works_in_a_folder_of_its_own() {
    let Some(a) = account() else { return };
    if env("DROPBOX_TEST_WRITE").as_deref() != Some("1") {
        eprintln!("DROPBOX_TEST_WRITE=1 isn't set: skipped");
        return;
    }
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let dir = format!("/OpenExplorer-teste-{n}");
    a.mkdir(&dir).unwrap();
    struct Clean(Arc<Account>, String);
    impl Drop for Clean {
        fn drop(&mut self) {
            let _ = self.0.delete(&self.1);
        }
    }
    let _clean = Clean(a.clone(), dir.clone());

    // Changes heard while the longpoll waits.
    let w = a.clone();
    let watching = std::thread::spawn(move || w.watch(30).unwrap());
    std::thread::sleep(Duration::from_secs(1));

    // One request, and an upload session in blocks (three of 8 MiB, the last short).
    let small: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    let big: Vec<u8> = (0..(17u32 << 20)).map(|i| (i % 253) as u8).collect();
    a.upload(&format!("{dir}/pequeno ação 😀.bin"), &mut &small[..], small.len() as u64, &mut |_| true).unwrap();
    a.upload_limited(&format!("{dir}/grande.bin"), &mut &big[..], big.len() as u64, 0, &mut |_| true).unwrap();
    let changes = watching.join().unwrap();
    assert!(changes.touches(&dir), "{changes:?}");
    for (name, data) in [("pequeno ação 😀.bin", &small), ("grande.bin", &big)] {
        let mut out = Vec::new();
        a.download(&format!("{dir}/{name}"), &mut out, &mut |_| true).unwrap();
        assert_eq!(&out, data, "{name}");
    }

    a.mkdir(&format!("{dir}/Sub")).unwrap();
    a.copy(&format!("{dir}/grande.bin"), &format!("{dir}/Sub/cópia.bin")).unwrap();
    a.rename(&format!("{dir}/Sub/cópia.bin"), &format!("{dir}/Sub/Cópia.bin")).unwrap();
    let names: Vec<String> = a.list(&format!("{dir}/Sub")).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["Cópia.bin"]);

    let link = a.create_link(&format!("{dir}/grande.bin")).unwrap();
    assert_eq!(a.link_of(&format!("{dir}/grande.bin")).unwrap(), Some(link));
    assert_eq!(a.remove_link(&format!("{dir}/grande.bin")).unwrap(), 1);

    // Deleted, then back.
    let gone = a.delete(&format!("{dir}/grande.bin")).unwrap();
    assert!(a.stat(&format!("{dir}/grande.bin")).unwrap().is_none());
    a.restore(&format!("{dir}/grande.bin"), &gone.rev).unwrap();
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 - 600;
    let results = a.delete_many(&[format!("{dir}/Sub")]).unwrap();
    assert!(results[0].is_ok());
    assert_eq!(a.restore_folder(&format!("{dir}/Sub"), since).unwrap(), 1);
    assert!(a.stat(&format!("{dir}/Sub/Cópia.bin")).unwrap().is_some());
}
