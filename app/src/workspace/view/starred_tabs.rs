//! The floating block at the top of the tab list, which the fork's emoji tags
//! ride on (see `tab_tags`). With the "Float tagged tabs to the top" setting
//! on, an ungrouped tab floats while it wears emoji: it's an upstream pinned
//! tab, so it sits in the block and bulk closes leave it open. A starred tab
//! group floats the same way and wears ⭐.
//!
//! The flag and the names here keep the word "starred" from before tags, when
//! the one mark a floating tab could wear was a star.

use warp_core::ui::theme::Fill;
use warp_core::ui::Icon;
use warpui::elements::{Align, ConstrainedBox, Element};
use warpui::fonts::FamilyId;
use warpui::{AppContext, EntityId, SingletonEntity, ViewContext};

use super::tab_tags::{render_tags, worn_tags, TabTags, TITLE_TAG_SIZE};
use super::Workspace;
use crate::features::FeatureFlag;
use crate::tab::{bulk_close_label, uses_vertical_tabs};
use crate::workspace::tab_settings::TabSettings;
use crate::workspace::WorkspaceRegistry;

/// Whether emoji tags are on: the pinned-tabs engine and the fork's tags on
/// top of it, both.
pub(crate) fn starred_tabs_enabled() -> bool {
    FeatureFlag::PinnedTabs.is_enabled() && FeatureFlag::StarredTabs.is_enabled()
}

/// Whether tagged tabs float: tags are on, and so is the "Float tagged tabs to
/// the top" setting.
pub(crate) fn tagged_tabs_float(app: &AppContext) -> bool {
    starred_tabs_enabled() && *TabSettings::as_ref(app).float_tagged_tabs
}

/// What a starred tab group wears in the horizontal tab bar, in the
/// `slot_size` square upstream sizes for its pin: ⭐, centred, or upstream's
/// pin when tags are off. A tab wears its emoji before its title instead.
pub(crate) fn render_pin_slot_mark(
    slot_size: f32,
    ink: Fill,
    font_family: FamilyId,
) -> Box<dyn Element> {
    let mark = if starred_tabs_enabled() {
        Align::new(render_tags(&TabTags::star(), TITLE_TAG_SIZE, font_family)).finish()
    } else {
        Icon::PinFilledDiagonal.to_warpui_icon(ink).finish()
    };
    ConstrainedBox::new(mark)
        .with_width(slot_size)
        .with_height(slot_size)
        .finish()
}

/// Whether upstream's pin overlay shows on a pinned row or group header. That's
/// upstream's rule, which hides it while the hover controls show, except that
/// tags replace it outright: it would sit over the unread dot.
pub(super) fn shows_pin_overlay(is_pinned: bool, hidden_by_hover: bool) -> bool {
    FeatureFlag::PinnedTabs.is_enabled()
        && !FeatureFlag::StarredTabs.is_enabled()
        && is_pinned
        && !hidden_by_hover
}

/// Where the vertical tabs panel draws the hairline under the floating tabs:
/// the position, among `shown` (the indices of the tabs the panel shows, in
/// list order, after any search), of the first tab past the floating block.
/// Only when a floating tab shows above it, so there's no line when a search
/// or the tabs themselves leave either side empty, and none with tags off,
/// when the block is empty.
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
/// `label`, with "(keep tagged)" exactly when the close would spare a floating
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
    /// Floats or sinks the tab that owns `pane_group_id`, through upstream's
    /// pin, which moves it to the end of the floating block or just past it.
    /// Changing a tab's emoji calls it, to float the tab as its first goes on
    /// and sink it as its last comes off. Resolving the tab by identity rather
    /// than index keeps it acting on the right tab however the tabs have moved.
    /// A no-op without tags, when that tab has since closed, or when it already
    /// has the requested state.
    pub(super) fn set_tab_starred(
        &mut self,
        pane_group_id: EntityId,
        starred: bool,
        ctx: &mut ViewContext<Self>,
    ) {
        if !starred_tabs_enabled() || !self.float_tab(pane_group_id, starred, ctx) {
            return;
        }
        // The tab moved; when the vertical tabs panel shows it, keep it in view
        // where it landed.
        if self.vertical_tabs_panel_open && uses_vertical_tabs(ctx) {
            if let Some(index) = self.tab_index_of(pane_group_id) {
                self.vertical_tabs_panel.scroll_to_tab(index);
            }
        }
    }

    /// Floats or sinks the tab that owns `pane_group_id` through upstream's
    /// pin, which moves it. Returns whether it moved.
    fn float_tab(
        &mut self,
        pane_group_id: EntityId,
        float: bool,
        ctx: &mut ViewContext<Self>,
    ) -> bool {
        let Some(index) = self.tab_index_of(pane_group_id) else {
            return false;
        };
        if self.tabs[index].pinned == float {
            return false;
        }
        if float {
            self.pin_tab(index, ctx);
        } else {
            self.unpin_tab(index, ctx);
        }
        true
    }

    /// Brings every tab's float into line with its emoji and the "Float tagged
    /// tabs to the top" setting. With it on, an ungrouped tab that wears emoji
    /// floats, and one floating with none of its own keeps floating, wearing
    /// ⭐, as a tab starred before tags existed does; an explicit unpin stays
    /// unpinned. With it off, none floats: a floating tab with no emoji of its
    /// own first takes its ⭐ as a real emoji, so sinking it loses nothing it
    /// wore. A group's members go where their group goes, whatever they wear.
    /// Runs after a restore, when the setting changes, after undo-close, and
    /// after every action that saves, which covers a tagged tab leaving its
    /// group.
    pub(super) fn sync_tag_floats(&mut self, ctx: &mut ViewContext<Self>) {
        if !starred_tabs_enabled() {
            return;
        }
        let float = tagged_tabs_float(ctx);
        let ungrouped: Vec<EntityId> = self
            .tabs
            .iter()
            .filter(|tab| tab.group_id.is_none())
            .map(|tab| tab.pane_group.id())
            .collect();
        let mut changed = false;
        if !float {
            for pane_group_id in &ungrouped {
                let Some(index) = self.tab_index_of(*pane_group_id) else {
                    continue;
                };
                let tab = &mut self.tabs[index];
                if tab.pinned && tab.tags.is_empty() {
                    tab.tags = TabTags::star();
                    changed = true;
                }
            }
        }
        let floats = |workspace: &Self, pane_group_id: EntityId| {
            workspace.tab_index_of(pane_group_id).is_some_and(|index| {
                let tab = &workspace.tabs[index];
                float && !worn_tags(&tab.tags, tab.pinned).is_empty()
            })
        };
        // Sink from the bottom of the block up, and float from the top of the
        // list down. Each tab lands at the block's edge, just past the ones
        // already moved, so the tabs on both sides keep their order.
        for &pane_group_id in ungrouped.iter().rev() {
            if !floats(self, pane_group_id) {
                changed |= self.float_tab(pane_group_id, false, ctx);
            }
        }
        for &pane_group_id in &ungrouped {
            if floats(self, pane_group_id) {
                changed |= self.float_tab(pane_group_id, true, ctx);
            }
        }
        if changed {
            ctx.dispatch_global_action("workspace:save_app", ());
            ctx.notify();
        }
    }

    /// The index of the open tab that owns `pane_group_id`.
    pub(super) fn tab_index_of(&self, pane_group_id: EntityId) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.pane_group.id() == pane_group_id)
    }

    /// How many tabs lead the list as floating, which the close menus promise
    /// to spare. Zero when tags are off.
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
