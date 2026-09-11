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
use warpui::{
    App, AppContext, Presenter, SingletonEntity as _, TypedActionView as _, ViewHandle,
    WindowInvalidation,
};

use super::{row_shows_star, shows_pin_overlay, starred_divider_position};
use crate::appearance::Appearance;
use crate::features::FeatureFlag;
use crate::pane_group::Direction;
use crate::tab::tab_position_id;
use crate::workspace::tab_settings::{
    TabSettings, VerticalTabsDisplayGranularity, VerticalTabsTabItemMode, VerticalTabsViewMode,
};
use crate::workspace::view::tests::{initialize_app, mock_workspace};
use crate::workspace::view::vertical_tabs::vtab_group_position_id;
use crate::workspace::view::VERTICAL_TABS_PANEL_POSITION_ID;
use crate::workspace::{Workspace, WorkspaceAction};

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

/// Every layout whose rows the star leads: the vertical panel's compact rows
/// (Cooper's), expanded rows and Summary cards, each pane's row, and the
/// horizontal tab bar.
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
