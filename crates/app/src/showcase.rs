//! The README's screenshots: headless scenes with made-up accounts and files, so nothing of the
//! machine they run on shows. Run them with an empty profile (`APPDATA` and `LOCALAPPDATA` set
//! to an empty folder) so no saved account joins them, and `OE_SHOT_LANG=en` for English. Each
//! scene is taken in English and Portuguese, light and dark, into `docs/screenshots`:
//!
//! `openexplorer --scene readme-main --snap docs/screenshots/main-en-dark.png --theme dark
//! --size 1600x900 --scale 1 --settle`

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use openexplorer_gdrive::Quota;
use openexplorer_gdrive::store::StoredAccount;
use openexplorer_i18n::t;

use crate::model::{DriveOrigin, DriveSearch, Entry, Location, Space};
use crate::settings::{Language, PaneLayout};
use crate::transfers::{self, End, Op};
use crate::{State, conflict, model};

/// The language the screenshots are taken in: `OE_SHOT_LANG=en` for English, else Portuguese.
fn language(s: &mut State) {
    let english = std::env::var("OE_SHOT_LANG").is_ok_and(|v| v.eq_ignore_ascii_case("en"));
    s.settings.language = if english { Language::English } else { Language::Portuguese };
    s.settings.language.apply();
    if let Some((_, name)) = &mut s.places.desktop {
        *name = t("Área de Trabalho", "Desktop").into();
    }
    s.local_drives = vec![(PathBuf::from(r"C:\"), model::DriveKind::Fixed), (PathBuf::from(r"D:\"), model::DriveKind::Fixed)];
}

/// Both trees open down to the folder each pane shows, without reading anything.
fn trees(s: &mut State) {
    s.layout.tree[0] = 220.0;
    s.layout.tree[1] = 220.0;
    crate::tree::stage(s, 0, &[t("Estúdio", "Studio"), t("Escritório", "Office")]);
    crate::tree::stage(s, 1, &[t("Estúdio", "Studio"), t("Escritório", "Office")]);
}

fn ago(hours: u64) -> Option<SystemTime> {
    Some(SystemTime::now() - Duration::from_secs(hours * 3600))
}

fn entry(name: &str, size: Option<u64>, hours: u64, mime: Option<&str>) -> Entry {
    Entry {
        id: name.into(),
        name: name.into(),
        is_dir: size.is_none() && mime.is_none(),
        size,
        modified: ago(hours),
        target: None,
        mime: mime.map(str::to_string),
        md5: None,
        starred: false,
        drive_origin: None,
    }
}

fn folder(name: &str, hours: u64) -> Entry {
    entry(name, None, hours, None)
}

fn file(name: &str, size: u64, hours: u64) -> Entry {
    entry(name, Some(size), hours, None)
}

/// Two Google accounts and one of each other cloud and a server, in the tree.
fn accounts(s: &mut State) {
    s.accounts = vec![
        StoredAccount { email: "voce@gmail.com".into(), name: t("Você", "You").into(), client_id: String::new() },
        StoredAccount { email: "voce@empresa.com".into(), name: t("Você", "You").into(), client_id: String::new() },
    ];
    s.quotas.insert("voce@gmail.com".into(), Some(Quota { limit: Some(2_199_023_255_552), usage: 412_316_860_416, in_drive: 401_000_000_000, in_trash: 1_200_000_000 }));
    let server = |id: &str, name: &str, protocol, brand, host: &str, port, user: &str| openexplorer_remote::Server {
        id: id.into(),
        name: name.into(),
        protocol,
        brand,
        host: host.into(),
        port,
        user: user.into(),
        root: "/".into(),
        key: None,
        host_key: None,
        cert: None,
        connections: 4,
        implicit: false,
    };
    let pcloud = server("pcloud", "pCloud (voce@exemplo.com)", openexplorer_remote::Protocol::Dav, Some(openexplorer_remote::Brand::PCloud), "webdav.pcloud.com", 443, "voce@exemplo.com");
    let nas = server("nas", t("NAS de casa", "Home NAS"), openexplorer_remote::Protocol::Sftp, None, "192.168.0.20", 22, "voce");
    s.servers.list = vec![
        openexplorer_remote::Server::onedrive("b!exemplo", "voce@outlook.com"),
        openexplorer_remote::Server::dropbox("voce@exemplo.com"),
        openexplorer_remote::Server::mega("fotos@exemplo.com"),
        openexplorer_remote::Server::b2("0051a2b3c4d5e6f0000000001", "B2 · backup"),
        openexplorer_remote::Server::telegram(123456789, t("Você", "You")),
        pcloud,
        nas,
    ];
}

/// A project folder on the computer.
fn local_files() -> Vec<Entry> {
    vec![
        folder(t("Fotos da obra", "Site photos"), 30),
        folder(t("Plantas", "Floor plans"), 52),
        folder(t("Contratos", "Contracts"), 400),
        file(t("Apresentação para o cliente.pdf", "Client presentation.pdf"), 18_431_220, 5),
        file(t("Cronograma 2026.csv", "Schedule 2026.csv"), 84_112, 26),
        file(t("Memorial descritivo.docx", "Specification.docx"), 241_870, 74),
        file(t("Orçamento revisado.pdf", "Revised budget.pdf"), 1_204_330, 3),
        file("fachada_render_final.png", 6_812_004, 2),
        file(t("Levantamento topográfico.zip", "Land survey.zip"), 12_550_912, 120),
        file(t("Vídeo do terreno.mp4", "Site walkthrough.mp4"), 486_201_344, 300),
        file(t("notas da reunião.txt", "meeting notes.txt"), 3_214, 1),
    ]
}

/// A shared project folder on Google Drive.
fn drive_files() -> Vec<Entry> {
    vec![
        folder(t("Entregas", "Deliverables"), 20),
        folder(t("Referências", "References"), 200),
        folder(t("Fotos da obra", "Site photos"), 1),
        file(t("Controle de custos.csv", "Cost tracking.csv"), 96_412, 4),
        file(t("Ata de reunião.docx", "Meeting minutes.docx"), 38_120, 28),
        file(t("Apresentação para o cliente.pdf", "Client presentation.pdf"), 18_431_220, 6),
        file(t("Orçamento revisado.pdf", "Revised budget.pdf"), 1_204_330, 3),
        file("fachada_render_final.png", 6_812_004, 2),
        file(t("Contrato assinado.pdf", "Signed contract.pdf"), 2_408_660, 500),
    ]
}

/// The free space the computer's pane shows.
const FREE: u64 = 684_500_000_000;

fn project_dir() -> PathBuf {
    PathBuf::from(t(r"D:\Projetos\Casa Jardins", r"D:\Projects\Garden House"))
}

fn drive_project() -> Location {
    Location::Drive {
        email: "voce@gmail.com".into(),
        space: Space::MyDrive,
        path: vec![("p".into(), t("Projetos", "Projects").into()), ("c".into(), t("Casa Jardins", "Garden House").into())],
    }
}

/// The computer and a Google Drive folder side by side, with copies running in the queue.
pub fn main_scene(s: &mut State) {
    language(s);
    accounts(s);
    s.settings.pane_layout = PaneLayout::Two;
    s.panes[0].go(Location::Local(project_dir()));
    s.panes[0].set_entries(Ok(local_files()));
    // A made-up free space, not the one of the disk the screenshot is taken on.
    s.panes[0].free = Some(FREE);
    s.panes[0].selected = [t("Fotos da obra", "Site photos").to_string(), "fachada_render_final.png".to_string()].into();
    s.panes[1].go(drive_project());
    s.panes[1].set_entries(Ok(drive_files()));
    s.active = 0;
    trees(s);
    let dir = project_dir();
    let drive = drive_project();
    let rows = [
        (Op::Upload, End::local(&dir.join(t("Vídeo do terreno.mp4", "Site walkthrough.mp4"))), End::drive(&drive, None)),
        (Op::Upload, End::local(&dir.join(t("Fotos da obra", "Site photos"))), End::drive(&drive, None)),
        (Op::Download, End::drive(&drive, Some(t("Entregas", "Deliverables"))), End::local(&dir)),
    ];
    for (op, a, b) in rows {
        std::mem::forget(transfers::queue(&mut s.transfers, None, op, [(a, b)]));
    }
    let p = |i: usize| s.transfers[i].progress.clone();
    p(0).size(486_201_344);
    p(0).begin();
    p(0).add_bytes(301_000_000);
    p(1).add_items(84);
    p(1).size(912_000_000);
    p(1).begin();
    p(1).add_bytes(240_000_000);
    for _ in 0..22 {
        p(1).item_done();
    }
    p(2).add_items(12);
    p(2).size(96_000_000);
    p(2).begin();
    p(2).add_bytes(96_000_000);
    for _ in 0..12 {
        p(2).item_done();
    }
    p(2).end(Ok(()));
}

/// Copying into a folder where a file of the same name is already, the question open.
pub fn conflict_scene(s: &mut State) {
    language(s);
    accounts(s);
    s.panes[0].go(Location::Local(project_dir()));
    s.panes[0].set_entries(Ok(local_files()));
    s.panes[0].free = Some(FREE);
    s.panes[1].go(drive_project());
    s.panes[1].set_entries(Ok(drive_files()));
    s.active = 0;
    trees(s);
    let name = t("Orçamento revisado.pdf", "Revised budget.pdf");
    let src = conflict::FileInfo {
        name: name.into(),
        place: project_dir().display().to_string(),
        is_dir: false,
        size: Some(1_318_204),
        modified: ago(1),
        kind: model::Kind::Pdf,
    };
    let dst = conflict::FileInfo {
        name: name.into(),
        place: t(r"Meu Drive\Projetos\Casa Jardins", r"My Drive\Projects\Garden House").into(),
        is_dir: false,
        size: Some(1_204_330),
        modified: ago(75),
        kind: model::Kind::Pdf,
    };
    s.dialog = Some(crate::actions::Dialog::Conflict(Box::new(conflict::Prompt {
        op: Op::Upload,
        src,
        dst,
        renamed: t("Orçamento revisado (2).pdf", "Revised budget (2).pdf").into(),
        may_have_more: true,
        apply_all: false,
        reply: None,
        live: None,
    })));
}

/// A search of every Google account, where each result is in the Location column.
pub fn search_scene(s: &mut State) {
    language(s);
    accounts(s);
    let text = t("orçamento", "budget");
    let loc = Location::DriveSearch(Box::new(DriveSearch { text: text.into() }));
    s.panes[1].go(loc);
    s.search_scope[1] = model::DriveSearchScope::AllAccounts;
    s.search[1] = rune::TextEdit::new(text);
    s.searched[1] = text.to_string();
    // Room for the Location column.
    s.show_tree[1] = false;
    s.layout.columns[1] = [280.0, 90.0, 150.0, 300.0];
    let at = |e: Entry, email: &str, place: &str| Entry {
        drive_origin: Some(Box::new(DriveOrigin { email: email.into(), shared_drive: None, owned_by_me: true, label: format!("{email} · {place}") })),
        ..e
    };
    let my = t("Meu Drive", "My Drive");
    let finance = t("Financeiro", "Finance");
    s.panes[1].set_entries(Ok(vec![
        at(file(t("Orçamento revisado.pdf", "Revised budget.pdf"), 1_204_330, 3), "voce@gmail.com", my),
        at(file(t("Orçamento 2026.csv", "Budget 2026.csv"), 210_330, 50), "voce@empresa.com", finance),
        at(folder(t("Orçamentos antigos", "Old budgets"), 900), "voce@gmail.com", my),
        at(file(t("Orçamento da reforma.docx", "Renovation budget.docx"), 61_904, 300), "voce@gmail.com", t("Compartilhados comigo", "Shared with me")),
        at(file(t("orçamento_fornecedor.csv", "supplier_budget.csv"), 48_210, 130), "voce@empresa.com", finance),
        at(file(t("Orçamento aprovado.pdf", "Approved budget.pdf"), 880_120, 700), "voce@empresa.com", t("Diretoria", "Board")),
    ]));
    s.panes[0].go(Location::Local(project_dir()));
    s.panes[0].set_entries(Ok(local_files()));
    s.panes[0].free = Some(FREE);
    trees(s);
}
