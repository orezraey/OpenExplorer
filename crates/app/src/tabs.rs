//! Per-pane browser-style tabs and the "+" new-tab picker (quick local folders,
//! registered cloud accounts & servers, and "Registrar conta").
//!
//! Each side (`p` in `0..2`) keeps its currently active tab's [`Pane`] live in
//! `State::panes[p]` so every existing operation, shortcut, drag/drop and listing
//! handler works on `s.panes[p]` unchanged. Inactive tabs hold their own [`Pane`]
//! (preserving location, entries, back/forward history, selection and sort) here.
//! When `active` is `None`, the "+" new-tab picker is shown for that pane and all
//! open tabs live in `items`.

use std::sync::atomic::Ordering;

use openexplorer_i18n::t;
use rune::Icon;

use crate::icons;
use crate::model::{Location, Pane};
use crate::servers::Provider;
use crate::sysicons::IconKey;
use crate::State;

/// One open tab inside a pane.
pub struct TabSlot {
    pub id: u64,
    /// `None` while this tab is active (its `Pane` lives in `s.panes[p]`).
    /// `Some(pane)` while this tab is inactive or while the "+" picker is active.
    pub pane: Option<Pane>,
    /// The search box text saved when leaving this tab.
    pub search: String,
    /// What that text last searched (see `State::searched`), so coming back doesn't search
    /// again, and leaving doesn't search the next tab with this text.
    pub searched: String,
}

/// State of the "+" new-tab picker inside a pane.
pub struct PickerState {
    pub filter: Option<Provider>,
    pub filter_open: bool,
    pub search: rune::TextEdit,
    pub accounts_open: bool,
}

impl Default for PickerState {
    fn default() -> Self {
        Self {
            filter: None,
            filter_open: false,
            search: rune::TextEdit::new(""),
            accounts_open: true,
        }
    }
}

/// All tabs belonging to one side (left = 0, right = 1).
pub struct PaneTabs {
    pub items: Vec<TabSlot>,
    /// Index into `items` of the active explorer tab, or `None` when the "+"
    /// new-tab picker is active.
    pub active: Option<usize>,
    /// The tab that was active right before opening the "+" picker, so Escape
    /// can return to it if it still exists.
    pub last_active: Option<usize>,
    pub picker: PickerState,
    next_id: u64,
}

impl PaneTabs {
    /// Starts with a single active tab whose `Pane` lives in `s.panes[p]`.
    pub fn initial() -> Self {
        Self {
            items: vec![TabSlot { id: 1, pane: None, search: String::new(), searched: String::new() }],
            active: Some(0),
            last_active: Some(0),
            picker: PickerState::default(),
            next_id: 2,
        }
    }

    pub fn is_picker(&self) -> bool {
        self.active.is_none()
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// The [`Location`] of tab `idx` in pane `p`.
    pub fn location_of<'a>(&'a self, live: &'a Pane, idx: usize) -> &'a Location {
        match self.items[idx].pane.as_ref() {
            Some(pane) => &pane.loc,
            None => &live.loc,
        }
    }
}

/// What a tab shows in the tab strip: optional Windows shell icon key, fallback vector icon,
/// and display label, derived from what is open in that tab.
pub fn tab_header(s: &State, loc: &Location) -> (Option<IconKey>, &'static Icon, String) {
    match loc {
        // A folder on the computer by its name and icon as the tree shows it ("Documentos",
        // "Disco Local (C:)"); a search by the folder it looks in.
        Location::Local(_) | Location::Found(_) => match crate::tree::node_of(s, loc) {
            Some(node) => (node.icon, node.fallback, node.label),
            None => (Some(IconKey::Computer), &icons::COMPUTER, t("Este Computador", "This PC").into()),
        },
        Location::Mock(_) => (None, &icons::LAB, t("Drive simulado", "Simulated drive").into()),
        Location::Drive { email, .. } => {
            let label = s
                .accounts
                .iter()
                .find(|a| a.email.eq_ignore_ascii_case(email))
                .map(|a| a.name.trim())
                .filter(|n| !n.is_empty())
                .unwrap_or(email.as_str())
                .to_string();
            (None, &icons::PROVIDER_GDRIVE, label)
        }
        Location::Server { server, .. } => {
            let provider = Provider::of_server(server);
            let label = match provider {
                Provider::OneDrive => s
                    .servers
                    .onedrive
                    .accounts
                    .iter()
                    .find(|a| a.id == server.user)
                    .map(|a| {
                        let n = a.name.trim();
                        if !n.is_empty() { n } else { a.email.as_str() }
                    })
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| if !server.name.trim().is_empty() { server.name.as_str() } else { "OneDrive" })
                    .to_string(),
                Provider::Dropbox => s
                    .servers
                    .dropbox_accounts
                    .iter()
                    .find(|a| a.email.eq_ignore_ascii_case(&server.user))
                    .map(|a| {
                        let n = a.name.trim();
                        if !n.is_empty() { n } else { a.email.as_str() }
                    })
                    .filter(|n| !n.is_empty())
                    .unwrap_or(if !server.user.is_empty() { server.user.as_str() } else { server.name.as_str() })
                    .to_string(),
                _ => {
                    if !server.name.trim().is_empty() {
                        server.name.clone()
                    } else if !server.user.is_empty() {
                        server.user.clone()
                    } else if !server.host.is_empty() {
                        server.host.clone()
                    } else {
                        provider.label().to_string()
                    }
                }
            };
            (None, provider.icon(), label)
        }
    }
}

/// Clears transient per-pane UI states (in-place rename, address edit, open filter menus)
/// before switching tabs on pane `p`.
fn reset_transient(s: &mut State, p: usize) {
    if s.renaming.as_ref().is_some_and(|(rp, _)| *rp == p) {
        s.renaming = None;
    }
    s.address[p] = None;
    s.tabs[p].picker.filter_open = false;
    s.live[p].fetch_add(1, Ordering::Relaxed);
}

/// Stashes the currently active `s.panes[p]` back into its `TabSlot`.
fn stash_active(s: &mut State, p: usize) {
    if let Some(cur) = s.tabs[p].active {
        let placeholder = Pane::new(Location::Mock(Vec::new()));
        let live = std::mem::replace(&mut s.panes[p], placeholder);
        let search = std::mem::take(&mut s.search[p].text);
        let searched = std::mem::take(&mut s.searched[p]);
        set_search(s, p, String::new(), String::new());
        if let Some(slot) = s.tabs[p].items.get_mut(cur) {
            slot.pane = Some(live);
            slot.search = search;
            slot.searched = searched;
        }
        s.tabs[p].last_active = Some(cur);
        s.tabs[p].active = None;
    }
}

/// Puts `text` in pane `p`'s search box, with `searched` as what it last searched. Text already
/// searched starts nothing; text typed but not searched yet (the tab was left within the typing
/// pause) is due at once, and `poll` runs it at the start of the next frame.
fn set_search(s: &mut State, p: usize, text: String, searched: String) {
    let due = if text != searched { f64::MIN_POSITIVE } else { 0.0 };
    s.typed[p] = (text.clone(), due);
    s.searched[p] = searched;
    s.found_again[p] = 0.0;
    s.search[p] = rune::TextEdit::new(text);
}

/// Takes tab `idx`'s pane and search state into `s.panes[p]` and makes it the active tab.
fn bring_in(s: &mut State, p: usize, idx: usize) {
    let (pane, search, searched) = {
        let slot = &mut s.tabs[p].items[idx];
        (slot.pane.take(), std::mem::take(&mut slot.search), std::mem::take(&mut slot.searched))
    };
    if let Some(pane) = pane {
        s.panes[p] = pane;
    }
    set_search(s, p, search, searched);
    s.tabs[p].active = Some(idx);
    s.tabs[p].last_active = Some(idx);
    s.icons_for[p] = u64::MAX;
    // If the tab was still loading when left, kick off a fresh listing.
    if s.panes[p].loading {
        crate::list(s, p, false);
    }
    crate::tree::follow(s, p);
}

/// Switches pane `p` to the existing tab at `idx`.
pub fn select_tab(s: &mut State, p: usize, idx: usize) {
    s.active = p;
    if idx >= s.tabs[p].items.len() {
        return;
    }
    if s.tabs[p].active == Some(idx) {
        s.tabs[p].picker.filter_open = false;
        return;
    }
    reset_transient(s, p);
    stash_active(s, p);
    bring_in(s, p, idx);
}

/// Opens the "+" new-tab picker on pane `p`.
pub fn open_picker(s: &mut State, p: usize) {
    s.active = p;
    if s.tabs[p].is_picker() {
        return;
    }
    reset_transient(s, p);
    stash_active(s, p);
    s.tabs[p].picker.filter_open = false;
    s.tabs[p].picker.search = rune::TextEdit::new("");
}

/// Returns from the "+" picker to the previously active tab (if any).
pub fn cancel_picker(s: &mut State, p: usize) {
    if !s.tabs[p].is_picker() {
        return;
    }
    if let Some(idx) = s.tabs[p].last_active.filter(|&i| i < s.tabs[p].items.len()) {
        select_tab(s, p, idx);
    } else if !s.tabs[p].items.is_empty() {
        select_tab(s, p, 0);
    }
}

/// Opens `loc` in a brand-new tab on pane `p` and makes it active.
pub fn open_new_tab(s: &mut State, p: usize, loc: Location) {
    s.active = p;
    reset_transient(s, p);
    stash_active(s, p);
    let id = s.tabs[p].alloc_id();
    let idx = s.tabs[p].items.len();
    s.tabs[p].items.push(TabSlot { id, pane: None, search: String::new(), searched: String::new() });
    s.tabs[p].active = Some(idx);
    s.tabs[p].last_active = Some(idx);
    s.tabs[p].picker.filter_open = false;
    s.tabs[p].picker.search = rune::TextEdit::new("");
    s.panes[p] = Pane::new(loc);
    set_search(s, p, String::new(), String::new());
    s.icons_for[p] = u64::MAX;
    crate::list(s, p, false);
    crate::tree::follow(s, p);
}

/// Closes tab `idx` on pane `p`.
/// If it was the last tab, the pane switches to the "+" new-tab picker.
pub fn close_tab(s: &mut State, p: usize, idx: usize) {
    s.active = p;
    if idx >= s.tabs[p].items.len() {
        return;
    }
    let was_active = s.tabs[p].active == Some(idx);
    if was_active {
        reset_transient(s, p);
        s.tabs[p].items.remove(idx);
        s.tabs[p].active = None;
        if s.tabs[p].items.is_empty() {
            s.tabs[p].last_active = None;
            s.panes[p] = Pane::new(Location::Mock(Vec::new()));
            set_search(s, p, String::new(), String::new());
            return;
        }
        let next = idx.min(s.tabs[p].items.len() - 1);
        bring_in(s, p, next);
    } else {
        s.tabs[p].items.remove(idx);
        if let Some(active) = s.tabs[p].active
            && idx < active
        {
            s.tabs[p].active = Some(active - 1);
        }
        if let Some(last) = s.tabs[p].last_active {
            if last == idx {
                s.tabs[p].last_active = s.tabs[p].items.len().checked_sub(1).map(|m| idx.min(m));
            } else if idx < last {
                s.tabs[p].last_active = Some(last - 1);
            }
        }
    }
}

/// Closes the currently active tab on pane `p` (for `Ctrl+W`).
pub fn close_active_tab(s: &mut State, p: usize) {
    if let Some(idx) = s.tabs[p].active {
        close_tab(s, p, idx);
    }
}

/// Cleans up inactive tabs whose location or history references a removed account or server.
pub fn purge_locations(s: &mut State, gone: impl Fn(&Location) -> bool, fallback: &Location) {
    for p in 0..2 {
        for slot in &mut s.tabs[p].items {
            if let Some(pane) = &mut slot.pane {
                pane.back.retain(|l| !gone(l));
                pane.forward.retain(|l| !gone(l));
                if gone(&pane.loc) {
                    pane.go(fallback.clone());
                    pane.loading = true;
                }
            }
        }
    }
}

/// Where `loc` goes when folder `from` is renamed to `to`, or deleted (`to` is `None`: up to the
/// folder `from` was in). `None` when `loc` isn't `from` or a folder inside it.
pub fn moved(loc: &Location, from: &Location, to: Option<&Location>) -> Option<Location> {
    match (loc, from) {
        (Location::Local(at), Location::Local(f)) => {
            let rest = at.strip_prefix(f).ok()?;
            match to {
                Some(Location::Local(t)) => Some(Location::Local(t.join(rest))),
                Some(_) => None,
                None => Some(Location::Local(f.parent()?.to_path_buf())),
            }
        }
        (Location::Server { server, path, find: None }, Location::Server { server: fs, path: fp, .. }) if !fp.is_empty() && server.id == fs.id && path.starts_with(fp) => match to {
            Some(Location::Server { path: tp, .. }) => Some(Location::Server { server: server.clone(), path: tp.iter().chain(&path[fp.len()..]).cloned().collect(), find: None }),
            Some(_) => None,
            None => Some(Location::Server { server: server.clone(), path: fp[..fp.len() - 1].to_vec(), find: None }),
        },
        // Drive's folders by id: a renamed one keeps its id and changes its name.
        (Location::Drive { email, space, path }, Location::Drive { email: fe, space: fs, path: fp })
            if !fp.is_empty() && email == fe && space == fs && fp.len() <= path.len() && path.iter().zip(fp).all(|(a, b)| a.0 == b.0) =>
        {
            match to {
                Some(Location::Drive { path: tp, .. }) => Some(Location::Drive { email: email.clone(), space: space.clone(), path: tp.iter().chain(&path[fp.len()..]).cloned().collect() }),
                Some(_) => None,
                None => Some(Location::Drive { email: email.clone(), space: space.clone(), path: fp[..fp.len() - 1].to_vec() }),
            }
        }
        _ => None,
    }
}

/// Whether a pane or a tab shows folder `from` or a folder inside it.
pub fn any_inside(s: &State, from: &Location) -> bool {
    let inside = |pane: &Pane| pane.back.iter().chain(&pane.forward).chain([&pane.loc]).any(|l| moved(l, from, None).is_some());
    (0..2).any(|p| (!s.tabs[p].is_picker() && inside(&s.panes[p])) || s.tabs[p].items.iter().filter_map(|slot| slot.pane.as_ref()).any(inside))
}

/// Voltar's and Avançar's steps inside folder `from` follow it to `to`, or go when it's
/// deleted (`to` is `None`), so neither leads to a folder that's gone.
fn remap_history(pane: &mut Pane, from: &Location, to: Option<&Location>) {
    for steps in [&mut pane.back, &mut pane.forward] {
        match to {
            Some(_) => {
                for l in steps.iter_mut() {
                    if let Some(new) = moved(l, from, to) {
                        *l = new;
                    }
                }
            }
            None => steps.retain(|l| moved(l, from, None).is_none()),
        }
    }
}

/// Moves the panes and tabs inside folder `from` to where [`moved`] says, and their history
/// with them, so none shows or goes back to a folder that was renamed or deleted.
pub fn repoint(s: &mut State, from: &Location, to: Option<&Location>) {
    for p in 0..2 {
        remap_history(&mut s.panes[p], from, to);
        for pane in s.tabs[p].items.iter_mut().filter_map(|slot| slot.pane.as_mut()) {
            remap_history(pane, from, to);
        }
        if !s.tabs[p].is_picker()
            && let Some(loc) = moved(&s.panes[p].loc, from, to)
        {
            if s.renaming.as_ref().is_some_and(|(rp, _)| *rp == p) {
                s.renaming = None;
            }
            s.panes[p].go(loc);
            crate::list(s, p, false);
            crate::tree::follow(s, p);
        }
        for slot in &mut s.tabs[p].items {
            if let Some(pane) = &mut slot.pane
                && let Some(loc) = moved(&pane.loc, from, to)
            {
                pane.go(loc);
                pane.loading = true;
            }
        }
    }
}

/// Tabs left showing a folder in `reload`, which an operation changed, list it again when
/// they come back.
pub fn changed(s: &mut State, reload: &[Location]) {
    for slot in s.tabs.iter_mut().flat_map(|t| t.items.iter_mut()) {
        if let Some(pane) = &mut slot.pane
            && reload.contains(&pane.loc)
        {
            pane.loading = true;
        }
    }
}

/// Closes any open provider filter dropdown in the "+" picker.
pub fn close_picker_filter(s: &mut State) {
    for p in 0..2 {
        s.tabs[p].picker.filter_open = false;
    }
}

/// Default local location when clicking "Este Computador" in the "+" picker.
pub fn computer_location(s: &State) -> Location {
    s.places
        .desktop
        .as_ref()
        .map_or_else(Location::home, |(dir, _)| Location::Local(dir.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_switch_and_close_tabs_preserves_state() {
        let mut s = State::placeholder();
        let home_a = Location::Mock(vec!["A".into()]);
        let home_b = Location::Mock(vec!["B".into()]);
        s.panes[0] = Pane::new(home_a.clone());
        s.tabs[0] = PaneTabs::initial();

        // Opening picker stashes tab 0 and marks picker active.
        open_picker(&mut s, 0);
        assert!(s.tabs[0].is_picker());
        assert_eq!(s.tabs[0].items.len(), 1);
        assert_eq!(s.tabs[0].items[0].pane.as_ref().unwrap().loc, home_a);

        // Opening a new tab creates tab 1 and activates it.
        open_new_tab(&mut s, 0, home_b.clone());
        assert!(!s.tabs[0].is_picker());
        assert_eq!(s.tabs[0].active, Some(1));
        assert_eq!(s.tabs[0].items.len(), 2);
        assert_eq!(s.panes[0].loc, home_b);

        // Switching back to tab 0 restores home_a and stashes home_b.
        select_tab(&mut s, 0, 0);
        assert_eq!(s.tabs[0].active, Some(0));
        assert_eq!(s.panes[0].loc, home_a);
        assert_eq!(s.tabs[0].items[1].pane.as_ref().unwrap().loc, home_b);

        // Closing active tab 0 activates tab 1 (now index 0).
        close_tab(&mut s, 0, 0);
        assert_eq!(s.tabs[0].active, Some(0));
        assert_eq!(s.tabs[0].items.len(), 1);
        assert_eq!(s.panes[0].loc, home_b);

        // Closing the last tab enters the "+" picker with 0 tabs.
        close_tab(&mut s, 0, 0);
        assert!(s.tabs[0].is_picker());
        assert!(s.tabs[0].items.is_empty());
    }

    #[test]
    fn closing_inactive_tab_updates_active_index() {
        let mut s = State::placeholder();
        s.panes[0] = Pane::new(Location::Mock(vec!["0".into()]));
        s.tabs[0] = PaneTabs::initial();
        open_new_tab(&mut s, 0, Location::Mock(vec!["1".into()]));
        open_new_tab(&mut s, 0, Location::Mock(vec!["2".into()]));
        assert_eq!(s.tabs[0].active, Some(2));

        // Close tab 0 (before active): active shifts from 2 to 1.
        close_tab(&mut s, 0, 0);
        assert_eq!(s.tabs[0].active, Some(1));
        assert_eq!(s.panes[0].loc, Location::Mock(vec!["2".into()]));
    }

    #[test]
    fn a_tab_keeps_its_search_and_switching_starts_none() {
        let mut s = State::placeholder();
        s.panes[0] = Pane::new(Location::Mock(vec!["A".into()]));
        s.tabs[0] = PaneTabs::initial();
        s.search[0] = rune::TextEdit::new("fotos");
        s.typed[0] = ("fotos".into(), 0.0);
        s.searched[0] = "fotos".into();
        open_new_tab(&mut s, 0, Location::Mock(vec!["B".into()]));
        // The new tab's box is empty and already seen: nothing waits to search there.
        assert_eq!(s.search[0].text, "");
        assert_eq!(s.typed[0], (String::new(), 0.0));
        assert_eq!(s.searched[0], "");
        select_tab(&mut s, 0, 0);
        assert_eq!(s.search[0].text, "fotos");
        assert_eq!(s.typed[0], ("fotos".to_string(), 0.0));
        assert_eq!(s.searched[0], "fotos");

        // Text typed and left within the typing pause is searched once its tab is back.
        s.search[0] = rune::TextEdit::new("fotos 2024");
        select_tab(&mut s, 0, 1);
        assert_eq!(s.typed[0], (String::new(), 0.0));
        select_tab(&mut s, 0, 0);
        assert_eq!(s.search[0].text, "fotos 2024");
        assert_eq!(s.searched[0], "fotos");
        assert!(s.typed[0].0 == "fotos 2024" && s.typed[0].1 > 0.0, "due at once: {:?}", s.typed[0]);
    }

    #[test]
    fn panes_and_tabs_follow_a_folder_renamed_or_deleted() {
        let local = |p: &str| Location::Local(p.into());
        let from = local(r"C:\Fotos");
        assert_eq!(moved(&local(r"C:\Fotos\2024"), &from, Some(&local(r"C:\Imagens"))), Some(local(r"C:\Imagens\2024")));
        assert_eq!(moved(&local(r"C:\Fotos"), &from, None), Some(local(r"C:\")));
        assert_eq!(moved(&local(r"C:\Fotos 2"), &from, None), None);
        let drive = |path: &[(&str, &str)]| Location::Drive {
            email: "a@b".into(),
            space: crate::model::Space::MyDrive,
            path: path.iter().map(|(id, name)| (id.to_string(), name.to_string())).collect(),
        };
        let from = drive(&[("1", "Fotos")]);
        let inside = drive(&[("1", "Fotos"), ("2", "2024")]);
        assert_eq!(moved(&inside, &from, Some(&drive(&[("1", "Imagens")]))), Some(drive(&[("1", "Imagens"), ("2", "2024")])));
        assert_eq!(moved(&inside, &from, None), Some(drive(&[])));
        assert_eq!(moved(&drive(&[("3", "Fotos")]), &from, None), None);

        // A tab left in the folder, and the other pane in it, both move.
        let mut s = State::placeholder();
        s.tabs = [PaneTabs::initial(), PaneTabs::initial()];
        s.panes[0] = Pane::new(local(r"C:\Fotos\2024"));
        s.panes[0].back = vec![local(r"C:\"), local(r"C:\Fotos")];
        open_new_tab(&mut s, 0, Location::Mock(Vec::new()));
        s.panes[1] = Pane::new(local(r"C:\Fotos"));
        assert!(any_inside(&s, &local(r"C:\Fotos")));
        assert!(!any_inside(&s, &local(r"C:\Outra")));
        repoint(&mut s, &local(r"C:\Fotos"), Some(&local(r"C:\Imagens")));
        let left = s.tabs[0].items[0].pane.as_ref().unwrap();
        assert_eq!(left.loc, local(r"C:\Imagens\2024"));
        // Voltar follows the rename too.
        assert_eq!(left.back, vec![local(r"C:\"), local(r"C:\Imagens")]);
        assert_eq!(s.panes[1].loc, local(r"C:\Imagens"));
        assert_eq!(s.panes[0].loc, Location::Mock(Vec::new()));

        // A folder only Voltar or Avançar leads to counts, and is dropped when deleted.
        s.panes[1] = Pane::new(local(r"C:\Outra"));
        s.panes[1].back = vec![local(r"C:\"), local(r"C:\Imagens\2024")];
        s.panes[1].forward = vec![local(r"C:\Imagens")];
        assert!(any_inside(&s, &local(r"C:\Imagens\2024")));
        repoint(&mut s, &local(r"C:\Imagens"), None);
        assert_eq!(s.panes[1].loc, local(r"C:\Outra"));
        assert_eq!(s.panes[1].back, vec![local(r"C:\")]);
        assert!(s.panes[1].forward.is_empty());
        let left = s.tabs[0].items[0].pane.as_ref().unwrap();
        assert_eq!((left.loc.clone(), left.back.clone()), (local(r"C:\"), vec![local(r"C:\")]));
    }
}
