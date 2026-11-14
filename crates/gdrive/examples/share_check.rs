//! Checks the quota and sharing calls against a real account, touching only a file it makes.
//!
//! Prints the first connected account's storage, uploads a small test file to its My Drive,
//! lists who has access, turns link access on and off (checking each time), and moves the file
//! to the Drive trash, whatever happened. Nobody is invited.
//!
//! `cargo run -p openexplorer-gdrive --example share_check`

use openexplorer_gdrive::{ClientConfig, Drive, store};

fn main() {
    let cfg = ClientConfig::load().expect("cliente do Google");
    let account = store::accounts().into_iter().next().expect("uma conta conectada");
    let drive = Drive::new(cfg, store::refresh_token(&account.email).expect("token da conta"));

    let q = drive.quota().expect("cota");
    println!("conta {}: usado {} de {:?} (Drive {}, lixeira {}), livre {:?}", account.email, q.usage, q.limit, q.in_drive, q.in_trash, q.free());

    let path = std::env::temp_dir().join("OpenExplorer - teste de compartilhamento.txt");
    std::fs::write(&path, "Arquivo de teste do Open Explorer; pode apagar.\n").unwrap();
    let file = drive.upload(&path, "root", "OpenExplorer - teste de compartilhamento.txt", &mut |_| true).expect("enviar o arquivo de teste");
    println!("arquivo de teste {}", file.id);

    let steps = || -> Result<(), String> {
        let show = |when: &str| -> Result<openexplorer_gdrive::Sharing, String> {
            let s = drive.sharing(&file.id)?;
            println!("{when}: pode compartilhar {}, link {:?}", s.can_share, s.link);
            for p in &s.permissions {
                println!("  {} — {} ({:?})", p.who(), p.role.label(), p.access);
            }
            println!("  resumo: {}", s.summary(&account.email));
            Ok(s)
        };
        assert!(show("no início")?.anyone().is_none(), "um arquivo novo é privado");
        drive.set_link_access(&file.id, true)?;
        assert!(show("link ativado")?.anyone().is_some(), "o link deveria estar aberto");
        drive.set_link_access(&file.id, false)?;
        assert!(show("link desativado")?.anyone().is_none(), "o link deveria estar fechado");
        // A malformed address is refused before any call: nobody can be invited by mistake.
        match drive.share_with(&file.id, "nao-existe", openexplorer_gdrive::Role::Reader, false) {
            Err(e) => println!("e-mail inválido recusado: {e}"),
            Ok(()) => return Err("um e-mail inválido foi aceito".into()),
        }
        Ok(())
    };
    let outcome = steps();
    drive.trash(&file.id).expect("mover o arquivo de teste para a lixeira");
    println!("arquivo de teste na lixeira");
    let _ = std::fs::remove_file(&path);
    outcome.expect("compartilhamento");
    println!("ok");
}
