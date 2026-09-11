//! Starred tabs, the fork's favorites. A starred tab is an upstream pinned tab
//! wearing a star: it sits in the block at the top of the tab list, and bulk
//! closes leave it open.
//!
//! The star has one look wherever it appears: a 10 px solid star in the ink of
//! the title it leads, taking no clicks of its own.

use warp_core::ui::theme::Fill;
use warp_core::ui::Icon;
use warpui::elements::{Align, ConstrainedBox, Element};
use warpui::{AppContext, EntityId, SingletonEntity, ViewContext};

use super::Workspace;
use crate::features::FeatureFlag;
use crate::tab::bulk_close_label;
use crate::workspace::WorkspaceRegistry;

/// The star's box: about a 12 px title's cap height, plus the overshoot a
/// pointed glyph needs to look as tall as the capitals beside it. The glyph is
/// drawn so that a box centred on the title's line puts it on the capitals.
pub(crate) const STAR_SIZE: f32 = 10.;

/// The gap between the star and the title it leads, the same as the gap before
/// the unread dot.
pub(crate) const STAR_TITLE_GAP: f32 = 4.;

/// What the star takes from the head of a row. A starred row's later lines are
/// indented by it, so each line's text starts where the title's does.
pub(crate) const STAR_SLOT_WIDTH: f32 = STAR_SIZE + STAR_TITLE_GAP;

/// Whether stars are on: the pinned-tabs engine and the fork's star re-skin
/// of it, both.
pub(crate) fn starred_tabs_enabled() -> bool {
    FeatureFlag::PinnedTabs.is_enabled() && FeatureFlag::StarredTabs.is_enabled()
}

/// The star, in `ink`. It's only a mark, with no hover state and no click of
/// its own, so a click on it lands on the row like a click anywhere else and a
/// stray one can never unstar a tab.
pub(crate) fn render_star(ink: Fill) -> Box<dyn Element> {
    ConstrainedBox::new(Icon::StarFilled.to_warpui_icon(ink).finish())
        .with_width(STAR_SIZE)
        .with_height(STAR_SIZE)
        .finish()
}

/// What a starred tab or group wears in the horizontal tab bar, in the
/// `slot_size` square upstream sizes for its pin: the star, centred at its own
/// size, or upstream's pin when stars are off.
pub(crate) fn render_pin_slot_mark(slot_size: f32, ink: Fill) -> Box<dyn Element> {
    let mark = if starred_tabs_enabled() {
        Align::new(render_star(ink)).finish()
    } else {
        Icon::PinFilledDiagonal.to_warpui_icon(ink).finish()
    };
    ConstrainedBox::new(mark)
        .with_width(slot_size)
        .with_height(slot_size)
        .finish()
}

/// Whether a vertical tabs row wears its tab's star: the first row of a starred
/// tab, and no other, so a split tab drawn one row per pane has one star. A
/// member of a starred group isn't starred itself; its group header wears the
/// star, as the group's one row.
pub(super) fn row_shows_star(tab_is_starred: bool, is_first_row_of_tab: bool) -> bool {
    starred_tabs_enabled() && tab_is_starred && is_first_row_of_tab
}

/// Whether upstream's pin overlay shows on a pinned row or group header. That's
/// upstream's rule, which hides it while the hover controls show, except that
/// stars replace it outright: it would sit over the unread dot.
pub(super) fn shows_pin_overlay(is_pinned: bool, hidden_by_hover: bool) -> bool {
    FeatureFlag::PinnedTabs.is_enabled()
        && !FeatureFlag::StarredTabs.is_enabled()
        && is_pinned
        && !hidden_by_hover
}

/// Where the vertical tabs panel draws the hairline under the starred tabs: the
/// position, among `shown` (the indices of the tabs the panel shows, in list
/// order, after any search), of the first tab past the starred block. Only when
/// a starred tab shows above it, so there's no line when a search or the tabs
/// themselves leave either side empty, and none with stars off, when the block
/// is empty.
pub(super) fn starred_divider_position(
    shown: impl IntoIterator<Item = usize>,
    starred_boundary: usize,
) -> Option<usize> {
    let mut starred_shown = false;
    for (position, index) in shown.into_iter().enumerate() {
        if index >= starred_boundary {
            return starred_shown.then_some(position);
        }
        starred_shown = true;
    }
    None
}

/// Runs `read` on the active window's workspace. The palette's labels read
/// the tab a key would act on through it.
fn with_active_workspace<T>(ctx: &AppContext, read: impl FnOnce(&Workspace) -> T) -> Option<T> {
    let window_id = ctx.windows().active_window()?;
    let workspace = WorkspaceRegistry::as_ref(ctx).get(window_id, ctx)?;
    Some(read(workspace.as_ref(ctx)))
}

/// The palette's words for ⌃⌘S on the active window's active tab, which follow
/// the tab menu's: "unstar current tab" on a starred tab, and "(leaves group)"
/// when starring pulls the tab out of its group. `None` keeps "Star current
/// tab".
pub(crate) fn active_tab_star_description(ctx: &AppContext) -> Option<String> {
    with_active_workspace(ctx, star_description).flatten()
}

/// The words `active_tab_star_description` gives `workspace`'s active tab.
pub(super) fn star_description(workspace: &Workspace) -> Option<String> {
    let tab = workspace.tabs.get(workspace.active_tab_index)?;
    let description = match (tab.pinned, tab.group_id.is_some()) {
        (true, _) => "unstar current tab",
        (false, true) => "star current tab (leaves group)",
        (false, false) => return None,
    };
    Some(description.to_owned())
}

/// A bulk close the palette runs on the active tab.
#[derive(Clone, Copy)]
pub(crate) enum PaletteBulkClose {
    /// Every tab but the active one.
    OtherTabs,
    /// The tabs after the active one: below it, or to its right in the
    /// horizontal tab bar.
    TabsAfter,
}

/// The palette's label for a bulk close on the active window's active tab:
/// `label`, with "(keep starred)" exactly when the close would spare a starred
/// tab, by the tab menu's own rule. `None` when there's no active workspace or
/// the close would close nothing, which leaves the binding's own wording.
pub(crate) fn active_tab_bulk_close_description(
    label: &str,
    close: PaletteBulkClose,
    ctx: &AppContext,
) -> Option<String> {
    with_active_workspace(ctx, |workspace| {
        bulk_close_description(workspace, label, close)
    })
    .flatten()
}

/// The label `active_tab_bulk_close_description` gives `workspace`'s active tab.
pub(super) fn bulk_close_description(
    workspace: &Workspace,
    label: &str,
    close: PaletteBulkClose,
) -> Option<String> {
    let active = workspace.active_tab_index;
    let tab_count = workspace.tabs.len();
    let starred_boundary = workspace.starred_boundary();
    match close {
        PaletteBulkClose::OtherTabs => bulk_close_label(
            label,
            (0..tab_count).filter(|index| *index != active),
            starred_boundary,
        ),
        PaletteBulkClose::TabsAfter => {
            bulk_close_label(label, active + 1..tab_count, starred_boundary)
        }
    }
}

impl Workspace {
    /// Stars or unstars the tab that owns `pane_group_id`, through upstream's
    /// pin, which moves it to the end of the starred block or just past it, and
    /// saves. Resolving the tab by identity rather than index keeps a menu
    /// opened before the tabs moved acting on the tab it was opened for. A no-op
    /// without stars, when that tab has since closed, or when it already has the
    /// requested state.
    pub(super) fn set_tab_starred(
        &mut self,
        pane_group_id: EntityId,
        starred: bool,
        ctx: &mut ViewContext<Self>,
    ) {
        if !starred_tabs_enabled() {
            return;
        }
        let Some(index) = self.tab_index_of(pane_group_id) else {
            return;
        };
        if self.tabs[index].pinned == starred {
            return;
        }
        if starred {
            self.pin_tab(index, ctx);
        } else {
            self.unpin_tab(index, ctx);
        }
        // The tab moved; keep it in view where it landed.
        if let Some(index) = self.tab_index_of(pane_group_id) {
            self.reveal_tab_in_vertical_panel(index, ctx);
        }
    }

    /// Stars the active tab, or unstars it if it's starred.
    pub(super) fn toggle_active_tab_star(&mut self, ctx: &mut ViewContext<Self>) {
        let Some(tab) = self.tabs.get(self.active_tab_index) else {
            return;
        };
        let (pane_group_id, starred) = (tab.pane_group.id(), tab.pinned);
        self.set_tab_starred(pane_group_id, !starred, ctx);
    }

    /// The index of the open tab that owns `pane_group_id`.
    fn tab_index_of(&self, pane_group_id: EntityId) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.pane_group.id() == pane_group_id)
    }

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

#[cfg(test)]
#[path = "starred_tabs_tests.rs"]
mod tests;
