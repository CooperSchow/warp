//! Tab-level unread: which of a tab's terminal panes are unread, whether a
//! vertical tabs row shows the unread dot, and the Mark as Unread / Mark as
//! Read actions. All of it reads `AgentNotificationsModel::is_unread`, so the
//! row's dot, the menu's label and ⌘J never disagree.

use std::time::Duration;

use warpui::r#async::Timer;
use warpui::{AppContext, EntityId, SingletonEntity, ViewContext, ViewHandle};

use super::Workspace;
use crate::ai::agent_management::{active_window_id, AgentNotificationsModel, DwellId};
use crate::pane_group::{PaneGroup, PaneId};
use crate::terminal::TerminalView;
use crate::workspace::tab_settings::VerticalTabsDisplayGranularity;
use crate::workspace::WorkspaceRegistry;

/// How long a mark staged after restore's commit waits for the window's first
/// input before it's committed anyway.
const STAGED_UNREAD_FALLBACK_COMMIT: Duration = Duration::from_secs(2);

/// How long focus has to stay on a pane it arrived at before the arrival reads
/// the pane: its manual mark clears and its notifications are read. A held key
/// repeats many times a second, and stepping through tabs by hand takes a
/// fraction of a second a tab, so a second passes over both without reading
/// anything, while stopping to look reads a pane about as soon as it's seen.
pub(crate) const ARRIVAL_DWELL: Duration = Duration::from_secs(1);

/// The terminal view a vertical tabs row shows for `pane_id`: the pane's own,
/// or, for a temporary replacement (an expanded code diff, say), that of the
/// pane it stands in for.
pub(crate) fn row_terminal_view(
    pane_group: &PaneGroup,
    pane_id: PaneId,
    app: &AppContext,
) -> Option<ViewHandle<TerminalView>> {
    let pane_id = pane_group
        .original_pane_for_replacement(pane_id)
        .unwrap_or(pane_id);
    pane_group.terminal_view_from_pane_id(pane_id, app)
}

/// The terminal views of the tab's rows, in pane order: one for each visible
/// pane that is, or stands in for, a terminal. A pane hidden while its close
/// can be undone has no row, and neither does an off-tree child agent's.
fn row_terminal_views(pane_group: &PaneGroup, app: &AppContext) -> Vec<EntityId> {
    pane_group
        .visible_pane_ids()
        .into_iter()
        .filter_map(|pane_id| row_terminal_view(pane_group, pane_id, app))
        .map(|terminal_view| terminal_view.id())
        .collect()
}

/// The tab's unread terminal views, among those with a row.
pub(crate) fn tab_unread_terminal_views(pane_group: &PaneGroup, app: &AppContext) -> Vec<EntityId> {
    let notifications = AgentNotificationsModel::as_ref(app);
    row_terminal_views(pane_group, app)
        .into_iter()
        .filter(|terminal_view_id| notifications.is_unread(*terminal_view_id))
        .collect()
}

/// Whether any of the tab's terminal panes with a row is unread: the dot on a
/// row that stands for the whole tab, and what ⌘J looks for.
pub(crate) fn tab_is_unread(pane_group: &PaneGroup, app: &AppContext) -> bool {
    let notifications = AgentNotificationsModel::as_ref(app);
    row_terminal_views(pane_group, app)
        .into_iter()
        .any(|terminal_view_id| notifications.is_unread(terminal_view_id))
}

/// Whether the vertical tabs row drawn for `pane_id` shows the unread dot. A
/// Panes-granularity row is one pane and shows that pane's dot. A
/// Tabs-granularity row stands for the whole tab, so it shows the dot while any
/// of the tab's terminal panes is unread, focused or not.
pub(crate) fn row_shows_unread(
    pane_group: &PaneGroup,
    pane_id: PaneId,
    display_granularity: VerticalTabsDisplayGranularity,
    app: &AppContext,
) -> bool {
    match display_granularity {
        VerticalTabsDisplayGranularity::Panes => row_terminal_view(pane_group, pane_id, app)
            .is_some_and(|terminal_view| {
                AgentNotificationsModel::as_ref(app).is_unread(terminal_view.id())
            }),
        VerticalTabsDisplayGranularity::Tabs => tab_is_unread(pane_group, app),
    }
}

/// The terminal view Mark as Unread marks for a whole tab: the focused pane's
/// when that pane shows a terminal, or else the first terminal pane's with a
/// row. `None` when the tab has no terminal pane to mark.
pub(crate) fn tab_mark_target(pane_group: &PaneGroup, app: &AppContext) -> Option<EntityId> {
    row_terminal_view(pane_group, pane_group.focused_pane_id(app), app)
        .map(|terminal_view| terminal_view.id())
        .or_else(|| row_terminal_views(pane_group, app).into_iter().next())
}

/// Whether ⌃⌘U, pressed with this tab active, does exactly what the tab
/// menu's item for the pane showing `terminal_view_id` does. With no unread
/// pane the key marks the tab's mark target, which is Mark as Unread on that
/// pane. With one, it clears every pane, which is Mark as Read on this pane
/// only when this pane is the tab's one unread pane.
pub(crate) fn toggle_key_acts_on_pane(
    pane_group: &PaneGroup,
    terminal_view_id: EntityId,
    app: &AppContext,
) -> bool {
    if tab_is_unread(pane_group, app) {
        let notifications = AgentNotificationsModel::as_ref(app);
        all_terminal_views(pane_group, app)
            .into_iter()
            .all(|view| notifications.is_unread(view) == (view == terminal_view_id))
    } else {
        tab_mark_target(pane_group, app) == Some(terminal_view_id)
    }
}

/// Every terminal view the tab holds, hidden ones included: what Mark as Read
/// clears for a whole tab, and where restore looks for staged marks.
fn all_terminal_views(pane_group: &PaneGroup, app: &AppContext) -> Vec<EntityId> {
    pane_group
        .terminal_pane_ids()
        .filter_map(|pane_id| pane_group.terminal_view_from_pane_id(pane_id, app))
        .map(|terminal_view| terminal_view.id())
        .collect()
}

/// Whether the active window's active tab has an unread terminal pane, for the
/// palette's "Mark current tab as unread" / "Mark current tab as read" entry.
pub(crate) fn active_tab_is_unread(ctx: &AppContext) -> bool {
    ctx.windows()
        .active_window()
        .and_then(|window_id| WorkspaceRegistry::as_ref(ctx).get(window_id, ctx))
        .is_some_and(|workspace| {
            tab_is_unread(
                workspace.as_ref(ctx).active_tab_pane_group().as_ref(ctx),
                ctx,
            )
        })
}

impl Workspace {
    /// Marks the tab that owns `pane_group_id` unread or read. A
    /// `terminal_view_id` targets that one pane. `None` targets the whole tab:
    /// marking it unread marks its focused terminal pane, or failing that its
    /// first, and marking it read clears every pane, notifications included.
    /// A no-op when that tab, or that pane, has since closed.
    pub(super) fn set_tab_unread(
        &mut self,
        pane_group_id: EntityId,
        terminal_view_id: Option<EntityId>,
        unread: bool,
        ctx: &mut ViewContext<Self>,
    ) {
        let Some(tab) = self
            .tabs
            .iter()
            .find(|tab| tab.pane_group.id() == pane_group_id)
        else {
            return;
        };
        let pane_group = tab.pane_group.as_ref(ctx);
        let targets: Vec<EntityId> = match terminal_view_id {
            Some(terminal_view_id) => all_terminal_views(pane_group, ctx)
                .into_iter()
                .filter(|candidate| *candidate == terminal_view_id)
                .collect(),
            None => {
                if unread {
                    tab_mark_target(pane_group, ctx).into_iter().collect()
                } else {
                    all_terminal_views(pane_group, ctx)
                }
            }
        };
        if targets.is_empty() {
            return;
        }
        AgentNotificationsModel::handle(ctx).update(ctx, |model, ctx| {
            if unread {
                model.mark_unread(&targets, ctx);
            } else {
                model.mark_read(&targets, ctx);
            }
        });
    }

    /// Marks the active tab unread, or read if it has an unread pane.
    pub(super) fn toggle_active_tab_unread(&mut self, ctx: &mut ViewContext<Self>) {
        let pane_group = self.active_tab_pane_group();
        let pane_group_id = pane_group.id();
        let unread = !tab_is_unread(pane_group.as_ref(ctx), ctx);
        self.set_tab_unread(pane_group_id, None, unread, ctx);
    }

    /// Commits the unread marks restored with this window's panes, once
    /// restore has activated its tab. The window's focus baseline comes from
    /// `active_tab_focused_terminal_view_id`, the resolver every later focus
    /// report goes through, so the first of those can't pass for an arrival.
    /// A mark staged any later is committed on the window's first input or
    /// after `STAGED_UNREAD_FALLBACK_COMMIT`, whichever comes first.
    pub(super) fn commit_restored_unread_marks(&mut self, ctx: &mut ViewContext<Self>) {
        if !ctx.has_singleton_model::<AgentNotificationsModel>() {
            return;
        }
        self.commit_staged_unread_marks(ctx);
        ctx.spawn(
            async {
                Timer::after(STAGED_UNREAD_FALLBACK_COMMIT).await;
            },
            |me, _, ctx| me.commit_straggling_unread_marks(ctx),
        );
    }

    /// Ends the dwell an arrival in this window began, once `ARRIVAL_DWELL`
    /// has passed, with what this window then has focused if it's still the
    /// active window. The timer belongs to this workspace, like the fallback
    /// commit's, so it can't fire once the window is gone.
    pub(super) fn wait_out_arrival_dwell(&self, dwell: DwellId, ctx: &mut ViewContext<Self>) {
        let window_id = ctx.window_id();
        ctx.spawn(
            async {
                Timer::after(ARRIVAL_DWELL).await;
            },
            move |me, _, ctx| {
                // Read from here rather than through the registry, which can't
                // reach this workspace while it's the view being updated.
                let looking_at = (active_window_id(ctx) == Some(window_id))
                    .then(|| me.active_tab_focused_terminal_view_id(ctx))
                    .flatten();
                AgentNotificationsModel::handle(ctx).update(ctx, |model, ctx| {
                    model.finish_dwell(window_id, dwell, looking_at, ctx);
                });
            },
        );
    }

    /// Commits any mark still staged among this window's panes. Runs on every
    /// input in the window, so while nothing is staged it costs one check.
    pub(super) fn commit_straggling_unread_marks(&self, ctx: &mut ViewContext<Self>) {
        if ctx.has_singleton_model::<AgentNotificationsModel>()
            && AgentNotificationsModel::as_ref(ctx).has_staged_marks()
        {
            self.commit_staged_unread_marks(ctx);
        }
    }

    /// Commits the marks staged among this window's panes, with the window's
    /// current focus as its baseline.
    fn commit_staged_unread_marks(&self, ctx: &mut ViewContext<Self>) {
        let terminal_view_ids: Vec<EntityId> = self
            .tabs
            .iter()
            .flat_map(|tab| all_terminal_views(tab.pane_group.as_ref(ctx), ctx))
            .collect();
        let focused_terminal_view_id = self.active_tab_focused_terminal_view_id(ctx);
        let window_id = ctx.window_id();
        AgentNotificationsModel::handle(ctx).update(ctx, |model, _| {
            model.commit_restored_unread(window_id, &terminal_view_ids, focused_terminal_view_id);
        });
    }
}

#[cfg(test)]
#[path = "tab_unread_tests.rs"]
mod tests;
