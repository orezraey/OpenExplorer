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
    use crate::network::{is_wsl, unc};
    if unc(dir).is_some_and(|(server, share)| share.is_empty() && is_wsl(&server)) {
        return "Linux".into();
    }
    crate::network::name(dir).unwrap_or_else(|| dir.display().to_string().trim_end_matches('\\').to_string())
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

// ---------------------------------------------------------------------------------------------
// Where the index can't look

/// Whether a search from `within` looks where the index has nothing: WSL's distributions and
/// the network's folders, which [`search_elsewhere`] reads instead.
pub fn elsewhere(within: &Path) -> bool {
    crate::network::is_remote(within)
}

/// The words of a search, folded as the index folds them: each must be in a name.
fn words(text: &str) -> Vec<Vec<u8>> {
    let mut words: Vec<Vec<u8>> = text.split_whitespace().map(openexplorer_index::fold::fold).filter(|w| !w.is_empty()).collect();
    words.sort();
    words.dedup();
    words
}

fn name_matches(words: &[Vec<u8>], name: &str) -> bool {
    matches_in(words, name, &mut Vec::new())
}

/// [`name_matches`], folding into `buf` (kept between calls, where names come by the hundred
/// thousand).
fn matches_in(words: &[Vec<u8>], name: &str, buf: &mut Vec<u8>) -> bool {
    buf.clear();
    openexplorer_index::fold::fold_into(name, buf);
    words.iter().all(|w| buf.windows(w.len()).any(|part| part == w.as_slice()))
}

/// Hands results over in pages as they're found: a few hundred at a time, or what came in the
/// last moment, so the list fills while a slow search goes on.
struct Pager<'a> {
    page: &'a mut dyn FnMut(Vec<Entry>, bool),
    buf: Vec<Entry>,
    since: std::time::Instant,
    sent: usize,
}

impl Pager<'_> {
    fn push(&mut self, e: Entry) {
        self.buf.push(e);
        if self.buf.len() >= 500 || self.since.elapsed() > Duration::from_millis(250) {
            self.flush(true);
        }
    }

    fn flush(&mut self, more: bool) {
        self.sent += self.buf.len();
        (self.page)(std::mem::take(&mut self.buf), more);
        self.since = std::time::Instant::now();
    }

    /// Whether [`LIMIT`] results were found.
    fn full(&self) -> bool {
        self.sent + self.buf.len() >= LIMIT
    }
}

/// Searches `f.within` where the index has nothing (see [`elsewhere`]): in WSL with `find`, run
/// inside the distribution, which reads its own disk in a second or two; on the network by
/// reading the folders, several at a time. Results come to `page` as they're found, the last
/// page with `more` false; it stops early once `alive` says the search was left.
pub fn search_elsewhere(f: &Found, alive: &dyn Fn() -> bool, page: &mut dyn FnMut(Vec<Entry>, bool)) -> Result<(), String> {
    use crate::network::{is_wsl, share_path, shares, unc};
    let within = f.within.as_deref().ok_or("Pesquisa sem pasta.")?;
    let words = words(&f.text);
    let mut pager = Pager { page, buf: Vec::new(), since: std::time::Instant::now(), sent: 0 };
    match unc(within) {
        // Linux: every distribution, one after the other (each starts if it isn't running).
        Some((server, distro)) if is_wsl(&server) && distro.is_empty() => {
            let mut failed = None;
            let mut any = false;
            let distros = shares(&server).map_err(|e| e.to_string())?;
            for distro in distros.iter().filter(|d| name_matches(&words, d)) {
                pager.push(entry(share_path(&server, distro).display().to_string(), true, 0, 0));
            }
            for distro in distros {
                if !alive() || pager.full() {
                    break;
                }
                match wsl_find(&distro, "/", &share_path(&server, &distro), &words, alive, &mut pager, true) {
                    Err(Find::NoPrintf) => wsl_find(&distro, "/", &share_path(&server, &distro), &words, alive, &mut pager, false),
                    done => done,
                }
                .map_or_else(|e| failed = failed.take().or(Some(e)), |_| any = true);
            }
            // One that fails (stopped for good, say) leaves the others' results.
            if let (false, Some(e)) = (any, failed) {
                return Err(match e {
                    Find::Failed(why) => why,
                    Find::NoPrintf => "O find desta distribuição não lista os arquivos como a pesquisa precisa.".into(),
                });
            }
        }
        // A computer: every one of its shared folders.
        Some((server, share)) if share.is_empty() => {
            let names = shares(&server).map_err(|e| format!("{}: {e}", within.display()))?;
            // The shared folders themselves, by name, then what's in them.
            for name in names.iter().filter(|n| name_matches(&words, n)) {
                pager.push(entry(share_path(&server, name).display().to_string(), true, 0, 0));
            }
            let all: Vec<PathBuf> = names.iter().map(|n| share_path(&server, n)).collect();
            walk(&all, &words, alive, &mut pager)?;
        }
        Some((server, distro)) if is_wsl(&server) => {
            // As `within` spells it, so results read as it does.
            let top = share_path(&server, &distro);
            let rest = within.strip_prefix(&top).map_err(|_| format!("Não entendi o caminho “{}”.", within.display()))?;
            let linux = format!("/{}", rest.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/"));
            match wsl_find(&distro, &linux, &top, &words, alive, &mut pager, true) {
                Err(Find::NoPrintf) => wsl_find(&distro, &linux, &top, &words, alive, &mut pager, false),
                done => done,
            }
            .map_err(|e| match e {
                Find::Failed(why) => why,
                Find::NoPrintf => "O find desta distribuição não lista os arquivos como a pesquisa precisa.".into(),
            })?;
        }
        _ => walk(&[within.to_path_buf()], &words, alive, &mut pager)?,
    }
    pager.flush(false);
    Ok(())
}

enum Find {
    /// BusyBox's `find` (in docker-desktop, Alpine) has no `-printf`.
    NoPrintf,
    Failed(String),
}

/// Runs `find` inside `distro` over the Linux folder `linux` and keeps what matches. With
/// `printf` each line brings the type, size and date; without it (BusyBox) only the path,
/// and each match is looked up through `\\wsl.localhost`.
fn wsl_find(distro: &str, linux: &str, top: &Path, words: &[Vec<u8>], alive: &dyn Fn() -> bool, pager: &mut Pager, printf: bool) -> Result<(), Find> {
    use std::io::{BufRead, Read};
    use std::process::{Command, Stdio};
    // `-xdev` keeps to the distribution's own disk: not /proc, nor Windows' disks in /mnt.
    // The C locale: patterns compare bytes, so `[! -~]` is any byte beyond ASCII.
    let mut args: Vec<String> = ["-d", distro, "--exec", "env", "LC_ALL=C", "find", linux, "-xdev"].map(String::from).into();
    if printf {
        args.extend(prefilter(words));
        args.extend(["-printf", "%y\\t%s\\t%T@\\t%p\\0"].map(String::from));
    } else {
        args.push("-print0".into());
    }
    let mut cmd = Command::new("wsl.exe");
    // WSL's own messages (no such distribution, WSL off) in UTF-8 rather than UTF-16.
    cmd.args(&args).env("WSL_UTF8", "1").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window flashing up.
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().map_err(|e| Find::Failed(format!("Não consegui rodar o WSL: {e}")))?;
    // Folders it may not read fill stderr: drained apart, so `find` never waits on it.
    let mut stderr = child.stderr.take().expect("piped");
    let said = std::thread::spawn(move || {
        let mut all = Vec::new();
        let _ = stderr.read_to_end(&mut all);
        String::from_utf8_lossy(&all[..all.len().min(4096)]).into_owned()
    });
    // Records are read on a thread of their own, so a search left while `find` is quiet (few
    // names match, or the disk is cold) still stops at once.
    let stdout = child.stdout.take().expect("piped");
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(4096);
    std::thread::spawn(move || {
        let mut out = std::io::BufReader::new(stdout);
        loop {
            let mut record = Vec::new();
            if out.read_until(0, &mut record).unwrap_or(0) == 0 || tx.send(record).is_err() {
                break;
            }
        }
    });
    let (mut read, mut told, mut folded) = (0usize, String::new(), Vec::new());
    loop {
        let record = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(record) => record,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if !alive() {
                    let _ = child.kill();
                    break;
                }
                // What matched shows, though no more come for a while.
                if !pager.buf.is_empty() && pager.since.elapsed() > Duration::from_millis(250) {
                    pager.flush(true);
                }
                continue;
            }
        };
        if !alive() || pager.full() {
            let _ = child.kill();
            break;
        }
        // `find`'s records end in a NUL; WSL's own messages (no such distribution) come on
        // stdout too, without one.
        let Some(record) = record.strip_suffix(&[0]) else {
            told.push_str(&String::from_utf8_lossy(&record));
            continue;
        };
        read += 1;
        // The name is checked first, from the bytes: most records don't match, and a whole
        // distribution is some 600,000 of them.
        let mut fields = record.splitn(if printf { 4 } else { 1 }, |&b| b == b'\t');
        let (kind, size, when) = if printf { (fields.next(), fields.next(), fields.next()) } else { (None, None, None) };
        let Some(path) = fields.next() else { continue };
        let name = String::from_utf8_lossy(path.rsplit(|&b| b == b'/').next().unwrap_or_default());
        // The folder searched itself isn't a result.
        if name.is_empty() || !matches_in(words, &name, &mut folded) || path == linux.as_bytes() {
            continue;
        }
        let path = String::from_utf8_lossy(path);
        let number = |f: Option<&[u8]>| -> u64 { f.and_then(|f| std::str::from_utf8(f).ok()).and_then(|t| t.split('.').next()?.parse().ok()).unwrap_or(0) };
        let (kind, size, when) = (kind == Some(b"d".as_slice()), number(size), number(when) as u32);
        let at = path.strip_prefix('/').unwrap_or(&path).split('/').fold(top.to_path_buf(), |at, step| at.join(step));
        let e = if printf {
            entry(at.display().to_string(), kind, size, when)
        } else {
            let meta = std::fs::metadata(&at).ok();
            let secs = meta.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as u32);
            entry(at.display().to_string(), meta.as_ref().is_some_and(|m| m.is_dir()), meta.map_or(0, |m| m.len()), secs)
        };
        pager.push(e);
    }
    let status = child.wait();
    let said = said.join().unwrap_or_default();
    // `find` also ends badly when some folder can't be read: only an empty answer is a failure.
    if read == 0 && !status.is_ok_and(|s| s.success()) {
        if printf && said.contains("printf") {
            return Err(Find::NoPrintf);
        }
        let why = told.lines().chain(said.lines()).map(str::trim).find(|l| !l.is_empty()).unwrap_or("o WSL não respondeu");
        return Err(Find::Failed(format!("Não consegui pesquisar em {distro}: {why}")));
    }
    Ok(())
}

/// `find`'s own first pass at the words, so a whole distribution's 600,000 names needn't all
/// cross over to be checked here: per word, a name holding it (any case), or any name with a
/// letter beyond ASCII (which may fold to it: "Relatório" for "relatorio"). Words that fold to
/// something beyond ASCII are left to the check here.
fn prefilter(words: &[Vec<u8>]) -> Vec<String> {
    let mut out = Vec::new();
    for w in words.iter().filter(|w| w.is_ascii()) {
        let mut glob = String::from("*");
        for &b in w.iter() {
            if matches!(b, b'*' | b'?' | b'[' | b']' | b'\\') {
                glob.push('\\');
            }
            glob.push(b as char);
        }
        glob.push('*');
        out.extend(["(", "-iname", &glob, "-o", "-name", "*[! -~]*", ")"].map(String::from));
    }
    out
}

/// Reads the folders under `roots` (on the network, where each folder is a round trip),
/// several at a time, and keeps what matches. Links to folders aren't followed, so a loop of
/// them ends. A root that can't be read is skipped, unless none can.
fn walk(roots: &[PathBuf], words: &[Vec<u8>], alive: &dyn Fn() -> bool, pager: &mut Pager) -> Result<(), String> {
    use std::collections::VecDeque;
    use std::sync::{Condvar, Mutex, mpsc};
    const READERS: usize = 8;
    let mut failed = None;
    let roots: VecDeque<PathBuf> = roots.iter().filter(|r| std::fs::read_dir(r).map_err(|e| failed = Some(format!("{}: {e}", r.display()))).is_ok()).cloned().collect();
    if roots.is_empty() {
        return failed.map_or(Ok(()), Err);
    }
    struct Work {
        todo: VecDeque<PathBuf>,
        /// Readers holding a folder: until they're done, more may come.
        busy: usize,
        stop: bool,
    }
    let work = Mutex::new(Work { todo: roots, busy: 0, stop: false });
    let changed = Condvar::new();
    let stop = |work: &Mutex<Work>| {
        work.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        changed.notify_all();
    };
    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::channel::<Entry>();
        for _ in 0..READERS {
            let (tx, work, changed) = (tx.clone(), &work, &changed);
            scope.spawn(move || {
                loop {
                    let dir = {
                        let mut w = work.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            if w.stop {
                                return;
                            }
                            if let Some(dir) = w.todo.pop_front() {
                                w.busy += 1;
                                break dir;
                            }
                            if w.busy == 0 {
                                w.stop = true;
                                changed.notify_all();
                                return;
                            }
                            w = changed.wait(w).unwrap_or_else(|e| e.into_inner());
                        }
                    };
                    let mut inside = Vec::new();
                    for item in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                        // A link to a folder isn't `is_dir`: it isn't gone into.
                        let is_dir = item.file_type().is_ok_and(|t| t.is_dir());
                        if is_dir {
                            inside.push(item.path());
                        }
                        let name = item.file_name().to_string_lossy().into_owned();
                        if name_matches(words, &name) {
                            // Windows' listing already holds the size and date.
                            let meta = item.metadata().ok();
                            let secs = meta.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as u32);
                            let _ = tx.send(entry(item.path().display().to_string(), is_dir, meta.map_or(0, |m| m.len()), secs));
                        }
                    }
                    let mut w = work.lock().unwrap_or_else(|e| e.into_inner());
                    w.todo.extend(inside);
                    w.busy -= 1;
                    changed.notify_all();
                }
            });
        }
        drop(tx);
        // Every reader dropping its sender ends this.
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(e) => pager.push(e),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !pager.buf.is_empty() && pager.since.elapsed() > Duration::from_millis(250) {
                        pager.flush(true);
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if !alive() || pager.full() {
                stop(&work);
            }
        }
    });
    Ok(())
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

    /// Searching by reading the folders: every word in the name, case and accents aside; the
    /// folder searched isn't a result; pages end with one saying there's no more; leaving the
    /// search stops it.
    #[test]
    fn a_search_by_reading_the_folders() {
        let w = words("  RELATORIO  2025 relatorio ");
        assert_eq!(w.len(), 2);
        assert!(name_matches(&w, "Relatório anual 2025.pdf") && !name_matches(&w, "Relatório 2024.pdf"));

        let root = std::env::temp_dir().join("openexplorer-finder-walk").join("relatorio");
        let _ = std::fs::remove_dir_all(&root);
        for d in ["a/b/c", "Relatórios", "x"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        for f in ["a/b/c/Relatório final.pdf", "x/relatorio.txt", "x/outro.txt"] {
            std::fs::write(root.join(f), "12345").unwrap();
        }
        // The folder searched is itself named "relatorio": it isn't among the results.
        let f = found("relatorio", Some(root.clone()));
        let (mut got, mut ends) = (Vec::new(), Vec::new());
        let mut pager = Pager {
            page: &mut |page: Vec<Entry>, more: bool| {
                got.extend(page);
                ends.push(more);
            },
            buf: Vec::new(),
            since: std::time::Instant::now(),
            sent: 0,
        };
        walk(std::slice::from_ref(&root), &words(&f.text), &|| true, &mut pager).unwrap();
        pager.flush(false);
        drop(pager);
        let mut names: Vec<(&str, bool, Option<u64>)> = got.iter().map(|e| (e.name.as_str(), e.is_dir, e.size)).collect();
        names.sort();
        assert_eq!(names, [("Relatório final.pdf", false, Some(5)), ("Relatórios", true, None), ("relatorio.txt", false, Some(5))]);
        assert_eq!(ends.last(), Some(&false));
        // A search left at once reads nothing more.
        let mut left = Vec::new();
        search_elsewhere(&f, &|| false, &mut |page, _| left.extend(page)).ok();
        assert!(left.len() <= 3);
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    /// Only reads, in this computer's archlinux distribution.
    #[test]
    #[ignore = "runs find in WSL's archlinux"]
    fn a_search_in_wsl() {
        let f = found("passwd", Some(PathBuf::from(r"\\wsl.localhost\archlinux\etc")));
        let mut got = Vec::new();
        let started = std::time::Instant::now();
        search_elsewhere(&f, &|| true, &mut |page, _| got.extend(page)).unwrap();
        println!("{} results in {:?}: {:?}", got.len(), started.elapsed(), got.iter().map(|e| (&e.id, e.is_dir, e.size)).collect::<Vec<_>>());
        assert!(got.iter().any(|e| e.id == r"\\wsl.localhost\archlinux\etc\passwd" && !e.is_dir && e.size.is_some()));
        // Reading the folders through `\\wsl.localhost`, as a shared folder is read, finds the same.
        let mut walked = Vec::new();
        let started = std::time::Instant::now();
        let mut pager = Pager { page: &mut |page: Vec<Entry>, _| walked.extend(page), buf: Vec::new(), since: std::time::Instant::now(), sent: 0 };
        walk(&[PathBuf::from(r"\\wsl.localhost\archlinux\etc")], &words("passwd"), &|| true, &mut pager).unwrap();
        pager.flush(false);
        drop(pager);
        println!("walked in {:?}", started.elapsed());
        let ids = |v: &[Entry]| v.iter().map(|e| e.id.clone()).collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids(&walked), ids(&got));
        // A spelling of its own stays in the results, and an unknown distribution says so.
        let mut upper = Vec::new();
        search_elsewhere(&found("passwd", Some(PathBuf::from(r"\\WSL.LOCALHOST\archlinux\etc"))), &|| true, &mut |page, _| upper.extend(page)).unwrap();
        assert!(upper.iter().all(|e| e.id.starts_with(r"\\WSL.LOCALHOST\archlinux\etc\")) && upper.len() == got.len());
        let missing = search_elsewhere(&found("x", Some(PathBuf::from(r"\\wsl.localhost\naoexiste"))), &|| true, &mut |_, _| {});
        println!("unknown distribution: {missing:?}");
        assert!(missing.is_err_and(|e| e.contains("naoexiste") && e.contains("distribuição") && !e.contains('\0')));
        // Linux searches every distribution, its names included; a computer, every shared
        // folder, the folders themselves included.
        let mut linux = Vec::new();
        let started = std::time::Instant::now();
        search_elsewhere(&found("passwd", Some(PathBuf::from(r"\\wsl.localhost"))), &|| true, &mut |page, _| linux.extend(page)).unwrap();
        println!("Linux: {} results in {:?}", linux.len(), started.elapsed());
        assert!(linux.iter().any(|e| e.id == r"\\wsl.localhost\archlinux\etc\passwd") && linux.iter().any(|e| e.id.starts_with(r"\\wsl.localhost\docker-desktop\")));
        let mut named = Vec::new();
        search_elsewhere(&found("arch", Some(PathBuf::from(r"\\wsl.localhost"))), &|| false, &mut |page, _| named.extend(page)).unwrap();
        assert!(named.iter().any(|e| e.id == r"\\wsl.localhost\archlinux" && e.is_dir), "{named:?}");
        let mut shared = Vec::new();
        search_elsewhere(&found("vods", Some(PathBuf::from(r"\\localhost"))), &|| true, &mut |page, _| shared.extend(page)).unwrap();
        assert!(shared.iter().any(|e| e.id == r"\\localhost\Vods" && e.is_dir), "{shared:?}");
        let f = found("hosts", Some(PathBuf::from(r"\\wsl.localhost\docker-desktop\etc")));
        let mut busybox = Vec::new();
        let r = search_elsewhere(&f, &|| true, &mut |page, _| busybox.extend(page));
        println!("busybox: {r:?} {:?}", busybox.iter().map(|e| (&e.id, e.is_dir, e.size)).collect::<Vec<_>>());
    }
}
