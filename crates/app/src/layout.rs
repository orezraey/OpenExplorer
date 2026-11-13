//! The window's adjustable sizes: how wide each pane's location tree and list columns are.
//! Kept in `layout.json` under the config folder so they survive a restart.

use serde::{Deserialize, Serialize};

pub const TREE_W: f32 = 190.0;
pub const TREE_MIN: f32 = 120.0;
pub const TREE_MAX: f32 = 480.0;

/// The list's columns, in order: Nome, Tamanho, Data, Tipo. Each has its own width, as in
/// Windows Explorer; together they can be wider than the pane, which then scrolls sideways.
pub const COLS: [f32; 4] = [240.0, 86.0, 128.0, 130.0];
pub const COL_MIN: [f32; 4] = [100.0, 60.0, 80.0, 60.0];
pub const COL_MAX: f32 = 1200.0;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    pub tree: [f32; 2],
    /// Per pane, the widths of Nome, Tamanho, Data and Tipo. (`cols`, from an earlier
    /// version that had three, is ignored.)
    pub columns: [[f32; 4]; 2],
}

impl Default for Layout {
    fn default() -> Layout {
        Layout { tree: [TREE_W; 2], columns: [COLS; 2] }
    }
}

/// Column `i`'s width kept between its minimum and [`COL_MAX`].
pub fn clamp_col(i: usize, w: f32) -> f32 {
    if w.is_finite() { w.clamp(COL_MIN[i], COL_MAX) } else { COLS[i] }
}

pub fn clamp_tree(w: f32) -> f32 {
    if w.is_finite() { w.clamp(TREE_MIN, TREE_MAX) } else { TREE_W }
}

fn file() -> std::path::PathBuf {
    openexplorer_gdrive::config_dir().join("layout.json")
}

impl Layout {
    pub fn load() -> Layout {
        let l: Layout = std::fs::read(file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Layout { tree: l.tree.map(clamp_tree), columns: l.columns.map(|c| [0, 1, 2, 3].map(|i| clamp_col(i, c[i]))) }
    }

    /// Saved when a drag ends, not on every move.
    pub fn save(&self) {
        let Ok(text) = serde_json::to_vec_pretty(self) else { return };
        let _ = std::fs::create_dir_all(openexplorer_gdrive::config_dir());
        let _ = std::fs::write(file(), text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_widths_stay_in_bounds() {
        assert_eq!(clamp_tree(200.0), 200.0);
        assert_eq!(clamp_tree(40.0), TREE_MIN);
        assert_eq!(clamp_tree(9999.0), TREE_MAX);
        assert_eq!(clamp_tree(f32::NAN), TREE_W);
        assert_eq!(clamp_col(1, 10.0), COL_MIN[1]);
        assert_eq!(clamp_col(3, 2000.0), COL_MAX);
    }

    #[test]
    fn a_saved_layout_from_an_older_version_still_loads() {
        // The pane split it held is gone; the tree widths stay.
        let l: Layout = serde_json::from_str(r#"{"split":0.62,"tree":[150.0,300.0],"cols":[[61.0,138.0,124.0],[86.0,132.0,124.0]]}"#).unwrap();
        assert_eq!(l, Layout { tree: [150.0, 300.0], columns: [COLS; 2] });
        let l: Layout = serde_json::from_str("{}").unwrap();
        assert_eq!(l, Layout::default());
    }
}
