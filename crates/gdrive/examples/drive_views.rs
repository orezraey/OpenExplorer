//! Tries Drive's views and the trash calls against the first connected account.
//!
//! Lists Compartilhados comigo, Com estrela, Recentes and Lixeira (counts and a few names), then
//! works only on folders it creates itself in My Drive: stars and unstars one, trashes them,
//! lists a trashed folder's contents, restores one, and deletes them for good. It never empties
//! the trash, which would delete the account's own trashed files.
//!
//! `cargo run -p openexplorer-gdrive --example drive_views`

use openexplorer_gdrive::{ClientConfig, Drive, DriveFile, View, store};

fn ids(d: &Drive, view: View) -> Vec<DriveFile> {
    let mut out = Vec::new();
    d.view(view, |page, _| {
        out.extend(page);
        true
    })
    .expect("listar a vista");
    out
}

fn main() {
    let cfg = ClientConfig::load().expect("cliente do Google");
    let account = store::accounts().into_iter().next().expect("uma conta conectada");
    let d = Drive::new(cfg, store::refresh_token(&account.email).expect("token da conta"));

    for view in [View::SharedWithMe, View::Starred, View::Recent, View::Trash] {
        let files = ids(&d, view);
        let names: Vec<&str> = files.iter().take(3).map(|f| f.name.as_str()).collect();
        println!("{view:?}: {} itens, por exemplo {names:?}", files.len());
    }

    let outer = d.create_folder("OpenExplorer - teste lixeira", "root").expect("pasta de teste");
    let inner = d.create_folder("dentro", &outer.id).expect("subpasta");
    let other = d.create_folder("OpenExplorer - teste restaurar", "root").expect("pasta de teste");
    println!("criadas: {} {} {}", outer.id, inner.id, other.id);

    assert!(d.set_starred(&outer.id, true).expect("estrela").starred);
    assert!(ids(&d, View::Starred).iter().any(|f| f.id == outer.id), "a pasta com estrela aparece em Com estrela");
    assert!(!d.set_starred(&outer.id, false).expect("tirar estrela").starred);
    println!("estrela: ok");

    d.trash(&outer.id).expect("lixeira");
    d.trash(&other.id).expect("lixeira");
    let trash = ids(&d, View::Trash);
    assert!(trash.iter().any(|f| f.id == outer.id) && trash.iter().any(|f| f.id == other.id), "as duas pastas estão na Lixeira");
    assert!(!trash.iter().any(|f| f.id == inner.id), "a subpasta só aparece dentro da pasta");
    let mut inside = Vec::new();
    d.list_trashed(&outer.id, |page, _| {
        inside.extend(page);
        true
    })
    .expect("conteúdo da pasta na lixeira");
    assert_eq!(inside.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), [inner.id.as_str()]);
    println!("lixeira: ok ({} itens)", trash.len());

    d.restore(&other.id).expect("restaurar");
    assert!(!ids(&d, View::Trash).iter().any(|f| f.id == other.id), "restaurada, sai da Lixeira");
    println!("restaurar: ok");

    d.trash(&other.id).expect("lixeira de novo");
    d.delete_forever(&other.id).expect("excluir permanentemente");
    d.delete_forever(&outer.id).expect("excluir permanentemente");
    let trash = ids(&d, View::Trash);
    assert!(!trash.iter().any(|f| f.id == outer.id || f.id == other.id), "excluídas para sempre");
    println!("excluir permanentemente: ok");
}
