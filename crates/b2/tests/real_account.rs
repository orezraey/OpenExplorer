//! Against a real B2 account, never run by default: they need a key.
//!
//! ```text
//! set B2_TEST_KEY_ID=…  set B2_TEST_KEY=…  [set B2_TEST_BUCKET=nome]
//! cargo test -p openexplorer-b2 --test real_account -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The writing test also needs `B2_TEST_WRITE=1` and stays inside a folder of its own,
//! `openexplorer-teste-<number>/` in the bucket, deleted (every version) at its end; with
//! `B2_TEST_LARGE=1` it also sends a 70 MiB file in parts. `B2_TEST_MODE` sets B2's
//! `X-Bz-Test-Mode` header (`fail_some_uploads`, `expire_some_account_authorization_tokens`) to
//! see the retries against B2 itself.

use openexplorer_b2::api::Api;
use openexplorer_b2::{Account, Uploads};

fn account() -> Option<Account> {
    let (Ok(id), Ok(key)) = (std::env::var("B2_TEST_KEY_ID"), std::env::var("B2_TEST_KEY")) else {
        eprintln!("B2_TEST_KEY_ID / B2_TEST_KEY not set: skipped");
        return None;
    };
    let mut api = Api::new(&id, &key);
    api.set_test_mode(std::env::var("B2_TEST_MODE").ok());
    Some(Account::new(api, None))
}

fn bucket(a: &Account) -> String {
    std::env::var("B2_TEST_BUCKET").unwrap_or_else(|_| a.buckets_fresh().unwrap().first().expect("the key reaches no bucket").name.clone())
}

#[test]
#[ignore]
fn lists_the_buckets_and_a_bucket_top() {
    let Some(a) = account() else { return };
    let auth = a.api().auth().unwrap();
    println!("conta {}, recommendedPartSize {}, mínimo {}", auth.account_id, auth.recommended_part, auth.minimum_part);
    println!("permissões: {:?}, buckets: {:?}, prefixo: {:?}", auth.allowed.capabilities, auth.allowed.buckets, auth.allowed.name_prefix);
    println!("rótulo: {}", a.label().unwrap());
    for b in a.buckets_fresh().unwrap() {
        println!("bucket {} ({}), versões escondidas: {:?}", b.name, b.id, b.hidden_kept(""));
    }
    let b = bucket(&a);
    let top = a.list_fresh(&format!("/{b}")).unwrap();
    println!("/{b}: {} itens", top.len());
    for i in top.iter().take(20) {
        println!("  {}{} {:?}", i.name, if i.is_dir { "/" } else { "" }, i.size);
    }
    // Read again: as kept. And what the app's own readings make of it.
    assert_eq!(a.reread(&format!("/{b}")).unwrap(), openexplorer_b2::Fresh::Same);
    assert_eq!(a.poll(&format!("/{b}")), openexplorer_b2::Polled::Same);
    println!("leituras próprias nesta hora e neste dia: {:?}", a.polls_spent());
}

#[test]
#[ignore]
fn writes_inside_a_folder_of_its_own() {
    if std::env::var("B2_TEST_WRITE").as_deref() != Ok("1") {
        eprintln!("B2_TEST_WRITE=1 not set: skipped");
        return;
    }
    let Some(a) = account() else { return };
    let b = bucket(&a);
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let dir = format!("/{b}/openexplorer-teste-{stamp}");
    let mut up = Uploads::default();
    let mut run = || -> Result<(), String> {
        a.mkdir(&mut up, &dir)?;
        assert!(a.list_fresh(&dir)?.is_empty(), "a new folder is empty (its marker hidden)");
        let text = "olá, B2 — relatório (1).txt".as_bytes();
        let file = format!("{dir}/relatório final (1).txt");
        a.upload(&mut up, &file, &mut &text[..], text.len() as u64, &mut |_| true)?;
        let mut back = Vec::new();
        a.download(&file, &mut back, &mut |_| true)?;
        assert_eq!(back, text);
        assert!(a.stat(&file)?.is_some_and(|i| i.size == Some(text.len() as u64)));
        // Renamed: a copy on B2's side, the old name retired.
        let renamed = format!("{dir}/renomeado.txt");
        a.rename(&file, &renamed)?;
        assert!(a.stat(&file)?.is_none() && a.stat(&renamed)?.is_some());
        // Copied, a folder with it.
        a.mkdir(&mut up, &format!("{dir}/sub"))?;
        a.copy(&renamed, &format!("{dir}/sub/copia.txt"))?;
        a.copy(&format!("{dir}/sub"), &format!("{dir}/sub2"))?;
        let names: Vec<String> = a.list_fresh(&format!("{dir}/sub2"))?.into_iter().map(|i| i.name).collect();
        assert_eq!(names, ["copia.txt"]);
        // A change made elsewhere (another connection of the same key) shows on the next
        // reading the app makes on its own.
        let Some(elsewhere) = account() else { unreachable!() };
        a.list_fresh(&dir)?;
        elsewhere.upload(&mut Uploads::default(), &format!("{dir}/de-fora.txt"), &mut &b"x"[..], 1, &mut |_| true)?;
        assert!(matches!(a.poll(&dir), openexplorer_b2::Polled::Changed(items) if items.iter().any(|i| i.name == "de-fora.txt")));
        // Deleted (hidden), then brought back.
        let since = a.hide(&renamed, false)?;
        assert!(a.stat(&renamed)?.is_none());
        assert_eq!(a.unhide(&renamed, false, since)?, 1);
        assert!(a.stat(&renamed)?.is_some());
        if std::env::var("B2_TEST_LARGE").as_deref() == Ok("1") {
            let big: Vec<u8> = (0..70u32 << 20).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
            let path = format!("{dir}/grande.bin");
            let started = std::time::Instant::now();
            a.upload(&mut up, &path, &mut &big[..], big.len() as u64, &mut |_| true)?;
            println!("70 MiB enviados em partes em {:.1} s", started.elapsed().as_secs_f64());
            let mut back = Vec::new();
            a.download(&path, &mut back, &mut |_| true)?;
            assert!(back == big, "the large file came back whole");
            // One that stops midway shows among the unfinished uploads, and is cancelled.
            struct Stops<'a>(&'a [u8], usize);
            impl std::io::Read for Stops<'_> {
                fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                    if self.1 >= 40 << 20 {
                        return Err(std::io::Error::other("parou"));
                    }
                    let n = buf.len().min(self.0.len() - self.1).min((40 << 20) - self.1);
                    buf[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
                    self.1 += n;
                    Ok(n)
                }
            }
            let stopped = format!("{dir}/parou.bin");
            assert!(a.upload(&mut up, &stopped, &mut Stops(&big, 0), big.len() as u64, &mut |_| true).is_err());
            let name = stopped.trim_start_matches(&format!("/{b}/")).to_string();
            let mine: Vec<_> = a.unfinished_uploads(&b)?.into_iter().filter(|u| u.name == name).collect();
            println!("inacabado: {mine:?}");
            assert_eq!(mine.len(), 1);
            assert!(mine[0].sent > 0 && mine[0].size == Some(big.len() as u64));
            assert_eq!(a.cancel_unfinished(&[mine[0].id.clone()])?, 1);
            assert!(a.unfinished_uploads(&b)?.iter().all(|u| u.name != name));
        }
        Ok(())
    };
    let result = run();
    // Every version of everything the test made goes, whatever happened.
    let gone = a.purge(&dir, true);
    println!("limpeza: {gone:?}");
    result.unwrap();
}
