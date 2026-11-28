//! The window's adjustable sizes: how wide each pane's location tree and list columns are, and
//! how tall the transfer queue is and how wide its columns.
//! Kept in `layout.json` under the config folder so they survive a restart.

use serde::{Deserialize, Deserializer, Serialize};

/// Maximum number of panes supported (1, 2, or 4 in a 2×2 grid).
pub const MAX_PANES: usize = 4;

pub const TREE_W: f32 = 190.0;
pub const TREE_MIN: f32 = 120.0;
pub const TREE_MAX: f32 = 480.0;

/// The list's columns, in order: Nome, Tamanho, Data, Tipo. Each has its own width, as in
/// Windows Explorer; together they can be wider than the pane, which then scrolls sideways.
pub const COLS: [f32; 4] = [240.0, 86.0, 128.0, 130.0];
pub const COL_MIN: [f32; 4] = [100.0, 60.0, 80.0, 60.0];
pub const COL_MAX: f32 = 1200.0;

/// The transfer queue's height, dragged from its top edge.
pub const QUEUE_H: f32 = 170.0;
pub const QUEUE_H_MIN: f32 = 80.0;
pub const QUEUE_H_MAX: f32 = 700.0;

/// The queue's columns: Operação, Origem, Destino, Estado, Tamanho, Progresso, Velocidade,
/// Estimado.
pub const QUEUE_COLS: [f32; 8] = [92.0, 300.0, 240.0, 190.0, 128.0, 112.0, 96.0, 88.0];
pub const QUEUE_COL_MIN: f32 = 50.0;

/// The columns of Sincronizar's comparison: the path, size and date of the left side, the
/// action, the path, size and date of the right side, and the status. A column is known by
/// its place here whatever order the table shows them in.
pub const SYNC_COLS: [f32; 8] = [300.0, 82.0, 124.0, 100.0, 300.0, 82.0, 124.0, 140.0];
pub const SYNC_COL_MIN: f32 = 50.0;
/// The order the comparison's columns start in: as [`SYNC_COLS`] lists them.
pub const SYNC_ORDER: [u8; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
/// Default visual order of the 4 list columns: Nome (0), Tamanho (1), Data (2), Tipo (3).
pub const COL_ORDER: [usize; 4] = [0, 1, 2, 3];

fn deserialize_tree<'de, D: Deserializer<'de>>(d: D) -> Result<[f32; MAX_PANES], D::Error> {
    let v = Vec::<f32>::deserialize(d)?;
    Ok(std::array::from_fn(|i| v.get(i).copied().unwrap_or(TREE_W)))
}

fn deserialize_columns<'de, D: Deserializer<'de>>(d: D) -> Result<[[f32; 4]; MAX_PANES], D::Error> {
    let v = Vec::<[f32; 4]>::deserialize(d)?;
    Ok(std::array::from_fn(|i| v.get(i).copied().unwrap_or(COLS)))
}

/// The column orders saved, one per pane: a file from when there were two panes gives the
/// others the default order.
fn deserialize_col_order<'de, D: Deserializer<'de>>(d: D) -> Result<[[usize; 4]; MAX_PANES], D::Error> {
    let v = Vec::<[usize; 4]>::deserialize(d)?;
    Ok(std::array::from_fn(|i| v.get(i).copied().unwrap_or(COL_ORDER)))
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    #[serde(deserialize_with = "deserialize_tree")]
    pub tree: [f32; MAX_PANES],
    /// Per pane, the widths of Nome, Tamanho, Data and Tipo. (`cols`, from an earlier
    /// version that had three, is ignored.)
    #[serde(deserialize_with = "deserialize_columns")]
    pub columns: [[f32; 4]; MAX_PANES],
    /// Per pane, the visual order of the four columns (a permutation of `0..4`).
    #[serde(deserialize_with = "deserialize_col_order")]
    pub col_order: [[usize; 4]; MAX_PANES],
    pub queue_h: f32,
    pub queue_cols: [f32; 8],
    /// The widths of the comparison's columns, each at its place in [`SYNC_COLS`].
    pub sync_cols: [f32; 8],
    /// The comparison's columns from left to right, as dragged about by their headers.
    pub sync_order: [u8; 8],
}

impl Default for Layout {
    fn default() -> Layout {
        Layout {
            tree: [TREE_W; MAX_PANES],
            columns: [COLS; MAX_PANES],
            col_order: [COL_ORDER; MAX_PANES],
            queue_h: QUEUE_H,
            queue_cols: QUEUE_COLS,
            sync_cols: SYNC_COLS,
            sync_order: SYNC_ORDER,
        }
    }
}

/// Comparison column `i`'s width kept between [`SYNC_COL_MIN`] and [`COL_MAX`].
pub fn clamp_sync_col(i: usize, w: f32) -> f32 {
    if w.is_finite() { w.clamp(SYNC_COL_MIN, COL_MAX) } else { SYNC_COLS[i] }
}

/// `order` when it names each of the comparison's columns once, the starting order otherwise.
pub fn sync_order(order: [u8; 8]) -> [u8; 8] {
    let mut sorted = order;
    sorted.sort_unstable();
    if sorted == SYNC_ORDER { order } else { SYNC_ORDER }
}

/// The comparison's columns with the one at place `from` moved to place `to`, the ones
/// between making room.
pub fn moved_sync_col(order: [u8; 8], from: usize, to: usize) -> [u8; 8] {
    let mut order = order.to_vec();
    if from < order.len() && to < order.len() {
        let col = order.remove(from);
        order.insert(to, col);
    }
    std::array::from_fn(|i| order[i])
}

/// Ensures `order` is a valid permutation of `0..4`, falling back to [`COL_ORDER`].
pub fn valid_col_order(order: [usize; 4]) -> [usize; 4] {
    let mut seen = [false; 4];
    for i in order {
        if i >= 4 || seen[i] {
            return COL_ORDER;
        }
        seen[i] = true;
    }
    order
}

pub fn clamp_queue_h(h: f32) -> f32 {
    if h.is_finite() { h.clamp(QUEUE_H_MIN, QUEUE_H_MAX) } else { QUEUE_H }
}

/// Queue column `i`'s width kept between [`QUEUE_COL_MIN`] and [`COL_MAX`].
pub fn clamp_queue_col(i: usize, w: f32) -> f32 {
    if w.is_finite() { w.clamp(QUEUE_COL_MIN, COL_MAX) } else { QUEUE_COLS[i] }
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
        Layout {
            tree: l.tree.map(clamp_tree),
            columns: l.columns.map(|c| [0, 1, 2, 3].map(|i| clamp_col(i, c[i]))),
            col_order: l.col_order.map(valid_col_order),
            queue_h: clamp_queue_h(l.queue_h),
            queue_cols: std::array::from_fn(|i| clamp_queue_col(i, l.queue_cols[i])),
            sync_cols: std::array::from_fn(|i| clamp_sync_col(i, l.sync_cols[i])),
            sync_order: sync_order(l.sync_order),
        }
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
        assert_eq!(clamp_queue_h(10.0), QUEUE_H_MIN);
        assert_eq!(clamp_queue_h(f32::INFINITY), QUEUE_H);
        assert_eq!(clamp_queue_col(1, 5.0), QUEUE_COL_MIN);
        assert_eq!((clamp_sync_col(0, 5.0), clamp_sync_col(0, 400.0), clamp_sync_col(7, f32::NAN)), (SYNC_COL_MIN, 400.0, SYNC_COLS[7]));
    }

    /// The comparison's columns move about by their headers; an order that isn't each column
    /// once (a file changed by hand, or from a version with other columns) starts over.
    #[test]
    fn the_comparison_columns_keep_an_order_that_names_each_once() {
        assert_eq!(moved_sync_col(SYNC_ORDER, 2, 1), [0, 2, 1, 3, 4, 5, 6, 7]);
        assert_eq!(moved_sync_col(SYNC_ORDER, 0, 3), [1, 2, 3, 0, 4, 5, 6, 7]);
        assert_eq!(moved_sync_col(SYNC_ORDER, 7, 0), [7, 0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(moved_sync_col(SYNC_ORDER, 4, 4), SYNC_ORDER);
        assert_eq!(moved_sync_col(SYNC_ORDER, 9, 0), SYNC_ORDER);
        assert_eq!(sync_order([7, 0, 1, 2, 3, 4, 5, 6]), [7, 0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(sync_order([0, 0, 1, 2, 3, 4, 5, 6]), SYNC_ORDER);
        assert_eq!(sync_order([0, 1, 2, 3, 4, 5, 6, 9]), SYNC_ORDER);
        let l: Layout = serde_json::from_str(r#"{"sync_order":[1,0,2,3,4,5,6,7],"sync_cols":[400.0,82.0,124.0,100.0,300.0,82.0,124.0,140.0]}"#).unwrap();
        assert_eq!((l.sync_order[0], l.sync_cols[0]), (1, 400.0));
    }

    #[test]
    fn a_saved_layout_from_an_older_version_still_loads() {
        // The pane split it held is gone; the 2 saved tree widths stay and the new panes get defaults.
        let l: Layout = serde_json::from_str(r#"{"split":0.62,"tree":[150.0,300.0],"cols":[[61.0,138.0,124.0],[86.0,132.0,124.0]]}"#).unwrap();
        assert_eq!(l, Layout { tree: [150.0, 300.0, TREE_W, TREE_W], ..Layout::default() });
        let l: Layout = serde_json::from_str("{}").unwrap();
        assert_eq!(l, Layout::default());
        // A column order saved with two panes: the other two take the default one.
        let l: Layout = serde_json::from_str(r#"{"col_order":[[1,0,2,3],[3,2,1,0]]}"#).unwrap();
        assert_eq!(l.col_order, [[1, 0, 2, 3], [3, 2, 1, 0], COL_ORDER, COL_ORDER]);
    }
}
