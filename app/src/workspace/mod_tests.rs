use warpui::keymap::Keystroke;
use warpui::platform::OperatingSystem;
use warpui::App;

use crate::features::FeatureFlag;
use crate::util::bindings::trigger_to_keystroke;
use crate::workspace::view::tests::initialize_app;
use crate::workspace::view::{
    JUMP_TO_NEXT_UNREAD_TAB_BINDING_NAME, TOGGLE_ACTIVE_TAB_UNREAD_BINDING_NAME,
};

/// ⌃⌘U and ⌘J each belong to exactly one default binding, the tab mark each
/// was chosen for; they're macOS defaults only, so elsewhere nothing binds
/// them. ⌃⌘S, which starred a tab before emoji tags, binds nothing now.
#[test]
fn tab_mark_keys_each_have_exactly_one_default_binding() {
    // The registry lists only enabled bindings, and these follow their flags.
    let _pins = FeatureFlag::PinnedTabs.override_enabled(true);
    let _stars = FeatureFlag::StarredTabs.override_enabled(true);
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);

    App::test((), |mut app| async move {
        initialize_app(&mut app);
        app.update(|ctx| {
            for (key, binding_name) in [
                ("cmd-ctrl-u", Some(TOGGLE_ACTIVE_TAB_UNREAD_BINDING_NAME)),
                ("cmd-ctrl-s", None),
                ("cmd-j", Some(JUMP_TO_NEXT_UNREAD_TAB_BINDING_NAME)),
            ] {
                let keystroke = Keystroke::parse(key).expect("keystroke should parse");
                let bound: Vec<&str> = ctx
                    .editable_bindings()
                    .filter(|binding| {
                        trigger_to_keystroke(binding.trigger).as_ref() == Some(&keystroke)
                    })
                    .map(|binding| binding.name)
                    .collect();
                let expected: Vec<&str> = if OperatingSystem::get().is_mac() {
                    binding_name.into_iter().collect()
                } else {
                    vec![]
                };
                assert_eq!(bound, expected, "default bindings on {key}");
            }
        });
    });
}
