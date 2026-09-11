use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use pathfinder_color::ColorU;
use settings::Setting as _;
use warp_errors::report_if_error;
use warpui::platform::WindowStyle;
use warpui::r#async::Timer;
use warpui::{
    App, AppContext, EntityId, SingletonEntity as _, TypedActionView as _, View as _, ViewContext,
    ViewHandle,
};

use super::{row_shows_unread, tab_is_unread, ARRIVAL_DWELL, STAGED_UNREAD_FALLBACK_COMMIT};
use crate::ai::agent_management::{ActiveWindowForTests, AgentNotificationsModel};
use crate::app_state::WindowSnapshot;
use crate::features::FeatureFlag;
use crate::menu::MenuItem;
use crate::notebooks::notebook::NotebookView;
use crate::pane_group::{Direction, NotebookPane, PaneGroup, PaneId};
use crate::root_view::NewWorkspaceSource;
use crate::tab::PaneNameMenuTarget;
use crate::util::bindings::keybinding_name_to_display_string;
use crate::workspace::tab_settings::{TabSettings, VerticalTabsDisplayGranularity};
use crate::workspace::view::tests::{initialize_app, mock_workspace};
use crate::workspace::view::TOGGLE_ACTIVE_TAB_UNREAD_BINDING_NAME;
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
        tab_index == workspace.active_tab_index,
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

/// The tab menu's unread item: what choosing it does, and its key hint.
fn unread_item_action_and_hint(
    workspace: &Workspace,
    tab_index: usize,
    target: Option<PaneNameMenuTarget>,
    app: &AppContext,
) -> (WorkspaceAction, Option<String>) {
    menu_items(workspace, tab_index, target, app)
        .iter()
        .filter_map(MenuItem::fields)
        .find_map(|fields| match fields.on_select_action() {
            Some(action @ WorkspaceAction::SetTabUnread { .. }) => Some((
                action.clone(),
                fields.key_shortcut_label().map(str::to_owned),
            )),
            _ => None,
        })
        .expect("the tab menu has an unread item")
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

/// ⌃⌘U's hint shows beside Mark as Unread or Mark as Read exactly where
/// pressing the key leaves the same marks as choosing the item. Swept over
/// every layout × which tab is active × which of a split tab's terminals has
/// focus × which terminals are unread × each of the split tab's menus (the
/// whole tab, a right-click around its rows, and each row). That comes to the
/// active tab's whole-tab items, and a pane's own item only for the pane the
/// key acts on.
#[test]
fn the_unread_hint_shows_exactly_where_the_key_does_what_the_item_does() {
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);
    let _vertical_tabs = FeatureFlag::VerticalTabs.override_enabled(true);
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        // Tab 0 holds terminals a and b; tab 1 holds one more.
        let (pane_group_id, a, b) = workspace.update(&mut app, |workspace, ctx| {
            let (a, b) = split_into_two_terminals(workspace, 0, ctx);
            workspace.add_terminal_tab(false, ctx);
            (workspace.tabs[0].pane_group.id(), a, b)
        });

        let (mut cases, mut hinted) = (0, 0);
        for (vertical, granularity) in [
            (false, VerticalTabsDisplayGranularity::Tabs),
            (true, VerticalTabsDisplayGranularity::Tabs),
            (true, VerticalTabsDisplayGranularity::Panes),
        ] {
            set_tab_layout(&mut app, vertical, granularity);
            workspace.update(&mut app, |workspace, ctx| {
                let hint =
                    keybinding_name_to_display_string(TOGGLE_ACTIVE_TAB_UNREAD_BINDING_NAME, ctx);
                let pane_group = workspace.tabs[0].pane_group.clone();
                let views = [
                    terminal_view_id(pane_group.as_ref(ctx), a, ctx),
                    terminal_view_id(pane_group.as_ref(ctx), b, ctx),
                    focused_terminal_view_id(workspace, 1, ctx),
                ];
                for active in [0, 1] {
                    for focused in [a, b] {
                        for unread_set in 0..8 {
                            let unread: Vec<EntityId> = views
                                .iter()
                                .enumerate()
                                .filter(|(index, _)| unread_set & (1 << index) != 0)
                                .map(|(_, view)| *view)
                                .collect();
                            let reset =
                                |workspace: &mut Workspace, ctx: &mut ViewContext<Workspace>| {
                                    workspace.activate_tab(active, ctx);
                                    pane_group.update(ctx, |pane_group, ctx| {
                                        pane_group.focus_pane_by_id(focused, ctx);
                                    });
                                    set_marks(&views, &unread, ctx);
                                };
                            let rows = match (vertical, granularity) {
                                (false, _) => vec![],
                                (true, VerticalTabsDisplayGranularity::Tabs) => vec![focused],
                                (true, VerticalTabsDisplayGranularity::Panes) => vec![a, b],
                            };
                            let targets = [None, Some(tab_target(pane_group_id, focused))]
                                .into_iter()
                                .chain(
                                    rows.into_iter()
                                        .map(|row| Some(row_target(pane_group_id, row))),
                                );
                            for target in targets {
                                let case = format!(
                                    "vertical {vertical}, {granularity:?}, tab {active} active, \
                                     focused {focused:?}, unread set {unread_set}, \
                                     row {:?}",
                                    target
                                        .filter(|target| target.is_pane_row)
                                        .map(|target| target.locator.pane_id)
                                );
                                reset(workspace, ctx);
                                let (action, shown_hint) =
                                    unread_item_action_and_hint(workspace, 0, target, ctx);

                                workspace
                                    .handle_action(&WorkspaceAction::ToggleActiveTabUnread, ctx);
                                let after_key = views.map(|view| is_unread(view, ctx));
                                reset(workspace, ctx);
                                workspace.handle_action(&action, ctx);
                                let after_item = views.map(|view| is_unread(view, ctx));

                                let same = after_key == after_item;
                                assert_eq!(
                                    shown_hint,
                                    hint.clone().filter(|_| same),
                                    "{case}: the key leaves {after_key:?}, the item {after_item:?}"
                                );
                                cases += 1;
                                hinted += usize::from(same);
                            }
                        }
                    }
                }
            });
        }
        // Per active tab, focus and unread set: two whole-tab items
        // horizontally, three vertically in Tabs, four in Panes.
        assert_eq!(cases, 2 * 2 * 8 * (2 + 3 + 4));
        assert!(0 < hinted && hinted < cases, "{hinted} of {cases} hinted");
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
                assert_eq!(
                    is_unread(second, ctx),
                    tab_mark_unread,
                    "on arrival, {context}"
                );
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

/// A dot on the selected row takes the title's ink, and every other dot keeps
/// the accent. With TabMarkUnread off, every dot keeps the accent.
#[test]
fn the_dot_takes_the_title_ink_on_a_selected_row_and_the_accent_elsewhere() {
    use warp_core::ui::theme::Fill;

    let accent = Fill::Solid(ColorU::new(0, 194, 255, 255));
    let title_ink = Fill::Solid(ColorU::new(240, 240, 240, 230));
    for tab_mark_unread in [false, true] {
        let _unread = FeatureFlag::TabMarkUnread.override_enabled(tab_mark_unread);
        for on_selected_row in [false, true] {
            let expected = if tab_mark_unread && on_selected_row {
                title_ink
            } else {
                accent
            };
            assert_eq!(
                super::unread_dot_ink(accent, title_ink, on_selected_row).into_solid(),
                expected.into_solid(),
                "TabMarkUnread {tab_mark_unread}, selected {on_selected_row}"
            );
        }
    }
}

/// The inks of the unread dots painted inside each tab's row or card, in tab
/// order, from one paint of the workspace's whole window.
fn painted_dot_inks(app: &mut App, workspace: &ViewHandle<Workspace>) -> Vec<Vec<ColorU>> {
    use pathfinder_geometry::vector::{vec2f, vec2i};
    use warpui::assets::asset_cache::{AssetCache, AssetSource, AssetState};
    use warpui::image_cache::{AnimatedImageBehavior, CacheOption, FitType, Image, ImageCache};
    use warpui::{Presenter, WindowInvalidation};

    use crate::tab::tab_position_id;

    let (window_id, tab_count) = workspace.read(app, |workspace, _| {
        (workspace.window_id, workspace.tabs.len())
    });
    app.update(|ctx| {
        let mut presenter = Presenter::new(window_id);
        // The first frame asks for each icon; the second paints any that were
        // still loading during the first.
        let mut scene = None;
        for _ in 0..2 {
            let invalidation = WindowInvalidation {
                updated: ctx.view_ids_for_window(window_id).into_iter().collect(),
                ..Default::default()
            };
            presenter.invalidate(invalidation, ctx);
            scene = Some(presenter.build_scene(vec2f(1280., 800.), 1., None, ctx));
        }
        let scene = scene.expect("the window was painted");
        let AssetState::Loaded { data } = ImageCache::as_ref(ctx).image(
            AssetSource::Bundled {
                path: "bundled/svg/circle-filled.svg",
            },
            vec2i(8, 8),
            FitType::Contain,
            AnimatedImageBehavior::FullAnimation,
            CacheOption::BySize,
            None,
            AssetCache::as_ref(ctx),
        ) else {
            panic!("the dot should load from the bundled assets");
        };
        let Image::Static(dot) = data.as_ref() else {
            panic!("the dot should be a still image");
        };
        let positions = presenter.position_cache();
        (0..tab_count)
            .map(|index| {
                let row = positions
                    .get_position(tab_position_id(index))
                    .unwrap_or_else(|| panic!("tab {index}'s row should be painted"));
                scene
                    .layers()
                    .flat_map(|layer| &layer.icons)
                    .filter(|icon| Arc::ptr_eq(&icon.asset, dot) && row.contains_rect(icon.bounds))
                    .map(|icon| icon.color)
                    .collect()
            })
            .collect()
    })
}

/// Painted in every vertical layout, with the active tab split and its
/// unfocused pane unread, and a background tab unread. In the Tabs layouts the
/// active tab's one row is drawn selected and wears the dot in the title's
/// ink; in the Panes layouts that dot sits on the unfocused pane's own row,
/// which isn't selected, so it keeps the accent, as the background tab's does.
/// With TabMarkUnread off, every dot keeps the accent.
#[test]
fn a_dot_on_the_selected_row_is_painted_in_the_title_ink() {
    use crate::appearance::Appearance;
    use crate::workspace::tab_settings::{VerticalTabsTabItemMode, VerticalTabsViewMode};

    let _vertical_tabs = FeatureFlag::VerticalTabs.override_enabled(true);
    let _summary = FeatureFlag::VerticalTabsSummaryMode.override_enabled(true);
    for tab_mark_unread in [false, true] {
        let _unread = FeatureFlag::TabMarkUnread.override_enabled(tab_mark_unread);
        App::test(crate::ASSETS, |mut app| async move {
            initialize_app(&mut app);
            let workspace = mock_workspace(&mut app);
            workspace.update(&mut app, |workspace, ctx| {
                while workspace.tab_count() < 3 {
                    workspace.add_terminal_tab(false, ctx);
                }
                workspace.activate_tab(0, ctx);
                let (first, second) = split_into_two_terminals(workspace, 0, ctx);
                let pane_group = workspace.tabs[0].pane_group.as_ref(ctx);
                let unfocused = if pane_group.focused_pane_id(ctx) == first {
                    second
                } else {
                    first
                };
                let unfocused = terminal_view_id(pane_group, unfocused, ctx);
                let background = focused_terminal_view_id(workspace, 1, ctx);
                set_marks(&[], &[unfocused, background], ctx);
            });
            let (title_ink, accent) = app.update(|ctx| {
                let theme = Appearance::as_ref(ctx).theme();
                (
                    theme.main_text_color(theme.background()).into_solid(),
                    theme.accent().into_solid(),
                )
            });
            assert_ne!(title_ink, accent);

            for (name, granularity, view_mode, item_mode) in [
                (
                    "compact tab rows",
                    VerticalTabsDisplayGranularity::Tabs,
                    VerticalTabsViewMode::Compact,
                    VerticalTabsTabItemMode::FocusedSession,
                ),
                (
                    "expanded tab rows",
                    VerticalTabsDisplayGranularity::Tabs,
                    VerticalTabsViewMode::Expanded,
                    VerticalTabsTabItemMode::FocusedSession,
                ),
                (
                    "summary cards",
                    VerticalTabsDisplayGranularity::Tabs,
                    VerticalTabsViewMode::Compact,
                    VerticalTabsTabItemMode::Summary,
                ),
                (
                    "compact pane rows",
                    VerticalTabsDisplayGranularity::Panes,
                    VerticalTabsViewMode::Compact,
                    VerticalTabsTabItemMode::FocusedSession,
                ),
                (
                    "expanded pane rows",
                    VerticalTabsDisplayGranularity::Panes,
                    VerticalTabsViewMode::Expanded,
                    VerticalTabsTabItemMode::FocusedSession,
                ),
            ] {
                TabSettings::handle(&app).update(&mut app, |settings, ctx| {
                    report_if_error!(settings.use_vertical_tabs.set_value(true, ctx));
                    report_if_error!(settings
                        .vertical_tabs_display_granularity
                        .set_value(granularity, ctx));
                    report_if_error!(settings.vertical_tabs_view_mode.set_value(view_mode, ctx));
                    report_if_error!(settings
                        .vertical_tabs_tab_item_mode
                        .set_value(item_mode, ctx));
                });
                workspace.update(&mut app, |workspace, _| {
                    workspace.vertical_tabs_panel_open = true;
                });
                let selected_row_shows_the_dot =
                    matches!(granularity, VerticalTabsDisplayGranularity::Tabs);
                let active_tab_dot = if tab_mark_unread && selected_row_shows_the_dot {
                    title_ink
                } else {
                    accent
                };
                assert_eq!(
                    painted_dot_inks(&mut app, &workspace),
                    vec![vec![active_tab_dot], vec![accent], vec![]],
                    "{name}, TabMarkUnread {tab_mark_unread}"
                );
            }
        });
    }
}
