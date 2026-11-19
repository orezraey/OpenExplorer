//! Tests against a real OneDrive account, ignored by default: they need an app registration and
//! a refresh token, read from the environment.
//!
//! - `ONEDRIVE_TEST_CLIENT_ID` (and `ONEDRIVE_TEST_TENANT`, `common` if unset): the app
//!   registration (see `openexplorer_onedrive::registration_help`).
//! - `ONEDRIVE_TEST_REFRESH_TOKEN`: a refresh token for it. `sign_in_prints_a_refresh_token`
//!   gets one: it prints the address to open in the browser, waits, and prints the token.
//! - `ONEDRIVE_TEST_WRITE=1` lets `writing_round_trip` write, inside a folder of its own at the
//!   drive's top that it deletes at the end.
//!
//! `cargo test -p openexplorer-onedrive --test real_account -- --ignored --nocapture --test-threads=1`

use openexplorer_onedrive::{Account, ClientConfig, Kind, account, graph, oauth};

fn client() -> ClientConfig {
    let id = std::env::var("ONEDRIVE_TEST_CLIENT_ID").expect("ONEDRIVE_TEST_CLIENT_ID");
    let tenant = std::env::var("ONEDRIVE_TEST_TENANT").unwrap_or_else(|_| "common".into());
    ClientConfig { client_id: id, tenant }
}

fn open() -> Account {
    let refresh = std::env::var("ONEDRIVE_TEST_REFRESH_TOKEN").expect("ONEDRIVE_TEST_REFRESH_TOKEN");
    account::open_with_refresh(client(), &refresh, Box::new(|_| {})).expect("the account opens")
}

#[test]
#[ignore]
fn sign_in_prints_a_refresh_token() {
    let login = oauth::begin(&client(), &graph::Endpoints::default()).unwrap();
    println!("Abra no navegador:\n{}", login.url);
    let tokens = login.finish().unwrap();
    println!("ONEDRIVE_TEST_REFRESH_TOKEN={}", tokens.refresh_token.expect("offline_access"));
}

#[test]
#[ignore]
fn reading_lists_the_top_and_reads_the_quota() {
    let a = open();
    println!("{} ({:?}), drive {}", a.meta.email, a.meta.kind, a.meta.id);
    let top = a.list("/Meus arquivos").unwrap();
    println!("{} itens no topo", top.len());
    let q = a.quota().unwrap();
    assert!(q.total > 0 && q.used <= q.total);
    // Nothing changed in between: the delta feed says so.
    assert!(!a.catch_up().unwrap().all);
    for it in top.iter().filter(|i| !i.is_dir).take(1) {
        let mut out = Vec::new();
        a.download(&format!("/Meus arquivos/{}", it.name), &mut out, &mut |_| true).unwrap();
        assert_eq!(Some(out.len() as u64), it.size);
    }
}

#[test]
#[ignore]
fn writing_round_trip() {
    if std::env::var("ONEDRIVE_TEST_WRITE").as_deref() != Ok("1") {
        eprintln!("ONEDRIVE_TEST_WRITE=1 para escrever");
        return;
    }
    let a = open();
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let dir = format!("/Meus arquivos/Open Explorer teste {stamp}");
    a.mkdir(&dir).unwrap();
    let small: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
    a.upload(&format!("{dir}/pequeno #1.bin"), &mut &small[..], small.len() as u64, &mut |_| true).unwrap();
    // A session upload in two parts; the QuickXorHash is checked against Graph's.
    let big: Vec<u8> = (0..12_000_000u32).map(|i| (i * 31 + i / 977) as u8).collect();
    a.upload(&format!("{dir}/grande.bin"), &mut &big[..], big.len() as u64, &mut |_| true).unwrap();
    let mut back = Vec::new();
    a.download(&format!("{dir}/grande.bin"), &mut back, &mut |_| true).unwrap();
    assert!(back == big);
    a.mkdir(&format!("{dir}/Sub")).unwrap();
    a.rename(&format!("{dir}/pequeno #1.bin"), &format!("{dir}/Sub/renomeado.bin")).unwrap();
    a.copy(&format!("{dir}/Sub"), &format!("{dir}/Sub 2")).unwrap();
    let mut names: Vec<String> = a.list(&format!("{dir}/Sub 2")).unwrap().into_iter().map(|i| i.name).collect();
    names.sort();
    assert_eq!(names, ["renomeado.bin"]);
    let link = a.create_link(&format!("{dir}/grande.bin")).unwrap();
    println!("link: {link}");
    assert!(a.link_of(&format!("{dir}/grande.bin")).unwrap().is_some());
    assert!(a.remove_link(&format!("{dir}/grande.bin")).unwrap() >= 1);
    let (gone, err) = a.trash_many(&[format!("{dir}/Sub 2"), format!("{dir}/grande.bin")]);
    assert_eq!((gone.len(), err), (2, None));
    if a.meta.kind == Kind::Personal {
        let ids: Vec<String> = gone.iter().map(|(id, _)| id.clone()).collect();
        let (back, err) = a.restore_many(&ids);
        assert_eq!((back.len(), err), (2, None));
        assert!(a.stat(&format!("{dir}/grande.bin")).unwrap().is_some());
    }
    a.remove(&dir).unwrap();
    // Seen from the delta feed as well.
    a.catch_up().unwrap();
    assert!(a.stat(&dir).unwrap().is_none());
}
