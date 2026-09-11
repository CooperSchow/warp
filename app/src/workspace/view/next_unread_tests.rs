use std::collections::HashSet;
use std::time::Duration;

use settings::Setting as _;
use warp_errors::report_if_error;
use warpui::r#async::Timer;
use warpui::{App, AppContext, EntityId, SingletonEntity as _, TypedActionView as _, ViewContext};

use super::{next_unread_tab, nowhere_to_jump_message};
use crate::ai::agent_management::{ActiveWindowForTests, AgentNotificationsModel};
use crate::features::FeatureFlag;
use crate::pane_group::{Direction, PaneId};
use crate::workspace::tab_settings::{TabSettings, VerticalTabsDisplayGranularity};
use crate::workspace::view::tab_unread::{row_shows_unread, ARRIVAL_DWELL};
use crate::workspace::view::tests::{initialize_app, mock_workspace};
use crate::workspace::{Workspace, WorkspaceAction};

fn set_tab_layout(app: &mut App, vertical: bool, granularity: VerticalTabsDisplayGranularity) {
    TabSettings::handle(app).update(app, |settings, ctx| {
        report_if_error!(settings.use_vertical_tabs.set_value(vertical, ctx));
        report_if_error!(settings
            .vertical_tabs_display_granularity
            .set_value(granularity, ctx));
    });
}

fn set_marks(read: &[EntityId], unread: &[EntityId], ctx: &mut ViewContext<Workspace>) {
    AgentNotificationsModel::handle(ctx).update(ctx, |model, ctx| {
        model.mark_read(read, ctx);
        model.mark_unread(unread, ctx);
    });
}

/// A search record in which each tab's one row stands for the whole tab.
fn whole_tabs(pane_group_ids: &[EntityId]) -> Vec<(EntityId, Option<Vec<PaneId>>)> {
    pane_group_ids.iter().map(|id| (*id, None)).collect()
}

/// Adds a terminal pane beside the tab's one, keeps focus on the first, and
/// returns both panes with their terminal views.
fn split_tab(
    workspace: &Workspace,
    tab_index: usize,
    ctx: &mut ViewContext<Workspace>,
) -> [(PaneId, EntityId); 2] {
    let pane_group = workspace.tabs[tab_index].pane_group.clone();
    let (first, second) = pane_group.update(ctx, |pane_group, ctx| {
        let first = pane_group.focused_pane_id(ctx);
        let second: PaneId = pane_group
            .add_terminal_pane(Direction::Right, None, ctx)
            .into();
        pane_group.focus_pane_by_id(first, ctx);
        (first, second)
    });
    [first, second].map(|pane_id| {
        let terminal_view = pane_group
            .as_ref(ctx)
            .terminal_view_from_pane_id(pane_id, ctx)
            .expect("both panes are terminals");
        (pane_id, terminal_view.id())
    })
}

/// Every visible list drawn from up to seven tabs × every unread set × every
/// starred-prefix length × every active tab.
#[test]
fn next_unread_tab_sweeps_every_small_tab_list() {
    let mut cases = 0u64;
    for tab_count in 0..=7usize {
        for shown in 0u32..(1 << tab_count) {
            let visible: Vec<usize> = (0..tab_count)
                .filter(|index| shown & (1 << index) != 0)
                .collect();
            for unread_set in 0u32..(1 << tab_count) {
                let unread = |index: usize| unread_set & (1 << index) != 0;
                for starred in 0..=tab_count {
                    for active in 0..tab_count.max(1) {
                        cases += 1;
                        let case = format!(
                            "visible {visible:?}, unread {unread_set:#b}, \
                             {starred} starred, active {active}"
                        );
                        let others: Vec<usize> = visible
                            .iter()
                            .copied()
                            .filter(|&index| index != active && unread(index))
                            .collect();

                        let next = next_unread_tab(&visible, active, unread);
                        // None exactly when no other visible tab is unread,
                        // and otherwise the first of them below the active
                        // tab, or the topmost when none is below it.
                        let below = others.iter().copied().find(|&index| index > active);
                        assert_eq!(next, below.or(others.first().copied()), "{case}");
                        if let Some(next) = next {
                            assert_ne!(next, active, "{case}");
                            assert!(unread(next), "{case}");
                            // Going down from the active tab and wrapping to
                            // the top, no visible unread tab is passed over.
                            let passed_over = |index: usize| {
                                if next > active {
                                    index > active && index < next
                                } else {
                                    index > active || index < next
                                }
                            };
                            assert!(
                                visible
                                    .iter()
                                    .all(|&index| !passed_over(index) || !unread(index)),
                                "no visible unread tab is passed over: {case}"
                            );
                            if below.is_none() && others.iter().any(|&index| index < starred) {
                                assert!(
                                    next < starred,
                                    "wrapping to the top, a starred tab comes first: {case}"
                                );
                            }
                        }

                        // Walking with ⌘J, where each arrival reads the tab,
                        // visits every unread tab once. The starting tab is
                        // one of them once there's somewhere else to go.
                        let mut still_unread = unread_set;
                        let mut at = active;
                        let mut visited = vec![];
                        while let Some(next) =
                            next_unread_tab(&visible, at, |index| still_unread & (1 << index) != 0)
                        {
                            assert!(visited.len() < tab_count, "the walk ends: {case}");
                            still_unread &= !(1 << next);
                            visited.push(next);
                            at = next;
                        }
                        let expected: HashSet<usize> = if others.is_empty() {
                            HashSet::new()
                        } else {
                            visible
                                .iter()
                                .copied()
                                .filter(|&index| unread(index))
                                .collect()
                        };
                        assert_eq!(visited.len(), expected.len(), "{case}: {visited:?}");
                        assert_eq!(
                            visited.iter().copied().collect::<HashSet<_>>(),
                            expected,
                            "{case}: {visited:?}"
                        );
                        // In list order: down from the starting tab, then
                        // from the top, and the starting tab last.
                        if !others.is_empty() {
                            let mut order: Vec<usize> = others
                                .iter()
                                .copied()
                                .filter(|&index| index > active)
                                .collect();
                            order.extend(others.iter().copied().filter(|&index| index < active));
                            if visible.contains(&active) && unread(active) {
                                order.push(active);
                            }
                            assert_eq!(visited, order, "the walk's order: {case}");
                        }
                    }
                }
            }
        }
    }
    // The sum over tab counts n of 4^n × (n + 1) × max(n, 1).
    assert_eq!(cases, 1_126_249);
}

#[test]
fn the_toast_says_why_there_is_nowhere_to_jump() {
    assert_eq!(
        nowhere_to_jump_message(false, false),
        "You're all caught up"
    );
    assert_eq!(nowhere_to_jump_message(true, false), "No other unread tabs");
    assert_eq!(
        nowhere_to_jump_message(false, true),
        "No unread tabs match this search"
    );
    assert_eq!(
        nowhere_to_jump_message(true, true),
        "No unread tabs match this search"
    );
}

/// Three tabs, the middle one split with its unread terminal in the pane that
/// doesn't have focus: ⌘J activates that tab and focuses that pane.
#[test]
fn cmd_j_activates_the_unread_tab_and_focuses_its_unread_pane() {
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            workspace.add_terminal_tab(false, ctx);
            workspace.add_terminal_tab(false, ctx);
            let [(focused, _), (unread_pane, unread_view)] = split_tab(workspace, 1, ctx);
            set_marks(&[], &[unread_view], ctx);
            workspace.activate_tab(0, ctx);
            let middle = workspace.tabs[1].pane_group.clone();
            assert_eq!(middle.as_ref(ctx).focused_pane_id(ctx), focused);

            workspace.handle_action(&WorkspaceAction::JumpToNextUnreadTab, ctx);

            assert_eq!(workspace.active_tab_index, 1);
            assert_eq!(middle.as_ref(ctx).focused_pane_id(ctx), unread_pane);
        });
    });
}

#[test]
fn cmd_j_stays_put_and_says_so_when_no_other_tab_is_unread() {
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            workspace.add_terminal_tab(false, ctx);
            workspace.activate_tab(0, ctx);
            assert_eq!(
                workspace.next_unread_target(ctx),
                Err("You're all caught up")
            );

            let active_pane_group = workspace.tabs[0].pane_group.id();
            workspace.set_tab_unread(active_pane_group, None, true, ctx);
            assert_eq!(
                workspace.next_unread_target(ctx),
                Err("No other unread tabs")
            );

            workspace.jump_to_next_unread_tab(ctx);
            assert_eq!(workspace.active_tab_index, 0);
            assert!(workspace.toast_stack.as_ref(ctx).has_toasts());
        });
    });
}

#[test]
fn cmd_j_keeps_to_the_tabs_the_search_shows() {
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);
    let _vertical_tabs = FeatureFlag::VerticalTabs.override_enabled(true);
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        set_tab_layout(&mut app, true, VerticalTabsDisplayGranularity::Tabs);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            workspace.add_terminal_tab(false, ctx);
            workspace.add_terminal_tab(false, ctx);
            workspace.activate_tab(0, ctx);
            let tabs: Vec<EntityId> = workspace
                .tabs
                .iter()
                .map(|tab| tab.pane_group.id())
                .collect();
            workspace.set_tab_unread(tabs[2], None, true, ctx);

            workspace.vertical_tabs_panel_open = true;
            workspace.vertical_tabs_panel.search_query = "build".to_owned();
            // The panel's last render for this search showed the first two tabs.
            workspace
                .vertical_tabs_panel
                .record_search_matches("build", whole_tabs(&tabs[..2]));
            assert_eq!(
                workspace.next_unread_target(ctx),
                Err("No unread tabs match this search")
            );
            workspace.jump_to_next_unread_tab(ctx);
            assert_eq!(workspace.active_tab_index, 0);

            // A render that shows the unread tab too.
            workspace
                .vertical_tabs_panel
                .record_search_matches("build", whole_tabs(&tabs));
            assert_eq!(workspace.next_unread_target(ctx), Ok(2));

            // A render for a query since changed doesn't filter anything.
            workspace
                .vertical_tabs_panel
                .record_search_matches("bui", whole_tabs(&tabs[..1]));
            assert_eq!(workspace.next_unread_target(ctx), Ok(2));

            // Nor does a search in a panel that isn't showing.
            workspace
                .vertical_tabs_panel
                .record_search_matches("build", whole_tabs(&tabs[..2]));
            workspace.vertical_tabs_panel_open = false;
            assert_eq!(workspace.next_unread_target(ctx), Ok(2));
        });
    });
}

/// ⌘J goes to a tab exactly when one of the rows the panel shows for it has
/// the dot, and focuses an unread pane, in the Panes layout one whose row the
/// panel shows. Swept over both vertical layouts × which of the tab's two
/// terminals has focus × which of them is unread × no search, or a search
/// matching each subset of them. A Panes-layout search shows a row for each
/// matching pane; in the Tabs layout the tab shows, as one row standing for
/// the whole tab, when its focused pane matches.
#[test]
fn cmd_j_goes_to_exactly_the_tabs_whose_rows_show_the_dot() {
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);
    let _vertical_tabs = FeatureFlag::VerticalTabs.override_enabled(true);
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        // Tab 0 holds two terminals; tab 1, where ⌘J starts, is active.
        let panes = workspace.update(&mut app, |workspace, ctx| {
            let panes = split_tab(workspace, 0, ctx);
            workspace.add_terminal_tab(false, ctx);
            panes
        });

        let mut cases = 0;
        for granularity in [
            VerticalTabsDisplayGranularity::Tabs,
            VerticalTabsDisplayGranularity::Panes,
        ] {
            set_tab_layout(&mut app, true, granularity);
            workspace.update(&mut app, |workspace, ctx| {
                let pane_group = workspace.tabs[0].pane_group.clone();
                let tab_ids = [pane_group.id(), workspace.tabs[1].pane_group.id()];
                let views = panes.map(|(_, view)| view);
                let pane_ids = panes.map(|(pane_id, _)| pane_id);
                let subset = |bits: u32| -> Vec<PaneId> {
                    pane_ids
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| bits & (1 << index) != 0)
                        .map(|(_, pane_id)| *pane_id)
                        .collect()
                };
                for focused in pane_ids {
                    for unread_set in 0..4 {
                        // `None` for no search, or the panes a search matches.
                        for search in std::iter::once(None).chain((0..4).map(Some)) {
                            // ⌘J starts on tab 1, with `focused` focused in tab 0.
                            workspace.activate_tab(1, ctx);
                            pane_group.update(ctx, |pane_group, ctx| {
                                pane_group.focus_pane_by_id(focused, ctx);
                            });
                            let unread: Vec<EntityId> = views
                                .iter()
                                .enumerate()
                                .filter(|(index, _)| unread_set & (1 << index) != 0)
                                .map(|(_, view)| *view)
                                .collect();
                            set_marks(&views, &unread, ctx);

                            // The rows the panel shows for tab 0.
                            let rows: Vec<PaneId> = match (granularity, search) {
                                (VerticalTabsDisplayGranularity::Tabs, None) => vec![focused],
                                (VerticalTabsDisplayGranularity::Panes, None) => pane_ids.to_vec(),
                                (VerticalTabsDisplayGranularity::Tabs, Some(matching)) => {
                                    subset(matching)
                                        .into_iter()
                                        .filter(|pane_id| *pane_id == focused)
                                        .collect()
                                }
                                (VerticalTabsDisplayGranularity::Panes, Some(matching)) => {
                                    subset(matching)
                                }
                            };
                            // What the panel's render records for the search.
                            workspace.vertical_tabs_panel_open = true;
                            let panel = &mut workspace.vertical_tabs_panel;
                            match search {
                                None => panel.search_query.clear(),
                                Some(_) => {
                                    panel.search_query = "x".to_owned();
                                    let mut matches = whole_tabs(&tab_ids[1..]);
                                    if !rows.is_empty() {
                                        let recorded = match granularity {
                                            VerticalTabsDisplayGranularity::Tabs => None,
                                            VerticalTabsDisplayGranularity::Panes => {
                                                Some(rows.clone())
                                            }
                                        };
                                        matches.insert(0, (tab_ids[0], recorded));
                                    }
                                    panel.record_search_matches("x", matches);
                                }
                            }
                            cases += 1;
                            let case = format!(
                                "{granularity:?}, focused {focused:?}, unread set {unread_set}, \
                                 search {search:?}"
                            );

                            let a_row_shows_the_dot = rows.iter().any(|row| {
                                row_shows_unread(pane_group.as_ref(ctx), *row, granularity, ctx)
                            });
                            assert_eq!(
                                workspace.next_unread_target(ctx).ok(),
                                a_row_shows_the_dot.then_some(0),
                                "{case}"
                            );
                            if !a_row_shows_the_dot {
                                continue;
                            }

                            workspace.jump_to_next_unread_tab(ctx);
                            assert_eq!(workspace.active_tab_index, 0, "{case}");
                            let landed = pane_group.as_ref(ctx).focused_pane_id(ctx);
                            let may_land_on: &[PaneId] = match granularity {
                                VerticalTabsDisplayGranularity::Tabs => &pane_ids,
                                VerticalTabsDisplayGranularity::Panes => &rows,
                            };
                            assert!(
                                may_land_on.contains(&landed)
                                    && row_shows_unread(
                                        pane_group.as_ref(ctx),
                                        landed,
                                        VerticalTabsDisplayGranularity::Panes,
                                        ctx
                                    ),
                                "{case}: focused {landed:?}"
                            );
                        }
                    }
                }
            });
        }
        assert_eq!(cases, 2 * 2 * 4 * 5);
    });
}

/// In the Panes layout, a search that shows only a tab's read pane hides the
/// tab from ⌘J, whose toast says the search hides the unread tabs. Once the
/// unread pane's row shows too, ⌘J goes there and focuses that pane.
#[test]
fn cmd_j_skips_a_tab_whose_unread_pane_the_search_hides() {
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);
    let _vertical_tabs = FeatureFlag::VerticalTabs.override_enabled(true);
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        set_tab_layout(&mut app, true, VerticalTabsDisplayGranularity::Panes);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            let [(read, _), (unread, unread_view)] = split_tab(workspace, 0, ctx);
            workspace.add_terminal_tab(false, ctx);
            workspace.activate_tab(1, ctx);
            set_marks(&[], &[unread_view], ctx);
            let tabs = [
                workspace.tabs[0].pane_group.id(),
                workspace.tabs[1].pane_group.id(),
            ];
            workspace.vertical_tabs_panel_open = true;
            workspace.vertical_tabs_panel.search_query = "x".to_owned();

            workspace
                .vertical_tabs_panel
                .record_search_matches("x", vec![(tabs[0], Some(vec![read])), (tabs[1], None)]);
            assert_eq!(
                workspace.next_unread_target(ctx),
                Err("No unread tabs match this search")
            );

            workspace.vertical_tabs_panel.record_search_matches(
                "x",
                vec![(tabs[0], Some(vec![read, unread])), (tabs[1], None)],
            );
            workspace.jump_to_next_unread_tab(ctx);
            assert_eq!(workspace.active_tab_index, 0);
            assert_eq!(
                workspace.tabs[0]
                    .pane_group
                    .as_ref(ctx)
                    .focused_pane_id(ctx),
                unread
            );
        });
    });
}

/// The terminal view in the focused pane of the tab at `tab_index`.
fn focused_terminal_view(workspace: &Workspace, tab_index: usize, app: &AppContext) -> EntityId {
    let pane_group = workspace.tabs[tab_index].pane_group.as_ref(app);
    pane_group
        .terminal_view_from_pane_id(pane_group.focused_pane_id(app), app)
        .expect("the tab's focused pane is a terminal")
        .id()
}

/// Quick ⌘J presses across marked tabs, as holding the key makes, step down the
/// list and wrap to the top, and leave every tab they pass through marked. The
/// tab they stop on clears once focus has stayed on it for the dwell, and no
/// other tab does.
#[test]
fn quick_cmd_j_presses_leave_the_tabs_they_pass_through_marked() {
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        let marked = workspace.update(&mut app, |workspace, ctx| {
            for _ in 0..3 {
                workspace.add_terminal_tab(false, ctx);
            }
            (1..4)
                .map(|index| focused_terminal_view(workspace, index, ctx))
                .collect::<Vec<_>>()
        });
        let window_id = app.read(|ctx| workspace.window_id(ctx));
        let _active = ActiveWindowForTests::set(window_id);
        let is_unread =
            |view: EntityId, app: &AppContext| AgentNotificationsModel::as_ref(app).is_unread(view);

        let stops = workspace.update(&mut app, |workspace, ctx| {
            // The window's first report in front, on tab 0, is its baseline.
            workspace.activate_tab(0, ctx);
            set_marks(&[], &marked, ctx);
            let stops: Vec<usize> = (0..4)
                .map(|_| {
                    workspace.handle_action(&WorkspaceAction::JumpToNextUnreadTab, ctx);
                    workspace.active_tab_index
                })
                .collect();
            assert!(
                marked.iter().all(|view| is_unread(*view, ctx)),
                "no quick press clears a tab: stops {stops:?}"
            );
            assert_eq!(
                stops,
                [1, 2, 3, 1],
                "quick presses step down the list and wrap"
            );
            stops
        });

        Timer::after(ARRIVAL_DWELL + Duration::from_millis(500)).await;
        let last = *stops.last().expect("four presses");
        workspace.read(&app, |_, ctx| {
            for (index, view) in (1..4).zip(&marked) {
                assert_eq!(
                    is_unread(*view, ctx),
                    index != last,
                    "tab {index}: only the tab the presses stopped on clears, stops {stops:?}"
                );
            }
        });
    });
}
