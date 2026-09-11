use warp_core::ui::appearance::Appearance;
use warpui::elements::Empty;
use warpui::platform::WindowStyle;
use warpui::{App, AppContext, Element, Entity, TypedActionView, View, WindowId};

use super::*;
use crate::auth::AuthStateProvider;
use crate::network::NetworkStatus;
use crate::server::server_api::ServerApiProvider;
use crate::settings::PrivacySettings;
use crate::settings_view::keybindings::KeybindingChangedNotifier;
use crate::test_util::settings::initialize_settings_for_tests;
use crate::workspaces::user_workspaces::UserWorkspaces;

struct TestRootView;

impl Entity for TestRootView {
    type Event = ();
}

impl View for TestRootView {
    fn ui_name() -> &'static str {
        "TestRootView"
    }

    fn render(&self, _: &AppContext) -> Box<dyn Element> {
        Empty::new().finish()
    }
}

impl TypedActionView for TestRootView {
    type Action = ();
}

fn create_test_window(app: &mut App) -> WindowId {
    let (window_id, _root_view) = app.add_window(WindowStyle::NotStealFocus, |_| TestRootView);
    window_id
}

fn init_test_app(app: &mut App) -> WindowId {
    initialize_settings_for_tests(app);

    // Singletons the shared settings widgets (and the embedded editor) expect.
    app.add_singleton_model(|_ctx| ServerApiProvider::new_for_test());
    app.add_singleton_model(|_| AuthStateProvider::new_for_test());
    app.add_singleton_model(|_| Appearance::mock());
    app.add_singleton_model(UserWorkspaces::default_mock);
    app.add_singleton_model(PrivacySettings::mock);
    app.add_singleton_model(|_| NetworkStatus::new());
    app.add_singleton_model(|_| KeybindingChangedNotifier::new());

    create_test_window(app)
}

// Note: the Save/Remove buttons and the hex field are embedded as `ChildView`s
// of their own views, so their labels are not part of this view's element tree
// and cannot be asserted on via `debug_text_content`. These tests therefore
// assert that every mode renders without panicking; the button set itself is
// driven by `allow_remove`, asserted below.

#[test]
fn renders_in_both_edit_and_add_modes() {
    App::test((), |mut app| async move {
        let window_id = init_test_app(&mut app);

        app.update(|ctx| {
            let picker = ctx.add_typed_action_view(window_id, ColorPickerPopover::new);

            // Editing an existing color (Remove offered).
            picker.update(ctx, |picker, ctx| {
                picker.open_with(Some("#502fef"), true, ctx);
            });
            assert!(picker.as_ref(ctx).allow_remove);
            let _ = picker.as_ref(ctx).render(ctx);

            // Adding a new color (no Remove).
            picker.update(ctx, |picker, ctx| {
                picker.open_with(None, false, ctx);
            });
            assert!(!picker.as_ref(ctx).allow_remove);
            let _ = picker.as_ref(ctx).render(ctx);
        });
    });
}

#[test]
fn seeds_hsv_state_from_the_initial_hex() {
    App::test((), |mut app| async move {
        let window_id = init_test_app(&mut app);

        app.update(|ctx| {
            let picker = ctx.add_typed_action_view(window_id, ColorPickerPopover::new);
            picker.update(ctx, |picker, ctx| {
                picker.open_with(Some("#ff8800"), false, ctx);
            });
            assert_eq!(picker.as_ref(ctx).current_hex(), "#ff8800");

            // A short (`#rgb`) hex is accepted and normalized.
            picker.update(ctx, |picker, ctx| {
                picker.open_with(Some("#0f0"), false, ctx);
            });
            assert_eq!(picker.as_ref(ctx).current_hex(), "#00ff00");

            // Garbage leaves the previous color untouched.
            picker.update(ctx, |picker, ctx| {
                picker.open_with(Some("not a color"), false, ctx);
            });
            assert_eq!(picker.as_ref(ctx).current_hex(), "#00ff00");
        });
    });
}

#[test]
fn hue_and_sv_actions_update_the_color() {
    App::test((), |mut app| async move {
        let window_id = init_test_app(&mut app);

        app.update(|ctx| {
            let picker = ctx.add_typed_action_view(window_id, ColorPickerPopover::new);
            picker.update(ctx, |picker, ctx| {
                picker.open_with(Some("#ff0000"), false, ctx);
                // Full saturation and value at 120° is pure green.
                picker.handle_action(&ColorPickerPopoverAction::SetHue(120.), ctx);
                picker.handle_action(&ColorPickerPopoverAction::SetSv { sat: 1., val: 1. }, ctx);
            });
            assert_eq!(picker.as_ref(ctx).current_hex(), "#00ff00");

            // Dragging to the top-left of the SV square is white.
            picker.update(ctx, |picker, ctx| {
                picker.handle_action(&ColorPickerPopoverAction::SetSv { sat: 0., val: 1. }, ctx);
            });
            assert_eq!(picker.as_ref(ctx).current_hex(), "#ffffff");

            // The bottom of the square is black regardless of hue/saturation.
            picker.update(ctx, |picker, ctx| {
                picker.handle_action(&ColorPickerPopoverAction::SetSv { sat: 1., val: 0. }, ctx);
            });
            assert_eq!(picker.as_ref(ctx).current_hex(), "#000000");
        });
    });
}

#[test]
fn out_of_range_drag_values_are_clamped() {
    App::test((), |mut app| async move {
        let window_id = init_test_app(&mut app);

        app.update(|ctx| {
            let picker = ctx.add_typed_action_view(window_id, ColorPickerPopover::new);
            picker.update(ctx, |picker, ctx| {
                picker.open_with(Some("#ff0000"), false, ctx);
                picker.handle_action(&ColorPickerPopoverAction::SetSv { sat: 5., val: -3. }, ctx);
                picker.handle_action(&ColorPickerPopoverAction::SetHue(999.), ctx);
            });
            // Clamped rather than wrapped or panicking.
            assert_eq!(picker.as_ref(ctx).current_hex(), "#000000");
        });
    });
}

#[test]
fn renders_after_every_interaction() {
    // Guards against a control's render path depending on state that only
    // exists before the first interaction.
    App::test((), |mut app| async move {
        let window_id = init_test_app(&mut app);

        app.update(|ctx| {
            let picker = ctx.add_typed_action_view(window_id, ColorPickerPopover::new);
            for (hue, sat, val) in [
                (0., 0., 0.),
                (359.9, 1., 1.),
                (180., 0.5, 0.5),
                (60., 0., 1.),
            ] {
                picker.update(ctx, |picker, ctx| {
                    picker.handle_action(&ColorPickerPopoverAction::SetHue(hue), ctx);
                    picker.handle_action(&ColorPickerPopoverAction::SetSv { sat, val }, ctx);
                });
                let _ = picker.as_ref(ctx).render(ctx);
            }
        });
    });
}
