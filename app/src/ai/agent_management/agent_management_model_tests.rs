use std::collections::{HashMap, HashSet};

use settings::Setting as _;
use warp_core::features::FeatureFlag;
use warp_errors::report_if_error;
use warpui::{App, EntityId, ModelHandle, SingletonEntity, WindowId};

use super::{AgentNotificationsModel, ACTIVE_WINDOW_FOCUS_FOR_TESTS};
use crate::ai::active_agent_views_model::ActiveAgentViewsModel;
use crate::ai::agent::conversation::{AIConversation, AIConversationId, ConversationStatus};
use crate::ai::agent_management::notifications::{
    NotificationCategory, NotificationFilter, NotificationOrigin, NotificationSourceAgent,
};
use crate::ai::artifacts::Artifact;
use crate::ai::blocklist::BlocklistAIHistoryEvent;
use crate::settings::AISettings;
use crate::terminal::cli_agent_sessions::{
    CLIAgentSessionContext, CLIAgentSessionStatus, CLIAgentSessionsModel,
    CLIAgentSessionsModelEvent,
};
use crate::terminal::CLIAgent;
use crate::test_util::settings::initialize_settings_for_tests;
use crate::workspace::WorkspaceRegistry;
use crate::BlocklistAIHistoryModel;

fn setup_app(
    app: &mut App,
) -> (
    ModelHandle<BlocklistAIHistoryModel>,
    ModelHandle<AgentNotificationsModel>,
) {
    initialize_settings_for_tests(app);
    let history = app.add_singleton_model(|_| BlocklistAIHistoryModel::new(vec![], vec![], &[]));
    // Registered after the history model since it subscribes to history events; the
    // notifications model reads it to suppress completion notifications when a prompt is queued.
    app.add_singleton_model(crate::ai::blocklist::QueuedQueryModel::new);
    app.add_singleton_model(|_| CLIAgentSessionsModel::new());
    app.add_singleton_model(|_| ActiveAgentViewsModel::new());
    let notifications = app.add_singleton_model(AgentNotificationsModel::new);
    (history, notifications)
}

fn make_pr_artifact(url: &str, branch: &str) -> Artifact {
    Artifact::PullRequest {
        url: url.to_string(),
        branch: branch.to_string(),
        repo: None,
        number: None,
    }
}

fn make_plan_artifact(doc_uid: &str, title: &str) -> Artifact {
    Artifact::Plan {
        document_uid: doc_uid.to_string(),
        notebook_uid: None,
        title: Some(title.to_string()),
    }
}

#[test]
fn artifact_event_accumulates_into_pending() {
    App::test((), |mut app| async move {
        let _guard = FeatureFlag::HOANotifications.override_enabled(true);
        let (history, notifications) = setup_app(&mut app);

        let conversation_id = AIConversationId::new();
        let terminal_view_id = EntityId::new();

        history.update(&mut app, |_: &mut BlocklistAIHistoryModel, ctx| {
            ctx.emit(BlocklistAIHistoryEvent::UpdatedConversationArtifacts {
                terminal_surface_id: terminal_view_id,
                conversation_id,
                artifact: make_pr_artifact("https://github.com/org/repo/pull/42", "feature-branch"),
            });
        });

        notifications.read(&app, |model, _| {
            let pending = model.pending_artifacts.get(&conversation_id).unwrap();
            assert_eq!(pending.len(), 1);
            assert!(matches!(&pending[0], Artifact::PullRequest { branch, .. } if branch == "feature-branch"));
        });
    });
}

#[test]
fn multiple_artifacts_accumulated_across_turns() {
    App::test((), |mut app| async move {
        let _guard = FeatureFlag::HOANotifications.override_enabled(true);
        let (history, notifications) = setup_app(&mut app);

        let conversation_id = AIConversationId::new();
        let terminal_view_id = EntityId::new();

        history.update(&mut app, |_: &mut BlocklistAIHistoryModel, ctx| {
            ctx.emit(BlocklistAIHistoryEvent::UpdatedConversationArtifacts {
                terminal_surface_id: terminal_view_id,
                conversation_id,
                artifact: make_plan_artifact("doc-1", "My Plan"),
            });
        });
        history.update(&mut app, |_: &mut BlocklistAIHistoryModel, ctx| {
            ctx.emit(BlocklistAIHistoryEvent::UpdatedConversationArtifacts {
                terminal_surface_id: terminal_view_id,
                conversation_id,
                artifact: make_pr_artifact("https://github.com/org/repo/pull/1", "main"),
            });
        });

        notifications.read(&app, |model, _| {
            let pending = model.pending_artifacts.get(&conversation_id).unwrap();
            assert_eq!(pending.len(), 2);
            assert!(matches!(&pending[0], Artifact::Plan { title: Some(t), .. } if t == "My Plan"));
            assert!(matches!(&pending[1], Artifact::PullRequest { .. }));
        });
    });
}

#[test]
fn add_notification_tracks_unread_activity_when_in_app_notifications_are_hidden() {
    App::test((), |mut app| async move {
        let _guard = FeatureFlag::HOANotifications.override_enabled(true);
        let (_history, notifications) = setup_app(&mut app);

        AISettings::handle(&app).update(&mut app, |settings, ctx| {
            report_if_error!(settings.show_agent_notifications.set_value(false, ctx));
        });

        let conversation_id = AIConversationId::new();
        let terminal_view_id = EntityId::new();
        notifications.update(&mut app, |model, ctx| {
            model.add_notification(
                "Agent task".to_owned(),
                "Task completed.".to_owned(),
                NotificationCategory::Complete,
                NotificationSourceAgent::Oz { is_ambient: false },
                NotificationOrigin::Conversation(conversation_id),
                terminal_view_id,
                vec![],
                None,
                ctx,
            );
        });

        notifications.read(&app, |model, _| {
            assert_eq!(
                model
                    .notifications()
                    .filtered_count(NotificationFilter::All),
                1
            );
            assert!(model
                .notifications()
                .has_unread_for_terminal_view(terminal_view_id));
        });
    });
}

#[test]
fn is_unread_follows_a_notification_until_it_is_read() {
    App::test((), |mut app| async move {
        let _guard = FeatureFlag::HOANotifications.override_enabled(true);
        let (_history, notifications) = setup_app(&mut app);

        let terminal_view_id = EntityId::new();
        notifications.update(&mut app, |model, ctx| {
            assert!(!model.is_unread(terminal_view_id));
            model.add_notification(
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
            assert!(model.is_unread(terminal_view_id));

            model.mark_items_from_terminal_view_read(terminal_view_id, ctx);
            assert!(!model.is_unread(terminal_view_id));
        });
    });
}

#[test]
fn flush_drains_pending_artifacts() {
    App::test((), |mut app| async move {
        let _guard = FeatureFlag::HOANotifications.override_enabled(true);
        let (history, notifications) = setup_app(&mut app);

        let conversation_id = AIConversationId::new();
        let terminal_view_id = EntityId::new();

        history.update(&mut app, |_: &mut BlocklistAIHistoryModel, ctx| {
            ctx.emit(BlocklistAIHistoryEvent::UpdatedConversationArtifacts {
                terminal_surface_id: terminal_view_id,
                conversation_id,
                artifact: make_pr_artifact("https://github.com/org/repo/pull/1", "branch-1"),
            });
        });

        notifications.update(&mut app, |model, _| {
            let artifacts = model.flush_pending_artifacts(conversation_id);
            assert_eq!(artifacts.len(), 1);
            assert!(matches!(&artifacts[0], Artifact::PullRequest { branch, .. } if branch == "branch-1"));
        });

        notifications.read(&app, |model, _| {
            assert!(!model.pending_artifacts.contains_key(&conversation_id));
        });
    });
}

#[test]
fn flush_returns_empty_vec_when_no_artifacts() {
    App::test((), |mut app| async move {
        let _guard = FeatureFlag::HOANotifications.override_enabled(true);
        let (_history, notifications) = setup_app(&mut app);

        let conversation_id = AIConversationId::new();

        notifications.update(&mut app, |model, _| {
            let artifacts = model.flush_pending_artifacts(conversation_id);
            assert!(artifacts.is_empty());
        });
    });
}

#[test]
fn deletion_cleans_up_pending_artifacts() {
    App::test((), |mut app| async move {
        let _guard = FeatureFlag::HOANotifications.override_enabled(true);
        let (history, notifications) = setup_app(&mut app);

        let conversation_id = AIConversationId::new();
        let terminal_view_id = EntityId::new();

        history.update(&mut app, |_: &mut BlocklistAIHistoryModel, ctx| {
            ctx.emit(BlocklistAIHistoryEvent::UpdatedConversationArtifacts {
                terminal_surface_id: terminal_view_id,
                conversation_id,
                artifact: make_pr_artifact("https://github.com/org/repo/pull/1", "branch-1"),
            });
        });

        history.update(&mut app, |_: &mut BlocklistAIHistoryModel, ctx| {
            ctx.emit(BlocklistAIHistoryEvent::DeletedConversation {
                terminal_surface_id: terminal_view_id,
                conversation_id,
                conversation_title: None,
                run_id: None,
            });
        });

        notifications.read(&app, |model, _| {
            assert!(!model.pending_artifacts.contains_key(&conversation_id));
        });
    });
}

#[test]
fn separate_conversations_have_independent_pending_artifacts() {
    App::test((), |mut app| async move {
        let _guard = FeatureFlag::HOANotifications.override_enabled(true);
        let (history, notifications) = setup_app(&mut app);

        let conv_a = AIConversationId::new();
        let conv_b = AIConversationId::new();
        let terminal_view_id = EntityId::new();

        history.update(&mut app, |_: &mut BlocklistAIHistoryModel, ctx| {
            ctx.emit(BlocklistAIHistoryEvent::UpdatedConversationArtifacts {
                terminal_surface_id: terminal_view_id,
                conversation_id: conv_a,
                artifact: make_pr_artifact("https://github.com/org/repo/pull/1", "branch-a"),
            });
        });
        history.update(&mut app, |_: &mut BlocklistAIHistoryModel, ctx| {
            ctx.emit(BlocklistAIHistoryEvent::UpdatedConversationArtifacts {
                terminal_surface_id: terminal_view_id,
                conversation_id: conv_b,
                artifact: make_plan_artifact("doc-b", "Plan B"),
            });
        });

        notifications.update(&mut app, |model, _| {
            let a = model.flush_pending_artifacts(conv_a);
            assert_eq!(a.len(), 1);
            assert!(matches!(&a[0], Artifact::PullRequest { branch, .. } if branch == "branch-a"));

            let b = model.flush_pending_artifacts(conv_b);
            assert_eq!(b.len(), 1);
            assert!(matches!(&b[0], Artifact::Plan { title: Some(t), .. } if t == "Plan B"));
        });
    });
}

// should_trigger_notification: pure-function tests pinning which statuses
// fire user-facing notifications. Terminal-error and blocked surface;
// in-progress, waiting-for-events, and user-cancelled do not.

#[test]
fn should_trigger_notification_returns_true_for_success() {
    assert!(ConversationStatus::Success.should_trigger_notification());
}

#[test]
fn should_trigger_notification_returns_true_for_blocked() {
    assert!(ConversationStatus::Blocked {
        blocked_action: "approve diff".to_owned(),
    }
    .should_trigger_notification());
}

#[test]
fn should_trigger_notification_returns_true_for_error() {
    assert!(ConversationStatus::Error.should_trigger_notification());
}

#[test]
fn should_trigger_notification_returns_false_for_in_progress() {
    assert!(!ConversationStatus::InProgress.should_trigger_notification());
}

#[test]
fn should_trigger_notification_returns_false_for_waiting_for_events() {
    assert!(!ConversationStatus::WaitingForEvents.should_trigger_notification());
}

#[test]
fn should_trigger_notification_returns_false_for_cancelled() {
    assert!(!ConversationStatus::Cancelled.should_trigger_notification());
}

// Mailbox suppression for non-terminal status updates. In App::test the
// `is_conversation_open` gate always returns false, so the
// WaitingForEvents and InProgress arms both clear stale notifications
// regardless of status; this still pins the user-visible contract that
// no stale "Task completed" toast survives a non-terminal transition.

/// Disables `show_agent_notifications` so subsequent `add_notification`
/// calls skip the `send_telemetry_from_ctx!` branch — the test app does
/// not register a `TelemetryContextProvider` singleton and the macro
/// would otherwise panic.
fn disable_telemetry_path(app: &mut App) {
    AISettings::handle(app).update(app, |settings, ctx| {
        report_if_error!(settings.show_agent_notifications.set_value(false, ctx));
    });
}

/// Pre-populates a `Complete` notification for `conversation_id` so that a
/// subsequent non-terminal status update has something to clear.
fn seed_stale_notification(
    notifications: &ModelHandle<AgentNotificationsModel>,
    app: &mut App,
    conversation_id: AIConversationId,
    terminal_view_id: EntityId,
) {
    notifications.update(app, |model, ctx| {
        model.add_notification(
            "Agent task".to_owned(),
            "Task completed.".to_owned(),
            NotificationCategory::Complete,
            NotificationSourceAgent::Oz { is_ambient: false },
            NotificationOrigin::Conversation(conversation_id),
            terminal_view_id,
            vec![],
            None,
            ctx,
        );
    });
}

#[test]
fn waiting_for_events_clears_stale_notification_and_adds_none() {
    App::test((), |mut app| async move {
        let _guard = FeatureFlag::HOANotifications.override_enabled(true);
        let (history, notifications) = setup_app(&mut app);
        disable_telemetry_path(&mut app);

        let conversation = AIConversation::new(false, false);
        let conversation_id = conversation.id();
        let terminal_view_id = EntityId::new();
        history.update(&mut app, |model, ctx| {
            model.restore_conversations(terminal_view_id, vec![conversation], ctx);
        });

        seed_stale_notification(&notifications, &mut app, conversation_id, terminal_view_id);
        notifications.read(&app, |model, _| {
            assert_eq!(
                model
                    .notifications()
                    .filtered_count(NotificationFilter::All),
                1,
                "precondition: one stale notification queued"
            );
        });

        history.update(&mut app, |model, ctx| {
            let conv = model
                .conversation_mut(&conversation_id)
                .expect("conversation was just restored");
            conv.update_status(ConversationStatus::WaitingForEvents, terminal_view_id, ctx);
        });

        notifications.read(&app, |model, _| {
            assert_eq!(
                model
                    .notifications()
                    .filtered_count(NotificationFilter::All),
                0,
                "WaitingForEvents must clear stale notifications and add no new toast"
            );
        });
    });
}

#[test]
fn in_progress_resume_clears_stale_notification_and_adds_none() {
    App::test((), |mut app| async move {
        let _guard = FeatureFlag::HOANotifications.override_enabled(true);
        let (history, notifications) = setup_app(&mut app);
        disable_telemetry_path(&mut app);

        let conversation = AIConversation::new(false, false);
        let conversation_id = conversation.id();
        let terminal_view_id = EntityId::new();
        history.update(&mut app, |model, ctx| {
            model.restore_conversations(terminal_view_id, vec![conversation], ctx);
        });

        // First move the conversation into WaitingForEvents, then back into
        // InProgress. The second transition is the resume signal that
        // PRODUCT.md (18) requires not to fire a notification.
        history.update(&mut app, |model, ctx| {
            let conv = model
                .conversation_mut(&conversation_id)
                .expect("conversation was just restored");
            conv.update_status(ConversationStatus::WaitingForEvents, terminal_view_id, ctx);
        });

        seed_stale_notification(&notifications, &mut app, conversation_id, terminal_view_id);
        notifications.read(&app, |model, _| {
            assert_eq!(
                model
                    .notifications()
                    .filtered_count(NotificationFilter::All),
                1,
                "precondition: one stale notification queued before the resume transition"
            );
        });

        history.update(&mut app, |model, ctx| {
            let conv = model
                .conversation_mut(&conversation_id)
                .expect("conversation still exists");
            conv.update_status(ConversationStatus::InProgress, terminal_view_id, ctx);
        });

        notifications.read(&app, |model, _| {
            assert_eq!(
                model
                    .notifications()
                    .filtered_count(NotificationFilter::All),
                0,
                "WaitingForEvents → InProgress resume must not fire a notification \
                 (covers PRODUCT.md (18))"
            );
        });
    });
}

/// Names the terminal focused in the active window for as long as it lives,
/// since app tests have no active window of their own.
struct ActiveWindowFocus;

impl ActiveWindowFocus {
    fn on(focused: Option<EntityId>) -> Self {
        ACTIVE_WINDOW_FOCUS_FOR_TESTS.with(|cell| cell.set(Some(focused)));
        Self
    }
}

impl Drop for ActiveWindowFocus {
    fn drop(&mut self) {
        ACTIVE_WINDOW_FOCUS_FOR_TESTS.with(|cell| cell.set(None));
    }
}

fn emit_cli_status(app: &mut App, terminal_view_id: EntityId, status: CLIAgentSessionStatus) {
    CLIAgentSessionsModel::handle(app).update(app, |_, ctx| {
        ctx.emit(CLIAgentSessionsModelEvent::StatusChanged {
            terminal_view_id,
            agent: CLIAgent::Claude,
            status,
            session_context: Box::new(CLIAgentSessionContext::default()),
        });
    });
}

fn is_unread(
    notifications: &ModelHandle<AgentNotificationsModel>,
    app: &App,
    terminal_view_id: EntityId,
) -> bool {
    notifications.read(app, |model, _| model.is_unread(terminal_view_id))
}

#[test]
fn a_manual_mark_is_unread_without_a_notification() {
    App::test((), |mut app| async move {
        let (_history, notifications) = setup_app(&mut app);
        let marked = EntityId::new();
        notifications.update(&mut app, |model, ctx| {
            model.mark_unread(&[marked], ctx);
            assert!(model.is_unread(marked));
            assert!(!model.is_unread(EntityId::new()));
            assert_eq!(
                model
                    .notifications()
                    .filtered_count(NotificationFilter::All),
                0
            );
        });
    });
}

#[test]
fn focus_reports_that_must_not_clear_a_mark() {
    App::test((), |mut app| async move {
        let (_history, notifications) = setup_app(&mut app);
        let (front, back) = (WindowId::new(), WindowId::new());
        let (marked, other) = (EntityId::new(), EntityId::new());
        notifications.update(&mut app, |model, ctx| {
            model.mark_unread(&[marked], ctx);

            // The first report in a window is a baseline, even of the marked view.
            model.record_terminal_focus(front, Some(marked), true, ctx);
            assert!(model.is_unread(marked), "the first report");

            // The same view again: a menu, the palette or the file tree closing.
            model.record_terminal_focus(front, Some(marked), true, ctx);
            assert!(model.is_unread(marked), "the same view again");

            // A window in the background, even one whose focus moves to the view.
            model.record_terminal_focus(back, Some(other), false, ctx);
            model.record_terminal_focus(back, Some(marked), false, ctx);
            assert!(model.is_unread(marked), "an inactive window");

            // That window coming to the front reports the view it already had.
            model.record_terminal_focus(back, Some(marked), true, ctx);
            assert!(model.is_unread(marked), "a window refocus");
        });
    });
}

#[test]
fn an_arrival_clears_a_mark() {
    App::test((), |mut app| async move {
        let (_history, notifications) = setup_app(&mut app);
        let window = WindowId::new();
        let (marked, other) = (EntityId::new(), EntityId::new());
        notifications.update(&mut app, |model, ctx| {
            model.record_terminal_focus(window, Some(other), true, ctx);
            model.mark_unread(&[marked], ctx);
            model.record_terminal_focus(window, Some(marked), true, ctx);
            assert!(!model.is_unread(marked), "an arrival from another terminal");

            // Leaving for a non-terminal pane clears nothing; coming back does.
            model.mark_unread(&[marked], ctx);
            model.record_terminal_focus(window, None, true, ctx);
            assert!(model.is_unread(marked));
            model.record_terminal_focus(window, Some(marked), true, ctx);
            assert!(
                !model.is_unread(marked),
                "an arrival from a non-terminal pane"
            );
        });
    });
}

#[test]
fn a_reply_clears_a_mark_only_in_the_pane_being_looked_at() {
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);
    // The clear doesn't depend on the mailbox.
    let _mailbox = FeatureFlag::HOANotifications.override_enabled(false);
    App::test((), |mut app| async move {
        let (history, notifications) = setup_app(&mut app);
        let (marked, elsewhere) = (EntityId::new(), EntityId::new());
        let mark = |app: &mut App| {
            notifications.update(app, |model, ctx| model.mark_unread(&[marked], ctx));
        };

        mark(&mut app);
        // No active window, as while the app is in the background.
        emit_cli_status(&mut app, marked, CLIAgentSessionStatus::InProgress);
        assert!(is_unread(&notifications, &app, marked), "no active window");
        // A prompt that starts on its own in a background tab: a /loop, or a
        // queued or scheduled prompt.
        {
            let _focus = ActiveWindowFocus::on(Some(elsewhere));
            emit_cli_status(&mut app, marked, CLIAgentSessionStatus::InProgress);
        }
        assert!(
            is_unread(&notifications, &app, marked),
            "another pane focused"
        );
        {
            let _focus = ActiveWindowFocus::on(None);
            emit_cli_status(&mut app, marked, CLIAgentSessionStatus::InProgress);
        }
        assert!(
            is_unread(&notifications, &app, marked),
            "a non-terminal pane focused"
        );
        {
            let _focus = ActiveWindowFocus::on(Some(marked));
            emit_cli_status(&mut app, marked, CLIAgentSessionStatus::Success);
        }
        assert!(
            is_unread(&notifications, &app, marked),
            "a finished turn isn't a reply"
        );
        // The next prompt, typed into the pane being looked at.
        {
            let _focus = ActiveWindowFocus::on(Some(marked));
            emit_cli_status(&mut app, marked, CLIAgentSessionStatus::InProgress);
        }
        assert!(!is_unread(&notifications, &app, marked), "a CLI reply");

        // Oz: the conversation in the pane resuming.
        let conversation = AIConversation::new(false, false);
        let conversation_id = conversation.id();
        history.update(&mut app, |model, ctx| {
            model.restore_conversations(marked, vec![conversation], ctx);
        });
        let set_status = |app: &mut App, status: ConversationStatus| {
            history.update(app, |model, ctx| {
                model
                    .conversation_mut(&conversation_id)
                    .expect("conversation was just restored")
                    .update_status(status, marked, ctx);
            });
        };
        set_status(&mut app, ConversationStatus::WaitingForEvents);
        mark(&mut app);
        {
            let _focus = ActiveWindowFocus::on(Some(elsewhere));
            set_status(&mut app, ConversationStatus::InProgress);
        }
        assert!(
            is_unread(&notifications, &app, marked),
            "Oz resuming in a background tab"
        );
        set_status(&mut app, ConversationStatus::WaitingForEvents);
        {
            let _focus = ActiveWindowFocus::on(Some(marked));
            set_status(&mut app, ConversationStatus::InProgress);
        }
        assert!(!is_unread(&notifications, &app, marked), "an Oz reply");
    });
}

#[test]
fn a_reply_leaves_marks_alone_without_tab_mark_unread() {
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(false);
    App::test((), |mut app| async move {
        let (_history, notifications) = setup_app(&mut app);
        let marked = EntityId::new();
        notifications.update(&mut app, |model, ctx| model.mark_unread(&[marked], ctx));
        {
            let _focus = ActiveWindowFocus::on(Some(marked));
            emit_cli_status(&mut app, marked, CLIAgentSessionStatus::InProgress);
        }
        assert!(is_unread(&notifications, &app, marked));
    });
}

#[test]
fn mark_read_clears_manual_and_restored_marks_and_reads_notifications() {
    App::test((), |mut app| async move {
        let _guard = FeatureFlag::HOANotifications.override_enabled(true);
        let (_history, notifications) = setup_app(&mut app);
        let (manual, restored, notified) = (EntityId::new(), EntityId::new(), EntityId::new());
        notifications.update(&mut app, |model, ctx| {
            model.mark_unread(&[manual], ctx);
            model.stage_restored_unread(restored, ctx);
            model.add_notification(
                "Agent task".to_owned(),
                "Task completed.".to_owned(),
                NotificationCategory::Complete,
                NotificationSourceAgent::Oz { is_ambient: false },
                NotificationOrigin::Conversation(AIConversationId::new()),
                notified,
                vec![],
                None,
                ctx,
            );
            let all = [manual, restored, notified];
            assert!(all.iter().all(|id| model.is_unread(*id)));

            model.mark_read(&all, ctx);
            assert!(!all.iter().any(|id| model.is_unread(*id)));
            assert!(!model.has_staged_marks());
            assert!(!model.notifications().has_unread_for_terminal_view(notified));
        });
    });
}

#[test]
fn forgetting_a_view_drops_its_marks() {
    App::test((), |mut app| async move {
        let (_history, notifications) = setup_app(&mut app);
        let (manual, restored) = (EntityId::new(), EntityId::new());
        notifications.update(&mut app, |model, ctx| {
            model.mark_unread(&[manual], ctx);
            model.stage_restored_unread(restored, ctx);
            model.forget_terminal_view(manual, ctx);
            model.forget_terminal_view(restored, ctx);
            assert!(!model.is_unread(manual));
            assert!(!model.is_unread(restored));
            assert!(!model.has_staged_marks());
        });
    });
}

#[test]
fn a_restored_mark_survives_arrivals_until_committed_and_the_commit_sets_the_baseline() {
    App::test((), |mut app| async move {
        let (_history, notifications) = setup_app(&mut app);
        let window = WindowId::new();
        let (restored, other, in_another_window) =
            (EntityId::new(), EntityId::new(), EntityId::new());
        notifications.update(&mut app, |model, ctx| {
            model.stage_restored_unread(restored, ctx);
            model.stage_restored_unread(in_another_window, ctx);
            model.record_terminal_focus(window, Some(other), true, ctx);
            model.record_terminal_focus(window, Some(restored), true, ctx);
            assert!(
                model.is_unread(restored),
                "a staged mark survives an arrival"
            );

            model.commit_restored_unread(window, &[restored, other], Some(restored));
            assert!(model.is_unread(restored));
            assert!(
                model.has_staged_marks(),
                "a mark staged in another window waits for that window's commit"
            );

            // The commit made the focused view the baseline, so reporting it
            // again isn't an arrival.
            model.record_terminal_focus(window, Some(restored), true, ctx);
            assert!(model.is_unread(restored));
            model.record_terminal_focus(window, Some(other), true, ctx);
            model.record_terminal_focus(window, Some(restored), true, ctx);
            assert!(
                !model.is_unread(restored),
                "committed, it clears like any other mark"
            );
        });
    });
}

#[test]
fn a_window_the_registry_no_longer_knows_loses_its_focus_baseline() {
    App::test((), |mut app| async move {
        let (_history, notifications) = setup_app(&mut app);
        // No workspace is registered, so every window but the reporting one
        // has closed.
        app.add_singleton_model(|_| WorkspaceRegistry::new());
        let (closed, open) = (WindowId::new(), WindowId::new());
        let (marked, other) = (EntityId::new(), EntityId::new());
        notifications.update(&mut app, |model, ctx| {
            model.record_terminal_focus(closed, Some(other), true, ctx);
            assert!(
                model.last_focus_by_window.contains_key(&closed),
                "the reporting window stays"
            );

            model.record_terminal_focus(open, Some(other), false, ctx);
            assert!(!model.last_focus_by_window.contains_key(&closed));
            assert!(model.last_focus_by_window.contains_key(&open));

            // Its baseline gone, the window's next report is a baseline again.
            model.mark_unread(&[marked], ctx);
            model.record_terminal_focus(closed, Some(marked), true, ctx);
            assert!(model.is_unread(marked));
        });
    });
}

/// A small deterministic generator, so a failing run can be replayed from its
/// seed.
struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % bound as u64) as usize
    }
}

/// Two windows of three tabs of two terminal panes, as the unread model sees
/// them through focus reports.
struct World {
    windows: [WindowId; 2],
    panes: [[[EntityId; 2]; 3]; 2],
    active_window: usize,
    active_tab: [usize; 2],
    /// Each tab's focused pane, `None` for a non-terminal pane beside them.
    focused_pane: [[Option<usize>; 3]; 2],
}

impl World {
    fn new() -> Self {
        Self {
            windows: [WindowId::new(), WindowId::new()],
            panes: std::array::from_fn(|_| {
                std::array::from_fn(|_| std::array::from_fn(|_| EntityId::new()))
            }),
            active_window: 0,
            active_tab: [0, 0],
            focused_pane: [[Some(0); 3]; 2],
        }
    }

    /// The terminal focused in the window's active tab.
    fn focus(&self, window: usize) -> Option<EntityId> {
        let tab = self.active_tab[window];
        self.focused_pane[window][tab].map(|pane| self.panes[window][tab][pane])
    }

    fn window_panes(&self, window: usize) -> Vec<EntityId> {
        self.panes[window].iter().flatten().copied().collect()
    }

    fn all_panes(&self) -> Vec<EntityId> {
        (0..2)
            .flat_map(|window| self.window_panes(window))
            .collect()
    }
}

/// What the rule says, worked out apart from the model: when each view was
/// last marked and last cleared, which views are staged, and what each window
/// last reported.
#[derive(Default)]
struct Expected {
    last_mark: HashMap<EntityId, usize>,
    last_clear: HashMap<EntityId, usize>,
    staged: HashSet<EntityId>,
    reported: HashMap<WindowId, Option<EntityId>>,
}

impl Expected {
    /// Marked after its last arrival, qualifying reply, Mark as Read or close.
    fn manually_unread(&self, view: EntityId) -> bool {
        self.last_mark
            .get(&view)
            .is_some_and(|mark| self.last_clear.get(&view).is_none_or(|clear| mark > clear))
    }

    /// A report is an arrival when the window is in front, had reported
    /// before, and reported something else then.
    fn report(
        &mut self,
        window: WindowId,
        focused: Option<EntityId>,
        is_active: bool,
        step: usize,
    ) {
        let previous = self.reported.insert(window, focused);
        if let Some(focused) = focused {
            if is_active && previous.is_some_and(|previous| previous != Some(focused)) {
                self.last_clear.insert(focused, step);
            }
        }
    }
}

/// What happens in one step of the sweep.
#[derive(Clone, Copy)]
enum SweepEvent {
    /// Mark as Unread.
    MarkUnread,
    /// Mark as Read.
    MarkRead,
    /// The same view reported again.
    SameFocus,
    /// Focus moves to a terminal, in any tab of either window.
    FocusTerminal,
    /// Focus moves to a non-terminal pane.
    FocusNonTerminal,
    /// A window comes to the front and reports what it had.
    WindowRefocus,
    /// The window in the background reports a focus change.
    BackgroundReport,
    /// A reply: the agent in the view starts a turn.
    Reply,
    /// Restore stages a mark.
    Stage,
    /// Restore commits a window's marks.
    Commit,
    /// The pane closes for good, and a new one takes its place.
    Close,
}

const SWEEP_EVENTS: [SweepEvent; 11] = [
    SweepEvent::MarkUnread,
    SweepEvent::MarkRead,
    SweepEvent::SameFocus,
    SweepEvent::FocusTerminal,
    SweepEvent::FocusNonTerminal,
    SweepEvent::WindowRefocus,
    SweepEvent::BackgroundReport,
    SweepEvent::Reply,
    SweepEvent::Stage,
    SweepEvent::Commit,
    SweepEvent::Close,
];

struct Sweep {
    notifications: ModelHandle<AgentNotificationsModel>,
    world: World,
    expected: Expected,
}

impl Sweep {
    fn report(&mut self, app: &mut App, window: usize, is_active: bool, step: usize) {
        let window_id = self.world.windows[window];
        let focused = self.world.focus(window);
        self.notifications.update(app, |model, ctx| {
            model.record_terminal_focus(window_id, focused, is_active, ctx);
        });
        self.expected.report(window_id, focused, is_active, step);
    }

    fn step(&mut self, app: &mut App, rng: &mut Rng, step: usize) {
        let view = self.world.panes[rng.below(2)][rng.below(3)][rng.below(2)];
        match SWEEP_EVENTS[rng.below(SWEEP_EVENTS.len())] {
            SweepEvent::MarkUnread => {
                self.notifications
                    .update(app, |model, ctx| model.mark_unread(&[view], ctx));
                self.expected.last_mark.insert(view, step);
            }
            SweepEvent::MarkRead => {
                self.notifications
                    .update(app, |model, ctx| model.mark_read(&[view], ctx));
                self.expected.last_clear.insert(view, step);
                self.expected.staged.remove(&view);
            }
            SweepEvent::SameFocus => {
                let window = rng.below(2);
                self.report(app, window, window == self.world.active_window, step);
            }
            SweepEvent::FocusTerminal => {
                let (window, tab) = (rng.below(2), rng.below(3));
                self.world.active_tab[window] = tab;
                self.world.focused_pane[window][tab] = Some(rng.below(2));
                self.report(app, window, window == self.world.active_window, step);
            }
            SweepEvent::FocusNonTerminal => {
                let (window, tab) = (rng.below(2), rng.below(3));
                self.world.active_tab[window] = tab;
                self.world.focused_pane[window][tab] = None;
                self.report(app, window, window == self.world.active_window, step);
            }
            SweepEvent::WindowRefocus => {
                let window = rng.below(2);
                self.world.active_window = window;
                self.report(app, window, true, step);
            }
            SweepEvent::BackgroundReport => {
                let (window, tab) = (1 - self.world.active_window, rng.below(3));
                self.world.active_tab[window] = tab;
                self.world.focused_pane[window][tab] = (rng.below(3) != 0).then(|| rng.below(2));
                self.report(app, window, false, step);
            }
            SweepEvent::Reply => {
                let focused = self.world.focus(self.world.active_window);
                {
                    let _focus = ActiveWindowFocus::on(focused);
                    emit_cli_status(app, view, CLIAgentSessionStatus::InProgress);
                }
                if focused == Some(view) {
                    self.expected.last_clear.insert(view, step);
                }
            }
            SweepEvent::Stage => {
                self.notifications
                    .update(app, |model, ctx| model.stage_restored_unread(view, ctx));
                self.expected.staged.insert(view);
            }
            SweepEvent::Commit => {
                let window = rng.below(2);
                let window_id = self.world.windows[window];
                let views = self.world.window_panes(window);
                let focused = self.world.focus(window);
                self.notifications.update(app, |model, _| {
                    model.commit_restored_unread(window_id, &views, focused);
                });
                for view in views {
                    if self.expected.staged.remove(&view) {
                        self.expected.last_mark.insert(view, step);
                    }
                }
                self.expected.reported.insert(window_id, focused);
            }
            SweepEvent::Close => {
                self.notifications
                    .update(app, |model, ctx| model.forget_terminal_view(view, ctx));
                self.expected.last_clear.insert(view, step);
                self.expected.staged.remove(&view);
                for pane in self.world.panes.iter_mut().flatten().flatten() {
                    if *pane == view {
                        *pane = EntityId::new();
                    }
                }
            }
        }
    }
}

/// Seeded runs of 500 steps over two windows of three tabs of two terminal
/// panes. After every step, a view is manually unread exactly when it was
/// marked, or had its restored mark committed, after its last arrival,
/// qualifying reply, Mark as Read or close; and staged exactly when restore
/// staged it after its last commit, Mark as Read or close.
#[test]
fn unread_clearing_sweep() {
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);
    let _mailbox = FeatureFlag::HOANotifications.override_enabled(false);
    App::test((), |mut app| async move {
        let (_history, notifications) = setup_app(&mut app);
        for seed in 1..=16u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut sweep = Sweep {
                notifications: notifications.clone(),
                world: World::new(),
                expected: Expected::default(),
            };
            for step in 0..500 {
                sweep.step(&mut app, &mut rng, step);
                let views = sweep.world.all_panes();
                notifications.read(&app, |model, _| {
                    for view in views {
                        assert_eq!(
                            model.manually_unread.contains(&view),
                            sweep.expected.manually_unread(view),
                            "seed {seed}, step {step}: the manual mark on {view:?}"
                        );
                        assert_eq!(
                            model.staged_unread.contains(&view),
                            sweep.expected.staged.contains(&view),
                            "seed {seed}, step {step}: the staged mark on {view:?}"
                        );
                    }
                });
            }
        }
    });
}
