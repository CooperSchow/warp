//! ⌘J: jump to the topmost unread tab other than the active one. Starred tabs
//! lead the list, so they come first, with no priorities to learn.

use warpui::{AppContext, SingletonEntity, ViewContext};

use super::tab_unread::{row_terminal_view, tab_is_unread};
use super::Workspace;
use crate::ai::agent_management::AgentNotificationsModel;
use crate::pane_group::{PaneGroup, PaneId};
use crate::tab::uses_vertical_tabs;
use crate::view_components::DismissibleToast;

/// The topmost tab in `visible` (tab indices, in list order) that is unread
/// and isn't `active`.
pub(super) fn next_unread_tab(
    visible: &[usize],
    active: usize,
    is_unread: impl Fn(usize) -> bool,
) -> Option<usize> {
    visible
        .iter()
        .copied()
        .filter(|&index| index != active)
        .find(|&index| is_unread(index))
}

/// What ⌘J's toast says when there's no tab to jump to.
pub(super) fn nowhere_to_jump_message(
    active_is_unread: bool,
    search_hides_unread_tabs: bool,
) -> &'static str {
    if search_hides_unread_tabs {
        "No unread tabs match this search"
    } else if active_is_unread {
        "No other unread tabs"
    } else {
        "You're all caught up"
    }
}

/// The pane ⌘J focuses in the tab it jumps to, when activating the tab
/// doesn't already land on an unread one: the first unread pane with a row.
fn unread_pane_to_focus(pane_group: &PaneGroup, app: &AppContext) -> Option<PaneId> {
    let notifications = AgentNotificationsModel::as_ref(app);
    let pane_is_unread = |pane_id: PaneId| {
        row_terminal_view(pane_group, pane_id, app)
            .is_some_and(|terminal_view| notifications.is_unread(terminal_view.id()))
    };
    if pane_is_unread(pane_group.focused_pane_id(app)) {
        return None;
    }
    pane_group
        .visible_pane_ids()
        .into_iter()
        .find(|pane_id| pane_is_unread(*pane_id))
}

impl Workspace {
    /// Activates the topmost unread tab other than the active one, among the
    /// tabs the vertical tabs search shows, and focuses its unread pane. A
    /// toast says so when there's none. It reads only what's already in
    /// memory: no file, and no terminal model lock.
    pub(super) fn jump_to_next_unread_tab(&mut self, ctx: &mut ViewContext<Self>) {
        match self.next_unread_target(ctx) {
            Ok(index) => {
                let pane_group = self.tabs[index].pane_group.clone();
                let unread_pane = unread_pane_to_focus(pane_group.as_ref(ctx), ctx);
                self.activate_tab(index, ctx);
                if let Some(pane_id) = unread_pane {
                    pane_group.update(ctx, |pane_group, ctx| {
                        pane_group.focus_pane_by_id(pane_id, ctx);
                    });
                }
            }
            Err(message) => {
                self.toast_stack.update(ctx, |toast_stack, ctx| {
                    toast_stack
                        .add_ephemeral_toast(DismissibleToast::default(message.to_owned()), ctx);
                });
            }
        }
    }

    /// Where ⌘J goes, or what its toast says when there's nowhere to go.
    fn next_unread_target(&self, ctx: &AppContext) -> Result<usize, &'static str> {
        let unread: Vec<bool> = self
            .tabs
            .iter()
            .map(|tab| tab_is_unread(tab.pane_group.as_ref(ctx), ctx))
            .collect();
        let shown_by_search = self.tabs_shown_by_search(ctx);
        let visible = shown_by_search
            .clone()
            .unwrap_or_else(|| (0..self.tabs.len()).collect());
        if let Some(index) = next_unread_tab(&visible, self.active_tab_index, |index| unread[index])
        {
            return Ok(index);
        }
        let other_tab_is_unread = unread
            .iter()
            .enumerate()
            .any(|(index, unread)| *unread && index != self.active_tab_index);
        let active_is_unread = unread.get(self.active_tab_index).copied().unwrap_or(false);
        Err(nowhere_to_jump_message(
            active_is_unread,
            shown_by_search.is_some() && other_tab_is_unread,
        ))
    }

    /// The tabs the vertical tabs search shows, in list order, while the panel
    /// is showing and a search filters it; `None` otherwise. They come from the
    /// panel's last render rather than a fresh match, since matching reads
    /// terminal state under its lock.
    fn tabs_shown_by_search(&self, app: &AppContext) -> Option<Vec<usize>> {
        if !self.vertical_tabs_panel_open || !uses_vertical_tabs(app) {
            return None;
        }
        let matching = self.vertical_tabs_panel.tabs_matching_search()?;
        Some(
            self.tabs
                .iter()
                .enumerate()
                .filter(|(_, tab)| matching.contains(&tab.pane_group.id()))
                .map(|(index, _)| index)
                .collect(),
        )
    }
}

#[cfg(test)]
#[path = "next_unread_tests.rs"]
mod tests;
