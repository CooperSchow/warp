//! The Starred zone: the block of starred tabs at the top of the vertical tabs
//! panel, kept on screen while the rest of the list scrolls.

use warpui::AppContext;

use super::Workspace;
use crate::tab::uses_vertical_tabs;

/// How much of the list's height the Starred zone may take before it scrolls
/// on its own: its share of the rows, but never less than half. `None` when
/// either side is empty, since the panel then shows one list.
pub(super) fn starred_zone_share(starred_rows: usize, other_rows: usize) -> Option<f32> {
    if starred_rows == 0 || other_rows == 0 {
        return None;
    }
    Some((starred_rows as f32 / (starred_rows + other_rows) as f32).max(0.5))
}

impl Workspace {
    /// Where the vertical tabs panel splits into the Starred zone and the rest:
    /// the index of the first tab below the zone, or `None` when the panel
    /// shows one list.
    pub(super) fn starred_zone_split(&self, _ctx: &AppContext) -> Option<usize> {
        None
    }

    /// Scrolls the vertical tabs panel, when it's showing, so the tab at
    /// `index` is in view in whichever of its lists holds it.
    pub(super) fn reveal_tab_in_vertical_panel(&self, index: usize, ctx: &AppContext) {
        if self.vertical_tabs_panel_open && uses_vertical_tabs(ctx) {
            self.vertical_tabs_panel.scroll_to_tab(index);
        }
    }
}
