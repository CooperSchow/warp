//! A compact, self-contained HSV color picker popover used by the tab-color
//! settings widgets (custom palette + Claude auto-color rules).
//!
//! Layout: a saturation/value square with a draggable crosshair, a hue slider
//! over an exact six-segment rainbow (HSV hue is piecewise linear, so six
//! 2-stop gradients render it precisely), a live preview swatch, and an
//! editable hex field that stays in sync both ways. The popover only *selects*
//! a color — what "Save"/"Remove" mean is up to the parent, which subscribes
//! to [`ColorPickerPopoverEvent`].

use std::sync::atomic::{AtomicUsize, Ordering};

use pathfinder_color::ColorU;
use pathfinder_geometry::rect::RectF;
use warp_core::ui::color::hex_color::{coloru_from_hex_string, coloru_to_hex_string};
use warpui::elements::{
    AnchorPair, Border, ChildView, ConstrainedBox, Container, CornerRadius, CrossAxisAlignment,
    Dismiss, Draggable, DraggableState, DropShadow, Element, Expanded, Flex, Hoverable,
    MainAxisAlignment, MainAxisSize, MouseStateHandle, OffsetPositioning, OffsetType,
    ParentElement, PositionedElementOffsetBounds, PositioningAxis, Radius, Rect, SavePosition,
    Stack, XAxisAnchor, YAxisAnchor,
};
use warpui::geometry::vector::vec2f;
use warpui::platform::Cursor;
use warpui::ui_components::components::{UiComponent, UiComponentStyles};
use warpui::ui_components::slider::SliderStateHandle;
use warpui::{
    AppContext, Entity, SingletonEntity, TypedActionView, View, ViewContext, ViewHandle,
};

use crate::appearance::Appearance;
use crate::editor::{
    EditOrigin, EditorView, Event as EditorEvent, SingleLineEditorOptions, TextOptions,
};
use crate::ui_components::hsv::{hsv_to_rgb, rgb_to_hsv};
use crate::view_components::action_button::{ActionButton, PrimaryTheme, SecondaryTheme};

/// Width of the saturation/value square (and of every row below it).
const SV_WIDTH: f32 = 208.;
/// Height of the saturation/value square.
const SV_HEIGHT: f32 = 130.;
/// Diameter of the crosshair on the saturation/value square.
const CROSSHAIR_SIZE: f32 = 14.;
/// Diameter of the hue slider thumb.
const HUE_THUMB_SIZE: f32 = 14.;
/// Height of the hue rainbow track.
const HUE_TRACK_HEIGHT: f32 = 10.;
/// Inner padding of the popover panel.
const PANEL_PADDING: f32 = 12.;

/// Unique SavePosition-id counter so multiple pickers never collide.
static PICKER_ID_COUNT: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, PartialEq)]
pub(super) enum ColorPickerPopoverAction {
    /// Hue changed via the hue slider (degrees, `[0, 360]`).
    SetHue(f32),
    /// Saturation/value changed via the SV square (both `[0, 1]`).
    SetSv { sat: f32, val: f32 },
    /// The Save button (or Enter in the hex field) was pressed.
    Confirm,
    /// The Remove button was pressed.
    Remove,
    /// The user clicked outside the popover.
    Dismiss,
}

pub(super) enum ColorPickerPopoverEvent {
    /// The user confirmed the current color (normalized `#rrggbb`).
    Submitted(String),
    /// The user asked to remove the color being edited.
    RemoveRequested,
    /// The user clicked away; the parent should close the popover.
    DismissRequested,
}

/// See the module docs. Owned by the appearance settings page and rendered as
/// an anchored overlay child by whichever widget currently has it open.
pub(super) struct ColorPickerPopover {
    hue: f32,
    sat: f32,
    val: f32,
    hex_editor: ViewHandle<EditorView>,
    hue_slider_state: SliderStateHandle,
    sv_mouse_state: MouseStateHandle,
    crosshair_drag_state: DraggableState,
    confirm_button: ViewHandle<ActionButton>,
    remove_button: ViewHandle<ActionButton>,
    /// Whether the Remove button is rendered (true when editing an existing
    /// color, false when adding a new one).
    allow_remove: bool,
    /// SavePosition id of the SV square, unique per picker instance.
    sv_position_id: String,
}

impl ColorPickerPopover {
    pub(super) fn new(ctx: &mut ViewContext<Self>) -> Self {
        let editor_options = SingleLineEditorOptions {
            text: TextOptions::ui_font_size(Appearance::as_ref(ctx)),
            ..Default::default()
        };
        let hex_editor = ctx.add_typed_action_view(|ctx| {
            let mut editor = EditorView::single_line(editor_options, ctx);
            editor.system_reset_buffer_text("#502fef", ctx);
            editor
        });
        ctx.subscribe_to_view(&hex_editor, |me, _, event, ctx| {
            me.handle_hex_editor_event(event, ctx);
        });

        let confirm_button = ctx.add_typed_action_view(|_| {
            ActionButton::new("Save", PrimaryTheme).on_click(|ctx| {
                ctx.dispatch_typed_action(ColorPickerPopoverAction::Confirm);
            })
        });
        let remove_button = ctx.add_typed_action_view(|_| {
            ActionButton::new("Remove", SecondaryTheme).on_click(|ctx| {
                ctx.dispatch_typed_action(ColorPickerPopoverAction::Remove);
            })
        });

        let (hue, sat, val) = rgb_to_hsv(0x50, 0x2f, 0xef);
        Self {
            hue,
            sat,
            val,
            hex_editor,
            hue_slider_state: SliderStateHandle::default(),
            sv_mouse_state: MouseStateHandle::default(),
            crosshair_drag_state: DraggableState::default(),
            confirm_button,
            remove_button,
            allow_remove: false,
            sv_position_id: format!(
                "ColorPickerSv{}",
                PICKER_ID_COUNT.fetch_add(1, Ordering::Relaxed)
            ),
        }
    }

    /// Prepares the picker to be shown: seeds it with `initial_hex` (falls back
    /// to the current color when unparsable/absent) and toggles the Remove
    /// button. Call right before rendering the popover open.
    pub(super) fn open_with(
        &mut self,
        initial_hex: Option<&str>,
        allow_remove: bool,
        ctx: &mut ViewContext<Self>,
    ) {
        if let Some(color) = initial_hex.and_then(|hex| coloru_from_hex_string(hex.trim()).ok()) {
            let (hue, sat, val) = rgb_to_hsv(color.r, color.g, color.b);
            self.hue = hue;
            self.sat = sat;
            self.val = val;
        }
        self.allow_remove = allow_remove;
        self.sync_hex_editor(ctx);
        self.hue_slider_state.reset_offset();
        ctx.notify();
    }

    /// The current color as a normalized `#rrggbb` string.
    pub(super) fn current_hex(&self) -> String {
        let (r, g, b) = hsv_to_rgb(self.hue, self.sat, self.val);
        coloru_to_hex_string(&ColorU::new(r, g, b, 255))
    }

    fn current_color(&self) -> ColorU {
        let (r, g, b) = hsv_to_rgb(self.hue, self.sat, self.val);
        ColorU::new(r, g, b, 255)
    }

    /// Rewrites the hex field from the current HSV state (system edit — does
    /// not re-enter [`Self::handle_hex_editor_event`]).
    fn sync_hex_editor(&mut self, ctx: &mut ViewContext<Self>) {
        let hex = self.current_hex();
        self.hex_editor.update(ctx, |editor, ctx| {
            editor.system_reset_buffer_text(&hex, ctx);
        });
    }

    /// Confirms the current color, preferring valid in-progress hex text over
    /// the control state so Enter right after typing a hex applies exactly it.
    fn confirm(&mut self, ctx: &mut ViewContext<Self>) {
        let typed = self.hex_editor.as_ref(ctx).buffer_text(ctx);
        if let Ok(color) = coloru_from_hex_string(typed.trim()) {
            let (hue, sat, val) = rgb_to_hsv(color.r, color.g, color.b);
            self.hue = hue;
            self.sat = sat;
            self.val = val;
        }
        ctx.emit(ColorPickerPopoverEvent::Submitted(self.current_hex()));
    }

    fn handle_hex_editor_event(&mut self, event: &EditorEvent, ctx: &mut ViewContext<Self>) {
        match event {
            EditorEvent::Edited(EditOrigin::UserTyped | EditOrigin::UserInitiated) => {
                let text = self.hex_editor.as_ref(ctx).buffer_text(ctx);
                if let Ok(color) = coloru_from_hex_string(text.trim()) {
                    let (hue, sat, val) = rgb_to_hsv(color.r, color.g, color.b);
                    self.hue = hue;
                    self.sat = sat;
                    self.val = val;
                    // Keep the user's in-progress text; only the controls follow.
                    self.hue_slider_state.reset_offset();
                    ctx.notify();
                }
            }
            EditorEvent::Enter => {
                self.confirm(ctx);
            }
            EditorEvent::Escape | EditorEvent::Blurred => {
                // Normalize back to the effective color (drops invalid text).
                self.sync_hex_editor(ctx);
                ctx.notify();
            }
            _ => {}
        }
    }

    /// The saturation/value square: hue-colored base with the standard
    /// white→hue horizontal and transparent→black vertical gradient overlay,
    /// plus a draggable crosshair. Click anywhere to jump.
    fn render_sv_square(&self, app: &AppContext) -> Box<dyn Element> {
        let (hr, hg, hb) = hsv_to_rgb(self.hue, 1., 1.);
        let hue_color = ColorU::new(hr, hg, hb, 255);
        let corner = CornerRadius::with_all(Radius::Pixels(6.));

        let base = ConstrainedBox::new(
            Stack::new()
                .with_child(
                    Rect::new()
                        .with_horizontal_background_gradient(ColorU::white(), hue_color)
                        .with_corner_radius(corner)
                        .finish(),
                )
                .with_child(
                    Rect::new()
                        .with_background_gradient(
                            vec2f(0., 0.),
                            vec2f(0., 1.),
                            ColorU::transparent_black(),
                            ColorU::black(),
                        )
                        .with_corner_radius(corner)
                        .finish(),
                )
                .finish(),
        )
        .with_width(SV_WIDTH)
        .with_height(SV_HEIGHT)
        .finish();

        let sv_position_id = self.sv_position_id.clone();
        let square = Hoverable::new(self.sv_mouse_state.clone(), move |_| base)
            .on_mouse_down({
                let sv_position_id = self.sv_position_id.clone();
                move |ctx, _, position| {
                    let Some(square) = ctx.element_position_by_id(sv_position_id.as_str()) else {
                        return;
                    };
                    let sat = ((position.x() - square.origin_x()) / square.width()).clamp(0., 1.);
                    let val =
                        1. - ((position.y() - square.origin_y()) / square.height()).clamp(0., 1.);
                    ctx.dispatch_typed_action(ColorPickerPopoverAction::SetSv { sat, val });
                }
            })
            .with_cursor(Cursor::PointingHand);

        let mut stack = Stack::new();
        stack.add_child(SavePosition::new(square.finish(), &sv_position_id).finish());

        // Crosshair: positioned from state, draggable within the square.
        let crosshair = ConstrainedBox::new(
            Rect::new()
                .with_background_color(self.current_color())
                .with_border(Border::all(2.).with_border_color(ColorU::white()))
                .with_corner_radius(CornerRadius::with_all(Radius::Percentage(50.)))
                .with_drop_shadow(DropShadow {
                    color: ColorU::new(0, 0, 0, 90),
                    offset: vec2f(0., 1.),
                    blur_radius: 4.,
                    spread_radius: 0.,
                })
                .finish(),
        )
        .with_width(CROSSHAIR_SIZE)
        .with_height(CROSSHAIR_SIZE)
        .finish();

        let mut crosshair_draggable = Draggable::new(self.crosshair_drag_state.clone(), crosshair)
            .with_drag_threshold(0.)
            .with_drag_bounds_callback({
                let sv_position_id = self.sv_position_id.clone();
                move |position_cache, _| {
                    position_cache
                        .get_position(sv_position_id.as_str())
                        .map(|square| {
                            // Bounds are for the crosshair's full rect: expand
                            // by half the crosshair on every side so its CENTER
                            // can reach every edge of the square.
                            RectF::new(
                                vec2f(
                                    square.origin_x() - CROSSHAIR_SIZE / 2.,
                                    square.origin_y() - CROSSHAIR_SIZE / 2.,
                                ),
                                vec2f(
                                    square.width() + CROSSHAIR_SIZE,
                                    square.height() + CROSSHAIR_SIZE,
                                ),
                            )
                        })
                }
            });
        crosshair_draggable.set_on_drag({
            let sv_position_id = self.sv_position_id.clone();
            move |ctx, _, crosshair_rect, _| {
                let Some(square) = ctx.element_position_by_id(sv_position_id.as_str()) else {
                    return;
                };
                let center_x = crosshair_rect.origin_x() + CROSSHAIR_SIZE / 2.;
                let center_y = crosshair_rect.origin_y() + CROSSHAIR_SIZE / 2.;
                let sat = ((center_x - square.origin_x()) / square.width()).clamp(0., 1.);
                let val = 1. - ((center_y - square.origin_y()) / square.height()).clamp(0., 1.);
                ctx.dispatch_typed_action(ColorPickerPopoverAction::SetSv { sat, val });
            }
        });

        stack.add_positioned_child(
            crosshair_draggable.finish(),
            OffsetPositioning::from_axes(
                PositioningAxis::relative_to_stack_child(
                    &sv_position_id,
                    PositionedElementOffsetBounds::Unbounded,
                    OffsetType::Pixel(self.sat * SV_WIDTH - CROSSHAIR_SIZE / 2.),
                    AnchorPair::new(XAxisAnchor::Left, XAxisAnchor::Left),
                ),
                PositioningAxis::relative_to_stack_child(
                    &sv_position_id,
                    PositionedElementOffsetBounds::Unbounded,
                    OffsetType::Pixel((1. - self.val) * SV_HEIGHT - CROSSHAIR_SIZE / 2.),
                    AnchorPair::new(YAxisAnchor::Top, YAxisAnchor::Top),
                ),
            ),
        );

        let _ = app;
        stack.finish()
    }

    /// The hue slider: an exact six-segment HSV rainbow with a transparent
    /// slider (thumb + interaction) layered on top, geometry-matched to it.
    fn render_hue_slider(&self, app: &AppContext) -> Box<dyn Element> {
        let appearance = Appearance::as_ref(app);

        // Six exact hue segments: 0°→60°→120°→180°→240°→300°→360°.
        let mut rainbow = Flex::row().with_main_axis_size(MainAxisSize::Max);
        for segment in 0..6 {
            let start = hsv_to_rgb(segment as f32 * 60., 1., 1.);
            let end = hsv_to_rgb((segment + 1) as f32 * 60., 1., 1.);
            rainbow.add_child(
                Expanded::new(
                    1.,
                    Rect::new()
                        .with_horizontal_background_gradient(
                            ColorU::new(start.0, start.1, start.2, 255),
                            ColorU::new(end.0, end.1, end.2, 255),
                        )
                        .finish(),
                )
                .finish(),
            );
        }
        // Mirror the slider track's internal geometry (thumb_size/2 horizontal
        // padding, vertical padding up to the thumb height) so the rainbow
        // sits exactly under the transparent track.
        let rainbow_track = Container::new(
            ConstrainedBox::new(
                Container::new(
                    ConstrainedBox::new(
                        Container::new(rainbow.finish())
                            .with_corner_radius(CornerRadius::with_all(Radius::Percentage(50.)))
                            .finish(),
                    )
                    .with_height(HUE_TRACK_HEIGHT)
                    .finish(),
                )
                .with_padding_left(HUE_THUMB_SIZE / 2.)
                .with_padding_right(HUE_THUMB_SIZE / 2.)
                .finish(),
            )
            .with_width(SV_WIDTH)
            .finish(),
        )
        .with_padding_top((HUE_THUMB_SIZE - HUE_TRACK_HEIGHT).max(0.) / 2.)
        .with_padding_bottom((HUE_THUMB_SIZE - HUE_TRACK_HEIGHT).max(0.) / 2.)
        .finish();

        let slider = appearance
            .ui_builder()
            .slider(self.hue_slider_state.clone())
            .with_range(0.0..360.)
            .with_default_value(self.hue)
            .with_thumb_size(HUE_THUMB_SIZE)
            .with_track_height(HUE_TRACK_HEIGHT)
            .with_track_fill(warpui::elements::Fill::None)
            .with_style(UiComponentStyles {
                width: Some(SV_WIDTH),
                ..Default::default()
            })
            .on_drag(|ctx, _, value| {
                ctx.dispatch_typed_action(ColorPickerPopoverAction::SetHue(value));
            })
            .on_change(|ctx, _, value| {
                ctx.dispatch_typed_action(ColorPickerPopoverAction::SetHue(value));
            })
            .build();

        Stack::new()
            .with_child(rainbow_track)
            .with_child(slider.finish())
            .finish()
    }

    /// Preview swatch + hex field, with the Save/Remove buttons on their own
    /// right-aligned row so the panel stays exactly as wide as the SV square.
    fn render_controls_row(&self, app: &AppContext) -> Box<dyn Element> {
        let appearance = Appearance::as_ref(app);
        let theme = appearance.theme();

        let preview = ConstrainedBox::new(
            Rect::new()
                .with_background_color(self.current_color())
                .with_border(Border::all(1.).with_border_fill(theme.outline()))
                .with_corner_radius(CornerRadius::with_all(Radius::Pixels(5.)))
                .finish(),
        )
        .with_width(24.)
        .with_height(24.)
        .finish();

        let hex_field = Container::new(
            ConstrainedBox::new(ChildView::new(&self.hex_editor).finish())
                .with_width(76.)
                .finish(),
        )
        .with_horizontal_padding(6.)
        .with_vertical_padding(3.)
        .with_border(Border::all(1.).with_border_fill(theme.outline()))
        .with_corner_radius(CornerRadius::with_all(Radius::Pixels(5.)))
        .finish();

        let value_row = Flex::row()
            .with_spacing(8.)
            .with_main_axis_size(MainAxisSize::Min)
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_child(preview)
            .with_child(hex_field)
            .finish();

        let mut buttons = Flex::row()
            .with_spacing(6.)
            .with_main_axis_size(MainAxisSize::Max)
            .with_main_axis_alignment(MainAxisAlignment::End)
            .with_cross_axis_alignment(CrossAxisAlignment::Center);
        if self.allow_remove {
            buttons.add_child(ChildView::new(&self.remove_button).finish());
        }
        buttons.add_child(ChildView::new(&self.confirm_button).finish());

        Flex::column()
            .with_spacing(8.)
            .with_main_axis_size(MainAxisSize::Min)
            .with_child(value_row)
            .with_child(buttons.finish())
            .finish()
    }
}

impl Entity for ColorPickerPopover {
    type Event = ColorPickerPopoverEvent;
}

impl View for ColorPickerPopover {
    fn ui_name() -> &'static str {
        "ColorPickerPopover"
    }

    fn render(&self, app: &AppContext) -> Box<dyn Element> {
        let appearance = Appearance::as_ref(app);
        let theme = appearance.theme();

        let content = Flex::column()
            .with_spacing(10.)
            .with_child(self.render_sv_square(app))
            .with_child(self.render_hue_slider(app))
            .with_child(self.render_controls_row(app))
            .finish();

        // Hug the content: without an explicit width the overlay child takes
        // the full width of the settings pane.
        let panel = Container::new(
            ConstrainedBox::new(content)
                .with_width(SV_WIDTH)
                .finish(),
        )
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

        Dismiss::new(panel)
            .on_dismiss(|ctx, _| {
                ctx.dispatch_typed_action(ColorPickerPopoverAction::Dismiss);
            })
            .prevent_interaction_with_other_elements()
            .finish()
    }
}

impl TypedActionView for ColorPickerPopover {
    type Action = ColorPickerPopoverAction;

    fn handle_action(&mut self, action: &Self::Action, ctx: &mut ViewContext<Self>) {
        match action {
            ColorPickerPopoverAction::SetHue(hue) => {
                self.hue = hue.clamp(0., 360.);
                self.sync_hex_editor(ctx);
                ctx.notify();
            }
            ColorPickerPopoverAction::SetSv { sat, val } => {
                self.sat = sat.clamp(0., 1.);
                self.val = val.clamp(0., 1.);
                self.sync_hex_editor(ctx);
                ctx.notify();
            }
            ColorPickerPopoverAction::Confirm => {
                self.confirm(ctx);
            }
            ColorPickerPopoverAction::Remove => {
                ctx.emit(ColorPickerPopoverEvent::RemoveRequested);
            }
            ColorPickerPopoverAction::Dismiss => {
                ctx.emit(ColorPickerPopoverEvent::DismissRequested);
            }
        }
    }
}

#[cfg(test)]
#[path = "color_picker_popover_tests.rs"]
mod tests;
