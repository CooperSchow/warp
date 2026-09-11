use std::sync::Arc;

use pathfinder_geometry::rect::RectF;
use pathfinder_geometry::vector::{vec2f, vec2i};
use settings::Setting as _;
use warp_core::ui::theme::color::internal_colors;
use warp_errors::report_if_error;
use warpui::assets::asset_cache::{AssetCache, AssetSource, AssetState};
use warpui::elements::Fill as ElementFill;
use warpui::image_cache::{
    AnimatedImageBehavior, CacheOption, FitType, Image, ImageCache, StaticImage,
};
use warpui::platform::WindowStyle;
use warpui::{
    App, AppContext, EntityId, Presenter, SingletonEntity as _, TypedActionView as _, ViewHandle,
    WindowInvalidation,
};

use super::{
    bulk_close_description, row_shows_star, shows_pin_overlay, star_description,
    starred_divider_position, PaletteBulkClose,
};
use crate::appearance::Appearance;
use crate::features::FeatureFlag;
use crate::menu::MenuItem;
use crate::pane_group::Direction;
use crate::root_view::NewWorkspaceSource;
use crate::tab::tab_position_id;
use crate::undo_close::UndoCloseStack;
use crate::workspace::tab_settings::{
    TabSettings, VerticalTabsDisplayGranularity, VerticalTabsTabItemMode, VerticalTabsViewMode,
};
use crate::workspace::view::tests::{initialize_app, mock_workspace};
use crate::workspace::view::vertical_tabs::vtab_group_position_id;
use crate::workspace::view::VERTICAL_TABS_PANEL_POSITION_ID;
use crate::workspace::{Workspace, WorkspaceAction};
use crate::GlobalResourceHandles;

/// Runs `check` under each combination of the two flags stars need, passing
/// `PinnedTabs` and `StarredTabs` in.
fn for_each_flag_state(mut check: impl FnMut(bool, bool)) {
    for pins in [false, true] {
        for stars in [false, true] {
            let _pins = FeatureFlag::PinnedTabs.override_enabled(pins);
            let _stars = FeatureFlag::StarredTabs.override_enabled(stars);
            check(pins, stars);
        }
    }
}

/// A row wears the star exactly when stars are on, its tab is starred and it's
/// the tab's first row, in every combination.
#[test]
fn a_row_wears_the_star_exactly_on_the_first_row_of_a_starred_tab() {
    for_each_flag_state(|pins, stars| {
        for starred in [false, true] {
            for first_row in [false, true] {
                assert_eq!(
                    row_shows_star(starred, first_row),
                    pins && stars && starred && first_row,
                    "pins {pins}, stars {stars}, starred {starred}, first row {first_row}"
                );
            }
        }
    });
}

/// With `StarredTabs` off the pin overlay follows upstream's rule exactly; with
/// it on, the overlay never shows.
#[test]
fn stars_replace_the_pin_overlay_and_leave_it_as_it_was_when_off() {
    for_each_flag_state(|pins, stars| {
        for pinned in [false, true] {
            for hidden_by_hover in [false, true] {
                let upstream = pins && pinned && !hidden_by_hover;
                assert_eq!(
                    shows_pin_overlay(pinned, hidden_by_hover),
                    upstream && !stars,
                    "pins {pins}, stars {stars}, pinned {pinned}, hover {hidden_by_hover}"
                );
            }
        }
    });
}

/// Every set of shown tabs drawn from up to eight, with every length of starred
/// block: the hairline goes before the first shown tab past the block, exactly
/// when shown tabs sit on both sides of it, and so never with an empty block
/// (stars off), every tab starred, or a search that hides either side.
#[test]
fn the_hairline_sits_under_the_starred_block_only_when_both_sides_show() {
    let mut cases = 0;
    for tab_count in 0..=8usize {
        for shown_set in 0u32..(1 << tab_count) {
            let shown: Vec<usize> = (0..tab_count)
                .filter(|index| shown_set & (1 << index) != 0)
                .collect();
            for boundary in 0..=tab_count {
                let starred_show = shown.iter().any(|index| *index < boundary);
                let others_show = shown.iter().any(|index| *index >= boundary);
                let position = starred_divider_position(shown.iter().copied(), boundary);
                assert_eq!(
                    position.is_some(),
                    starred_show && others_show,
                    "shown {shown:?}, boundary {boundary}"
                );
                if let Some(position) = position {
                    assert!(
                        shown[..position].iter().all(|index| *index < boundary)
                            && shown[position..].iter().all(|index| *index >= boundary),
                        "shown {shown:?}, boundary {boundary}: the line at {position} must \
                         split the starred rows from the rest"
                    );
                }
                cases += 1;
            }
        }
    }
    assert_eq!(cases, 4097);
}

/// A tab layout to paint, named for failure messages.
#[derive(Clone, Copy)]
struct Layout {
    name: &'static str,
    vertical: bool,
    granularity: VerticalTabsDisplayGranularity,
    view_mode: VerticalTabsViewMode,
    item_mode: VerticalTabsTabItemMode,
}

impl Layout {
    const fn vertical(
        name: &'static str,
        granularity: VerticalTabsDisplayGranularity,
        view_mode: VerticalTabsViewMode,
        item_mode: VerticalTabsTabItemMode,
    ) -> Self {
        Self {
            name,
            vertical: true,
            granularity,
            view_mode,
            item_mode,
        }
    }

    /// Whether the layout draws one row per pane, so a split tab gets two.
    fn rows_per_pane(self) -> bool {
        self.vertical && matches!(self.granularity, VerticalTabsDisplayGranularity::Panes)
    }
}

/// Every layout whose rows the star leads: the vertical panel's compact rows,
/// expanded rows and Summary cards, each pane's row, and the horizontal tab
/// bar.
const LAYOUTS: [Layout; 6] = [
    Layout::vertical(
        "compact tab rows",
        VerticalTabsDisplayGranularity::Tabs,
        VerticalTabsViewMode::Compact,
        VerticalTabsTabItemMode::FocusedSession,
    ),
    Layout::vertical(
        "expanded tab rows",
        VerticalTabsDisplayGranularity::Tabs,
        VerticalTabsViewMode::Expanded,
        VerticalTabsTabItemMode::FocusedSession,
    ),
    Layout::vertical(
        "summary cards",
        VerticalTabsDisplayGranularity::Tabs,
        VerticalTabsViewMode::Compact,
        VerticalTabsTabItemMode::Summary,
    ),
    Layout::vertical(
        "compact pane rows",
        VerticalTabsDisplayGranularity::Panes,
        VerticalTabsViewMode::Compact,
        VerticalTabsTabItemMode::FocusedSession,
    ),
    Layout::vertical(
        "expanded pane rows",
        VerticalTabsDisplayGranularity::Panes,
        VerticalTabsViewMode::Expanded,
        VerticalTabsTabItemMode::FocusedSession,
    ),
    Layout {
        name: "horizontal tab bar",
        vertical: false,
        granularity: VerticalTabsDisplayGranularity::Tabs,
        view_mode: VerticalTabsViewMode::Compact,
        item_mode: VerticalTabsTabItemMode::FocusedSession,
    },
];

fn use_layout(app: &mut App, workspace: &ViewHandle<Workspace>, layout: Layout) {
    TabSettings::handle(app).update(app, |settings, ctx| {
        report_if_error!(settings.use_vertical_tabs.set_value(layout.vertical, ctx));
        report_if_error!(settings
            .vertical_tabs_display_granularity
            .set_value(layout.granularity, ctx));
        report_if_error!(settings
            .vertical_tabs_view_mode
            .set_value(layout.view_mode, ctx));
        report_if_error!(settings
            .vertical_tabs_tab_item_mode
            .set_value(layout.item_mode, ctx));
    });
    workspace.update(app, |workspace, _| {
        workspace.vertical_tabs_panel_open = layout.vertical;
    });
}

/// The bitmap the image cache holds for the bundled `path` at `size` points: the
/// very `Arc` that every icon painted from that file at that size shares, so an
/// icon in a scene can be told apart from any other of the same size and ink.
fn cached_icon(path: &'static str, size: i32, ctx: &AppContext) -> Arc<StaticImage> {
    let state = ImageCache::as_ref(ctx).image(
        AssetSource::Bundled { path },
        vec2i(size, size),
        FitType::Contain,
        AnimatedImageBehavior::FullAnimation,
        CacheOption::BySize,
        None,
        AssetCache::as_ref(ctx),
    );
    let AssetState::Loaded { data } = state else {
        panic!("{path} should load from the bundled assets");
    };
    let Image::Static(image) = data.as_ref() else {
        panic!("{path} should be a still image");
    };
    image.clone()
}

/// What one paint of a workspace's window drew for the tab marks.
#[derive(Debug, PartialEq)]
struct PaintedMarks {
    /// Per tab, the stars painted inside its row.
    stars: Vec<usize>,
    /// Per tab, upstream's pins painted inside its row.
    pins: Vec<usize>,
    /// Per hairline painted in the vertical tabs panel, the index of the first
    /// tab whose row lies wholly below it.
    hairlines: Vec<usize>,
    /// Per tab group, in `groups` order, the stars painted inside its block.
    group_stars: Vec<usize>,
}

/// Paints the workspace's whole window, every view in it, and reads back where
/// the stars, pins and hairlines landed.
fn paint_marks(app: &mut App, workspace: &ViewHandle<Workspace>) -> PaintedMarks {
    let (window_id, tab_count, groups) = workspace.read(app, |workspace, _| {
        let mut groups: Vec<_> = workspace
            .tabs
            .iter()
            .filter_map(|tab| tab.group_id)
            .collect();
        groups.dedup();
        (workspace.window_id, workspace.tabs.len(), groups)
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
        let star = cached_icon("bundled/svg/star-filled.svg", 10, ctx);
        let pin = cached_icon("bundled/svg/pin-filled-diagonal.svg", 16, ctx);
        let positions = presenter.position_cache();
        let rows: Vec<RectF> = (0..tab_count)
            .map(|index| {
                positions
                    .get_position(tab_position_id(index))
                    .unwrap_or_else(|| panic!("tab {index}'s row should be painted"))
            })
            .collect();
        let icons_inside = |image: &Arc<StaticImage>, bounds: RectF| {
            scene
                .layers()
                .flat_map(|layer| &layer.icons)
                .filter(|icon| Arc::ptr_eq(&icon.asset, image) && bounds.contains_rect(icon.bounds))
                .count()
        };
        let theme = Appearance::as_ref(ctx).theme();
        let hairline_inks = [
            internal_colors::fg_overlay_1(theme).into_solid(),
            internal_colors::fg_overlay_2(theme).into_solid(),
        ];
        let hairlines = match positions.get_position(VERTICAL_TABS_PANEL_POSITION_ID) {
            None => vec![],
            Some(panel) => scene
                .layers()
                .flat_map(|layer| &layer.rects)
                .filter(|rect| {
                    rect.bounds.height() == 1.
                        && panel.contains_rect(rect.bounds)
                        && matches!(
                            rect.background,
                            ElementFill::Solid(ink) if hairline_inks.contains(&ink)
                        )
                })
                .map(|rect| {
                    rows.iter()
                        .position(|row| row.min_y() >= rect.bounds.max_y())
                        .unwrap_or(tab_count)
                })
                .collect(),
        };
        PaintedMarks {
            stars: rows.iter().map(|row| icons_inside(&star, *row)).collect(),
            pins: rows.iter().map(|row| icons_inside(&pin, *row)).collect(),
            hairlines,
            group_stars: groups
                .iter()
                .map(|group_id| {
                    positions
                        .get_position(vtab_group_position_id(*group_id))
                        .map_or(0, |block| icons_inside(&star, block))
                })
                .collect(),
        }
    })
}

/// Four terminal tabs, the third split in two, then the third and fourth
/// starred, which moves them to the top: the split tab first, then the other.
fn workspace_with_two_starred_tabs(app: &mut App) -> ViewHandle<Workspace> {
    let workspace = mock_workspace(app);
    workspace.update(app, |workspace, ctx| {
        while workspace.tab_count() < 4 {
            workspace.add_terminal_tab(false, ctx);
        }
        workspace.tabs[2]
            .pane_group
            .clone()
            .update(ctx, |pane_group, ctx| {
                pane_group.add_terminal_pane(Direction::Right, None, ctx);
            });
        workspace.pin_tab(2, ctx);
        workspace.pin_tab(3, ctx);
        assert!(workspace.tabs[0].pinned && workspace.tabs[1].pinned);
        assert_eq!(
            workspace.tabs[0]
                .pane_group
                .as_ref(ctx)
                .visible_pane_ids()
                .len(),
            2,
            "the first starred tab is the split one"
        );
    });
    workspace
}

/// With stars on, every layout paints one star on each starred tab, a split tab
/// included, and none elsewhere; upstream's pin nowhere; and in the vertical
/// panel one hairline, between the starred tabs and the rest. Starring every
/// tab takes the line away, and a starred group wears one star, on its header.
#[test]
fn stars_and_the_hairline_paint_where_they_belong() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(true);
    let _vertical_tabs = FeatureFlag::VerticalTabs.override_enabled(true);
    let _summary = FeatureFlag::VerticalTabsSummaryMode.override_enabled(true);
    let _groups = FeatureFlag::GroupedTabs.override_enabled(true);

    App::test(crate::ASSETS, |mut app| async move {
        initialize_app(&mut app);
        let workspace = workspace_with_two_starred_tabs(&mut app);
        for layout in LAYOUTS {
            use_layout(&mut app, &workspace, layout);
            assert_eq!(
                paint_marks(&mut app, &workspace),
                PaintedMarks {
                    stars: vec![1, 1, 0, 0],
                    pins: vec![0; 4],
                    hairlines: if layout.vertical { vec![2] } else { vec![] },
                    group_stars: vec![],
                },
                "{}",
                layout.name
            );
        }

        // Every tab starred: there's nothing to divide.
        workspace.update(&mut app, |workspace, ctx| {
            workspace.pin_tab(2, ctx);
            workspace.pin_tab(3, ctx);
        });
        for layout in LAYOUTS {
            use_layout(&mut app, &workspace, layout);
            let painted = paint_marks(&mut app, &workspace);
            assert_eq!(painted.stars, vec![1; 4], "{}", layout.name);
            assert_eq!(painted.hairlines, Vec::<usize>::new(), "{}", layout.name);
        }

        // The last two tabs unstarred and grouped, and the group starred: its
        // header wears the star, and its members wear none of their own.
        workspace.update(&mut app, |workspace, ctx| {
            workspace.unpin_tab(3, ctx);
            workspace.unpin_tab(2, ctx);
            workspace.handle_action(&WorkspaceAction::NewTabGroupFromTab(2), ctx);
            let group_id = workspace.tabs[2].group_id.expect("tab 2 is grouped");
            workspace.handle_action(
                &WorkspaceAction::MoveTabToGroup {
                    tab_index: 3,
                    group_id,
                },
                ctx,
            );
            workspace.handle_action(&WorkspaceAction::PinTabGroup(group_id), ctx);
            assert!(workspace.tab_groups[&group_id].pinned);
        });
        use_layout(&mut app, &workspace, LAYOUTS[0]);
        let painted = paint_marks(&mut app, &workspace);
        assert_eq!(
            painted.group_stars,
            vec![1],
            "one star, on the group's header"
        );
        assert_eq!(
            painted.stars.iter().sum::<usize>(),
            2,
            "the two starred tabs wear one star each, the group's members none: {painted:?}"
        );
    });
}

/// With `StarredTabs` off, pinned tabs paint as upstream paints them: its pin on
/// each pinned row (each of a split tab's rows, in the per-pane layouts) or in
/// the horizontal tab's close slot, with no star and no hairline anywhere.
#[test]
fn with_stars_off_pinned_tabs_paint_as_upstream_does() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(false);
    let _vertical_tabs = FeatureFlag::VerticalTabs.override_enabled(true);
    let _summary = FeatureFlag::VerticalTabsSummaryMode.override_enabled(true);
    let _groups = FeatureFlag::GroupedTabs.override_enabled(true);

    App::test(crate::ASSETS, |mut app| async move {
        initialize_app(&mut app);
        let workspace = workspace_with_two_starred_tabs(&mut app);
        for layout in LAYOUTS {
            use_layout(&mut app, &workspace, layout);
            let split_tab_pins = if layout.rows_per_pane() { 2 } else { 1 };
            assert_eq!(
                paint_marks(&mut app, &workspace),
                PaintedMarks {
                    stars: vec![0; 4],
                    pins: vec![split_tab_pins, 1, 0, 0],
                    hairlines: vec![],
                    group_stars: vec![],
                },
                "{}",
                layout.name
            );
        }
    });
}

/// The tabs' pane-group ids, in list order.
fn tab_ids(workspace: &Workspace) -> Vec<EntityId> {
    workspace
        .tabs
        .iter()
        .map(|tab| tab.pane_group.id())
        .collect()
}

fn workspace_with_tabs(app: &mut App, count: usize) -> ViewHandle<Workspace> {
    let workspace = mock_workspace(app);
    workspace.update(app, |workspace, ctx| {
        while workspace.tab_count() < count {
            workspace.add_terminal_tab(false, ctx);
        }
    });
    workspace
}

/// Moves the tab at `index` from `source` into `target`'s window, dropped on
/// `slot`, the way a cross-window drag's handoff does. Returns the moved tab's
/// pane group and the index it landed at.
fn move_tab_to_other_window(
    app: &mut App,
    source: &ViewHandle<Workspace>,
    index: usize,
    target: &ViewHandle<Workspace>,
    slot: usize,
) -> (EntityId, usize) {
    let transferred = source
        .read(app, |workspace, ctx| workspace.get_tab_transfer_info(index, ctx))
        .expect("the source keeps another tab");
    let pane_group_id = transferred.pane_group.id();
    source.update(app, |workspace, ctx| {
        workspace.prepare_for_transferred_tab_attach(&transferred.pane_group, ctx);
    });
    let (from, to) = app.read(|ctx| (source.window_id(ctx), target.window_id(ctx)));
    app.update(|ctx| {
        ctx.transfer_view_tree_to_window(pane_group_id, from, to);
    });
    let landed = target.update(app, |workspace, ctx| {
        workspace.insert_transferred_tab_at_index(transferred, slot, ctx)
    });
    source.update(app, |workspace, ctx| {
        workspace.remove_tab_without_undo(index, ctx);
        workspace.set_suppress_detach_panes_on_window_close(false);
    });
    (pane_group_id, landed)
}

/// A starred tab moved to another window keeps its star and lands at the end
/// of that window's starred block, whatever slot it's dropped on; an unstarred
/// one lands on its slot. With stars off, upstream's result: the pin stays
/// behind, and every tab lands on its slot, pushed past the pinned block.
#[test]
fn a_starred_tab_moved_to_another_window_keeps_its_star() {
    for stars in [true, false] {
        let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
        let _stars = FeatureFlag::StarredTabs.override_enabled(stars);
        App::test((), |mut app| async move {
            initialize_app(&mut app);
            // The source leads with two starred tabs, the target with one.
            let source = workspace_with_tabs(&mut app, 4);
            let target = workspace_with_tabs(&mut app, 3);
            source.update(&mut app, |workspace, ctx| {
                workspace.pin_tab(0, ctx);
                workspace.pin_tab(1, ctx);
            });
            target.update(&mut app, |workspace, ctx| workspace.pin_tab(0, ctx));

            // Each move takes the source's first tab: (starred, drop slot,
            // where it lands and how long the target's starred block is then,
            // with stars on and with them off).
            for (starred, slot, with_stars, without_stars) in [
                (true, 3, (1, 2), (3, 1)),
                (true, 0, (2, 3), (1, 1)),
                (false, 5, (5, 3), (5, 1)),
            ] {
                let case = format!("stars {stars}, starred {starred}, slot {slot}");
                let (expected_landing, block) = if stars { with_stars } else { without_stars };
                let (moved, landed) = move_tab_to_other_window(&mut app, &source, 0, &target, slot);
                target.read(&app, |workspace, _| {
                    assert_eq!(landed, expected_landing, "{case}");
                    assert_eq!(workspace.tabs[landed].pane_group.id(), moved, "{case}");
                    assert_eq!(workspace.active_tab_index, landed, "{case}");
                    assert_eq!(workspace.tabs[landed].pinned, stars && starred, "{case}");
                    assert_eq!(
                        workspace.pinned_boundary_index(&workspace.tabs),
                        block,
                        "{case}"
                    );
                    assert!(
                        workspace.tabs[block..].iter().all(|tab| !tab.pinned),
                        "{case}: the starred tabs stay one block at the top"
                    );
                });
            }
        });
    }
}

/// Moved into a window of its own, a starred tab keeps its star with stars on,
/// and arrives unpinned with them off, as upstream's does.
#[test]
fn a_starred_tab_moved_to_a_new_window_keeps_its_star() {
    for stars in [true, false] {
        let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
        let _stars = FeatureFlag::StarredTabs.override_enabled(stars);
        App::test((), |mut app| async move {
            initialize_app(&mut app);
            let source = workspace_with_tabs(&mut app, 2);
            source.update(&mut app, |workspace, ctx| workspace.pin_tab(1, ctx));
            for (index, starred) in [(0, true), (1, false)] {
                let transferred = source
                    .read(&app, |workspace, ctx| {
                        workspace.get_tab_transfer_info(index, ctx)
                    })
                    .expect("the source has two tabs");
                assert_eq!(transferred.pinned, starred, "the snapshot carries the star");

                let global_resource_handles = GlobalResourceHandles::mock(&mut app);
                let (_, window) = app.add_window(WindowStyle::NotStealFocus, |ctx| {
                    Workspace::new(
                        global_resource_handles,
                        None,
                        NewWorkspaceSource::TransferredTab {
                            tab_color: None,
                            custom_title: None,
                            left_panel_open: false,
                            vertical_tabs_panel_open: false,
                            right_panel_open: false,
                            is_right_panel_maximized: false,
                            is_tab_drag_preview: false,
                            pinned: transferred.pinned,
                        },
                        ctx,
                    )
                });
                window.read(&app, |workspace, _| {
                    assert_eq!(workspace.tabs.len(), 1);
                    assert_eq!(
                        workspace.tabs[0].pinned,
                        stars && starred,
                        "stars {stars}, starred {starred}"
                    );
                });
            }
        });
    }
}

fn star(pane_group_id: EntityId, starred: bool) -> WorkspaceAction {
    WorkspaceAction::SetTabStarred {
        pane_group_id,
        starred,
    }
}

/// The action behind the star item of the menu for the tab at `index`.
fn star_item_action(workspace: &Workspace, index: usize, ctx: &AppContext) -> WorkspaceAction {
    let items = workspace.tabs[index].menu_items(
        index,
        workspace.tabs.len(),
        workspace.starred_boundary(),
        index == workspace.active_tab_index,
        &workspace.tab_groups,
        false,
        index > 0,
        index + 1 < workspace.tabs.len(),
        ctx,
    );
    items
        .iter()
        .find_map(|item| match item {
            MenuItem::Item(fields) if fields.label().contains("tar tab") => {
                fields.on_select_action().cloned()
            }
            _ => None,
        })
        .expect("the tab menu has a star item")
}

/// A star acts on the tab its action names, wherever that tab has moved since
/// the menu opened. Asking for the state a tab already has, or naming a tab that
/// has since closed, changes nothing.
#[test]
fn a_star_follows_its_tab_and_repeating_it_changes_nothing() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(true);

    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = workspace_with_tabs(&mut app, 4);
        workspace.update(&mut app, |workspace, ctx| {
            let [a, b, c, d] = <[EntityId; 4]>::try_from(tab_ids(workspace)).unwrap();

            workspace.handle_action(&star(c, true), ctx);
            assert_eq!(tab_ids(workspace), [c, a, b, d]);
            assert!(workspace.tabs[0].pinned);

            // Again, and an unstar of a tab that isn't starred: no change.
            workspace.handle_action(&star(c, true), ctx);
            workspace.handle_action(&star(d, false), ctx);
            assert_eq!(tab_ids(workspace), [c, a, b, d]);
            assert_eq!(workspace.starred_boundary(), 1);

            // A menu opened on d, then d moved up before the click: the click
            // still stars d.
            let click = star_item_action(workspace, 3, ctx);
            workspace.handle_action(&WorkspaceAction::MoveTabLeft(3), ctx);
            assert_eq!(tab_ids(workspace), [c, a, d, b]);
            workspace.handle_action(&click, ctx);
            assert_eq!(tab_ids(workspace), [c, d, a, b]);
            assert_eq!(workspace.starred_boundary(), 2);

            // A tab that has since closed.
            workspace.handle_action(&WorkspaceAction::CloseTab(3), ctx);
            assert_eq!(tab_ids(workspace), [c, d, a]);
            workspace.handle_action(&star(b, true), ctx);
            assert_eq!(tab_ids(workspace), [c, d, a]);
            assert_eq!(workspace.starred_boundary(), 2);
        });
    });
}

/// Ctrl-Cmd-S stars the active tab, which stays active as it moves to the top,
/// and pressing it again unstars it, still active.
#[test]
fn the_star_key_keeps_the_active_tab_active() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(true);

    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = workspace_with_tabs(&mut app, 4);
        workspace.update(&mut app, |workspace, ctx| {
            workspace.handle_action(&WorkspaceAction::ActivateTab(2), ctx);
            let active = workspace.tabs[2].pane_group.id();

            workspace.handle_action(&WorkspaceAction::ToggleActiveTabStar, ctx);
            assert_eq!(workspace.tabs[0].pane_group.id(), active);
            assert!(workspace.tabs[0].pinned);
            assert_eq!(workspace.active_tab_index, 0);

            workspace.handle_action(&WorkspaceAction::ToggleActiveTabStar, ctx);
            assert!(workspace.tabs.iter().all(|tab| !tab.pinned));
            assert_eq!(
                workspace.tabs[workspace.active_tab_index].pane_group.id(),
                active
            );
        });
    });
}

/// With pins on but stars off, the star actions do nothing.
#[test]
fn the_star_actions_do_nothing_with_stars_off() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(false);

    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = workspace_with_tabs(&mut app, 3);
        workspace.update(&mut app, |workspace, ctx| {
            let before = tab_ids(workspace);
            workspace.handle_action(&star(before[2], true), ctx);
            workspace.handle_action(&WorkspaceAction::ToggleActiveTabStar, ctx);
            assert_eq!(tab_ids(workspace), before);
            assert!(workspace.tabs.iter().all(|tab| !tab.pinned));
        });
    });
}

/// The palette's words for Ctrl-Cmd-S follow the active tab as the menu's do,
/// and its bulk closes say "(keep starred)" exactly when they would spare a
/// starred tab.
#[test]
fn the_palette_says_what_the_star_and_bulk_close_keys_will_do() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(true);
    let _groups = FeatureFlag::GroupedTabs.override_enabled(true);

    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = workspace_with_tabs(&mut app, 4);
        workspace.update(&mut app, |workspace, ctx| {
            let others = |workspace: &Workspace| {
                bulk_close_description(workspace, "close other tabs", PaletteBulkClose::OtherTabs)
            };
            let below = |workspace: &Workspace| {
                bulk_close_description(workspace, "close tabs below", PaletteBulkClose::TabsAfter)
            };

            workspace.handle_action(&WorkspaceAction::ActivateTab(3), ctx);
            assert_eq!(star_description(workspace), None);
            assert_eq!(others(workspace).as_deref(), Some("close other tabs"));
            assert_eq!(below(workspace), None, "nothing below the last tab");

            // The active tab starred: it moves to the top.
            workspace.handle_action(&WorkspaceAction::ToggleActiveTabStar, ctx);
            assert_eq!(
                star_description(workspace).as_deref(),
                Some("unstar current tab")
            );
            assert_eq!(others(workspace).as_deref(), Some("close other tabs"));
            assert_eq!(below(workspace).as_deref(), Some("close tabs below"));

            // A second starred tab, below the active one.
            let second = workspace.tabs[1].pane_group.id();
            workspace.handle_action(&star(second, true), ctx);
            assert_eq!(
                others(workspace).as_deref(),
                Some("close other tabs (keep starred)")
            );
            assert_eq!(
                below(workspace).as_deref(),
                Some("close tabs below (keep starred)")
            );

            // An unstarred, grouped active tab: starring pulls it out.
            workspace.handle_action(&WorkspaceAction::ActivateTab(3), ctx);
            workspace.handle_action(&WorkspaceAction::NewTabGroupFromTab(3), ctx);
            assert_eq!(
                star_description(workspace).as_deref(),
                Some("star current tab (leaves group)")
            );
            assert_eq!(below(workspace), None);
            assert_eq!(
                others(workspace).as_deref(),
                Some("close other tabs (keep starred)")
            );
        });
    });
}

/// Every bulk close, from every tab of every list of up to five tabs with every
/// length of starred block: it closes exactly the unstarred tabs in its range,
/// and never a starred one.
#[test]
fn bulk_closes_never_close_a_starred_tab() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(true);

    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let mut cases = 0;
        for tab_count in 1..=5 {
            for starred in 0..=tab_count {
                for index in 0..tab_count {
                    for close in [
                        WorkspaceAction::CloseOtherTabs(index),
                        WorkspaceAction::CloseTabsRight(index),
                        WorkspaceAction::CloseNonActiveTabs,
                        WorkspaceAction::CloseTabsRightActiveTab,
                    ] {
                        let workspace = workspace_with_tabs(&mut app, tab_count);
                        workspace.update(&mut app, |workspace, ctx| {
                            let ids = tab_ids(workspace);
                            for id in &ids[..starred] {
                                workspace.handle_action(&star(*id, true), ctx);
                            }
                            assert_eq!(tab_ids(workspace), ids);
                            workspace.handle_action(&WorkspaceAction::ActivateTab(index), ctx);
                            let closes_both_sides = matches!(
                                close,
                                WorkspaceAction::CloseOtherTabs(_)
                                    | WorkspaceAction::CloseNonActiveTabs
                            );
                            let survivors: Vec<EntityId> = ids
                                .iter()
                                .enumerate()
                                .filter(|(i, _)| {
                                    *i < starred
                                        || *i == index
                                        || (!closes_both_sides && *i < index)
                                })
                                .map(|(_, id)| *id)
                                .collect();
                            workspace.handle_action(&close, ctx);
                            assert_eq!(
                                tab_ids(workspace),
                                survivors,
                                "{close:?} from tab {index} of {tab_count}, {starred} starred"
                            );
                        });
                        cases += 1;
                    }
                }
            }
        }
        assert_eq!(cases, 280);
    });
}

/// A small deterministic generator, so a failing run replays from its seed.
struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % bound as u64) as usize
    }
}

/// One step of the zone sweep: everything that stars, moves, adds, closes,
/// reopens, groups or jumps between tabs.
#[derive(Clone, Copy, Debug)]
enum Step {
    Star(usize),
    Unstar(usize),
    ToggleActive,
    Activate(usize),
    MoveUp(usize),
    MoveDown(usize),
    Add,
    Close(usize),
    Reopen,
    NewGroup(usize),
    JoinGroup(usize),
    LeaveGroup(usize),
    StarGroup(usize),
    UnstarGroup(usize),
    CloseOthers(usize),
    CloseBelow(usize),
    CloseNonActive,
    CloseBelowActive,
    MarkUnread(usize),
    JumpToNextUnread,
}

impl Step {
    fn random(rng: &mut Rng, tab_count: usize) -> Self {
        let tab = rng.below(tab_count);
        match rng.below(20) {
            0 => Step::Star(tab),
            1 => Step::Unstar(tab),
            2 => Step::ToggleActive,
            3 => Step::Activate(tab),
            4 => Step::MoveUp(tab),
            5 => Step::MoveDown(tab),
            6 if tab_count < 7 => Step::Add,
            7 if tab_count > 1 => Step::Close(tab),
            8 => Step::Reopen,
            9 => Step::NewGroup(tab),
            10 => Step::JoinGroup(tab),
            11 => Step::LeaveGroup(tab),
            12 => Step::StarGroup(tab),
            13 => Step::UnstarGroup(tab),
            14 => Step::CloseOthers(tab),
            15 => Step::CloseBelow(tab),
            16 => Step::CloseNonActive,
            17 => Step::CloseBelowActive,
            18 => Step::MarkUnread(tab),
            19 => Step::JumpToNextUnread,
            _ => Step::Star(tab),
        }
    }

    /// Whether the step leaves the same tab active. Activating, adding,
    /// reopening and jumping move focus by design, and so does a new group,
    /// which upstream activates.
    fn keeps_the_active_tab(self) -> bool {
        !matches!(
            self,
            Step::Activate(_)
                | Step::Add
                | Step::Reopen
                | Step::JumpToNextUnread
                | Step::NewGroup(_)
        ) && !self.closes()
    }

    fn closes(self) -> bool {
        matches!(self, Step::Close(_)) || self.bulk_closes()
    }

    fn bulk_closes(self) -> bool {
        matches!(
            self,
            Step::CloseOthers(_)
                | Step::CloseBelow(_)
                | Step::CloseNonActive
                | Step::CloseBelowActive
        )
    }

    /// The workspace action this step dispatches, if it is one and it applies.
    fn action(self, workspace: &Workspace) -> Option<WorkspaceAction> {
        let id = |index: usize| workspace.tabs[index].pane_group.id();
        let group = |index: usize| workspace.tabs[index].group_id;
        Some(match self {
            Step::Star(index) => star(id(index), true),
            Step::Unstar(index) => star(id(index), false),
            Step::ToggleActive => WorkspaceAction::ToggleActiveTabStar,
            Step::Activate(index) => WorkspaceAction::ActivateTab(index),
            Step::MoveUp(index) => WorkspaceAction::MoveTabLeft(index),
            Step::MoveDown(index) => WorkspaceAction::MoveTabRight(index),
            Step::Close(index) => WorkspaceAction::CloseTab(index),
            Step::NewGroup(index) => WorkspaceAction::NewTabGroupFromTab(index),
            Step::JoinGroup(index) => {
                let group_id = workspace
                    .tabs
                    .iter()
                    .filter_map(|tab| tab.group_id)
                    .find(|group_id| group(index) != Some(*group_id))?;
                WorkspaceAction::MoveTabToGroup {
                    tab_index: index,
                    group_id,
                }
            }
            Step::LeaveGroup(index) => WorkspaceAction::RemoveTabFromGroup(index),
            Step::StarGroup(index) => WorkspaceAction::PinTabGroup(group(index)?),
            Step::UnstarGroup(index) => WorkspaceAction::UnpinTabGroup(group(index)?),
            Step::CloseOthers(index) => WorkspaceAction::CloseOtherTabs(index),
            Step::CloseBelow(index) => WorkspaceAction::CloseTabsRight(index),
            Step::CloseNonActive => WorkspaceAction::CloseNonActiveTabs,
            Step::CloseBelowActive => WorkspaceAction::CloseTabsRightActiveTab,
            Step::MarkUnread(index) => WorkspaceAction::SetTabUnread {
                pane_group_id: id(index),
                terminal_view_id: None,
                unread: true,
            },
            Step::JumpToNextUnread => WorkspaceAction::JumpToNextUnreadTab,
            Step::Add | Step::Reopen => return None,
        })
    }
}

/// What the zone sweep checks a step against.
struct Before {
    active: EntityId,
    starred: Vec<EntityId>,
}

/// Seeded runs of 200 steps each: after every step the starred tabs are one
/// block at the top of the list, no tab is both starred and grouped, the active
/// tab is the same tab unless the step closed it or was meant to move focus, and
/// no bulk close took a starred tab with it.
#[test]
fn starred_tabs_stay_one_block_at_the_top_through_any_sequence() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(true);
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);
    let _groups = FeatureFlag::GroupedTabs.override_enabled(true);

    for seed in 1..=8u64 {
        App::test((), |mut app| async move {
            initialize_app(&mut app);
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let workspace = workspace_with_tabs(&mut app, 3);
            for step_number in 0..200 {
                let (step, before) = workspace.read(&app, |workspace, _| {
                    let step = Step::random(&mut rng, workspace.tabs.len());
                    let before = Before {
                        active: workspace.tabs[workspace.active_tab_index].pane_group.id(),
                        starred: workspace
                            .tabs
                            .iter()
                            .filter(|tab| workspace.is_tab_effectively_pinned(tab))
                            .map(|tab| tab.pane_group.id())
                            .collect(),
                    };
                    (step, before)
                });
                match step {
                    Step::Add => workspace.update(&mut app, |workspace, ctx| {
                        workspace.add_terminal_tab(false, ctx);
                    }),
                    Step::Reopen => UndoCloseStack::handle(&app)
                        .update(&mut app, |stack, ctx| stack.undo_close(ctx)),
                    _ => workspace.update(&mut app, |workspace, ctx| {
                        if let Some(action) = step.action(workspace) {
                            workspace.handle_action(&action, ctx);
                        }
                    }),
                }
                workspace.read(&app, |workspace, _| {
                    let context = format!("seed {seed}, step {step_number} ({step:?})");
                    let ids = tab_ids(workspace);
                    assert!(
                        !ids.is_empty() && workspace.active_tab_index < ids.len(),
                        "{context}"
                    );
                    let starred: Vec<bool> = workspace
                        .tabs
                        .iter()
                        .map(|tab| workspace.is_tab_effectively_pinned(tab))
                        .collect();
                    assert!(
                        starred.windows(2).all(|pair| pair[0] || !pair[1]),
                        "{context}: the starred tabs must lead the list as one block, \
                         got {starred:?}"
                    );
                    assert!(
                        workspace
                            .tabs
                            .iter()
                            .all(|tab| !(tab.pinned && tab.group_id.is_some())),
                        "{context}: a tab is both starred and grouped"
                    );
                    let active = ids[workspace.active_tab_index];
                    if step.keeps_the_active_tab()
                        || (step.closes() && ids.contains(&before.active))
                    {
                        assert_eq!(active, before.active, "{context}: the active tab changed");
                    }
                    if step.bulk_closes() {
                        assert!(
                            before.starred.iter().all(|id| ids.contains(id)),
                            "{context}: a bulk close took a starred tab"
                        );
                    }
                });
            }
        });
    }
}
