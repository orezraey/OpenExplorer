//! The file list's keyboard beyond the arrows: typing a name's start to reach it (typeahead),
//! and paging by the rows the pane shows. [`crate::key`] routes the keys here.

use crate::model::{Entry, shown};
use crate::{State, pane_key};

/// How long a pause ends a typed name: the next key starts a new one.
pub const PAUSE: f64 = 1.0;

/// Rows a page moves before the pane was first laid out.
const PAGE_BEFORE_LAYOUT: usize = 20;

/// What was typed in a pane's list, and when the last key came.
#[derive(Default)]
pub struct Typeahead {
    pane: usize,
    query: String,
    at: f64,
}

impl Typeahead {
    /// Forgets the name typed, so a key after another one (Enter into a folder, an arrow)
    /// starts a new one.
    pub fn clear(&mut self) {
        self.query.clear();
    }

    /// Adds `typed` to what was typed in pane `p`, after a pause or in another pane starting
    /// over, and gives the name to look for. A space can't start one: alone it's no name's
    /// start, and Space keeps its own job.
    pub fn push(&mut self, p: usize, typed: &str, now: f64) -> Option<&str> {
        if self.pane != p || now - self.at > PAUSE {
            self.query.clear();
        }
        if self.query.is_empty() && typed.trim().is_empty() {
            return None;
        }
        self.pane = p;
        self.at = now;
        self.query.push_str(typed);
        Some(&self.query)
    }
}

/// The entry `query` reaches in `entries`, in the order shown, from the cursor at `from`.
/// Names are compared as the search compares them, without case or accents. One letter, or
/// the same letter pressed again and again, goes to the next name starting with it after the
/// cursor, coming round to the top; a longer start may be the cursor's own name, so typing on
/// ("d", "do", "doc") stays on "Documentos" while it still fits.
pub fn find(entries: &[Entry], from: Option<usize>, query: &str) -> Option<usize> {
    find_by(entries.len(), from, query, |i, out| openexplorer_index::fold::fold_into(&shown(&entries[i].name), out))
}

/// [`find`] over any `n` names (the tree's rows too), `name` folding the `i`th into its buffer.
pub fn find_by(n: usize, from: Option<usize>, query: &str, mut name: impl FnMut(usize, &mut Vec<u8>)) -> Option<usize> {
    let folded = String::from_utf8(openexplorer_index::fold::fold(query)).ok()?;
    let mut chars = folded.chars();
    let first = chars.next()?;
    let repeated = chars.all(|c| c == first);
    let mut one = [0u8; 4];
    let (want, start) = if repeated {
        (first.encode_utf8(&mut one).as_bytes(), from.map_or(0, |i| i + 1))
    } else {
        (folded.as_bytes(), from.unwrap_or(0))
    };
    let mut buf = Vec::new();
    (0..n).map(|k| (start + k) % n).find(|&i| {
        buf.clear();
        name(i, &mut buf);
        buf.starts_with(want)
    })
}

/// How far Page Up and Page Down move with `rows` rows in view: a page less one, so the row
/// left stays in sight, as Explorer's list does.
pub fn page(rows: usize) -> usize {
    match rows {
        0 => PAGE_BEFORE_LAYOUT,
        r => r.saturating_sub(1).max(1),
    }
}

/// Whether pane `p`'s list has the keyboard: not while a name is being edited in place, the
/// address typed or the search field focused, which own their keys, nor while another control
/// in the pane (a button, the tree) has focus. `focused` is [`rune::Tx::focused`].
pub fn list_has_keys(s: &State, p: usize, focused: Option<&str>) -> bool {
    s.renaming.is_none()
        && !s.search[p].focused
        && s.address[p].as_ref().is_none_or(|a| !a.focused)
        && focused.is_none_or(|f| f == pane_key(p) || f.starts_with("r:"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Location, Pane};
    use crate::{Mods, init, key};
    use rune::{Key, KeyEvent, Tx};

    fn entry(name: &str) -> Entry {
        Entry { id: name.to_string(), name: name.to_string(), is_dir: false, size: Some(1), modified: None, target: None, mime: None, md5: None, starred: false }
    }

    fn names(list: &[&str]) -> Vec<Entry> {
        list.iter().map(|n| entry(n)).collect()
    }

    /// A state whose left pane lists `list` (sorted by name), none of it selected.
    fn state(list: &[&str]) -> State {
        let mut s = init();
        s.panes[0] = Pane::new(Location::Mock(Vec::new()));
        s.panes[0].set_entries(Ok(names(list)));
        s
    }

    fn press(s: &mut State, k: Key, mods: Mods) -> bool {
        key(s, 0, &KeyEvent { key: k, mods }, &mut Tx::default())
    }

    fn typed(s: &mut State, t: &str) -> bool {
        press(s, Key::Text(t.into()), Mods::default())
    }

    fn cursor(s: &State) -> Option<&str> {
        s.panes[0].cursor.as_deref()
    }

    fn selected(s: &State) -> Vec<&str> {
        let pane = &s.panes[0];
        pane.entries.iter().filter(|e| pane.selected.contains(&e.id)).map(|e| e.id.as_str()).collect()
    }

    fn ctrl() -> Mods {
        Mods { cmd: true, ctrl: true, ..Mods::default() }
    }

    /// One letter goes to the next name starting with it, from after the cursor and coming
    /// round; a longer start may stay on the cursor's own name; case and accents don't count.
    #[test]
    fn typing_finds_names_by_their_start() {
        let list = names(&["Área de trabalho", "agenda.txt", "Documentos", "downloads", "Música"]);
        assert_eq!(find(&list, None, "a"), Some(0));
        assert_eq!(find(&list, Some(0), "a"), Some(1));
        assert_eq!(find(&list, Some(1), "a"), Some(0));
        // The same letter again is still one letter.
        assert_eq!(find(&list, Some(2), "dd"), Some(3));
        assert_eq!(find(&list, Some(2), "do"), Some(2));
        assert_eq!(find(&list, Some(2), "dow"), Some(3));
        assert_eq!(find(&list, None, "MUSI"), Some(4));
        assert_eq!(find(&list, None, "x"), None);
        assert_eq!(find(&[], None, "a"), None);
        assert_eq!(find(&list, None, ""), None);
    }

    /// Keys within a second make one name; after a pause, or in the other pane, a new one; a
    /// space can't start one.
    #[test]
    fn keys_close_together_make_one_name() {
        let mut t = Typeahead::default();
        assert_eq!(t.push(0, "d", 1.0), Some("d"));
        assert_eq!(t.push(0, "o", 1.5), Some("do"));
        assert_eq!(t.push(0, " ", 2.0), Some("do "));
        assert_eq!(t.push(0, "m", 3.5), Some("m"));
        assert_eq!(t.push(1, "a", 3.6), Some("a"));
        assert_eq!(t.push(1, " ", 9.0), None);
        t.clear();
        assert_eq!(t.push(1, "b", 9.1), Some("b"));
    }

    /// Typed in the list: "do" reaches Documentos and selects it alone; d again goes on to
    /// the next name with d; Shift (a capital) doesn't select a range.
    #[test]
    fn typing_in_the_list_moves_the_cursor_and_selection() {
        let mut s = state(&["agenda.txt", "Documentos", "downloads", "Música"]);
        s.now = 10.0;
        assert!(typed(&mut s, "d"));
        assert_eq!(cursor(&s), Some("Documentos"));
        s.now = 10.3;
        typed(&mut s, "o");
        assert_eq!(cursor(&s), Some("Documentos"));
        s.now = 10.6;
        typed(&mut s, "w");
        assert_eq!(cursor(&s), Some("downloads"));
        assert_eq!(selected(&s), ["downloads"]);
        // After a pause, d starts over and goes on from the cursor.
        s.now = 12.0;
        typed(&mut s, "d");
        assert_eq!(cursor(&s), Some("Documentos"));
        s.now = 14.0;
        assert!(press(&mut s, Key::Text("M".into()), Mods { shift: true, ..Mods::default() }));
        assert_eq!(selected(&s), ["Música"]);
        // Another key ends the name: "a" right after Home starts one, it doesn't add to "m".
        press(&mut s, Key::Home, Mods::default());
        s.now = 14.2;
        typed(&mut s, "a");
        assert_eq!(cursor(&s), Some("agenda.txt"));
    }

    /// The name field, the address and the search field own their keys; so does another
    /// control with focus.
    #[test]
    fn typing_elsewhere_leaves_the_list_alone() {
        let mut s = state(&["agenda.txt", "Documentos"]);
        s.search[0].focused = true;
        assert!(!typed(&mut s, "d"));
        s.search[0].focused = false;
        let mut field = rune::TextEdit::new("");
        field.focused = true;
        s.address[0] = Some(field);
        assert!(!typed(&mut s, "d"));
        s.address[0] = None;
        assert!(!list_has_keys(&s, 0, Some("back-0")));
        assert!(list_has_keys(&s, 0, Some("pane-0")));
        assert_eq!(cursor(&s), None);
    }

    /// A page is the rows in view less one; before the first layout, 20.
    #[test]
    fn pages_follow_the_rows_in_view() {
        let list: Vec<String> = (0..100).map(|i| format!("f{i:03}")).collect();
        let mut s = state(&list.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(page(0), 20);
        assert_eq!(page(1), 1);
        s.page_rows[0] = 11;
        press(&mut s, Key::Home, Mods::default());
        press(&mut s, Key::PageDown, Mods::default());
        assert_eq!(cursor(&s), Some("f010"));
        press(&mut s, Key::PageDown, Mods::default());
        assert_eq!(cursor(&s), Some("f020"));
        press(&mut s, Key::PageUp, Mods::default());
        assert_eq!(cursor(&s), Some("f010"));
    }

    /// Esc empties the selection and keeps the cursor; Ctrl with an arrow moves only the
    /// cursor, Ctrl+Space then adds or takes away the item under it.
    #[test]
    fn esc_and_ctrl_keys_work_the_selection_as_explorer_does() {
        let mut s = state(&["a", "b", "c", "d"]);
        press(&mut s, Key::Down, Mods::default());
        assert_eq!(selected(&s), ["a"]);
        assert!(press(&mut s, Key::Down, ctrl()));
        press(&mut s, Key::Down, ctrl());
        assert_eq!((cursor(&s), selected(&s)), (Some("c"), vec!["a"]));
        assert!(press(&mut s, Key::Text(" ".into()), ctrl()));
        assert_eq!(selected(&s), ["a", "c"]);
        assert!(press(&mut s, Key::Command(' '), ctrl()));
        assert_eq!(selected(&s), ["a"]);
        press(&mut s, Key::Command(' '), ctrl());
        // Shift still reaches from the anchor, the item Ctrl+Space last picked.
        press(&mut s, Key::Down, Mods { shift: true, ..Mods::default() });
        assert_eq!(selected(&s), ["c", "d"]);
        assert!(press(&mut s, Key::Escape, Mods::default()));
        assert_eq!((cursor(&s), selected(&s)), (Some("d"), vec![]));
        // Nothing left to clear: Esc is someone else's.
        assert!(!press(&mut s, Key::Escape, Mods::default()));
    }

    /// The cursor's item gone, the cursor goes to the item now at its place, or the last one,
    /// and the selection with it when it was that item; a listing without it leaves nothing.
    #[test]
    fn the_cursor_recovers_when_its_item_goes() {
        let mut s = state(&["a", "b", "c", "d"]);
        let pane = &mut s.panes[0];
        pane.cursor = Some("b".into());
        pane.selected = ["b".to_string()].into();
        pane.anchor = Some("b".into());
        pane.set_entries(Ok(names(&["d", "c", "a"])));
        assert_eq!((pane.cursor.as_deref(), pane.anchor.as_deref()), (Some("c"), Some("c")));
        assert!(pane.selected.contains("c") && pane.selected.len() == 1);
        // The last item goes: the one before it, now last.
        pane.cursor = Some("d".into());
        pane.selected.clear();
        pane.set_entries(Ok(names(&["a", "c"])));
        assert_eq!(pane.cursor.as_deref(), Some("c"));
        // It wasn't selected: the selection stays empty.
        assert!(pane.selected.is_empty());
        // Others still selected keep the selection as it is.
        pane.selected = ["a".to_string(), "c".to_string()].into();
        pane.set_entries(Ok(names(&["a", "b"])));
        assert_eq!(pane.cursor.as_deref(), Some("b"));
        assert_eq!(pane.selected.iter().collect::<Vec<_>>(), ["a"]);
        pane.set_entries(Ok(Vec::new()));
        assert_eq!((pane.cursor.as_deref(), pane.anchor.as_deref()), (None, None));
        // The next arrow goes on from the recovered cursor, not from the top.
        let mut s = state(&["a", "b", "c", "d", "e"]);
        s.panes[0].cursor = Some("c".into());
        s.panes[0].set_entries(Ok(names(&["a", "b", "d", "e"])));
        assert_eq!(cursor(&s), Some("d"));
        press(&mut s, Key::Down, Mods::default());
        assert_eq!(cursor(&s), Some("e"));
    }

    /// A listing in pages: the cursor's item may come on a later page, so the cursor waits
    /// for the last one before moving.
    #[test]
    fn the_cursor_waits_for_the_last_page() {
        use crate::{Msg, Part};
        use std::sync::atomic::Ordering;
        let mut s = state(&["a", "b", "c", "d"]);
        s.cache = crate::cache::Cache::memory_only();
        s.panes[0].cursor = Some("c".into());
        s.panes[0].selected = ["c".to_string()].into();
        let request = s.live[0].load(Ordering::Relaxed);
        let page = |s: &mut State, list: &[&str], first: bool, more: bool| {
            s.outbox.send(Msg::Listed { pane: 0, request, part: Part::Page { first, more }, result: Ok(names(list)) }).unwrap();
            crate::poll(s, 0.0, &mut Tx::default());
        };
        page(&mut s, &["a", "b"], true, true);
        assert_eq!((cursor(&s), selected(&s)), (Some("c"), vec![]));
        page(&mut s, &["c", "d"], false, false);
        assert_eq!((cursor(&s), selected(&s)), (Some("c"), vec!["c"]));
        // Gone for good: moved to its place once the last page came.
        page(&mut s, &["a"], true, true);
        page(&mut s, &["b", "d"], false, false);
        assert_eq!((cursor(&s), selected(&s)), (Some("d"), vec!["d"]));
    }
}
