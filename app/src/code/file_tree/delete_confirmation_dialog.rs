//! Confirmation dialog shown before the Project Explorer permanently deletes a file or folder.

use std::fs::{FileType, Metadata};
use std::io;
use std::path::PathBuf;

use warp_util::standardized_path::StandardizedPath;
use warpui::elements::{ChildView, Container, Dismiss, Empty, ParentElement, Stack, Text};
use warpui::keymap::{FixedBinding, Keystroke};
use warpui::ui_components::components::{UiComponent, UiComponentStyles};
use warpui::{
    AppContext, Element, Entity, SingletonEntity, TypedActionView, View, ViewContext, ViewHandle,
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
/// dialog showed. On Unix this is the device and inode, which survive edits in place and change
/// when the item is replaced. Elsewhere it is the size and modification time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ItemIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    len: u64,
    #[cfg(not(unix))]
    modified: Option<std::time::SystemTime>,
}

impl ItemIdentity {
    #[cfg(unix)]
    pub(crate) fn from_metadata(metadata: &Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;

        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    #[cfg(not(unix))]
    pub(crate) fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
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

pub(crate) fn changed_since_request_message(display_name: &str) -> String {
    format!("\"{display_name}\" changed since you asked. Nothing was deleted.")
}

pub(crate) fn no_longer_exists_message(display_name: &str) -> String {
    format!("\"{display_name}\" no longer exists. Nothing was deleted.")
}

pub(crate) enum DeleteFileConfirmationEvent {
    Confirm,
    Cancel,
}

#[derive(Debug)]
pub(crate) enum DeleteFileConfirmationAction {
    Confirm,
    Cancel,
}

/// Asks before the file tree permanently deletes an item. The file tree owns the target and
/// decides what happens; this view shows the target and reports which button was chosen.
pub(crate) struct DeleteFileConfirmationDialog {
    cancel_button: ViewHandle<ActionButton>,
    delete_button: ViewHandle<ActionButton>,
    target: Option<PendingDelete>,
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

        Self {
            cancel_button,
            delete_button,
            target: None,
        }
    }

    pub(crate) fn set_target(&mut self, target: PendingDelete, ctx: &mut ViewContext<Self>) {
        self.target = Some(target);
        ctx.notify();
    }

    pub(crate) fn clear_target(&mut self, ctx: &mut ViewContext<Self>) {
        self.target = None;
        ctx.notify();
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
        .with_bottom_row_child(ChildView::new(&self.delete_button).finish())
        .build()
        .finish();

        // Under the dialog, a layer the size of the window: nothing behind it responds to the
        // mouse, and a click on it cancels. The dialog is drawn above that layer and handles its
        // own clicks, so a click on the dialog's text does nothing.
        let mut stack = Stack::new();
        stack.add_child(
            Dismiss::new(Empty::new().finish())
                .prevent_interaction_with_other_elements()
                .on_dismiss(|ctx, _| {
                    ctx.dispatch_typed_action(DeleteFileConfirmationAction::Cancel);
                })
                .finish(),
        );
        stack.add_child(dialog);
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
            DeleteFileConfirmationAction::Confirm => {
                ctx.emit(DeleteFileConfirmationEvent::Confirm);
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
