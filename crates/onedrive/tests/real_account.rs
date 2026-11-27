//! Tests against a real OneDrive account, ignored by default: they need an app registration and
//! a refresh token, read from the environment.
//!
//! - `ONEDRIVE_TEST_CLIENT_ID` (and `ONEDRIVE_TEST_TENANT`, `common` if unset): the app
//!   registration (see `openexplorer_onedrive::registration_help`).
//! - `ONEDRIVE_TEST_REFRESH_TOKEN`: a refresh token for it. `sign_in_prints_a_refresh_token`
//!   gets one: it prints the address to open in the browser, waits, and prints the token.
//! - `ONEDRIVE_TEST_WRITE=1` lets `writing_round_trip`, `folder_sizes` and `work_bin_round_trip`
//!   write, inside a folder of their own at the drive's top that they delete at the end (and,
//!   on a work account, delete for good from the recycle bin). On a personal account
//!   `folder_sizes` saw the totals right at once in 2 runs of 5, and 0 for 85 s, 10 min, 14 min
//!   in the others (and over 20 min in a check outside the app's code); the delta feed never
//!   brought the folders again once the totals were right.
//!
//! A work or school account's recycle bin needs SharePoint's permission (AllSites.Write) in the
//! registration and granted: `work_bin_grant` opens the sign-in that grants it. Then
//! `work_bin_lists` (read only) and `work_bin_round_trip` check what the fake can't: the shape of
//! `DirName` the undo match relies on, and that an item keeps its Graph id once restored.
//!
//! `cargo test -p openexplorer-onedrive --test real_account -- --ignored --nocapture --test-threads=1`

use openexplorer_onedrive::{Account, BinAccess, ClientConfig, Kind, account, graph, oauth};

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

/// What Graph says about folder sizes, printed: whether a folder's `size` counts everything
/// under it, whether the delta feed brings the folders above a changed file with their new size,
/// and where a shortcut to a shared folder keeps its size.
#[test]
#[ignore]
fn folder_sizes() {
    use serde_json::Value;
    if std::env::var("ONEDRIVE_TEST_WRITE").as_deref() != Ok("1") {
        eprintln!("ONEDRIVE_TEST_WRITE=1 para escrever");
        return;
    }
    let a = open();
    let refresh = std::env::var("ONEDRIVE_TEST_REFRESH_TOKEN").unwrap();
    let g = graph::Graph::new(client(), graph::Endpoints::default(), refresh, Box::new(|_| {}));
    println!("conta {:?}", a.meta.kind);
    let size = |v: &Value| v.get("size").and_then(Value::as_u64);
    let at = |path: &str| path.split('/').map(graph::segment).collect::<Vec<_>>().join("/");
    let show = |what: &str| {
        let top = g.get(&format!("/me/drive/root:/{}?$select=id,name,size,folder", at(what)), "ler").unwrap();
        println!("  {what}: size {:?}, childCount {:?}", size(&top), top.pointer("/folder/childCount"));
        size(&top)
    };

    // What an earlier run left behind (it stopped before cleaning up).
    for it in a.list("/Meus arquivos").unwrap() {
        if it.is_dir && it.name.strip_prefix("Open Explorer tamanho ").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) {
            println!("apagando o que sobrou: {}", it.name);
            a.remove(&format!("/Meus arquivos/{}", it.name)).unwrap();
        }
    }

    // Shortcuts to shared folders at the top: where their size is.
    let top = g.get("/me/drive/root/children?$top=999", "listar").unwrap();
    for it in top["value"].as_array().unwrap().iter().filter(|i| i.get("remoteItem").is_some()) {
        println!("atalho {:?}: size {:?}, remoteItem.size {:?}, remoteItem.folder {:?}", it["name"], size(it), it.pointer("/remoteItem/size"), it.pointer("/remoteItem/folder"));
    }

    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let name = format!("Open Explorer tamanho {stamp}");
    let dir = format!("/Meus arquivos/{name}");
    a.mkdir(&dir).unwrap();
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        a.mkdir(&format!("{dir}/Sub")).unwrap();
        a.mkdir(&format!("{dir}/Sub/Fundo")).unwrap();
        a.upload(&format!("{dir}/raiz.bin"), &mut &[1u8; 300][..], 300, &mut |_| true).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(5));
        println!("antes (300 bytes na pasta, nada nas subpastas):");
        show(&name);
        show(&format!("{name}/Sub"));

        // A delta position from now, then a file two folders down.
        let v = g.get("/me/drive/root/delta?token=latest", "delta").unwrap();
        let mut link = v["@odata.deltaLink"].as_str().unwrap().to_string();
        a.upload(&format!("{dir}/Sub/Fundo/fundo.bin"), &mut &[2u8; 1000][..], 1000, &mut |_| true).unwrap();
        let sent = std::time::Instant::now();

        // Graph works the totals out later: the folders asked directly, and what each delta
        // read brings since the one before (does the new total come as a change of its own?).
        let (mut direct_ok, mut delta_ok) = (false, false);
        while sent.elapsed() < std::time::Duration::from_secs(20 * 60) && !(direct_ok && delta_ok) {
            let secs = sent.elapsed().as_secs();
            let top = show(&name);
            let sub = show(&format!("{name}/Sub"));
            let root = g.get("/me/drive/root?$select=size", "ler").unwrap();
            println!("+{secs} s: {name} {top:?}, Sub {sub:?}, raiz {:?}", root["size"]);
            direct_ok |= top == Some(1300);
            loop {
                let d = g.get(&link, "delta").unwrap();
                for it in d["value"].as_array().unwrap() {
                    println!("  delta +{secs} s: {:?} size {:?} pasta {}", it["name"], size(it), it.get("folder").is_some());
                    delta_ok |= it["name"].as_str() == Some(name.as_str()) && size(it) == Some(1300);
                }
                if let Some(n) = d.get("@odata.nextLink").and_then(Value::as_str) {
                    link = n.to_string();
                } else {
                    link = d["@odata.deltaLink"].as_str().unwrap().to_string();
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
        println!("fim: direto {direct_ok}, delta {delta_ok}, {} s", sent.elapsed().as_secs());
        // What the account makes of it: the delta feed taken in, the totals as listed.
        a.catch_up().unwrap();
        let listed = a.list("/Meus arquivos").unwrap().into_iter().find(|i| i.name == name).map(|i| i.size);
        let stat = a.stat(&dir).unwrap().map(|i| i.size);
        println!("conta: listada {listed:?}, stat {stat:?}");
        if delta_ok {
            assert_eq!((listed, stat), (Some(Some(1300)), Some(Some(1300))));
        }
    }));
    a.remove(&dir).unwrap();
    if let Err(e) = ran {
        std::panic::resume_unwind(e);
    }
}

/// A work account, or the test says why it doesn't run.
fn work() -> Option<Account> {
    let a = open();
    if a.meta.kind != Kind::Business {
        eprintln!("a conta é pessoal: a Lixeira só existe em contas corporativas ou de estudante");
        return None;
    }
    Some(a)
}

#[test]
#[ignore]
fn work_bin_grant() {
    let Some(a) = work() else { return };
    let login = a.bin_grant().unwrap();
    println!("Abra no navegador:\n{}", login.url);
    a.bin_granted(&login).unwrap();
    println!("SharePoint autorizado; {} itens na lixeira", a.bin_shown().len());
}

#[test]
#[ignore]
fn work_bin_lists() {
    let Some(a) = work() else { return };
    match a.bin_items() {
        Ok(items) => {
            println!("{} itens na lixeira", items.len());
            for b in items.iter().take(10) {
                println!("{:?} | DirName {:?} → {:?} | {} | 2º estágio: {}", b.name, b.dir, b.origin, b.deleted, b.second_stage);
            }
        }
        Err(e) => {
            assert!(matches!(a.bin_access(), BinAccess::NeedsConsent(_)), "{e}");
            println!("SharePoint não autorizado:\n{e}");
        }
    }
}

#[test]
#[ignore]
fn work_bin_round_trip() {
    if std::env::var("ONEDRIVE_TEST_WRITE").as_deref() != Ok("1") {
        eprintln!("ONEDRIVE_TEST_WRITE=1 para escrever");
        return;
    }
    let Some(a) = work() else { return };
    if let Err(e) = a.bin_items() {
        panic!("a Lixeira não abre (rode work_bin_grant antes):\n{e}");
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let dir = format!("/Meus arquivos/Open Explorer lixeira {stamp}");
    a.mkdir(&dir).unwrap();
    a.mkdir(&format!("{dir}/Sub pasta")).unwrap();
    for name in ["um #1.txt", "dois.txt"] {
        a.upload(&format!("{dir}/Sub pasta/{name}"), &mut &b"conteudo"[..], 8, &mut |_| true).unwrap();
    }
    // Excluir: Graph deletes, the recycle bin items are found by name, folder and time.
    let (gone, err) = a.trash_many(&[format!("{dir}/Sub pasta/um #1.txt"), format!("{dir}/Sub pasta/dois.txt")]);
    assert_eq!((gone.len(), err), (2, None));
    for (id, name) in &gone {
        assert!(a.can_restore(id), "“{name}” não foi achado na lixeira: confira o DirName em work_bin_lists");
    }
    // Desfazer: both back.
    let ids: Vec<String> = gone.iter().map(|(id, _)| id.clone()).collect();
    let (back, err) = a.restore_many(&ids);
    assert_eq!((back.len(), err), (2, None));
    assert!(a.stat(&format!("{dir}/Sub pasta/um #1.txt")).unwrap().is_some());
    // Refazer by the same Graph id: it must have survived the restore.
    for id in &ids {
        a.trash_id(id).unwrap_or_else(|e| panic!("o id do Graph mudou depois de restaurar? {e}"));
        assert!(a.can_restore(id));
    }
    // From the Lixeira: listed with where they came from, restored by the name shown.
    let shown = a.bin_items().unwrap();
    let mine: Vec<_> = shown.iter().filter(|b| b.origin.ends_with("Sub pasta") && b.dir.contains(&format!("lixeira {stamp}"))).collect();
    assert_eq!(mine.len(), 2, "{shown:#?}");
    let (r, err) = a.bin_restore(&[mine[0].name.clone()]);
    assert_eq!((r.len(), err), (1, None));
    // Cleaning up: the folder to the bin, then deleted for good from it.
    a.trash_many(std::slice::from_ref(&dir));
    let leftover: Vec<String> = a.bin_items().unwrap().into_iter().filter(|b| b.dir.contains(&format!("lixeira {stamp}")) || b.leaf.ends_with(&format!("lixeira {stamp}"))).map(|b| b.name).collect();
    let (purged, err) = a.bin_purge(&leftover);
    assert_eq!((purged.len(), err), (leftover.len(), None));
}

