//! Scrolling a pane's file list and tree while files are dragged near their top or bottom edge,
//! so what's out of view can be seen without letting go: faster the nearer the edge, as
//! vercel-labs/ui's `createAutoScroll` does (a band of 48 points, at most 18 points a frame).
//! A drop still lands in the folder the pane shows, wherever on the pane it falls: rows and
//! tree folders don't take drops of their own.
//!
//! Rune scrolls a viewport by itself only for a text selection held at its edge, and its tab
//! strip's drag scroll follows its own pointer drags; a drag of files is an OLE drag the pane
//! hears through `on_drop_move`, which reports where the drag goes but not a pointer held
//! still. So the pane notes where the drag is, and `poll` steps the scroll each frame while it
//! sits in a band that has more to show, as the tab strip's (`rune_kit` `auto_scroll`) does:
//! the viewport reports where it drew and how far it has scrolled, and each step asks for the
//! new offset with `Tx::scroll_to_y`.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use rune::{El, Live, ScrollPos, Tx};

/// How far in from an edge scrolling starts, in points (a third of the viewport at most).
pub const EDGE: f32 = 48.0;
/// The fastest it goes, with the pointer at the edge or past it: 18 points a frame at 60 Hz.
pub const SPEED: f32 = 18.0 * 60.0;
/// The longest step one frame takes, in seconds: a frame that came late doesn't jump the list.
const LONGEST_STEP: f64 = 0.05;
/// One frame at 60 Hz, when the next step is due.
const FRAME: Duration = Duration::from_micros(16_667);

/// How fast to scroll, in points a second, with the pointer at `at` over a viewport running
/// from `start` to `end`: negative toward the start, by how deep into the edge band the pointer
/// is, `speed` at the edge itself and beyond it; 0 between the bands.
pub fn velocity(at: f32, start: f32, end: f32, edge: f32, speed: f32) -> f32 {
    let band = edge.min((end - start) / 3.0);
    if band <= 0.0 {
        0.0
    } else if at < start + band {
        -speed * ((start + band - at) / band).min(1.0)
    } else if at > end - band {
        speed * ((at - (end - band)) / band).min(1.0)
    } else {
        0.0
    }
}

/// Where a step of `dt` seconds at `v` points a second takes an `offset` that can go from 0 to
/// `max`.
pub fn step(offset: f32, max: f32, v: f32, dt: f32) -> f32 {
    (offset + v * dt).clamp(0.0, max.max(0.0))
}

/// Where a scroll viewport drew and how far it has scrolled, as it reports them.
#[derive(Clone)]
pub struct Watch {
    rect: Live<[f32; 4]>,
    pos: Rc<Cell<ScrollPos>>,
}

impl Default for Watch {
    fn default() -> Self {
        Watch { rect: Live::new([0.0; 4]), pos: Rc::new(Cell::new(ScrollPos::default())) }
    }
}

impl Watch {
    /// The viewport's height as last drawn (0 before it was).
    pub fn height(&self) -> f32 {
        self.rect.get()[3]
    }

    /// Has the scroll viewport `scroll` report where it draws and how far it scrolled here.
    pub fn on(&self, scroll: El) -> El {
        let pos = self.pos.clone();
        scroll.drawn_rect_into(&self.rect).chain_scroll_offset(move |at| pos.set(at))
    }

    /// One step of `dt` seconds of the viewport keyed `key` for a drag at `at` (window
    /// points), whose rows start `top` points below the viewport's top (under a header that
    /// stays). True while the drag sits in a band of an end with more to show.
    fn step(&self, key: &str, at: (f32, f32), top: f32, dt: f32, tx: &mut Tx) -> bool {
        let ([x, y, w, h], mut pos) = (self.rect.get(), self.pos.get());
        if w <= 0.0 || !(x..=x + w).contains(&at.0) || !(y..=y + h).contains(&at.1) {
            return false;
        }
        let v = velocity(at.1, y + top, y + h, EDGE, SPEED);
        let to = step(pos.offset[1], pos.max[1], v, dt);
        let can = (v < 0.0 && pos.offset[1] > 0.0) || (v > 0.0 && pos.offset[1] < pos.max[1]);
        if to != pos.offset[1] {
            // Noted at once, so the next step goes on from here before the viewport reports.
            pos.offset[1] = to;
            self.pos.set(pos);
            tx.scroll_to_y(key, to);
        }
        can
    }
}

/// The two viewports of a pane that scroll during a drag.
#[derive(Clone, Default)]
pub struct Pane {
    pub list: Watch,
    pub tree: Watch,
}

/// A drag of files over pane `pane`, at `at` in window points; `last` is when it last stepped
/// (app clock, seconds).
pub struct Drag {
    pub pane: usize,
    pub at: (f32, f32),
    last: f64,
}

/// The drag moved to `at` over pane `p`: noted, and the next frame steps it.
pub fn moved(drag: &mut Option<Drag>, p: usize, at: (f32, f32), now: f64, tx: &mut Tx) {
    match drag {
        Some(d) if d.pane == p => d.at = at,
        _ => *drag = Some(Drag { pane: p, at, last: now }),
    }
    tx.wake_after(Duration::ZERO);
}

/// The drag left pane `p`, or dropped there: its scrolling stops.
pub fn left(drag: &mut Option<Drag>, p: usize) {
    if drag.as_ref().is_some_and(|d| d.pane == p) {
        *drag = None;
    }
}

/// A frame's step of the drag's scrolling, from `poll`: the list keyed `list` (rows under a
/// header `header` tall) or the tree keyed `tree`, whichever the drag is over. While the drag
/// stays in a band with more to show it asks for the next frame.
pub fn tick(drag: &mut Option<Drag>, panes: &[Pane], keys: impl Fn(usize) -> (String, String), header: f32, now: f64, tx: &mut Tx) {
    let Some(d) = drag else { return };
    let dt = (now - d.last).clamp(0.0, LONGEST_STEP) as f32;
    d.last = now;
    let (list, tree) = keys(d.pane);
    let pane = &panes[d.pane];
    let going = pane.list.step(&list, d.at, header, dt, tx) | pane.tree.step(&tree, d.at, 0.0, dt, tx);
    if going {
        tx.wake_after(FRAME);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faster_the_nearer_the_edge() {
        // A viewport from 100 to 500: bands of 48 points at each end.
        let v = |at| velocity(at, 100.0, 500.0, 48.0, 1000.0);
        assert_eq!(v(300.0), 0.0);
        assert_eq!(v(148.0), 0.0);
        assert_eq!(v(452.0), 0.0);
        assert_eq!(v(124.0), -500.0);
        assert_eq!(v(100.0), -1000.0);
        assert_eq!(v(476.0), 500.0);
        assert_eq!(v(500.0), 1000.0);
        // Past an edge (over the list's header, say) it goes no faster.
        assert_eq!(v(80.0), -1000.0);
        assert_eq!(v(520.0), 1000.0);
        assert!(v(110.0) < v(130.0) && v(130.0) < 0.0);
        assert!(v(490.0) > v(470.0) && v(470.0) > 0.0);
    }

    #[test]
    fn a_short_viewport_keeps_a_middle_that_doesnt_scroll() {
        // 60 points tall: bands of 20, not 48, so its middle stays still.
        let v = |at| velocity(at, 0.0, 60.0, 48.0, 1000.0);
        assert_eq!(v(30.0), 0.0);
        assert_eq!(v(10.0), -500.0);
        assert_eq!(v(50.0), 500.0);
        assert_eq!(velocity(5.0, 0.0, 0.0, 48.0, 1000.0), 0.0);
    }

    #[test]
    fn a_step_goes_by_the_time_and_stays_within_the_content() {
        assert_eq!(step(100.0, 1000.0, 600.0, 0.5), 400.0);
        assert_eq!(step(100.0, 1000.0, -600.0, 0.1), 40.0);
        assert_eq!(step(100.0, 1000.0, -600.0, 1.0), 0.0);
        assert_eq!(step(900.0, 1000.0, 600.0, 1.0), 1000.0);
        assert_eq!(step(0.0, 0.0, 600.0, 1.0), 0.0);
    }

    #[test]
    fn the_drag_follows_its_pane_and_stops_when_it_leaves() {
        let mut tx = Tx::default();
        let mut drag = None;
        moved(&mut drag, 1, (10.0, 20.0), 5.0, &mut tx);
        moved(&mut drag, 1, (12.0, 30.0), 6.0, &mut tx);
        let d = drag.as_ref().unwrap();
        // A move keeps the time of the last step, so moving the pointer doesn't slow it.
        assert_eq!((d.pane, d.at, d.last), (1, (12.0, 30.0), 5.0));
        left(&mut drag, 0);
        assert!(drag.is_some());
        left(&mut drag, 1);
        assert!(drag.is_none());
    }
}
