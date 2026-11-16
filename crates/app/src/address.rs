//! The address bar as text: what it shows of a location, to copy or edit, and where text typed
//! or pasted there leads. On the computer that's a path (relative to the folder shown, with
//! `%VARIÁVEIS%`); in Google Drive, a path of names under the account
//! (`ana@gmail.com/Meu Drive/Fotos`) or a link from drive.google.com.
//!
//! Drive lets a name hold `/`: in an address it shows as `／`, as Air Explorer's does, and leads
//! back to that name (the name in Drive keeps its `/`). Drive also lets names repeat in a
//! folder: a path leads to the first one Drive lists; a link always leads to the one it names.

use std::path::{Component, Path, PathBuf};

use openexplorer_gdrive::{Drive, DriveFile, Place, Top, View, valid_id};

use crate::model::{Location, NAME_SLASH, Space, shown, typed};

/// The names an address step may stand for: with [`NAME_SLASH`] as the `/` it stands for in
/// the address, first; then as written, for a name that holds a real `／`.
fn names_for(step: &str) -> Vec<String> {
    let mut names = Vec::new();
    if step.contains(NAME_SLASH) {
        names.push(typed(step));
    }
    names.push(step.to_string());
    names
}

/// The Drive spaces an address names after the account, in the words the path bar uses, and
/// the English ones Drive's website uses.
const SPACES: &[(&str, &str, Root)] = &[
    ("Meu Drive", "My Drive", Root::MyDrive),
    ("Drives compartilhados", "Shared drives", Root::SharedDrives),
    ("Compartilhados comigo", "Shared with me", Root::View(View::SharedWithMe)),
    ("Com estrela", "Starred", Root::View(View::Starred)),
    ("Recentes", "Recent", Root::View(View::Recent)),
    ("Lixeira", "Trash", Root::View(View::Trash)),
];

/// The simulated drive's name in the path bar.
const MOCK: &str = "Drive simulado";

/// Where a pane goes, and the entry (id, name) to select there once it's listed.
#[derive(Clone, Debug, PartialEq)]
pub struct Reached {
    pub loc: Location,
    pub reveal: Option<(String, String)>,
}

/// What an address names, before anything is looked up.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Local(PathBuf),
    Mock(Vec<String>),
    /// In `email`'s Drive.
    Drive { email: String, way: Way },
}

/// How a Drive address names its place.
#[derive(Clone, Debug, PartialEq)]
pub enum Way {
    /// Folder names under a space; with `SharedDrives`, the first is the shared drive's.
    Path { root: Root, names: Vec<String> },
    /// A file or folder by its id, from a link.
    Item(String),
    /// A space a link names (drive.google.com/drive/starred).
    Space(Root),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Root {
    MyDrive,
    SharedDrives,
    View(View),
}

impl Root {
    fn label(self) -> &'static str {
        SPACES.iter().find(|(_, _, r)| *r == self).map_or("", |(pt, _, _)| pt)
    }

    fn space(self) -> Space {
        match self {
            Root::MyDrive => Space::MyDrive,
            Root::SharedDrives => Space::SharedDrives,
            Root::View(View::SharedWithMe) => Space::SharedWithMe,
            Root::View(View::Starred) => Space::Starred,
            Root::View(View::Recent) => Space::Recent,
            Root::View(View::Trash) => Space::Trash,
        }
    }
}

/// The address of `loc`, as the bar shows it to edit and Copiar endereço copies it. A search's
/// results give the place searched (the account, for a search of Drive).
pub fn text(loc: &Location) -> String {
    match loc {
        Location::Local(dir) => dir.display().to_string(),
        Location::Found(f) => f.within.as_ref().map(|w| w.display().to_string()).unwrap_or_default(),
        Location::Mock(segs) => std::iter::once(MOCK).chain(segs.iter().map(String::as_str)).collect::<Vec<_>>().join("/"),
        Location::Drive { email, space, path } => {
            let mut parts: Vec<String> = vec![email.clone()];
            match space {
                Space::MyDrive => parts.push(Root::MyDrive.label().into()),
                Space::SharedDrives => parts.push(Root::SharedDrives.label().into()),
                Space::Shared { name, .. } => parts.extend([Root::SharedDrives.label().into(), shown(name)]),
                Space::SharedWithMe | Space::Starred | Space::Recent | Space::Trash => parts.push(space.label().into()),
                Space::Search(_) => return email.clone(),
            }
            parts.extend(path.iter().map(|(_, name)| shown(name)));
            parts.join("/")
        }
    }
}

/// The drive.google.com link of the Drive place `loc` shows: a folder, a shared drive or one
/// of Drive's lists. `None` off Drive and for a search's results.
pub fn drive_link(loc: &Location) -> Option<String> {
    const BASE: &str = "https://drive.google.com/drive";
    let Location::Drive { space, path, .. } = loc else { return None };
    if let Some((id, _)) = path.last() {
        return Some(format!("{BASE}/folders/{id}"));
    }
    Some(match space {
        Space::MyDrive => format!("{BASE}/my-drive"),
        Space::SharedDrives => format!("{BASE}/shared-drives"),
        Space::Shared { id, .. } => format!("{BASE}/folders/{id}"),
        Space::SharedWithMe => format!("{BASE}/shared-with-me"),
        Space::Starred => format!("{BASE}/starred"),
        Space::Recent => format!("{BASE}/recent"),
        Space::Trash => format!("{BASE}/trash"),
        Space::Search(_) => return None,
    })
}

/// What `typed` names, read from the pane at `here`. `accounts` are the connected ones: a Drive
/// path starts with one, and a link opens in `here`'s account or else the first.
pub fn parse(typed: &str, here: &Location, accounts: &[String]) -> Result<Target, String> {
    // Explorer's "Copiar como caminho" wraps the path in quotes.
    let typed = typed.trim().trim_matches('"').trim();
    if typed.is_empty() {
        return Err("Digite um endereço.".into());
    }
    if let Some(way) = link(typed) {
        let email = match here {
            Location::Drive { email, .. } => Some(email.clone()),
            _ => accounts.first().cloned(),
        };
        let email = email.ok_or("Conecte uma conta do Google Drive para abrir links do Drive.")?;
        return Ok(Target::Drive { email, way });
    }
    let mut parts = typed.split('/').filter(|p| !p.is_empty());
    let first = parts.next().unwrap_or_default();
    if let Some(email) = accounts.iter().find(|a| a.eq_ignore_ascii_case(first)) {
        let rest: Vec<String> = parts.map(str::to_string).collect();
        return Ok(Target::Drive { email: email.clone(), way: drive_path(rest) });
    }
    if first.eq_ignore_ascii_case(MOCK) {
        return Ok(Target::Mock(parts.map(str::to_string).collect()));
    }
    Ok(Target::Local(local_path(typed, here)))
}

/// A path's names after the account: a space's name first, Meu Drive when it names none.
fn drive_path(mut names: Vec<String>) -> Way {
    let root = names.first().and_then(|n| SPACES.iter().find(|(pt, en, _)| n.eq_ignore_ascii_case(pt) || n.eq_ignore_ascii_case(en)));
    match root {
        Some((_, _, root)) => {
            names.remove(0);
            Way::Path { root: *root, names }
        }
        None => Way::Path { root: Root::MyDrive, names },
    }
}

/// What a drive.google.com (or docs.google.com) link names: an item by its id, or one of
/// Drive's spaces. `None` for anything else.
fn link(typed: &str) -> Option<Way> {
    let rest = typed.strip_prefix("https://").or_else(|| typed.strip_prefix("http://")).unwrap_or(typed);
    let (host, rest) = rest.split_once('/').unwrap_or((rest, ""));
    if !matches!(host, "drive.google.com" | "docs.google.com") {
        return None;
    }
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let path = path.split('#').next().unwrap_or_default();
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // /drive/folders/ID, /file/d/ID/view, /document/d/ID/edit...
    let after = |word: &str| segs.iter().position(|s| *s == word).and_then(|i| segs.get(i + 1)).copied();
    let id = after("folders").or_else(|| after("d")).or_else(|| query.split('&').find_map(|kv| kv.strip_prefix("id=")));
    if let Some(id) = id.filter(|id| valid_id(id)) {
        return Some(Way::Item(id.to_string()));
    }
    let space = match segs.last().copied() {
        Some("my-drive") | Some("root") | Some("home") => Root::MyDrive,
        Some("shared-drives") => Root::SharedDrives,
        Some("shared-with-me") => Root::View(View::SharedWithMe),
        Some("starred") => Root::View(View::Starred),
        Some("recent") => Root::View(View::Recent),
        Some("trash") => Root::View(View::Trash),
        _ => return None,
    };
    Some(Way::Space(space))
}

/// A typed path on the computer: `%VARIÁVEIS%` filled in, a leading `~` as the user's folder,
/// relative to the folder `here` shows, `.` and `..` worked out, a bare `C:` as the disk's
/// root.
fn local_path(typed: &str, here: &Location) -> PathBuf {
    let mut text = expand_vars(typed);
    if let Some(rest) = text.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with(['/', '\\']))
        && let Location::Local(home) = Location::home()
    {
        text = format!("{}{rest}", home.display());
    }
    if text.len() == 2 && text.ends_with(':') {
        text.push(std::path::MAIN_SEPARATOR);
    }
    let path = PathBuf::from(text);
    let base = match here {
        Location::Local(dir) => Some(dir.clone()),
        Location::Found(f) => f.within.clone(),
        _ => None,
    };
    let path = match base {
        Some(base) if path.is_relative() => base.join(path),
        _ => path,
    };
    normalize(&path)
}

/// `%NAME%` replaced by the environment variable, when there's one by that name.
fn expand_vars(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%').map(|end| (&after[..end], &after[end + 1..])) {
            Some((name, tail)) if !name.is_empty() && let Ok(value) = std::env::var(name) => {
                out.push_str(&value);
                rest = tail;
            }
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `.` and `..` worked out by hand, without asking the disk (which would also turn `C:\` into
/// `\\?\C:\`).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// Where a path on the computer leads: the folder itself, or a file's folder with the file
/// selected. It asks the disk, which can take a while on a network drive: off the UI thread.
pub fn reach_local(path: &Path) -> Result<Reached, String> {
    let meta = std::fs::metadata(path).map_err(|_| format!("Não encontrei “{}”.", path.display()))?;
    if meta.is_dir() {
        return Ok(Reached { loc: Location::Local(path.to_path_buf()), reveal: None });
    }
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(format!("Não encontrei “{}”.", path.display()));
    };
    let name = name.to_string_lossy().into_owned();
    Ok(Reached { loc: Location::Local(dir.to_path_buf()), reveal: Some((name.clone(), name)) })
}

/// Where a Drive address leads in `email`'s account: a folder, or a file's folder with the file
/// selected.
pub fn reach_drive(d: &Drive, email: String, way: Way) -> Result<Reached, String> {
    match way {
        Way::Space(root) => Ok(Reached { loc: Location::drive(email, root.space()), reveal: None }),
        Way::Item(id) => {
            let place = d.place(&id)?;
            if place.is_folder {
                return Ok(Reached { loc: folder_location(email, &id, place), reveal: None });
            }
            let name = place.name.clone();
            Ok(Reached { loc: place_location(email, place), reveal: Some((id, name)) })
        }
        Way::Path { root, names } => reach_path(d, email, root, names),
    }
}

fn reach_path(d: &Drive, email: String, root: Root, names: Vec<String>) -> Result<Reached, String> {
    let mut names = names.into_iter();
    // Where the first name is looked for: a folder (and its shared drive), or one of Drive's
    // lists.
    let (space, mut at, mut shared) = match root {
        Root::MyDrive => (Space::MyDrive, Some("root".to_string()), None),
        Root::SharedDrives => match names.next() {
            None => (Space::SharedDrives, None, None),
            Some(name) => {
                let drives = d.shared_drives()?;
                let wanted = names_for(&name);
                let found = drives.into_iter().find(|s| wanted.contains(&s.name)).ok_or_else(|| format!("Não encontrei o drive compartilhado “{name}”."))?;
                (Space::Shared { id: found.id.clone(), name: found.name }, Some(found.id.clone()), Some(found.id))
            }
        },
        Root::View(View::Recent | View::Trash) if !names.as_slice().is_empty() => {
            return Err(format!("{} só abre como lista: as pastas dela não abrem pelo endereço.", root.label()));
        }
        // Drive's lists hold no folders of their own: names are looked for among what they list.
        Root::View(_) => (root.space(), None, None),
    };
    let mut path: Vec<(String, String)> = Vec::new();
    let mut names = names.peekable();
    while let Some(name) = names.next() {
        // With `／`, the `/` name it stands for first, then the step as written.
        let mut found = None;
        for wanted in names_for(&name) {
            found = match (&at, root) {
                (Some(parent), _) => d.child_named(parent, shared.as_deref(), &wanted)?,
                (None, Root::View(view)) => d.view_child_named(view, &wanted)?,
                (None, _) => None,
            };
            if found.is_some() {
                break;
            }
        }
        let Some(found) = found else {
            let place = path.last().map_or_else(|| root_place(root, &space), |(_, n)| n.clone());
            return Err(format!("Não encontrei “{name}” em {place}."));
        };
        if !found.is_dir {
            // A file names the end of the way: its folder opens with it selected.
            if names.peek().is_some() {
                return Err(format!("“{name}” é um arquivo, não uma pasta."));
            }
            return Ok(Reached { loc: Location::Drive { email, space, path }, reveal: Some((found.id, found.name)) });
        }
        let id = opened_id(&found);
        // A shortcut may lead into another shared drive: from there, names are looked for in
        // every drive.
        if found.target.is_some() {
            shared = None;
        }
        at = Some(id.clone());
        path.push((id, found.name));
    }
    Ok(Reached { loc: Location::Drive { email, space, path }, reveal: None })
}

/// How the first place an address looks in is called in an error.
fn root_place(root: Root, space: &Space) -> String {
    match space {
        Space::Shared { name, .. } => name.clone(),
        _ => root.label().to_string(),
    }
}

/// The folder a found folder opens: a shortcut's target, else itself.
fn opened_id(f: &DriveFile) -> String {
    f.target.clone().unwrap_or_else(|| f.id.clone())
}

/// The folder Drive says an item of `email`'s sits in, as a pane goes there.
pub fn place_location(email: String, place: Place) -> Location {
    let space = match place.top {
        Top::MyDrive => Space::MyDrive,
        Top::SharedDrive { id, name } => Space::Shared { id, name },
        Top::SharedWithMe => Space::SharedWithMe,
    };
    Location::Drive { email, space, path: place.folders }
}

/// Folder `id` itself, where `place` says it sits.
fn folder_location(email: String, id: &str, place: Place) -> Location {
    // A shared drive's root is the drive, not a folder in it.
    if matches!(&place.top, Top::SharedDrive { id: drive, .. } if drive == id) {
        return place_location(email, place);
    }
    let name = place.name.clone();
    let mut loc = place_location(email, place);
    if let Location::Drive { path, .. } = &mut loc {
        path.push((id.to_string(), name));
    }
    loc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(space: Space, path: &[(&str, &str)]) -> Location {
        Location::Drive { email: "ana@gmail.com".into(), space, path: path.iter().map(|(i, n)| (i.to_string(), n.to_string())).collect() }
    }

    fn accounts() -> Vec<String> {
        vec!["ana@gmail.com".into(), "bia@gmail.com".into()]
    }

    /// What the bar shows of each place reads back to that place.
    #[test]
    fn an_address_names_its_place_and_reads_back() {
        let here = Location::Local(PathBuf::from(r"C:\Users\Ana"));
        assert_eq!(text(&here), r"C:\Users\Ana");
        assert_eq!(parse(&text(&here), &here, &accounts()), Ok(Target::Local(PathBuf::from(r"C:\Users\Ana"))));
        let fotos = drive(Space::MyDrive, &[("f1", "Fotos"), ("f2", "2024")]);
        assert_eq!(text(&fotos), "ana@gmail.com/Meu Drive/Fotos/2024");
        let way = Way::Path { root: Root::MyDrive, names: vec!["Fotos".into(), "2024".into()] };
        assert_eq!(parse(&text(&fotos), &here, &accounts()), Ok(Target::Drive { email: "ana@gmail.com".into(), way }));
        let shared = drive(Space::Shared { id: "0A".into(), name: "Equipe".into() }, &[("f1", "Filmes")]);
        assert_eq!(text(&shared), "ana@gmail.com/Drives compartilhados/Equipe/Filmes");
        let way = Way::Path { root: Root::SharedDrives, names: vec!["Equipe".into(), "Filmes".into()] };
        assert_eq!(parse(&text(&shared), &here, &accounts()), Ok(Target::Drive { email: "ana@gmail.com".into(), way }));
        assert_eq!(text(&drive(Space::Starred, &[])), "ana@gmail.com/Com estrela");
        assert_eq!(text(&Location::Mock(vec!["Fotos".into()])), "Drive simulado/Fotos");
        assert_eq!(parse("Drive simulado/Fotos", &here, &accounts()), Ok(Target::Mock(vec!["Fotos".into()])));
    }

    /// A `/` in a Drive name shows as `／` and reads back as one step that stands for the `/`
    /// name first.
    #[test]
    fn a_slash_in_a_drive_name_is_a_full_width_one() {
        let here = Location::home();
        let loc = drive(Space::Shared { id: "0A".into(), name: "Séries/Filmes".into() }, &[("f1", "tes/test")]);
        assert_eq!(text(&loc), "ana@gmail.com/Drives compartilhados/Séries／Filmes/tes／test");
        let way = Way::Path { root: Root::SharedDrives, names: vec!["Séries／Filmes".into(), "tes／test".into()] };
        assert_eq!(parse(&text(&loc), &here, &accounts()), Ok(Target::Drive { email: "ana@gmail.com".into(), way }));
        assert_eq!(names_for("tes／test"), ["tes/test", "tes／test"]);
        assert_eq!(names_for("Fotos"), ["Fotos"]);
    }

    /// Typed paths on the computer: quotes, relative to the folder shown, `..`, a bare disk,
    /// and environment variables.
    #[test]
    fn a_typed_path_is_read_as_explorer_reads_it() {
        let here = Location::Local(PathBuf::from(r"C:\Users\Ana"));
        let local = |t: &str| match parse(t, &here, &[]) {
            Ok(Target::Local(p)) => p,
            other => panic!("{t}: {other:?}"),
        };
        assert_eq!(local(r#""C:\Program Files""#), PathBuf::from(r"C:\Program Files"));
        assert_eq!(local("Downloads"), PathBuf::from(r"C:\Users\Ana\Downloads"));
        assert_eq!(local(r"..\Bia\.\Fotos"), PathBuf::from(r"C:\Users\Bia\Fotos"));
        assert_eq!(local("D:"), PathBuf::from(r"D:\"));
        // SAFETY (set_var): only this test reads OE_ADDRESS_TEST.
        unsafe { std::env::set_var("OE_ADDRESS_TEST", r"C:\Dados") };
        assert_eq!(local(r"%OE_ADDRESS_TEST%\x"), PathBuf::from(r"C:\Dados\x"));
        assert_eq!(local(r"100% certo"), PathBuf::from(r"C:\Users\Ana\100% certo"));
        assert_eq!(parse("  ", &here, &[]), Err("Digite um endereço.".into()));
    }

    /// Drive paths: the account in any case, a space's English name, Meu Drive when none is
    /// named, and only connected accounts.
    #[test]
    fn a_drive_path_starts_with_a_connected_account() {
        let here = Location::Local(PathBuf::from(r"C:\"));
        let way = |t: &str| match parse(t, &here, &accounts()) {
            Ok(Target::Drive { way, .. }) => way,
            other => panic!("{t}: {other:?}"),
        };
        assert_eq!(way("ANA@gmail.com/Shared with me/Aulas/"), Way::Path { root: Root::View(View::SharedWithMe), names: vec!["Aulas".into()] });
        assert_eq!(way("bia@gmail.com/Fotos"), Way::Path { root: Root::MyDrive, names: vec!["Fotos".into()] });
        assert_eq!(way("ana@gmail.com"), Way::Path { root: Root::MyDrive, names: vec![] });
        assert!(matches!(parse("eva@gmail.com/Meu Drive", &here, &accounts()), Ok(Target::Local(_))));
    }

    /// Links name an item by id, or one of Drive's lists; they open in the pane's account.
    #[test]
    fn a_drive_link_names_an_item_or_a_list() {
        let here = drive(Space::MyDrive, &[]);
        let way = |t: &str| match parse(t, &here, &accounts()) {
            Ok(Target::Drive { email, way }) => {
                assert_eq!(email, "ana@gmail.com");
                way
            }
            other => panic!("{t}: {other:?}"),
        };
        assert_eq!(way("https://drive.google.com/drive/folders/1AbC_d-9?usp=sharing"), Way::Item("1AbC_d-9".into()));
        assert_eq!(way("https://drive.google.com/drive/u/1/folders/0AH3L1"), Way::Item("0AH3L1".into()));
        assert_eq!(way("https://drive.google.com/file/d/1xYz/view?usp=drive_link"), Way::Item("1xYz".into()));
        assert_eq!(way("https://docs.google.com/spreadsheets/d/1Sheet/edit#gid=0"), Way::Item("1Sheet".into()));
        assert_eq!(way("https://drive.google.com/open?id=1Open"), Way::Item("1Open".into()));
        assert_eq!(way("drive.google.com/drive/u/0/starred"), Way::Space(Root::View(View::Starred)));
        assert_eq!(way("https://drive.google.com/drive/my-drive"), Way::Space(Root::MyDrive));
        assert_eq!(way("https://drive.google.com/drive/u/0/home"), Way::Space(Root::MyDrive));
        // Each place's link reads back to it.
        for loc in [drive(Space::MyDrive, &[("f9", "Fotos")]), drive(Space::Trash, &[]), drive(Space::SharedDrives, &[])] {
            let link = drive_link(&loc).unwrap();
            assert!(matches!(parse(&link, &here, &accounts()), Ok(Target::Drive { .. })), "{link}");
        }
        assert_eq!(drive_link(&drive(Space::Shared { id: "0A".into(), name: "E".into() }, &[])).as_deref(), Some("https://drive.google.com/drive/folders/0A"));
        // From the computer, a link opens in the first account; with none, it can't.
        assert!(matches!(parse("https://drive.google.com/drive/recent", &Location::home(), &accounts()), Ok(Target::Drive { email, .. }) if email == "ana@gmail.com"));
        assert!(parse("https://drive.google.com/drive/recent", &Location::home(), &[]).is_err());
    }

    /// Against the first connected account (read-only): a shared drive's first folder reached
    /// by its address and by its link, the drive itself by its link, and a file in that folder
    /// by its link, landing in the folder with it selected.
    /// `cargo test -p openexplorer --release live_drive_addresses -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_drive_addresses() {
        let account = openexplorer_gdrive::store::accounts().into_iter().next().expect("no connected account");
        let d = crate::open_drive(&account.email).unwrap();
        let email = account.email.clone();
        let shared = d.shared_drives().unwrap().into_iter().next().expect("no shared drive");
        let mut first = None;
        d.list(&shared.id, Some(&shared.id), |page, _| {
            first = page.into_iter().find(|f| f.is_dir);
            first.is_none()
        })
        .unwrap();
        let folder = first.expect("no folder in the shared drive");
        let space = Space::Shared { id: shared.id.clone(), name: shared.name.clone() };
        let loc = Location::Drive { email: email.clone(), space: space.clone(), path: vec![(folder.id.clone(), folder.name.clone())] };
        let accounts = [email.clone()];
        let go = |typed: &str| {
            let t = std::time::Instant::now();
            let Ok(Target::Drive { email, way }) = parse(typed, &Location::home(), &accounts) else { panic!("{typed}: not a Drive address") };
            let reached = reach_drive(&d, email, way);
            println!("{typed}\n  -> {reached:?} ({} ms)", t.elapsed().as_millis());
            reached.unwrap()
        };
        assert_eq!(go(&text(&loc)), Reached { loc: loc.clone(), reveal: None });
        assert_eq!(go(&drive_link(&loc).unwrap()), Reached { loc: loc.clone(), reveal: None });
        let root = Location::drive(email.clone(), space);
        assert_eq!(go(&drive_link(&root).unwrap()), Reached { loc: root, reveal: None });
        // A file at the top of My Drive, by its link and by its name.
        let mut file = None;
        d.list("root", None, |page, _| {
            file = page.into_iter().find(|f| !f.is_dir);
            file.is_none()
        })
        .unwrap();
        let file = file.expect("no file at the top of My Drive");
        let my_drive = Location::drive(email.clone(), Space::MyDrive);
        let link = format!("https://drive.google.com/file/d/{}/view", file.id);
        assert_eq!(go(&link), Reached { loc: my_drive.clone(), reveal: Some((file.id.clone(), file.name.clone())) });
        let by_name = format!("{}/{}", text(&my_drive), file.name);
        assert_eq!(go(&by_name), Reached { loc: my_drive, reveal: Some((file.id.clone(), file.name.clone())) });
        assert!(reach_drive(&d, email.clone(), Way::Path { root: Root::MyDrive, names: vec!["não existe 7f3a".into()] }).unwrap_err().starts_with("Não encontrei"));
    }

    /// A folder opens itself; a file, its folder with it selected; anything else says so.
    #[test]
    fn a_local_path_leads_to_a_folder_or_a_file_in_it() {
        let dir = std::env::temp_dir().join("openexplorer-address-test");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("notas.txt"), "x").unwrap();
        assert_eq!(reach_local(&dir), Ok(Reached { loc: Location::Local(dir.clone()), reveal: None }));
        assert_eq!(reach_local(&dir.join("notas.txt")), Ok(Reached { loc: Location::Local(dir.clone()), reveal: Some(("notas.txt".into(), "notas.txt".into())) }));
        assert!(reach_local(&dir.join("não existe")).unwrap_err().starts_with("Não encontrei"));
    }
}
