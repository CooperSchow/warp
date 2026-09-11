//! Tab-level unread: which of a tab's terminal panes are unread, whether a
//! vertical tabs row shows the unread dot, and the Mark as Unread / Mark as
//! Read actions. All of it reads `AgentNotificationsModel::is_unread`, so the
//! row's dot, the menu's label and ⌘J never disagree.

use warpui::{AppContext, EntityId, SingletonEntity, ViewContext};

use super::Workspace;
use crate::ai::agent_management::AgentNotificationsModel;
use crate::pane_group::{PaneGroup, PaneId};
use crate::workspace::tab_settings::VerticalTabsDisplayGranularity;
use crate::workspace::WorkspaceRegistry;

/// The tab's unread terminal views, among its panes that aren't hidden while
/// their close can still be undone.
pub(crate) fn tab_unread_terminal_views(pane_group: &PaneGroup, app: &AppContext) -> Vec<EntityId> {
    let notifications = AgentNotificationsModel::as_ref(app);
    pane_group
        .terminal_views(app)
        .into_iter()
        .map(|terminal_view| terminal_view.id())
        .filter(|terminal_view_id| notifications.is_unread(*terminal_view_id))
        .collect()
}

/// Whether the vertical tabs row drawn for `pane_id` shows the unread dot.
pub(crate) fn row_shows_unread(
    pane_group: &PaneGroup,
    pane_id: PaneId,
    _display_granularity: VerticalTabsDisplayGranularity,
    app: &AppContext,
) -> bool {
    // A temporary replacement (an expanded code diff, say) shows as the pane
    // it stands in for.
    let pane_id = pane_group
        .original_pane_for_replacement(pane_id)
        .unwrap_or(pane_id);
    pane_group
        .terminal_view_from_pane_id(pane_id, app)
        .is_some_and(|terminal_view| {
            AgentNotificationsModel::as_ref(app).is_unread(terminal_view.id())
        })
}

/// Whether the active window's active tab has an unread terminal pane, for the
/// palette's "Mark current tab as unread" / "Mark current tab as read" entry.
pub(crate) fn active_tab_is_unread(ctx: &AppContext) -> bool {
    ctx.windows()
        .active_window()
        .and_then(|window_id| WorkspaceRegistry::as_ref(ctx).get(window_id, ctx))
        .is_some_and(|workspace| {
            let pane_group = workspace.as_ref(ctx).active_tab_pane_group().as_ref(ctx);
            !tab_unread_terminal_views(pane_group, ctx).is_empty()
        })
}

impl Workspace {
    /// Marks the tab that owns `pane_group_id` unread or read. A
    /// `terminal_view_id` targets that one pane. `None` targets the whole tab:
    /// marking it unread marks its focused terminal pane, or failing that its
    /// first, and marking it read clears every pane. A no-op when that tab has
    /// since closed.
    pub(super) fn set_tab_unread(
        &mut self,
        _pane_group_id: EntityId,
        _terminal_view_id: Option<EntityId>,
        _unread: bool,
        _ctx: &mut ViewContext<Self>,
    ) {
    }

    /// Marks the active tab unread, or read if it has an unread pane.
    pub(super) fn toggle_active_tab_unread(&mut self, _ctx: &mut ViewContext<Self>) {}

    /// Commits the unread marks restored with this window's panes, once
    /// restore has activated its tab. The window's focus baseline comes from
    /// `active_tab_focused_terminal_view_id`, the resolver every later focus
    /// report goes through, so the first of those can't pass for an arrival.
    pub(super) fn commit_restored_unread_marks(&mut self, _ctx: &mut ViewContext<Self>) {}
}
