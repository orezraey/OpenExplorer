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
    // A folder deleted and brought back: its file and its empty subfolder, within the window
    // of Dropbox's own clock around the delete.
    a.mkdir(&format!("{dir}/Sub/Vazia")).unwrap();
    let from = a.api.server_now() - 2;
    let results = a.delete_many(&[format!("{dir}/Sub")]).unwrap();
    let to = a.api.server_now() + 1;
    assert!(results[0].is_ok());
    assert_eq!(a.restore_folder(&format!("{dir}/Sub"), Some((from, to))).unwrap(), 1);
    assert!(a.stat(&format!("{dir}/Sub/Cópia.bin")).unwrap().is_some());
    assert!(a.stat(&format!("{dir}/Sub/Vazia")).unwrap().is_some_and(|e| e.dir));

    // Batches: small files together, then moved and copied in one job each.
    let local = std::env::temp_dir().join(format!("oe-dropbox-real-{n}"));
    std::fs::create_dir_all(&local).unwrap();
    let files: Vec<(String, std::path::PathBuf)> = (0..20)
        .map(|i| {
            let f = local.join(format!("f{i}.txt"));
            std::fs::write(&f, format!("arquivo {i}").repeat(i + 1)).unwrap();
            (format!("{dir}/Lote/f{i}.txt"), f)
        })
        .collect();
    let before = a.api.requests.load(std::sync::atomic::Ordering::Relaxed);
    let sent = a.upload_many(&files, &mut |_, _| true);
    eprintln!("upload_many of 20: {} requests", a.api.requests.load(std::sync::atomic::Ordering::Relaxed) - before);
    assert!(sent.iter().all(Result::is_ok), "{sent:?}");
    // Sent again: the same bytes are there, nothing goes.
    let again = a.upload_many(&files, &mut |_, _| true);
    assert!(again.iter().all(Result::is_ok), "{again:?}");
    let moves: Vec<(String, String)> = (0..20).map(|i| (format!("{dir}/Lote/f{i}.txt"), format!("{dir}/Movidos/f{i}.txt"))).chain([(format!("{dir}/Lote/nada.txt"), format!("{dir}/Movidos/nada.txt"))]).collect();
    let moved = a.move_many(&moves);
    assert!(moved[..20].iter().all(Result::is_ok), "{moved:?}");
    assert!(moved[20].is_err(), "a missing item fails alone");
    let copies: Vec<(String, String)> = (0..5).map(|i| (format!("{dir}/Movidos/f{i}.txt"), format!("{dir}/Movidos/f{}.txt", i + 1))).collect();
    let copied = a.copy_many(&copies);
    assert!(copied.iter().all(Result::is_err), "names taken fail, one by one: {copied:?}");
    let mut out = Vec::new();
    a.download(&format!("{dir}/Movidos/f7.txt"), &mut out, &mut |_| true).unwrap();
    assert_eq!(out, std::fs::read(&files[7].1).unwrap());

    // The deleted items of a folder, and whether one deletion stamps its files alike.
    a.delete(&format!("{dir}/Movidos")).unwrap();
    let gone = a.deleted_in(&dir).unwrap();
    eprintln!("deleted in {dir}: {gone:?}");
    let folder = gone.iter().find(|d| d.name == "Movidos").expect("the folder among the deleted");
    assert_eq!(folder.dir, Some(true));
    assert_eq!(a.restore_deleted(folder).unwrap(), 20);
    a.delete(&format!("{dir}/Movidos")).unwrap();
    a.mkdir(&format!("{dir}/Movidos")).unwrap();
    let inside = a.deleted_in(&format!("{dir}/Movidos")).unwrap();
    let whens: std::collections::BTreeSet<Option<i64>> = inside.iter().map(|d| d.when).collect();
    eprintln!("server_deleted of one folder's 20 files: {whens:?}");
    let _ = std::fs::remove_dir_all(local);
}
