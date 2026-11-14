//! Searching the computer: the words typed in a local pane go to the index service
//! (`openexplorer-index`, see that crate), which answers from every NTFS volume's MFT in
//! milliseconds. Results are entries whose id is their full path.

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use crate::model::{Entry, Found};

/// The most results a search shows; how many there are in all is said apart.
pub const LIMIT: usize = 100_000;

/// The index service, as the search bar tells of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    Running,
    Starting,
    Stopped,
    Missing,
}

/// What a search's answer says besides its results.
#[derive(Clone, Debug, PartialEq)]
pub struct Info {
    pub total: usize,
    pub shown: usize,
    /// Volumes still being read ("C:\"), whose results may be missing.
    pub indexing: Vec<String>,
    pub service: Service,
}

/// Where a search from `dir` looks, in words: the folder's name, or the disk's ("C:").
pub fn place_of(dir: &Path) -> String {
    dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| dir.display().to_string().trim_end_matches('\\').to_string())
}

fn entry(path: String, is_dir: bool, size: u64, modified: u32) -> Entry {
    let name = Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
    Entry {
        id: path,
        name,
        is_dir,
        size: (!is_dir).then_some(size),
        modified: (modified > 0).then(|| UNIX_EPOCH + Duration::from_secs(modified as u64)),
        target: None,
        mime: None,
        md5: None,
        starred: false,
    }
}

/// Results to show a search without the service (the `found` scene).
pub fn sample() -> Vec<Entry> {
    [
        (r"C:\Users\voce\Documentos\Relatório anual 2025.pdf", false, 2_400_000, 1_750_000_000),
        (r"C:\Users\voce\Documentos\Relatórios", true, 0, 1_740_000_000),
        (r"C:\Users\voce\Downloads\relatorio-vendas.xlsx", false, 88_000, 1_760_000_000),
        (r"C:\Users\voce\Projetos\site\relatorio.ts", false, 4_100, 1_762_000_000),
    ]
    .into_iter()
    .map(|(p, d, s, m)| entry(p.into(), d, s, m))
    .collect()
}

/// Why a search can't run without the service.
pub const MISSING: &str = "Falta instalar o serviço de pesquisa (link acima).";

/// The folder `e`, a result, sits in.
pub fn folder_of(e: &Entry) -> String {
    Path::new(&e.id).parent().map(|p| p.display().to_string()).unwrap_or_default()
}

#[cfg(windows)]
fn service() -> Service {
    use openexplorer_index::service::{State, state};
    match state() {
        State::Running => Service::Running,
        State::Starting => Service::Starting,
        State::Stopped => Service::Stopped,
        State::Missing => Service::Missing,
    }
}

/// Asks the service for `f`'s words. Without it, an error saying why, and how it is.
#[cfg(windows)]
pub fn search(f: &Found) -> (Result<Vec<Entry>, String>, Info) {
    use openexplorer_index::proto::Request;
    let req = Request { text: f.text.clone(), within: f.within.as_ref().map(|w| w.display().to_string()), limit: LIMIT };
    match openexplorer_index::pipe::ask(&req) {
        Ok(a) => {
            let info = Info {
                total: a.header.total,
                shown: a.rows.len(),
                indexing: a.header.volumes.iter().filter(|v| v.indexing).map(|v| v.root.clone()).collect(),
                service: Service::Running,
            };
            let entries = a.rows.into_iter().map(|r| entry(r.path, r.is_dir, r.size, r.modified)).collect();
            (a.header.error.map_or(Ok(entries), Err), info)
        }
        Err(e) => {
            let service = service();
            let why = match service {
                Service::Missing => MISSING.to_string(),
                Service::Stopped => "O serviço de pesquisa está parado. Reinstale-o para que volte a iniciar com o Windows.".to_string(),
                Service::Starting => "O serviço de pesquisa está iniciando; tente de novo em instantes.".to_string(),
                Service::Running => e,
            };
            (Err(why), Info { total: 0, shown: 0, indexing: Vec::new(), service })
        }
    }
}

/// Asks the user's own service, which is started when it isn't running.
#[cfg(target_os = "linux")]
pub fn search(f: &Found) -> (Result<Vec<Entry>, String>, Info) {
    use openexplorer_index::proto::Request;
    let req = Request { text: f.text.clone(), within: f.within.as_ref().map(|w| w.display().to_string()), limit: LIMIT };
    match openexplorer_index::unix::ask(&req) {
        Ok(a) => {
            let info = Info { total: a.header.total, shown: a.rows.len(), indexing: a.header.volumes.iter().filter(|v| v.indexing).map(|v| v.root.clone()).collect(), service: Service::Running };
            let entries = a.rows.into_iter().map(|r| entry(r.path, r.is_dir, r.size, r.modified)).collect();
            (a.header.error.map_or(Ok(entries), Err), info)
        }
        Err(e) => (Err(e), Info { total: 0, shown: 0, indexing: Vec::new(), service: Service::Stopped }),
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn search(_: &Found) -> (Result<Vec<Entry>, String>, Info) {
    (Err("A pesquisa no computador só existe no Windows e no Linux.".into()), Info { total: 0, shown: 0, indexing: Vec::new(), service: Service::Missing })
}

/// Installs the service, Windows asking for an administrator's permission; blocks until done.
#[cfg(windows)]
pub fn install() -> Result<String, String> {
    openexplorer_index::service::install_elevated()?;
    // Started: its pipe opens as soon as it runs, and volumes fill in as they're read.
    for _ in 0..50 {
        if openexplorer_index::pipe::ask(&openexplorer_index::proto::Request::default()).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok("Serviço de pesquisa instalado. Os discos estão sendo lidos; os resultados aparecem em segundos.".into())
}

/// Has systemd start the service at login (no root); it runs from now on too.
#[cfg(target_os = "linux")]
pub fn install() -> Result<String, String> {
    openexplorer_index::unix::install()?;
    Ok("Serviço de pesquisa instalado: inicia com a sessão.".into())
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn install() -> Result<String, String> {
    Err("A pesquisa no computador só existe no Windows e no Linux.".into())
}

/// Where searching `text` from the local location `within` (a folder, or everywhere) goes.
pub fn found(text: &str, within: Option<PathBuf>) -> Found {
    let place = within.as_deref().map_or_else(|| "todo o computador".to_string(), place_of);
    Found { text: text.split_whitespace().collect::<Vec<_>>().join(" "), within, place }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_become_entries_named_by_their_file() {
        // The paths each system's index sends.
        let (ana, file, fotos, root, root_place) =
            if cfg!(windows) { (r"C:\Users\Ana", r"C:\Users\Ana\Relatório.pdf", r"D:\Fotos", r"C:\", "C:") } else { ("/home/Ana", "/home/Ana/Relatório.pdf", "/mnt/hd/Fotos", "/", "/") };
        let e = entry(file.into(), false, 10, 60);
        assert_eq!((e.id.as_str(), e.name.as_str(), e.size), (file, "Relatório.pdf", Some(10)));
        assert_eq!(e.modified, Some(UNIX_EPOCH + Duration::from_secs(60)));
        assert_eq!(folder_of(&e), ana);
        let d = entry(fotos.into(), true, 0, 0);
        assert_eq!((d.name.as_str(), d.size, d.modified), ("Fotos", None, None));
        assert_eq!(place_of(Path::new(root)), root_place);
        assert_eq!(place_of(Path::new(ana)), "Ana");
        let f = found("  nota   fiscal ", Some(ana.into()));
        assert_eq!((f.text.as_str(), f.place.as_str()), ("nota fiscal", "Ana"));
        assert_eq!(found("x", None).place, "todo o computador");
    }
}
