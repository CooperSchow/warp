use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use settings::Setting as _;
use warp_errors::report_if_error;
use warpui::platform::WindowStyle;
use warpui::r#async::Timer;
use warpui::{App, AppContext, EntityId, SingletonEntity as _, View as _, ViewContext, ViewHandle};

use super::{row_shows_unread, tab_is_unread, ARRIVAL_DWELL, STAGED_UNREAD_FALLBACK_COMMIT};
use crate::ai::agent_management::{ActiveWindowForTests, AgentNotificationsModel};
use crate::app_state::WindowSnapshot;
use crate::features::FeatureFlag;
use crate::menu::MenuItem;
use crate::notebooks::notebook::NotebookView;
use crate::pane_group::{Direction, NotebookPane, PaneGroup, PaneId};
use crate::root_view::NewWorkspaceSource;
use crate::tab::PaneNameMenuTarget;
use crate::workspace::tab_settings::{TabSettings, VerticalTabsDisplayGranularity};
use crate::workspace::view::tests::{initialize_app, mock_workspace};
use crate::workspace::{PaneViewLocator, Workspace, WorkspaceAction};
use crate::GlobalResourceHandles;

fn set_tab_layout(app: &mut App, vertical: bool, granularity: VerticalTabsDisplayGranularity) {
    TabSettings::handle(app).update(app, |settings, ctx| {
        report_if_error!(settings.use_vertical_tabs.set_value(vertical, ctx));
        report_if_error!(settings
            .vertical_tabs_display_granularity
            .set_value(granularity, ctx));
    });
}

/// Adds a terminal pane beside the tab's one, and returns both.
fn split_into_two_terminals(
    workspace: &Workspace,
    tab_index: usize,
    ctx: &mut ViewContext<Workspace>,
) -> (PaneId, PaneId) {
    workspace.tabs[tab_index]
        .pane_group
        .clone()
        .update(ctx, |pane_group, ctx| {
            let first = pane_group.focused_pane_id(ctx);
            let second = pane_group
                .add_terminal_pane(Direction::Right, None, ctx)
                .into();
            (first, second)
        })
}

fn terminal_view_id(pane_group: &PaneGroup, pane_id: PaneId, app: &AppContext) -> EntityId {
    pane_group
        .terminal_view_from_pane_id(pane_id, app)
        .expect("the pane should be a terminal")
        .id()
}

fn focused_terminal_view_id(workspace: &Workspace, tab_index: usize, app: &AppContext) -> EntityId {
    let pane_group = workspace.tabs[tab_index].pane_group.as_ref(app);
    terminal_view_id(pane_group, pane_group.focused_pane_id(app), app)
}

fn set_marks(read: &[EntityId], unread: &[EntityId], ctx: &mut ViewContext<Workspace>) {
    AgentNotificationsModel::handle(ctx).update(ctx, |model, ctx| {
        model.mark_read(read, ctx);
        model.mark_unread(unread, ctx);
    });
}

fn is_unread(terminal_view_id: EntityId, app: &AppContext) -> bool {
    AgentNotificationsModel::as_ref(app).is_unread(terminal_view_id)
}

fn row_target(pane_group_id: EntityId, pane_id: PaneId) -> PaneNameMenuTarget {
    PaneNameMenuTarget {
        locator: PaneViewLocator {
            pane_group_id,
            pane_id,
        },
        rename_label: "Rename pane",
        reset_label: "Reset pane name",
        is_pane_row: true,
    }
}

/// A right-click on a tab around its pane rows, which names the tab's active
/// pane.
fn tab_target(pane_group_id: EntityId, active_pane_id: PaneId) -> PaneNameMenuTarget {
    PaneNameMenuTarget {
        is_pane_row: false,
        rename_label: "Rename active pane",
        reset_label: "Reset active pane name",
        ..row_target(pane_group_id, active_pane_id)
    }
}

fn menu_items(
    workspace: &Workspace,
    tab_index: usize,
    target: Option<PaneNameMenuTarget>,
    app: &AppContext,
) -> Vec<MenuItem<WorkspaceAction>> {
    workspace.tabs[tab_index].menu_items_with_pane_name_target(
        tab_index,
        workspace.tabs.len(),
        0,
        &HashMap::new(),
        false,
        false,
        true,
        target,
        app,
    )
}

/// The tab menu's unread item: its label, the pane it targets (`None` for the
/// whole tab), and whether it marks unread.
fn unread_item(
    workspace: &Workspace,
    tab_index: usize,
    target: Option<PaneNameMenuTarget>,
    app: &AppContext,
) -> Option<(String, Option<EntityId>, bool)> {
    menu_items(workspace, tab_index, target, app)
        .iter()
        .filter_map(MenuItem::fields)
        .find_map(|fields| {
            if let Some(WorkspaceAction::SetTabUnread {
                terminal_view_id,
                unread,
                ..
            }) = fields.on_select_action()
            {
                Some((fields.label().to_owned(), *terminal_view_id, *unread))
            } else {
                None
            }
        })
}

fn restore(app: &mut App, window_snapshot: WindowSnapshot) -> ViewHandle<Workspace> {
    let global_resource_handles = GlobalResourceHandles::mock(app);
    let (_, workspace) = app.add_window(WindowStyle::NotStealFocus, |ctx| {
        Workspace::new(
            global_resource_handles,
            None,
            NewWorkspaceSource::Restored {
                window_snapshot,
                block_lists: Arc::new(HashMap::new()),
            },
            ctx,
        )
    });
    workspace
}

/// Sweeps every layout (horizontal, vertical Tabs, vertical Panes) × which of
/// a tab's two terminals has focus × which of them is unread. Each row's dot
/// follows the rule for its granularity, the menu opened on that row offers
/// Mark as Read exactly when the dot shows, and every entry point that isn't
/// one row speaks for the whole tab.
#[test]
fn the_menu_label_agrees_with_the_row_dot_for_every_layout_focus_and_unread_set() {
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);
    let _vertical_tabs = FeatureFlag::VerticalTabs.override_enabled(true);
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        // Tab 0 holds terminals A and B; tab 1 is the active tab.
        let (pane_group_id, a, b) = workspace.update(&mut app, |workspace, ctx| {
            let (a, b) = split_into_two_terminals(workspace, 0, ctx);
            workspace.add_terminal_tab(false, ctx);
            (workspace.tabs[0].pane_group.id(), a, b)
        });

        let mut cases = 0;
        for (vertical, granularity) in [
            (false, VerticalTabsDisplayGranularity::Tabs),
            (true, VerticalTabsDisplayGranularity::Tabs),
            (true, VerticalTabsDisplayGranularity::Panes),
        ] {
            set_tab_layout(&mut app, vertical, granularity);
            workspace.update(&mut app, |workspace, ctx| {
                let pane_group = workspace.tabs[0].pane_group.clone();
                let view_a = terminal_view_id(pane_group.as_ref(ctx), a, ctx);
                let view_b = terminal_view_id(pane_group.as_ref(ctx), b, ctx);
                let view_of = |pane_id: PaneId| if pane_id == a { view_a } else { view_b };
                for focused in [a, b] {
                    pane_group.update(ctx, |pane_group, ctx| {
                        pane_group.focus_pane_by_id(focused, ctx);
                    });
                    for unread_set in 0..4 {
                        let unread: Vec<EntityId> = [(1, view_a), (2, view_b)]
                            .into_iter()
                            .filter(|(bit, _)| unread_set & bit != 0)
                            .map(|(_, view)| view)
                            .collect();
                        set_marks(&[view_a, view_b], &unread, ctx);
                        cases += 1;
                        let any_unread = !unread.is_empty();
                        let context = format!(
                            "vertical {vertical}, {granularity:?}, focused {focused:?}, \
                             unread set {unread_set}"
                        );
                        assert_eq!(
                            tab_is_unread(pane_group.as_ref(ctx), ctx),
                            any_unread,
                            "{context}"
                        );

                        if vertical {
                            let rows = match granularity {
                                VerticalTabsDisplayGranularity::Tabs => vec![focused],
                                VerticalTabsDisplayGranularity::Panes => vec![a, b],
                            };
                            for row in rows {
                                let dot =
                                    row_shows_unread(pane_group.as_ref(ctx), row, granularity, ctx);
                                let (expected_dot, expected_target) = match granularity {
                                    // One row for the whole tab, whichever pane has focus.
                                    VerticalTabsDisplayGranularity::Tabs => (any_unread, None),
                                    VerticalTabsDisplayGranularity::Panes => {
                                        (unread.contains(&view_of(row)), Some(view_of(row)))
                                    }
                                };
                                assert_eq!(dot, expected_dot, "{context}, row {row:?}");
                                let item = unread_item(
                                    workspace,
                                    0,
                                    Some(row_target(pane_group_id, row)),
                                    ctx,
                                );
                                let expected_label = if dot {
                                    "Mark as Read"
                                } else {
                                    "Mark as Unread"
                                };
                                assert_eq!(
                                    item,
                                    Some((expected_label.to_owned(), expected_target, !dot)),
                                    "{context}, row {row:?}"
                                );
                            }
                        }

                        // No single row: the kebab or the horizontal tab bar
                        // (no target), and a right-click on the tab around
                        // its rows.
                        let expected_label = if any_unread {
                            "Mark as Read"
                        } else {
                            "Mark as Unread"
                        };
                        for target in [None, Some(tab_target(pane_group_id, focused))] {
                            assert_eq!(
                                unread_item(workspace, 0, target, ctx),
                                Some((expected_label.to_owned(), None, !any_unread)),
                                "{context}, whole tab"
                            );
                        }
                    }
                }
            });
        }
        assert_eq!(cases, 3 * 2 * 4);
    });
}

#[test]
fn mark_as_unread_on_a_tab_marks_its_focused_terminal_or_else_its_first() {
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            let (a, b) = split_into_two_terminals(workspace, 0, ctx);
            let pane_group = workspace.tabs[0].pane_group.clone();
            let view_a = terminal_view_id(pane_group.as_ref(ctx), a, ctx);
            let view_b = terminal_view_id(pane_group.as_ref(ctx), b, ctx);

            pane_group.update(ctx, |pane_group, ctx| pane_group.focus_pane_by_id(b, ctx));
            workspace.set_tab_unread(pane_group.id(), None, true, ctx);
            assert!(!is_unread(view_a, ctx));
            assert!(is_unread(view_b, ctx), "the focused terminal is marked");
            set_marks(&[view_a, view_b], &[], ctx);

            // A notebook beside the terminals has focus.
            pane_group.update(ctx, |pane_group, ctx| {
                let notebook = ctx.add_typed_action_view(NotebookView::new);
                pane_group.add_pane_with_direction(
                    Direction::Left,
                    NotebookPane::new(notebook, ctx),
                    true,
                    ctx,
                );
            });
            let first_terminal = {
                let pane_group = pane_group.as_ref(ctx);
                assert!(pane_group
                    .terminal_view_from_pane_id(pane_group.focused_pane_id(ctx), ctx)
                    .is_none());
                pane_group
                    .visible_pane_ids()
                    .into_iter()
                    .find_map(|pane_id| pane_group.terminal_view_from_pane_id(pane_id, ctx))
                    .expect("the tab still has terminals")
                    .id()
            };
            workspace.set_tab_unread(pane_group.id(), None, true, ctx);
            assert_eq!(
                [view_a, view_b].map(|view| is_unread(view, ctx)),
                [view_a, view_b].map(|view| view == first_terminal),
                "the first terminal pane is marked"
            );
        });
    });
}

#[test]
fn mark_as_read_clears_every_terminal_on_a_tab_and_one_on_a_pane() {
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            let (a, b) = split_into_two_terminals(workspace, 0, ctx);
            let pane_group_id = workspace.tabs[0].pane_group.id();
            let pane_group = workspace.tabs[0].pane_group.as_ref(ctx);
            let (view_a, view_b) = (
                terminal_view_id(pane_group, a, ctx),
                terminal_view_id(pane_group, b, ctx),
            );

            set_marks(&[], &[view_a, view_b], ctx);
            workspace.set_tab_unread(pane_group_id, None, false, ctx);
            assert!(!is_unread(view_a, ctx));
            assert!(!is_unread(view_b, ctx));

            set_marks(&[], &[view_a, view_b], ctx);
            workspace.set_tab_unread(pane_group_id, Some(view_a), false, ctx);
            assert!(!is_unread(view_a, ctx));
            assert!(is_unread(view_b, ctx), "a pane's own row clears that pane");
        });
    });
}

#[test]
fn set_tab_unread_ignores_a_closed_tab_and_another_tabs_pane() {
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            workspace.add_terminal_tab(false, ctx);
            let first = focused_terminal_view_id(workspace, 0, ctx);
            let second = focused_terminal_view_id(workspace, 1, ctx);
            let first_tab = workspace.tabs[0].pane_group.id();

            workspace.set_tab_unread(EntityId::new(), None, true, ctx);
            workspace.set_tab_unread(first_tab, Some(second), true, ctx);
            assert!(!is_unread(first, ctx));
            assert!(!is_unread(second, ctx));
        });
    });
}

#[test]
fn toggle_marks_the_active_tab_and_then_clears_it() {
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            let active = focused_terminal_view_id(workspace, workspace.active_tab_index, ctx);
            workspace.toggle_active_tab_unread(ctx);
            assert!(is_unread(active, ctx));
            workspace.toggle_active_tab_unread(ctx);
            assert!(!is_unread(active, ctx));
        });
    });
}

#[test]
fn the_unread_item_is_hidden_on_a_tab_with_no_terminal_pane() {
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            let pane_group = workspace.tabs[0].pane_group.clone();
            let terminal = pane_group.as_ref(ctx).focused_pane_id(ctx);
            pane_group.update(ctx, |pane_group, ctx| {
                let notebook = ctx.add_typed_action_view(NotebookView::new);
                pane_group.add_pane_with_direction(
                    Direction::Left,
                    NotebookPane::new(notebook, ctx),
                    true,
                    ctx,
                );
            });
            assert!(unread_item(workspace, 0, None, ctx).is_some());

            pane_group.update(ctx, |pane_group, ctx| pane_group.close_pane(terminal, ctx));
            assert_eq!(unread_item(workspace, 0, None, ctx), None);
        });
    });
}

#[test]
fn a_pane_closed_for_good_loses_its_mark_but_a_hidden_or_moved_one_keeps_it() {
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            let (a, b) = split_into_two_terminals(workspace, 0, ctx);
            let pane_group = workspace.tabs[0].pane_group.clone();
            let view_a = terminal_view_id(pane_group.as_ref(ctx), a, ctx);
            let view_b = terminal_view_id(pane_group.as_ref(ctx), b, ctx);
            set_marks(&[], &[view_a, view_b], ctx);

            pane_group.update(ctx, |pane_group, ctx| {
                pane_group.remove_pane_for_move(&b, ctx);
            });
            assert!(is_unread(view_b, ctx), "moved");

            pane_group.update(ctx, |pane_group, ctx| pane_group.detach_panes(ctx));
            assert!(
                is_unread(view_a, ctx),
                "hidden while its close can be undone"
            );

            pane_group.update(ctx, |pane_group, ctx| pane_group.clean_up_panes(ctx));
            assert!(!is_unread(view_a, ctx), "closed for good");
            assert!(is_unread(view_b, ctx));
        });
    });
}

/// Persistence test 3, in the workspace: a pane saved showing the dot comes
/// back staged, restore commits it after activating its tab, and the commit
/// makes the active tab's focus the window's baseline, so arriving at the
/// restored pane later clears it.
#[test]
fn a_restored_mark_is_committed_once_restore_activates_its_tab() {
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        let snapshot = workspace.update(&mut app, |workspace, ctx| {
            workspace.add_terminal_tab(false, ctx);
            let marked = focused_terminal_view_id(workspace, 1, ctx);
            set_marks(&[], &[marked], ctx);
            workspace.activate_tab(0, ctx);
            workspace.snapshot(ctx.window_id(), false, ctx)
        });
        let saved: Vec<bool> = snapshot
            .tabs
            .iter()
            .flat_map(|tab| tab.root.terminal_leaves())
            .map(|terminal| terminal.marked_unread)
            .collect();
        assert_eq!(saved, vec![false, true]);

        let restored = restore(&mut app, snapshot);
        restored.update(&mut app, |workspace, ctx| {
            assert_eq!(workspace.active_tab_index, 0);
            let restored_view = focused_terminal_view_id(workspace, 1, ctx);
            assert!(is_unread(restored_view, ctx));
            assert!(!is_unread(focused_terminal_view_id(workspace, 0, ctx), ctx));
            assert!(!AgentNotificationsModel::as_ref(ctx).has_staged_marks());

            let window_id = ctx.window_id();
            AgentNotificationsModel::handle(ctx).update(ctx, |model, ctx| {
                let dwell = model
                    .record_terminal_focus(window_id, Some(restored_view), true, ctx)
                    .expect("an arrival");
                model.finish_dwell(window_id, dwell, Some(restored_view), ctx);
            });
            assert!(
                !is_unread(restored_view, ctx),
                "an arrival clears it once focus stays for the dwell"
            );
        });
    });
}

/// Every staged mark is committed: by restore itself, by the window's first
/// input, or by the fallback shortly after restore.
#[test]
fn every_staged_mark_is_eventually_committed() {
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        let snapshot = workspace.update(&mut app, |workspace, ctx| {
            workspace.add_terminal_tab(false, ctx);
            workspace.snapshot(ctx.window_id(), false, ctx)
        });
        let restored = restore(&mut app, snapshot);
        let (first, second) = restored.read(&app, |workspace, ctx| {
            (
                focused_terminal_view_id(workspace, 0, ctx),
                focused_terminal_view_id(workspace, 1, ctx),
            )
        });
        let stage = |app: &mut App, view: EntityId| {
            AgentNotificationsModel::handle(app).update(app, |model, ctx| {
                model.stage_restored_unread(view, ctx);
            });
        };
        let staged = |app: &App| {
            AgentNotificationsModel::handle(app).read(app, |model, _| model.has_staged_marks())
        };

        // Staged after restore's commit, then the window takes input.
        stage(&mut app, first);
        assert!(staged(&app));
        restored.update(&mut app, |workspace, ctx| {
            workspace.self_or_child_interacted_with(ctx);
        });
        assert!(!staged(&app), "the first input commits it");

        // Staged with no input to follow: the fallback commits it.
        stage(&mut app, second);
        Timer::after(STAGED_UNREAD_FALLBACK_COMMIT + Duration::from_millis(500)).await;
        assert!(!staged(&app), "the fallback commits it");
        restored.read(&app, |_, ctx| {
            assert!(is_unread(first, ctx));
            assert!(is_unread(second, ctx));
        });
    });
}

/// Focus arriving at a pane reads its notifications at once, as upstream's
/// does, with TabMarkUnread off. With it on, they're read only once focus has
/// stayed on the pane for the dwell, so passing through leaves them unread.
#[test]
fn an_arrival_reads_notifications_at_once_as_upstream_or_after_the_dwell() {
    let _mailbox = FeatureFlag::HOANotifications.override_enabled(true);
    for tab_mark_unread in [false, true] {
        let _unread = FeatureFlag::TabMarkUnread.override_enabled(tab_mark_unread);
        App::test((), |mut app| async move {
            initialize_app(&mut app);
            let workspace = mock_workspace(&mut app);
            let second = workspace.update(&mut app, |workspace, ctx| {
                workspace.add_terminal_tab(false, ctx);
                focused_terminal_view_id(workspace, 1, ctx)
            });
            let window_id = app.read(|ctx| workspace.window_id(ctx));
            let _active = ActiveWindowForTests::set(window_id);
            let notify = |app: &mut App| {
                AgentNotificationsModel::handle(app).update(app, |model, ctx| {
                    model.add_notification_for_tests(second, ctx);
                });
            };
            let context = format!("TabMarkUnread {tab_mark_unread}");

            // The window's first report in front is its baseline.
            workspace.update(&mut app, |workspace, ctx| {
                workspace.activate_tab(1, ctx);
                workspace.activate_tab(0, ctx);
            });

            // Focus passes through the second tab.
            notify(&mut app);
            workspace.update(&mut app, |workspace, ctx| {
                workspace.activate_tab(1, ctx);
                assert_eq!(is_unread(second, ctx), tab_mark_unread, "on arrival, {context}");
                workspace.activate_tab(0, ctx);
            });
            Timer::after(ARRIVAL_DWELL + Duration::from_millis(500)).await;
            workspace.read(&app, |_, ctx| {
                assert_eq!(
                    is_unread(second, ctx),
                    tab_mark_unread,
                    "after passing through, {context}"
                );
            });

            // Focus stays on it.
            notify(&mut app);
            workspace.update(&mut app, |workspace, ctx| workspace.activate_tab(1, ctx));
            Timer::after(ARRIVAL_DWELL + Duration::from_millis(500)).await;
            workspace.read(&app, |_, ctx| {
                assert!(!is_unread(second, ctx), "after the dwell, {context}");
            });
        });
    }
}
