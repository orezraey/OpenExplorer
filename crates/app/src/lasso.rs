//! Rectangle selection in the file list, as Explorer has it: a drag from empty space draws a
//! translucent rectangle and selects the rows it touches while it moves; with Ctrl held it
//! toggles them against the selection from before the drag; held past the top or bottom edge
//! the list scrolls under it; Escape puts the old selection back. A click on empty space is a
//! drag that never moved: it touches nothing, so it clears the selection.
//!
//! The rows are virtual (only the ones in view are built), so hits come from row indexes and
//! [`ROW_H`], never from mounted elements.

use std::cell::Cell;
use std::collections::HashSet;
use std::ops::Range;
use std::rc::Rc;

use rune::{Drag, Phase, ScrollPos, Tx};

use crate::{ROW_H, State, pane_key};

/// The file list's column header, stuck to the top of its viewport.
pub const HEADER_H: f32 = 24.0;

/// Where a pane's list viewport scrolls to, as its `on_scroll_offset` last said. That handler
/// can't change state, so it writes here.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Scroll {
    pub x: f32,
    pub y: f32,
    pub max_y: f32,
    /// The viewport's height, header included.
    pub h: f32,
}

impl From<ScrollPos> for Scroll {
    fn from(at: ScrollPos) -> Self {
        Scroll { x: at.offset[0], y: at.offset[1], max_y: at.max[1], h: at.viewport[1] }
    }
}

/// A pane's list viewport: its rect in window points (from `on_layout`) and its scroll.
#[derive(Default)]
pub struct Viewport {
    pub rect: [f32; 4],
    pub scroll: Rc<Cell<Scroll>>,
}

/// A rectangle selection under way.
pub struct Lasso {
    pub pane: usize,
    /// Where the press was, in content points: from the columns' left edge and from the top of
    /// the first row, so it stays put as the list scrolls.
    pub start: [f32; 2],
    /// The pointer as last heard, in viewport points.
    pub pointer: [f32; 2],
    /// Ctrl was held at the press: rows touched flip against `before` instead of replacing it.
    pub toggle: bool,
    /// The selection, anchor and cursor from before the press, for Ctrl and for Escape.
    pub before: HashSet<String>,
    pub anchor: Option<String>,
    pub cursor: Option<String>,
    /// The pointer moved: the rectangle shows.
    pub moved: bool,
    /// The app clock at the last edge-scroll step (0: none yet).
    pub stepped: f64,
}

impl Lasso {
    /// The rectangle `[x0, y0, x1, y1]` in content points, ordered, with the list scrolled to
    /// `scroll`.
    pub fn rect(&self, scroll: Scroll) -> [f32; 4] {
        let [x, y] = content(self.pointer, scroll);
        let [sx, sy] = self.start;
        [sx.min(x), sy.min(y), sx.max(x), sy.max(y)]
    }
}

/// A point in viewport points as content points (see [`Lasso::start`]).
pub fn content(at: [f32; 2], scroll: Scroll) -> [f32; 2] {
    [at[0] + scroll.x, at[1] + scroll.y - HEADER_H]
}

/// The rows of `n` that rectangle `rect` (content points, ordered) touches: a row spans the
/// columns' `width` and [`ROW_H`] from `i * ROW_H`. A rectangle with no height still touches
/// the row it lies in, as a click on it would; one wholly beside the columns touches none.
pub fn hits(rect: [f32; 4], n: usize, width: f32) -> Range<usize> {
    let [x0, y0, x1, y1] = rect;
    if n == 0 || x0 >= width || x1 < 0.0 || y1 < 0.0 || y0 >= n as f32 * ROW_H {
        return 0..0;
    }
    let first = (y0.max(0.0) / ROW_H).floor() as usize;
    let last = (y1 / ROW_H).floor() as usize;
    first.min(n)..(last + 1).min(n)
}

/// The selection a lasso makes: the rows it touches, or with `toggle` the old selection with
/// those rows flipped (Ctrl, as Explorer does).
pub fn picked(before: &HashSet<String>, touched: impl Iterator<Item = String>, toggle: bool) -> HashSet<String> {
    let touched: HashSet<String> = touched.collect();
    if toggle { before.symmetric_difference(&touched).cloned().collect() } else { touched }
}

/// Points the list scrolls per second with the pointer `beyond` points past its edge: quicker
/// the farther out, as Explorer's does.
fn speed(beyond: f32) -> f32 {
    (120.0 + beyond * 18.0).min(3000.0)
}

/// How far past the top (negative) or bottom (positive) of the rows' part of the viewport the
/// pointer is; 0 inside.
fn beyond(y: f32, h: f32) -> f32 {
    if y < HEADER_H {
        y - HEADER_H
    } else if y > h {
        y - h
    } else {
        0.0
    }
}

/// Puts the lasso's selection on its pane.
fn apply(s: &mut State) {
    let Some(l) = &s.lasso else { return };
    let p = l.pane;
    let scroll = s.views[p].scroll.get();
    let width: f32 = s.layout.columns[p].iter().sum();
    let pane = &s.panes[p];
    let rows = hits(l.rect(scroll), pane.entries.len(), width);
    let selected = picked(&l.before, pane.entries[rows].iter().map(|e| e.id.clone()), l.toggle);
    s.panes[p].selected = selected;
}

/// The list viewport's `on_drag`: rows take their own presses, so this hears presses on
/// empty space (and on the header's blank end, which it leaves alone).
pub fn drag(s: &mut State, p: usize, d: &mut Drag, tx: &mut Tx) {
    match d.phase {
        Phase::Down => {
            if press(s, p, [d.x, d.y], tx.mods().cmd) {
                tx.focus(pane_key(p), false);
            }
        }
        Phase::Move => {
            // Past an edge, `poll` scrolls on while the pointer rests there.
            if moved(s, p, [d.x, d.y]) {
                tx.wake_after(std::time::Duration::from_millis(16));
            }
        }
        Phase::Up => released(s, p, d.cancelled()),
    }
}

/// A press at `at` (viewport points) on pane `p`'s list, `ctrl` held or not: starts a lasso
/// unless it's on the header. Either way the pane becomes the active one, as its own press
/// (which doesn't run under this one) would make it. True when it started a lasso.
pub fn press(s: &mut State, p: usize, at: [f32; 2], ctrl: bool) -> bool {
    s.active = p;
    if at[1] < HEADER_H {
        return false;
    }
    let pane = &s.panes[p];
    let scroll = s.views[p].scroll.get();
    s.lasso = Some(Lasso {
        pane: p,
        start: content(at, scroll),
        pointer: at,
        toggle: ctrl,
        before: pane.selected.clone(),
        anchor: pane.anchor.clone(),
        cursor: pane.cursor.clone(),
        moved: false,
        stepped: 0.0,
    });
    apply(s);
    true
}

/// The pointer moved to `at` (viewport points) during pane `p`'s lasso. True when it's past
/// the list's top or bottom edge.
pub fn moved(s: &mut State, p: usize, at: [f32; 2]) -> bool {
    let Some(l) = s.lasso.as_mut().filter(|l| l.pane == p) else { return false };
    l.pointer = at;
    l.moved = true;
    apply(s);
    beyond(at[1], s.views[p].scroll.get().h) != 0.0
}

/// Pane `p`'s lasso ended; `cancelled` (Escape) puts back what was selected before it. Else the
/// keyboard goes on from the rectangle, as in Explorer: the cursor on the touched row where the
/// drag ended, the anchor (for Shift) on the one where it started.
pub fn released(s: &mut State, p: usize, cancelled: bool) {
    let Some(l) = s.lasso.take_if(|l| l.pane == p) else { return };
    let scroll = s.views[p].scroll.get();
    let pane = &mut s.panes[p];
    if cancelled {
        pane.selected = l.before;
        pane.anchor = l.anchor;
        pane.cursor = l.cursor;
        return;
    }
    let rows = hits(l.rect(scroll), pane.entries.len(), f32::MAX);
    if !l.moved || rows.is_empty() {
        return;
    }
    let down = content(l.pointer, scroll)[1] >= l.start[1];
    let (from, to) = if down { (rows.start, rows.end - 1) } else { (rows.end - 1, rows.start) };
    pane.anchor = Some(pane.entries[from].id.clone());
    pane.cursor = Some(pane.entries[to].id.clone());
}

/// From `poll`: while the pointer rests past the list's top or bottom edge, scrolls it a step
/// and selects what comes into the rectangle. True when it scrolled.
pub fn step(s: &mut State, now: f64, tx: &mut Tx) -> bool {
    let Some(l) = &mut s.lasso else { return false };
    let p = l.pane;
    let cell = s.views[p].scroll.clone();
    let scroll = cell.get();
    let out = beyond(l.pointer[1], scroll.h);
    if out == 0.0 || !l.moved {
        l.stepped = 0.0;
        return false;
    }
    // A frame's worth on the first step; after that, the time since the last one (capped, so a
    // stalled frame doesn't jump the list).
    let dt = if l.stepped > 0.0 { (now - l.stepped).clamp(0.0, 0.05) as f32 } else { 1.0 / 60.0 };
    l.stepped = now;
    let y = (scroll.y + out.signum() * speed(out.abs()) * dt).clamp(0.0, scroll.max_y.max(0.0));
    tx.wake_after(std::time::Duration::from_millis(16));
    if y == scroll.y {
        return false;
    }
    cell.set(Scroll { y, ..scroll });
    tx.scroll_to_instant(crate::ui::list_key(p), scroll.x, y);
    apply(s);
    true
}

/// Where the menu the keyboard opens goes in a list viewport `view`: under row `row` (its
/// bottom-left, a little in, as Windows' keyboard menus open below what they're for), or at
/// the top of the rows with none. Also the scroll that shows that row whole, when it isn't.
pub fn menu_point(view: &Viewport, row: Option<usize>) -> (rune::PointerPress, Option<f32>) {
    let [x, y, _, h] = view.rect;
    let scroll = view.scroll.get();
    let rows_h = (h - HEADER_H).max(ROW_H);
    let (at, to) = match row {
        None => (0.0, None),
        Some(i) => {
            let top = i as f32 * ROW_H;
            let to = if top < scroll.y {
                Some(top)
            } else if top + ROW_H > scroll.y + rows_h {
                Some((top + ROW_H - rows_h).min(scroll.max_y.max(0.0)))
            } else {
                None
            };
            (top + ROW_H - to.unwrap_or(scroll.y), to)
        }
    };
    (rune::PointerPress { x: x + 24.0, y: y + HEADER_H + at.clamp(0.0, rows_h) }, to)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(r: &[&str]) -> HashSet<String> {
        r.iter().map(|s| s.to_string()).collect()
    }

    /// Rows are found from the row height alone: a rectangle from the middle of row 2 down to
    /// the top edge of row 5 touches rows 2 to 5; nothing past the last row or beside the
    /// columns counts.
    #[test]
    fn a_rectangle_touches_the_rows_it_crosses() {
        let h = ROW_H;
        assert_eq!(hits([10.0, 2.5 * h, 50.0, 5.0 * h], 100, 400.0), 2..6);
        assert_eq!(hits([10.0, 2.5 * h, 50.0, 4.99 * h], 100, 400.0), 2..5);
        // Up past the first row and down past the last.
        assert_eq!(hits([10.0, -40.0, 50.0, 1.5 * h], 100, 400.0), 0..2);
        assert_eq!(hits([10.0, 8.5 * h, 50.0, 30.0 * h], 10, 400.0), 8..10);
        // Below the rows, right of the columns, or in an empty folder: nothing.
        assert_eq!(hits([10.0, 10.0 * h, 50.0, 12.0 * h], 10, 400.0), 0..0);
        assert_eq!(hits([410.0, 0.0, 500.0, 5.0 * h], 10, 400.0), 0..0);
        assert_eq!(hits([10.0, 0.0, 50.0, 5.0 * h], 0, 400.0), 0..0);
        // A click right of a row's columns touches nothing; one reaching into them does.
        assert_eq!(hits([450.0, 3.5 * h, 450.0, 3.5 * h], 10, 400.0), 0..0);
        assert_eq!(hits([390.0, 3.5 * h, 450.0, 3.5 * h], 10, 400.0), 3..4);
    }

    /// The press is kept in content points, so a rectangle started before the list scrolled
    /// still reaches back to where it began: started on row 3 at the top, the pointer at the
    /// viewport's row 2 after scrolling 10 rows down covers rows 3 to 12.
    #[test]
    fn the_rectangle_follows_the_scroll() {
        let at_top = Scroll { x: 0.0, y: 0.0, max_y: 2000.0, h: 300.0 };
        let start = content([30.0, HEADER_H + 3.5 * ROW_H], at_top);
        let l = Lasso { pane: 0, start, pointer: [60.0, HEADER_H + 2.5 * ROW_H], toggle: false, before: HashSet::new(), anchor: None, cursor: None, moved: true, stepped: 0.0 };
        assert_eq!(hits(l.rect(at_top), 100, 400.0), 2..4);
        let scrolled = Scroll { y: 10.0 * ROW_H, ..at_top };
        assert_eq!(hits(l.rect(scrolled), 100, 400.0), 3..13);
        // Scrolled sideways, x counts from the columns' left edge.
        let wide = Scroll { x: 300.0, ..at_top };
        assert_eq!(content([50.0, HEADER_H], wide), [350.0, 0.0]);
    }

    /// Without Ctrl the rows touched are the selection; with it they flip against the old one.
    #[test]
    fn ctrl_toggles_the_rows_touched() {
        let before = ids(&["a", "b"]);
        let touched = || ["b", "c"].into_iter().map(String::from);
        assert_eq!(picked(&before, touched(), false), ids(&["b", "c"]));
        assert_eq!(picked(&before, touched(), true), ids(&["a", "c"]));
        // A click (nothing touched) clears, or with Ctrl leaves the selection as it was.
        assert_eq!(picked(&before, std::iter::empty(), false), HashSet::new());
        assert_eq!(picked(&before, std::iter::empty(), true), before);
    }

    /// Past an edge the list scrolls, quicker the farther out; inside it doesn't.
    #[test]
    fn the_list_scrolls_only_past_its_edges() {
        assert_eq!(beyond(HEADER_H + 5.0, 300.0), 0.0);
        assert_eq!(beyond(300.0, 300.0), 0.0);
        assert_eq!(beyond(4.0, 300.0), 4.0 - HEADER_H);
        assert_eq!(beyond(340.0, 300.0), 40.0);
        assert!(speed(80.0) > speed(10.0));
    }

    /// The whole gesture on a pane: a drag selects live, scrolls past the bottom edge, and
    /// Escape puts back what was selected before.
    #[test]
    fn a_drag_selects_scrolls_and_escape_restores() {
        let mut s = crate::init();
        s.cache = crate::cache::Cache::memory_only();
        s.panes[1].set_entries(Ok((0..200).map(|i| crate::model::Entry { id: format!("f{i}"), name: format!("arquivo {i}"), is_dir: false, size: Some(1), modified: None, target: None, mime: None, md5: None, starred: false }).collect()));
        s.panes[1].selected = ids(&["f150"]);
        s.views[1].scroll.set(Scroll { x: 0.0, y: 0.0, max_y: 200.0 * ROW_H, h: 300.0 });
        let mut tx = Tx::default();
        // Ctrl isn't held: the old selection goes.
        assert!(press(&mut s, 1, [30.0, HEADER_H + 1.5 * ROW_H], false));
        assert_eq!(s.panes[1].selected, ids(&["f1"]));
        assert!(!moved(&mut s, 1, [60.0, HEADER_H + 3.5 * ROW_H]));
        assert_eq!(s.panes[1].selected, ids(&["f1", "f2", "f3"]));
        // Below the bottom edge: each step scrolls and takes in the rows that come.
        assert!(moved(&mut s, 1, [60.0, 340.0]));
        assert!(step(&mut s, 1.0, &mut tx));
        assert!(step(&mut s, 1.05, &mut tx));
        let y = s.views[1].scroll.get().y;
        assert!(y > 0.0);
        let last = ((340.0 + y - HEADER_H) / ROW_H).floor() as usize;
        assert_eq!(s.panes[1].selected.len(), last);
        // Escape: Rune ends the gesture as cancelled.
        released(&mut s, 1, true);
        assert_eq!(s.panes[1].selected, ids(&["f150"]));
        assert!(s.lasso.is_none());
        // A press on the header starts nothing; one on empty space below the rows clears.
        assert!(!press(&mut s, 1, [30.0, 10.0], false));
        s.panes[1].set_entries(Ok(Vec::new()));
        s.panes[1].selected = ids(&["f150"]);
        assert!(press(&mut s, 1, [30.0, 200.0], false));
        released(&mut s, 1, false);
        assert!(s.panes[1].selected.is_empty());
    }

    /// After a rectangle, the keyboard goes on from it: the cursor on the row where the drag
    /// ended, the anchor where it started, whichever way it went.
    #[test]
    fn the_keyboard_goes_on_from_the_rectangle() {
        let mut s = crate::init();
        s.cache = crate::cache::Cache::memory_only();
        s.panes[1].set_entries(Ok((0..20).map(|i| crate::model::Entry { id: format!("f{i}"), name: format!("arquivo {i:02}"), is_dir: false, size: Some(1), modified: None, target: None, mime: None, md5: None, starred: false }).collect()));
        s.views[1].scroll.set(Scroll { x: 0.0, y: 0.0, max_y: 0.0, h: 600.0 });
        s.panes[1].cursor = Some("f15".into());
        press(&mut s, 1, [30.0, HEADER_H + 2.5 * ROW_H], false);
        moved(&mut s, 1, [60.0, HEADER_H + 5.5 * ROW_H]);
        released(&mut s, 1, false);
        assert_eq!((s.panes[1].anchor.as_deref(), s.panes[1].cursor.as_deref()), (Some("f2"), Some("f5")));
        // Drawn upward, the other way round.
        press(&mut s, 1, [30.0, HEADER_H + 8.5 * ROW_H], false);
        moved(&mut s, 1, [60.0, HEADER_H + 6.5 * ROW_H]);
        released(&mut s, 1, false);
        assert_eq!((s.panes[1].anchor.as_deref(), s.panes[1].cursor.as_deref()), (Some("f8"), Some("f6")));
    }

    /// The keyboard's menu opens under the row it's for, scrolling it into view first when
    /// it's out of it, or at the top of the rows when it's for the folder.
    #[test]
    fn the_keyboard_menu_opens_under_its_row() {
        let view = Viewport { rect: [100.0, 50.0, 600.0, HEADER_H + 10.0 * ROW_H], scroll: Default::default() };
        view.scroll.set(Scroll { x: 0.0, y: 3.0 * ROW_H, max_y: 90.0 * ROW_H, h: HEADER_H + 10.0 * ROW_H });
        let top = 50.0 + HEADER_H;
        let at = |row| menu_point(&view, row);
        assert_eq!(at(None), (rune::PointerPress { x: 124.0, y: top }, None));
        // Row 5 shows as the third row from the top.
        assert_eq!(at(Some(5)), (rune::PointerPress { x: 124.0, y: top + 3.0 * ROW_H }, None));
        // Row 1 is above the view: it scrolls to the top and the menu goes under it.
        assert_eq!(at(Some(1)), (rune::PointerPress { x: 124.0, y: top + ROW_H }, Some(ROW_H)));
        // Row 40 is below: it scrolls up to the bottom and the menu goes at the bottom edge.
        assert_eq!(at(Some(40)), (rune::PointerPress { x: 124.0, y: top + 10.0 * ROW_H }, Some(31.0 * ROW_H)));
    }
}
