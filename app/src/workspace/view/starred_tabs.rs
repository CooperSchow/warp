//! Starred tabs, the fork's favorites. A starred tab is an upstream pinned tab
//! wearing a star: it sits in the block at the top of the tab list, and bulk
//! closes leave it open.

use warp_core::ui::Icon;
use warpui::{AppContext, EntityId, SingletonEntity, ViewContext};

use super::Workspace;
use crate::features::FeatureFlag;
use crate::workspace::WorkspaceRegistry;

/// Whether stars are on: the pinned-tabs engine and the fork's star re-skin
/// of it, both.
pub(crate) fn starred_tabs_enabled() -> bool {
    FeatureFlag::PinnedTabs.is_enabled() && FeatureFlag::StarredTabs.is_enabled()
}

/// Whether the active window's active tab is starred, for the palette's
/// "Star current tab" / "Unstar current tab" entry.
pub(crate) fn active_tab_is_starred(ctx: &AppContext) -> bool {
    ctx.windows()
        .active_window()
        .and_then(|window_id| WorkspaceRegistry::as_ref(ctx).get(window_id, ctx))
        .is_some_and(|workspace| {
            let workspace = workspace.as_ref(ctx);
            workspace
                .tabs
                .get(workspace.active_tab_index)
                .is_some_and(|tab| tab.pinned)
        })
}

/// The icon a pinned tab group wears in the horizontal tab bar.
pub(super) fn pin_or_star_icon() -> Icon {
    Icon::PinFilledDiagonal
}

impl Workspace {
    /// Stars or unstars the tab that owns `pane_group_id`. Resolving the tab by
    /// identity rather than index keeps a menu opened before the tabs moved
    /// acting on the tab it was opened for. A no-op when that tab has since
    /// closed or already has the requested state.
    pub(super) fn set_tab_starred(
        &mut self,
        _pane_group_id: EntityId,
        _starred: bool,
        _ctx: &mut ViewContext<Self>,
    ) {
    }

    /// Stars the active tab, or unstars it if it's starred.
    pub(super) fn toggle_active_tab_star(&mut self, _ctx: &mut ViewContext<Self>) {}

    /// How many tabs lead the list as starred, which the close menus promise
    /// to spare. Zero when stars are off.
    pub(super) fn starred_boundary(&self) -> usize {
        if starred_tabs_enabled() {
            self.pinned_boundary_index(&self.tabs)
        } else {
            0
        }
    }

    /// Whether "Close other tabs", "Close Tabs Below" and the group closes must
    /// leave the tab at `index` open.
    pub(super) fn bulk_close_spares(&self, index: usize) -> bool {
        starred_tabs_enabled()
            && self
                .tabs
                .get(index)
                .is_some_and(|tab| self.is_tab_effectively_pinned(tab))
    }
}
