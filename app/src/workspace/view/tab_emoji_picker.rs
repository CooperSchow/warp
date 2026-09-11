//! The emoji picker a tab's hover controls and its menu open, for the emoji the
//! tab wears (see `tab_tags`). A search field; the tab's own emoji, each a
//! click to take off; and every emoji in a grid, grouped as Unicode groups
//! them, after a row of suggestions that leads with the emoji the window's
//! other tabs wear. A click puts an emoji on or takes it off, and the picker
//! stays open for the next, until Escape or a click outside closes it. Enter
//! puts on the search's best match.
//!
//! It opens beside the tab's row, or under its tab in the horizontal tab bar,
//! at a spot fixed as it opens: when tagged tabs float, a tab floats the
//! moment its first emoji goes on, and the picker stays put while it moves.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::OnceLock;

use emojis::{Emoji, EmojiVersion, Group};
use pathfinder_color::ColorU;
use pathfinder_geometry::vector::Vector2F;
use warp_core::ui::theme::color::internal_colors;
use warpui::elements::{
    Align, Border, ChildView, ConstrainedBox, Container, CornerRadius, CrossAxisAlignment, Dismiss,
    DropShadow, Element, Empty, Fill, Flex, Hoverable, MainAxisAlignment, MainAxisSize,
    MouseStateHandle, ParentElement, Radius, Rect, SavePosition, ScrollStateHandle, Scrollable,
    ScrollableElement, ScrollbarWidth, Shrinkable, Stack, Text, UniformList, UniformListState,
};
use warpui::geometry::vector::vec2f;
use warpui::platform::Cursor;
use warpui::text_layout::ClipConfig;
use warpui::{
    AppContext, Entity, EntityId, SingletonEntity, TypedActionView, View, ViewContext, ViewHandle,
    WeakViewHandle,
};

use super::starred_tabs::{starred_tabs_enabled, tagged_tabs_float};
use super::tab_tags::{render_emoji, worn_tags, TabTags, TagSize, MAX_TAB_TAGS};
use super::{PanelPosition, Workspace};
use crate::appearance::Appearance;
use crate::editor::{EditorView, Event as EditorEvent, SingleLineEditorOptions, TextOptions};
use crate::tab::{tab_position_id, uses_vertical_tabs};
use crate::workspace::tab_settings::TabSettings;

/// Emoji per grid row.
const COLUMNS: usize = 8;

/// A grid cell's side.
const CELL_SIZE: f32 = 32.;

/// An emoji in the grid, and each of the tab's own above it.
const GRID_EMOJI: TagSize = TagSize {
    font_size: 20.,
    box_size: 28.,
};

/// The grid's width, which the whole picker takes.
const GRID_WIDTH: f32 = COLUMNS as f32 * CELL_SIZE;

/// Rows the grid shows before it scrolls.
const GRID_ROWS_SHOWN: f32 = 7.;

const PANEL_PADDING: f32 = 8.;

/// Between the picker's parts.
const SECTION_SPACING: f32 = 8.;

/// Between the picker and the row or tab it opened beside.
const PICKER_GAP: f32 = 6.;

/// The picker panel's saved position, for tests that check where it opened.
pub const TAB_EMOJI_PICKER_POSITION_ID: &str = "tab_emoji_picker";

/// The newest emoji the picker offers. macOS has drawn Emoji 16.0 since 15.4;
/// a newer one would be an empty box on a Mac that can't draw it.
const NEWEST_EMOJI: EmojiVersion = EmojiVersion::new(16, 0);

/// What the suggestions offer after the emoji the window's other tabs wear.
const SUGGESTED: [&str; 16] = [
    "⭐", "🔥", "✅", "🚧", "⏳", "👀", "🧪", "💤", "📌", "❗", "💡", "🐛", "🚀", "🎯", "❤️", "🙏",
];

/// Every emoji the picker offers, in Unicode's order, which keeps each group
/// together.
fn catalog() -> &'static [&'static Emoji] {
    static CATALOG: OnceLock<Vec<&'static Emoji>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        emojis::iter()
            .filter(|emoji| emoji.emoji_version() <= NEWEST_EMOJI)
            .collect()
    })
}

fn group_title(group: Group) -> &'static str {
    match group {
        Group::SmileysAndEmotion => "Smileys & Emotion",
        Group::PeopleAndBody => "People & Body",
        Group::AnimalsAndNature => "Animals & Nature",
        Group::FoodAndDrink => "Food & Drink",
        Group::TravelAndPlaces => "Travel & Places",
        Group::Activities => "Activities",
        Group::Objects => "Objects",
        Group::Symbols => "Symbols",
        Group::Flags => "Flags",
    }
}

/// A query as the names spell it: trimmed, lower case, without a shortcode's
/// colons, and with its underscores as spaces.
fn normalize_query(query: &str) -> String {
    query
        .trim()
        .trim_matches(':')
        .trim()
        .to_lowercase()
        .replace('_', " ")
}

/// How well `emoji` matches `query`, lower being better: 0 when its name or a
/// shortcode is the query, 1 when one of them or one of their words starts
/// with it, 2 when one contains it.
fn match_rank(emoji: &Emoji, query: &str) -> Option<u8> {
    std::iter::once(emoji.name().to_lowercase())
        .chain(emoji.shortcodes().map(|code| code.replace('_', " ")))
        .filter_map(|text| {
            if text == query {
                Some(0)
            } else if text.starts_with(query)
                || text.split([' ', '-']).any(|word| word.starts_with(query))
            {
                Some(1)
            } else if text.contains(query) {
                Some(2)
            } else {
                None
            }
        })
        .min()
}

/// The catalog's emoji matching `query`, best first and in Unicode's order
/// among equals. Nothing for an empty query.
pub(super) fn search(query: &str) -> Vec<&'static Emoji> {
    let query = normalize_query(query);
    if query.is_empty() {
        return Vec::new();
    }
    let mut matches: Vec<(u8, usize, &'static Emoji)> = catalog()
        .iter()
        .enumerate()
        .filter_map(|(index, emoji)| match_rank(emoji, &query).map(|rank| (rank, index, *emoji)))
        .collect();
    matches.sort_by_key(|(rank, index, _)| (*rank, *index));
    matches.into_iter().map(|(_, _, emoji)| emoji).collect()
}

/// The suggestions: the emoji `in_use` names first, then the usual ones, each
/// once, two rows' worth.
fn suggestions(in_use: &[String]) -> Vec<&'static Emoji> {
    let mut seen = HashSet::new();
    in_use
        .iter()
        .map(String::as_str)
        .chain(SUGGESTED)
        .filter_map(emojis::get)
        .filter(|emoji| seen.insert(emoji.as_str()))
        .take(2 * COLUMNS)
        .collect()
}

/// One row of the grid. Titles take a row's height too, so every row is the
/// same height and only the rows on screen are drawn.
#[derive(Clone, Debug, PartialEq)]
enum PickerRow {
    Title(&'static str),
    Emoji(Vec<&'static Emoji>),
}

/// The grid's rows: with no query, the suggestions and then every group under
/// its title; with one, the matches, untitled, and no rows when none match.
fn build_rows(query: &str, suggested: &[&'static Emoji]) -> Vec<PickerRow> {
    let mut rows = Vec::new();
    if normalize_query(query).is_empty() {
        push_section(&mut rows, Some("Suggested"), suggested);
        let catalog = catalog();
        let mut start = 0;
        while start < catalog.len() {
            let group = catalog[start].group();
            let end = catalog[start..]
                .iter()
                .position(|emoji| emoji.group() != group)
                .map_or(catalog.len(), |length| start + length);
            push_section(&mut rows, Some(group_title(group)), &catalog[start..end]);
            start = end;
        }
    } else {
        push_section(&mut rows, None, &search(query));
    }
    rows
}

fn push_section(rows: &mut Vec<PickerRow>, title: Option<&'static str>, emoji: &[&'static Emoji]) {
    if emoji.is_empty() {
        return;
    }
    if let Some(title) = title {
        rows.push(PickerRow::Title(title));
    }
    rows.extend(
        emoji
            .chunks(COLUMNS)
            .map(|row| PickerRow::Emoji(row.to_vec())),
    );
}

/// An emoji's name as the footer shows it.
fn display_name(emoji: &Emoji) -> String {
    let mut chars = emoji.name().chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

#[derive(Clone, Debug)]
pub(super) enum TabEmojiPickerAction {
    /// Put this emoji on the tab, or take it off.
    Toggle(&'static Emoji),
    /// Take every emoji off the tab.
    Clear,
    /// The pointer moved onto an emoji, or off it.
    Hover {
        emoji: &'static Emoji,
        hovered: bool,
    },
    /// A click outside the picker.
    Dismiss,
}

pub(super) enum TabEmojiPickerEvent {
    /// Put `emoji` on the tab (`present`), or take it off.
    SetTag { emoji: &'static str, present: bool },
    /// Take every emoji off the tab.
    ClearTags,
    /// Escape or a click outside: the workspace closes the picker.
    Closed,
}

/// Where the emoji picker is open: for the tab that owns `pane_group_id`, at
/// `origin` in the window, which is fixed as it opens so the picker stays put
/// when the tab moves. It opens leftward from `origin` when `leftward`.
#[derive(Clone, Copy, Debug)]
pub(super) struct TabEmojiPickerTarget {
    pub pane_group_id: EntityId,
    pub origin: Vector2F,
    pub leftward: bool,
}

/// See the module docs.
pub(super) struct TabEmojiPicker {
    handle: WeakViewHandle<Self>,
    search_editor: ViewHandle<EditorView>,
    /// The emoji the tab wears, which the picker draws chosen.
    tags: TabTags,
    suggested: Vec<&'static Emoji>,
    rows: Vec<PickerRow>,
    list_state: UniformListState,
    scroll_state: ScrollStateHandle,
    /// Hover states for the grid's cells by row and column, rebuilt with the
    /// rows, and for the tab's own emoji by position.
    cell_mouse_states: RefCell<HashMap<(usize, usize), MouseStateHandle>>,
    own_tag_mouse_states: [MouseStateHandle; MAX_TAB_TAGS],
    clear_mouse_state: MouseStateHandle,
    /// The emoji under the pointer, which the footer names.
    hovered: Option<&'static Emoji>,
}

impl TabEmojiPicker {
    pub(super) fn new(ctx: &mut ViewContext<Self>) -> Self {
        let options = SingleLineEditorOptions {
            // The UI font, as the tab list's own search field has: an editor
            // otherwise draws in the terminal's monospace.
            text: TextOptions::ui_text(Some(12.), Appearance::as_ref(ctx)),
            ..Default::default()
        };
        let search_editor = ctx.add_typed_action_view(|ctx| {
            let mut editor = EditorView::single_line(options, ctx);
            editor.set_placeholder_text("Search emoji", ctx);
            editor
        });
        ctx.subscribe_to_view(&search_editor, |picker, _, event, ctx| {
            picker.handle_search_event(event, ctx);
        });
        Self {
            handle: ctx.handle(),
            search_editor,
            tags: TabTags::default(),
            suggested: suggestions(&[]),
            rows: Vec::new(),
            list_state: UniformListState::new(),
            scroll_state: ScrollStateHandle::default(),
            cell_mouse_states: RefCell::default(),
            own_tag_mouse_states: Default::default(),
            clear_mouse_state: MouseStateHandle::default(),
            hovered: None,
        }
    }

    /// Readies the picker for a tab wearing `tags`, its suggestions led by
    /// `in_use`, with an empty search that has the keyboard.
    pub(super) fn open(&mut self, tags: TabTags, in_use: &[String], ctx: &mut ViewContext<Self>) {
        self.tags = tags;
        self.suggested = suggestions(in_use);
        self.hovered = None;
        self.search_editor.update(ctx, |editor, ctx| {
            editor.system_reset_buffer_text("", ctx);
        });
        self.rebuild_rows("");
        ctx.focus(&self.search_editor);
        ctx.notify();
    }

    /// Draws `tags` as the ones the tab wears.
    pub(super) fn set_tags(&mut self, tags: TabTags, ctx: &mut ViewContext<Self>) {
        self.tags = tags;
        ctx.notify();
    }

    fn rebuild_rows(&mut self, query: &str) {
        self.rows = build_rows(query, &self.suggested);
        self.cell_mouse_states.borrow_mut().clear();
        self.list_state.scroll_to(0);
    }

    fn handle_search_event(&mut self, event: &EditorEvent, ctx: &mut ViewContext<Self>) {
        match event {
            EditorEvent::Edited(_) => {
                let query = self.search_editor.as_ref(ctx).buffer_text(ctx);
                self.rebuild_rows(&query);
                self.hovered = None;
                ctx.notify();
            }
            EditorEvent::Enter => {
                let query = self.search_editor.as_ref(ctx).buffer_text(ctx);
                if let Some(emoji) = search(&query).first().copied() {
                    self.toggle(emoji, ctx);
                    // Ready for the next: the query goes, and the grid is back.
                    self.search_editor.update(ctx, |editor, ctx| {
                        editor.system_reset_buffer_text("", ctx);
                    });
                    self.rebuild_rows("");
                    ctx.notify();
                }
            }
            EditorEvent::Escape => ctx.emit(TabEmojiPickerEvent::Closed),
            _ => {}
        }
    }

    /// Asks for `emoji` to go on the tab, or come off if the tab wears it.
    /// Nothing goes onto a full tab.
    fn toggle(&mut self, emoji: &'static Emoji, ctx: &mut ViewContext<Self>) {
        let present = !self.tags.contains(emoji.as_str());
        if present && self.tags.is_full() {
            return;
        }
        ctx.emit(TabEmojiPickerEvent::SetTag {
            emoji: emoji.as_str(),
            present,
        });
    }

    fn render_search_field(&self, appearance: &Appearance) -> Box<dyn Element> {
        let theme = appearance.theme();
        Container::new(ChildView::new(&self.search_editor).finish())
            .with_horizontal_padding(8.)
            .with_vertical_padding(5.)
            .with_background(theme.surface_1())
            .with_border(Border::all(1.).with_border_fill(theme.outline()))
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(5.)))
            .finish()
    }

    /// One emoji's cell: chosen when the tab wears it, and dimmed and inert
    /// when the tab is full and doesn't.
    fn render_cell(
        &self,
        mouse_state: MouseStateHandle,
        emoji: &'static Emoji,
        appearance: &Appearance,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        let font_family = appearance.ui_font_family();
        let worn = self.tags.contains(emoji.as_str());
        let unavailable = !worn && self.tags.is_full();
        let accent = theme.accent();
        let hover_background = internal_colors::fg_overlay_2(theme);
        let dimmer = theme.surface_2().with_opacity(60).into_solid();
        let hoverable = Hoverable::new(mouse_state, move |state| {
            let mut cell = Container::new(
                Align::new(render_emoji(emoji.as_str(), GRID_EMOJI, font_family)).finish(),
            )
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(6.)));
            if worn {
                cell = cell
                    .with_background(accent.with_opacity(25))
                    .with_border(Border::all(1.).with_border_fill(accent));
            } else if state.is_hovered() && !unavailable {
                cell = cell.with_background(hover_background);
            }
            let cell = ConstrainedBox::new(cell.finish())
                .with_width(CELL_SIZE)
                .with_height(CELL_SIZE)
                .finish();
            if unavailable {
                Stack::new()
                    .with_child(cell)
                    .with_child(
                        ConstrainedBox::new(Rect::new().with_background_color(dimmer).finish())
                            .with_width(CELL_SIZE)
                            .with_height(CELL_SIZE)
                            .finish(),
                    )
                    .finish()
            } else {
                cell
            }
        })
        .on_hover(move |hovered, ctx, _, _| {
            ctx.dispatch_typed_action(TabEmojiPickerAction::Hover { emoji, hovered });
        });
        if unavailable {
            hoverable.finish()
        } else {
            hoverable
                .with_cursor(Cursor::PointingHand)
                .on_click(move |ctx, _, _| {
                    ctx.dispatch_typed_action(TabEmojiPickerAction::Toggle(emoji));
                })
                .finish()
        }
    }

    /// The emoji the tab wears, each a click to take off, and Clear; or, while
    /// it wears none, how many it can. Always a cell tall, so the grid under
    /// it never moves as the first emoji goes on or the last comes off.
    fn render_own_tags(&self, appearance: &Appearance) -> Box<dyn Element> {
        let theme = appearance.theme();
        let font_family = appearance.ui_font_family();
        let sub_text = theme.sub_text_color(theme.surface_2());
        let main_text = theme.main_text_color(theme.surface_2());
        let content: Box<dyn Element> = if self.tags.is_empty() {
            Container::new(
                Text::new_inline(
                    format!("Pick up to {MAX_TAB_TAGS} for this tab"),
                    font_family,
                    12.,
                )
                .with_color(sub_text.into())
                .finish(),
            )
            .with_margin_left(4.)
            .finish()
        } else {
            let mut own = Flex::row()
                .with_main_axis_size(MainAxisSize::Min)
                .with_cross_axis_alignment(CrossAxisAlignment::Center)
                .with_spacing(2.);
            for (position, tag) in self.tags.iter().enumerate() {
                let Some(emoji) = emojis::get(tag) else {
                    continue;
                };
                own.add_child(self.render_cell(
                    self.own_tag_mouse_states[position].clone(),
                    emoji,
                    appearance,
                ));
            }
            let clear = Hoverable::new(self.clear_mouse_state.clone(), move |state| {
                let ink = if state.is_hovered() {
                    main_text
                } else {
                    sub_text
                };
                Container::new(
                    Text::new_inline("Clear", font_family, 12.)
                        .with_color(ink.into())
                        .finish(),
                )
                .with_horizontal_padding(6.)
                .with_vertical_padding(4.)
                .finish()
            })
            .with_cursor(Cursor::PointingHand)
            .on_click(|ctx, _, _| ctx.dispatch_typed_action(TabEmojiPickerAction::Clear))
            .finish();
            Flex::row()
                .with_main_axis_size(MainAxisSize::Max)
                .with_main_axis_alignment(MainAxisAlignment::SpaceBetween)
                .with_cross_axis_alignment(CrossAxisAlignment::Center)
                .with_child(own.finish())
                .with_child(clear)
                .finish()
        };
        ConstrainedBox::new(Align::new(content).left().finish())
            .with_height(CELL_SIZE)
            .finish()
    }

    fn render_row(&self, index: usize, app: &AppContext) -> Box<dyn Element> {
        let appearance = Appearance::as_ref(app);
        let theme = appearance.theme();
        let row = match self.rows.get(index) {
            Some(PickerRow::Title(title)) => Container::new(
                Text::new_inline(*title, appearance.ui_font_family(), 11.)
                    .with_color(theme.sub_text_color(theme.surface_2()).into())
                    .finish(),
            )
            .with_margin_top(14.)
            .with_margin_left(4.)
            .finish(),
            Some(PickerRow::Emoji(emoji)) => {
                let mut cells = Flex::row().with_main_axis_size(MainAxisSize::Min);
                for (column, emoji) in emoji.iter().enumerate() {
                    let mouse_state = self
                        .cell_mouse_states
                        .borrow_mut()
                        .entry((index, column))
                        .or_default()
                        .clone();
                    cells.add_child(self.render_cell(mouse_state, emoji, appearance));
                }
                cells.finish()
            }
            None => Empty::new().finish(),
        };
        ConstrainedBox::new(row)
            .with_width(GRID_WIDTH)
            .with_height(CELL_SIZE)
            .finish()
    }

    fn render_grid(&self, appearance: &Appearance) -> Box<dyn Element> {
        let theme = appearance.theme();
        let height = CELL_SIZE * GRID_ROWS_SHOWN;
        if self.rows.is_empty() {
            return ConstrainedBox::new(
                Align::new(
                    Text::new_inline("No emoji found", appearance.ui_font_family(), 12.)
                        .with_color(theme.sub_text_color(theme.surface_2()).into())
                        .finish(),
                )
                .finish(),
            )
            .with_width(GRID_WIDTH)
            .with_height(height)
            .finish();
        }
        let handle = self.handle.clone();
        let build_rows = move |range: Range<usize>, app: &AppContext| {
            let rows: Vec<Box<dyn Element>> = match handle.upgrade(app) {
                Some(picker) => {
                    let picker = picker.as_ref(app);
                    range.map(|index| picker.render_row(index, app)).collect()
                }
                None => Vec::new(),
            };
            rows.into_iter()
        };
        let list = UniformList::new(self.list_state.clone(), self.rows.len(), build_rows);
        ConstrainedBox::new(
            Scrollable::vertical(
                self.scroll_state.clone(),
                list.finish_scrollable(),
                ScrollbarWidth::Auto,
                theme.nonactive_ui_detail().into(),
                theme.active_ui_detail().into(),
                Fill::None,
            )
            .with_overlayed_scrollbar()
            .finish(),
        )
        .with_width(GRID_WIDTH)
        .with_height(height)
        .finish()
    }

    /// The emoji under the pointer by name, or why nothing more goes on; and
    /// how many the tab wears.
    fn render_footer(&self, appearance: &Appearance) -> Box<dyn Element> {
        let theme = appearance.theme();
        let font_family = appearance.ui_font_family();
        let sub_text = theme.sub_text_color(theme.surface_2());
        let hint = match self.hovered {
            Some(emoji) => display_name(emoji),
            None if self.tags.is_full() => "Take one off to add another".to_owned(),
            None => String::new(),
        };
        let mut footer = Flex::row()
            .with_main_axis_size(MainAxisSize::Max)
            .with_main_axis_alignment(MainAxisAlignment::SpaceBetween)
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_child(
                Shrinkable::new(
                    1.,
                    Text::new_inline(hint, font_family, 11.)
                        .with_clip(ClipConfig::ellipsis())
                        .with_color(sub_text.into())
                        .finish(),
                )
                .finish(),
            );
        if !self.tags.is_empty() {
            footer.add_child(
                Container::new(
                    Text::new_inline(
                        format!("{} of {MAX_TAB_TAGS}", self.tags.len()),
                        font_family,
                        11.,
                    )
                    .with_color(sub_text.into())
                    .finish(),
                )
                .with_margin_left(8.)
                .finish(),
            );
        }
        ConstrainedBox::new(
            Container::new(footer.finish())
                .with_horizontal_padding(4.)
                .finish(),
        )
        .with_height(16.)
        .finish()
    }
}

impl Entity for TabEmojiPicker {
    type Event = TabEmojiPickerEvent;
}

impl View for TabEmojiPicker {
    fn ui_name() -> &'static str {
        "TabEmojiPicker"
    }

    fn render(&self, app: &AppContext) -> Box<dyn Element> {
        let appearance = Appearance::as_ref(app);
        let theme = appearance.theme();
        let content = Flex::column()
            .with_main_axis_size(MainAxisSize::Min)
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_spacing(SECTION_SPACING)
            .with_child(self.render_search_field(appearance))
            .with_child(self.render_own_tags(appearance))
            .with_child(self.render_grid(appearance))
            .with_child(self.render_footer(appearance))
            .finish();
        let panel = Container::new(ConstrainedBox::new(content).with_width(GRID_WIDTH).finish())
            .with_uniform_padding(PANEL_PADDING)
            .with_background(theme.surface_2())
            .with_border(Border::all(1.).with_border_fill(theme.outline()))
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(8.)))
            .with_drop_shadow(DropShadow {
                color: ColorU::new(0, 0, 0, 70),
                offset: vec2f(0., 4.),
                blur_radius: 16.,
                spread_radius: 0.,
            })
            .finish();
        Dismiss::new(SavePosition::new(panel, TAB_EMOJI_PICKER_POSITION_ID).finish())
            .on_dismiss(|ctx, _| {
                ctx.dispatch_typed_action(TabEmojiPickerAction::Dismiss);
            })
            .prevent_interaction_with_other_elements()
            .finish()
    }
}

impl TypedActionView for TabEmojiPicker {
    type Action = TabEmojiPickerAction;

    fn handle_action(&mut self, action: &Self::Action, ctx: &mut ViewContext<Self>) {
        match action {
            TabEmojiPickerAction::Toggle(emoji) => {
                self.toggle(emoji, ctx);
                // A click takes the keyboard nowhere: typing still searches.
                ctx.focus(&self.search_editor);
            }
            TabEmojiPickerAction::Clear => {
                ctx.emit(TabEmojiPickerEvent::ClearTags);
                ctx.focus(&self.search_editor);
            }
            TabEmojiPickerAction::Hover { emoji, hovered } => {
                if *hovered {
                    self.hovered = Some(emoji);
                } else if self
                    .hovered
                    .is_some_and(|current| std::ptr::eq(current, *emoji))
                {
                    self.hovered = None;
                }
                ctx.notify();
            }
            TabEmojiPickerAction::Dismiss => ctx.emit(TabEmojiPickerEvent::Closed),
        }
    }
}

impl Workspace {
    /// The pane group of the tab the emoji picker is open for, if it's open.
    pub fn tab_emoji_picker_tab(&self) -> Option<EntityId> {
        self.tab_emoji_picker_target
            .map(|target| target.pane_group_id)
    }

    /// Opens the emoji picker for the tab that owns `pane_group_id`, or closes
    /// it when it's open for that tab.
    pub(super) fn toggle_tab_emoji_picker(
        &mut self,
        pane_group_id: EntityId,
        ctx: &mut ViewContext<Self>,
    ) {
        let open_for_this_tab = self
            .tab_emoji_picker_target
            .is_some_and(|target| target.pane_group_id == pane_group_id);
        self.close_tab_emoji_picker(ctx);
        if open_for_this_tab || !starred_tabs_enabled() {
            return;
        }
        let Some(index) = self.tab_index_of(pane_group_id) else {
            return;
        };
        // A floating tab with no emoji of its own wears ⭐; the picker shows it
        // as one to take off like any other.
        let tab = &mut self.tabs[index];
        tab.tags = worn_tags(&tab.tags, tab.pinned);
        let tags = tab.tags.clone();
        let in_use: Vec<String> = self
            .tabs
            .iter()
            .filter(|tab| tab.pane_group.id() != pane_group_id)
            .flat_map(|tab| worn_tags(&tab.tags, tab.pinned).to_vec())
            .collect();
        let (origin, leftward) = self.tab_emoji_picker_origin(index, ctx);
        self.tab_emoji_picker.update(ctx, |picker, ctx| {
            picker.open(tags, &in_use, ctx);
        });
        self.tab_emoji_picker_target = Some(TabEmojiPickerTarget {
            pane_group_id,
            origin,
            leftward,
        });
        ctx.notify();
    }

    /// Where the picker opens for the tab at `index`: beside its row, on the
    /// side away from the vertical tabs panel's edge, or under its tab in the
    /// horizontal tab bar. The window's corner when the tab wasn't drawn.
    fn tab_emoji_picker_origin(&self, index: usize, ctx: &ViewContext<Self>) -> (Vector2F, bool) {
        let tab_bounds =
            ctx.element_position_by_id_at_last_frame(self.window_id, tab_position_id(index));
        let Some(bounds) = tab_bounds else {
            return (vec2f(PICKER_GAP, PICKER_GAP), false);
        };
        if !(self.vertical_tabs_panel_open && uses_vertical_tabs(ctx)) {
            return (bounds.lower_left() + vec2f(0., PICKER_GAP), false);
        }
        let panel_side =
            Self::tabs_panel_side(&TabSettings::as_ref(ctx).header_toolbar_chip_selection);
        if panel_side == PanelPosition::Left {
            (bounds.upper_right() + vec2f(PICKER_GAP, 0.), false)
        } else {
            (bounds.origin() - vec2f(PICKER_GAP, 0.), true)
        }
    }

    /// Closes the emoji picker and gives the keyboard back to the active tab.
    pub(super) fn close_tab_emoji_picker(&mut self, ctx: &mut ViewContext<Self>) {
        if self.tab_emoji_picker_target.take().is_none() {
            return;
        }
        self.focus_active_tab(ctx);
        ctx.notify();
    }

    pub(super) fn handle_tab_emoji_picker_event(
        &mut self,
        event: &TabEmojiPickerEvent,
        ctx: &mut ViewContext<Self>,
    ) {
        let Some(target) = self.tab_emoji_picker_target else {
            return;
        };
        match event {
            TabEmojiPickerEvent::SetTag { emoji, present } => {
                self.update_tab_tags(target.pane_group_id, ctx, |tags| tags.set(emoji, *present));
            }
            TabEmojiPickerEvent::ClearTags => {
                self.update_tab_tags(target.pane_group_id, ctx, TabTags::clear);
            }
            TabEmojiPickerEvent::Closed => self.close_tab_emoji_picker(ctx),
        }
    }

    /// Changes the emoji of the tab that owns `pane_group_id` with `change`,
    /// which says whether it changed anything, and saves. When tagged tabs
    /// float, an ungrouped tab floats as its first emoji goes on and sinks as
    /// its last comes off.
    pub(super) fn update_tab_tags(
        &mut self,
        pane_group_id: EntityId,
        ctx: &mut ViewContext<Self>,
        change: impl FnOnce(&mut TabTags) -> bool,
    ) {
        if !starred_tabs_enabled() {
            return;
        }
        let Some(index) = self.tab_index_of(pane_group_id) else {
            return;
        };
        if !change(&mut self.tabs[index].tags) {
            return;
        }
        let tags = self.tabs[index].tags.clone();
        if tagged_tabs_float(ctx) && self.tabs[index].group_id.is_none() {
            self.set_tab_starred(pane_group_id, !tags.is_empty(), ctx);
        }
        self.tab_emoji_picker.update(ctx, |picker, ctx| {
            picker.set_tags(tags, ctx);
        });
        ctx.dispatch_global_action("workspace:save_app", ());
        ctx.notify();
    }
}

#[cfg(test)]
#[path = "tab_emoji_picker_tests.rs"]
mod tests;
