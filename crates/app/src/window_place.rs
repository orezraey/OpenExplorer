//! The main window reopens where it was closed, as Explorer's does: its place, its size, and
//! maximized or not. What it was is kept in the settings (`Settings::window`) as it moves and
//! when it closes.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use rune::Tx;
use serde::{Deserialize, Serialize};

use crate::State;

/// Where the main window was when it last closed, in physical pixels: the frame's top-left
/// and the content's size, as `rune::WindowOptions` takes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub position: (i32, i32),
    pub size: (u32, u32),
    pub maximized: bool,
}

impl Saved {
    /// What to keep of a placement: a maximized or minimized window keeps the frame restoring
    /// it goes back to. A minimized one reopens maximized if it was when minimized
    /// (`was_maximized`), as Windows would restore it. `None` when there is no such frame or no
    /// size to keep.
    pub fn of(p: &rune::WindowPlacement, was_maximized: bool) -> Option<Saved> {
        let (position, size) = if p.maximized || p.minimized { p.restore? } else { (p.position, p.content_size) };
        if p.full_screen || size.0 == 0 || size.1 == 0 {
            return None;
        }
        Some(Saved { position, size, maximized: p.maximized || (p.minimized && was_maximized) })
    }
}

thread_local! {
    /// The main window, from its creation on, to read where it is when it closes.
    static MAIN: RefCell<Option<rune::WindowHandle>> = const { RefCell::new(None) };
    /// Whether the main window was maximized the last time it was shown (not minimized).
    static WAS_MAXIMIZED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// When the place the main window moved to is written to the settings file, if it moved
    /// since the last write.
    static WRITE_AT: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

/// How long the window rests before its new place is written, so a drag writes once.
const WRITE_AFTER: Duration = Duration::from_secs(1);

/// `App::on_placement`: the main window moved, was resized, maximized or restored. Its place goes
/// into the settings at once and to the file once it rests, so it reopens there even after the
/// program was ended without closing (a crash, Task Manager). Wayland never calls it: there the
/// place is kept only on close (`remember`).
pub fn placed(s: &mut State, p: rune::WindowPlacement, tx: &mut Tx) -> bool {
    if s.is_placeholder {
        return false;
    }
    if let Some(saved) = Saved::of(&p, WAS_MAXIMIZED.with(std::cell::Cell::get))
        && s.settings.window != Some(saved)
    {
        s.settings.window = Some(saved);
        WRITE_AT.with(|w| w.set(Some(Instant::now() + WRITE_AFTER)));
        tx.wake_after(WRITE_AFTER);
    }
    false
}

/// From the main window's `poll`: writes the place it moved to, once it rested `WRITE_AFTER`.
/// Woken a little early, it asks to be woken again when that time comes.
pub fn write_when_rested(s: &State, tx: &mut Tx) {
    let Some(at) = WRITE_AT.with(std::cell::Cell::get) else { return };
    let now = Instant::now();
    if now >= at {
        WRITE_AT.with(|w| w.set(None));
        s.settings.save();
    } else {
        tx.wake_after(at - now);
    }
}

/// `App::on_window_geometry`: the window's size changed (maximized, restored, minimized). While
/// it shows, it notes whether it is maximized, for a close that comes while minimized.
pub fn resized() {
    let Some(p) = MAIN.with(|m| m.borrow().as_ref().and_then(rune::WindowHandle::placement)) else { return };
    if !p.minimized {
        WAS_MAXIMIZED.with(|w| w.set(p.maximized));
    }
}

/// `App::on_window`: the main window was created. On macOS and Linux the displays can only be
/// asked through an open window, so a place none of them shows any more (one unplugged, a
/// resolution changed) is found now, and the window goes to the middle of the screen. Windows
/// drops such a place before the window is made (`options`).
pub fn created(window: &rune::WindowHandle) {
    MAIN.with(|m| *m.borrow_mut() = Some(window.clone()));
    #[cfg(not(windows))]
    if let Some(p) = window.placement()
        && !p.maximized
        && !p.full_screen
    {
        let displays: Vec<Area> = rune::displays().iter().map(|d| (d.position, d.size)).collect();
        if !reachable(p.position, p.content_size.0, &displays) {
            window.center();
        }
    }
}

/// A display's place: its top-left and its size, in desktop pixels.
type Area = ((i32, i32), (u32, u32));

/// Whether some display shows the top of a window at `top_left`, `width` wide (its first 32
/// pixels, where it is grabbed to move it); displays as (position, size) in desktop pixels.
/// With no display known it says yes: a window is never moved on no news.
#[cfg_attr(windows, allow(dead_code))]
fn reachable(top_left: (i32, i32), width: u32, displays: &[Area]) -> bool {
    if displays.is_empty() {
        return true;
    }
    let (left, top) = (i64::from(top_left.0), i64::from(top_left.1));
    let (right, bottom) = (left + i64::from(width), top + 32);
    displays.iter().any(|&((x, y), (w, h))| {
        let (x, y) = (i64::from(x), i64::from(y));
        left < x + i64::from(w) && right > x && top < y + i64::from(h) && bottom > y
    })
}

/// `App::on_close`: keeps where the main window is, for the next start.
pub fn remember(s: &mut State) {
    if s.is_placeholder {
        return;
    }
    // A place `placed` took in but has not written yet is written now.
    let mut unwritten = WRITE_AT.with(std::cell::Cell::take).is_some();
    if let Some(placement) = MAIN.with(|m| m.borrow().as_ref().and_then(rune::WindowHandle::placement))
        && let Some(saved) = Saved::of(&placement, WAS_MAXIMIZED.with(std::cell::Cell::get))
        && s.settings.window != Some(saved)
    {
        s.settings.window = Some(saved);
        unwritten = true;
    }
    if unwritten {
        s.settings.save();
    }
}

/// The options the main window opens with: where it was, while some monitor still shows its
/// title bar (one unplugged since, or a resolution changed, would leave it out of reach).
pub fn options(saved: Option<Saved>) -> Option<rune::WindowOptions> {
    let saved = saved.filter(on_screen)?;
    Some(rune::WindowOptions { position: Some(saved.position), physical_size: Some(saved.size), ..Default::default() })
}

/// Whether a monitor shows the top of the window, where it is grabbed to move it.
#[cfg(windows)]
fn on_screen(w: &Saved) -> bool {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromRect};
    let width = i32::try_from(w.size.0).unwrap_or(i32::MAX);
    let strip = RECT { left: w.position.0, top: w.position.1, right: w.position.0.saturating_add(width), bottom: w.position.1.saturating_add(32) };
    // SAFETY: MonitorFromRect only reads the rectangle it is given.
    !unsafe { MonitorFromRect(&strip, MONITOR_DEFAULTTONULL) }.is_null()
}

#[cfg(not(windows))]
fn on_screen(_: &Saved) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window in its own frame keeps that frame; a maximized or minimized one, the frame it
    /// goes back to, and whether it was maximized.
    #[test]
    fn keeps_the_frame_restoring_goes_back_to() {
        let normal = rune::WindowPlacement { position: (100, 80), content_size: (1400, 900), scale: 1.5, ..Default::default() };
        assert_eq!(Saved::of(&normal, true), Some(Saved { position: (100, 80), size: (1400, 900), maximized: false }));
        let maximized = rune::WindowPlacement { position: (-8, -8), content_size: (3840, 2050), maximized: true, restore: Some(((200, 150), (1200, 800))), ..Default::default() };
        assert_eq!(Saved::of(&maximized, false), Some(Saved { position: (200, 150), size: (1200, 800), maximized: true }));
        let minimized = rune::WindowPlacement { minimized: true, restore: Some(((10, 20), (1000, 700))), ..Default::default() };
        assert_eq!(Saved::of(&minimized, false), Some(Saved { position: (10, 20), size: (1000, 700), maximized: false }));
        // Minimized from maximized, it reopens maximized.
        assert_eq!(Saved::of(&minimized, true), Some(Saved { position: (10, 20), size: (1000, 700), maximized: true }));
        assert_eq!(Saved::of(&rune::WindowPlacement { maximized: true, ..Default::default() }, false), None);
        assert_eq!(Saved::of(&rune::WindowPlacement { full_screen: true, content_size: (10, 10), ..Default::default() }, false), None);
    }

    /// A window left where no monitor is now opens where the system puts it.
    #[test]
    fn a_place_off_every_monitor_is_dropped() {
        let far = Saved { position: (-200_000, -200_000), size: (1200, 800), maximized: false };
        assert!(options(Some(far)).is_none());
        assert!(options(None).is_none());
    }

    /// macOS and Linux: a window's top on some display is reachable, one past every display
    /// isn't, and with no display known the window stays where it is.
    #[test]
    fn a_window_top_off_every_display_is_found() {
        let two = [((0, 0), (1920, 1080)), ((1920, 0), (2560, 1440))];
        assert!(reachable((100, 80), 1200, &two));
        assert!(reachable((3000, 1300), 1200, &two));
        // Hanging off the left edge, its right part still shows.
        assert!(reachable((-1000, 100), 1200, &two));
        assert!(!reachable((5000, 100), 1200, &two));
        assert!(!reachable((100, 1500), 1200, &two));
        assert!(!reachable((100, -40), 1200, &two));
        assert!(reachable((5000, 5000), 1200, &[]));
    }
}
