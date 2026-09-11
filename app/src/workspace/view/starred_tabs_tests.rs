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
    bulk_close_description, shows_pin_overlay, starred_divider_position, PaletteBulkClose,
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
use crate::workspace::view::tab_emoji_picker::TabEmojiPickerEvent;
use crate::workspace::view::tab_tags::STAR;
use crate::workspace::view::tests::{initialize_app, mock_workspace};
use crate::workspace::view::VERTICAL_TABS_PANEL_POSITION_ID;
use crate::workspace::{Workspace, WorkspaceAction};
use crate::GlobalResourceHandles;

/// Runs `check` under each combination of the two flags tags need, passing
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

/// With `StarredTabs` off the pin overlay follows upstream's rule exactly; with
/// it on, the overlay never shows.
#[test]
fn tags_replace_the_pin_overlay_and_leave_it_as_it_was_when_off() {
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

/// Every set of shown tabs drawn from up to eight, with every length of
/// floating block: the hairline goes before the first shown tab past the
/// block, exactly when shown tabs sit on both sides of it, and so never with
/// an empty block (tags off), every tab floating, or a search that hides
/// either side.
#[test]
fn the_hairline_sits_under_the_floating_block_only_when_both_sides_show() {
    let mut cases = 0;
    for tab_count in 0..=8usize {
        for shown_set in 0u32..(1 << tab_count) {
            let shown: Vec<usize> = (0..tab_count)
                .filter(|index| shown_set & (1 << index) != 0)
                .collect();
            for boundary in 0..=tab_count {
                let floating_show = shown.iter().any(|index| *index < boundary);
                let others_show = shown.iter().any(|index| *index >= boundary);
                let position = starred_divider_position(shown.iter().copied(), boundary);
                assert_eq!(
                    position.is_some(),
                    floating_show && others_show,
                    "shown {shown:?}, boundary {boundary}"
                );
                if let Some(position) = position {
                    assert!(
                        shown[..position].iter().all(|index| *index < boundary)
                            && shown[position..].iter().all(|index| *index >= boundary),
                        "shown {shown:?}, boundary {boundary}: the line at {position} must \
                         split the floating rows from the rest"
                    );
                }
                cases += 1;
            }
        }
    }
    assert_eq!(cases, 4097);
}

/// The icon renderer tints a bundled icon with its ink and takes the icon's red
/// channel as that ink's opacity, so the fork's icons must paint in white to
/// show at full strength. The star, filled with #121212, once painted at about
/// 7% of its ink and all but vanished.
#[test]
fn the_forks_icons_paint_in_white_so_they_show_in_their_full_ink() {
    use warpui::assets::AssetProvider as _;

    for path in [
        "bundled/svg/star-filled.svg",
        "bundled/svg/face-smile-plus.svg",
    ] {
        let svg = crate::ASSETS
            .get(path)
            .expect("the fork's icons are bundled");
        let svg = std::str::from_utf8(&svg).expect("an icon is text");
        let paint_values = |attribute: &str| -> Vec<String> {
            svg.match_indices(attribute)
                .map(|(start, _)| {
                    let value = &svg[start + attribute.len()..];
                    value[..value.find('"').expect("a closing quote")].to_ascii_lowercase()
                })
                .collect()
        };

        // The root's `fill="none"` only says the canvas has no fill of its own.
        let paints: Vec<String> = paint_values(" fill=\"")
            .into_iter()
            .chain(paint_values(" stroke=\""))
            .filter(|paint| paint != "none")
            .collect();
        assert!(!paints.is_empty(), "{path} paints something");
        for paint in &paints {
            assert!(
                matches!(paint.as_str(), "white" | "#fff" | "#ffffff"),
                "{path} must paint white, not {paint}"
            );
        }
        for attribute in [" opacity=\"", " fill-opacity=\"", " stroke-opacity=\""] {
            assert_eq!(
                paint_values(attribute),
                Vec::<String>::new(),
                "{path}: a{attribute}..\" would dim it"
            );
        }
    }
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

/// Every layout a tab's emoji lead: the vertical panel's compact rows,
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

fn set_float_tagged_tabs(app: &mut App, float: bool) {
    TabSettings::handle(app).update(app, |settings, ctx| {
        report_if_error!(settings.float_tagged_tabs.set_value(float, ctx));
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

/// What one paint of a workspace's window drew around the floating block.
/// Where the emoji themselves land is the screenshot test's to check.
#[derive(Debug, PartialEq)]
struct PaintedMarks {
    /// Per tab, upstream's pins painted inside its row.
    pins: Vec<usize>,
    /// Per hairline painted in the vertical tabs panel, the index of the first
    /// tab whose row lies wholly below it.
    hairlines: Vec<usize>,
    /// How many lines the vertical tabs panel draws where the floating block
    /// ends, between the last floating tab's row and the first other one: the
    /// hairline, and any border along either row's facing edge. `None` when
    /// the panel isn't showing or the block doesn't end between two tabs.
    lines_closing_the_block: Option<usize>,
}

/// Paints the workspace's whole window, every view in it, and reads back where
/// the pins and hairlines landed.
fn paint_marks(app: &mut App, workspace: &ViewHandle<Workspace>) -> PaintedMarks {
    let (window_id, tab_count, starred_boundary) = workspace.read(app, |workspace, _| {
        (
            workspace.window_id,
            workspace.tabs.len(),
            workspace.starred_boundary(),
        )
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
        let is_hairline_ink = |fill: &ElementFill| {
            matches!(fill, ElementFill::Solid(ink) if hairline_inks.contains(ink))
        };
        let panel = positions.get_position(VERTICAL_TABS_PANEL_POSITION_ID);
        let hairlines = match panel {
            None => vec![],
            Some(panel) => scene
                .layers()
                .flat_map(|layer| &layer.rects)
                .filter(|rect| {
                    rect.bounds.height() == 1.
                        && panel.contains_rect(rect.bounds)
                        && is_hairline_ink(&rect.background)
                })
                .map(|rect| {
                    rows.iter()
                        .position(|row| row.min_y() >= rect.bounds.max_y())
                        .unwrap_or(tab_count)
                })
                .collect(),
        };
        let lines_closing_the_block = panel
            .filter(|_| starred_boundary > 0 && starred_boundary < tab_count)
            .map(|panel| {
                let (above, below) = (rows[starred_boundary - 1], rows[starred_boundary]);
                let at_the_edge = |y: f32| y >= above.max_y() - 1.5 && y <= below.min_y() + 1.5;
                scene
                    .layers()
                    .flat_map(|layer| &layer.rects)
                    .filter(|rect| panel.contains_rect(rect.bounds))
                    .map(|rect| {
                        let own_line = rect.bounds.height() == 1.
                            && is_hairline_ink(&rect.background)
                            && at_the_edge(rect.bounds.min_y());
                        let border = &rect.border;
                        let border_line = border.width == 1. && is_hairline_ink(&border.color);
                        usize::from(own_line)
                            + usize::from(
                                border_line && border.top && at_the_edge(rect.bounds.min_y()),
                            )
                            + usize::from(
                                border_line
                                    && border.bottom
                                    && at_the_edge(rect.bounds.max_y() - 1.),
                            )
                    })
                    .sum()
            });
        PaintedMarks {
            pins: rows.iter().map(|row| icons_inside(&pin, *row)).collect(),
            hairlines,
            lines_closing_the_block,
        }
    })
}

/// Four terminal tabs, the third split in two, then the third and fourth
/// floated, which moves them to the top: the split tab first, then the other.
fn workspace_with_two_floating_tabs(app: &mut App) -> ViewHandle<Workspace> {
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
            "the first floating tab is the split one"
        );
    });
    workspace
}

/// With tags on, no layout paints upstream's pin, and the vertical panel
/// paints one hairline, between the floating tabs and the rest, a split tab's
/// rows included. Floating every tab takes the line away.
#[test]
fn the_hairline_paints_under_the_floating_tabs_and_no_pin_shows() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(true);
    let _vertical_tabs = FeatureFlag::VerticalTabs.override_enabled(true);
    let _summary = FeatureFlag::VerticalTabsSummaryMode.override_enabled(true);
    let _groups = FeatureFlag::GroupedTabs.override_enabled(true);

    App::test(crate::ASSETS, |mut app| async move {
        initialize_app(&mut app);
        let workspace = workspace_with_two_floating_tabs(&mut app);
        for layout in LAYOUTS {
            use_layout(&mut app, &workspace, layout);
            assert_eq!(
                paint_marks(&mut app, &workspace),
                PaintedMarks {
                    pins: vec![0; 4],
                    hairlines: if layout.vertical { vec![2] } else { vec![] },
                    lines_closing_the_block: layout.vertical.then_some(1),
                },
                "{}",
                layout.name
            );
        }

        // Every tab floating: there's nothing to divide.
        workspace.update(&mut app, |workspace, ctx| {
            workspace.pin_tab(2, ctx);
            workspace.pin_tab(3, ctx);
        });
        for layout in LAYOUTS {
            use_layout(&mut app, &workspace, layout);
            let painted = paint_marks(&mut app, &workspace);
            assert_eq!(painted.hairlines, Vec::<usize>::new(), "{}", layout.name);
            assert_eq!(painted.pins, vec![0; 4], "{}", layout.name);
        }
    });
}

/// With `StarredTabs` off, pinned tabs paint as upstream paints them: its pin on
/// each pinned row (each of a split tab's rows, in the per-pane layouts) or in
/// the horizontal tab's close slot, with no hairline anywhere.
#[test]
fn with_tags_off_pinned_tabs_paint_as_upstream_does() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(false);
    let _vertical_tabs = FeatureFlag::VerticalTabs.override_enabled(true);
    let _summary = FeatureFlag::VerticalTabsSummaryMode.override_enabled(true);
    let _groups = FeatureFlag::GroupedTabs.override_enabled(true);

    App::test(crate::ASSETS, |mut app| async move {
        initialize_app(&mut app);
        let workspace = workspace_with_two_floating_tabs(&mut app);
        for layout in LAYOUTS {
            use_layout(&mut app, &workspace, layout);
            let split_tab_pins = if layout.rows_per_pane() { 2 } else { 1 };
            assert_eq!(
                paint_marks(&mut app, &workspace),
                PaintedMarks {
                    pins: vec![split_tab_pins, 1, 0, 0],
                    hairlines: vec![],
                    lines_closing_the_block: None,
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

/// The tabs' emoji, in list order.
fn tab_tags(workspace: &Workspace) -> Vec<Vec<String>> {
    workspace.tabs.iter().map(|tab| tab.tags.to_vec()).collect()
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

fn emoji(pane_group_id: EntityId, emoji: &str, present: bool) -> WorkspaceAction {
    WorkspaceAction::SetTabEmoji {
        pane_group_id,
        emoji: emoji.to_owned(),
        present,
    }
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
        .read(app, |workspace, ctx| {
            workspace.get_tab_transfer_info(index, ctx)
        })
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

/// A tagged tab moved to another window keeps its emoji and keeps floating,
/// landing at the end of that window's floating block whatever slot it's
/// dropped on; an untagged one lands on its slot. With tags off, upstream's
/// result: the pin stays behind, and every tab lands on its slot, pushed past
/// the pinned block.
#[test]
fn a_tagged_tab_moved_to_another_window_keeps_its_emoji() {
    for tags in [true, false] {
        let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
        let _stars = FeatureFlag::StarredTabs.override_enabled(tags);
        App::test((), |mut app| async move {
            initialize_app(&mut app);
            // The source leads with two floating tabs wearing 🔥, the target
            // with one.
            let source = workspace_with_tabs(&mut app, 4);
            let target = workspace_with_tabs(&mut app, 3);
            source.update(&mut app, |workspace, ctx| {
                if tags {
                    // Tagging floats each, in order.
                    for id in tab_ids(workspace)[..2].to_vec() {
                        workspace.handle_action(&emoji(id, "🔥", true), ctx);
                    }
                } else {
                    workspace.pin_tab(0, ctx);
                    workspace.pin_tab(1, ctx);
                }
            });
            target.update(&mut app, |workspace, ctx| workspace.pin_tab(0, ctx));

            // Each move takes the source's first tab: (tagged, drop slot,
            // where it lands and how long the target's floating block is then,
            // with tags on and with them off).
            for (tagged, slot, with_tags, without_tags) in [
                (true, 3, (1, 2), (3, 1)),
                (true, 0, (2, 3), (1, 1)),
                (false, 5, (5, 3), (5, 1)),
            ] {
                let case = format!("tags {tags}, tagged {tagged}, slot {slot}");
                let (expected_landing, block) = if tags { with_tags } else { without_tags };
                let (moved, landed) = move_tab_to_other_window(&mut app, &source, 0, &target, slot);
                target.read(&app, |workspace, _| {
                    assert_eq!(landed, expected_landing, "{case}");
                    assert_eq!(workspace.tabs[landed].pane_group.id(), moved, "{case}");
                    assert_eq!(workspace.active_tab_index, landed, "{case}");
                    assert_eq!(workspace.tabs[landed].pinned, tags && tagged, "{case}");
                    let worn: Vec<String> = if tags && tagged {
                        vec!["🔥".to_owned()]
                    } else {
                        vec![]
                    };
                    assert_eq!(workspace.tabs[landed].tags.to_vec(), worn, "{case}");
                    assert_eq!(
                        workspace.pinned_boundary_index(&workspace.tabs),
                        block,
                        "{case}"
                    );
                    assert!(
                        workspace.tabs[block..].iter().all(|tab| !tab.pinned),
                        "{case}: the floating tabs stay one block at the top"
                    );
                });
            }
        });
    }
}

/// Moved into a window of its own, a tagged tab keeps its emoji and its float
/// with tags on, and arrives plain and unpinned with them off, as upstream's
/// does.
#[test]
fn a_tagged_tab_moved_to_a_new_window_keeps_its_emoji() {
    for tags in [true, false] {
        let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
        let _stars = FeatureFlag::StarredTabs.override_enabled(tags);
        App::test((), |mut app| async move {
            initialize_app(&mut app);
            let source = workspace_with_tabs(&mut app, 2);
            source.update(&mut app, |workspace, ctx| {
                workspace.pin_tab(1, ctx);
                let id = workspace.tabs[0].pane_group.id();
                workspace.handle_action(&emoji(id, "🧪", true), ctx);
            });
            for (index, tagged) in [(0, true), (1, false)] {
                let transferred = source
                    .read(&app, |workspace, ctx| {
                        workspace.get_tab_transfer_info(index, ctx)
                    })
                    .expect("the source has two tabs");
                assert_eq!(transferred.pinned, tagged, "the snapshot carries the float");
                assert_eq!(
                    transferred.tags,
                    if tags && tagged {
                        vec!["🧪".to_owned()]
                    } else {
                        vec![]
                    },
                    "the snapshot carries the emoji"
                );

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
                            tags: transferred.tags.clone(),
                        },
                        ctx,
                    )
                });
                window.read(&app, |workspace, _| {
                    assert_eq!(workspace.tabs.len(), 1);
                    let case = format!("tags {tags}, tagged {tagged}");
                    assert_eq!(workspace.tabs[0].pinned, tags && tagged, "{case}");
                    assert_eq!(workspace.tabs[0].tags.to_vec(), transferred.tags, "{case}");
                });
            }
        });
    }
}

/// The action behind the emoji item of the menu for the tab at `index`.
fn emoji_item_action(workspace: &Workspace, index: usize, ctx: &AppContext) -> WorkspaceAction {
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
            MenuItem::Item(fields) if fields.label().ends_with("emoji…") => {
                fields.on_select_action().cloned()
            }
            _ => None,
        })
        .expect("the tab menu has an emoji item")
}

/// An emoji acts on the tab its action names, wherever that tab has moved,
/// and so does the menu's picker; asking for what a tab already wears, or
/// naming a tab that has since closed, changes nothing. A tab floats as its
/// first emoji goes on and sinks just past the block as its last comes off.
#[test]
fn an_emoji_follows_its_tab_and_repeating_it_changes_nothing() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(true);

    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = workspace_with_tabs(&mut app, 4);
        workspace.update(&mut app, |workspace, ctx| {
            let [a, b, c, d] = <[EntityId; 4]>::try_from(tab_ids(workspace)).unwrap();

            workspace.handle_action(&emoji(c, "🔥", true), ctx);
            assert_eq!(tab_ids(workspace), [c, a, b, d]);
            assert!(workspace.tabs[0].pinned);
            assert_eq!(workspace.tabs[0].tags.to_vec(), ["🔥"]);

            // Again, and taking off an emoji a tab doesn't wear: no change.
            workspace.handle_action(&emoji(c, "🔥", true), ctx);
            workspace.handle_action(&emoji(d, "🔥", false), ctx);
            assert_eq!(tab_ids(workspace), [c, a, b, d]);
            assert_eq!(workspace.starred_boundary(), 1);

            // A menu opened on d, then d moved up before the click: the click
            // opens the picker for d, and a pick there tags d.
            let click = emoji_item_action(workspace, 3, ctx);
            workspace.handle_action(&WorkspaceAction::MoveTabLeft(3), ctx);
            assert_eq!(tab_ids(workspace), [c, a, d, b]);
            workspace.handle_action(&click, ctx);
            assert_eq!(
                workspace
                    .tab_emoji_picker_target
                    .map(|target| target.pane_group_id),
                Some(d)
            );
            workspace.handle_tab_emoji_picker_event(
                &TabEmojiPickerEvent::SetTag {
                    emoji: "✅",
                    present: true,
                },
                ctx,
            );
            assert_eq!(tab_ids(workspace), [c, d, a, b]);
            assert_eq!(workspace.starred_boundary(), 2);
            // The picker stays open for the next pick, until it's closed.
            assert!(workspace.tab_emoji_picker_target.is_some());
            workspace.handle_tab_emoji_picker_event(&TabEmojiPickerEvent::Closed, ctx);
            assert!(workspace.tab_emoji_picker_target.is_none());

            // c's last emoji comes off: it sinks to just past the block.
            workspace.handle_action(&emoji(c, "🔥", false), ctx);
            assert_eq!(tab_ids(workspace), [d, c, a, b]);
            assert_eq!(workspace.starred_boundary(), 1);
            assert!(workspace.tabs[1].tags.to_vec().is_empty());

            // A tab that has since closed.
            workspace.handle_action(&WorkspaceAction::CloseTab(3), ctx);
            assert_eq!(tab_ids(workspace), [d, c, a]);
            workspace.handle_action(&emoji(b, "🔥", true), ctx);
            assert_eq!(tab_ids(workspace), [d, c, a]);
            assert_eq!(workspace.starred_boundary(), 1);
        });
    });
}

/// The active tab stays active as its first emoji floats it to the top and its
/// last sinks it again.
#[test]
fn a_tagged_active_tab_stays_active_as_it_floats_and_sinks() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(true);

    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = workspace_with_tabs(&mut app, 4);
        workspace.update(&mut app, |workspace, ctx| {
            workspace.handle_action(&WorkspaceAction::ActivateTab(2), ctx);
            let active = workspace.tabs[2].pane_group.id();

            workspace.handle_action(&emoji(active, "🔥", true), ctx);
            assert_eq!(workspace.tabs[0].pane_group.id(), active);
            assert!(workspace.tabs[0].pinned);
            assert_eq!(workspace.active_tab_index, 0);

            workspace.handle_action(&emoji(active, "🔥", false), ctx);
            assert!(workspace.tabs.iter().all(|tab| !tab.pinned));
            assert_eq!(
                workspace.tabs[workspace.active_tab_index].pane_group.id(),
                active
            );
        });
    });
}

/// With pins on but tags off, the emoji action and the picker do nothing.
#[test]
fn the_emoji_actions_do_nothing_with_tags_off() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(false);

    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = workspace_with_tabs(&mut app, 3);
        workspace.update(&mut app, |workspace, ctx| {
            let before = tab_ids(workspace);
            workspace.handle_action(&emoji(before[2], "🔥", true), ctx);
            workspace.handle_action(
                &WorkspaceAction::ToggleTabEmojiPicker {
                    pane_group_id: before[2],
                },
                ctx,
            );
            assert_eq!(tab_ids(workspace), before);
            assert!(workspace
                .tabs
                .iter()
                .all(|tab| !tab.pinned && tab.tags.is_empty()));
            assert!(workspace.tab_emoji_picker_target.is_none());
        });
    });
}

/// With "Float tagged tabs to the top" off, emoji are labels: tabs keep their
/// places as emoji go on and off. Turning it on floats every tagged tab at
/// once, in list order; turning it off settles every floating tab where it
/// is, still wearing its emoji, and a tab that floated with none of its own,
/// as an old star does, keeps a ⭐.
#[test]
fn the_float_setting_decides_whether_tagged_tabs_float() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(true);

    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = workspace_with_tabs(&mut app, 4);
        set_float_tagged_tabs(&mut app, false);
        let [a, b, c, d] = workspace.read(&app, |workspace, _| {
            <[EntityId; 4]>::try_from(tab_ids(workspace)).unwrap()
        });
        workspace.update(&mut app, |workspace, ctx| {
            workspace.handle_action(&emoji(c, "🔥", true), ctx);
            workspace.handle_action(&emoji(d, "✅", true), ctx);
            assert_eq!(tab_ids(workspace), [a, b, c, d], "labels move nothing");
            assert!(workspace.tabs.iter().all(|tab| !tab.pinned));
            assert_eq!(workspace.starred_boundary(), 0);
        });

        set_float_tagged_tabs(&mut app, true);
        workspace.update(&mut app, |workspace, ctx| {
            assert_eq!(tab_ids(workspace), [c, d, a, b], "both float, in order");
            assert_eq!(workspace.starred_boundary(), 2);
            // An old star: a tab floating with no emoji of its own.
            workspace.pin_tab(2, ctx);
            assert_eq!(tab_ids(workspace), [c, d, a, b]);
        });

        set_float_tagged_tabs(&mut app, false);
        workspace.read(&app, |workspace, _| {
            assert!(workspace.tabs.iter().all(|tab| !tab.pinned), "none floats");
            assert_eq!(
                tab_ids(workspace),
                [c, d, a, b],
                "each settles where it was"
            );
            assert_eq!(
                tab_tags(workspace),
                [
                    vec!["🔥".to_owned()],
                    vec!["✅".to_owned()],
                    vec![STAR.to_owned()],
                    vec![],
                ]
            );
        });
    });
}

/// A tab in a group wears emoji like any other and stays with its group; once
/// it leaves the group it floats.
#[test]
fn a_grouped_tab_keeps_its_group_and_floats_once_it_leaves() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(true);
    let _groups = FeatureFlag::GroupedTabs.override_enabled(true);

    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = workspace_with_tabs(&mut app, 3);
        workspace.update(&mut app, |workspace, ctx| {
            let c = workspace.tabs[2].pane_group.id();
            workspace.handle_action(&WorkspaceAction::NewTabGroupFromTab(2), ctx);
            workspace.handle_action(&emoji(c, "🔥", true), ctx);
            let index = workspace.tab_index_of(c).expect("c is open");
            let tab = &workspace.tabs[index];
            assert!(tab.group_id.is_some(), "it stays in its group");
            assert!(!tab.pinned, "a group member doesn't float on its own");
            assert_eq!(tab.tags.to_vec(), ["🔥"]);

            workspace.handle_action(&WorkspaceAction::RemoveTabFromGroup(index), ctx);
            assert_eq!(tab_ids(workspace)[0], c, "out of the group, it floats");
            assert!(workspace.tabs[0].pinned && workspace.tabs[0].group_id.is_none());
            assert_eq!(workspace.tabs[0].tags.to_vec(), ["🔥"]);
        });
    });
}

/// The palette's bulk closes say "(keep tagged)" exactly when they would spare
/// a floating tab.
#[test]
fn the_palette_says_what_the_bulk_close_keys_will_do() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(true);

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
            assert_eq!(others(workspace).as_deref(), Some("close other tabs"));
            assert_eq!(below(workspace), None, "nothing below the last tab");

            // The active tab tagged: it floats to the top.
            let active = workspace.tabs[3].pane_group.id();
            workspace.handle_action(&emoji(active, "🔥", true), ctx);
            assert_eq!(others(workspace).as_deref(), Some("close other tabs"));
            assert_eq!(below(workspace).as_deref(), Some("close tabs below"));

            // A second tagged tab, below the active one.
            let second = workspace.tabs[1].pane_group.id();
            workspace.handle_action(&emoji(second, "✅", true), ctx);
            assert_eq!(
                others(workspace).as_deref(),
                Some("close other tabs (keep tagged)")
            );
            assert_eq!(
                below(workspace).as_deref(),
                Some("close tabs below (keep tagged)")
            );
        });
    });
}

/// Every bulk close, from every tab of every list of up to five tabs with every
/// length of floating block: it closes exactly the other tabs in its range, and
/// never a floating one.
#[test]
fn bulk_closes_never_close_a_floating_tab() {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(true);

    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let mut cases = 0;
        for tab_count in 1..=5 {
            for tagged in 0..=tab_count {
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
                            for id in &ids[..tagged] {
                                workspace.handle_action(&emoji(*id, "🔥", true), ctx);
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
                                    *i < tagged || *i == index || (!closes_both_sides && *i < index)
                                })
                                .map(|(_, id)| *id)
                                .collect();
                            workspace.handle_action(&close, ctx);
                            assert_eq!(
                                tab_ids(workspace),
                                survivors,
                                "{close:?} from tab {index} of {tab_count}, {tagged} tagged"
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

const SWEEP_EMOJI: [&str; 4] = ["🔥", "✅", "🧪", "💤"];

/// One step of the float sweep: everything that tags, moves, adds, closes,
/// reopens, groups or jumps between tabs, and the float setting itself.
#[derive(Clone, Copy, Debug)]
enum Step {
    Tag(usize, &'static str),
    Untag(usize, &'static str),
    ToggleFloat,
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
        let emoji = SWEEP_EMOJI[rng.below(SWEEP_EMOJI.len())];
        match rng.below(21) {
            0 | 1 => Step::Tag(tab, emoji),
            2 => Step::Untag(tab, emoji),
            3 => Step::ToggleFloat,
            4 => Step::Activate(tab),
            5 => Step::MoveUp(tab),
            6 => Step::MoveDown(tab),
            7 if tab_count < 7 => Step::Add,
            8 if tab_count > 1 => Step::Close(tab),
            9 => Step::Reopen,
            10 => Step::NewGroup(tab),
            11 => Step::JoinGroup(tab),
            12 => Step::LeaveGroup(tab),
            13 => Step::StarGroup(tab),
            14 => Step::UnstarGroup(tab),
            15 => Step::CloseOthers(tab),
            16 => Step::CloseBelow(tab),
            17 => Step::CloseNonActive,
            18 => Step::CloseBelowActive,
            19 => Step::MarkUnread(tab),
            20 => Step::JumpToNextUnread,
            _ => Step::Tag(tab, emoji),
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
            Step::Tag(index, tag) => emoji(id(index), tag, true),
            Step::Untag(index, tag) => emoji(id(index), tag, false),
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
            Step::Add | Step::Reopen | Step::ToggleFloat => return None,
        })
    }
}

/// What the float sweep checks a step against.
struct Before {
    active: EntityId,
    floating: Vec<EntityId>,
    float: bool,
}

/// Seeded runs of 200 steps each: after every step the floating tabs are one
/// block at the top of the list; no tab both floats and is grouped; an
/// ungrouped tab that wears emoji floats while the setting is on, and none
/// floats with it off; no tab wears more than three; the active tab is the same
/// tab unless the step closed it or was meant to move focus; and no bulk close
/// took a floating tab with it.
#[test]
fn tagged_tabs_stay_one_block_at_the_top_through_any_sequence() {
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
                let float = app.read(|ctx| *TabSettings::as_ref(ctx).float_tagged_tabs.value());
                let (step, before) = workspace.read(&app, |workspace, _| {
                    let step = Step::random(&mut rng, workspace.tabs.len());
                    let before = Before {
                        active: workspace.tabs[workspace.active_tab_index].pane_group.id(),
                        floating: workspace
                            .tabs
                            .iter()
                            .filter(|tab| workspace.is_tab_effectively_pinned(tab))
                            .map(|tab| tab.pane_group.id())
                            .collect(),
                        float,
                    };
                    (step, before)
                });
                match step {
                    Step::Add => workspace.update(&mut app, |workspace, ctx| {
                        workspace.add_terminal_tab(false, ctx);
                    }),
                    Step::Reopen => UndoCloseStack::handle(&app)
                        .update(&mut app, |stack, ctx| stack.undo_close(ctx)),
                    Step::ToggleFloat => set_float_tagged_tabs(&mut app, !before.float),
                    _ => workspace.update(&mut app, |workspace, ctx| {
                        if let Some(action) = step.action(workspace) {
                            workspace.handle_action(&action, ctx);
                        }
                    }),
                }
                let float = app.read(|ctx| *TabSettings::as_ref(ctx).float_tagged_tabs.value());
                workspace.read(&app, |workspace, _| {
                    let context = format!("seed {seed}, step {step_number} ({step:?})");
                    let ids = tab_ids(workspace);
                    assert!(
                        !ids.is_empty() && workspace.active_tab_index < ids.len(),
                        "{context}"
                    );
                    let floating: Vec<bool> = workspace
                        .tabs
                        .iter()
                        .map(|tab| workspace.is_tab_effectively_pinned(tab))
                        .collect();
                    assert!(
                        floating.windows(2).all(|pair| pair[0] || !pair[1]),
                        "{context}: the floating tabs must lead the list as one block, \
                         got {floating:?}"
                    );
                    for tab in &workspace.tabs {
                        assert!(
                            !(tab.pinned && tab.group_id.is_some()),
                            "{context}: a tab both floats and is grouped"
                        );
                        assert!(tab.tags.len() <= 3, "{context}: {:?}", tab.tags);
                        if tab.group_id.is_none() && !matches!(step, Step::Reopen) {
                            if float {
                                assert!(
                                    tab.tags.is_empty() || tab.pinned,
                                    "{context}: a tagged, ungrouped tab floats while the \
                                     setting is on ({:?})",
                                    tab.tags
                                );
                            } else {
                                assert!(
                                    !tab.pinned,
                                    "{context}: nothing floats while the setting is off ({:?})",
                                    tab.tags
                                );
                            }
                        }
                    }
                    let active = ids[workspace.active_tab_index];
                    if step.keeps_the_active_tab()
                        || (step.closes() && ids.contains(&before.active))
                    {
                        assert_eq!(active, before.active, "{context}: the active tab changed");
                    }
                    if step.bulk_closes() {
                        assert!(
                            before.floating.iter().all(|id| ids.contains(id)),
                            "{context}: a bulk close took a floating tab"
                        );
                    }
                });
            }
        });
    }
}
