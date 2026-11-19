//! The account against the stand-in for Graph (`fake.rs`): listings kept and brought up to
//! date by the delta feed, every operation, transfers that drop midway, throttling, renewal
//! and the tree kept on disk.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use crate::account::{Account, Kind, Meta, on};
use crate::fake::{DRIVE, Fake, ROOT};
use crate::graph::Graph;

const TOP: &str = "/Meus arquivos";

fn names(a: &Account, path: &str) -> Vec<String> {
    let mut v: Vec<String> = a.list(path).unwrap().into_iter().map(|i| i.name).collect();
    v.sort();
    v
}

/// Docs (a.txt, b.txt) and Fotos (empty) at the top, and the account opened.
fn setup(kind: Kind) -> (Fake, Account, String, String) {
    let fake = Fake::start();
    let (docs, fotos) = fake.with(|st| {
        let docs = st.add(ROOT, "Docs", true, &[]);
        st.add(&docs, "a.txt", false, b"alfa");
        st.add(&docs, "b.txt", false, b"beta");
        (docs, st.add(ROOT, "Fotos", true, &[]))
    });
    let a = on(fake.ends(), kind, None).unwrap();
    (fake, a, docs, fotos)
}

#[test]
fn the_top_holds_my_files_and_folders_list_by_name() {
    let (_fake, a, _, _) = setup(Kind::Personal);
    assert_eq!(names(&a, "/"), ["Meus arquivos"]);
    assert_eq!(names(&a, TOP), ["Docs", "Fotos"]);
    assert_eq!(names(&a, "/My files/docs"), ["a.txt", "b.txt"]);
    let it = a.stat("/Meus arquivos/Docs/a.txt").unwrap().unwrap();
    assert_eq!((it.is_dir, it.size), (false, Some(4)));
    assert!(a.stat("/Meus arquivos/Nada").unwrap().is_none());
    assert!(a.list("/Meus arquivos/Docs/a.txt").is_err());
}

#[test]
fn an_unchanged_folder_reopens_with_one_delta_request() {
    let (fake, a, _, _) = setup(Kind::Personal);
    names(&a, "/Meus arquivos/Docs");
    let before = fake.requests();
    assert!(a.catch_up().unwrap().is_empty());
    assert_eq!(names(&a, "/Meus arquivos/Docs"), ["a.txt", "b.txt"]);
    assert_eq!(fake.requests() - before, 1);
}

#[test]
fn changes_elsewhere_reach_kept_folders_without_listing_them_again() {
    let (fake, a, docs, fotos) = setup(Kind::Personal);
    names(&a, "/Meus arquivos/Docs");
    names(&a, "/Meus arquivos/Fotos");
    let (a_id, b_id) = fake.with(|st| {
        let ids: Vec<String> = st.items.values().filter(|i| i.name == "a.txt" || i.name == "b.txt").map(|i| i.id.clone()).collect();
        (ids[0].clone(), ids[1].clone())
    });
    fake.with(|st| {
        st.add(&docs, "novo.txt", false, b"n");
        st.touch(&a_id, |i| i.parent = Some(fotos.clone()));
        st.touch(&b_id, |i| i.deleted = true);
    });
    let before = fake.requests();
    let c = a.poll().unwrap();
    assert!(c.touches(&docs) && c.touches(&fotos) && !c.touches(ROOT));
    assert_eq!(names(&a, "/Meus arquivos/Docs"), ["novo.txt"]);
    assert_eq!(names(&a, "/Meus arquivos/Fotos"), ["a.txt"]);
    assert_eq!(fake.requests() - before, 1);
    assert_eq!(a.folder_id("/Meus arquivos/Docs").as_deref(), Some(docs.as_str()));
}

#[test]
fn folders_are_made_renamed_moved_and_deleted() {
    let (fake, a, _, fotos) = setup(Kind::Personal);
    a.mkdir("/Meus arquivos/Docs/Nova").unwrap();
    assert!(a.mkdir("/Meus arquivos/Docs/nova").unwrap_err().contains("já existe"));
    assert!(a.mkdir("/Meus arquivos/Docs/a:b").is_err());
    assert!(a.mkdir("/Meus arquivos").is_err());
    a.rename("/Meus arquivos/Docs/a.txt", "/Meus arquivos/Docs/A.txt").unwrap();
    assert!(a.rename("/Meus arquivos/Docs/A.txt", "/Meus arquivos/Docs/b.txt").unwrap_err().contains("Já existe"));
    a.rename("/Meus arquivos/Docs/A.txt", "/Meus arquivos/Fotos/a.txt").unwrap();
    // Taken in at once, before the delta feed says so.
    assert_eq!(names(&a, "/Meus arquivos/Docs"), ["Nova", "b.txt"]);
    assert_eq!(names(&a, "/Meus arquivos/Fotos"), ["a.txt"]);
    let id = a.id_of("/Meus arquivos/Fotos/a.txt").unwrap();
    a.remove("/Meus arquivos/Fotos/a.txt").unwrap();
    assert!(names(&a, "/Meus arquivos/Fotos").is_empty());
    assert!(fake.with(|st| st.items[&id].deleted && st.items[&id].parent.as_deref() == Some(fotos.as_str())));
    // Back from the recycle bin, where it was.
    a.restore(&id).unwrap();
    assert_eq!(names(&a, "/Meus arquivos/Fotos"), ["a.txt"]);
    // The delta feed bringing the same states again changes nothing.
    a.catch_up().unwrap();
    assert_eq!(names(&a, "/Meus arquivos/Docs"), ["Nova", "b.txt"]);
}

#[test]
fn several_items_go_to_the_bin_and_back_in_one_batch() {
    let (fake, a, _, _) = setup(Kind::Personal);
    names(&a, "/Meus arquivos/Docs");
    let before = fake.requests();
    let (gone, err) = a.trash_many(&["/Meus arquivos/Docs/a.txt".into(), "/Meus arquivos/Docs/b.txt".into()]);
    assert_eq!((gone.len(), err), (2, None));
    assert_eq!(fake.requests() - before, 1, "one $batch");
    assert!(names(&a, "/Meus arquivos/Docs").is_empty());
    let ids: Vec<String> = gone.iter().map(|(id, _)| id.clone()).collect();
    let (back, err) = a.restore_many(&ids);
    assert_eq!((back.len(), err), (2, None));
    assert_eq!(names(&a, "/Meus arquivos/Docs"), ["a.txt", "b.txt"]);
}

#[test]
fn work_accounts_say_restoring_is_on_the_website() {
    let (_fake, a, _, _) = setup(Kind::Business);
    let (_, err) = a.restore_many(&["x".into()]);
    assert!(err.unwrap().contains("site"));
}

#[test]
fn copies_happen_on_the_server_and_are_followed_to_the_end() {
    let (fake, a, _, _) = setup(Kind::Personal);
    a.copy("/Meus arquivos/Docs", "/Meus arquivos/Fotos/Docs 2").unwrap();
    assert_eq!(names(&a, "/Meus arquivos/Fotos"), ["Docs 2"]);
    assert_eq!(names(&a, "/Meus arquivos/Fotos/Docs 2"), ["a.txt", "b.txt"]);
    assert!(a.copy("/Meus arquivos/Docs", "/Meus arquivos/Fotos/docs 2").unwrap_err().contains("Já existe"));
    assert_eq!(fake.with(|st| st.items.values().filter(|i| i.name == "a.txt" && !i.deleted).count()), 2);
}

fn data(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
}

#[test]
fn a_dropped_download_resumes_and_is_checked() {
    let (fake, a, docs, _) = setup(Kind::Personal);
    let body = data(700_000);
    fake.with(|st| st.add(&docs, "grande.bin", false, &body));
    fake.with(|st| st.cut_download = Some(123_457));
    let mut out = Vec::new();
    let mut last = 0;
    a.download("/Meus arquivos/Docs/grande.bin", &mut out, &mut |n| {
        last = n;
        true
    })
    .unwrap();
    assert_eq!(out, body);
    assert_eq!(last, 700_000);
    // Stopped by Cancelar.
    let err = a.download("/Meus arquivos/Docs/grande.bin", &mut Vec::new(), &mut |_| false).unwrap_err();
    assert_eq!(err, crate::CANCELLED);
}

#[test]
fn uploads_go_whole_or_in_session_parts_that_resume() {
    let (fake, a, docs, _) = setup(Kind::Personal);
    let small = data(1000);
    a.upload("/Meus arquivos/Docs/p.bin", &mut &small[..], 1000, &mut |_| true).unwrap();
    // Bigger than 4 MiB: a session in parts of 10 MiB, one of which fails halfway.
    let big = data(crate::transfer::PART + 3_000_000);
    fake.with(|st| st.cut_upload = true);
    let mut seen = Vec::new();
    let it = a.upload("/Meus arquivos/Docs/g.bin", &mut &big[..], big.len() as u64, &mut |n| {
        seen.push(n);
        true
    })
    .unwrap();
    assert_eq!(it.size, Some(big.len() as u64));
    assert_eq!(*seen.last().unwrap(), big.len() as u64);
    assert!(seen.windows(2).filter(|w| w[1] < w[0]).count() <= 1, "progress goes back once, to where the session was");
    fake.with(|st| {
        let g = st.items.values().find(|i| i.name == "g.bin").unwrap();
        assert!(g.data == big && g.parent.as_deref() == Some(docs.as_str()));
    });
    assert_eq!(names(&a, "/Meus arquivos/Docs"), ["a.txt", "b.txt", "g.bin", "p.bin"]);
    // Replacing keeps one item.
    a.upload("/Meus arquivos/Docs/p.bin", &mut &small[..10], 10, &mut |_| true).unwrap();
    assert_eq!(a.stat("/Meus arquivos/Docs/p.bin").unwrap().unwrap().size, Some(10));
    // Cancelled midway: the session is deleted.
    let err = a.upload("/Meus arquivos/Docs/c.bin", &mut &big[..], big.len() as u64, &mut |n| n < 1_000_000).unwrap_err();
    assert_eq!(err, crate::CANCELLED);
    assert!(fake.with(|st| !st.items.values().any(|i| i.name == "c.bin")));
}

#[test]
fn throttling_waits_and_goes_on() {
    let (fake, a, _, _) = setup(Kind::Personal);
    fake.with(|st| st.throttle = 2);
    assert_eq!(names(&a, "/Meus arquivos/Docs"), ["a.txt", "b.txt"]);
}

#[test]
fn a_refused_delta_position_lists_again() {
    let (fake, a, _, _) = setup(Kind::Personal);
    names(&a, "/Meus arquivos/Docs");
    fake.with(|st| st.resync_before = st.version + 1);
    fake.with(|st| {
        let v = st.version + 1;
        st.version = v;
    });
    let c = a.catch_up().unwrap();
    assert!(c.all);
    assert_eq!(a.kept(), 1);
    fake.with(|st| st.resync_before = 0);
    assert_eq!(names(&a, "/Meus arquivos/Docs"), ["a.txt", "b.txt"]);
}

#[test]
fn renewed_tokens_are_kept_as_they_come() {
    let fake = Fake::start();
    let kept = Arc::new(Mutex::new(Vec::<String>::new()));
    let k = kept.clone();
    let cfg = crate::ClientConfig { client_id: "0b1c2d3e-4f50-6172-8394-a5b6c7d8e9f0".into(), tenant: "common".into() };
    let graph = Graph::new(cfg, fake.ends(), "r0".into(), Box::new(move |t| k.lock().unwrap().push(t.into())));
    let a = Account::start(Meta { id: DRIVE.into(), email: "x".into(), name: "x".into(), kind: Kind::Personal }, graph, None).unwrap();
    a.list(TOP).unwrap();
    assert_eq!(*kept.lock().unwrap(), ["r1"]);
    assert_eq!(a.graph.refresh_token(), "r1");
}

#[test]
fn the_kept_tree_opens_the_next_start_with_one_request() {
    let (fake, a, docs, _) = setup(Kind::Personal);
    let dir = std::env::temp_dir().join(format!("oe-onedrive-scn-{}", std::process::id()));
    let keep = || Some((crate::cache::file_in(&dir, DRIVE), crate::cache::Secret::of(DRIVE, &[9; 32])));
    drop(a);
    let a = on(fake.ends(), Kind::Personal, keep()).unwrap();
    assert!(!a.started_from_cache());
    names(&a, "/Meus arquivos/Docs");
    a.save_cache();
    drop(a);
    fake.with(|st| st.add(&docs, "c.txt", false, b"c"));
    let before = fake.requests();
    let b = on(fake.ends(), Kind::Personal, keep()).unwrap();
    assert!(b.started_from_cache());
    assert_eq!(names(&b, "/Meus arquivos/Docs"), ["a.txt", "b.txt", "c.txt"]);
    // A new access token (they aren't kept) and the delta feed.
    assert_eq!(fake.requests() - before, 2);
    b.forgotten.store(true, Ordering::SeqCst);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn links_are_made_found_and_removed() {
    let (_fake, a, _, _) = setup(Kind::Personal);
    let p = "/Meus arquivos/Docs/a.txt";
    assert_eq!(a.link_of(p).unwrap(), None);
    let link = a.create_link(p).unwrap();
    assert!(link.starts_with("https://1drv.ms/"));
    assert_eq!(a.link_of(p).unwrap(), Some(link));
    assert_eq!(a.remove_link(p).unwrap(), 1);
    assert_eq!(a.link_of(p).unwrap(), None);
}

#[test]
fn the_quota_is_read() {
    let (_fake, a, _, _) = setup(Kind::Personal);
    let q = a.quota().unwrap();
    assert_eq!((q.total, q.used), (5_368_709_120, 8));
}
