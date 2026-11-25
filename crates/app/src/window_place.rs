//! The main window reopens where it was closed, as Explorer's does: its place, its size, and
//! maximized or not. What it was is kept in the settings (`Settings::window`) when it closes.

use std::cell::RefCell;

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
}

/// `App::on_window_geometry`: the window's size changed (maximized, restored, minimized). While
/// it shows, it notes whether it is maximized, for a close that comes while minimized.
pub fn resized() {
    let Some(p) = MAIN.with(|m| m.borrow().as_ref().and_then(rune::WindowHandle::placement)) else { return };
    if !p.minimized {
        WAS_MAXIMIZED.with(|w| w.set(p.maximized));
    }
}

/// `App::on_window`: the main window was created.
pub fn created(window: &rune::WindowHandle) {
    MAIN.with(|m| *m.borrow_mut() = Some(window.clone()));
}

/// `App::on_close`: keeps where the main window is, for the next start.
pub fn remember(s: &mut State) {
    if s.is_placeholder {
        return;
    }
    let Some(placement) = MAIN.with(|m| m.borrow().as_ref().and_then(rune::WindowHandle::placement)) else { return };
    if let Some(saved) = Saved::of(&placement, WAS_MAXIMIZED.with(std::cell::Cell::get))
        && s.settings.window != Some(saved)
    {
        s.settings.window = Some(saved);
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
}
