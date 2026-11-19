//! The account's calls against the stand-in for B2 ([`crate::mock`]): listings and their cache,
//! restricted keys, renewed tokens and retries, uploads small and large (resumed, cancelled),
//! downloads picked up after a cut, hiding and bringing back, every version deleted, renames
//! and copies on B2's side.

use std::io::Read;
use std::sync::atomic::Ordering;

use crate::account::{Account, EMPTY_MARKER};
use crate::cache;
use crate::mock::Mock;
use crate::upload::Uploads;

fn names(a: &Account, path: &str) -> Vec<String> {
    let mut v: Vec<String> = a.list(path).unwrap().into_iter().map(|i| if i.is_dir { format!("{}/", i.name) } else { i.name }).collect();
    v.sort();
    v
}

fn put(a: &Account, up: &mut Uploads, path: &str, data: &[u8]) {
    a.upload(up, path, &mut &data[..], data.len() as u64, &mut |_| true).unwrap();
}

fn get(a: &Account, path: &str) -> Vec<u8> {
    let mut out = Vec::new();
    a.download(path, &mut out, &mut |_| true).unwrap();
    out
}

/// Bytes that don't repeat, so a part out of place shows.
fn noise(len: usize) -> Vec<u8> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn mock() -> Mock {
    let m = Mock::start();
    {
        let mut st = m.lock();
        st.add("bkt_fotos", "2024/praia.jpg", b"praia");
        st.add("bkt_fotos", "2024/ferias/a.jpg", b"aaa");
        st.add("bkt_fotos", "leia-me.txt", b"oi");
        st.add("bkt_fotos", "vazia/.bzEmpty", b"");
    }
    m
}

#[test]
fn buckets_and_folders_list_by_delimiter_and_markers_stay_hidden() {
    let m = mock();
    let a = m.account();
    assert_eq!(names(&a, "/"), ["docs/", "fotos/"]);
    assert_eq!(names(&a, "/fotos"), ["2024/", "leia-me.txt", "vazia/"]);
    assert_eq!(names(&a, "/fotos/2024"), ["ferias/", "praia.jpg"]);
    assert!(names(&a, "/fotos/vazia").is_empty());
    let it = a.stat("/fotos/2024/praia.jpg").unwrap().unwrap();
    assert_eq!((it.is_dir, it.size), (false, Some(5)));
    assert!(a.stat("/fotos/2024").unwrap().unwrap().is_dir);
    assert!(a.stat("/fotos/nada").unwrap().is_none());
    assert!(a.stat("/nenhum").unwrap().is_none());
    // The lifecycle rules came with the buckets.
    assert_eq!(a.hidden_kept("/docs/x"), Some(Some(1)));
    assert_eq!(a.hidden_kept("/fotos/x"), Some(None));
}

#[test]
fn a_kept_listing_shows_while_b2_is_asked_again_and_changes_elsewhere_come_through() {
    let m = mock();
    let a = m.account();
    names(&a, "/fotos/2024");
    m.reset_counts();
    // Kept: there at once, nothing asked.
    assert_eq!(a.cached("/fotos/2024").unwrap().items.len(), 2);
    assert_eq!(m.lock().requests, 0);
    // Read again: the same, one request.
    assert_eq!(a.reread("/fotos/2024").unwrap(), crate::Fresh::Same);
    assert_eq!(m.calls("b2_list_file_names"), 1);
    // A file added and one hidden elsewhere: the next reading says so, and keeps it.
    {
        let mut st = m.lock();
        st.add("bkt_fotos", "2024/de-fora.jpg", b"x");
        st.hide("bkt_fotos", "2024/praia.jpg");
    }
    let crate::Fresh::Changed(items) = a.reread("/fotos/2024").unwrap() else { panic!("the change isn't seen") };
    let mut got: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
    got.sort();
    assert_eq!(got, ["de-fora.jpg", "ferias"]);
    assert_eq!(a.cached("/fotos/2024").unwrap().items, items);
    // Before writing, B2 is asked, never the kept listing: a name taken elsewhere counts.
    m.lock().add("bkt_fotos", "2024/tomado.jpg", b"t");
    m.reset_counts();
    assert!(a.stat("/fotos/2024/tomado.jpg").unwrap().is_some());
    assert!(a.stat("/fotos/2024/outra.jpg").unwrap().is_none());
    assert!(a.stat("/fotos/2024/ferias").unwrap().unwrap().is_dir);
    assert_eq!(m.lock().requests, 3, "one request each");
    assert!(a.mkdir(&mut Uploads::default(), "/fotos/2024/tomado.jpg").is_err());
}

#[test]
fn a_name_between_a_file_and_its_folder_is_still_found_in_one_or_two_requests() {
    let m = mock();
    {
        let mut st = m.lock();
        // Names sorting between "x" and "x/": "x (2)", "x.txt"...; then the folder "x/".
        for i in 0..150 {
            st.add("bkt_docs", &format!("x.{i:03}"), b"");
        }
        st.add("bkt_docs", "x/dentro.txt", b"d");
    }
    let a = m.account();
    a.buckets_fresh().unwrap();
    m.reset_counts();
    assert!(a.stat("/docs/x").unwrap().unwrap().is_dir);
    assert_eq!(m.lock().requests, 2, "the page of names between, then the folder alone");
    m.reset_counts();
    assert!(a.stat("/docs/x.007").unwrap().is_some_and(|i| !i.is_dir));
    assert!(a.stat("/docs/y").unwrap().is_none());
    assert_eq!(m.lock().requests, 2);
}

#[test]
fn the_apps_own_readings_say_what_changed_and_keep_within_their_budget() {
    let m = mock();
    let a = m.account();
    names(&a, "/fotos/2024");
    assert_eq!(a.poll("/fotos/2024"), crate::Polled::Same);
    m.lock().add("bkt_fotos", "2024/nova.jpg", b"n");
    assert!(matches!(a.poll("/fotos/2024"), crate::Polled::Changed(items) if items.len() == 3));
    // The top isn't read on its own.
    m.reset_counts();
    assert_eq!(a.poll("/"), crate::Polled::Same);
    assert_eq!(m.lock().requests, 0);
    assert_eq!(a.polls_spent(), (2, 2));
    // A 429 isn't sent again: the reading waits its Retry-After, and so do the next ones.
    m.faults.too_many.store(1, Ordering::SeqCst);
    m.lock().retry_after = 30;
    let crate::Polled::Wait(w) = a.poll("/fotos/2024") else { panic!("no wait") };
    assert!((29..=30).contains(&w), "{w}");
    assert_eq!(m.calls("b2_list_file_names"), 1, "tried once");
    assert!(matches!(a.poll("/fotos/2024"), crate::Polled::Wait(_)));
    assert_eq!(m.calls("b2_list_file_names"), 1, "held while paused");
}

#[test]
fn a_cap_stops_the_apps_own_readings_until_the_user_reads() {
    let m = mock();
    let a = m.account();
    names(&a, "/fotos/2024");
    m.faults.capped.store(1, Ordering::SeqCst);
    assert!(matches!(a.poll("/fotos/2024"), crate::Polled::Stopped(why) if why.contains("Caps & Alerts")));
    m.reset_counts();
    assert!(matches!(a.poll("/fotos/2024"), crate::Polled::Stopped(_)));
    assert_eq!(m.lock().requests, 0);
    // Atualizar goes through: the readings start again.
    a.list_fresh("/fotos/2024").unwrap();
    assert_eq!(a.poll("/fotos/2024"), crate::Polled::Same);
}

#[test]
fn a_listing_read_across_an_own_change_isnt_kept() {
    let m = mock();
    let a = std::sync::Arc::new(m.account());
    names(&a, "/fotos");
    m.faults.latency_ms.store(300, Ordering::SeqCst);
    let reader = {
        let a = a.clone();
        std::thread::spawn(move || a.list_fresh("/fotos/2024").unwrap())
    };
    std::thread::sleep(std::time::Duration::from_millis(100));
    // While it's read, the app makes a change there.
    a.changed(&crate::account::Spot::of("/fotos/2024/x").unwrap(), false);
    reader.join().unwrap();
    assert!(a.cached("/fotos/2024").is_none(), "what was read may predate the change");
}

#[test]
fn own_changes_drop_the_listings_they_touch() {
    let m = mock();
    let a = m.account();
    let mut up = Uploads::default();
    names(&a, "/fotos");
    names(&a, "/fotos/2024");
    names(&a, "/fotos/2024/ferias");
    put(&a, &mut up, "/fotos/2024/nova.jpg", b"nova");
    // The folder and those above it (a folder may have appeared there); not those beside or under.
    assert!(a.cached("/fotos/2024").is_none() && a.cached("/fotos").is_none());
    assert!(a.cached("/fotos/2024/ferias").is_some());
    assert!(names(&a, "/fotos/2024").contains(&"nova.jpg".to_string()));
    // A folder renamed: its old and new places and everything under them.
    a.rename("/fotos/2024", "/fotos/2023").unwrap();
    assert!(a.cached("/fotos").is_none() && a.cached("/fotos/2024/ferias").is_none());
    assert_eq!(names(&a, "/fotos"), ["2023/", "leia-me.txt", "vazia/"]);
    // The last file of a folder hidden: the folder leaves the listing above.
    names(&a, "/fotos/2023/ferias");
    a.hide("/fotos/2023/ferias/a.jpg", false).unwrap();
    assert_eq!(names(&a, "/fotos/2023"), ["nova.jpg", "praia.jpg"]);
    // And a file into folders that didn't exist shows them.
    put(&a, &mut up, "/fotos/x/y/z.txt", b"z");
    assert!(names(&a, "/fotos").contains(&"x/".to_string()));
}

#[test]
fn listings_survive_a_restart_sealed_under_the_key() {
    let m = mock();
    let dir = std::env::temp_dir().join(format!("oe-b2-flows-disk-{}", std::process::id()));
    let file = cache::file_in(&dir, crate::mock::KEY_ID);
    {
        let a = Account::new(m.api(), Some(file.clone()));
        names(&a, "/");
        names(&a, "/fotos/2024");
        a.lists.save_if_changed();
    }
    m.reset_counts();
    let a = Account::new(m.api(), Some(file.clone()));
    assert_eq!(a.cached("/fotos/2024").unwrap().items.len(), 2);
    assert_eq!(a.known_buckets().len(), 2, "the buckets come back from the kept top");
    assert_eq!(m.lock().requests, 0, "a restart shows a folder seen before without waiting for B2");
    assert_eq!(a.reread("/fotos/2024").unwrap(), crate::Fresh::Same);
    // Another application key under the same id opens nothing.
    let other = Account::new(crate::api::Api::at(&m.url, crate::mock::KEY_ID, "K000outra"), Some(file.clone()));
    assert!(other.cached("/fotos/2024").is_none());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_expired_token_is_renewed_and_busy_answers_are_sent_again() {
    let m = mock();
    let a = m.account();
    names(&a, "/");
    m.faults.expire.store(1, Ordering::SeqCst);
    assert_eq!(names(&a, "/fotos").len(), 3);
    assert_eq!(m.calls("b2_authorize_account"), 2);
    m.faults.busy.store(2, Ordering::SeqCst);
    m.faults.too_many.store(1, Ordering::SeqCst);
    assert_eq!(a.list_fresh("/fotos/2024").unwrap().len(), 2);
    // Expired in the middle of a download, too.
    m.faults.expire.store(1, Ordering::SeqCst);
    assert_eq!(get(&a, "/fotos/2024/praia.jpg"), b"praia");
}

#[test]
fn a_key_limited_to_a_bucket_and_prefix_sees_the_way_down() {
    let m = mock();
    {
        let mut st = m.lock();
        st.allowed_buckets = Some(vec![("bkt_fotos".into(), "fotos".into())]);
        st.name_prefix = Some("2024/ferias/".into());
    }
    let a = m.account();
    // The folder that is the key's prefix, asked about before any listing.
    assert!(a.stat("/fotos/2024/ferias").unwrap().unwrap().is_dir);
    assert!(a.stat("/fotos/outra").unwrap().is_none());
    assert_eq!(a.label().unwrap(), "B2 · fotos");
    assert_eq!(names(&a, "/"), ["fotos/"]);
    assert_eq!(names(&a, "/fotos"), ["2024/"]);
    assert_eq!(names(&a, "/fotos/2024"), ["ferias/"]);
    assert_eq!(names(&a, "/fotos/2024/ferias"), ["a.jpg"]);
    assert!(a.list("/fotos/outra").is_err());
    assert!(a.stat("/fotos/2024").unwrap().unwrap().is_dir);
    let mut up = Uploads::default();
    put(&a, &mut up, "/fotos/2024/ferias/b.jpg", b"bbb");
    assert_eq!(get(&a, "/fotos/2024/ferias/b.jpg"), b"bbb");
}

#[test]
fn small_uploads_reuse_their_url_and_take_a_new_one_when_turned_down() {
    let m = mock();
    let a = m.account();
    let mut up = Uploads::default();
    put(&a, &mut up, "/docs/a.txt", b"um");
    put(&a, &mut up, "/docs/b.txt", b"dois");
    assert_eq!(m.calls("b2_get_upload_url"), 1, "one URL for both");
    m.faults.upload_busy.store(2, Ordering::SeqCst);
    put(&a, &mut up, "/docs/c.txt", b"tres");
    assert_eq!(m.calls("b2_get_upload_url"), 3, "a new URL after each 503");
    assert_eq!(get(&a, "/docs/c.txt"), b"tres");
    // Spaces, accents and symbols in the name.
    put(&a, &mut up, "/docs/relatório final + 100%.txt", b"x");
    assert!(names(&a, "/docs").contains(&"relatório final + 100%.txt".to_string()));
    assert_eq!(get(&a, "/docs/relatório final + 100%.txt"), b"x");
    // An empty file.
    put(&a, &mut up, "/docs/vazio", b"");
    assert_eq!(get(&a, "/docs/vazio"), b"");
}

fn small_parts(m: &Mock) {
    let mut st = m.lock();
    st.recommended = 1 << 20;
    st.minimum = 100_000;
}

#[test]
fn large_files_go_in_parts_side_by_side_each_thread_with_its_url() {
    let m = mock();
    small_parts(&m);
    let a = m.account();
    let mut up = Uploads::default();
    let data = noise(5 * (1 << 20) + 12_345);
    let mut seen = Vec::new();
    a.upload(&mut up, "/fotos/grande.bin", &mut &data[..], data.len() as u64, &mut |n| {
        seen.push(n);
        true
    })
    .unwrap();
    assert_eq!(m.calls("b2_upload_part"), 6);
    assert!(m.calls("b2_get_upload_part_url") <= crate::upload::PARTS_AT_ONCE);
    assert_eq!(seen.last(), Some(&(data.len() as u64)));
    assert!(m.lock().large.is_empty());
    assert_eq!(get(&a, "/fotos/grande.bin"), data);
    // A busy part goes again through a new URL.
    m.faults.upload_busy.store(2, Ordering::SeqCst);
    put(&a, &mut up, "/fotos/grande2.bin", &data);
    assert_eq!(get(&a, "/fotos/grande2.bin"), data);
}

/// Reads `data`, failing after `stop_at` bytes.
struct Breaks<'a> {
    data: &'a [u8],
    at: usize,
    stop_at: usize,
}

impl Read for Breaks<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.at >= self.stop_at {
            return Err(std::io::Error::other("disco removido"));
        }
        let n = buf.len().min(self.stop_at - self.at).min(self.data.len() - self.at);
        buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

#[test]
fn a_large_upload_that_failed_resumes_with_the_parts_b2_has() {
    let m = mock();
    small_parts(&m);
    let a = m.account();
    let mut up = Uploads::default();
    let data = noise(6 * (1 << 20));
    let mut broken = Breaks { data: &data, at: 0, stop_at: 3 * (1 << 20) + 10 };
    assert!(a.upload(&mut up, "/fotos/retoma.bin", &mut broken, data.len() as u64, &mut |_| true).is_err());
    let kept = m.lock().large.values().map(|l| l.parts.len()).sum::<usize>();
    assert_eq!(m.lock().large.len(), 1, "the unfinished upload stays to be resumed");
    assert!(kept >= 1);
    m.reset_counts();
    put(&a, &mut up, "/fotos/retoma.bin", &data);
    assert_eq!(m.calls("b2_upload_part"), 6 - kept, "only the parts B2 didn't have");
    assert_eq!(m.calls("b2_start_large_file"), 0);
    assert_eq!(get(&a, "/fotos/retoma.bin"), data);
    // Another size to the same name: the stale unfinished upload is cancelled.
    let mut broken = Breaks { data: &data, at: 0, stop_at: 2 * (1 << 20) };
    assert!(a.upload(&mut up, "/fotos/outro.bin", &mut broken, data.len() as u64, &mut |_| true).is_err());
    let shorter = &data[..5 * (1 << 20)];
    put(&a, &mut up, "/fotos/outro.bin", shorter);
    assert!(m.lock().large.is_empty());
    assert_eq!(get(&a, "/fotos/outro.bin"), shorter);
}

#[test]
fn unfinished_uploads_show_what_b2_holds_and_go_on_or_are_cancelled() {
    let _one = crate::notes::TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("oe-b2-flows-unfinished-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    crate::notes::set_file(Some(dir.join("b2-uploads.json")));
    let m = mock();
    small_parts(&m);
    let a = m.account();
    let mut up = Uploads::default();
    let data = noise(6 * (1 << 20));
    let len = data.len() as u64;
    let local = dir.join("video.bin");
    std::fs::write(&local, &data).unwrap();
    // This app sent the file from here and stopped midway (as upload_file notes it).
    crate::notes::begin(crate::mock::KEY_ID, "fotos", "video.bin", len, &local);
    let mut broken = Breaks { data: &data, at: 0, stop_at: 3 * (1 << 20) + 10 };
    assert!(a.upload(&mut up, "/fotos/video.bin", &mut broken, len, &mut |_| true).is_err());
    // Another program's, older, with one part.
    let part = std::sync::Arc::new(vec![1u8; 100]);
    m.lock().large.insert("4_zoutro".into(), crate::mock::Large { bucket: "bkt_fotos".into(), name: "de-outro.bin".into(), info: Default::default(), parts: [(1, (part.clone(), crate::api::sha1_hex(&part)))].into(), ts: 1 });
    let list = a.unfinished_uploads("fotos").unwrap();
    assert_eq!(list.iter().map(|u| u.name.as_str()).collect::<Vec<_>>(), ["de-outro.bin", "video.bin"], "oldest first");
    assert_eq!((list[0].size, list[0].local.as_ref(), list[0].sent, list[0].parts), (None, None, 100, 1));
    let mine = &list[1];
    assert_eq!((mine.size, mine.local.as_ref(), mine.sending), (Some(len), Some(&local), false));
    assert!(mine.parts >= 1 && mine.sent == mine.parts as u64 * (1 << 20), "{mine:?}");
    // Taken up again from the file on this computer: only the parts B2 doesn't hold go.
    m.reset_counts();
    a.upload_file(&mut up, "/fotos/video.bin", &local, &mut |_| true).unwrap();
    assert_eq!(m.calls("b2_upload_part"), 6 - mine.parts);
    assert_eq!(get(&a, "/fotos/video.bin"), data);
    assert_eq!(crate::notes::local_of(crate::mock::KEY_ID, "fotos", "video.bin", len), None, "its note went with it");
    // One being sent now isn't cancelled from the list; otherwise it is, its parts deleted.
    let busy = crate::upload::InFlight::new("4_zoutro");
    assert_eq!(a.cancel_unfinished(&["4_zoutro".into()]).unwrap(), 0);
    drop(busy);
    assert_eq!(a.cancel_unfinished(&["4_zoutro".into()]).unwrap(), 1);
    assert!(m.lock().large.is_empty() && a.unfinished_uploads("fotos").unwrap().is_empty());
    crate::notes::set_file(None);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unfinished_uploads_list_past_a_page_and_parts_past_theirs() {
    let m = mock();
    let a = m.account();
    {
        let mut st = m.lock();
        for i in 0..130u64 {
            let parts = if i == 0 { (1..=1500u64).map(|n| (n, (std::sync::Arc::new(vec![0u8; 2]), String::new()))).collect() } else { Default::default() };
            st.large.insert(format!("4_zl{i:04}"), crate::mock::Large { bucket: "bkt_docs".into(), name: format!("l{i}"), info: Default::default(), parts, ts: i as i64 });
        }
    }
    let list = a.unfinished_uploads("docs").unwrap();
    assert_eq!(list.len(), 130);
    assert_eq!((list[0].parts, list[0].sent), (1500, 3000));
    assert_eq!(m.calls("b2_list_unfinished_large_files"), 2);
}

#[test]
fn cancelling_a_large_upload_cancels_it_on_b2() {
    let m = mock();
    small_parts(&m);
    let a = m.account();
    let mut up = Uploads::default();
    let data = noise(8 * (1 << 20));
    let e = a.upload(&mut up, "/fotos/cancela.bin", &mut &data[..], data.len() as u64, &mut |n| n < 2 * (1 << 20)).unwrap_err();
    assert_eq!(e, crate::CANCELLED);
    assert!(m.lock().large.is_empty(), "its parts don't stay taking storage");
    assert!(a.stat("/fotos/cancela.bin").unwrap().is_none());
}

#[test]
fn a_download_cut_midway_resumes_by_the_version_id() {
    let m = mock();
    let a = m.account();
    let data = noise(700_000);
    m.lock().add("bkt_fotos", "video.mp4", &data);
    m.faults.cut_download.store(300_000, Ordering::SeqCst);
    assert_eq!(get(&a, "/fotos/video.mp4"), data);
    assert_eq!(m.calls("b2_download_file_by_id"), 1, "picked up by id, with a range");
    // Bytes that don't match B2's SHA-1 are refused.
    {
        let mut st = m.lock();
        let v = st.versions.iter_mut().find(|v| v.name == "video.mp4").unwrap();
        v.data = std::sync::Arc::new(noise(700_001)[1..].to_vec());
    }
    let mut out = Vec::new();
    assert!(a.download("/fotos/video.mp4", &mut out, &mut |_| true).unwrap_err().contains("SHA-1"));
    assert!(a.download("/fotos/nada.mp4", &mut Vec::new(), &mut |_| true).unwrap_err().contains("não está mais"));
}

#[test]
fn deleting_hides_and_undoing_brings_back() {
    let m = mock();
    let a = m.account();
    let since = a.hide("/fotos/leia-me.txt", false).unwrap();
    assert!(!names(&a, "/fotos").contains(&"leia-me.txt".to_string()));
    assert_eq!(a.hidden_since("/fotos/leia-me.txt", false, since).unwrap().len(), 1);
    assert_eq!(a.unhide("/fotos/leia-me.txt", false, since).unwrap(), 1);
    assert_eq!(get(&a, "/fotos/leia-me.txt"), b"oi");
    // A whole folder, then back.
    let since = a.hide("/fotos/2024", true).unwrap();
    assert_eq!(names(&a, "/fotos"), ["leia-me.txt", "vazia/"]);
    assert_eq!(a.unhide("/fotos/2024", true, since).unwrap(), 2);
    assert_eq!(names(&a, "/fotos/2024"), ["ferias/", "praia.jpg"]);
    // A hide sent twice (its first answer lost) leaves two markers: both go.
    let since = a.hide("/fotos/leia-me.txt", false).unwrap();
    m.lock().hide("bkt_fotos", "leia-me.txt");
    assert_eq!(a.unhide("/fotos/leia-me.txt", false, since).unwrap(), 1);
    assert_eq!(get(&a, "/fotos/leia-me.txt"), b"oi");
    // A file hidden then uploaded again isn't brought back over the new one.
    let since = a.hide("/fotos/leia-me.txt", false).unwrap();
    put(&a, &mut Uploads::default(), "/fotos/leia-me.txt", b"novo");
    assert!(a.hidden_since("/fotos/leia-me.txt", false, since).unwrap().is_empty());
    assert_eq!(get(&a, "/fotos/leia-me.txt"), b"novo");
}

#[test]
fn deleting_every_version_leaves_nothing() {
    let m = mock();
    small_parts(&m);
    let a = m.account();
    let mut up = Uploads::default();
    put(&a, &mut up, "/fotos/2024/praia.jpg", b"v2");
    a.hide("/fotos/leia-me.txt", false).unwrap();
    // An unfinished upload in the folder goes too.
    let data = noise(3 << 20);
    let mut broken = Breaks { data: &data, at: 0, stop_at: 1 << 20 };
    assert!(a.upload(&mut up, "/fotos/2024/meio.bin", &mut broken, data.len() as u64, &mut |_| true).is_err());
    assert_eq!(m.lock().large.len(), 1);
    assert_eq!(a.purge("/fotos/2024", true).unwrap(), 4);
    assert!(m.lock().large.is_empty());
    assert_eq!(a.purge("/fotos/leia-me.txt", false).unwrap(), 2);
    let st = m.lock();
    assert!(st.versions.iter().all(|v| !v.name.starts_with("2024/") && v.name != "leia-me.txt"));
    // The name "leia-me.txt.bak" would have matched the prefix: it's not touched.
    drop(st);
}

#[test]
fn renames_copy_on_b2_and_retire_the_old_name_keeping_history() {
    let m = mock();
    let a = m.account();
    // One version: the copied one is deleted, no hide marker left behind.
    a.rename("/fotos/leia-me.txt", "/fotos/LEIA.txt").unwrap();
    assert_eq!(get(&a, "/fotos/LEIA.txt"), b"oi");
    assert!(!m.lock().versions.iter().any(|v| v.name == "leia-me.txt"));
    // Two versions: the old name is hidden, its history kept.
    let mut up = Uploads::default();
    put(&a, &mut up, "/fotos/2024/praia.jpg", b"v2");
    a.rename("/fotos/2024/praia.jpg", "/docs/praia.jpg").unwrap();
    assert_eq!(get(&a, "/docs/praia.jpg"), b"v2");
    assert_eq!(m.lock().versions.iter().filter(|v| v.name == "2024/praia.jpg").count(), 3);
    assert!(a.stat("/fotos/2024/praia.jpg").unwrap().is_none());
    // Taken names and folders into themselves are refused before asking B2 to copy.
    assert!(a.rename("/fotos/LEIA.txt", "/fotos/2024/ferias/a.jpg").unwrap_err().contains("Já existe"));
    assert!(a.rename("/fotos/2024", "/fotos/2024/dentro").unwrap_err().contains("dentro dela"));
    assert!(a.rename("/fotos", "/outro").unwrap_err().contains("Buckets"));
    // A folder with its empty-folder marker.
    a.rename("/fotos/vazia", "/fotos/Vazia2").unwrap();
    assert_eq!(names(&a, "/fotos"), ["2024/", "LEIA.txt", "Vazia2/"]);
    assert!(m.lock().versions.iter().any(|v| v.name == format!("Vazia2/{EMPTY_MARKER}")));
}

#[test]
fn copies_stay_on_b2_and_big_ones_go_in_parts() {
    let m = mock();
    small_parts(&m);
    let a = m.account();
    a.copy("/fotos/2024", "/docs/copia").unwrap();
    assert_eq!(names(&a, "/docs/copia"), ["ferias/", "praia.jpg"]);
    assert_eq!(names(&a, "/fotos/2024"), ["ferias/", "praia.jpg"]);
    assert_eq!(m.calls("b2_download_file_by_name") + m.calls("b2_upload_file"), 0, "no bytes through the app");
    // A file of 5 GB or more is copied in parts; here, with the parts made small.
    let data = noise(2 * (1 << 20) + 77);
    let id = m.lock().add("bkt_fotos", "enorme.iso", &data);
    let bucket = a.bucket("docs").unwrap();
    let v = a.file(&a.bucket("fotos").unwrap(), "enorme.iso").unwrap().unwrap();
    assert_eq!(v.id, id);
    a.copy_large(&v, &bucket, "enorme.iso").unwrap();
    assert_eq!(m.calls("b2_copy_part"), 3);
    assert_eq!(get(&a, "/docs/enorme.iso"), data);
}

#[test]
fn new_folders_are_markers_and_go_back_empty() {
    let m = mock();
    let a = m.account();
    let mut up = Uploads::default();
    a.mkdir(&mut up, "/fotos/Nova").unwrap();
    assert!(names(&a, "/fotos").contains(&"Nova/".to_string()));
    assert!(names(&a, "/fotos/Nova").is_empty());
    assert!(a.mkdir(&mut up, "/fotos/Nova").unwrap_err().contains("Já existe"));
    assert!(a.mkdir(&mut up, "/novo-bucket").unwrap_err().contains("Buckets"));
    a.remove_dir("/fotos/Nova").unwrap();
    assert!(!names(&a, "/fotos").contains(&"Nova/".to_string()));
}
