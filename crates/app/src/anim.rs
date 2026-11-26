//! Explorer-style fluid animations inspired by GPU-accelerated Explorer views:
//! - Gliding hover highlight (80 ms ease-out cubic)
//! - Clipboard Copy/Cut icon wiggle (360 ms period, per-item staggered phase)
//! - Smooth Ctrl+wheel / pinch list zoom (180 ms ease-out cubic)
//! - Smooth selection & tree chevron transitions

use std::collections::HashMap;

use rune::{Anim, Curve, Easing, Spring};

/// Default file list row height (points), matching `crate::ROW_H`.
pub const ROW_H_DEFAULT: f32 = crate::ROW_H;
/// Minimum zoomed-out row height (compact view).
pub const ROW_H_MIN: f32 = 18.0;
/// Maximum zoomed-in row height (large icon/row view).
pub const ROW_H_MAX: f32 = 56.0;
/// Step size per Ctrl+wheel notch (points).
pub const ZOOM_STEP: f32 = 3.0;

/// Hover highlight glide duration (80 ms, matching Explorer GPU mod).
pub const HOVER_DURATION: f32 = 0.080;
/// Zoom, sort reorder & keyboard selection glide duration (180 ms, matching Explorer GPU mod).
pub const ZOOM_DURATION: f64 = 0.180;
/// Lasso / marquee fade-out duration on release (140 ms).
pub const LASSO_FADE_DURATION: f64 = 0.140;
/// Clipboard icon wiggle full oscillation period (360 ms = 2 × 180 ms).
pub const WIGGLE_PERIOD: f64 = 0.360;

/// Standard cubic ease-out: `1 - (1 - t)^3`.
#[inline]
pub fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let inv = 1.0 - t;
    1.0 - inv * inv * inv
}

/// 80 ms ease-out cubic curve for the gliding hover highlight and hover fades.
pub fn hover_curve() -> Anim {
    Anim::new(Curve::tween(HOVER_DURATION, Easing::cubic(0.215, 0.61, 0.355, 1.0)))
}

/// Crisp, non-bouncy spring for selection highlights and tree chevron rotation.
pub fn ui_spring() -> Spring {
    Spring::new(0.16, 0.0)
}

/// Deterministic FNV-1a hash of an entry id for per-item wiggle phase staggering.
fn stable_hash(id: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in id.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Maximum wiggle angle in degrees for an icon of `icon_size` points:
/// keeps small icons legible (~8°) without swinging large icons wildly (down to 1.2°).
#[inline]
pub fn wiggle_amplitude_deg(icon_size: f32) -> f32 {
    (192.0 / icon_size.max(1.0)).clamp(1.2, 8.0)
}

/// Clipboard Copy/Cut icon rotation in radians at `elapsed_secs` since pickup,
/// with a stable per-item 0..120 ms start delay so multiple selected items wiggle organically.
pub fn wiggle_radians(icon_size: f32, elapsed_secs: f64, id: &str) -> f32 {
    let delay_secs = (stable_hash(id) % 121) as f64 * 0.001;
    if elapsed_secs <= delay_secs {
        return 0.0;
    }
    let amp = wiggle_amplitude_deg(icon_size);
    let phase = ((elapsed_secs - delay_secs) % WIGGLE_PERIOD) * (std::f64::consts::TAU / WIGGLE_PERIOD);
    (amp * phase.sin() as f32).to_radians()
}

/// Per-pane smooth list zoom state (row height, icon size, font size).
#[derive(Clone, Copy, Debug)]
pub struct ZoomState {
    pub target: f32,
    pub current: f32,
    from: f32,
    started: f64,
    animating: bool,
}

impl Default for ZoomState {
    fn default() -> Self {
        Self {
            target: ROW_H_DEFAULT,
            current: ROW_H_DEFAULT,
            from: ROW_H_DEFAULT,
            started: 0.0,
            animating: false,
        }
    }
}

impl ZoomState {
    /// Adjust zoom by `delta_steps` (positive zooms in, negative zooms out).
    /// Retargets seamlessly from the current in-flight height.
    pub fn step_by(&mut self, delta_steps: f32, now: f64, animate: bool) -> bool {
        let next = (self.target + delta_steps * ZOOM_STEP).clamp(ROW_H_MIN, ROW_H_MAX);
        self.set_target(next, now, animate)
    }

    /// Adjust zoom multiplicatively (for trackpad pinch gestures).
    pub fn scale_by(&mut self, factor: f32, now: f64, animate: bool) -> bool {
        if !factor.is_finite() || factor <= 0.0 {
            return false;
        }
        let next = (self.target * factor).clamp(ROW_H_MIN, ROW_H_MAX);
        self.set_target(next, now, animate)
    }

    /// Reset zoom back to the default row height.
    pub fn reset(&mut self, now: f64, animate: bool) -> bool {
        self.set_target(ROW_H_DEFAULT, now, animate)
    }

    fn set_target(&mut self, next: f32, now: f64, animate: bool) -> bool {
        if (next - self.target).abs() < 0.05 && !self.animating {
            return false;
        }
        if self.animating {
            self.tick(now);
        }
        self.target = next;
        if !animate || (self.target - self.current).abs() < 0.05 {
            self.current = self.target;
            self.from = self.target;
            self.animating = false;
            return true;
        }
        self.from = self.current;
        self.started = now;
        self.animating = true;
        true
    }

    /// Advance the zoom animation to `now`. Returns `true` if `current` changed.
    pub fn tick(&mut self, now: f64) -> bool {
        if !self.animating {
            return false;
        }
        let elapsed = (now - self.started).max(0.0);
        let progress = (elapsed / ZOOM_DURATION).clamp(0.0, 1.0) as f32;
        let eased = ease_out_cubic(progress);
        let next = self.from + (self.target - self.from) * eased;
        let moved = (next - self.current).abs() > 1e-4 || progress >= 1.0;
        if progress >= 1.0 {
            self.current = self.target;
            self.animating = false;
        } else {
            self.current = next;
        }
        moved
    }

    pub fn is_animating(&self) -> bool {
        self.animating
    }

    /// Current presented row height in points.
    #[inline]
    pub fn row_height(&self) -> f32 {
        self.current
    }

    /// Current icon size in points: exact 16.0 at default row height, scaling smoothly up to 40.0.
    #[inline]
    pub fn icon_size(&self) -> f32 {
        if (self.current - ROW_H_DEFAULT).abs() < 0.01 {
            16.0
        } else {
            (16.0 + (self.current - ROW_H_DEFAULT) * 0.72).clamp(13.0, 40.0)
        }
    }

    /// Scales a base font size smoothly with the current zoom level.
    #[inline]
    pub fn font_size(&self, base: f32) -> f32 {
        if (self.current - ROW_H_DEFAULT).abs() < 0.01 {
            base
        } else {
            let factor = (1.0 + (self.current - ROW_H_DEFAULT) / 110.0).clamp(0.88, 1.32);
            (base * factor * 2.0).round() * 0.5
        }
    }

    /// Zoom percentage relative to default (100% at `ROW_H_DEFAULT`).
    #[inline]
    pub fn percent(&self) -> u32 {
        ((self.target / ROW_H_DEFAULT) * 100.0).round() as u32
    }
}

/// Cubic Hermite with zero arrival velocity (matching Explorer GPU mod's `SelectionAxis`).
/// Carries the in-flight tangent on rapid key repeats so holding ↑/↓ glides continuously
/// without stuttering or overshooting.
fn hermite_axis(from: f32, to: f32, tangent: f32, progress: f32) -> (f32, f32) {
    let delta = to - from;
    let tangent = if delta >= 0.0 {
        tangent.clamp(0.0, 3.0 * delta)
    } else {
        tangent.clamp(3.0 * delta, 0.0)
    };
    let t = progress.clamp(0.0, 1.0);
    let t2 = t * t;
    let pos = from + delta * (3.0 * t2 - 2.0 * t2 * t) + tangent * (t - 2.0 * t2 + t2 * t);
    let vel = delta * (6.0 * t - 6.0 * t2) + tangent * (1.0 - 4.0 * t + 3.0 * t2);
    (pos, vel)
}

/// Gliding keyboard selection highlight for a single pane.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyboardSel {
    active: bool,
    from_row: f32,
    to_row: usize,
    tangent: f32,
    started: f64,
    current_row: f32,
}

impl KeyboardSel {
    /// Start or retarget a glide from `from_row` to `to_row`.
    pub fn start(&mut self, from_row: usize, to_row: usize, now: f64) {
        if from_row == to_row && !self.active {
            return;
        }
        let (cur, vel) = if self.active {
            let p = ((now - self.started).max(0.0) / ZOOM_DURATION).clamp(0.0, 1.0) as f32;
            hermite_axis(self.from_row, self.to_row as f32, self.tangent, p)
        } else {
            (from_row as f32, 3.0 * (to_row as f32 - from_row as f32))
        };
        self.from_row = cur;
        self.to_row = to_row;
        self.tangent = vel;
        self.started = now;
        self.current_row = cur;
        self.active = true;
    }

    pub fn stop(&mut self) {
        self.active = false;
    }

    pub fn tick(&mut self, now: f64) -> bool {
        if !self.active {
            return false;
        }
        let p = ((now - self.started).max(0.0) / ZOOM_DURATION).clamp(0.0, 1.0) as f32;
        if p >= 1.0 {
            self.current_row = self.to_row as f32;
            self.active = false;
            return true;
        }
        self.current_row = hermite_axis(self.from_row, self.to_row as f32, self.tangent, p).0;
        true
    }

    #[inline]
    pub fn is_active(&self) -> bool {
        self.active
    }

    #[inline]
    pub fn target_row(&self) -> usize {
        self.to_row
    }

    #[inline]
    pub fn presented_row(&self) -> f32 {
        self.current_row
    }
}

/// Fading marquee/lasso rectangle after the user releases the mouse button.
#[derive(Clone, Copy, Debug)]
pub struct LassoFade {
    pub pane: usize,
    pub rect: [f32; 4],
    pub started: f64,
}

impl LassoFade {
    /// Current opacity in `[0, 1]` at `now`.
    #[inline]
    pub fn opacity(&self, now: f64) -> f32 {
        let p = ((now - self.started).max(0.0) / LASSO_FADE_DURATION).clamp(0.0, 1.0) as f32;
        (1.0 - ease_out_cubic(p)).clamp(0.0, 1.0)
    }

    #[inline]
    pub fn done(&self, now: f64) -> bool {
        (now - self.started) >= LASSO_FADE_DURATION
    }
}

/// Smooth vertical slide of visible rows when re-sorting a folder.
#[derive(Clone, Debug, Default)]
pub struct RowReorder {
    offsets: HashMap<String, f32>,
    started: f64,
    progress: f32,
    active: bool,
}

impl RowReorder {
    pub fn start(&mut self, offsets: HashMap<String, f32>, now: f64) {
        if offsets.is_empty() {
            self.active = false;
            self.offsets.clear();
            return;
        }
        self.offsets = offsets;
        self.started = now;
        self.progress = 0.0;
        self.active = true;
    }

    pub fn clear(&mut self) {
        self.active = false;
        self.offsets.clear();
    }

    pub fn tick(&mut self, now: f64) -> bool {
        if !self.active {
            return false;
        }
        let p = ((now - self.started).max(0.0) / ZOOM_DURATION).clamp(0.0, 1.0) as f32;
        self.progress = ease_out_cubic(p);
        if p >= 1.0 {
            self.active = false;
            self.offsets.clear();
        }
        true
    }

    #[inline]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Vertical pixel offset to apply to entry `id` during the reorder glide.
    #[inline]
    pub fn offset_px(&self, id: &str, row_h: f32) -> f32 {
        if !self.active {
            return 0.0;
        }
        self.offsets.get(id).copied().unwrap_or(0.0) * (1.0 - self.progress) * row_h
    }
}

/// Active header column drag-to-reorder state.
#[derive(Clone, Copy, Debug)]
pub struct ColDrag {
    pub pane: usize,
    pub col_id: usize,
    pub original_order: [usize; 4],
    pub start_x: f32,
    pub visual_dx: f32,
    pub moved: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ease_out_cubic_endpoints_and_monotonic() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        let mid = ease_out_cubic(0.5);
        assert!(mid > 0.5 && mid < 1.0);
    }

    #[test]
    fn zoom_interpolates_and_settles() {
        let mut z = ZoomState::default();
        assert_eq!(z.row_height(), ROW_H_DEFAULT);
        assert_eq!(z.icon_size(), 16.0);
        assert!(z.step_by(2.0, 1.0, true));
        assert!(z.is_animating());
        assert_eq!(z.target, ROW_H_DEFAULT + 6.0);
        assert!(z.tick(1.09));
        assert!(z.row_height() > ROW_H_DEFAULT && z.row_height() < z.target);
        assert!(z.tick(1.20));
        assert!(!z.is_animating());
        assert_eq!(z.row_height(), z.target);
    }

    #[test]
    fn zoom_snaps_when_animation_disabled() {
        let mut z = ZoomState::default();
        assert!(z.step_by(1.0, 0.0, false));
        assert!(!z.is_animating());
        assert_eq!(z.row_height(), ROW_H_DEFAULT + ZOOM_STEP);
    }

    #[test]
    fn wiggle_amplitude_and_phase_are_bounded() {
        assert_eq!(wiggle_amplitude_deg(16.0), 8.0);
        assert!(wiggle_amplitude_deg(40.0) < 8.0 && wiggle_amplitude_deg(40.0) > 1.2);
        assert_eq!(wiggle_radians(16.0, 0.0, "folder-a"), 0.0);
        let max_rad = 8.0_f32.to_radians() + 1e-4;
        for step in 0..40 {
            let a = wiggle_radians(16.0, step as f64 * 0.015, "folder-a");
            assert!(a.abs() <= max_rad);
        }
    }

    #[test]
    fn keyboard_selection_glides_and_settles() {
        let mut ks = KeyboardSel::default();
        ks.start(2, 5, 1.0);
        assert!(ks.is_active());
        assert!(ks.tick(1.09));
        assert!(ks.presented_row() > 2.0 && ks.presented_row() < 5.0);
        assert!(ks.tick(1.20));
        assert!(!ks.is_active());
        assert_eq!(ks.presented_row(), 5.0);
    }

    #[test]
    fn row_reorder_decays_to_zero() {
        let mut rr = RowReorder::default();
        let mut map = HashMap::new();
        map.insert("a".to_string(), 3.0);
        rr.start(map, 10.0);
        assert!(rr.is_active());
        assert!(rr.tick(10.09));
        let mid = rr.offset_px("a", 22.0);
        assert!(mid > 0.0 && mid < 66.0);
        assert!(rr.tick(10.20));
        assert!(!rr.is_active());
        assert_eq!(rr.offset_px("a", 22.0), 0.0);
    }
}
