//! The addresses reached and the searches run, brought back in the address field and the search
//! fields with ↑ and ↓ as a terminal brings back its commands: ↑ goes to earlier ones, newest
//! first, and ↓ past the newest gives back what was being typed. Edits to an entry brought back
//! stay while the others are visited. Kept in `history.json` under the config folder.

use std::collections::HashMap;
use std::path::PathBuf;

use rune::{Key, KeyEvent};
use serde::{Deserialize, Serialize};

use crate::State;

/// How many entries each list keeps; older ones go.
pub const CAP: usize = 50;

/// What's kept on disk: each list oldest first.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    #[serde(default)]
    pub address: Vec<String>,
    #[serde(default)]
    pub search: Vec<String>,
}

/// Adds `text` as the newest of `list`: not when blank, and an earlier copy moves up rather than
/// showing twice. Whether the list changed.
pub fn add(list: &mut Vec<String>, text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() || list.last().is_some_and(|l| l == text) {
        return false;
    }
    list.retain(|l| l != text);
    list.push(text.to_string());
    let over = list.len().saturating_sub(CAP);
    list.drain(..over);
    true
}

/// Where ↑ and ↓ have gone in a list, for one field.
#[derive(Debug, Default)]
pub struct Recall {
    /// The entry shown, counted from the newest (0); `None` while the field holds what was typed.
    at: Option<usize>,
    /// What was typed before ↑ first went back.
    draft: String,
    /// Entries changed while shown, by `at`, kept for coming back to them.
    edits: HashMap<usize, String>,
}

impl Recall {
    /// Starts again from what's typed: the field opened afresh, or the list changed.
    pub fn reset(&mut self) {
        *self = Recall::default();
    }

    /// ↑ (`back`) or ↓ in `entries` (oldest first), the field holding `current`: the text the field
    /// takes, or `None` when there's nothing further that way.
    pub fn step(&mut self, entries: &[String], current: &str, back: bool) -> Option<String> {
        let n = entries.len();
        let next = match (self.at, back) {
            (None, true) if n > 0 => Some(0),
            (Some(i), true) if i + 1 < n => Some(i + 1),
            (Some(0), false) => None,
            (Some(i), false) => Some(i - 1),
            _ => return None,
        };
        match self.at {
            None => self.draft = current.to_string(),
            Some(i) => {
                self.edits.insert(i, current.to_string());
            }
        }
        self.at = next;
        Some(match next {
            None => self.draft.clone(),
            Some(i) => self.edits.get(&i).cloned().unwrap_or_else(|| entries[n - 1 - i].clone()),
        })
    }
}

#[derive(Default)]
pub struct History {
    pub saved: Saved,
    /// Where it's saved; `None` (tests) saves nothing.
    file: Option<PathBuf>,
    /// Each pane's address field and search field.
    pub address: [Recall; crate::layout::MAX_PANES],
    pub search: [Recall; crate::layout::MAX_PANES],
    /// Per pane: an address typed whose place is being looked up, with the pane's `moves` then.
    /// It joins the list once the pane gets there.
    pending: [Option<(u64, String)>; crate::layout::MAX_PANES],
}

impl History {
    fn at(file: Option<PathBuf>) -> History {
        let saved = file.as_ref().and_then(|f| std::fs::read(f).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        History { saved, file, address: Default::default(), search: Default::default(), pending: Default::default() }
    }

    /// The lists saved; in tests, empty ones that are never written.
    pub fn load() -> History {
        History::at((!cfg!(test)).then(|| openexplorer_gdrive::config_dir().join("history.json")))
    }

    fn save(&self) {
        let Some(file) = &self.file else { return };
        let Ok(text) = serde_json::to_vec_pretty(&self.saved) else { return };
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(file, text);
    }

    /// An address the pane got to.
    pub fn add_address(&mut self, text: &str) {
        if add(&mut self.saved.address, text) {
            self.address.iter_mut().for_each(Recall::reset);
            self.save();
        }
    }

    /// Words searched for.
    pub fn add_search(&mut self, text: &str) {
        if add(&mut self.saved.search, text) {
            self.search.iter_mut().for_each(Recall::reset);
            self.save();
        }
    }

    /// Pane `p` looks for the place `typed` names, its `moves` now.
    pub fn asked(&mut self, p: usize, moves: u64, typed: &str) {
        self.pending[p] = Some((moves, typed.to_string()));
    }

    /// The look-up pane `p` started at `moves` ended, there (`ok`) or not: an address typed then
    /// joins the list once it led somewhere.
    pub fn reached(&mut self, p: usize, moves: u64, ok: bool) {
        if let Some((m, typed)) = self.pending[p].take()
            && m == moves
            && ok
        {
            self.add_address(&typed);
        }
    }
}

/// Whether `ev` is a plain ↑ or ↓, and which (`true` for ↑).
fn arrow(ev: &KeyEvent) -> Option<bool> {
    let m = ev.mods;
    if m.shift || m.cmd || m.ctrl || m.alt || m.logo {
        return None;
    }
    match ev.key {
        Key::Up => Some(true),
        Key::Down => Some(false),
        _ => None,
    }
}

/// ↑ or ↓ in pane `p`'s address field: an earlier address, or back to what was typed.
pub fn address_key(s: &mut State, p: usize, ev: &KeyEvent) -> bool {
    let Some(back) = arrow(ev) else { return false };
    let (Some(edit), history) = (s.address[p].as_mut(), &mut s.history) else { return false };
    if edit.is_composing() {
        return false;
    }
    let Some(text) = history.address[p].step(&history.saved.address, &edit.text, back) else { return false };
    edit.replace_text(text);
    true
}

/// ↑ or ↓ in pane `p`'s search field: earlier words searched for, or back to what was typed.
pub fn search_key(s: &mut State, p: usize, ev: &KeyEvent) -> bool {
    let Some(back) = arrow(ev) else { return false };
    let (edit, history) = (&mut s.search[p], &mut s.history);
    if edit.is_composing() {
        return false;
    }
    let Some(text) = history.search[p].step(&history.saved.search, &edit.text, back) else { return false };
    edit.replace_text(text);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// Blank entries and repeats don't count; a repeat from further back moves up; only the
    /// newest `CAP` stay.
    #[test]
    fn entries_are_added_newest_last_without_repeats() {
        let mut l = Vec::new();
        assert!(add(&mut l, "C:\\Users"));
        assert!(!add(&mut l, "  C:\\Users "));
        assert!(!add(&mut l, "   "));
        assert!(add(&mut l, "D:\\"));
        assert!(add(&mut l, "C:\\Users"));
        assert_eq!(l, list(&["D:\\", "C:\\Users"]));
        for i in 0..CAP + 10 {
            add(&mut l, &format!("pasta {i}"));
        }
        assert_eq!(l.len(), CAP);
        assert_eq!(l.last().unwrap(), &format!("pasta {}", CAP + 9));
        assert_eq!(l[0], "pasta 10");
    }

    /// ↑ goes back newest first and stops at the oldest; ↓ comes forward to what was typed;
    /// changes to an entry shown stay while going through the others.
    #[test]
    fn up_and_down_go_through_the_entries_like_a_terminal() {
        let entries = list(&["um", "dois", "três"]);
        let mut r = Recall::default();
        assert_eq!(r.step(&entries, "digit", false), None, "nothing past what's typed");
        assert_eq!(r.step(&entries, "digit", true).as_deref(), Some("três"));
        assert_eq!(r.step(&entries, "três", true).as_deref(), Some("dois"));
        // An edit to "dois" stays when coming back to it.
        assert_eq!(r.step(&entries, "dois!", true).as_deref(), Some("um"));
        assert_eq!(r.step(&entries, "um", true), None, "the oldest is the end");
        assert_eq!(r.step(&entries, "um", false).as_deref(), Some("dois!"));
        assert_eq!(r.step(&entries, "dois!", false).as_deref(), Some("três"));
        assert_eq!(r.step(&entries, "três", false).as_deref(), Some("digit"));
        assert_eq!(r.step(&entries, "digit", false), None);
        // Nothing kept: ↑ does nothing.
        let mut r = Recall::default();
        assert_eq!(r.step(&[], "x", true), None);
    }

    /// An address joins the list once its pane got there, not when the look-up failed or the
    /// pane went elsewhere meanwhile; the lists are saved and read back.
    #[test]
    fn addresses_are_kept_once_reached_and_saved() {
        let file = std::env::temp_dir().join("openexplorer-history-test").join("history.json");
        let _ = std::fs::remove_file(&file);
        let mut h = History::at(Some(file.clone()));
        h.asked(0, 3, "C:\\Windows");
        h.reached(0, 3, false);
        h.asked(0, 4, "X:\\não existe");
        h.reached(0, 5, true);
        assert!(h.saved.address.is_empty());
        h.asked(1, 7, " C:\\Windows ");
        h.reached(1, 7, true);
        // Reaching it once more (Mostrar na pasta) adds nothing.
        h.reached(1, 7, true);
        h.add_search("relatório");
        assert_eq!(h.saved.address, list(&["C:\\Windows"]));
        let again = History::at(Some(file));
        assert_eq!(again.saved, Saved { address: list(&["C:\\Windows"]), search: list(&["relatório"]) });
        // Tests save nothing of the user's.
        assert!(History::load().file.is_none());
    }

    /// ↑ in a field brings back the newest entry with the caret at its end, and ↓ what was typed;
    /// keys with a modifier go to the field.
    #[test]
    fn the_fields_bring_entries_back() {
        let mut s = crate::init();
        s.history.add_search("praia");
        s.history.add_search("relatório");
        s.search[0] = rune::TextEdit::new("fér");
        let up = KeyEvent { key: Key::Up, mods: Default::default() };
        let down = KeyEvent { key: Key::Down, mods: Default::default() };
        assert!(search_key(&mut s, 0, &up));
        assert_eq!(s.search[0].text, "relatório");
        assert_eq!(s.search[0].range(), ("relatório".len(), "relatório".len()));
        assert!(search_key(&mut s, 0, &up));
        assert_eq!(s.search[0].text, "praia");
        assert!(search_key(&mut s, 0, &down) && search_key(&mut s, 0, &down));
        assert_eq!(s.search[0].text, "fér");
        let shifted = KeyEvent { key: Key::Up, mods: rune::Mods { shift: true, ..Default::default() } };
        assert!(!search_key(&mut s, 0, &shifted));

        s.history.add_address("C:\\Users");
        assert!(!address_key(&mut s, 1, &up), "no field open");
        crate::actions::edit_address(&mut s, 1);
        let typed = s.address[1].as_ref().unwrap().text.clone();
        assert!(address_key(&mut s, 1, &up));
        let field = s.address[1].as_ref().unwrap();
        assert_eq!(field.text, "C:\\Users");
        assert!(field.focused, "the field keeps the keyboard");
        assert!(address_key(&mut s, 1, &down));
        assert_eq!(s.address[1].as_ref().unwrap().text, typed);
    }
}
