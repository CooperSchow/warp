use std::collections::{HashMap, HashSet};

use warp_core::features::FeatureFlag;
use warpui::{AppContext, Entity, EntityId, ModelContext, SingletonEntity, ViewHandle, WindowId};

use crate::ai::active_agent_views_model::{ActiveAgentViewsEvent, ActiveAgentViewsModel};
use crate::ai::agent::conversation::{AIConversationId, ConversationStatus};
use crate::ai::agent_management::notifications::{
    NotificationCategory, NotificationId, NotificationItem, NotificationItems, NotificationOrigin,
    NotificationSourceAgent,
};
use crate::ai::artifacts::Artifact;
use crate::ai::blocklist::{BlocklistAIHistoryEvent, ConversationStatusUpdate, QueuedQueryModel};
use crate::terminal::cli_agent_sessions::{
    CLIAgentSessionStatus, CLIAgentSessionsModel, CLIAgentSessionsModelEvent,
};
use crate::terminal::{CLIAgent, TerminalView};
use crate::workspace::util::is_terminal_view_in_same_tab;
use crate::workspace::{Workspace, WorkspaceRegistry};
use crate::BlocklistAIHistoryModel;

/// Singleton model responsible for triggering in-app notifications on blocking conversation
/// status updates and tracking/storing these notifications for the notifications mailbox.
/// Tracks and stores notifications for both warp agent conversations and other supported
/// cli agent sessions.
pub struct AgentNotificationsModel {
    notifications: NotificationItems,
    /// Artifacts accumulated during the current turn for each conversation.
    /// Drained into the notification when a terminal state fires, cleared on InProgress.
    pub(crate) pending_artifacts: HashMap<AIConversationId, Vec<Artifact>>,
    /// Terminal views marked unread by hand. A mark holds until focus arrives
    /// at its view and stays there for the dwell, or the view is replied to,
    /// marked read, or closed for good.
    manually_unread: HashSet<EntityId>,
    /// Marks restored with their panes: shown as unread, but out of reach of
    /// focus reports until restore commits them.
    staged_unread: HashSet<EntityId>,
    /// The terminal last seen focused in each window's active tab while the
    /// window was active, `None` while a non-terminal pane had focus. A window
    /// has no entry until its first report while active, which is a baseline
    /// rather than an arrival.
    last_focus_by_window: HashMap<WindowId, Option<EntityId>>,
    /// The arrival in each window still waiting out its dwell.
    dwells: HashMap<WindowId, Dwell>,
    /// The id the next dwell gets.
    next_dwell_id: u64,
}

/// Names one arrival's dwell, so a timer that outlives the dwell finds
/// nothing to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DwellId(u64);

/// An arrival waiting to see whether focus stays on the view it reached.
struct Dwell {
    id: DwellId,
    terminal_view_id: EntityId,
    /// Whether the view was marked after the arrival began. That mark is newer
    /// than the arrival, so the dwell leaves it.
    marked_since_arrival: bool,
}

impl Entity for AgentNotificationsModel {
    type Event = AgentManagementEvent;
}

impl SingletonEntity for AgentNotificationsModel {}

impl AgentNotificationsModel {
    pub(crate) fn new(ctx: &mut ModelContext<Self>) -> Self {
        let history_model = BlocklistAIHistoryModel::handle(ctx);
        ctx.subscribe_to_model(&history_model, move |me, _, event, ctx| {
            me.handle_history_event(event, ctx);
        });

        let cli_sessions_model = CLIAgentSessionsModel::handle(ctx);
        ctx.subscribe_to_model(&cli_sessions_model, |me, _, event, ctx| {
            me.handle_cli_agent_session_event(event, ctx);
        });

        let active_views_model = ActiveAgentViewsModel::handle(ctx);
        ctx.subscribe_to_model(&active_views_model, |me, _, event, ctx| {
            me.handle_active_agent_views_changed(event, ctx);
        });

        Self {
            notifications: NotificationItems::default(),
            pending_artifacts: HashMap::new(),
            manually_unread: HashSet::new(),
            staged_unread: HashSet::new(),
            last_focus_by_window: HashMap::new(),
            dwells: HashMap::new(),
            next_dwell_id: 0,
        }
    }

    pub(crate) fn notifications(&self) -> &NotificationItems {
        &self.notifications
    }

    pub(crate) fn mark_item_read(&mut self, id: NotificationId, ctx: &mut ModelContext<Self>) {
        if self.notifications.mark_item_read(id) {
            ctx.emit(AgentManagementEvent::NotificationUpdated);
        }
    }

    pub(crate) fn mark_all_items_read(&mut self, ctx: &mut ModelContext<Self>) {
        if self.notifications.mark_all_items_read() {
            ctx.emit(AgentManagementEvent::AllNotificationsMarkedRead);
        }
    }

    /// Marks all notifications from the given terminal view as read.
    pub(crate) fn mark_items_from_terminal_view_read(
        &mut self,
        terminal_view_id: EntityId,
        ctx: &mut ModelContext<Self>,
    ) {
        if !FeatureFlag::HOANotifications.is_enabled() {
            return;
        }
        if self
            .notifications
            .mark_all_terminal_view_items_as_read(terminal_view_id)
        {
            ctx.emit(AgentManagementEvent::NotificationUpdated);
        }
    }

    /// Whether the terminal view shows as unread: marked unread by hand,
    /// restored with a mark, or holding an agent notification nobody has
    /// looked at. The one predicate behind a tab row's dot, the tab menu's
    /// Mark as Read label and ⌘J.
    pub(crate) fn is_unread(&self, terminal_view_id: EntityId) -> bool {
        self.manually_unread.contains(&terminal_view_id)
            || self.staged_unread.contains(&terminal_view_id)
            || self
                .notifications
                .has_unread_for_terminal_view(terminal_view_id)
    }

    /// Whether a snapshot records the terminal view as unread. It records the
    /// dot as shown, whatever lit it, so a finished session nobody opened
    /// still shows its dot after a relaunch. False while `TabMarkUnread` is
    /// off, and in a harness that never registered this model.
    pub(crate) fn unread_for_snapshot(terminal_view_id: EntityId, app: &AppContext) -> bool {
        FeatureFlag::TabMarkUnread.is_enabled()
            && app.has_singleton_model::<Self>()
            && Self::as_ref(app).is_unread(terminal_view_id)
    }

    /// Whether any restored mark is still waiting to be committed.
    pub(crate) fn has_staged_marks(&self) -> bool {
        !self.staged_unread.is_empty()
    }

    /// Marks the terminal views unread by hand. A mark holds until focus
    /// arrives at its view and stays there for the dwell, or the view is
    /// replied to, marked read, or closed for good. A mark set while focus is
    /// dwelling on its view outlasts that dwell.
    pub(crate) fn mark_unread(
        &mut self,
        terminal_view_ids: &[EntityId],
        ctx: &mut ModelContext<Self>,
    ) {
        let mut changed = false;
        for terminal_view_id in terminal_view_ids {
            changed |= self.manually_unread.insert(*terminal_view_id);
        }
        self.note_marks_since_arrival(terminal_view_ids);
        if changed {
            ctx.emit(AgentManagementEvent::NotificationUpdated);
        }
    }

    /// Clears the terminal views' manual and restored marks, and marks their
    /// notifications read.
    pub(crate) fn mark_read(
        &mut self,
        terminal_view_ids: &[EntityId],
        ctx: &mut ModelContext<Self>,
    ) {
        let mut changed = false;
        for terminal_view_id in terminal_view_ids {
            changed |= self.manually_unread.remove(terminal_view_id);
            changed |= self.staged_unread.remove(terminal_view_id);
        }
        if changed {
            ctx.emit(AgentManagementEvent::NotificationUpdated);
        }
        for terminal_view_id in terminal_view_ids {
            self.mark_items_from_terminal_view_read(*terminal_view_id, ctx);
        }
    }

    /// Records the terminal focused in `window_id`'s active tab (`None` when a
    /// non-terminal pane has focus). Returns the dwell the report begins, for
    /// the caller to end with `finish_dwell` once `ARRIVAL_DWELL` has passed.
    ///
    /// A report begins a dwell when it's an arrival: its view replaces a
    /// different, already-known focus while the window is active. That's
    /// measured against the focus last seen while the window was active, so a
    /// switch made while the window was in the background, like a notification
    /// click, counts once the window comes to the front, while the first
    /// report in a window, a repeat of the same view and a window coming back
    /// to the view it had never do. Any report that names another view ends
    /// the window's dwell early. Dwells need `TabMarkUnread`.
    pub(crate) fn record_terminal_focus(
        &mut self,
        window_id: WindowId,
        focused_terminal_view_id: Option<EntityId>,
        is_active_window: bool,
        ctx: &mut ModelContext<Self>,
    ) -> Option<DwellId> {
        self.forget_closed_windows(window_id, ctx);
        if self
            .dwells
            .get(&window_id)
            .is_some_and(|dwell| Some(dwell.terminal_view_id) != focused_terminal_view_id)
        {
            self.dwells.remove(&window_id);
        }
        if !is_active_window {
            return None;
        }
        let previous = self
            .last_focus_by_window
            .insert(window_id, focused_terminal_view_id);
        let focused = focused_terminal_view_id?;
        let arrived = previous.is_some_and(|previous| previous != Some(focused));
        if !arrived || !FeatureFlag::TabMarkUnread.is_enabled() {
            return None;
        }
        let id = DwellId(self.next_dwell_id);
        self.next_dwell_id += 1;
        self.dwells.insert(
            window_id,
            Dwell {
                id,
                terminal_view_id: focused,
                marked_since_arrival: false,
            },
        );
        Some(id)
    }

    /// Whether an arrival in `window_id` is waiting out its dwell. While one
    /// is, focus reports leave the arrived-at view's notifications for the
    /// dwell to read.
    pub(crate) fn has_pending_dwell(&self, window_id: WindowId) -> bool {
        self.dwells.contains_key(&window_id)
    }

    /// Ends the dwell `dwell_id` once `ARRIVAL_DWELL` has passed, given
    /// `looking_at`, the terminal in the focused pane of the active tab in the
    /// active window. If that's still the view the arrival reached, the view's
    /// manual mark clears, unless it was set after the arrival began, and its
    /// notifications are read. A dwell that ended early, or that a later
    /// arrival replaced, does nothing.
    pub(crate) fn finish_dwell(
        &mut self,
        window_id: WindowId,
        dwell_id: DwellId,
        looking_at: Option<EntityId>,
        ctx: &mut ModelContext<Self>,
    ) {
        if !self
            .dwells
            .get(&window_id)
            .is_some_and(|dwell| dwell.id == dwell_id)
        {
            return;
        }
        let Some(dwell) = self.dwells.remove(&window_id) else {
            return;
        };
        if looking_at != Some(dwell.terminal_view_id) {
            return;
        }
        if !dwell.marked_since_arrival && self.manually_unread.remove(&dwell.terminal_view_id) {
            ctx.emit(AgentManagementEvent::NotificationUpdated);
        }
        self.mark_items_from_terminal_view_read(dwell.terminal_view_id, ctx);
    }

    /// Notes marks just set on views that focus is dwelling on: newer than
    /// the arrival, they're left by its dwell.
    fn note_marks_since_arrival(&mut self, terminal_view_ids: &[EntityId]) {
        for dwell in self.dwells.values_mut() {
            if terminal_view_ids.contains(&dwell.terminal_view_id) {
                dwell.marked_since_arrival = true;
            }
        }
    }

    /// Drops the focus and the dwell recorded for windows that have closed,
    /// which `WindowClosed` doesn't name. A window's workspace is registered
    /// from its creation until the window closes. The reporting window always
    /// stays, since it may be reporting from inside its creation, before it's
    /// registered.
    fn forget_closed_windows(&mut self, reporting_window_id: WindowId, app: &AppContext) {
        if !app.has_singleton_model::<WorkspaceRegistry>() {
            return;
        }
        let registry = WorkspaceRegistry::as_ref(app);
        let is_open = |window_id: &WindowId| {
            *window_id == reporting_window_id || registry.is_registered(*window_id)
        };
        self.last_focus_by_window
            .retain(|window_id, _| is_open(window_id));
        self.dwells.retain(|window_id, _| is_open(window_id));
    }

    /// Drops what's held for a terminal view that closed for good, as opposed
    /// to one hidden while its close can be undone, or moved.
    pub(crate) fn forget_terminal_view(
        &mut self,
        terminal_view_id: EntityId,
        ctx: &mut ModelContext<Self>,
    ) {
        let was_marked = self.manually_unread.remove(&terminal_view_id);
        let was_staged = self.staged_unread.remove(&terminal_view_id);
        self.dwells
            .retain(|_, dwell| dwell.terminal_view_id != terminal_view_id);
        if was_marked || was_staged {
            ctx.emit(AgentManagementEvent::NotificationUpdated);
        }
    }

    /// Stages a mark restored with its pane: shown as unread, but no focus
    /// report clears it until `commit_restored_unread` runs.
    pub(crate) fn stage_restored_unread(
        &mut self,
        terminal_view_id: EntityId,
        ctx: &mut ModelContext<Self>,
    ) {
        if self.staged_unread.insert(terminal_view_id) {
            ctx.emit(AgentManagementEvent::NotificationUpdated);
        }
    }

    /// Commits the staged marks among `terminal_view_ids` once restore has
    /// activated `window_id`'s tab, taking `focused_terminal_view_id` as that
    /// window's focus baseline. A committed mark shows just as it did staged,
    /// and from here on it clears like any other, as a mark set now would.
    pub(crate) fn commit_restored_unread(
        &mut self,
        window_id: WindowId,
        terminal_view_ids: &[EntityId],
        focused_terminal_view_id: Option<EntityId>,
    ) {
        let mut committed = Vec::new();
        for terminal_view_id in terminal_view_ids {
            if self.staged_unread.remove(terminal_view_id) {
                self.manually_unread.insert(*terminal_view_id);
                committed.push(*terminal_view_id);
            }
        }
        self.note_marks_since_arrival(&committed);
        self.last_focus_by_window
            .insert(window_id, focused_terminal_view_id);
    }

    /// A reply in `terminal_view_id` (a prompt submitted to Claude Code, or an
    /// Oz conversation resuming) clears its manual mark, but only while the
    /// user is looking at it: its pane is the focused pane of the active tab
    /// in the active window. A prompt that starts on its own in a background
    /// tab, like a /loop or a queued or scheduled prompt, leaves the mark.
    fn clear_mark_on_reply(&mut self, terminal_view_id: EntityId, ctx: &mut ModelContext<Self>) {
        if !FeatureFlag::TabMarkUnread.is_enabled() {
            return;
        }
        let focused_in_active_window = focused_terminal_in_active_window(ctx);
        self.clear_mark_on_reply_while_focused(terminal_view_id, focused_in_active_window, ctx);
    }

    /// `clear_mark_on_reply`, given the terminal focused in the active window.
    fn clear_mark_on_reply_while_focused(
        &mut self,
        terminal_view_id: EntityId,
        focused_in_active_window: Option<EntityId>,
        ctx: &mut ModelContext<Self>,
    ) {
        if focused_in_active_window == Some(terminal_view_id)
            && self.manually_unread.remove(&terminal_view_id)
        {
            ctx.emit(AgentManagementEvent::NotificationUpdated);
        }
    }

    fn handle_active_agent_views_changed(
        &mut self,
        event: &ActiveAgentViewsEvent,
        ctx: &mut ModelContext<Self>,
    ) {
        if !FeatureFlag::HOANotifications.is_enabled() {
            return;
        }

        match event {
            ActiveAgentViewsEvent::ConversationClosed { conversation_id } => {
                // When a conversation is closed, clean up its notifications
                // (as there's no conversation to navigate to when you click said notifications).
                if self
                    .notifications
                    .remove_by_origin(NotificationOrigin::Conversation(*conversation_id))
                {
                    ctx.emit(AgentManagementEvent::NotificationUpdated);
                }
            }
            ActiveAgentViewsEvent::TerminalViewFocused
            | ActiveAgentViewsEvent::WindowClosed
            | ActiveAgentViewsEvent::AmbientSessionOpened { .. }
            | ActiveAgentViewsEvent::AmbientSessionClosed { .. } => {}
        }
    }

    fn handle_cli_agent_session_event(
        &mut self,
        event: &CLIAgentSessionsModelEvent,
        ctx: &mut ModelContext<Self>,
    ) {
        // A reply clears a manual mark whether or not the mailbox is on.
        if let CLIAgentSessionsModelEvent::StatusChanged {
            terminal_view_id,
            status: CLIAgentSessionStatus::InProgress,
            ..
        } = event
        {
            self.clear_mark_on_reply(*terminal_view_id, ctx);
        }

        if !FeatureFlag::HOANotifications.is_enabled() {
            return;
        }

        match event {
            CLIAgentSessionsModelEvent::Ended {
                terminal_view_id, ..
            } => {
                self.remove_notification_by_source(
                    NotificationOrigin::CLISession(*terminal_view_id),
                    ctx,
                );
            }
            CLIAgentSessionsModelEvent::Started { .. }
            | CLIAgentSessionsModelEvent::InputSessionChanged { .. }
            | CLIAgentSessionsModelEvent::SessionUpdated { .. } => {}
            CLIAgentSessionsModelEvent::StatusChanged {
                terminal_view_id,
                agent,
                status,
                session_context,
            } => match status {
                // When the agent resumes its work we can assume that the previous notification is stale.
                CLIAgentSessionStatus::InProgress => {
                    self.remove_notification_by_source(
                        NotificationOrigin::CLISession(*terminal_view_id),
                        ctx,
                    );
                }
                CLIAgentSessionStatus::Success => {
                    let title = session_context
                        .display_title()
                        .unwrap_or_else(|| format!("{} completed", agent.display_name()));
                    let message = match agent {
                        CLIAgent::Codex => "Notification from Codex",
                        _ => "Task completed.",
                    };
                    let metadata = TerminalViewMetadata::lookup(*terminal_view_id, ctx);
                    self.add_notification(
                        title,
                        message.to_owned(),
                        NotificationCategory::Complete,
                        NotificationSourceAgent::CLI {
                            agent: *agent,
                            is_ambient: metadata.is_ambient,
                        },
                        NotificationOrigin::CLISession(*terminal_view_id),
                        *terminal_view_id,
                        vec![],
                        metadata.branch,
                        ctx,
                    );
                }
                CLIAgentSessionStatus::Blocked { message } => {
                    let title = session_context
                        .display_title()
                        .unwrap_or_else(|| format!("{} needs attention", agent.display_name()));
                    let metadata = TerminalViewMetadata::lookup(*terminal_view_id, ctx);
                    self.add_notification(
                        title,
                        message
                            .clone()
                            .unwrap_or_else(|| "Waiting for input.".to_owned()),
                        NotificationCategory::Request,
                        NotificationSourceAgent::CLI {
                            agent: *agent,
                            is_ambient: metadata.is_ambient,
                        },
                        NotificationOrigin::CLISession(*terminal_view_id),
                        *terminal_view_id,
                        vec![],
                        metadata.branch,
                        ctx,
                    );
                }
            },
        }
    }

    fn handle_history_event(
        &mut self,
        event: &BlocklistAIHistoryEvent,
        ctx: &mut ModelContext<Self>,
    ) {
        // When a conversation is deleted or removed, clean up its notification and pending artifacts.
        if let BlocklistAIHistoryEvent::DeletedConversation {
            conversation_id, ..
        }
        | BlocklistAIHistoryEvent::RemoveConversation {
            conversation_id, ..
        } = event
        {
            if FeatureFlag::HOANotifications.is_enabled() {
                self.pending_artifacts.remove(conversation_id);
                self.remove_notification_by_source(
                    NotificationOrigin::Conversation(*conversation_id),
                    ctx,
                );
            }
            return;
        }

        // Accumulate artifacts as they arrive during the conversation.
        if let BlocklistAIHistoryEvent::UpdatedConversationArtifacts {
            conversation_id,
            artifact,
            ..
        } = event
        {
            if FeatureFlag::HOANotifications.is_enabled() {
                self.pending_artifacts
                    .entry(*conversation_id)
                    .or_default()
                    .push(artifact.clone());
            }
            return;
        }

        let BlocklistAIHistoryEvent::UpdatedConversationStatus {
            terminal_surface_id,
            conversation_id,
            // We shouldn't trigger toasts when restoring conversations on startup.
            update: ConversationStatusUpdate::Changed { .. },
            ..
        } = event
        else {
            return;
        };

        let ai_history_model = BlocklistAIHistoryModel::as_ref(ctx);
        let Some(updated_conversation) = ai_history_model.conversation(conversation_id) else {
            return;
        };

        if updated_conversation.should_exclude_from_navigation() {
            return;
        }

        let status = updated_conversation.status().clone();
        let latest_query = updated_conversation.latest_user_query();

        // A reply clears a manual mark whether or not the mailbox is on.
        match status {
            ConversationStatus::InProgress => {
                self.clear_mark_on_reply(*terminal_surface_id, ctx);
            }
            ConversationStatus::Success
            | ConversationStatus::Blocked { .. }
            | ConversationStatus::Error
            | ConversationStatus::Cancelled
            | ConversationStatus::TransientError
            | ConversationStatus::WaitingForEvents => {}
        }

        if FeatureFlag::HOANotifications.is_enabled() {
            self.handle_history_event_for_mailbox(
                &status,
                *conversation_id,
                latest_query,
                *terminal_surface_id,
                ctx,
            );
            // The new mailbox path handled the event — skip the legacy toast path below.
            return;
        }

        if !status.should_trigger_notification() {
            return;
        }

        if is_terminal_view_visible(*terminal_surface_id, ctx) {
            return;
        }

        let Some((window_id, tab_index)) =
            window_and_tab_idx_id_for_conversation(*conversation_id, ctx)
        else {
            return;
        };

        ctx.emit(AgentManagementEvent::ConversationNeedsAttention {
            window_id,
            tab_index,
            terminal_view_id: *terminal_surface_id,
            conversation_id: *conversation_id,
        });
    }

    fn handle_history_event_for_mailbox(
        &mut self,
        status: &ConversationStatus,
        conversation_id: AIConversationId,
        latest_query: Option<String>,
        terminal_view_id: EntityId,
        ctx: &mut ModelContext<Self>,
    ) {
        let origin = NotificationOrigin::Conversation(conversation_id);

        // If the conversation view is no longer open, don't create notifications for it
        // (there's nothing to navigate to when clicking them).
        if !ActiveAgentViewsModel::as_ref(ctx).is_conversation_open(conversation_id, ctx) {
            self.pending_artifacts.remove(&conversation_id);
            self.remove_notification_by_source(origin, ctx);
            return;
        }

        let title = latest_query.unwrap_or_else(|| "Agent task".to_owned());
        let metadata = TerminalViewMetadata::lookup(terminal_view_id, ctx);
        let oz_agent = NotificationSourceAgent::Oz {
            is_ambient: metadata.is_ambient,
        };

        match status {
            // When the agent resumes its work (or is automatically recovering from a
            // transient failure), clear stale notifications.
            ConversationStatus::InProgress | ConversationStatus::TransientError => {
                self.remove_notification_by_source(origin, ctx);
            }
            ConversationStatus::Success => {
                // Suppress the completion notification when a queued follow-up prompt will
                // auto-send as soon as this conversation finishes. The conversation isn't
                // really in a stopped state, so the notification would be noisy. Pending
                // artifacts are left intact so they roll into the notification fired when the
                // conversation eventually finishes with an empty queue.
                if QueuedQueryModel::as_ref(ctx).has_autofireable_prompt(conversation_id) {
                    return;
                }
                let artifacts = self.flush_pending_artifacts(conversation_id);
                self.add_notification(
                    title,
                    "Task completed.".to_owned(),
                    NotificationCategory::Complete,
                    oz_agent,
                    origin,
                    terminal_view_id,
                    artifacts,
                    metadata.branch,
                    ctx,
                );
            }
            ConversationStatus::Cancelled => {
                let artifacts = self.flush_pending_artifacts(conversation_id);
                self.add_notification(
                    title,
                    "Task was cancelled.".to_owned(),
                    NotificationCategory::Complete,
                    oz_agent,
                    origin,
                    terminal_view_id,
                    artifacts,
                    metadata.branch,
                    ctx,
                );
            }
            ConversationStatus::Blocked { blocked_action } => {
                self.add_notification(
                    title,
                    blocked_action.clone(),
                    NotificationCategory::Request,
                    oz_agent,
                    origin,
                    terminal_view_id,
                    vec![],
                    metadata.branch,
                    ctx,
                );
            }
            ConversationStatus::Error => {
                let artifacts = self.flush_pending_artifacts(conversation_id);
                self.add_notification(
                    title,
                    "Something went wrong.".to_owned(),
                    NotificationCategory::Error,
                    oz_agent,
                    origin,
                    terminal_view_id,
                    artifacts,
                    metadata.branch,
                    ctx,
                );
            }
            // Yielded conversations are still active; mirror the
            // InProgress arm and clear any stale notification for this
            // origin.
            ConversationStatus::WaitingForEvents => {
                self.remove_notification_by_source(origin, ctx);
            }
        }
    }

    /// Adds a finished-task notification from `terminal_view_id`, for tests
    /// outside this module.
    #[cfg(test)]
    pub(crate) fn add_notification_for_tests(
        &mut self,
        terminal_view_id: EntityId,
        ctx: &mut ModelContext<Self>,
    ) {
        self.add_notification(
            "Agent task".to_owned(),
            "Task completed.".to_owned(),
            NotificationCategory::Complete,
            NotificationSourceAgent::Oz { is_ambient: false },
            NotificationOrigin::Conversation(AIConversationId::new()),
            terminal_view_id,
            vec![],
            None,
            ctx,
        );
    }

    /// Removes the existing notification for the given source (if any) and emits an update event.
    fn remove_notification_by_source(
        &mut self,
        origin: NotificationOrigin,
        ctx: &mut ModelContext<Self>,
    ) {
        if self.notifications.remove_by_origin(origin) {
            ctx.emit(AgentManagementEvent::NotificationUpdated);
        }
    }

    /// Drains and returns the pending artifacts for a conversation.
    pub(crate) fn flush_pending_artifacts(
        &mut self,
        conversation_id: AIConversationId,
    ) -> Vec<Artifact> {
        self.pending_artifacts
            .remove(&conversation_id)
            .unwrap_or_default()
    }

    #[allow(clippy::too_many_arguments)]
    fn add_notification(
        &mut self,
        title: String,
        message: String,
        category: NotificationCategory,
        agent: NotificationSourceAgent,
        origin: NotificationOrigin,
        terminal_view_id: EntityId,
        artifacts: Vec<Artifact>,
        branch: Option<String>,
        ctx: &mut ModelContext<Self>,
    ) {
        let is_visible = is_terminal_view_visible(terminal_view_id, ctx);
        let item = NotificationItem::new(
            title,
            message,
            category,
            agent,
            origin,
            is_visible,
            terminal_view_id,
            artifacts,
            branch,
        );

        let id = item.id;
        self.notifications.push(item);
        ctx.emit(AgentManagementEvent::NotificationAdded { id });
    }
}

#[derive(Clone, Debug)]
pub enum AgentManagementEvent {
    /// A Warp-native conversation needs attention and is not visible in the current window/tab.
    ConversationNeedsAttention {
        window_id: WindowId,
        tab_index: usize,
        terminal_view_id: EntityId,
        conversation_id: AIConversationId,
    },
    /// A new notification was added to the persistent notification center.
    NotificationAdded { id: NotificationId },
    /// A notification's read state changed.
    NotificationUpdated,
    /// All notifications were marked as read.
    AllNotificationsMarkedRead,
}

impl ConversationStatus {
    /// Returns true if the updating the conversation with this status should trigger some
    /// notification to the user.
    ///
    /// Exhaustive match so a new `ConversationStatus` variant forces a
    /// deliberate decision about whether it should fire a notification.
    pub fn should_trigger_notification(&self) -> bool {
        match self {
            ConversationStatus::Success
            | ConversationStatus::Blocked { .. }
            | ConversationStatus::Error => true,
            // Streaming hasn't reached a notable state; a recovering or
            // yielded conversation is still active; user-cancellations are
            // self-evident.
            ConversationStatus::InProgress
            | ConversationStatus::TransientError
            | ConversationStatus::WaitingForEvents
            | ConversationStatus::Cancelled => false,
        }
    }
}

fn is_terminal_view_visible(terminal_view_id: EntityId, app: &AppContext) -> bool {
    let Some(active_id) = active_focused_terminal_id(app) else {
        return false;
    };
    active_id == terminal_view_id
        || is_terminal_view_in_same_tab(&active_id, &terminal_view_id, app)
}

fn window_and_tab_idx_id_for_conversation(
    conversation_id: AIConversationId,
    app: &AppContext,
) -> Option<(WindowId, usize)> {
    WorkspaceRegistry::as_ref(app)
        .all_workspaces(app)
        .iter()
        .find_map(|(window_id, workspace_handle)| {
            workspace_handle
                .as_ref(app)
                .tab_views()
                .enumerate()
                .find_map(|(tab_idx, pane_group)| {
                    pane_group
                        .as_ref(app)
                        .terminal_pane_ids()
                        .filter_map(|pane_id| {
                            pane_group
                                .as_ref(app)
                                .terminal_view_from_pane_id(pane_id, app)
                        })
                        .find_map(|terminal_view| {
                            let terminal_view_conversation_id =
                                terminal_view.as_ref(app).active_conversation_id(app)?;
                            (terminal_view_conversation_id == conversation_id)
                                .then_some((*window_id, tab_idx))
                        })
                })
        })
}

/// Per-notification metadata derived from a single [`TerminalView`] lookup. Both fields
/// are read on the same emit path, so we resolve the view once and pass the projection
/// down rather than walking the workspace tree for each.
struct TerminalViewMetadata {
    is_ambient: bool,
    branch: Option<String>,
}

impl TerminalViewMetadata {
    fn lookup(terminal_view_id: EntityId, app: &AppContext) -> Self {
        let Some(terminal_view) = find_terminal_view_by_id(terminal_view_id, app) else {
            return Self {
                is_ambient: false,
                branch: None,
            };
        };
        let view = terminal_view.as_ref(app);
        Self {
            is_ambient: view.is_ambient_agent_session(app),
            branch: view.current_git_branch(app),
        }
    }
}

fn find_terminal_view_by_id(
    terminal_view_id: EntityId,
    app: &AppContext,
) -> Option<ViewHandle<TerminalView>> {
    for (_, workspace_handle) in WorkspaceRegistry::as_ref(app).all_workspaces(app) {
        for pane_group in workspace_handle.as_ref(app).tab_views() {
            let pane_group = pane_group.as_ref(app);
            for pane_id in pane_group.terminal_pane_ids() {
                if let Some(terminal_view) = pane_group.terminal_view_from_pane_id(pane_id, app) {
                    if terminal_view.id() == terminal_view_id {
                        return Some(terminal_view);
                    }
                }
            }
        }
    }
    None
}

fn active_focused_terminal_id(app: &AppContext) -> Option<EntityId> {
    let active_window = app.windows().active_window()?;
    let workspace = app
        .views_of_type::<Workspace>(active_window)
        .and_then(|views| views.first().cloned())?;

    let workspace = workspace.as_ref(app);
    workspace.active_terminal_id(app)
}

/// The terminal in the focused pane of the active window's active tab: the one
/// pane a reply can come from while the user is looking at it. `None` when no
/// window is active, as when the app is in the background, or when that pane
/// isn't a terminal.
fn focused_terminal_in_active_window(app: &AppContext) -> Option<EntityId> {
    #[cfg(test)]
    {
        if let Some(focused) = ACTIVE_WINDOW_FOCUS_FOR_TESTS.with(std::cell::Cell::get) {
            return focused;
        }
    }
    let window_id = active_window_id(app)?;
    if !app.has_singleton_model::<WorkspaceRegistry>() {
        return None;
    }
    let workspace = WorkspaceRegistry::as_ref(app).get(window_id, app)?;
    workspace
        .as_ref(app)
        .active_tab_focused_terminal_view_id(app)
}

/// The active window, the key window on macOS: `None` while the app is in the
/// background.
pub(crate) fn active_window_id(app: &AppContext) -> Option<WindowId> {
    #[cfg(test)]
    {
        if let Some(window_id) = ACTIVE_WINDOW_FOR_TESTS.with(std::cell::Cell::get) {
            return Some(window_id);
        }
    }
    app.windows().active_window()
}

#[cfg(test)]
thread_local! {
    /// App tests can't make a window active, so a test that needs one names
    /// the terminal that would be focused in it (`Some(None)` for a
    /// non-terminal pane).
    static ACTIVE_WINDOW_FOCUS_FOR_TESTS: std::cell::Cell<Option<Option<EntityId>>> =
        const { std::cell::Cell::new(None) };

    /// Or, for a test with real workspaces, the window that would be active.
    static ACTIVE_WINDOW_FOR_TESTS: std::cell::Cell<Option<WindowId>> =
        const { std::cell::Cell::new(None) };
}

/// Makes a window the active one for as long as it lives, since app tests
/// have no active window of their own.
#[cfg(test)]
pub(crate) struct ActiveWindowForTests;

#[cfg(test)]
impl ActiveWindowForTests {
    pub(crate) fn set(window_id: WindowId) -> Self {
        ACTIVE_WINDOW_FOR_TESTS.with(|cell| cell.set(Some(window_id)));
        Self
    }
}

#[cfg(test)]
impl Drop for ActiveWindowForTests {
    fn drop(&mut self) {
        ACTIVE_WINDOW_FOR_TESTS.with(|cell| cell.set(None));
    }
}

#[cfg(test)]
#[path = "agent_management_model_tests.rs"]
mod tests;
