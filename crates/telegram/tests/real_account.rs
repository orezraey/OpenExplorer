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

/// `session` with the key of its own DC only, as a new login has: `TELEGRAM_HOME_ONLY=1`.
fn home_only(session: &[u8]) -> Vec<u8> {
    const ENTRY: usize = 4 + 4 + 2 + 16 + 2 + 256;
    let home = i32::from_le_bytes(session[1..5].try_into().unwrap());
    let n = session[5] as usize;
    let entries: Vec<&[u8]> = (0..n).map(|i| &session[6 + i * ENTRY..6 + (i + 1) * ENTRY]).filter(|e| i32::from_le_bytes(e[..4].try_into().unwrap()) == home).collect();
    let mut out = session[..5].to_vec();
    out.push(entries.len() as u8);
    for e in entries {
        out.extend_from_slice(e);
    }
    out
}

fn account(api_id: i32, session: &[u8], peer: PeerRef) -> Account {
    let session = if std::env::var("TELEGRAM_HOME_ONLY").as_deref() == Ok("1") { home_only(session) } else { session.to_vec() };
    let conn = Conn::start(api_id, Arc::new(KeyringSession::from_bytes(&session).unwrap())).unwrap();
    // `TELEGRAM_PREMIUM=1`: tuned as a Premium account is.
    let premium = std::env::var("TELEGRAM_PREMIUM").as_deref() == Ok("1");
    Account::open(1, "teste", Box::new(Real::new(conn, peer, premium)), None)
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

/// Speed by connections per DC, the variants interleaved round after round so the network's
/// swings fall on all of them alike. `TELEGRAM_ROUNDS` rounds (3 by default).
#[test]
#[ignore = "uses a real Telegram account"]
fn speed_by_connections() {
    use openexplorer_telegram::real::{PREMIUM_WAITS, WAITS};
    use std::sync::atomic::Ordering;
    let Some((api_id, session, peer)) = poc() else { return };
    let rounds: usize = std::env::var("TELEGRAM_ROUNDS").ok().and_then(|r| r.parse().ok()).unwrap_or(3);
    // `TELEGRAM_VARIANTS=4x16,8x16`: connections × parts in flight.
    let variants: Vec<(usize, usize)> = std::env::var("TELEGRAM_VARIANTS")
        .ok()
        .map(|v| v.split(',').filter_map(|p| p.split_once('x')).map(|(c, n)| (c.parse().unwrap(), n.parse().unwrap())).collect())
        .unwrap_or(vec![(1, 12), (2, 12), (4, 16), (8, 32)]);
    let a = account(api_id, &session, peer);
    let path = format!("/OE velocidade {}.bin", std::process::id());
    let big = random(100 * 1024 * 1024);
    let up = random(40 * 1024 * 1024);
    a.upload(&path, &mut &big[..], big.len() as u64, None, &mut |_| true).unwrap();
    let mut down: Vec<Vec<f64>> = vec![Vec::new(); variants.len()];
    let mut sent: Vec<Vec<f64>> = vec![Vec::new(); variants.len()];
    let (waits, premium) = (WAITS.load(Ordering::Relaxed), PREMIUM_WAITS.load(Ordering::Relaxed));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for round in 1..=rounds {
            for (i, &(connections, parts)) in variants.iter().enumerate() {
                a.tune(connections, parts);
                let t = Instant::now();
                let mut out = Vec::with_capacity(big.len());
                a.download(&path, &mut out, &mut |_| true).unwrap();
                let speed = big.len() as f64 / 1048576.0 / t.elapsed().as_secs_f64();
                assert!(out == big);
                down[i].push(speed);
                let target = format!("/OE envio {} {connections}.bin", std::process::id());
                let t = Instant::now();
                a.upload(&target, &mut &up[..], up.len() as u64, None, &mut |_| true).unwrap();
                let speed_up = up.len() as f64 / 1048576.0 / t.elapsed().as_secs_f64();
                a.remove(&target, false).unwrap();
                sent[i].push(speed_up);
                let dropped = openexplorer_telegram::real::DROPPED_CALLS.load(Ordering::Relaxed);
                println!("rodada {round}, {connections} conexões / {parts} partes: baixar {speed:.1} MiB/s, enviar {speed_up:.1} MiB/s; abertas por DC: {:?}; esperas até aqui {}, pedidos refeitos {dropped}", a.connections_open(), WAITS.load(Ordering::Relaxed) - waits);
            }
        }
    }));
    a.remove(&path, false).unwrap();
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        (v[v.len() / 2], v[0], v[v.len() - 1])
    };
    println!("\nmediana (mín–máx) em MiB/s, {rounds} rodadas:");
    for (i, &(connections, parts)) in variants.iter().enumerate() {
        if down[i].is_empty() {
            continue;
        }
        let (d, dmin, dmax) = median(&mut down[i]);
        let (u, umin, umax) = median(&mut sent[i]);
        println!("  {connections} conexões / {parts} partes: baixar {d:.1} ({dmin:.1}–{dmax:.1}), enviar {u:.1} ({umin:.1}–{umax:.1})");
    }
    println!("esperas pedidas: {} (FLOOD_PREMIUM_WAIT: {})", WAITS.load(Ordering::Relaxed) - waits, PREMIUM_WAITS.load(Ordering::Relaxed) - premium);
    println!("conexões extras descartadas: {:?}", openexplorer_telegram::real::DROPPED_LANES.lock().unwrap());
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}

/// The chats, read-only: the first page of them, Saved Messages' months, its newest month and
/// the smallest file there, downloaded whole. Prints counts and times only, never names.
/// `TELEGRAM_CHATS_ALL=1` also reads every chat, as the app does over its asks, and times it.
#[test]
#[ignore = "uses a real Telegram account"]
fn the_chats_list_and_a_file_of_saved_messages_downloads() {
    use openexplorer_telegram::chats::folder_name;
    let Some((api_id, bytes, peer)) = poc() else { return };
    let a = account(api_id, &bytes, peer);
    let top = format!("/{}", folder_name());
    assert!(names(&a, "/").contains(&folder_name().to_string()));

    let t = Instant::now();
    let chats = a.list(&top).unwrap();
    println!("primeira página: {} conversas em {:.2} s", chats.len(), t.elapsed().as_secs_f64());
    let saved = &chats[0].name;

    let t = Instant::now();
    let months = names(&a, &format!("{top}/{saved}"));
    println!("Mensagens Salvas: {} meses em {:.2} s", months.len(), t.elapsed().as_secs_f64());
    let Some(month) = months.first() else { return };
    let t = Instant::now();
    let files = a.list(&format!("{top}/{saved}/{month}")).unwrap();
    println!("mês mais recente: {} arquivos em {:.2} s", files.len(), t.elapsed().as_secs_f64());
    if let Some(f) = files.iter().filter(|f| f.size > 0).min_by_key(|f| f.size) {
        let t = Instant::now();
        let got = get(&a, &format!("{top}/{saved}/{month}/{}", f.name));
        assert_eq!(got.len() as u64, f.size, "o arquivo veio inteiro");
        println!("menor arquivo ({} bytes) baixado em {:.2} s", f.size, t.elapsed().as_secs_f64());
    }
    // Read-only.
    assert!(a.mkdir(&format!("{top}/{saved}/x")).is_err());

    if std::env::var("TELEGRAM_CHATS_ALL").as_deref() == Ok("1") {
        let t = Instant::now();
        let mut asks = 0;
        while t.elapsed().as_secs() < 900 {
            asks += 1;
            a.refresh().unwrap();
            let n = a.kept(&top).map_or(0, |v| v.len());
            println!("  pedido {asks}: {n} conversas, {:.0} s", t.elapsed().as_secs_f64());
            if !a.refresh().unwrap() && asks > 1 && a.kept(&top).map_or(0, |v| v.len()) == n {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        println!("todas as conversas: {} em {:.0} s", a.kept(&top).map_or(0, |v| v.len()), t.elapsed().as_secs_f64());
    }
}

/// A file from chats other than Saved Messages: the first few chats with files, the smallest of
/// each one's newest month, downloaded whole (or why not). Prints counts, sizes and times only.
#[test]
#[ignore = "uses a real Telegram account"]
fn files_download_from_other_chats() {
    use openexplorer_telegram::chats::folder_name;
    let Some((api_id, bytes, peer)) = poc() else { return };
    let a = account(api_id, &bytes, peer);
    let top = format!("/{}", folder_name());
    let chats = a.list(&top).unwrap();
    let mut done = 0;
    for (i, c) in chats.iter().enumerate().skip(1).take(40) {
        let months = match a.list(&format!("{top}/{}", c.name)) {
            Ok(m) => m,
            Err(e) => {
                println!("conversa {i}: meses: erro: {e}");
                continue;
            }
        };
        let Some(month) = months.first() else { continue };
        let files = a.list(&format!("{top}/{}/{}", c.name, month.name)).unwrap();
        let Some(f) = files.iter().filter(|f| f.size > 0 && f.size < 20 << 20).min_by_key(|f| f.size) else { continue };
        let t = Instant::now();
        let mut out = Vec::new();
        match a.download(&format!("{top}/{}/{}/{}", c.name, month.name, f.name), &mut out, &mut |_| true) {
            Ok(()) => {
                assert_eq!(out.len() as u64, f.size);
                println!("conversa {i}: {} meses, {} arquivos no mais recente, {} bytes em {:.2} s", months.len(), files.len(), f.size, t.elapsed().as_secs_f64());
            }
            Err(e) => println!("conversa {i}: download: erro: {e}"),
        }
        done += 1;
        if done == 5 {
            break;
        }
    }
    assert!(done > 0, "alguma conversa com arquivos");
}

/// Whether a chat's months each hold files: every month of the first chats with any, listed.
#[test]
#[ignore = "uses a real Telegram account"]
fn months_of_a_chat_hold_files() {
    use openexplorer_telegram::chats::folder_name;
    // Some 850 requests over five minutes: only when asked.
    if std::env::var("TELEGRAM_MONTHS").as_deref() != Ok("1") {
        eprintln!("TELEGRAM_MONTHS=1 não está definido: nada a fazer.");
        return;
    }
    let Some((api_id, bytes, peer)) = poc() else { return };
    let a = account(api_id, &bytes, peer);
    let top = format!("/{}", folder_name());
    let chats = a.list(&top).unwrap();
    for (i, c) in chats.iter().enumerate().take(4) {
        let months = a.list(&format!("{top}/{}", c.name)).unwrap();
        let mut empty = 0;
        let mut files = 0;
        for m in &months {
            let n = a.list(&format!("{top}/{}/{}", c.name, m.name)).unwrap().len();
            files += n;
            if n == 0 {
                empty += 1;
            }
        }
        println!("conversa {i}: {} meses ({} a {}), {empty} vazios, {files} arquivos", months.len(), months.last().map_or("", |m| m.name.as_str()), months.first().map_or("", |m| m.name.as_str()));
    }
}



/// The main list of chats and the archive, paged as the account pages them: how many each has
/// and how many of the archive's the main list lacks. Prints counts only.
#[test]
#[ignore = "uses a real Telegram account"]
fn the_archive_adds_the_chats_the_main_list_lacks() {
    use openexplorer_telegram::backend::{Backend, Paged};
    use openexplorer_telegram::chats::Cursor;
    use std::collections::HashSet;
    let Some((api_id, bytes, peer)) = poc() else { return };
    let conn = Conn::start(api_id, Arc::new(KeyringSession::from_bytes(&bytes).unwrap())).unwrap();
    let real = Real::new(conn, peer, false);
    let all = |start: Option<Cursor>| {
        let mut seen = HashSet::new();
        let mut after = start;
        let mut pages = Vec::new();
        for _ in 0..100 {
            match real.chats(after.as_ref(), true).unwrap() {
                Paged::Page(p) => {
                    pages.push(p.chats.len());
                    seen.extend(p.chats.iter().map(|c| (c.at.kind as u8, c.at.id)));
                    match p.next {
                        Some(n) => after = Some(n),
                        None => break,
                    }
                }
                Paged::WaitUntil(_) => unreachable!("patient"),
            }
        }
        (seen, pages)
    };
    let (main, mp) = all(None);
    let (archive, ap) = all(Some(Cursor::archive()));
    let lacking = archive.iter().filter(|c| !main.contains(*c)).count();
    println!("principal: {} ({} páginas {:?}); arquivo: {} ({} páginas {:?}); do arquivo fora da principal: {lacking}", main.len(), mp.len(), mp, archive.len(), ap.len(), ap);
}

/// Files of several chats downloaded at once, as the queue does: four at a time. Prints sizes,
/// times and errors only. `TELEGRAM_PARALLEL=n` changes how many at once.
#[test]
#[ignore = "uses a real Telegram account"]
fn files_of_several_chats_download_at_once() {
    use openexplorer_telegram::chats::folder_name;
    let Some((api_id, bytes, peer)) = poc() else { return };
    let a = Arc::new(account(api_id, &bytes, peer));
    let top = format!("/{}", folder_name());
    let chats = a.list(&top).unwrap();
    let mut files = Vec::new();
    for c in chats.iter().take(40) {
        let Ok(months) = a.list(&format!("{top}/{}", c.name)) else { continue };
        let Some(m) = months.first() else { continue };
        let Ok(items) = a.list(&format!("{top}/{}/{}", c.name, m.name)) else { continue };
        for f in items.iter().filter(|f| f.size > 0 && f.size < std::env::var("TELEGRAM_MAX_MB").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(8) << 20).take(3) {
            files.push((format!("{top}/{}/{}/{}", c.name, m.name, f.name), f.size));
        }
        if files.len() >= 24 {
            break;
        }
    }
    let at_once: usize = std::env::var("TELEGRAM_PARALLEL").ok().and_then(|v| v.parse().ok()).unwrap_or(4);
    let queue = Arc::new(std::sync::Mutex::new(files.into_iter().enumerate().collect::<Vec<_>>()));
    let t = Instant::now();
    let workers: Vec<_> = (0..at_once)
        .map(|_| {
            let (a, queue) = (a.clone(), queue.clone());
            std::thread::spawn(move || {
                let mut out = Vec::new();
                loop {
                    let Some((i, (path, size))) = queue.lock().unwrap().pop() else { break };
                    let mut got = Vec::new();
                    let r = a.download(&path, &mut got, &mut |_| true);
                    out.push((i, size, r.map(|()| got.len() as u64)));
                }
                out
            })
        })
        .collect();
    let mut results: Vec<_> = workers.into_iter().flat_map(|w| w.join().unwrap()).collect();
    results.sort_by_key(|r| r.0);
    let failed = results.iter().filter(|r| r.2.is_err()).count();
    for (i, size, r) in &results {
        match r {
            Ok(n) => println!("arquivo {i}: {size} bytes, {}", if n == size { "inteiro" } else { "INCOMPLETO" }),
            Err(e) => println!("arquivo {i}: {size} bytes, ERRO {e}"),
        }
    }
    println!("{} arquivos, {failed} com erro, {at_once} de cada vez, {:.1} s", results.len(), t.elapsed().as_secs_f64());
    assert_eq!(failed, 0);
}

/// Files of several chats downloaded at once while the account's connections are ended again and
/// again, as Telegram may end one: every file still comes whole. Prints sizes and counts only.
#[test]
#[ignore = "uses a real Telegram account"]
fn downloads_survive_connections_that_end() {
    use openexplorer_telegram::chats::folder_name;
    use openexplorer_telegram::real::DROPPED_CALLS;
    let Some((api_id, bytes, peer)) = poc() else { return };
    let conn = Conn::start(api_id, Arc::new(KeyringSession::from_bytes(&bytes).unwrap())).unwrap();
    let breaker = conn.breaker();
    let a = Arc::new(Account::open(1, "teste", Box::new(Real::new(conn, peer, true)), None));
    let top = format!("/{}", folder_name());
    let chats = a.list(&top).unwrap();
    let mut files = Vec::new();
    for c in chats.iter().take(40) {
        let Ok(months) = a.list(&format!("{top}/{}", c.name)) else { continue };
        for m in months.iter().take(3) {
            let Ok(items) = a.list(&format!("{top}/{}/{}", c.name, m.name)) else { continue };
            for f in items.iter().filter(|f| f.size > 2 << 20 && f.size < 60 << 20).take(2) {
                files.push((format!("{top}/{}/{}/{}", c.name, m.name, f.name), f.size));
            }
        }
        if files.len() >= 8 {
            break;
        }
    }
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let chaos = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut n = 0;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(std::env::var("TELEGRAM_CHAOS_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(3000)));
                for dc in 1..=5 {
                    breaker(dc);
                }
                n += 1;
            }
            n
        })
    };
    let t = Instant::now();
    let workers: Vec<_> = files
        .into_iter()
        .map(|(path, size)| {
            let a = a.clone();
            std::thread::spawn(move || {
                let mut got = Vec::new();
                let r = a.download(&path, &mut got, &mut |_| true);
                (size, r.map(|()| got.len() as u64))
            })
        })
        .collect();
    let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let breaks = chaos.join().unwrap();
    let failed: Vec<String> = results.iter().filter_map(|(s, r)| match r {
        Ok(n) if n == s => None,
        Ok(n) => Some(format!("{n} de {s} bytes")),
        Err(e) => Some(e.clone()),
    }).collect();
    println!("{} arquivos, {} MiB, {breaks} quedas forçadas, {} pedidos refeitos, {:.1} s; falhas: {failed:?}", results.len(), results.iter().map(|r| r.0).sum::<u64>() >> 20, DROPPED_CALLS.load(std::sync::atomic::Ordering::Relaxed), t.elapsed().as_secs_f64());
    assert!(failed.is_empty());
}

/// How fast one big file of a chat comes down, and over how many connections per DC: the
/// biggest of the first chats' recent months (up to 400 MiB), into nothing. Prints sizes, speeds
/// and DCs only. With `TELEGRAM_PREMIUM=1`, tuned as Premium.
#[test]
#[ignore = "uses a real Telegram account"]
fn speed_of_a_big_chat_file() {
    use openexplorer_telegram::chats::folder_name;
    let Some((api_id, bytes, peer)) = poc() else { return };
    let a = account(api_id, &bytes, peer);
    let top = format!("/{}", folder_name());
    let chats = a.list(&top).unwrap();
    let mut best: Option<(String, u64)> = None;
    for c in chats.iter().take(30) {
        let Ok(months) = a.list(&format!("{top}/{}", c.name)) else { continue };
        for m in months.iter().take(4) {
            let Ok(items) = a.list(&format!("{top}/{}/{}", c.name, m.name)) else { continue };
            for f in items.iter().filter(|f| f.size < 400 << 20) {
                if best.as_ref().is_none_or(|b| f.size > b.1) {
                    best = Some((format!("{top}/{}/{}/{}", c.name, m.name, f.name), f.size));
                }
            }
        }
    }
    let (path, size) = best.expect("a file");
    let _ = std::fs::write(std::env::temp_dir().join("oe-telegram-big-file.txt"), format!("{size}
{path}"));
    big_file_rounds(&a, &path, size);
}

fn big_file_rounds(a: &Account, path: &str, size: u64) {
    use openexplorer_telegram::real::{DROPPED_LANES, PREMIUM_WAITS, WAITED_SECS, WAITS};
    use std::sync::atomic::Ordering::Relaxed;
    for round in 0..2 {
        let t = Instant::now();
        let mut sink = std::io::sink();
        a.download(path, &mut sink, &mut |_| true).unwrap();
        println!("rodada {round}: {:.1} MiB em {:.1} s = {}; conexões por DC {:?}", size as f64 / 1048576.0, t.elapsed().as_secs_f64(), mib_s(size as usize, t), a.connections_open());
        println!("  esperas {} ({} s), premium {}, conexões perdidas {:?}", WAITS.load(Relaxed), WAITED_SECS.load(Relaxed), PREMIUM_WAITS.load(Relaxed), DROPPED_LANES.lock().unwrap());
    }
}

/// [`speed_of_a_big_chat_file`] again on the file it found, with no search first.
#[test]
#[ignore = "uses a real Telegram account"]
fn speed_of_the_big_chat_file_again() {
    let Some((api_id, bytes, peer)) = poc() else { return };
    let Ok(kept) = std::fs::read_to_string(std::env::temp_dir().join("oe-telegram-big-file.txt")) else { return };
    let (size, path) = kept.split_once('\n').unwrap();
    let a = account(api_id, &bytes, peer);
    let top = path.split('/').take(4).collect::<Vec<_>>().join("/");
    a.list(&top).unwrap();
    let home = KeyringSession::from_bytes(&bytes).unwrap().home();
    println!("DC da conta: {home}");
    // `TELEGRAM_TUNE=8x32`: connections per DC and parts in flight.
    if let Some((c, p)) = std::env::var("TELEGRAM_TUNE").ok().and_then(|v| v.split_once('x').map(|(c, p)| (c.parse().unwrap(), p.parse().unwrap()))) {
        a.tune(c, p);
    }
    big_file_rounds(&a, path, size.parse().unwrap());
}

/// The big chat file over 1, 4 and 8 connections, interleaved, three rounds, a pause between:
/// the median of each. Prints speeds only.
#[test]
#[ignore = "uses a real Telegram account"]
fn speed_of_the_big_chat_file_by_connections() {
    let Some((api_id, bytes, peer)) = poc() else { return };
    let Ok(kept) = std::fs::read_to_string(std::env::temp_dir().join("oe-telegram-big-file.txt")) else { return };
    let (size, path) = kept.split_once('\n').unwrap();
    let size: u64 = size.parse().unwrap();
    let variants: Vec<(usize, usize)> = std::env::var("TELEGRAM_VARIANTS").ok().map_or_else(|| vec![(1, 16), (4, 16), (8, 32)], |v| v.split(',').map(|p| { let (c, n) = p.split_once('x').unwrap(); (c.parse().unwrap(), n.parse().unwrap()) }).collect());
    let mut speeds: Vec<Vec<f64>> = vec![Vec::new(); variants.len()];
    for round in 0..3 {
        for (v, &(c, p)) in variants.iter().enumerate() {
            std::thread::sleep(std::time::Duration::from_secs(15));
            // A fresh account each time: no connection left over from the variant before.
            let a = account(api_id, &bytes, peer);
            a.list(&path.split('/').take(4).collect::<Vec<_>>().join("/")).unwrap();
            a.tune(c, p);
            let t = Instant::now();
            a.download(path, &mut std::io::sink(), &mut |_| true).unwrap();
            let mibs = size as f64 / 1048576.0 / t.elapsed().as_secs_f64();
            speeds[v].push(mibs);
            println!("rodada {round}, {c}x{p}: {mibs:.1} MiB/s, conexões {:?}", a.connections_open());
        }
    }
    for (v, &(c, p)) in variants.iter().enumerate() {
        let mut s = speeds[v].clone();
        s.sort_by(f64::total_cmp);
        println!("{c}x{p}: mediana {:.1} MiB/s ({:?})", s[s.len() / 2], s.iter().map(|x| format!("{x:.1}")).collect::<Vec<_>>());
    }
}
