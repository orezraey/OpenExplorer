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
fn an_empty_folder_goes_only_while_graph_says_it_is_empty() {
    let (fake, a, _, fotos) = setup(Kind::Personal);
    assert!(names(&a, "/Meus arquivos/Fotos").is_empty());
    // Something arrived from elsewhere, not yet in the kept listing.
    fake.with(|st| st.add(&fotos, "novo.jpg", false, b"x"));
    assert!(a.remove_empty_dir("/Meus arquivos/Fotos").unwrap_err().contains("não está vazia"));
    assert!(a.remove_empty_dir("/Meus arquivos/Docs/a.txt").is_err());
    a.mkdir("/Meus arquivos/Vazia").unwrap();
    a.remove_empty_dir("/Meus arquivos/Vazia").unwrap();
    assert_eq!(names(&a, TOP), ["Docs", "Fotos"]);
}

#[test]
fn folders_opened_at_once_ask_the_delta_feed_once() {
    let (fake, a, _, _) = setup(Kind::Personal);
    names(&a, "/Meus arquivos/Docs");
    names(&a, "/Meus arquivos/Fotos");
    std::thread::sleep(std::time::Duration::from_millis(3100));
    let before = fake.requests();
    std::thread::scope(|sc| {
        for p in ["/Meus arquivos/Docs", "/Meus arquivos/Fotos", "/Meus arquivos", "/Meus arquivos/Docs"] {
            let a = &a;
            sc.spawn(move || a.list(p).unwrap());
        }
        // Changes taken in meanwhile, on another thread, as the watch does.
        sc.spawn(|| a.catch_up().unwrap());
    });
    assert!(fake.requests() - before <= 2, "{} requests", fake.requests() - before);
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
fn personal_accounts_undo_and_redo_several_deletes() {
    let (fake, a, _, _) = setup(Kind::Personal);
    assert_eq!(names(&a, "/"), ["Meus arquivos"]);
    assert!(a.stat("/Lixeira").unwrap().is_none() && a.list("/Lixeira").is_err());
    let (gone, _) = a.trash_many(&["/Meus arquivos/Docs/a.txt".into(), "/Meus arquivos/Docs/b.txt".into(), "/Meus arquivos/Fotos".into()]);
    let ids: Vec<String> = gone.iter().map(|(id, _)| id.clone()).collect();
    assert!(ids.iter().all(|id| a.can_restore(id)));
    // Desfazer goes item by item, as the undo steps do.
    for id in &ids {
        a.restore(id).unwrap();
    }
    assert_eq!(names(&a, TOP), ["Docs", "Fotos"]);
    assert_eq!(names(&a, "/Meus arquivos/Docs"), ["a.txt", "b.txt"]);
    // Refazer, then Desfazer again.
    for id in &ids {
        a.trash_id(id).unwrap();
    }
    assert!(names(&a, "/Meus arquivos/Docs").is_empty());
    let (back, err) = a.restore_many(&ids);
    assert_eq!((back.len(), err), (3, None));
    assert_eq!(names(&a, "/Meus arquivos/Docs"), ["a.txt", "b.txt"]);
    assert_eq!(fake.with(|st| st.sp_requests), 0);
}

/// 25 files in Docs besides a.txt and b.txt, their paths, and Docs listed.
fn many(fake: &Fake, a: &Account, docs: &str) -> Vec<String> {
    let paths = fake.with(|st| {
        (0..25)
            .map(|i| {
                st.add(docs, &format!("f{i:02}.txt"), false, b"f");
                format!("/Meus arquivos/Docs/f{i:02}.txt")
            })
            .collect::<Vec<_>>()
    });
    a.catch_up().unwrap();
    assert_eq!(names(a, "/Meus arquivos/Docs").len(), 27);
    paths
}

/// What's left of f00…f24 in Docs, as the account has it and as Graph does.
fn left(fake: &Fake, a: &Account) -> (Vec<String>, Vec<String>) {
    let mut graph: Vec<String> = fake.with(|st| st.items.values().filter(|i| !i.deleted && i.name.starts_with('f')).map(|i| i.name.clone()).collect());
    graph.sort();
    (names(a, "/Meus arquivos/Docs").into_iter().filter(|n| n.starts_with('f')).collect(), graph)
}

#[test]
fn a_batch_cut_midway_keeps_what_its_first_part_deleted() {
    for kind in [Kind::Personal, Kind::Business] {
        let (fake, a, docs, _) = setup(kind);
        fake.with(|st| {
            st.business = kind == Kind::Business;
            st.sp_granted = true;
        });
        let paths = many(&fake, &a, &docs);
        // The second $batch (items 21 to 25) gets no answer.
        fake.with(|st| st.drop_batch = Some(1));
        let (gone, err) = a.trash_many(&paths);
        assert_eq!(gone.len(), 20, "{kind:?}");
        assert!(err.unwrap().contains("Não consegui falar com o OneDrive"));
        let rest: Vec<String> = (20..25).map(|i| format!("f{i:02}.txt")).collect();
        // Out of the kept tree at once, without waiting for the delta feed.
        assert_eq!(names(&a, "/Meus arquivos/Docs").into_iter().filter(|n| n.starts_with('f')).collect::<Vec<_>>(), rest);
        assert_eq!(left(&fake, &a), (rest.clone(), rest.clone()));
        // Each of the 20 can be undone.
        let ids: Vec<String> = gone.iter().map(|(id, _)| id.clone()).collect();
        assert!(ids.iter().all(|id| a.can_restore(id)), "{kind:?}");
        // Undone. On a personal account the 20 restores go in one $batch and the reading of
        // where they came back to, cut, in another: they're back all the same, and the delta
        // feed brings them to the tree.
        fake.with(|st| st.drop_batch = Some(1));
        let (back, err) = a.restore_many(&ids);
        assert_eq!((back.len(), err), (20, None), "{kind:?}");
        a.catch_up().unwrap();
        assert_eq!(left(&fake, &a).0.len(), 25);
    }
}

#[test]
fn restores_cut_midway_say_which_came_back() {
    let (fake, a, docs, _) = setup(Kind::Personal);
    let paths = many(&fake, &a, &docs);
    let (gone, err) = a.trash_many(&paths);
    assert_eq!((gone.len(), err), (25, None));
    let ids: Vec<String> = gone.iter().map(|(id, _)| id.clone()).collect();
    fake.with(|st| st.drop_batch = Some(1));
    let (back, err) = a.restore_many(&ids);
    assert_eq!(back, ids[..20]);
    assert!(err.unwrap().contains("Não consegui falar com o OneDrive"));
    let first: Vec<String> = (0..20).map(|i| format!("f{i:02}.txt")).collect();
    assert_eq!(left(&fake, &a), (first.clone(), first));
}

/// A work account with SharePoint's permission granted, Docs (a.txt, b.txt) and Fotos.
fn setup_work() -> (Fake, Account, String) {
    let fake = Fake::start();
    let docs = fake.with(|st| {
        st.business = true;
        st.sp_granted = true;
        let docs = st.add(ROOT, "Docs", true, &[]);
        st.add(&docs, "a.txt", false, b"alfa");
        st.add(&docs, "b.txt", false, b"beta");
        st.add(ROOT, "Fotos", true, &[]);
        docs
    });
    let a = on(fake.ends(), Kind::Business, None).unwrap();
    (fake, a, docs)
}

fn bin_names(a: &Account) -> Vec<String> {
    let mut v = names(a, "/Lixeira");
    v.sort();
    v
}

#[test]
fn a_work_account_shows_its_bin_and_restores_and_purges_from_it() {
    let (fake, a, docs) = setup_work();
    assert_eq!(names(&a, "/"), ["Lixeira", "Meus arquivos"]);
    assert_eq!(names(&a, "/Recycle bin"), Vec::<String>::new());
    let (gone, err) = a.trash_many(&["/Meus arquivos/Docs/a.txt".into(), "/Meus arquivos/Docs/b.txt".into(), "/Meus arquivos/Fotos".into()]);
    assert_eq!((gone.len(), err), (3, None));
    assert_eq!(bin_names(&a), ["Fotos", "a.txt", "b.txt"]);
    let shown = a.bin_shown();
    let a_txt = shown.iter().find(|b| b.name == "a.txt").unwrap();
    assert_eq!((a_txt.origin.as_str(), a_txt.size, a_txt.is_dir), ("Meus arquivos/Docs", 4, false));
    assert_eq!(shown.iter().find(|b| b.name == "Fotos").unwrap().origin, "Meus arquivos");
    assert!(shown.iter().find(|b| b.name == "Fotos").unwrap().is_dir);
    let it = a.stat("/Lixeira/a.txt").unwrap().unwrap();
    assert_eq!((it.is_dir, it.size), (false, Some(4)));
    assert!(a.stat("/Lixeira/nada").unwrap().is_none());
    // Nothing is put in, renamed or opened there.
    assert!(a.mkdir("/Lixeira/Nova").is_err());
    assert!(a.rename("/Lixeira/a.txt", "/Meus arquivos/a.txt").is_err());
    assert!(a.list("/Lixeira/Fotos").unwrap_err().contains("restaure"));
    assert!(a.download("/Lixeira/a.txt", &mut Vec::new(), &mut |_| true).is_err());
    // Restaurar: back where it was, the folder brought up to date.
    let (back, err) = a.bin_restore(&["a.txt".into(), "Fotos".into()]);
    assert_eq!((back.len(), err), (2, None));
    assert_eq!(names(&a, "/Meus arquivos/Docs"), ["a.txt"]);
    assert_eq!(names(&a, TOP), ["Docs", "Fotos"]);
    // Excluir permanentemente.
    let (purged, err) = a.bin_purge(&["b.txt".into()]);
    assert_eq!((purged, err), (vec!["b.txt".to_string()], None));
    assert!(fake.with(|st| !st.items.values().any(|i| i.name == "b.txt")));
    assert!(bin_names(&a).is_empty());
    // Esvaziar: exactly what's listed.
    a.trash_many(&["/Meus arquivos/Docs/a.txt".into()]);
    fake.with(|st| {
        let id = st.add(&docs, "c.txt", false, b"c");
        st.touch(&id, |i| i.deleted = true);
        let when = crate::fake::now();
        st.put_in_bin(&id, &when);
        st.bin.last_mut().unwrap().stage = 2;
    });
    assert_eq!(a.bin_empty().unwrap(), 2);
    assert!(fake.with(|st| st.bin.is_empty() && !st.items.values().any(|i| i.name == "a.txt" || i.name == "c.txt")));
}

#[test]
fn undo_and_redo_on_a_work_account_go_through_sharepoint() {
    let (fake, a, _) = setup_work();
    names(&a, "/Meus arquivos/Docs");
    let (gone, _) = a.trash_many(&["/Meus arquivos/Docs/a.txt".into(), "/Meus arquivos/Docs/b.txt".into()]);
    let ids: Vec<String> = gone.iter().map(|(id, _)| id.clone()).collect();
    assert!(ids.iter().all(|id| a.can_restore(id)));
    // Desfazer.
    let (back, err) = a.restore_many(&ids);
    assert_eq!((back.len(), err), (2, None));
    assert_eq!(names(&a, "/Meus arquivos/Docs"), ["a.txt", "b.txt"]);
    assert!(fake.with(|st| st.bin.is_empty()));
    // Refazer finds the new bin item, so Desfazer works again.
    for id in &ids {
        a.trash_id(id).unwrap();
        assert!(a.can_restore(id));
    }
    for id in &ids {
        a.restore(id).unwrap();
    }
    assert_eq!(names(&a, "/Meus arquivos/Docs"), ["a.txt", "b.txt"]);
}

#[test]
fn twins_in_the_bin_are_told_apart_and_undo_takes_the_one_it_deleted() {
    let (fake, a, docs) = setup_work();
    let (first, _) = a.trash_many(&["/Meus arquivos/Docs/a.txt".into()]);
    fake.with(|st| st.add(&docs, "a.txt", false, b"segunda"));
    a.catch_up().unwrap();
    let (second, _) = a.trash_many(&["/Meus arquivos/Docs/a.txt".into()]);
    assert_eq!(bin_names(&a), ["a (2).txt", "a.txt"]);
    // Undoing the second delete brings the second file back, not the first.
    a.restore(&second[0].0).unwrap();
    let mut out = Vec::new();
    a.download("/Meus arquivos/Docs/a.txt", &mut out, &mut |_| true).unwrap();
    assert_eq!(out, b"segunda");
    assert!(a.can_restore(&first[0].0));
    // The first is still in the bin under its own name.
    assert_eq!(bin_names(&a), ["a.txt"]);
}

#[test]
fn without_sharepoint_permission_the_account_keeps_working() {
    let (fake, a, _) = setup_work();
    fake.with(|st| st.sp_granted = false);
    let err = a.list("/Lixeira").unwrap_err();
    assert!(err.contains("AllSites.Write") && err.contains("Autorizar no navegador"), "{err}");
    assert!(matches!(a.bin_access(), crate::bin::Access::NeedsConsent(why) if why.contains("AADSTS65001")));
    // Deleting still works; only Desfazer is left out.
    let (gone, err) = a.trash_many(&["/Meus arquivos/Docs/a.txt".into()]);
    assert_eq!((gone.len(), err), (1, None));
    assert!(!a.can_restore(&gone[0].0));
    assert!(a.restore_many(&[gone[0].0.clone()]).1.unwrap().contains("autorize"));
    assert_eq!(names(&a, "/Meus arquivos/Docs"), ["b.txt"]);
    // The grant's sign-in asks SharePoint's host for what the registration lists.
    let login = a.bin_grant().unwrap();
    let url = url::Url::parse(&login.url).unwrap();
    let q = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
    assert_eq!(q("scope").unwrap(), format!("{}/.default offline_access", fake.base));
    assert_eq!(q("login_hint").unwrap(), "ana@exemplo.com");
    // Granted (by the person, or an administrator): the bin opens.
    fake.with(|st| st.sp_granted = true);
    assert_eq!(bin_names(&a), ["a.txt"]);
    assert_eq!(a.bin_access(), crate::bin::Access::Ready);
}

/// Answers a sign-in's loopback as the browser would, with `query` after its state.
fn browser_comes_back(login: &crate::PendingLogin, query: &str) -> std::thread::JoinHandle<()> {
    use std::io::{Read, Write};
    let url = url::Url::parse(&login.url).unwrap();
    let q = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned()).unwrap();
    let port = url::Url::parse(&q("redirect_uri")).unwrap().port().unwrap();
    let target = format!("/?state={}&{query}", q("state"));
    std::thread::spawn(move || {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
    })
}

#[test]
fn authorizing_the_bin_keeps_the_new_refresh_token_and_graph_goes_on() {
    let fake = Fake::start();
    fake.with(|st| {
        st.business = true;
        let docs = st.add(ROOT, "Docs", true, &[]);
        st.add(&docs, "a.txt", false, b"alfa");
    });
    let kept = Arc::new(Mutex::new(Vec::<String>::new()));
    let k = kept.clone();
    let cfg = crate::ClientConfig { client_id: "0b1c2d3e-4f50-6172-8394-a5b6c7d8e9f0".into(), tenant: "common".into() };
    let graph = Graph::new(cfg, fake.ends(), "r0".into(), Box::new(move |t| k.lock().unwrap().push(t.into())));
    let a = Account::start(Meta { id: DRIVE.into(), email: "ana@exemplo.com".into(), name: "Ana".into(), kind: Kind::Business }, graph, None).unwrap();
    assert!(a.list("/Lixeira").unwrap_err().contains("AllSites.Write"));
    // Refused in the browser: what to add, not a failed sign-in.
    let login = a.bin_grant().unwrap();
    let b = browser_comes_back(&login, "error=invalid_client&error_description=AADSTS650057%3A+Invalid+resource");
    let err = a.bin_granted(&login).unwrap_err();
    b.join().unwrap();
    assert!(err.contains("AllSites.Write") && !err.contains("administrador aprove"), "{err}");
    // Granted on Microsoft's page: the code becomes SharePoint's token and a new refresh token.
    fake.with(|st| st.sp_granted = true);
    let login = a.bin_grant().unwrap();
    let b = browser_comes_back(&login, "code=c1");
    a.bin_granted(&login).unwrap();
    b.join().unwrap();
    let newest = kept.lock().unwrap().last().cloned().unwrap();
    assert_eq!(a.graph.refresh_token(), newest);
    assert_eq!(a.bin_access(), crate::bin::Access::Ready);
    // Graph's token is still Graph's: the files list.
    assert_eq!(names(&a, "/Meus arquivos/Docs"), ["a.txt"]);
    let (gone, _) = a.trash_many(&["/Meus arquivos/Docs/a.txt".into()]);
    assert!(a.can_restore(&gone[0].0));
}

#[test]
fn a_sharepoint_permission_that_isnt_enough_asks_for_the_right_one() {
    let (fake, a, _) = setup_work();
    // Both the site collection's bin and the site's answer 403.
    fake.with(|st| {
        st.sp_site_refused = true;
        st.sp_web_refused = true;
    });
    let err = a.list("/Lixeira").unwrap_err();
    assert!(err.contains("AllSites.Write"), "{err}");
    assert!(matches!(a.bin_access(), crate::bin::Access::NeedsConsent(why) if why.contains("negou")));
}

#[test]
fn a_filter_answered_with_any_error_reads_the_newest_first() {
    let (fake, a, _) = setup_work();
    fake.with(|st| st.sp_filter_status = 500);
    let (gone, _) = a.trash_many(&["/Meus arquivos/Docs/a.txt".into()]);
    assert!(a.can_restore(&gone[0].0));
}

#[test]
fn sharepoint_asking_for_a_digest_refusing_the_site_bin_paging_and_no_filter() {
    let (fake, a, docs) = setup_work();
    fake.with(|st| {
        st.sp_digest = true;
        st.sp_page = 1;
        st.sp_no_filter = true;
    });
    let (gone, _) = a.trash_many(&["/Meus arquivos/Docs/a.txt".into(), "/Meus arquivos/Docs/b.txt".into()]);
    assert!(gone.iter().all(|(id, _)| a.can_restore(id)));
    assert_eq!(bin_names(&a), ["a.txt", "b.txt"]);
    let (back, err) = a.bin_restore(&["a.txt".into()]);
    assert_eq!((back.len(), err), (1, None));
    // The site collection's bin refused: the site's, without the second stage.
    fake.with(|st| {
        st.sp_site_refused = true;
        let id = st.add(&docs, "velho.txt", false, b"v");
        st.touch(&id, |i| i.deleted = true);
        let when = crate::fake::now();
        st.put_in_bin(&id, &when);
        st.bin.last_mut().unwrap().stage = 2;
    });
    assert_eq!(bin_names(&a), ["b.txt"]);
    let (back, err) = a.bin_restore(&["b.txt".into()]);
    assert_eq!((back.len(), err), (1, None));
    assert_eq!(names(&a, "/Meus arquivos/Docs"), ["a.txt", "b.txt"]);
}

/// `n` items deleted long ago in Docs' recycle bin, and files `novo 0.txt`… to delete.
fn old_bin(fake: &Fake, docs: &str, n: usize, new: usize) {
    fake.with(|st| {
        for i in 0..n {
            let id = st.add(docs, &format!("velho {i}.txt"), false, b"v");
            st.touch(&id, |it| it.deleted = true);
            st.put_in_bin(&id, "2024-05-06T07:08:09Z");
        }
        for i in 0..new {
            st.add(docs, &format!("novo {i}.txt"), false, b"n");
        }
    });
}

/// Deletes `novo {k}.txt`: whether Desfazer can bring it back, and the SharePoint requests.
fn delete_new(fake: &Fake, a: &Account, k: usize) -> (bool, usize) {
    let before = fake.with(|st| st.sp_requests);
    let (gone, err) = a.trash_many(&[format!("/Meus arquivos/Docs/novo {k}.txt")]);
    assert_eq!((gone.len(), err), (1, None));
    (a.can_restore(&gone[0].0), fake.with(|st| st.sp_requests) - before)
}

#[test]
fn without_a_filter_a_delete_reads_only_the_newest_of_the_bin() {
    let (fake, a, docs) = setup_work();
    old_bin(&fake, &docs, 300, 3);
    fake.with(|st| {
        st.sp_no_filter = true;
        st.sp_page = 50;
    });
    a.list("/Meus arquivos/Docs").unwrap();
    // The first delete asks for the site and finds the filter refused, once.
    assert!(delete_new(&fake, &a, 0).0);
    // From then on one request, not the 300 items in 6 pages.
    assert_eq!(delete_new(&fake, &a, 1), (true, 1));
    // Twins: Desfazer takes the one it deleted.
    fake.with(|st| st.add(&docs, "novo 1.txt", false, b"outro"));
    a.catch_up().unwrap();
    let (second, _) = a.trash_many(&["/Meus arquivos/Docs/novo 1.txt".into()]);
    a.restore(&second[0].0).unwrap();
    let mut out = Vec::new();
    a.download("/Meus arquivos/Docs/novo 1.txt", &mut out, &mut |_| true).unwrap();
    assert_eq!(out, b"outro");
}

#[test]
fn a_bin_that_takes_no_order_is_read_whole_and_that_is_remembered() {
    // Refused.
    let (fake, a, docs) = setup_work();
    old_bin(&fake, &docs, 30, 3);
    fake.with(|st| {
        st.sp_no_filter = true;
        st.sp_no_orderby = true;
        st.sp_page = 10;
    });
    assert!(delete_new(&fake, &a, 0).0);
    assert_eq!(delete_new(&fake, &a, 1), (true, 4), "the whole bin, without asking what it refused again");
    // Taken but not followed (oldest first), the bin smaller than `$top`: seen as not sorted.
    let (fake, a, docs) = setup_work();
    old_bin(&fake, &docs, 5, 3);
    fake.with(|st| {
        st.sp_no_filter = true;
        st.sp_ignore_orderby = true;
    });
    assert!(delete_new(&fake, &a, 0).0);
    assert_eq!(delete_new(&fake, &a, 1), (true, 1));
    // Taken but not followed, the bin bigger than `$top`: the oldest look sorted and the new item
    // isn't among them; the whole bin finds it, and is read from then on.
    let (fake, a, docs) = setup_work();
    old_bin(&fake, &docs, 100, 3);
    fake.with(|st| {
        st.sp_no_filter = true;
        st.sp_ignore_orderby = true;
    });
    assert!(delete_new(&fake, &a, 0).0);
    assert_eq!(delete_new(&fake, &a, 1), (true, 1));
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
fn a_small_upload_reports_progress_and_cancelling_it_makes_no_file() {
    let (fake, a, _, _) = setup(Kind::Personal);
    let body = data(1_000_000);
    let len = body.len() as u64;
    // Progress as the bytes go, never back, also when the request is sent again (throttled; the
    // folder just listed, so the PUT is the request throttled).
    names(&a, "/Meus arquivos/Docs");
    fake.with(|st| st.throttle = 1);
    let mut seen = Vec::new();
    a.upload("/Meus arquivos/Docs/s.bin", &mut &body[..], len, &mut |n| {
        seen.push(n);
        true
    })
    .unwrap();
    assert!(seen.len() > 1 && seen.is_sorted() && *seen.last().unwrap() == len, "{seen:?}");
    assert!(seen.iter().filter(|n| **n == len).count() > 1, "the body went twice");
    assert!(fake.with(|st| st.items.values().any(|i| i.name == "s.bin" && i.data == body)));
    // Cancelled at the start, or at the very last bytes: the file is never made.
    for (name, stop_at) in [("c1.bin", 100_000), ("c2.bin", len)] {
        let err = a.upload(&format!("/Meus arquivos/Docs/{name}"), &mut &body[..], len, &mut |n| n < stop_at).unwrap_err();
        assert_eq!(err, crate::CANCELLED);
        assert!(fake.with(|st| !st.items.values().any(|i| i.name == name)), "{name}");
        assert!(a.stat(&format!("/Meus arquivos/Docs/{name}")).unwrap().is_none());
    }
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
