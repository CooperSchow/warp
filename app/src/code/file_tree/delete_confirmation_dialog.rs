//! Confirmation dialog shown before the Project Explorer permanently deletes a file or folder.

use std::fs::{FileType, Metadata};
use std::io;
#[cfg(any(test, feature = "integration_tests"))]
use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use instant::Instant;
use warp_util::standardized_path::StandardizedPath;
use warpui::elements::{
    Align, ChildView, Container, Dismiss, DispatchEventResult, Empty, EventHandler, ParentElement,
    SavePosition, Stack, Text,
};
use warpui::keymap::{FixedBinding, Keystroke};
use warpui::r#async::{SpawnedFutureHandle, Timer};
use warpui::ui_components::components::{UiComponent, UiComponentStyles};
use warpui::{
    AppContext, BlurContext, Element, Entity, SingletonEntity, TypedActionView, View, ViewContext,
    ViewHandle,
};

use crate::appearance::Appearance;
use crate::ui_components::blended_colors;
use crate::ui_components::dialog::{dialog_styles, Dialog};
use crate::view_components::action_button::{
    ActionButton, DangerPrimaryTheme, KeystrokeSource, SecondaryTheme,
};

pub(crate) fn init(app: &mut AppContext) {
    use warpui::keymap::macros::*;

    // Return, and the keypad's Enter, cancel the same as Escape. Deleting is permanent, so it
    // takes a click on the Delete button: a reflexive Return after a mis-click must never delete
    // anything.
    app.register_fixed_bindings([
        FixedBinding::new(
            "escape",
            DeleteFileConfirmationAction::Cancel,
            id!(DeleteFileConfirmationDialog::ui_name()),
        ),
        FixedBinding::new(
            "enter",
            DeleteFileConfirmationAction::Cancel,
            id!(DeleteFileConfirmationDialog::ui_name()),
        ),
        FixedBinding::new(
            "numpadenter",
            DeleteFileConfirmationAction::Cancel,
            id!(DeleteFileConfirmationDialog::ui_name()),
        ),
    ]);
}

const DIALOG_WIDTH: f32 = 460.;
const PATH_FONT_SIZE: f32 = 12.;

/// How long Delete stays disabled after the dialog appears. The dialog opens on the mouse-up that
/// chooses "Delete…" in the context menu, so the second click of a double-click arrives a moment
/// later, wherever the pointer happens to be. Without a delay it could land on Delete and confirm
/// with no second decision. Firefox holds its dangerous-download button back for the same reason.
const ARM_DELAY: Duration = Duration::from_millis(500);

/// Whether a click on Delete confirms the delete.
///
/// - `opened_at` is when the dialog appeared.
/// - `pressed_at` is when the mouse button went down on Delete while this dialog was showing, or
///   `None` if it didn't. A click whose press began before the dialog appeared, on the context
///   menu for example, has no press here.
/// - `clicked_at` is when the button came back up on Delete, completing the click.
///
/// A click counts only if its press began once Delete was armed, `arm_delay` after the dialog
/// appeared. So a click that completes within the delay never counts, and neither does one whose
/// press began before the dialog appeared, or while Delete was still drawn disabled. Refusing
/// costs at most a second click.
pub(crate) fn click_confirms(
    opened_at: Instant,
    pressed_at: Option<Instant>,
    clicked_at: Instant,
    arm_delay: Duration,
) -> bool {
    let Some(pressed_at) = pressed_at else {
        return false;
    };
    pressed_at >= opened_at + arm_delay && clicked_at >= pressed_at
}

/// Shown when the file tree was rebuilt between opening the context menu and choosing Delete, so
/// the menu's row no longer holds the item the menu was opened on.
pub(crate) const STALE_MENU_MESSAGE: &str =
    "The file tree changed while the menu was open. Nothing was deleted.";

/// The kind of item a delete targets. It comes from `lstat`, so a symbolic link is never
/// mistaken for the item it points to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ItemKind {
    File,
    Directory,
    Symlink,
}

impl ItemKind {
    pub(crate) fn from_file_type(file_type: FileType) -> Self {
        if file_type.is_symlink() {
            ItemKind::Symlink
        } else if file_type.is_dir() {
            ItemKind::Directory
        } else {
            ItemKind::File
        }
    }
}

/// Identifies an item on disk, so a confirm can tell whether its path still names the item the
/// dialog showed. Edits in place keep an item's identity; replacing the item changes it.
///
/// On Unix it is the device and inode, plus the birth time. The inode alone isn't enough: Linux
/// file systems such as ext4 often hand a deleted file's inode to the next file created in the
/// same folder, so deleting a file and creating another under the same name can reuse it. The
/// birth time tells the two apart wherever the file system records one (`st_birthtime` on
/// macOS, `statx` on Linux). Where it doesn't, the check falls back to the inode alone.
///
/// Elsewhere it is best effort: the size, the modification time and the creation time. Windows
/// gives a new file the creation time of a file deleted under the same name moments earlier
/// ("tunneling"), and a copy keeps the original's modification time, so a replacement of the
/// same size can pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ItemIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    len: u64,
    #[cfg(not(unix))]
    modified: Option<SystemTime>,
    /// When the item was created, if the file system records it.
    created: Option<SystemTime>,
}

impl ItemIdentity {
    #[cfg(unix)]
    pub(crate) fn from_metadata(metadata: &Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;

        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            created: metadata.created().ok(),
        }
    }

    #[cfg(not(unix))]
    pub(crate) fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
        }
    }
}

/// The item the delete dialog is asking about. It is captured by path when the dialog opens and
/// is never looked up by list index again, because the file tree rebuilds its list whenever the
/// file watcher reports a change.
#[derive(Clone, Debug)]
pub(crate) struct PendingDelete {
    pub(crate) std_path: StandardizedPath,
    pub(crate) local_path: PathBuf,
    pub(crate) kind: ItemKind,
    pub(crate) display_name: String,
    /// How many items the folder holds directly, according to the in-memory tree. `None` unless
    /// the folder is loaded and not ignored: the dialog never reads the disk to count.
    pub(crate) child_count: Option<usize>,
    pub(crate) identity: ItemIdentity,
}

impl PendingDelete {
    pub(crate) fn title(&self) -> String {
        format!("Delete \"{}\"?", self.display_name)
    }

    pub(crate) fn body(&self) -> String {
        match self.kind {
            ItemKind::File => {
                "This file will be permanently deleted. You can't undo this.".to_owned()
            }
            ItemKind::Directory => {
                let warning = "This folder and everything in it will be permanently deleted. \
                               You can't undo this.";
                match self.child_count {
                    Some(1) => format!("{warning} It contains 1 item."),
                    Some(count) => format!("{warning} It contains {count} items."),
                    None => warning.to_owned(),
                }
            }
            ItemKind::Symlink => "This symbolic link will be permanently deleted. \
                                  The item it points to won't be affected."
                .to_owned(),
        }
    }
}

pub(crate) fn delete_failed_message(display_name: &str, error: &io::Error) -> String {
    format!("Couldn't delete \"{display_name}\": {error}")
}

/// For a folder whose delete stopped partway. Deleting a folder removes what's inside it first,
/// so by the time an error stops it, some of its contents may already be gone. The error itself
/// goes to the log.
pub(crate) fn folder_partly_deleted_message(display_name: &str) -> String {
    format!("Couldn't finish deleting \"{display_name}\". Some items may already be gone.")
}

pub(crate) fn changed_since_request_message(display_name: &str) -> String {
    format!("\"{display_name}\" changed since you asked. Nothing was deleted.")
}

pub(crate) fn no_longer_exists_message(display_name: &str) -> String {
    format!("\"{display_name}\" no longer exists. Nothing was deleted.")
}

/// Why a test build must not delete `path`, if it mustn't.
///
/// Unit and integration tests only ever delete what they created in a temp folder, so a build
/// with tests compiled in refuses anything else, and a bug in a test or in the code under test
/// can't reach real files. It fails closed: `path` is allowed only if its canonical location is
/// inside the system temp folder (or, in an integration test, the throwaway home folder the
/// harness runs the app in), and it is refused inside the real Desktop and Documents folders
/// whatever else holds. The canonical location resolves every folder on the way to `path`, so a
/// link to a folder elsewhere can't carry a delete out of the temp folder, but not `path` itself,
/// because deleting a link removes only the link. A path that can't be resolved is refused.
///
/// This is compiled into test builds only; the app never makes this check.
#[cfg(any(test, feature = "integration_tests"))]
pub(crate) fn test_build_delete_refusal(path: &Path) -> Option<String> {
    match canonical_location(path) {
        Ok(location) => {
            let (allowed, forbidden) = test_build_delete_roots();
            delete_refusal_for_location(path, &location, &allowed, &forbidden)
        }
        Err(error) => Some(format!("couldn't resolve {}: {error}", path.display())),
    }
}

/// `path`, with the folder it's in resolved to its canonical form.
#[cfg(any(test, feature = "integration_tests"))]
fn canonical_location(path: &Path) -> io::Result<PathBuf> {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => Ok(dunce::canonicalize(parent)?.join(name)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "it has no parent folder",
        )),
    }
}

/// The decision [`test_build_delete_refusal`] makes once `path`'s canonical `location` is known.
#[cfg(any(test, feature = "integration_tests"))]
fn delete_refusal_for_location(
    path: &Path,
    location: &Path,
    allowed: &[PathBuf],
    forbidden: &[PathBuf],
) -> Option<String> {
    if let Some(root) = forbidden
        .iter()
        .find(|root| location.starts_with(root) || path.starts_with(root))
    {
        return Some(format!(
            "{} is inside {}, and a test build never deletes there",
            path.display(),
            root.display()
        ));
    }
    let inside_allowed = allowed
        .iter()
        .any(|root| location.starts_with(root) && location != root.as_path());
    if inside_allowed {
        None
    } else {
        Some(format!(
            "{} is outside the temp folder, and a test build deletes only inside it",
            path.display()
        ))
    }
}

/// The folders a test build may delete inside, and the folders it never deletes inside.
#[cfg(any(test, feature = "integration_tests"))]
fn test_build_delete_roots() -> (Vec<PathBuf>, Vec<PathBuf>) {
    let real_home = real_home_dir();
    let env_home = std::env::var_os("HOME").map(PathBuf::from);
    // The integration harness keeps the real HOME here before pointing HOME at a throwaway one.
    let original_home = std::env::var_os("ORIGINAL_HOME").map(PathBuf::from);

    let temp_dir = dunce::canonicalize(std::env::temp_dir()).ok();
    // Only a HOME that isn't the real one: where the harness hasn't replaced it, nothing under
    // it is allowed.
    #[cfg(feature = "integration_tests")]
    let harness_home = env_home
        .as_ref()
        .filter(|home| Some(*home) != real_home.as_ref() && Some(*home) != original_home.as_ref())
        .and_then(|home| dunce::canonicalize(home).ok());
    #[cfg(not(feature = "integration_tests"))]
    let harness_home: Option<PathBuf> = None;
    let allowed = [temp_dir, harness_home].into_iter().flatten().collect();

    let forbidden = [real_home, env_home, original_home]
        .into_iter()
        .flatten()
        .flat_map(|home| [home.join("Desktop"), home.join("Documents")])
        .collect();
    (allowed, forbidden)
}

/// The user's home folder from the user database, which tests can't redirect the way they can
/// redirect HOME.
#[cfg(all(any(test, feature = "integration_tests"), unix))]
fn real_home_dir() -> Option<PathBuf> {
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|user| user.dir)
}

#[cfg(all(any(test, feature = "integration_tests"), not(unix)))]
fn real_home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

pub(crate) enum DeleteFileConfirmationEvent {
    Confirm,
    Cancel,
    /// Focus moved from the open dialog to something else.
    FocusLost,
}

#[derive(Debug)]
pub(crate) enum DeleteFileConfirmationAction {
    Confirm,
    Cancel,
    /// The mouse button went down on Delete. A click on Delete confirms only if the dialog saw
    /// its press; see [`click_confirms`].
    DeletePressed,
}

/// Asks before the file tree permanently deletes an item. The file tree owns the target and
/// decides what happens; this view shows the target and reports which button was chosen.
pub(crate) struct DeleteFileConfirmationDialog {
    cancel_button: ViewHandle<ActionButton>,
    delete_button: ViewHandle<ActionButton>,
    target: Option<PendingDelete>,
    /// When the dialog last opened.
    opened_at: Option<Instant>,
    /// When the mouse button last went down on Delete since the dialog opened, if it has.
    pressed_at: Option<Instant>,
    /// How long Delete stays disabled after the dialog opens.
    arm_delay: Duration,
    /// Wakes the dialog when the arming delay is up, so it can draw Delete enabled.
    arm_timer: Option<SpawnedFutureHandle>,
    /// Where the dialog box was drawn in the last frame.
    box_position_id: String,
    /// Where the Delete button was drawn in the last frame.
    delete_button_position_id: String,
}

impl DeleteFileConfirmationDialog {
    pub(crate) fn new(ctx: &mut ViewContext<Self>) -> Self {
        let enter_keystroke = Keystroke::parse("enter").expect("Valid keystroke");
        let cancel_button = ctx.add_typed_action_view(|ctx| {
            ActionButton::new("Cancel", SecondaryTheme)
                .with_keybinding(KeystrokeSource::Fixed(enter_keystroke), ctx)
                .on_click(|ctx| {
                    ctx.dispatch_typed_action(DeleteFileConfirmationAction::Cancel);
                })
        });

        // No keybinding: the only way to delete is to click this button.
        let delete_button = ctx.add_typed_action_view(|_| {
            ActionButton::new("Delete", DangerPrimaryTheme).on_click(|ctx| {
                ctx.dispatch_typed_action(DeleteFileConfirmationAction::Confirm);
            })
        });

        let view_id = ctx.view_id();
        Self {
            cancel_button,
            delete_button,
            target: None,
            opened_at: None,
            pressed_at: None,
            arm_delay: ARM_DELAY,
            arm_timer: None,
            box_position_id: format!("delete_file_confirmation_dialog_{view_id}"),
            delete_button_position_id: format!("delete_file_confirmation_delete_{view_id}"),
        }
    }

    /// Shows the dialog for `target`. Delete starts disabled and is armed after the delay.
    pub(crate) fn set_target(&mut self, target: PendingDelete, ctx: &mut ViewContext<Self>) {
        self.target = Some(target);
        self.opened_at = Some(Instant::now());
        self.pressed_at = None;
        self.update_arming(ctx);
        ctx.notify();
    }

    pub(crate) fn clear_target(&mut self, ctx: &mut ViewContext<Self>) {
        self.target = None;
        self.opened_at = None;
        self.pressed_at = None;
        self.update_arming(ctx);
        ctx.notify();
    }

    /// Whether Delete is drawn enabled: the dialog is open and has been for at least the delay.
    fn is_armed(&self) -> bool {
        self.target.is_some()
            && self
                .opened_at
                .is_some_and(|opened_at| opened_at.elapsed() >= self.arm_delay)
    }

    /// Draws Delete disabled until the dialog is armed, and wakes the dialog when that happens.
    /// A disabled button still takes its clicks, so an early click does nothing at all rather
    /// than falling through to the layer behind it, which would cancel.
    fn update_arming(&mut self, ctx: &mut ViewContext<Self>) {
        if let Some(arm_timer) = self.arm_timer.take() {
            arm_timer.abort();
        }
        let armed = self.is_armed();
        self.delete_button
            .update(ctx, |button, ctx| button.set_disabled(!armed, ctx));

        if let (false, true, Some(opened_at)) = (armed, self.target.is_some(), self.opened_at) {
            let remaining = self.arm_delay.saturating_sub(opened_at.elapsed());
            // If the timer wakes a little early, this runs again and waits out the rest.
            self.arm_timer = Some(ctx.spawn(Timer::after(remaining), |me, _, ctx| {
                me.update_arming(ctx);
            }));
        }
    }
}

#[cfg(test)]
impl DeleteFileConfirmationDialog {
    pub(super) fn set_arm_delay(&mut self, arm_delay: Duration, ctx: &mut ViewContext<Self>) {
        self.arm_delay = arm_delay;
        self.update_arming(ctx);
    }

    pub(super) fn is_delete_button_enabled(&self, app: &AppContext) -> bool {
        !self.delete_button.as_ref(app).is_disabled()
    }

    pub(super) fn box_position_id(&self) -> &str {
        &self.box_position_id
    }

    pub(super) fn delete_button_position_id(&self) -> &str {
        &self.delete_button_position_id
    }
}

impl Entity for DeleteFileConfirmationDialog {
    type Event = DeleteFileConfirmationEvent;
}

impl View for DeleteFileConfirmationDialog {
    fn ui_name() -> &'static str {
        "DeleteFileConfirmationDialog"
    }

    fn on_focus(&mut self, _focus_ctx: &warpui::FocusContext, ctx: &mut ViewContext<Self>) {
        ctx.focus_self();
    }

    fn on_blur(&mut self, _: &BlurContext, ctx: &mut ViewContext<Self>) {
        // Focus went somewhere else while the dialog was up, for example because a shortcut
        // opened a new tab. The file tree closes the dialog rather than leave it on screen,
        // blocking the mouse, while its keys go elsewhere.
        if self.target.is_some() && !ctx.is_self_or_child_focused() {
            ctx.emit(DeleteFileConfirmationEvent::FocusLost);
        }
    }

    fn render(&self, app: &AppContext) -> Box<dyn Element> {
        let Some(target) = &self.target else {
            return Empty::new().finish();
        };
        let appearance = Appearance::as_ref(app);
        let theme = appearance.theme();

        // The full path on its own, so there is no doubt about which item goes.
        let path = Text::new(
            target.local_path.display().to_string(),
            appearance.monospace_font_family(),
            PATH_FONT_SIZE,
        )
        .with_color(blended_colors::text_sub(theme, theme.surface_1()))
        .finish();

        let cancel_button = Container::new(ChildView::new(&self.cancel_button).finish())
            .with_margin_right(12.)
            .finish();
        // Tells the dialog when a press on Delete begins, so a click can be traced back to it
        // (see `click_confirms`). It always runs, because the button takes the press first.
        let delete_button = EventHandler::new(
            SavePosition::new(
                ChildView::new(&self.delete_button).finish(),
                &self.delete_button_position_id,
            )
            .for_single_frame()
            .finish(),
        )
        .with_always_handle()
        .on_left_mouse_down(|ctx, _, _| {
            ctx.dispatch_typed_action(DeleteFileConfirmationAction::DeletePressed);
            DispatchEventResult::StopPropagation
        })
        .finish();

        // The box's own `Dismiss` gets a handler that does nothing: the backdrop below does the
        // cancelling, and a `Dismiss` with no handler logs a warning for every click outside it.
        let dialog = Dialog::new(
            target.title(),
            Some(target.body()),
            UiComponentStyles {
                width: Some(DIALOG_WIDTH),
                ..dialog_styles(appearance)
            },
        )
        .with_child(path)
        .with_bottom_row_child(cancel_button)
        .with_bottom_row_child(delete_button)
        .build()
        .on_dismiss(|_, _| {})
        .finish();

        // Under the dialog, a layer the size of the window, dimmed like Warp's other modal
        // dialogs: nothing behind it responds to the mouse, and a click on it cancels. The dialog
        // is drawn above that layer and handles its own clicks, so a click on the dialog's text
        // does nothing.
        let backdrop = Container::new(Empty::new().finish())
            .with_background_color(theme.blurred_background_overlay().into())
            .with_corner_radius(app.windows().window_corner_radius())
            .finish();
        let mut stack = Stack::new();
        stack.add_child(
            Dismiss::new(backdrop)
                .prevent_interaction_with_other_elements()
                .on_dismiss(|ctx, _| {
                    ctx.dispatch_typed_action(DeleteFileConfirmationAction::Cancel);
                })
                .finish(),
        );
        // The file tree lays this view out at the size of the window and pins it to the
        // window's top-left corner, so centring the box here centres it in the window, wherever
        // the file tree itself sits. In a window narrower than the box, the box narrows to fit.
        stack.add_child(
            Align::new(
                SavePosition::new(dialog, &self.box_position_id)
                    .for_single_frame()
                    .finish(),
            )
            .finish(),
        );
        stack.finish()
    }
}

impl TypedActionView for DeleteFileConfirmationDialog {
    type Action = DeleteFileConfirmationAction;

    fn handle_action(
        &mut self,
        action: &DeleteFileConfirmationAction,
        ctx: &mut ViewContext<Self>,
    ) {
        match action {
            DeleteFileConfirmationAction::DeletePressed => {
                if self.target.is_some() {
                    self.pressed_at = Some(Instant::now());
                }
            }
            DeleteFileConfirmationAction::Confirm => {
                // Each press completes at most one click.
                let pressed_at = self.pressed_at.take();
                let confirms = self.target.is_some()
                    && self.opened_at.is_some_and(|opened_at| {
                        click_confirms(opened_at, pressed_at, Instant::now(), self.arm_delay)
                    });
                if confirms {
                    ctx.emit(DeleteFileConfirmationEvent::Confirm);
                } else {
                    // The dialog stays open: failing closed costs at most a second click.
                    log::info!("Ignoring a click on Delete that began before Delete was armed");
                }
            }
            DeleteFileConfirmationAction::Cancel => {
                ctx.emit(DeleteFileConfirmationEvent::Cancel);
            }
        }
    }
}

#[cfg(test)]
#[path = "delete_confirmation_dialog_tests.rs"]
mod tests;
