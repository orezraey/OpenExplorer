//! Against a real Telegram account: ignored unless asked for, and only with `TELEGRAM_TEST=1`.
//!
//! It runs over the session the proof of concept (`poc/telegram`) keeps in the credential store
//! ("OpenExplorer Telegram PoC") and in the channel it made (`poc/telegram/estado-poc.txt`), so it
//! needs no login and writes nothing to the credential store. Everything it creates goes in a
//! folder of its own that it deletes at the end.
//!
//! ```text
//! TELEGRAM_TEST=1 cargo test -p openexplorer-telegram --test real_account -- --ignored --nocapture --test-threads 1
//! ```

use std::sync::Arc;
use std::time::Instant;

use grammers_session::types::{PeerAuth, PeerId, PeerRef};
use openexplorer_telegram::real::{Conn, Real};
use openexplorer_telegram::session::KeyringSession;
use openexplorer_telegram::{Account, Found};

fn poc() -> Option<(i32, Vec<u8>, PeerRef)> {
    if std::env::var("TELEGRAM_TEST").as_deref() != Ok("1") {
        eprintln!("TELEGRAM_TEST=1 não está definido: nada a fazer.");
        return None;
    }
    let service = "OpenExplorer Telegram PoC";
    let api = keyring::Entry::new(service, "api").unwrap().get_password().expect("a sessão da PoC (rode `telegram-poc entrar`)");
    let api_id: i32 = api.split_once(':').unwrap().0.parse().unwrap();
    let session = keyring::Entry::new(service, "sessao").unwrap().get_secret().unwrap();
    let state = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../poc/telegram/estado-poc.txt")).expect("o canal da PoC (rode `telegram-poc canal`)");
    let line = state.lines().find(|l| l.starts_with("canal ")).unwrap();
    let w: Vec<&str> = line.split(' ').collect();
    let peer = PeerRef { id: PeerId::channel(w[1].parse().unwrap()).unwrap(), auth: PeerAuth::from_hash(w[2].parse().unwrap()) };
    Some((api_id, session, peer))
}

fn account(api_id: i32, session: &[u8], peer: PeerRef) -> Account {
    let conn = Conn::start(api_id, Arc::new(KeyringSession::from_bytes(session).unwrap())).unwrap();
    Account::open(1, "teste", Box::new(Real::new(conn, peer, false)), None)
}

fn random(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    let mut x: u64 = 0x9e3779b97f4a7c15 ^ n as u64;
    for v in b.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *v = x as u8;
    }
    b
}

fn get(a: &Account, path: &str) -> Vec<u8> {
    let mut out = Vec::new();
    a.download(path, &mut out, &mut |_| true).unwrap();
    out
}

fn names(a: &Account, path: &str) -> Vec<String> {
    a.list(path).unwrap().into_iter().map(|i| i.name).collect()
}

fn mib_s(bytes: usize, t: Instant) -> String {
    format!("{:.1} MiB/s", bytes as f64 / 1048576.0 / t.elapsed().as_secs_f64())
}

#[test]
#[ignore = "uses a real Telegram account"]
fn the_whole_round_on_a_real_channel() {
    let Some((api_id, session, peer)) = poc() else { return };
    let a = account(api_id, &session, peer);
    let top = format!("/OE teste {}", std::process::id());
    let t = Instant::now();
    a.mkdir(&top).unwrap();
    println!("listagem inicial + pasta: {:.2} s", t.elapsed().as_secs_f64());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| round(&a, &top, api_id, &session, peer)));
    let t = Instant::now();
    a.remove(&top, true).unwrap();
    println!("apagar tudo: {:.2} s", t.elapsed().as_secs_f64());
    assert!(a.stat(&top).unwrap().is_none());
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}

fn round(a: &Account, top: &str, api_id: i32, session: &[u8], peer: PeerRef) {
    a.mkdir(&format!("{top}/sub")).unwrap();

    let big = random(30 * 1024 * 1024 + 123);
    let t = Instant::now();
    a.upload(&format!("{top}/grande.bin"), &mut &big[..], big.len() as u64, Some(1_700_000_000), &mut |_| true).unwrap();
    println!("enviar 30 MiB: {}", mib_s(big.len(), t));

    let mid = random(3 * 1024 * 1024 + 7);
    a.upload(&format!("{top}/sub/medio.bin"), &mut &mid[..], mid.len() as u64, None, &mut |_| true).unwrap();
    a.upload(&format!("{top}/vazio.txt"), &mut std::io::empty(), 0, None, &mut |_| true).unwrap();

    let t = Instant::now();
    let small: Vec<(String, Vec<u8>, Option<i64>)> = (0..25).map(|i| (format!("{top}/sub/p{i:02}.txt"), format!("arquivo {i}").into_bytes(), Some(1_600_000_000 + i))).collect();
    let r = a.upload_small(small);
    assert!(r.iter().all(Result::is_ok), "{r:?}");
    println!("25 pequenos em grupos: {:.1} s", t.elapsed().as_secs_f64());

    let Some(Found::Item(g)) = a.stat(&format!("{top}/grande.bin")).unwrap() else { panic!() };
    assert_eq!((g.size, g.modified), (big.len() as u64, 1_700_000_000));
    assert_eq!(a.list(&format!("{top}/sub")).unwrap().len(), 26);

    let t = Instant::now();
    assert!(get(a, &format!("{top}/grande.bin")) == big);
    println!("baixar 30 MiB: {}", mib_s(big.len(), t));
    assert!(get(a, &format!("{top}/sub/medio.bin")) == mid);
    assert_eq!(get(a, &format!("{top}/sub/p07.txt")), b"arquivo 7");
    assert!(get(a, &format!("{top}/vazio.txt")).is_empty());

    a.rename(&format!("{top}/sub/medio.bin"), &format!("{top}/medio renomeado.bin")).unwrap();
    a.rename(&format!("{top}/sub"), &format!("{top}/pasta")).unwrap();
    a.copy(&format!("{top}/pasta"), &format!("{top}/copia")).unwrap();
    a.copy(&format!("{top}/vazio.txt"), &format!("{top}/copia/vazio2.txt")).unwrap();
    assert_eq!(names(a, top), ["copia", "pasta", "grande.bin", "medio renomeado.bin", "vazio.txt"]);
    assert_eq!(a.list(&format!("{top}/copia")).unwrap().len(), 26);
    assert_eq!(get(a, &format!("{top}/copia/p24.txt")), b"arquivo 24");

    // Another PC on the same account: what it does reaches this one through the change log.
    let other = account(api_id, session, peer);
    other.mkdir(&format!("{top}/do outro PC")).unwrap();
    other.remove(&format!("{top}/copia/p00.txt"), false).unwrap();
    std::thread::sleep(std::time::Duration::from_secs(4));
    let t = Instant::now();
    assert!(names(a, top).contains(&"do outro PC".to_string()));
    assert_eq!(a.list(&format!("{top}/copia")).unwrap().len(), 25);
    println!("mudanças de outro PC: {:.2} s", t.elapsed().as_secs_f64());

    // Cancelar during an upload.
    let mut seen = 0;
    let e = a
        .upload(&format!("{top}/cancelado.bin"), &mut &big[..], big.len() as u64, None, &mut |n| {
            seen = n;
            n < 4 * 1024 * 1024
        })
        .unwrap_err();
    assert_eq!(e, openexplorer_telegram::CANCELLED);
    assert!(a.stat(&format!("{top}/cancelado.bin")).unwrap().is_none());
    println!("cancelado depois de {:.1} MiB", seen as f64 / 1048576.0);
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

/// A WAV of a second of a tone, 8 kHz, 8 bits.
fn wav() -> Vec<u8> {
    let samples: Vec<u8> = (0..8000u32).map(|i| (128.0 + 100.0 * (i as f64 * 0.3).sin()) as u8).collect();
    let mut w = Vec::new();
    w.extend(b"RIFF");
    w.extend((36 + samples.len() as u32).to_le_bytes());
    w.extend(b"WAVEfmt ");
    w.extend(16u32.to_le_bytes());
    w.extend(1u16.to_le_bytes());
    w.extend(1u16.to_le_bytes());
    w.extend(8000u32.to_le_bytes());
    w.extend(8000u32.to_le_bytes());
    w.extend(1u16.to_le_bytes());
    w.extend(8u16.to_le_bytes());
    w.extend(b"data");
    w.extend((samples.len() as u32).to_le_bytes());
    w.extend(samples);
    w
}

#[test]
#[ignore = "uses a real Telegram account"]
fn names_and_media_come_back_the_same() {
    let Some((api_id, session, peer)) = poc() else { return };
    let a = account(api_id, &session, peer);
    let top = format!("/OE nomes {}", std::process::id());
    a.mkdir(&top).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let png = hex("89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000a49444154789c63000100000500010d0a2db40000000049454e44ae426082");
        let gif = hex("47494638396101000100800000000000ffffff21f90401000000002c00000000010001000002024401003b");
        let long = format!("{}.txt", "x".repeat(251));
        let files: Vec<(String, Vec<u8>)> = vec![
            (" começa com espaço.txt".into(), b"espaco".to_vec()),
            ("emoji \u{1F600} ação.png".into(), png),
            ("animado.gif".into(), gif),
            ("som.wav".into(), wav()),
            (long, b"longo".to_vec()),
            ("nome #oe1 f 0 5.txt".into(), b"tag no nome".to_vec()),
        ];
        // One by one, then the same in one group.
        for (name, bytes) in &files {
            a.upload(&format!("{top}/{name}"), &mut &bytes[..], bytes.len() as u64, None, &mut |_| true).unwrap();
        }
        a.mkdir(&format!("{top}/grupo")).unwrap();
        let r = a.upload_small(files.iter().map(|(n, b)| (format!("{top}/grupo/{n}"), b.clone(), None)).collect());
        assert!(r.iter().all(Result::is_ok), "{r:?}");
        for dir in [top.clone(), format!("{top}/grupo")] {
            let mut shown = names(&a, &dir);
            shown.retain(|n| n != "grupo");
            let mut want: Vec<String> = files.iter().map(|(n, _)| n.clone()).collect();
            shown.sort();
            want.sort();
            assert_eq!(shown, want, "nomes em {dir}");
            for (name, bytes) in &files {
                let got = get(&a, &format!("{dir}/{name}"));
                assert!(got == *bytes, "{dir}/{name}: {} bytes, esperava {}", got.len(), bytes.len());
                let Some(Found::Item(i)) = a.stat(&format!("{dir}/{name}")).unwrap() else { panic!() };
                assert_eq!(i.size, bytes.len() as u64, "{name}");
            }
        }
        println!("nomes e mídias iguais, um a um e em grupo");
    }));
    a.remove(&top, true).unwrap();
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}

#[test]
#[ignore = "uses a real Telegram account"]
fn many_files_from_several_panes_arrive_once() {
    use openexplorer_telegram::real::{WAITED_SECS, WAITS};
    use std::sync::atomic::Ordering;
    let Some((api_id, session, peer)) = poc() else { return };
    let a = account(api_id, &session, peer);
    let top = format!("/OE muitos {}", std::process::id());
    a.mkdir(&top).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let t = Instant::now();
        std::thread::scope(|s| {
            // Two panes sending groups, four sending one file at a time.
            for pane in 0..2 {
                let (a, top) = (&a, &top);
                s.spawn(move || {
                    let files = (0..100).map(|i| (format!("{top}/g{pane}-{i:03}.txt"), format!("grupo {pane} {i}").into_bytes(), None)).collect();
                    let r = a.upload_small(files);
                    assert!(r.iter().all(Result::is_ok), "{r:?}");
                });
            }
            for pane in 0..4 {
                let (a, top) = (&a, &top);
                s.spawn(move || {
                    for i in 0..15 {
                        let bytes = format!("um {pane} {i}").into_bytes();
                        a.upload(&format!("{top}/u{pane}-{i:02}.txt"), &mut &bytes[..], bytes.len() as u64, None, &mut |_| true).unwrap();
                    }
                });
            }
        });
        let took = t.elapsed().as_secs_f64();
        let list = a.list(&top).unwrap();
        assert_eq!(list.len(), 260);
        assert!(list.iter().all(|i| !i.name.contains(" (")), "nome repetido: um arquivo chegou duas vezes");
        assert_eq!(get(&a, &format!("{top}/g1-099.txt")), b"grupo 1 99");
        assert_eq!(get(&a, &format!("{top}/u3-14.txt")), b"um 3 14");
        println!("260 arquivos de 6 painéis em {took:.1} s ({:.1}/s); {} esperas, {} s pedidos", 260.0 / took, WAITS.load(Ordering::Relaxed), WAITED_SECS.load(Ordering::Relaxed));
    }));
    let t = Instant::now();
    a.remove(&top, true).unwrap();
    println!("apagar 261: {:.1} s", t.elapsed().as_secs_f64());
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}

#[test]
#[ignore = "uses a real Telegram account"]
fn reach_finds_the_kept_session_and_the_marked_channel() {
    use grammers_tl_types as tl;
    use openexplorer_telegram::records::CHANNEL_MARK;
    use openexplorer_telegram::{StoredAccount, cache, login, session};
    let Some((api_id, bytes, peer)) = poc() else { return };
    let conn = Conn::start(api_id, Arc::new(KeyringSession::from_bytes(&bytes).unwrap())).unwrap();
    let me = conn.rt.block_on(conn.client.get_me()).unwrap();
    let user_id = me.id().bare_id_unchecked();

    // The PoC's channel takes the mark, as one an Open Explorer made would have.
    let full = tl::functions::channels::GetFullChannel { channel: peer.into() };
    let tl::enums::messages::ChatFull::Full(f) = conn.rt.block_on(conn.client.invoke(&full)).unwrap();
    let tl::enums::ChatFull::ChannelFull(f) = f.full_chat else { panic!() };
    if !f.about.contains(CHANNEL_MARK) {
        let about = format!("{} {CHANNEL_MARK}", f.about);
        conn.rt.block_on(conn.client.invoke(&tl::functions::messages::EditChatAbout { peer: peer.into(), about })).unwrap();
    }
    // Scanning the conversations is paced hard: only when asked (`TELEGRAM_FIND=1`).
    if std::env::var("TELEGRAM_FIND").as_deref() == Ok("1") {
        let t = Instant::now();
        let found = conn.rt.block_on(login::find_channel(&conn)).unwrap();
        assert_eq!(found.map(|p| p.id), Some(peer.id), "o canal marcado é encontrado");
        println!("canal encontrado pela marca em {:.1} s", t.elapsed().as_secs_f64());
    }

    // The session in the credential store under the product's name, as a login leaves it.
    let dir = std::env::temp_dir().join(format!("oe-telegram-real-{}", std::process::id()));
    cache::set_dir(Some(dir.clone()));
    keyring::Entry::new("OpenExplorer Telegram", &user_id.to_string()).unwrap().set_secret(&bytes).unwrap();
    let api = keyring::Entry::new("OpenExplorer Telegram PoC", "api").unwrap().get_password().unwrap();
    let (_, hash) = api.split_once(':').unwrap();
    session::set_api(user_id, api_id, hash).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let stored = StoredAccount { user_id, name: me.full_name(), channel_id: peer.id.bare_id_unchecked(), channel_hash: peer.auth.hash(), premium: false };
        // As the app reaches it: by its user id, from the accounts' list.
        let list = openexplorer_telegram::Store::at(dir.join("telegram-accounts.json"));
        list.add(stored.clone()).unwrap();
        openexplorer_telegram::set_store(list);
        let a = openexplorer_telegram::reach_id(user_id).unwrap();
        assert!(Arc::ptr_eq(&a, &openexplorer_telegram::reach(&stored).unwrap()));
        let n = a.list("/").unwrap().len();
        openexplorer_telegram::save_caches();
        let file = cache::file_of(user_id).unwrap();
        assert!(file.exists(), "a árvore ficou no disco");
        println!("reach com a sessão guardada: {n} itens no topo; árvore guardada ({} bytes)", std::fs::metadata(&file).unwrap().len());
    }));
    // Only the product's entries go: the PoC's session stays (`antigo` still needs it).
    session::forget(user_id);
    let _ = std::fs::remove_dir_all(dir);
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}

#[test]
#[ignore = "uses a real Telegram account"]
fn a_file_in_parts_works_as_one_on_a_real_channel() {
    let Some((api_id, session, peer)) = poc() else { return };
    let a = account(api_id, &session, peer);
    // Parts of 10 MiB stand in for Telegram's 2000 MiB, so the test sends 25 MiB, not gigabytes.
    a.set_part_size(10 * 1024 * 1024);
    let top = format!("/OE partes {}", std::process::id());
    a.mkdir(&top).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let big = random(25 * 1024 * 1024 + 99);
        let t = Instant::now();
        a.upload(&format!("{top}/grande.bin"), &mut &big[..], big.len() as u64, Some(1_700_000_000), &mut |_| true).unwrap();
        println!("enviar 25 MiB em 3 partes: {}", mib_s(big.len(), t));
        let Some(Found::Item(i)) = a.stat(&format!("{top}/grande.bin")).unwrap() else { panic!() };
        assert_eq!((i.size, i.parts.len(), i.incomplete, i.modified), (big.len() as u64, 3, false, 1_700_000_000));
        assert_eq!(names(&a, &top), ["grande.bin"]);
        let t = Instant::now();
        assert!(get(&a, &format!("{top}/grande.bin")) == big, "the parts come back as the file");
        println!("baixar as 3 partes: {}", mib_s(big.len(), t));
        a.mkdir(&format!("{top}/sub")).unwrap();
        a.rename(&format!("{top}/grande.bin"), &format!("{top}/sub/renomeado.bin")).unwrap();
        a.copy(&format!("{top}/sub/renomeado.bin"), &format!("{top}/copia.bin")).unwrap();
        // Another PC sees one file of three parts, as this one does.
        let other = account(api_id, &session, peer);
        let Some(Found::Item(c)) = other.stat(&format!("{top}/copia.bin")).unwrap() else { panic!("the copy, seen from another PC") };
        assert_eq!((c.size, c.parts.len(), c.incomplete), (big.len() as u64, 3, false));
        assert!(get(&other, &format!("{top}/sub/renomeado.bin")) == big);
    }));
    a.remove(&top, true).unwrap();
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}

/// A PNG of `w`×`h` pixels, a gradient, written with stored (uncompressed) deflate blocks.
fn png(w: u32, h: u32) -> Vec<u8> {
    fn crc(data: &[u8]) -> u32 {
        let mut c = 0xffff_ffffu32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend(data);
        out.extend(&body);
        out.extend(crc(&body).to_be_bytes());
    }
    let mut raw = Vec::new();
    for y in 0..h {
        raw.push(0);
        for x in 0..w {
            raw.extend([(x * 255 / w) as u8, (y * 255 / h) as u8, 128]);
        }
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &v in &raw {
        a = (a + v as u32) % 65521;
        b = (b + a) % 65521;
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, block) in blocks.iter().enumerate() {
        z.push((i + 1 == blocks.len()) as u8);
        z.extend((block.len() as u16).to_le_bytes());
        z.extend((!(block.len() as u16)).to_le_bytes());
        z.extend(*block);
    }
    z.extend(((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[test]
#[ignore = "uses a real Telegram account"]
fn a_photo_sent_from_a_phone_shows_and_downloads() {
    use grammers_client::message::InputMessage;
    let Some((api_id, session, peer)) = poc() else { return };
    // As a phone sends it: a photo, not a file, with no record.
    let conn = Conn::start(api_id, Arc::new(KeyringSession::from_bytes(&session).unwrap())).unwrap();
    let image = png(320, 240);
    let up = conn.rt.block_on(conn.client.upload_stream(&mut std::io::Cursor::new(image.clone()), image.len(), "foto.png".into())).unwrap();
    let sent = conn.rt.block_on(conn.client.send_message(peer, InputMessage::new().text("do celular").photo(up))).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let a = account(api_id, &session, peer);
        let name = format!("foto-{}.jpg", sent.id());
        let Some(Found::Item(i)) = a.stat(&format!("/{name}")).unwrap() else { panic!("{name} isn't listed: {:?}", names(&a, "/")) };
        assert!(i.size > 0);
        let bytes = get(&a, &format!("/{name}"));
        assert_eq!(bytes.len() as u64, i.size, "the size listed is the size downloaded");
        assert_eq!(&bytes[..2], [0xff, 0xd8], "Telegram keeps photos as JPEG");
        // Renamed, it keeps being the photo.
        a.rename(&format!("/{name}"), "/praia.jpg").unwrap();
        assert!(get(&a, "/praia.jpg") == bytes);
        println!("foto do celular: {} bytes, listada, baixada e renomeada", bytes.len());
    }));
    conn.rt.block_on(conn.client.delete_messages(peer, &[sent.id()])).unwrap();
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}

#[test]
#[ignore = "uses a real Telegram account"]
fn download_speed_of_a_big_file() {
    let Some((api_id, session, peer)) = poc() else { return };
    let a = account(api_id, &session, peer);
    let path = format!("/OE velocidade {}.bin", std::process::id());
    let big = random(100 * 1024 * 1024);
    let t = Instant::now();
    a.upload(&path, &mut &big[..], big.len() as u64, None, &mut |_| true).unwrap();
    println!("enviar 100 MiB: {}", mib_s(big.len(), t));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for round in 1..=2 {
            let t = Instant::now();
            let mut out = Vec::with_capacity(big.len());
            a.download(&path, &mut out, &mut |_| true).unwrap();
            println!("baixar 100 MiB ({round}ª vez): {}", mib_s(out.len(), t));
            assert!(out == big);
        }
    }));
    a.remove(&path, false).unwrap();
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}
