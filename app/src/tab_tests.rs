use std::collections::HashMap;

use settings::Setting as _;
use warp_errors::report_if_error;
use warpui::platform::OperatingSystem;
use warpui::{App, AppContext, SingletonEntity as _};

use super::{bulk_close_label, tab_group_menu_entry_flags, PaneNameMenuTarget};
use crate::features::FeatureFlag;
use crate::menu::MenuItem;
use crate::util::bindings::keybinding_name_to_display_string;
use crate::workspace::tab_group::{TabGroup, TabGroupId};
use crate::workspace::tab_settings::{TabSettings, VerticalTabsDisplayGranularity};
use crate::workspace::view::tests::{initialize_app, mock_workspace};
use crate::workspace::view::TOGGLE_ACTIVE_TAB_STAR_BINDING_NAME;
use crate::workspace::{PaneViewLocator, Workspace, WorkspaceAction};

/// Build a `tab_groups` map containing exactly the given group ids.
fn groups(ids: &[TabGroupId]) -> HashMap<TabGroupId, TabGroup> {
    ids.iter()
        .map(|id| {
            let mut group = TabGroup::new();
            group.id = *id;
            (*id, group)
        })
        .collect()
}

// GH-13073: a tab that is the sole member of its group must NOT be offered
// "New group with tab" (it would just recreate an identical single-tab group);
// it offers "Remove from group" instead.
#[test]
fn sole_member_of_group_hides_new_group_and_offers_remove() {
    let gid = TabGroupId::new();
    let (show_new_group, _show_move_to_group, show_remove_from_group) =
        tab_group_menu_entry_flags(Some(gid), &groups(&[gid]), /* is_only_member */ true);

    assert!(
        !show_new_group,
        "the sole member of a group should not offer 'New group with tab'"
    );
    assert!(
        show_remove_from_group,
        "a tab in a group should offer 'Remove from group'"
    );
}

// GH-13073 follow-up: a tab that shares a group with siblings SHOULD still be
// offered "New group with tab" so it can be pulled out into its own new group
// (à la Chrome), and it offers "Remove from group" as well.
#[test]
fn grouped_tab_with_siblings_offers_new_group_and_remove() {
    let gid = TabGroupId::new();
    let (show_new_group, _show_move_to_group, show_remove_from_group) =
        tab_group_menu_entry_flags(Some(gid), &groups(&[gid]), /* is_only_member */ false);

    assert!(
        show_new_group,
        "a grouped tab with siblings should still offer 'New group with tab'"
    );
    assert!(
        show_remove_from_group,
        "a grouped tab should offer 'Remove from group'"
    );
}

// An ungrouped tab always offers "New group with tab" and never offers
// "Remove from group". `is_only_member` is irrelevant when ungrouped.
#[test]
fn ungrouped_tab_offers_new_group_and_hides_remove() {
    let (show_new_group, _show_move_to_group, show_remove_from_group) =
        tab_group_menu_entry_flags(None, &HashMap::new(), /* is_only_member */ false);

    assert!(
        show_new_group,
        "an ungrouped tab should offer 'New group with tab'"
    );
    assert!(
        !show_remove_from_group,
        "an ungrouped tab should not offer 'Remove from group'"
    );
}

// "Move to group" should only appear when a group other than the tab's own
// exists — for both grouped and ungrouped tabs.
#[test]
fn move_to_group_only_shown_when_other_groups_exist() {
    let own = TabGroupId::new();
    let other = TabGroupId::new();

    // Grouped tab whose group is the only one: no other groups to move to.
    let (_n, move_only_own, _r) = tab_group_menu_entry_flags(Some(own), &groups(&[own]), true);
    assert!(!move_only_own);

    // Grouped tab with another group present: offer "Move to group".
    let (_n, move_with_other, _r) =
        tab_group_menu_entry_flags(Some(own), &groups(&[own, other]), true);
    assert!(move_with_other);

    // Ungrouped tab with an existing group: offer "Move to group".
    let (_n, move_ungrouped, _r) = tab_group_menu_entry_flags(None, &groups(&[other]), false);
    assert!(move_ungrouped);
}

/// One string per menu item: its label, `---` for a separator, and
/// `(custom row)` for an item that draws its own label, like the color row.
fn menu_labels(items: &[MenuItem<WorkspaceAction>]) -> Vec<String> {
    let label = |label: &str| {
        if label.is_empty() {
            "(custom row)".to_owned()
        } else {
            label.to_owned()
        }
    };
    items
        .iter()
        .map(|item| match item {
            MenuItem::Item(fields) => label(fields.label()),
            MenuItem::Submenu { fields, .. } => label(fields.label()),
            MenuItem::Header { fields, .. } => label(fields.label()),
            MenuItem::ItemsRow { items } => items
                .iter()
                .map(|fields| label(fields.label()))
                .collect::<Vec<_>>()
                .join(" | "),
            MenuItem::Separator => "---".to_owned(),
        })
        .collect()
}

/// With the tab-mark flags off, the tab menu reads exactly as it did before
/// tab marks existed. The fixture holds the labels captured at 7329f56f, one
/// line per case: `mode|tabs|index|pane target|labels`.
#[test]
fn tab_menu_is_unchanged_with_tab_mark_flags_off() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(false);
    let _stars = FeatureFlag::StarredTabs.override_enabled(false);
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(false);
    // The rest of the menu, as every WarpOSS build has it.
    let _vertical_tabs = FeatureFlag::VerticalTabs.override_enabled(true);
    let _groups = FeatureFlag::GroupedTabs.override_enabled(true);
    let _configs = FeatureFlag::TabConfigs.override_enabled(true);
    let _colors = FeatureFlag::DirectoryTabColors.override_enabled(true);

    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let mut cases = vec![];
        for (mode, vertical, granularity) in [
            ("horizontal", false, VerticalTabsDisplayGranularity::Tabs),
            ("vertical tabs", true, VerticalTabsDisplayGranularity::Tabs),
            (
                "vertical panes",
                true,
                VerticalTabsDisplayGranularity::Panes,
            ),
        ] {
            TabSettings::handle(&app).update(&mut app, |settings, ctx| {
                report_if_error!(settings.use_vertical_tabs.set_value(vertical, ctx));
                report_if_error!(settings
                    .vertical_tabs_display_granularity
                    .set_value(granularity, ctx));
            });
            for tab_count in [1, 2, 8] {
                let workspace = mock_workspace(&mut app);
                workspace.update(&mut app, |workspace, ctx| {
                    while workspace.tab_count() < tab_count {
                        workspace.add_terminal_tab(false, ctx);
                    }
                    let mut indices = vec![0, 3.min(tab_count - 1), tab_count - 1];
                    indices.dedup();
                    for index in indices {
                        let tab = &workspace.tabs[index];
                        let locator = PaneViewLocator {
                            pane_group_id: tab.pane_group.id(),
                            pane_id: tab.pane_group.as_ref(ctx).focused_pane_id(ctx),
                        };
                        for (target, pane_name_target) in [
                            ("none", None),
                            (
                                "active",
                                Some(PaneNameMenuTarget {
                                    locator,
                                    rename_label: "Rename active pane",
                                    reset_label: "Reset active pane name",
                                    is_pane_row: false,
                                }),
                            ),
                            (
                                "clicked",
                                Some(PaneNameMenuTarget {
                                    locator,
                                    rename_label: "Rename pane",
                                    reset_label: "Reset pane name",
                                    is_pane_row: true,
                                }),
                            ),
                        ] {
                            let items = tab.menu_items_with_pane_name_target(
                                index,
                                tab_count,
                                0,
                                &HashMap::new(),
                                false,
                                index > 0,
                                index + 1 < tab_count,
                                pane_name_target,
                                ctx,
                            );
                            cases.push(format!(
                                "{mode}|{tab_count}|{index}|{target}|{}",
                                menu_labels(&items).join(" ; ")
                            ));
                        }
                    }
                });
            }
        }

        let expected: Vec<&str> = include_str!("../test_data/tab_menu_labels_before_tab_marks.txt")
            .lines()
            .collect();
        assert_eq!(cases.len(), expected.len());
        for (case, expected) in cases.iter().zip(expected) {
            assert_eq!(case, expected);
        }
    });
}

/// With the flags on, the menu opens with the tab-marks section: Mark as
/// Unread, then the star item, with no separator between them.
#[test]
fn tab_marks_section_opens_the_menu() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(true);
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);

    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            let opening_items = |workspace: &Workspace, ctx: &AppContext| {
                let items = workspace.tabs[0].menu_items(
                    0,
                    1,
                    0,
                    &HashMap::new(),
                    false,
                    false,
                    false,
                    ctx,
                );
                menu_labels(&items).into_iter().take(3).collect::<Vec<_>>()
            };
            assert_eq!(
                opening_items(workspace, ctx),
                ["Mark as Unread", "Star tab", "---"]
            );

            workspace.tabs[0].group_id = Some(TabGroupId::new());
            assert_eq!(opening_items(workspace, ctx)[1], "Star tab (leaves group)");

            workspace.tabs[0].group_id = None;
            workspace.tabs[0].pinned = true;
            assert_eq!(opening_items(workspace, ctx)[1], "Unstar tab");
        });
    });
}

#[test]
fn bulk_closes_spare_starred_tabs_and_say_so() {
    // Tabs 0 and 1 are starred.
    assert_eq!(
        bulk_close_label("Close other tabs", [1, 2, 3], 2).as_deref(),
        Some("Close other tabs (keep starred)")
    );
    assert_eq!(
        bulk_close_label("Close Tabs Below", 1..4, 2).as_deref(),
        Some("Close Tabs Below (keep starred)")
    );
    assert_eq!(
        bulk_close_label("Close Tabs Below", 2..4, 2).as_deref(),
        Some("Close Tabs Below")
    );
    // Only starred tabs in range, so the close would do nothing: hidden.
    assert_eq!(bulk_close_label("Close other tabs", [1], 2), None);
    assert_eq!(bulk_close_label("Close Tabs Below", 4..4, 2), None);
}

/// Every tab's menu in every list of up to eight tabs, with every length of
/// starred block, in both tab bars: each bulk close shows exactly when it would
/// close a tab, and says "(keep starred)" exactly when it would spare one.
#[test]
fn bulk_close_items_across_every_small_tab_list() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(true);
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(false);
    let _vertical_tabs = FeatureFlag::VerticalTabs.override_enabled(true);

    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        let mut menus = 0;
        for vertical in [false, true] {
            TabSettings::handle(&app).update(&mut app, |settings, ctx| {
                report_if_error!(settings.use_vertical_tabs.set_value(vertical, ctx));
            });
            let close_after = if vertical {
                "Close Tabs Below"
            } else {
                "Close Tabs to the Right"
            };
            workspace.read(&app, |workspace, ctx| {
                for tab_count in 1..=8usize {
                    for starred in 0..=tab_count {
                        for index in 0..tab_count {
                            let items = workspace.tabs[0].menu_items(
                                index,
                                tab_count,
                                starred,
                                &HashMap::new(),
                                false,
                                index > 0,
                                index + 1 < tab_count,
                                ctx,
                            );
                            let labels = menu_labels(&items);
                            let others: Vec<usize> =
                                (0..tab_count).filter(|other| *other != index).collect();
                            let after: Vec<usize> = (index + 1..tab_count).collect();
                            for (label, range) in
                                [("Close other tabs", others), (close_after, after)]
                            {
                                let closes = range.iter().any(|tab| *tab >= starred);
                                let spares = range.iter().any(|tab| *tab < starred);
                                let expected: Vec<String> = closes
                                    .then(|| {
                                        if spares {
                                            format!("{label} (keep starred)")
                                        } else {
                                            label.to_owned()
                                        }
                                    })
                                    .into_iter()
                                    .collect();
                                let shown: Vec<String> = labels
                                    .iter()
                                    .filter(|shown| shown.starts_with(label))
                                    .cloned()
                                    .collect();
                                assert_eq!(
                                    shown, expected,
                                    "tab {index} of {tab_count}, {starred} starred, vertical \
                                     {vertical}"
                                );
                            }
                            menus += 1;
                        }
                    }
                }
            });
        }
        assert_eq!(menus, 480);
    });
}

/// The star item reads "Star tab", "Star tab (leaves group)" or "Unstar tab"
/// from the tab's own state, with the star key as the keymap has it for a hint.
#[test]
fn the_star_item_follows_the_tab_and_hints_the_live_key() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(true);
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(false);

    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            let hint = keybinding_name_to_display_string(TOGGLE_ACTIVE_TAB_STAR_BINDING_NAME, ctx);
            assert_eq!(
                hint.is_some(),
                OperatingSystem::get().is_mac(),
                "Ctrl-Cmd-S is a macOS default only"
            );
            for (pinned, grouped, label) in [
                (false, false, "Star tab"),
                (false, true, "Star tab (leaves group)"),
                (true, false, "Unstar tab"),
            ] {
                workspace.tabs[0].pinned = pinned;
                workspace.tabs[0].group_id = grouped.then(TabGroupId::new);
                let items = workspace.tabs[0].menu_items(
                    0,
                    1,
                    usize::from(pinned),
                    &HashMap::new(),
                    false,
                    false,
                    false,
                    ctx,
                );
                let MenuItem::Item(fields) = &items[0] else {
                    panic!("the menu opens with the star item");
                };
                assert_eq!(fields.label(), label);
                assert_eq!(fields.key_shortcut_label(), hint.as_deref(), "{label}");
            }
        });
    });
}
