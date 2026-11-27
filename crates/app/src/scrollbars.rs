//! The app's scroll bars, laid out as Windows lays out its own: each in a gutter of its own beside
//! what it scrolls, so it never covers a row or a column, with an arrow at each end. A click on an
//! arrow scrolls a little, again and again while it's held, and the thumb stops against the
//! arrows, so the top and the bottom of a list read alike. An arrow dims when its end is reached.
//!
//! The vertical bar is `rune-kit`'s (its thumb drag and track paging), moved into a gutter the
//! wrapper always keeps on the right: shown or not, its room stays, so content never reflows as a
//! list grows past the window. The horizontal one is drawn here, in a gutter along the bottom that
//! the view lays out only while the content is wider than the viewport: when that changes, the
//! view runs again ([`take_stale`]).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use rune::{Color, Cursor, Drag, El, Icon, Live, Phase, ScrollPos, Tx, Waker, col, el, icon};
use rune_kit::{Kit, ScrollBar, ScrollBarStyle};

use crate::icons;

/// How thick a gutter is, and how long each arrow at its ends.
pub(crate) const GUTTER: f32 = 16.0;
/// The thumb's thickness, centred in the gutter.
const THUMB: f32 = 8.0;
/// The room around the thumb in its gutter: on both sides, and between it and the arrows.
const GAP: f32 = (GUTTER - THUMB) / 2.0;
/// The shortest a thumb gets.
const MIN_THUMB: f32 = 32.0;
/// How far one arrow step scrolls: about a row.
const STEP: f32 = 24.0;
/// A held arrow waits this long, then steps this often, as a Windows scroll bar's does.
const REPEAT_DELAY: Duration = Duration::from_millis(300);
const REPEAT_EVERY: Duration = Duration::from_millis(40);
/// An arrow at its end of the content.
const DIM: f32 = 0.35;
/// The most bars kept between views; past it they're all dropped and each comes back on its next
/// build.
const KEPT: usize = 64;

thread_local! {
    static BARS: RefCell<HashMap<(u64, String), Rc<Bars>>> = RefCell::new(HashMap::new());
    /// A viewport's content became wider or narrower than it: the view should run again to lay
    /// out its bottom gutter, or take it away.
    static STALE: Cell<bool> = const { Cell::new(false) };
}

/// Whether a horizontal gutter came or went since the last call: `poll` runs the view again.
pub(crate) fn take_stale() -> bool {
    STALE.with(|s| s.replace(false))
}

/// What one viewport's bars keep across rebuilds: where it stands, and the live values that
/// follow it, so scrolling and hovering never run the view.
struct Bars {
    pos: Cell<ScrollPos>,
    /// How far down the vertical track starts, below a header.
    top: Cell<f32>,
    /// Whether this viewport has a horizontal bar at all, and whether the view last built laid
    /// out its gutter.
    sideways: Cell<bool>,
    built_x: Cell<bool>,
    /// Wakes `poll` when the gutter should come or go.
    waker: RefCell<Option<Waker>>,
    /// The horizontal thumb under the pointer, and the press holding it: where the press began
    /// and the offset then.
    hover: Cell<bool>,
    drag: Cell<Option<(f32, f32)>>,
    rest: Cell<Color>,
    hot: Cell<Color>,
    h_thumb: Live<[f32; 4]>,
    h_fill: Live<Color>,
    /// 1 while the vertical bar shows, 0 while the content fits.
    v_on: Live<f32>,
    /// The up, down, left and right arrows' opacity: [`DIM`] at their end, 0 with nothing to
    /// scroll.
    ends: [Live<f32>; 4],
}

fn kept(key: &str) -> Rc<Bars> {
    let id = (rune::current_runtime_id(), key.to_owned());
    BARS.with(|bars| {
        let mut bars = bars.borrow_mut();
        if bars.len() >= KEPT && !bars.contains_key(&id) {
            bars.clear();
        }
        bars.entry(id)
            .or_insert_with(|| {
                Rc::new(Bars {
                    pos: Cell::new(ScrollPos::default()),
                    top: Cell::new(0.0),
                    sideways: Cell::new(false),
                    built_x: Cell::new(false),
                    waker: RefCell::new(None),
                    hover: Cell::new(false),
                    drag: Cell::new(None),
                    rest: Cell::new(Color::TRANSPARENT),
                    hot: Cell::new(Color::TRANSPARENT),
                    h_thumb: Live::new([0.0; 4]),
                    h_fill: Live::new(Color::TRANSPARENT),
                    v_on: Live::new(0.0),
                    ends: std::array::from_fn(|_| Live::new(0.0)),
                })
            })
            .clone()
    })
}

/// Whether a viewport at `at` scrolls sideways, and is wide enough for a thumb between the
/// arrows.
fn h_shows(at: ScrollPos) -> bool {
    at.max[0] > 0.5 && at.viewport[0] - 2.0 * GUTTER - 2.0 * GAP >= MIN_THUMB
}

/// An arrow's opacity: hidden with nothing to scroll, dimmed at its end.
fn end_opacity(on: bool, stopped: bool) -> f32 {
    if !on {
        0.0
    } else if stopped {
        DIM
    } else {
        1.0
    }
}

/// The horizontal thumb on a track `track` long (its gaps included) for a viewport at `at`: its
/// left and width inside the track, and how far it travels.
fn h_thumb(at: ScrollPos, track: f32) -> (f32, f32, f32) {
    let len = (track - 2.0 * GAP).max(0.0);
    let w = (len * at.viewport[0] / (at.viewport[0] + at.max[0]).max(1.0)).max(MIN_THUMB).min(len);
    let travel = (len - w).max(0.0);
    let x = if at.max[0] > 0.0 { travel * (at.offset[0] / at.max[0]).clamp(0.0, 1.0) } else { 0.0 };
    (GAP + x, w, travel)
}

impl Bars {
    /// Whether the vertical bar shows: the content is taller than the viewport, and the track
    /// between the arrows is long enough for a thumb (as `rune-kit` decides for its own thumb).
    fn v_shows(&self, at: ScrollPos) -> bool {
        at.max[1] > 0.5 && at.viewport[1] - self.top.get() - 2.0 * GUTTER >= MIN_THUMB
    }

    /// The horizontal track's length: the viewport's width less an arrow at each end.
    fn h_track_len(&self) -> f32 {
        (self.pos.get().viewport[0] - 2.0 * GUTTER).max(0.0)
    }

    fn sync(&self) {
        let at = self.pos.get();
        let (v, h) = (self.v_shows(at), h_shows(at));
        self.v_on.set(if v { 1.0 } else { 0.0 });
        self.ends[0].set(end_opacity(v, at.offset[1] <= 0.5));
        self.ends[1].set(end_opacity(v, at.offset[1] >= at.max[1] - 0.5));
        self.ends[2].set(end_opacity(h, at.offset[0] <= 0.5));
        self.ends[3].set(end_opacity(h, at.offset[0] >= at.max[0] - 0.5));
        let hot = self.hover.get() || self.drag.get().is_some();
        self.h_fill.set(if hot { self.hot.get() } else { self.rest.get() });
        let (x, w, _) = h_thumb(at, self.h_track_len());
        self.h_thumb.set(if h { [x, GAP, w, THUMB] } else { [0.0; 4] });
    }

    /// The viewport scrolled or changed size.
    fn follow(&self, mut at: ScrollPos) {
        at.viewport = at.drawn;
        self.pos.set(at);
        self.sync();
        if self.sideways.get()
            && h_shows(at) != self.built_x.get()
            && !STALE.with(|s| s.replace(true))
            && let Some(wake) = self.waker.borrow().as_ref()
        {
            wake();
        }
    }

    /// An arrow pressed, held or let go: it steps `dir` (-1 or 1 on an axis) at the press, then
    /// again while held, pausing while the pointer is off the arrow, as Windows' arrows do.
    fn arrow(&self, target: &str, dir: [f32; 2], d: &Drag, tx: &mut Tx) {
        tx.live_only();
        match d.phase {
            Phase::Down => tx.drag_repeat_after(REPEAT_DELAY),
            Phase::Move if tx.drag_repeat() => {
                tx.drag_repeat_after(REPEAT_EVERY);
                let on = |v: f32| (0.0..=GUTTER).contains(&v);
                if !(on(d.x) && on(d.y)) {
                    return;
                }
            }
            _ => return,
        }
        let at = self.pos.get();
        let x = (at.offset[0] + dir[0] * STEP).clamp(0.0, at.max[0].max(0.0));
        let y = (at.offset[1] + dir[1] * STEP).clamp(0.0, at.max[1].max(0.0));
        if (x - at.offset[0]).abs() > 0.01 || (y - at.offset[1]).abs() > 0.01 {
            tx.scroll_to_instant(target, x, y);
        }
    }

    /// The horizontal track pressed or dragged at `d.x` along it: a press on the thumb grabs it,
    /// one beside it brings the thumb's middle there.
    fn h_drag(&self, target: &str, d: &Drag, tx: &mut Tx) {
        tx.live_only();
        let at = self.pos.get();
        if !h_shows(at) {
            self.drag.set(None);
            return;
        }
        let (x, w, travel) = h_thumb(at, self.h_track_len());
        let to = |from: f32| if travel > 0.0 { (from / travel * at.max[0]).clamp(0.0, at.max[0]) } else { at.offset[0] };
        match d.phase {
            Phase::Down => {
                let start = if (x..=x + w).contains(&d.x) {
                    at.offset[0]
                } else {
                    let next = to(d.x - GAP - w / 2.0);
                    tx.scroll_to_instant(target, next, at.offset[1]);
                    next
                };
                self.drag.set(Some((d.x, start)));
                self.sync();
            }
            Phase::Move => {
                if let Some((from, start)) = self.drag.get()
                    && travel > 0.0
                {
                    let next = (start + (d.x - from) / travel * at.max[0]).clamp(0.0, at.max[0]);
                    tx.scroll_to_instant(target, next, at.offset[1]);
                }
            }
            Phase::Up => {
                self.drag.set(None);
                self.sync();
            }
        }
    }
}

/// The vertical bar: `rune-kit`'s thumb in the right gutter, between the arrows, its track
/// starting `top` points down (below a header).
fn v_style(k: &Kit, top: f32) -> ScrollBarStyle {
    ScrollBarStyle::from_kit(k)
        .webkit_parts(GUTTER, GAP, GUTTER)
        .min_thumb(MIN_THUMB)
        .thumb(k.alpha(k.c.text_base, 0.3))
        .thumb_hover(k.alpha(k.c.text_base, 0.5))
        .always_visible()
        .track_offset(top, -GUTTER)
}

/// The gutters' own fill, a shade off what they sit beside.
fn trough(k: &Kit) -> Color {
    k.alpha(k.c.text_base, if k.dark { 0.05 } else { 0.035 })
}

/// An arrow at one end of a bar, stepping its viewport `dir` while pressed.
fn arrow(k: &Kit, name: &str, glyph: &'static Icon, target: &str, bars: &Rc<Bars>, dir: [f32; 2], shown: &Live<f32>) -> El {
    let (bars, target) = (bars.clone(), target.to_owned());
    el().key(format!("arrow-{name}"))
        .size(GUTTER, GUTTER)
        .center()
        .a11y_hidden()
        .hover(k.c.hover)
        .press(k.c.pressed)
        .live_opacity(shown)
        .wheel_scrolls(target.clone())
        .on_drag_any(move |d: &mut Drag, tx: &mut Tx| bars.arrow(&target, dir, d, tx))
        .child(icon(glyph, 10.0).color(k.alpha(k.c.text_base, 0.6)))
}

/// A vertical scroll viewport keyed `key` with its bar in a gutter on the right. Size what this
/// returns as the viewport would be sized; the viewport fills it.
pub(crate) fn v_bar(k: &Kit, key: &str, viewport: El) -> El {
    build(k, key, viewport, 0.0, None)
}

/// How much taller the viewport keyed `key` will be after the view now running than it was laid
/// out last: `-GUTTER` while its bottom gutter is coming, `GUTTER` while it's going, else 0. Call
/// it before [`xy_bars`] builds the bars, to size content that fills the viewport's height from
/// its last layout.
pub(crate) fn height_change(key: &str) -> f32 {
    let id = (rune::current_runtime_id(), key.to_owned());
    BARS.with(|bars| match bars.borrow().get(&id) {
        Some(bars) if bars.sideways.get() => match (bars.built_x.get(), h_shows(bars.pos.get())) {
            (false, true) => -GUTTER,
            (true, false) => GUTTER,
            _ => 0.0,
        },
        _ => 0.0,
    })
}

/// A `scroll_xy()` viewport keyed `key` with a vertical bar on the right, starting `top` points
/// down beside a header that tall, and a horizontal bar along the bottom while the content is
/// wider than the viewport. `waker` runs the view again when that changes.
pub(crate) fn xy_bars(k: &Kit, key: &str, viewport: El, top: f32, waker: Option<Waker>) -> El {
    build(k, key, viewport, top, Some(waker))
}

fn build(k: &Kit, key: &str, viewport: El, top: f32, sideways: Option<Option<Waker>>) -> El {
    let bars = kept(key);
    bars.top.set(top);
    bars.sideways.set(sideways.is_some());
    let shows_x = sideways.is_some() && h_shows(bars.pos.get());
    bars.built_x.set(shows_x);
    // A track taken away hears no hover leave or release: start the next one at rest.
    if !shows_x {
        bars.hover.set(false);
        bars.drag.set(None);
    }
    if let Some(waker) = sideways {
        *bars.waker.borrow_mut() = waker;
    }
    bars.rest.set(k.alpha(k.c.text_base, 0.3));
    bars.hot.set(k.alpha(k.c.text_base, 0.5));
    bars.sync();

    let watch = bars.clone();
    let bottom = if shows_x { GUTTER } else { 0.0 };
    // The gutters are the padding of the area `rune-kit` wraps the viewport in, so the pointer
    // over the vertical thumb is inside that area, where the kit hears it (its hover colour).
    let viewport = viewport.chain_scroll_offset(move |at| watch.follow(at)).scroll_bar_styled(v_style(k, top)).pr(GUTTER).pb(bottom);
    let trough = trough(k);
    let mut wrap = col()
        .key(format!("bars-{key}"))
        .min_w(0.0)
        .min_h(0.0)
        .child(el().key("v-gutter").pin(None, Some(top), Some(0.0), Some(bottom)).w(GUTTER).fill(trough).live_opacity(&bars.v_on).wheel_scrolls(key))
        .child(viewport)
        .child(arrow(k, "up", &icons::SCROLL_UP, key, &bars, [0.0, -1.0], &bars.ends[0]).pin(None, Some(top), Some(0.0), None))
        .child(arrow(k, "down", &icons::SCROLL_DOWN, key, &bars, [0.0, 1.0], &bars.ends[1]).pin(None, None, Some(0.0), Some(bottom)));
    // The header runs on over the gutter, its rule with it.
    if top > 0.0 {
        wrap = wrap.child(el().key("v-head").pin(None, Some(0.0), Some(0.0), None).size(GUTTER, top).fill(k.c.bg_base).border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak).wheel_scrolls(key));
    }
    if !shows_x {
        return wrap;
    }
    let (hover, drag, target) = (bars.clone(), bars.clone(), key.to_owned());
    let thumb = el().key("h-thumb").absolute(0.0, 0.0).live_rect(&bars.h_thumb).radius(THUMB / 2.0).live_fill(&bars.h_fill);
    let track = el()
        .key("h-track")
        .pin(Some(GUTTER), None, Some(2.0 * GUTTER), Some(0.0))
        .h(GUTTER)
        .cursor(Cursor::Default)
        .a11y_hidden()
        .wheel_scrolls(key)
        .on_hover_change_any(move |inside, tx| {
            tx.live_only();
            hover.hover.set(inside);
            hover.sync();
        })
        .on_drag_any(move |d: &mut Drag, tx: &mut Tx| drag.h_drag(&target, d, tx))
        .child(thumb);
    wrap.child(el().key("h-gutter").pin(Some(0.0), None, Some(0.0), Some(0.0)).h(GUTTER).fill(trough).wheel_scrolls(key))
        .child(track)
        .child(arrow(k, "left", &icons::SCROLL_LEFT, key, &bars, [-1.0, 0.0], &bars.ends[2]).pin(Some(0.0), None, None, Some(0.0)))
        .child(arrow(k, "right", &icons::SCROLL_RIGHT, key, &bars, [1.0, 0.0], &bars.ends[3]).pin(None, None, Some(GUTTER), Some(0.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(offset: [f32; 2], max: [f32; 2], viewport: [f32; 2]) -> ScrollPos {
        let mut at = ScrollPos::default();
        (at.offset, at.max, at.viewport, at.drawn) = (offset, max, viewport, viewport);
        at
    }

    #[test]
    fn arrows_dim_at_their_ends_and_hide_with_nothing_to_scroll() {
        let bars = kept("test-ends");
        bars.follow(at([0.0, 0.0], [0.0, 400.0], [300.0, 200.0]));
        let ends: Vec<f32> = bars.ends.iter().map(Live::get).collect();
        assert_eq!(ends, [DIM, 1.0, 0.0, 0.0]);
        bars.follow(at([0.0, 400.0], [0.0, 400.0], [300.0, 200.0]));
        assert_eq!(bars.ends[0].get(), 1.0);
        assert_eq!(bars.ends[1].get(), DIM);
        assert_eq!(bars.v_on.get(), 1.0);
        bars.follow(at([0.0, 0.0], [0.0, 0.0], [300.0, 200.0]));
        assert_eq!(bars.v_on.get(), 0.0);
        assert_eq!(bars.ends[0].get(), 0.0);
    }

    #[test]
    fn the_horizontal_thumb_rests_against_each_arrow() {
        let track = 300.0 - 2.0 * GUTTER;
        let (x, w, travel) = h_thumb(at([0.0, 0.0], [600.0, 0.0], [300.0, 200.0]), track);
        assert_eq!(x, GAP);
        let (end, _, _) = h_thumb(at([600.0, 0.0], [600.0, 0.0], [300.0, 200.0]), track);
        assert_eq!(end + w, track - GAP);
        assert_eq!(travel, track - 2.0 * GAP - w);
    }

    #[test]
    fn a_change_in_width_asks_for_the_gutter_once() {
        take_stale();
        let bars = kept("test-stale");
        bars.sideways.set(true);
        bars.built_x.set(false);
        bars.follow(at([0.0, 0.0], [10.0, 0.0], [300.0, 200.0]));
        assert!(take_stale());
        bars.built_x.set(true);
        bars.follow(at([0.0, 0.0], [10.0, 0.0], [300.0, 200.0]));
        assert!(!take_stale());
    }

    #[test]
    fn the_height_change_follows_the_bottom_gutter_coming_and_going() {
        let bars = kept("test-height");
        bars.sideways.set(true);
        bars.built_x.set(false);
        bars.pos.set(at([0.0, 0.0], [10.0, 0.0], [300.0, 200.0]));
        assert_eq!(height_change("test-height"), -GUTTER);
        bars.built_x.set(true);
        assert_eq!(height_change("test-height"), 0.0);
        bars.pos.set(at([0.0, 0.0], [0.0, 0.0], [300.0, 200.0]));
        assert_eq!(height_change("test-height"), GUTTER);
    }
}
