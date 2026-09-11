use std::collections::{HashMap, HashSet};
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use editing::sort_entries_for_file_tree;
use itertools::Itertools;
use pathfinder_geometry::rect::RectF;
use pathfinder_geometry::vector::Vector2F;
use render::RenderState;
use repo_metadata::file_tree_store::{
    FileTreeDirectoryEntryState, FileTreeEntryState, FileTreeFileMetadata,
};
use repo_metadata::local_model::IndexedRepoState;
use repo_metadata::repositories::DetectedRepositories;
use repo_metadata::{FileTreeEntry, RepoMetadataModel};
use warp_core::features::FeatureFlag;
use warp_core::ui::theme::color::internal_colors;
use warp_core::ui::theme::Fill;
use warp_core::{send_telemetry_from_ctx, HostId};
use warp_util::path::LineAndColumnArg;
use warp_util::standardized_path::StandardizedPath;
use warpui::clipboard::ClipboardContent;
use warpui::elements::{
    AcceptedByDropTarget, Align, ChildAnchor, ChildView, Clipped, ConstrainedBox, Container,
    CrossAxisAlignment, Dismiss, Draggable, DraggableState, Empty, Flex, FormattedTextElement,
    Hoverable, MainAxisAlignment, MainAxisSize, MouseStateHandle, OffsetPositioning, ParentAnchor,
    ParentElement, ParentOffsetBounds, Percentage, Rect, SavePosition, ScrollStateHandle,
    Scrollable, ScrollableElement, ScrollbarWidth, Shrinkable, Stack, Text, UniformList,
    UniformListState,
};
use warpui::fonts::{Properties, Style, Weight};
use warpui::keymap::FixedBinding;
use warpui::platform::Cursor;
use warpui::text_layout::TextAlignment;
use warpui::{
    id, AppContext, BlurContext, Element, Entity, EventContext, ModelHandle, SingletonEntity as _,
    TypedActionView, View, ViewContext, ViewHandle, WeakViewHandle,
};

use crate::appearance::Appearance;
use crate::code::active_file::{ActiveFileEvent, ActiveFileModel};
use crate::code::buffer_location::LocalOrRemotePath;
use crate::code::file_tree::delete_confirmation_dialog::{
    self, changed_since_request_message, delete_failed_message, folder_partly_deleted_message,
    no_longer_exists_message, DeleteFileConfirmationDialog, DeleteFileConfirmationEvent,
    ItemIdentity, ItemKind, PendingDelete, STALE_MENU_MESSAGE,
};
use crate::coding_panel_enablement_state::CodingPanelEnablementState;
use crate::editor::{EditorOptions, EditorView, TextOptions};
use crate::menu::{Menu, MenuItem, MenuItemFields};
#[cfg(feature = "local_fs")]
use crate::server::telemetry::CodePanelsFileOpenEntrypoint;
use crate::server::telemetry::TelemetryEvent;
use crate::terminal::input::InputDropTargetData;
use crate::terminal::view::{TerminalDropTargetData, TerminalView};
use crate::ui_components::icons::Icon;
use crate::ui_components::item_highlight::{ImageOrIcon, ItemHighlightState};
#[cfg(feature = "local_fs")]
use crate::util::file::external_editor::EditorSettings;
use crate::util::openable_file_type::{
    is_file_content_binary, is_jupyter_notebook_file, is_markdown_file, EditorLayout, FileTarget,
};
#[cfg(feature = "local_fs")]
use crate::util::openable_file_type::{
    resolve_file_target_to_open_in_warp, resolve_file_target_with_editor_choice,
};
use crate::view_components::DismissibleToast;
use crate::workspace::ToastStack;

mod editing;
mod render;

use crate::settings::{CodeSettings, CodeSettingsChangedEvent};

const REMOTE_TEXT: &str = "The Project Explorer requires access to your local workspace, which isn’t supported in remote sessions.";
const DISABLED_TEXT: &str = "The Project Explorer requires access to your local workspace. Open a new session or navigate to an active session to view.";
const WSL_TEXT: &str = "The Project Explorer doesn't currently work in WSL.";

/// Stable identifier for an item in the file tree.
/// Includes both the root directory and the index within that root's flattened list.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileTreeIdentifier {
    /// The root directory this item belongs to
    pub root: StandardizedPath,
    /// Index within the flattened list for this root
    pub index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingEditKind {
    CreateNewFile,
    RenameExisting,
}

#[derive(Debug, Clone)]
pub enum FileTreeAction {
    ItemClicked {
        id: FileTreeIdentifier,
    },
    SelectPreviousItem,
    SelectNextItem,
    Expand,
    Collapse,
    ExecuteSelectedItem,
    OpenContextMenu {
        position: Vector2F,
        id: FileTreeIdentifier,
    },
    CopyPath {
        id: FileTreeIdentifier,
    },
    CopyRelativePath {
        id: FileTreeIdentifier,
    },
    AttachAsContext {
        id: FileTreeIdentifier,
    },
    OpenInFinder {
        id: FileTreeIdentifier,
    },
    Rename {
        id: FileTreeIdentifier,
    },
    Delete {
        id: FileTreeIdentifier,
        /// The item's path when the menu was built. The action is refused if the row at `id` no
        /// longer holds this path, because the list may have been rebuilt since.
        path: StandardizedPath,
    },
    NewFileBelowDirectory {
        id: FileTreeIdentifier,
    },
    OpenInNewPane {
        id: FileTreeIdentifier,
    },
    OpenInNewTab {
        id: FileTreeIdentifier,
    },
    CDToDirectory {
        id: FileTreeIdentifier,
    },
    DismissEditor,
    ItemDroppedOnInput {
        id: FileTreeIdentifier,
        terminal_input_data: InputDropTargetData,
    },
    ItemDroppedOnTerminal {
        id: FileTreeIdentifier,
        terminal_view: WeakViewHandle<TerminalView>,
    },
}

pub fn init(app: &mut AppContext) {
    app.register_fixed_bindings([
        FixedBinding::new(
            "up",
            FileTreeAction::SelectPreviousItem,
            id!(FileTreeView::ui_name()),
        ),
        FixedBinding::new(
            "down",
            FileTreeAction::SelectNextItem,
            id!(FileTreeView::ui_name()),
        ),
        FixedBinding::new(
            "right",
            FileTreeAction::Expand,
            id!(FileTreeView::ui_name()),
        ),
        FixedBinding::new(
            "left",
            FileTreeAction::Collapse,
            id!(FileTreeView::ui_name()),
        ),
        FixedBinding::new(
            "enter",
            FileTreeAction::ExecuteSelectedItem,
            id!(FileTreeView::ui_name()),
        ),
    ]);
    delete_confirmation_dialog::init(app);
}

// Constants matching the Drive panel styling
const ITEM_FONT_SIZE: f32 = 14.;
const FOLDER_INDENT: f32 = 16.; // Indentation per folder level
const ITEM_PADDING: f32 = 4.;

/// Represents a single item in the flattened file tree list.
/// This is used to store the necessary information for rendering each item
/// in the UniformList.
#[derive(Clone)]
enum FileTreeItem {
    /// A file item with its metadata and depth in the tree
    File {
        metadata: FileTreeFileMetadata,
        depth: usize,
        mouse_state_handle: MouseStateHandle,
        draggable_state: DraggableState,
    },
    /// A directory header with its metadata, depth, and expanded state
    DirectoryHeader {
        directory: FileTreeDirectoryEntryState,
        depth: usize,
        mouse_state_handle: MouseStateHandle,
        draggable_state: DraggableState,
    },
}

impl FileTreeItem {
    fn path(&self) -> &StandardizedPath {
        match self {
            FileTreeItem::File { metadata, .. } => &metadata.path,
            FileTreeItem::DirectoryHeader { directory, .. } => &directory.path,
        }
    }
}

struct ContextMenuState {
    position: Vector2F,
}

struct PendingEdit {
    kind: PendingEditKind,
    id: FileTreeIdentifier,
}

/// Per-root directory state for the file tree.
/// Contains all state that varies per root directory.
struct RootDirectory {
    /// File tree entry for this root. This may be backed by a repository root
    /// or by a lazily-loaded standalone path.
    entry: FileTreeEntry,
    /// Set of expanded folder paths within this root
    expanded_folders: HashSet<StandardizedPath>,
    /// Flattened list of items for rendering
    items: Vec<FileTreeItem>,
    /// Mouse state handles and draggable state preserved across rebuilds, keyed by path
    item_states: HashMap<StandardizedPath, (MouseStateHandle, DraggableState)>,
    /// The remote host this root belongs to, if any. `None` for local roots.
    remote_host_id: Option<HostId>,
}

impl RootDirectory {
    /// Returns whether this root is backed by a remote server.
    fn is_remote(&self) -> bool {
        self.remote_host_id.is_some()
    }
}

pub struct FileTreeView {
    /// Per-root state, keyed by root path
    root_directories: HashMap<StandardizedPath, RootDirectory>,
    /// The displayed directories
    displayed_directories: Vec<StandardizedPath>,
    #[cfg(feature = "local_fs")]
    enablement: CodingPanelEnablementState,
    #[cfg(feature = "local_fs")]
    repository_metadata_model: ModelHandle<RepoMetadataModel>,
    #[cfg(feature = "local_fs")]
    is_active: bool,
    /// Identifier of the currently selected item
    selected_item: Option<FileTreeIdentifier>,
    /// State for the UniformList
    list_state: UniformListState,
    /// Scroll state handle for the NewScrollable wrapper
    scroll_state: ScrollStateHandle,
    /// Weak view handle for the FilePicker
    view_handle: WeakViewHandle<Self>,
    context_menu: ViewHandle<Menu<FileTreeAction>>,
    /// State for the context menu, if one is currently open.
    context_menu_state: Option<ContextMenuState>,
    /// Asks for confirmation before an item is deleted from disk.
    delete_dialog: ViewHandle<DeleteFileConfirmationDialog>,
    /// The item the delete dialog is asking about, captured by path when the dialog opened. The
    /// dialog is shown exactly when this is set.
    pending_delete: Option<PendingDelete>,
    /// Paths with a delete running in the background. A second request for one of them is
    /// ignored until the first finishes.
    deletes_in_flight: HashSet<StandardizedPath>,
    /// The roots the file tree displayed when the delete dialog opened. The dialog closes if they
    /// change while it's open.
    pending_delete_roots: Vec<StandardizedPath>,
    /// Unique ID used to position this view.
    position_id: String,
    /// The pending edit, if any
    pending_edit: Option<PendingEdit>,
    /// Editor view used for editing (renaming, creating) an item in the tree.
    editor_view: ViewHandle<EditorView>,
    /// Handle to track the currently focused file
    active_file_model: Option<ModelHandle<ActiveFileModel>>,
    has_terminal_session: bool,
    /// Paths the user explicitly collapsed (per root).
    ///
    /// This is used to prevent automatic expansion behavior (e.g. when switching tabs,
    /// focusing the left panel, or "reveal active file") from overriding the user's intent.
    ///
    /// The root header collapse is represented by including the root path itself in the set.
    explicitly_collapsed: HashMap<StandardizedPath, HashSet<StandardizedPath>>,
    /// Lazy-loaded paths that this view has registered with the
    /// [`LocalRepoMetadataModel`] for file watching.
    #[cfg(feature = "local_fs")]
    registered_lazy_loaded_paths: HashSet<StandardizedPath>,
    /// Directory the view wants to focus once its entry becomes available.
    ///
    /// Set when a descendant path is absorbed into an ancestor root but the
    /// descendant's entry has not yet been materialized (e.g. the ancestor
    /// is still lazy-loading). Re-evaluated on every rebuild; cleared when
    /// the target is selected by the user or when the target root stops
    /// being displayed.
    pending_focus_target: Option<PendingFocusTarget>,
    /// Whether to show hidden files (dotfiles) in the file tree.
    show_hidden_files: bool,
}

/// Directory the file tree wants to focus once its entry becomes available.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingFocusTarget {
    root: StandardizedPath,
    path: StandardizedPath,
    /// Whether this target has already driven a scroll.
    ///
    /// The first successful apply scrolls the tree to the target. On
    /// subsequent rebuilds we still re-apply the selection marker so it
    /// doesn't get overridden by a default root-header fallback, but we
    /// intentionally do NOT scroll again — the user may have scrolled
    /// elsewhere since the initial focus-follow and a late metadata
    /// update must not snap them back.
    scrolled: bool,
}

impl FileTreeView {
    fn is_explicitly_collapsed(&self, root: &StandardizedPath, path: &StandardizedPath) -> bool {
        self.explicitly_collapsed
            .get(root)
            .is_some_and(|collapsed| collapsed.contains(path))
    }

    /// Returns whether the item identified by `id` belongs to a remote root.
    fn is_remote_item(&self, id: &FileTreeIdentifier) -> bool {
        self.root_directories
            .get(&id.root)
            .is_some_and(|r| r.is_remote())
    }

    #[cfg(feature = "local_fs")]
    fn is_active(&self) -> bool {
        self.is_active
    }

    #[cfg(not(feature = "local_fs"))]
    fn is_active(&self) -> bool {
        true
    }

    #[cfg(feature = "local_fs")]
    pub fn set_is_active(&mut self, is_active: bool, ctx: &mut ViewContext<Self>) {
        self.set_is_active_local_fs(is_active, ctx);
    }

    #[cfg(not(feature = "local_fs"))]
    pub fn set_is_active(&mut self, _is_active: bool, _ctx: &mut ViewContext<Self>) {}

    #[cfg(feature = "local_fs")]
    fn set_is_active_local_fs(&mut self, is_active: bool, ctx: &mut ViewContext<Self>) {
        if self.is_active == is_active {
            return;
        }

        self.is_active = is_active;

        if is_active {
            self.subscribe_to_repository_metadata(ctx);
            self.subscribe_to_active_file_model(ctx);
            self.subscribe_to_code_settings(ctx);
            self.show_hidden_files = *CodeSettings::as_ref(ctx).show_hidden_files;

            // Catch up on any repository/file changes that happened while inactive.
            // Skip remote-backed roots — their data comes from server pushes,
            // not from local DetectedRepositories / lazy-loading.
            let local_dirs: Vec<_> = self
                .displayed_directories
                .iter()
                .filter(|p| !self.root_directories.get(p).is_some_and(|r| r.is_remote()))
                .cloned()
                .collect();
            self.update_directory_contents(&local_dirs, false, ctx);

            // Catch up on remote roots already in root_directories whose
            // content may have changed while inactive. We only refresh
            // existing roots — new remote roots are managed by the
            // workspace via `set_remote_root_directories`.
            let existing_remote_ids: Vec<_> = self
                .root_directories
                .iter()
                .filter_map(|(_, root_dir)| {
                    let host_id = root_dir.remote_host_id.as_ref()?;
                    Some(repo_metadata::RemoteRepositoryIdentifier::new(
                        host_id.clone(),
                        root_dir.entry.root_directory().as_ref().clone(),
                    ))
                })
                .collect();
            if !existing_remote_ids.is_empty() {
                self.insert_or_update_remote_roots(&existing_remote_ids, false, ctx);
            }
        } else {
            // A file tree that isn't showing can't show its delete dialog either.
            self.close_delete_dialog(ctx);
            ctx.unsubscribe_to_model(&self.repository_metadata_model);
            self.unsubscribe_from_active_file_model(ctx);
            self.unsubscribe_from_code_settings(ctx);
            let repository_metadata_model = self.repository_metadata_model.clone();
            let paths: Vec<_> = self.registered_lazy_loaded_paths.drain().collect();
            repository_metadata_model.update(ctx, move |model: &mut RepoMetadataModel, ctx| {
                for path in &paths {
                    model.remove_lazy_loaded_path(path, ctx);
                }
            });
        }
    }

    #[cfg(feature = "local_fs")]
    fn subscribe_to_repository_metadata(&self, ctx: &mut ViewContext<Self>) {
        let model = self.repository_metadata_model.clone();
        ctx.subscribe_to_model(&model, |me, _, event, ctx| {
            me.handle_repository_metadata_event(event, ctx);
        });
    }

    #[cfg(feature = "local_fs")]
    fn remove_lazy_loaded_entry(&mut self, path: &StandardizedPath, ctx: &mut ViewContext<Self>) {
        let Some(std_path) = self.registered_lazy_loaded_paths.take(path) else {
            return;
        };
        let repository_metadata_model = self.repository_metadata_model.clone();
        repository_metadata_model.update(ctx, move |model: &mut RepoMetadataModel, ctx| {
            model.remove_lazy_loaded_path(&std_path, ctx);
        });
    }

    /// Inserts or updates remote root directories. When `insert_only` is
    /// true, roots already present in `displayed_directories` are skipped
    /// (used for catch-up on reactivation). When false, existing roots are
    /// updated in-place (used for live server push events).
    ///
    /// Performs a single `rebuild_flattened_items` / `ctx.notify` at the end
    /// regardless of how many roots were processed.
    #[cfg(feature = "local_fs")]
    fn insert_or_update_remote_roots(
        &mut self,
        remote_ids: &[repo_metadata::RemoteRepositoryIdentifier],
        insert_only: bool,
        ctx: &mut ViewContext<Self>,
    ) {
        use repo_metadata::RepositoryIdentifier;

        let mut changed = false;

        for remote_id in remote_ids {
            let repo_path = remote_id.path.clone();

            if insert_only && self.displayed_directories.contains(&repo_path) {
                continue;
            }

            let id = RepositoryIdentifier::Remote(remote_id.clone());
            let Some(state) = RepoMetadataModel::as_ref(ctx).get_repository(&id, ctx) else {
                continue;
            };

            // Remove any existing roots that are ancestors or descendants of the
            // new path. For example, when the user cd's from /home/user into
            // /home/user/repo, replace the home directory root with the repo root.
            self.displayed_directories.retain(|existing| {
                if *existing == repo_path {
                    return true;
                }
                let dominated = existing.starts_with(&repo_path) || repo_path.starts_with(existing);
                if dominated {
                    self.root_directories.remove(existing);
                }
                !dominated
            });

            let host_id = remote_id.host_id.clone();
            self.root_directories
                .entry(repo_path.clone())
                .and_modify(|root_dir| {
                    root_dir.entry = state.entry.clone();
                    root_dir.remote_host_id = Some(host_id.clone());
                })
                .or_insert_with(|| RootDirectory {
                    entry: state.entry.clone(),
                    expanded_folders: HashSet::new(),
                    items: Vec::new(),
                    item_states: HashMap::new(),
                    remote_host_id: Some(host_id),
                });

            if !self.displayed_directories.contains(&repo_path) {
                self.displayed_directories.push(repo_path.clone());
            }

            // Auto-expand the root, respecting explicit user collapses.
            if !self.is_explicitly_collapsed(&repo_path, &repo_path) {
                if let Some(root_dir) = self.root_directories.get_mut(&repo_path) {
                    root_dir.expanded_folders.insert(repo_path);
                }
            }

            changed = true;
        }

        if changed {
            self.rebuild_flattened_items();
            ctx.notify();
        }
    }

    #[cfg(feature = "local_fs")]
    fn handle_repository_metadata_event(
        &mut self,
        event: &repo_metadata::RepoMetadataEvent,
        ctx: &mut ViewContext<Self>,
    ) {
        use repo_metadata::{RepoMetadataEvent, RepositoryIdentifier};
        match event {
            RepoMetadataEvent::RepositoryUpdated {
                id: RepositoryIdentifier::Local(std_path),
            } => {
                // Always update when a repository finishes indexing
                // Collect matching directories first to avoid borrow checker issues
                let dirs_to_update: Vec<StandardizedPath> = self
                    .displayed_directories
                    .iter()
                    .filter(|active_dir| {
                        **active_dir == *std_path
                            || active_dir.starts_with(std_path)
                            || std_path.starts_with(active_dir)
                    })
                    .cloned()
                    .collect();

                self.update_directory_contents(&dirs_to_update, false, ctx);

                if !dirs_to_update.is_empty() {
                    self.auto_expand_to_most_recent_directory(ctx);
                    // If we've been waiting for a cd'd descendant to
                    // materialize (e.g. the ancestor just finished
                    // indexing), apply that selection on top of the
                    // default root-header auto-expand so the cwd-follow
                    // wins once its item is available.
                    self.apply_pending_focus_target();
                }
            }
            RepoMetadataEvent::FileTreeEntryUpdated {
                id: RepositoryIdentifier::Local(std_path),
                ..
            } => {
                // Find root directories whose backing model entry matches this path.
                let root_paths: Vec<StandardizedPath> = self
                    .root_directories
                    .iter()
                    .filter_map(|(root_path, root_dir)| {
                        (**root_dir.entry.root_directory() == *std_path)
                            .then_some(root_path.clone())
                    })
                    .collect();

                if !root_paths.is_empty() {
                    let id = RepositoryIdentifier::Local(std_path.clone());
                    if let Some(state) = RepoMetadataModel::as_ref(ctx).get_repository(&id, ctx) {
                        for root_path in &root_paths {
                            if let Some(root_dir) = self.root_directories.get_mut(root_path) {
                                root_dir.entry = state.entry.clone();
                            }
                        }

                        for root_path in &root_paths {
                            self.rebuild_flattened_items_for_root(root_path);
                        }
                        self.apply_pending_focus_target();
                        ctx.notify();
                    }
                }
            }
            RepoMetadataEvent::UpdatingRepositoryFailed {
                id: RepositoryIdentifier::Local(std_path),
            } => {
                self.update_directory_contents(std::slice::from_ref(std_path), false, ctx);
            }
            RepoMetadataEvent::RepositoryUpdated {
                id: RepositoryIdentifier::Remote(remote_id),
            } => {
                // Only update existing remote roots — never add new ones.
                // New remote roots are pushed by the workspace via
                // `set_remote_root_directories` when `NavigatedToDirectory`
                // resolves for a session in this pane group.
                //
                // Match on both path and host_id so that two different
                // hosts that happen to share a path (e.g. /home/user/repo)
                // don't interfere with each other.
                let repo_path = &remote_id.path;
                let belongs_to_this_tree =
                    self.root_directories
                        .get(repo_path)
                        .is_some_and(|root_dir| {
                            root_dir
                                .remote_host_id
                                .as_ref()
                                .is_some_and(|h| *h == remote_id.host_id)
                        });
                if belongs_to_this_tree {
                    self.insert_or_update_remote_roots(std::slice::from_ref(remote_id), false, ctx);
                }
            }
            RepoMetadataEvent::FileTreeEntryUpdated {
                id: RepositoryIdentifier::Remote(remote_id),
                ..
            } => {
                let repo_path = remote_id.path.clone();
                let id = RepositoryIdentifier::Remote(remote_id.clone());
                if let Some(state) = RepoMetadataModel::as_ref(ctx).get_repository(&id, ctx) {
                    if let Some(root_dir) = self.root_directories.get_mut(&repo_path) {
                        root_dir.entry = state.entry.clone();
                    }
                    // Only rebuild the affected remote root instead of all roots.
                    // Remote servers stream frequent incremental updates; a full
                    // rebuild would cause unrelated local roots to re-render on
                    // every remote filesystem change, leading to visible flicker.
                    self.rebuild_flattened_items_for_root(&repo_path);
                    ctx.notify();
                }
            }
            RepoMetadataEvent::RepositoryRemoved {
                id: RepositoryIdentifier::Remote(remote_id),
            } => {
                let repo_path = &remote_id.path;
                self.displayed_directories.retain(|p| p != repo_path);
                self.root_directories.remove(repo_path);
                // The removed root is already gone from root_directories, so
                // this is effectively a no-op rebuild that avoids touching
                // the remaining roots' flattened items.
                self.rebuild_flattened_items_for_root(repo_path);
                ctx.notify();
            }
            RepoMetadataEvent::FileTreeUpdated { .. }
            | RepoMetadataEvent::RepositoryRemoved { .. }
            | RepoMetadataEvent::StandingQueryResultsUpdated { .. }
            | RepoMetadataEvent::UpdatingRepositoryFailed { .. }
            | RepoMetadataEvent::IncrementalUpdateReady { .. } => {}
        }
        self.close_delete_dialog_if_stale(ctx);
    }

    #[cfg(feature = "local_fs")]
    fn subscribe_to_active_file_model(&self, ctx: &mut ViewContext<Self>) {
        let Some(active_file_model) = self.active_file_model.clone() else {
            return;
        };

        ctx.subscribe_to_model(&active_file_model, |me, _, event, ctx| {
            me.handle_code_event(event, ctx);
        });
    }

    #[cfg(feature = "local_fs")]
    fn unsubscribe_from_active_file_model(&self, ctx: &mut ViewContext<Self>) {
        let Some(active_file_model) = self.active_file_model.as_ref() else {
            return;
        };

        ctx.unsubscribe_to_model(active_file_model);
    }

    fn subscribe_to_code_settings(&self, ctx: &mut ViewContext<Self>) {
        ctx.subscribe_to_model(&CodeSettings::handle(ctx), |me, _, event, ctx| {
            if let CodeSettingsChangedEvent::ShowHiddenFiles { .. } = event {
                me.show_hidden_files = *CodeSettings::as_ref(ctx).show_hidden_files;
                me.rebuild_flattened_items();
                ctx.notify();
            }
        });
    }

    fn unsubscribe_from_code_settings(&self, ctx: &mut ViewContext<Self>) {
        ctx.unsubscribe_to_model(&CodeSettings::handle(ctx));
    }

    pub fn new(ctx: &mut ViewContext<Self>) -> Self {
        let context_menu = ctx.add_typed_action_view(|_| {
            Menu::new()
                .prevent_interaction_with_other_elements()
                .with_drop_shadow()
        });
        ctx.subscribe_to_view(&context_menu, |me, _, event, ctx| {
            me.handle_menu_event(event, ctx);
        });

        let delete_dialog = ctx.add_typed_action_view(DeleteFileConfirmationDialog::new);
        ctx.subscribe_to_view(&delete_dialog, |me, _, event, ctx| {
            me.handle_delete_dialog_event(event, ctx);
        });

        let editor_view = ctx.add_typed_action_view(|ctx| {
            let appearance = Appearance::as_ref(ctx);
            EditorView::new(
                EditorOptions {
                    autogrow: false,
                    soft_wrap: false,
                    single_line: true,
                    text: TextOptions {
                        font_size_override: Some(ITEM_FONT_SIZE),
                        font_family_override: Some(appearance.ui_font_family()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ctx,
            )
        });

        ctx.subscribe_to_view(&editor_view, |me, _, event, ctx| match event {
            crate::editor::Event::Enter => me.commit_pending_edit(ctx),
            crate::editor::Event::Escape => me.cancel_pending_edit(ctx),
            _ => {}
        });

        #[cfg(feature = "local_fs")]
        let repository_metadata_model = RepoMetadataModel::handle(ctx);

        let picker = Self {
            root_directories: HashMap::new(),
            displayed_directories: Vec::new(),
            #[cfg(feature = "local_fs")]
            enablement: CodingPanelEnablementState::Enabled,
            #[cfg(feature = "local_fs")]
            repository_metadata_model,
            #[cfg(feature = "local_fs")]
            is_active: false,
            selected_item: None,
            list_state: UniformListState::new(),
            scroll_state: ScrollStateHandle::default(),
            view_handle: ctx.handle(),
            context_menu,
            context_menu_state: None,
            delete_dialog,
            pending_delete: None,
            deletes_in_flight: HashSet::new(),
            pending_delete_roots: Vec::new(),
            position_id: format!("file_tree_{}", ctx.view_id()),
            pending_edit: None,
            editor_view,
            active_file_model: None,
            has_terminal_session: false,
            explicitly_collapsed: HashMap::new(),
            #[cfg(feature = "local_fs")]
            registered_lazy_loaded_paths: HashSet::new(),
            pending_focus_target: None,
            show_hidden_files: *CodeSettings::as_ref(ctx).show_hidden_files,
        };

        picker
    }

    /// Sets [`ActiveFileModel`] for the [`FileTreeView`] to track
    /// which files are currently open.
    pub fn set_active_file_model(
        &mut self,
        active_file_model: ModelHandle<ActiveFileModel>,
        ctx: &mut ViewContext<Self>,
    ) {
        // Unsubscribe from old model if any.
        if let Some(ref old_model) = self.active_file_model {
            ctx.unsubscribe_to_model(old_model);
        }

        self.active_file_model = Some(active_file_model.clone());
        if self.is_active() {
            ctx.subscribe_to_model(&active_file_model, |me, _, event, ctx| {
                me.handle_code_event(event, ctx);
            });
        }
    }

    /// Handles events from the CodeModel
    fn handle_code_event(&mut self, event: &ActiveFileEvent, ctx: &mut ViewContext<Self>) {
        // When a file is focused, scroll to show it in the file tree
        match event {
            ActiveFileEvent::ActiveFileChanged { location } => {
                let file_std = match location {
                    crate::code::buffer_location::LocalOrRemotePath::Local(path) => {
                        match StandardizedPath::try_from_local(path) {
                            Ok(std_path) => std_path,
                            Err(_) => return,
                        }
                    }
                    crate::code::buffer_location::LocalOrRemotePath::Remote(remote) => {
                        remote.path.clone()
                    }
                };
                // Prefer the currently-selected item's root if the file lives under it;
                // otherwise fall back to the deepest matching root directory.
                let repository_root = self
                    .selected_item
                    .as_ref()
                    .filter(|id| file_std.starts_with(&id.root))
                    .map(|id| id.root.clone())
                    .or_else(|| self.find_deepest_root_for_file(&file_std));

                let Some(repository_root) = repository_root else {
                    return;
                };
                self.scroll_to_file(&repository_root, &file_std, ctx);
            }
        }
    }

    /// Finds the deepest root directory that contains the given file path.
    /// Returns None if no root contains the file.
    /// Prefer passing around explicit [`FileTreeIdentifier`]s instead of paths.
    fn find_deepest_root_for_file(&self, file_path: &StandardizedPath) -> Option<StandardizedPath> {
        self.root_directories
            .keys()
            .filter(|root| file_path.starts_with(root))
            .max_by_key(|root| root.as_str().len())
            .cloned()
    }

    /// Scrolls to show the specified file in the file tree,
    /// ensuring that all parent folders are expanded and loaded.
    fn scroll_to_file(
        &mut self,
        repository_root: &StandardizedPath,
        file_path: &StandardizedPath,
        ctx: &mut ViewContext<Self>,
    ) {
        self.expand_ancestors_to_path(repository_root, file_path, ctx);

        // Create a FileTreeIdentifier to search for the file in the specific root
        // We need to find the item first by rebuilding the tree, then looking for it
        self.rebuild_flattened_items();

        // Now find the item in the specific root
        if let Some(root_dir) = self.root_directories.get(repository_root) {
            if let Some((index, _)) = root_dir
                .items
                .iter()
                .enumerate()
                .find(|(_, item)| *item.path() == *file_path)
            {
                let id = FileTreeIdentifier {
                    root: repository_root.clone(),
                    index,
                };
                self.select_id(&id, ctx);
            }
        }
    }

    /// Expands all ancestor directories between root and target_path.
    /// This ensures that target_path (or its parent if it's a file) is visible in the tree.
    fn expand_ancestors_to_path(
        &mut self,
        root: &StandardizedPath,
        target_path: &StandardizedPath,
        ctx: &mut ViewContext<Self>,
    ) {
        if !target_path.starts_with(root) {
            return;
        }
        if !self.root_directories.contains_key(root) {
            return;
        }

        // If the user explicitly collapsed the root header, do nothing.
        if self.is_explicitly_collapsed(root, root) {
            return;
        }

        // Collect ancestors between target and root (exclusive of root).
        let parents_to_expand: Vec<_> = target_path
            .ancestors()
            .skip(1) // skip target itself — we expand its *parents*
            .take_while(|p| *p != *root)
            .collect();

        // Expand from root -> leaf so we can stop at the first explicitly-collapsed folder.
        for parent in parents_to_expand.into_iter().rev() {
            if self.is_explicitly_collapsed(root, &parent) {
                break;
            }

            if let Some(root_dir) = self.root_directories.get_mut(root) {
                root_dir.expanded_folders.insert(parent.clone());
            }
            self.ensure_loaded_path(root, &parent, ctx);
        }
        // Also expand root itself.
        if let Some(root_dir) = self.root_directories.get_mut(root) {
            root_dir.expanded_folders.insert(root.clone());
        }
        self.ensure_loaded_path(root, root, ctx);
    }

    /// Scroll to the item at the given FileTreeIdentifier, without expanding folders.
    pub fn perform_scroll(&mut self, id: &FileTreeIdentifier) {
        if let Some(global_index) = self.identifier_to_global_index(id) {
            self.list_state.scroll_to(global_index);
        }
    }

    /// Scroll so the item at the given `FileTreeIdentifier` sits at the
    /// top of the viewport. Unlike [`perform_scroll`] (which only scrolls
    /// far enough to make the item visible), this always places the item
    /// at the top. Used for the cd-follow so a fresh repo root lands at
    /// the top of the tree rather than just being made visible at the
    /// current scroll position.
    fn perform_scroll_to_top(&mut self, id: &FileTreeIdentifier) {
        let Some(global_index) = self.identifier_to_global_index(id) else {
            return;
        };
        let current = self.list_state.scroll_top();
        let target = global_index as f64;
        let delta_lines = target - current.as_f64();
        // `UniformListState::add_scroll_top` clamps against zero; the
        // layout-time `autoscroll` will also clamp against `scroll_max`.
        self.list_state.add_scroll_top(delta_lines as f32);
    }

    #[cfg(feature = "local_fs")]
    pub(crate) fn set_enablement_state(
        &mut self,
        enablement: CodingPanelEnablementState,
        ctx: &mut ViewContext<Self>,
    ) {
        if self.enablement == enablement {
            return;
        }
        self.enablement = enablement;
        self.close_delete_dialog_if_stale(ctx);
        ctx.notify();
    }

    /// Sets the remote root directories to display in the file tree.
    ///
    /// This is the remote equivalent of [`set_root_directories`]. It
    /// inserts or updates the given remote repos and removes any existing
    /// remote roots that are NOT in `repos`. Local roots are unaffected.
    #[cfg(feature = "local_fs")]
    pub fn set_remote_root_directories(
        &mut self,
        repos: &[repo_metadata::RemoteRepositoryIdentifier],
        ctx: &mut ViewContext<Self>,
    ) {
        // Remove remote roots that are no longer in the desired set.
        let desired_paths: HashSet<&StandardizedPath> = repos.iter().map(|id| &id.path).collect();
        let stale_remote_paths: Vec<StandardizedPath> = self
            .root_directories
            .iter()
            .filter(|(_, root_dir)| root_dir.is_remote())
            .filter(|(path, _)| !desired_paths.contains(path))
            .map(|(path, _)| path.clone())
            .collect();
        let mut changed = !stale_remote_paths.is_empty();
        for path in &stale_remote_paths {
            self.displayed_directories.retain(|p| p != path);
            self.root_directories.remove(path);
        }

        // Insert or update the desired remote roots.
        // `insert_or_update_remote_roots` skips repos whose model data
        // hasn't arrived yet (`get_repository` returns None). For those,
        // we still register an empty placeholder in `root_directories`
        // and `displayed_directories` so that the subsequent
        // `RepositoryUpdated { Remote }` event (which fires when the
        // model data arrives) passes the `contains_key` guard and fills
        // in the tree.
        if !repos.is_empty() {
            // `insert_or_update_remote_roots` already rebuilds + notifies
            // when it mutates state. Track whether the placeholder loop
            // below adds anything new so we only rebuild a second time
            // when necessary.
            self.insert_or_update_remote_roots(repos, false, ctx);

            for remote_id in repos {
                let repo_path = &remote_id.path;
                if !self.root_directories.contains_key(repo_path) {
                    // Model data not available yet — create a placeholder.
                    let host_id = remote_id.host_id.clone();
                    self.root_directories.insert(
                        repo_path.clone(),
                        RootDirectory {
                            entry: Self::create_empty_entry(repo_path),
                            expanded_folders: HashSet::new(),
                            items: Vec::new(),
                            item_states: HashMap::new(),
                            remote_host_id: Some(host_id),
                        },
                    );
                    changed = true;
                }
                if !self.displayed_directories.contains(repo_path) {
                    self.displayed_directories.push(repo_path.clone());
                    changed = true;
                }
            }
        }

        if changed {
            self.rebuild_flattened_items();
            ctx.notify();
        }
        self.close_delete_dialog_if_stale(ctx);
    }

    /// Sets the root directories to display in the file tree.
    ///
    /// This only manages **local** roots. Remote-backed roots (those with a
    /// `remote_host_id`) are managed by [`set_remote_root_directories`]
    /// and are preserved across calls to this method.
    ///
    /// When multiple input paths have an ancestor/descendant relationship
    /// among local roots, only the surviving ancestor is displayed and the
    /// absorbed descendants' ancestor chains are auto-expanded so the user
    /// can still see their focus. Explicitly-collapsed folders are not
    /// re-expanded.
    #[cfg_attr(not(feature = "local_fs"), allow(unused_variables))]
    pub fn set_root_directories(&mut self, paths: Vec<PathBuf>, ctx: &mut ViewContext<Self>) {
        // Convert PathBuf inputs to StandardizedPath at this entry point.
        let std_paths: Vec<StandardizedPath> = paths
            .iter()
            .filter_map(|p| StandardizedPath::try_from_local(p).ok())
            .collect();

        // Collect existing remote directories so we can preserve them.
        // Remote CWDs are not present in `std_paths` because `normalize_cwd`
        // in the `WorkingDirectoriesModel` drops paths that cannot be
        // canonicalized on the local filesystem.
        let existing_remote_dirs: Vec<StandardizedPath> = self
            .displayed_directories
            .iter()
            .filter(|p| self.root_directories.get(p).is_some_and(|r| r.is_remote()))
            .cloned()
            .collect();

        // Only local paths participate in ancestor-dedup.
        let local_inputs: Vec<StandardizedPath> = std_paths
            .iter()
            .filter(|p| !self.root_directories.get(p).is_some_and(|r| r.is_remote()))
            .cloned()
            .collect();

        // Ancestor-dedup only local inputs. Shared with `GlobalSearchView`
        // via `warp_util::path::group_roots_by_common_ancestor`.
        let grouping = warp_util::path::group_roots_by_common_ancestor(&local_inputs);

        // Final displayed order: local surviving roots (in input order),
        // followed by preserved remote roots (in their existing order).
        let new_displayed: Vec<StandardizedPath> = grouping
            .roots
            .iter()
            .cloned()
            .chain(existing_remote_dirs.iter().cloned())
            .collect();

        // Capture the selected item's path and root before we mutate
        // per-root state, so we can remap selection across absorption.
        let prior_selected_path = self.selected_item_std_path();
        let prior_selected_root = self.selected_item.as_ref().map(|id| id.root.clone());

        // Migrate per-root state (expanded folders, item states, explicit
        // collapses) from each absorbed descendant into its surviving
        // ancestor. This must happen before we retain-drop the absorbed
        // entries from `root_directories`.
        for (ancestor, absorbed) in &grouping.absorbed_by_root {
            self.migrate_absorbed_root_state(ancestor, absorbed);
        }

        #[cfg(feature = "local_fs")]
        let new_last_directory = new_displayed.last() != self.displayed_directories.last();

        // Unregister any lazy-loaded paths that are no longer displayed
        // (includes absorbed descendants that were standalone-registered).
        #[cfg(feature = "local_fs")]
        {
            let removed_lazy_loaded_paths: Vec<StandardizedPath> = self
                .registered_lazy_loaded_paths
                .iter()
                .filter(|p| !new_displayed.contains(p))
                .cloned()
                .collect();
            for path in removed_lazy_loaded_paths {
                self.remove_lazy_loaded_entry(&path, ctx);
            }
        }

        // Retain roots that are in `new_displayed`. Remote roots that
        // were properly pushed by `set_remote_root_directories` are
        // already included via the `existing_remote_dirs` chain above.
        self.root_directories
            .retain(|root, _| new_displayed.contains(root));
        self.displayed_directories = new_displayed.clone();
        // Only update local roots — remote roots are managed by
        // `set_remote_root_directories` and must not be passed to
        // `update_directory_contents` which would overwrite their
        // remote-backed entry with an empty local lazy-loaded one.
        #[cfg(feature = "local_fs")]
        {
            let local_displayed: Vec<_> = new_displayed
                .iter()
                .filter(|p| !self.root_directories.get(p).is_some_and(|r| r.is_remote()))
                .cloned()
                .collect();
            self.update_directory_contents(&local_displayed, new_last_directory, ctx);
        }

        // Auto-expand the ancestor chain down to each absorbed descendant.
        // `expand_ancestors_to_path` expands the descendant's parents (and
        // respects `explicitly_collapsed`); we additionally expand the
        // descendant itself so its contents are visible, unless a link in
        // the chain is explicitly collapsed.
        let absorbed_by_root: Vec<(StandardizedPath, Vec<StandardizedPath>)> = grouping
            .absorbed_by_root
            .iter()
            .map(|(a, d)| (a.clone(), d.clone()))
            .collect();
        for (ancestor, absorbed) in &absorbed_by_root {
            for descendant in absorbed {
                self.expand_ancestors_to_path(ancestor, descendant, ctx);
                // Expand the descendant itself only if the parent chain up
                // to it is fully expanded (i.e., `expand_ancestors_to_path`
                // did not bail at an explicit collapse) and the descendant
                // is not itself explicitly collapsed.
                let parent = descendant.parent();
                let chain_expanded = match &parent {
                    Some(p) if p == ancestor => true,
                    Some(p) => self
                        .root_directories
                        .get(ancestor)
                        .is_some_and(|r| r.expanded_folders.contains(p)),
                    None => false,
                };
                if chain_expanded && !self.is_explicitly_collapsed(ancestor, descendant) {
                    if let Some(root_dir) = self.root_directories.get_mut(ancestor) {
                        root_dir.expanded_folders.insert(descendant.clone());
                    }
                    self.ensure_loaded_path(ancestor, descendant, ctx);
                }
            }
        }

        // Re-flatten because we may have expanded new folders above.
        if !absorbed_by_root.is_empty() {
            self.rebuild_flattened_items();
        }

        // Selection remap: if the prior selection's root was absorbed, look
        // up the prior path in the surviving ancestor's items.
        if let (Some(selected_path), Some(old_root)) = (prior_selected_path, prior_selected_root) {
            let was_absorbed = absorbed_by_root
                .iter()
                .any(|(_, absorbed)| absorbed.contains(&old_root));
            if was_absorbed {
                self.selected_item = None;
                for new_root in &new_displayed {
                    if let Some(root_dir) = self.root_directories.get(new_root) {
                        if let Some((index, _)) = root_dir
                            .items
                            .iter()
                            .enumerate()
                            .find(|(_, item)| item.path() == &selected_path)
                        {
                            self.selected_item = Some(FileTreeIdentifier {
                                root: new_root.clone(),
                                index,
                            });
                            break;
                        }
                    }
                }
            }
        }

        // Focus-follow: for the most-recent surviving local root, if it
        // absorbed descendants, select + scroll to the most-recent absorbed
        // descendant's directory header so the user's cwd is visible. If
        // the descendant isn't materialized yet (e.g. the ancestor is still
        // indexing), record it as the pending focus target so we can retry
        // once a later rebuild makes it available.
        //
        // Skip the pending target when the current selection is already
        // at or under the would-be descendant. This prevents an explicit
        // user selection (e.g. a file that was just clicked) from being
        // overridden when `DirectoriesChanged` fires as a side effect of
        // the code view opening that file.
        self.pending_focus_target = None;
        if let Some(first_local) = grouping.roots.first() {
            if let Some(absorbed) = grouping.absorbed_by_root.get(first_local) {
                if let Some(most_recent) = absorbed.first() {
                    let selection_is_under_target = self
                        .selected_item_std_path()
                        .is_some_and(|p| p.starts_with(most_recent));
                    if !selection_is_under_target {
                        self.pending_focus_target = Some(PendingFocusTarget {
                            root: first_local.clone(),
                            path: most_recent.clone(),
                            scrolled: false,
                        });
                    }
                }
            }
        }
        self.apply_pending_focus_target();
        self.close_delete_dialog_if_stale(ctx);
    }

    /// Attempts to select `pending_focus_target` if it is currently
    /// materialized in the root's flattened items. The target is kept
    /// across successful applies so subsequent rebuilds (e.g. a follow-up
    /// `auto_expand_to_most_recent_directory` on repo-metadata updates)
    /// do not override the cwd-follow selection. The target is cleared
    /// only when the user takes an explicit focus-changing action (see
    /// `select_id`, `toggle_folder_expansion`) or when the target root
    /// stops being displayed.
    ///
    /// Scrolling happens only on the first successful apply, so later
    /// metadata-driven rebuilds cannot snap the user's current scroll
    /// position back to the focus target. Returns `true` if the target
    /// was applied this call.
    fn apply_pending_focus_target(&mut self) -> bool {
        let Some(target) = self.pending_focus_target.clone() else {
            return false;
        };
        if !self.displayed_directories.contains(&target.root) {
            // The target root is no longer displayed; drop the pending target.
            self.pending_focus_target = None;
            return false;
        }
        let Some(id) = self.find_directory_header_id(&target.root, &target.path) else {
            return false;
        };
        self.selected_item = Some(id.clone());
        if !target.scrolled {
            // Scroll the cwd-absorbed descendant to the top of the viewport
            // so it behaves like a fresh repo root landing at the top of
            // the tree, rather than merely being made visible at the
            // current scroll position.
            self.perform_scroll_to_top(&id);
            if let Some(target) = self.pending_focus_target.as_mut() {
                target.scrolled = true;
            }
        }
        true
    }

    /// Merges per-root state from `absorbed` descendants into their
    /// surviving `ancestor` root. Removes the absorbed roots' entries
    /// from `root_directories` and `explicitly_collapsed`.
    fn migrate_absorbed_root_state(
        &mut self,
        ancestor: &StandardizedPath,
        absorbed: &[StandardizedPath],
    ) {
        // Ensure the ancestor has a `RootDirectory` to merge into. If it
        // doesn't already exist, start empty; `update_directory_contents`
        // will fill in the backing entry.
        self.root_directories
            .entry(ancestor.clone())
            .or_insert_with(|| RootDirectory {
                entry: Self::create_empty_entry(ancestor),
                expanded_folders: HashSet::new(),
                items: Vec::new(),
                item_states: HashMap::new(),
                remote_host_id: None,
            });

        for absorbed_root in absorbed {
            let Some(old) = self.root_directories.remove(absorbed_root) else {
                continue;
            };
            if let Some(ancestor_dir) = self.root_directories.get_mut(ancestor) {
                ancestor_dir.expanded_folders.extend(old.expanded_folders);
                for (path, state) in old.item_states {
                    ancestor_dir.item_states.entry(path).or_insert(state);
                }
            }
            if let Some(collapsed) = self.explicitly_collapsed.remove(absorbed_root) {
                self.explicitly_collapsed
                    .entry(ancestor.clone())
                    .or_default()
                    .extend(collapsed);
            }
        }
    }

    /// Finds the `FileTreeIdentifier` for the directory header at
    /// `directory_path` inside `root`, if it exists in the root's
    /// flattened items.
    fn find_directory_header_id(
        &self,
        root: &StandardizedPath,
        directory_path: &StandardizedPath,
    ) -> Option<FileTreeIdentifier> {
        let root_dir = self.root_directories.get(root)?;
        let (index, _) = root_dir.items.iter().enumerate().find(|(_, item)| {
            matches!(item, FileTreeItem::DirectoryHeader { .. }) && item.path() == directory_path
        })?;
        Some(FileTreeIdentifier {
            root: root.clone(),
            index,
        })
    }

    pub fn set_has_terminal_session(
        &mut self,
        has_terminal_session: bool,
        ctx: &mut ViewContext<Self>,
    ) {
        self.has_terminal_session = has_terminal_session;
        ctx.notify();
    }

    /// Updates the contents of the directories in the file tree.
    /// Will not add new root directories to the file tree.
    #[cfg(feature = "local_fs")]
    fn update_directory_contents(
        &mut self,
        paths: &[StandardizedPath],
        should_expand_last_directory: bool,
        ctx: &mut ViewContext<Self>,
    ) {
        for root_path in paths {
            self.root_directories
                .entry(root_path.clone())
                .or_insert_with(|| RootDirectory {
                    entry: Self::create_empty_entry(root_path),
                    expanded_folders: HashSet::new(),
                    items: Vec::new(),
                    item_states: HashMap::new(),
                    remote_host_id: None,
                });
            let root_local = root_path.to_local_path_lossy();
            if let Some(repo_root) = DetectedRepositories::as_ref(ctx)
                .get_root_for_path(&LocalOrRemotePath::Local(root_local))
                .and_then(|r| PathBuf::try_from(r).ok())
            {
                let repo_entry = {
                    let repo_metadata = RepoMetadataModel::as_ref(ctx);
                    let Some(id) = repo_metadata::RepositoryIdentifier::try_local(&repo_root)
                    else {
                        continue;
                    };
                    match repo_metadata.repository_state(&id, ctx) {
                        Some(IndexedRepoState::Indexed(state))
                            if state.entry.contains(root_path) =>
                        {
                            Some(state.entry.clone())
                        }
                        Some(IndexedRepoState::Pending(_)) => {
                            // Repo is being (re-)indexed. Keep whatever entry
                            // we already have so the tree doesn't flash to a
                            // loading state during the transition.
                            continue;
                        }
                        _ => None,
                    }
                };

                if let Some(repo_entry) = repo_entry {
                    // This displayed root was previously tracked as a standalone lazy-loaded
                    // path. Once the git repo finishes indexing, drop that standalone
                    // registration so the repo-backed entry becomes the single source of truth.
                    self.remove_lazy_loaded_entry(root_path, ctx);
                    if let Some(root_dir) = self.root_directories.get_mut(root_path) {
                        root_dir.entry = repo_entry;
                    }
                } else {
                    self.register_and_refresh_lazy_loaded_directory(root_path, ctx);
                }
            } else {
                self.register_and_refresh_lazy_loaded_directory(root_path, ctx);
            }
        }

        // Expand the last directory if requested.
        // Respect explicit user collapse of the root header.
        if should_expand_last_directory {
            if let Some(displayed_root) = self.displayed_directories.last().cloned() {
                if !self.is_explicitly_collapsed(&displayed_root, &displayed_root) {
                    self.ensure_loaded_path(&displayed_root, &displayed_root, ctx);
                    if let Some(root_dir) = self.root_directories.get_mut(&displayed_root) {
                        root_dir.expanded_folders.insert(displayed_root.clone());
                    }
                }
            }
        }

        // Ensure all expanded folders have their children loaded
        for root_path in self.displayed_directories.clone() {
            if let Some(root_dir) = self.root_directories.get(&root_path) {
                let expanded_folders: Vec<StandardizedPath> =
                    root_dir.expanded_folders.iter().cloned().collect();
                for folder_path in expanded_folders {
                    self.ensure_loaded_path(&root_path, &folder_path, ctx);
                }
            }
        }

        self.rebuild_flattened_items();
        ctx.notify();
    }

    fn ensure_loaded_path(
        &mut self,
        root_path: &StandardizedPath,
        path: &StandardizedPath,
        ctx: &mut ViewContext<Self>,
    ) {
        let Some(root_dir) = self.root_directories.get(root_path) else {
            return;
        };

        let Some(target_item) = root_dir.entry.get(path).cloned() else {
            return;
        };
        if target_item.loaded() {
            return;
        }

        let FileTreeEntryState::Directory(_) = target_item else {
            return;
        };

        self.load_directory_from_model(root_path, &target_item, ctx);
    }

    #[cfg(feature = "local_fs")]
    fn load_directory_from_model(
        &mut self,
        root_path: &StandardizedPath,
        target_item: &FileTreeEntryState,
        ctx: &mut ViewContext<Self>,
    ) {
        // Check if this is a remote-backed root.
        if self
            .root_directories
            .get(root_path)
            .is_some_and(|r| r.is_remote())
        {
            self.load_remote_directory(root_path, target_item, ctx);
            return;
        }

        let Some(root_dir) = self.root_directories.get(root_path) else {
            return;
        };
        let backing_root = (**root_dir.entry.root_directory()).clone();
        // Expand directories through the model-backed tree. If the backing root is no longer
        // registered, leave the stale entry alone until the next view/model resync.
        let backing_id = repo_metadata::RepositoryIdentifier::local(backing_root.clone());
        if RepoMetadataModel::as_ref(ctx)
            .get_repository(&backing_id, ctx)
            .is_none()
        {
            return;
        }

        let dir_path = target_item.path().clone();
        let load_result =
            self.repository_metadata_model
                .update(ctx, |model: &mut RepoMetadataModel, ctx| {
                    model.load_directory(&backing_root, &dir_path, ctx)
                });
        if let Err(error) = load_result {
            log::warn!("Failed to load directory {dir_path}: {error}");
        }

        if let Some(state) = RepoMetadataModel::as_ref(ctx).get_repository(&backing_id, ctx) {
            if let Some(root_dir) = self.root_directories.get_mut(root_path) {
                root_dir.entry = state.entry.clone();
            }
        }
    }

    /// Sends a `LoadRepoMetadataDirectory` request to the remote server for
    /// an unloaded subdirectory. The response flows back through
    /// `RepoMetadataEvent::FileTreeEntryUpdated { Remote }` which rebuilds
    /// the view automatically.
    #[cfg(feature = "local_fs")]
    fn load_remote_directory(
        &self,
        root_path: &StandardizedPath,
        target_item: &FileTreeEntryState,
        ctx: &mut ViewContext<Self>,
    ) {
        use crate::remote_server::manager::RemoteServerManager;

        if !FeatureFlag::SshRemoteServer.is_enabled() {
            return;
        }

        let Some(root_dir) = self.root_directories.get(root_path) else {
            return;
        };
        let repo_root = root_dir.entry.root_directory().to_string();
        let dir_path = target_item.path().to_string();
        let Some(host_id) = root_dir.remote_host_id.as_ref() else {
            log::warn!("load_remote_directory: no host_id for {root_path}");
            return;
        };

        // Find a connected session for the host that owns this remote root.
        let mgr = RemoteServerManager::as_ref(ctx);
        let Some(sessions) = mgr.sessions_for_host(host_id) else {
            log::warn!("load_remote_directory: no sessions for host {host_id}");
            return;
        };
        // Any session for this host suffices – they all share the same remote
        // server process, so any one of them can service the request.
        let Some(&session_id) = sessions.iter().next() else {
            return;
        };

        RemoteServerManager::handle(ctx).update(ctx, |mgr, ctx| {
            mgr.load_remote_repo_metadata_directory(session_id, repo_root, dir_path, ctx);
        });
    }

    #[cfg(not(feature = "local_fs"))]
    fn load_directory_from_model(
        &mut self,
        _root_path: &StandardizedPath,
        _target_item: &FileTreeEntryState,
        _ctx: &mut ViewContext<Self>,
    ) {
    }

    /// Toggles the expansion state of a folder
    pub fn toggle_folder_expansion(
        &mut self,
        root_path: &StandardizedPath,
        folder_path: &StandardizedPath,
        ctx: &mut ViewContext<Self>,
    ) {
        // User toggled expansion; supersede any pending focus-follow.
        self.pending_focus_target = None;
        if let Some(root_dir) = self.root_directories.get_mut(root_path) {
            if root_dir.expanded_folders.contains(folder_path) {
                root_dir.expanded_folders.remove(folder_path);

                // Track explicit collapse so auto-expansion doesn't override user intent.
                self.explicitly_collapsed
                    .entry(root_path.clone())
                    .or_default()
                    .insert(folder_path.clone());
            } else {
                root_dir.expanded_folders.insert(folder_path.clone());
                self.ensure_loaded_path(root_path, folder_path, ctx);

                if let Some(collapsed) = self.explicitly_collapsed.get_mut(root_path) {
                    collapsed.remove(folder_path);
                    if collapsed.is_empty() {
                        self.explicitly_collapsed.remove(root_path);
                    }
                }
            }
        }

        self.rebuild_flattened_items();
        ctx.notify();
    }

    fn is_folder_expanded(&self, root_path: &StandardizedPath, path: &StandardizedPath) -> bool {
        self.root_directories
            .get(root_path)
            .map(|root_dir| root_dir.expanded_folders.contains(path))
            .unwrap_or(false)
    }

    /// Ensures a displayed standalone directory is registered with
    /// [`LocalRepoMetadataModel`] as a lazily-loaded path while the file tree is active, then
    /// refreshes this view's directory entry from the model.
    #[cfg(feature = "local_fs")]
    fn register_and_refresh_lazy_loaded_directory(
        &mut self,
        path: &StandardizedPath,
        ctx: &mut ViewContext<Self>,
    ) {
        // Ensure the root directory entry exists.
        self.root_directories
            .entry(path.clone())
            .or_insert_with(|| RootDirectory {
                entry: Self::create_empty_entry(path),
                expanded_folders: HashSet::new(),
                items: Vec::new(),
                item_states: HashMap::new(),
                remote_host_id: None,
            });
        // When the file tree is active, index the lazy-loaded path through the
        // model so that a file watcher is started.
        if self.is_active && !self.registered_lazy_loaded_paths.contains(path) {
            let index_result = self
                .repository_metadata_model
                .update(ctx, |model: &mut RepoMetadataModel, ctx| {
                    model.index_lazy_loaded_path(path, ctx)
                });
            if let Err(error) = &index_result {
                log::warn!("Failed to index lazy-loaded path {path}: {error}");
            }
            if RepoMetadataModel::as_ref(ctx).is_lazy_loaded_path(path, ctx) {
                self.registered_lazy_loaded_paths.insert(path.clone());
            }
        }

        let id = repo_metadata::RepositoryIdentifier::local(path.clone());
        let repo_state = RepoMetadataModel::as_ref(ctx).repository_state(&id, ctx);
        if let Some(root_dir) = self.root_directories.get_mut(path) {
            match repo_state {
                Some(IndexedRepoState::Indexed(state)) => {
                    root_dir.entry = state.entry.clone();
                }
                Some(IndexedRepoState::Pending(_)) => {
                    // Repo is being (re-)indexed. Keep whatever entry we already
                    // have so the tree doesn't flash back to a loading state
                    // during the Pending → Indexed transition.
                }
                Some(IndexedRepoState::Failed(_)) | None => {
                    root_dir.entry = Self::create_empty_entry(path);
                }
            }
        }
    }

    fn create_empty_entry(path: &StandardizedPath) -> FileTreeEntry {
        FileTreeEntry::new_for_directory(Arc::new(path.clone()))
    }

    /// Rebuilds the flattened items list for a single root directory only,
    /// leaving all other roots untouched. Use this when only one root's
    /// backing data has changed (e.g. a metadata update) to avoid
    /// unnecessarily re-flattening — and re-rendering — unrelated roots.
    fn rebuild_flattened_items_for_root(&mut self, target_root: &StandardizedPath) {
        self.rebuild_flatten_items_impl(None, None, Some(target_root));
    }

    /// Rebuilds the flattened items list from the current entry tree, optionally removing an item.
    fn rebuild_flattened_items(&mut self) {
        self.rebuild_flatten_items_impl(None, None, None);
    }

    fn rebuild_flattened_items_without(&mut self, path_to_remove: &StandardizedPath) -> bool {
        self.rebuild_flatten_items_impl(None, Some(path_to_remove), None)
    }

    /// Core implementation for rebuilding the flattened items list.
    ///
    /// When `target_root` is `Some`, only that root is re-flattened; all
    /// other roots keep their existing items. When `None`, every displayed
    /// root is rebuilt.
    ///
    /// If `id_to_select` is `Some`, the item identified by that
    /// `FileTreeIdentifier` will be selected. If `path_to_remove` is
    /// `Some`, the item at that path will be excluded from the result.
    ///
    /// Returns `true` if an item was removed.
    fn rebuild_flatten_items_impl(
        &mut self,
        id_to_select: Option<&FileTreeIdentifier>,
        path_to_remove: Option<&StandardizedPath>,
        target_root: Option<&StandardizedPath>,
    ) -> bool {
        let mut any_item_removed = false;

        // Clone the ID to preserve so we don't hold a borrow on self.selected_item
        let id_to_preserve = id_to_select.cloned().or_else(|| self.selected_item.clone());

        // Process displayed directories, optionally filtering to a single root.
        for root_path in self.displayed_directories.clone() {
            if let Some(target) = target_root {
                if root_path != *target {
                    continue;
                }
            }

            let Some(root_dir) = self.root_directories.get(&root_path) else {
                continue;
            };

            let mut items = Vec::with_capacity(root_dir.items.len());
            let entry_state = root_dir.entry.clone();

            // Only get the path for the item if this is the root that contains the selection
            let selected_item_path = id_to_preserve
                .as_ref()
                .filter(|id| id.root == root_path)
                .and_then(|id| root_dir.items.get(id.index).map(|item| item.path().clone()));

            let (new_index, removed_item) = self.flatten_entry_for_root(
                &root_path,
                &root_path,
                &entry_state,
                0,
                selected_item_path.as_ref(),
                path_to_remove,
                &mut items,
            );

            if let Some(root_dir) = self.root_directories.get_mut(&root_path) {
                root_dir.items = items;
            }

            // If we found the selection in this root, update selected_item.
            // If the selection was expected but not found (e.g. filtered out as hidden),
            // clear selected_item to avoid stale references.
            if let Some(id) = id_to_preserve.as_ref() {
                if id.root == root_path {
                    if let Some(index) = new_index {
                        self.selected_item = Some(FileTreeIdentifier {
                            root: root_path,
                            index,
                        });
                    } else if selected_item_path.is_some() {
                        self.selected_item = None;
                    }
                }
            }

            any_item_removed = any_item_removed || removed_item;
        }

        any_item_removed
    }
    /// Recursively flattens entries for a specific root.
    /// Returns the index of the item matching `path_of_selected_item` within this root, if it exists.
    #[allow(clippy::too_many_arguments)]
    fn flatten_entry_for_root(
        &mut self,
        root_path: &StandardizedPath,
        current_path: &StandardizedPath,
        entry_map: &FileTreeEntry,
        depth: usize,
        path_of_selected_item: Option<&StandardizedPath>,
        path_of_removed_item: Option<&StandardizedPath>,
        items: &mut Vec<FileTreeItem>,
    ) -> (Option<usize>, bool) {
        let mut selected_item_index = None;
        let mut removed_item = false;

        if path_of_removed_item == Some(current_path) {
            return (None, true);
        }

        // Filter hidden files/directories when show_hidden_files is disabled.
        // Only filter descendants (depth > 0), not the root entry itself,
        // so that hidden workspace directories (e.g. ~/.config) are still shown.
        if !self.show_hidden_files && depth > 0 {
            if let Some(name) = current_path.file_name() {
                if name.starts_with('.') {
                    return (selected_item_index, removed_item);
                }
            }
        }

        if path_of_selected_item == Some(current_path) {
            selected_item_index = Some(items.len());
        }

        // Get item_states from the root directory
        let root_dir = self.root_directories.get_mut(root_path);

        match entry_map.get(current_path).cloned() {
            Some(FileTreeEntryState::File(file)) => {
                let file_std_path = (*file.path).clone();
                let (mouse_state_handle, draggable_state) = if let Some(root_dir) = root_dir {
                    root_dir
                        .item_states
                        .entry(file_std_path)
                        .or_insert_with(|| (MouseStateHandle::default(), DraggableState::default()))
                        .clone()
                } else {
                    (MouseStateHandle::default(), DraggableState::default())
                };

                items.push(FileTreeItem::File {
                    metadata: file.clone(),
                    depth,
                    mouse_state_handle,
                    draggable_state,
                });
            }
            Some(FileTreeEntryState::Directory(dir)) => {
                let is_expanded = self.is_folder_expanded(root_path, &dir.path);
                let dir_std_path = (*dir.path).clone();
                let (mouse_state_handle, draggable_state) = if let Some(root_dir) =
                    self.root_directories.get_mut(root_path)
                {
                    root_dir
                        .item_states
                        .entry(dir_std_path)
                        .or_insert_with(|| (MouseStateHandle::default(), DraggableState::default()))
                        .clone()
                } else {
                    (MouseStateHandle::default(), DraggableState::default())
                };

                items.push(FileTreeItem::DirectoryHeader {
                    directory: dir.clone(),
                    depth,
                    mouse_state_handle,
                    draggable_state,
                });

                // Add children if expanded
                if is_expanded {
                    for child in entry_map
                        .child_paths(&dir.path)
                        .sorted_by(|a, b| sort_entries_for_file_tree(a, b, entry_map))
                    {
                        let (child_selected_index, child_removed) = self.flatten_entry_for_root(
                            root_path,
                            child,
                            entry_map,
                            depth + 1,
                            path_of_selected_item,
                            path_of_removed_item,
                            items,
                        );

                        if selected_item_index.is_none() {
                            selected_item_index = child_selected_index;
                        }

                        removed_item = removed_item || child_removed;
                    }
                }
            }
            None => {
                return (selected_item_index, removed_item);
            }
        }

        (selected_item_index, removed_item)
    }

    /// Renders a file item with proper indentation and optional hover/selected styling
    fn render_item_with_hover(
        render_state: RenderState,
        appearance: &Appearance,
        item_highlight_state: ItemHighlightState,
        editor_view: Option<&ViewHandle<EditorView>>,
    ) -> Box<dyn Element> {
        // Create the folder header row
        let mut header_row = Flex::row()
            .with_main_axis_size(MainAxisSize::Max)
            .with_cross_axis_alignment(CrossAxisAlignment::Center);

        // Add an indentation spacer based on the depth of the item in the file tree.
        if render_state.depth > 0 {
            header_row.add_child(
                Container::new(
                    ConstrainedBox::new(Empty::new().finish())
                        .with_width(render_state.depth as f32 * FOLDER_INDENT)
                        .finish(),
                )
                .finish(),
            );
        }

        // Add expand/collapse button if the item is expandable.
        let expand_icon = render_state.is_expanded.map(|expanded| {
            if expanded {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            }
        });

        let expand_icon = match expand_icon {
            Some(icon) => {
                let chevron_icon_color = item_highlight_state.text_and_icon_color(appearance);
                icon.to_warpui_icon(chevron_icon_color.into()).finish()
            }
            None => Empty::new().finish(),
        };

        header_row.add_child(
            Container::new(
                ConstrainedBox::new(expand_icon)
                    .with_width(FOLDER_INDENT)
                    .with_height(FOLDER_INDENT)
                    .finish(),
            )
            .with_margin_right(4.)
            .finish(),
        );

        // Add the icon for the item.
        let icon_color = item_highlight_state.text_and_icon_color(appearance);
        let icon = match render_state.icon {
            ImageOrIcon::Icon(icon) => icon.to_warpui_icon(icon_color.into()).finish(),
            ImageOrIcon::Image(image) => image,
        };
        header_row.add_child(
            Container::new(
                ConstrainedBox::new(icon)
                    .with_width(FOLDER_INDENT)
                    .with_height(FOLDER_INDENT)
                    .finish(),
            )
            .with_margin_right(8.)
            .finish(),
        );

        let text_color = item_highlight_state.text_and_icon_color(appearance);
        let text_style = if render_state.is_ignored {
            Properties::default()
                .style(Style::Italic)
                .weight(Weight::Light)
        } else {
            Properties::default()
        };
        match editor_view {
            Some(editor_view) => {
                header_row.add_child(
                    Shrinkable::new(
                        1.,
                        Dismiss::new(Clipped::new(ChildView::new(editor_view).finish()).finish())
                            .on_dismiss(|ctx, _app| {
                                ctx.dispatch_typed_action(FileTreeAction::DismissEditor);
                            })
                            .finish(),
                    )
                    .finish(),
                );
            }
            None => {
                header_row.add_child(
                    Shrinkable::new(
                        1.,
                        Text::new_inline(
                            render_state.display_name,
                            appearance.ui_font_family(),
                            ITEM_FONT_SIZE,
                        )
                        .with_color(text_color)
                        .with_style(text_style)
                        .finish(),
                    )
                    .finish(),
                );
            }
        }

        let mut container = Container::new(header_row.finish())
            .with_padding_top(ITEM_PADDING)
            .with_padding_bottom(ITEM_PADDING)
            .with_padding_left(8.)
            .with_padding_right(8.);

        if let Some(background_color) = item_highlight_state.background_color(appearance) {
            container = container.with_background(background_color);
        }

        if let Some(corner_radius) = item_highlight_state.corner_radius() {
            container = container.with_corner_radius(corner_radius);
        }

        container.finish()
    }

    fn is_item_expanded(&self, root_path: &StandardizedPath, item: &FileTreeItem) -> Option<bool> {
        match item {
            FileTreeItem::File { .. } => None,
            FileTreeItem::DirectoryHeader { directory, .. } => {
                Some(self.is_folder_expanded(root_path, &directory.path))
            }
        }
    }

    fn render_item_while_dragging(
        &self,
        id: &FileTreeIdentifier,
        appearance: &Appearance,
    ) -> Box<dyn Element> {
        let Some(root_dir) = self.root_directories.get(&id.root) else {
            return Empty::new().finish();
        };
        let Some(item) = root_dir.items.get(id.index) else {
            return Empty::new().finish();
        };

        let item_highlight_state = ItemHighlightState::Selected;
        let render_state = item.to_render_state(None /* is_expanded */, appearance);

        let text_color = item_highlight_state.text_and_icon_color(appearance);
        let text = Text::new(
            render_state.display_name,
            appearance.ui_font_family(),
            ITEM_FONT_SIZE,
        )
        .with_color(text_color)
        .finish();

        let mut container = Container::new(text)
            .with_padding_top(ITEM_PADDING)
            .with_padding_bottom(ITEM_PADDING)
            .with_padding_left(8.)
            .with_padding_right(8.)
            .with_background(appearance.theme().background());

        if let Some(corner_radius) = item_highlight_state.corner_radius() {
            container = container.with_corner_radius(corner_radius);
        }

        container.finish()
    }

    /// Renders a clickable tree item with mouse state handle
    fn render_item(&self, id: &FileTreeIdentifier, appearance: &Appearance) -> Box<dyn Element> {
        let Some(root_dir) = self.root_directories.get(&id.root) else {
            return Empty::new().finish();
        };
        let Some(item) = root_dir.items.get(id.index) else {
            return Empty::new().finish();
        };

        let is_selected = self.selected_item.as_ref() == Some(id);
        let is_expanded = self.is_item_expanded(&id.root, item);
        let render_state = item.to_render_state(is_expanded, appearance);

        let item_display_name = render_state.display_name.clone();
        let item_position_id = format!("file_tree_item:{item_display_name}");

        let position_id = self.position_id.clone();
        let draggable_state = render_state.draggable_state.clone();

        let is_pending_edit = self
            .pending_edit
            .as_ref()
            .map(|pending_edit| &pending_edit.id == id)
            .unwrap_or(false);

        let editor_view = is_pending_edit.then_some(&self.editor_view);
        let id_for_click = id.clone();
        let id_for_context = id.clone();
        let id_for_drop = id.clone();
        let id_for_drag = id.clone();
        let hoverable = Hoverable::new(render_state.mouse_state.clone(), move |mouse_state| {
            let item_highlight_state = ItemHighlightState::new(is_selected, mouse_state);
            Self::render_item_with_hover(
                render_state,
                appearance,
                item_highlight_state,
                editor_view,
            )
        })
        .on_click(
            move |event_ctx: &mut EventContext, _app_ctx: &AppContext, _position| {
                // Dispatch the action to select this item
                event_ctx.dispatch_typed_action(FileTreeAction::ItemClicked {
                    id: id_for_click.clone(),
                });
            },
        )
        .on_right_click(
            move |event: &mut EventContext, _app_ctx: &AppContext, position| {
                let Some(parent_bounds) = event.element_position_by_id(&position_id) else {
                    return;
                };
                // Compute the position of the context menu relative to the parent bounds of the file tree.
                let offset = position - parent_bounds.origin();
                event.dispatch_typed_action(FileTreeAction::OpenContextMenu {
                    position: offset,
                    id: id_for_context.clone(),
                });
            },
        )
        .with_cursor(Cursor::PointingHand)
        .finish();

        let draggable = Draggable::new(draggable_state, hoverable)
            .with_drag_bounds_callback(|_, window_size| {
                Some(RectF::new(
                    pathfinder_geometry::vector::Vector2F::zero(),
                    window_size,
                ))
            })
            .use_copy_cursor_when_dragging_over_drop_target()
            .with_accepted_by_drop_target_fn(move |drop_target_data, _| {
                // Allow drops on terminal input and terminal block list
                if drop_target_data
                    .as_any()
                    .downcast_ref::<InputDropTargetData>()
                    .is_some()
                    || drop_target_data
                        .as_any()
                        .downcast_ref::<TerminalDropTargetData>()
                        .is_some()
                {
                    AcceptedByDropTarget::Yes
                } else {
                    AcceptedByDropTarget::No
                }
            })
            .on_drop(move |ctx, _app, _drag_position, data| {
                if let Some(terminal_input_data) = data
                    .as_ref()
                    .and_then(|data| data.as_any().downcast_ref::<InputDropTargetData>())
                {
                    ctx.dispatch_typed_action(FileTreeAction::ItemDroppedOnInput {
                        id: id_for_drop.clone(),
                        terminal_input_data: terminal_input_data.clone(),
                    });
                } else if let Some(terminal_drop_data) = data
                    .as_ref()
                    .and_then(|data| data.as_any().downcast_ref::<TerminalDropTargetData>())
                {
                    ctx.dispatch_typed_action(FileTreeAction::ItemDroppedOnTerminal {
                        id: id_for_drop.clone(),
                        terminal_view: terminal_drop_data.terminal_view.clone(),
                    });
                }
            })
            .with_alternate_drag_element(self.render_item_while_dragging(&id_for_drag, appearance))
            .with_keep_original_visible(true)
            .finish();

        SavePosition::new(draggable, item_position_id.as_str()).finish()
    }

    fn selected_item_std_path(&self) -> Option<StandardizedPath> {
        self.selected_item.as_ref().and_then(|id| {
            let root_dir = self.root_directories.get(&id.root)?;
            root_dir.items.get(id.index).map(|item| item.path().clone())
        })
    }

    /// Returns the path of the item relative to the repository root.
    fn relative_path_for_item(&self, id: &FileTreeIdentifier) -> Option<PathBuf> {
        let root_dir = self.root_directories.get(&id.root)?;
        let item = root_dir.items.get(id.index)?;

        let repository_root = self.root_for_path(&id.root)?;
        item.path()
            .strip_prefix(&repository_root)
            .map(PathBuf::from)
    }

    /// Selects the first item if no item is selected.
    pub fn select_first_item_if_no_selection(&mut self, ctx: &mut ViewContext<Self>) {
        if self.selected_item.is_none() {
            if let Some(active_dir) = self.displayed_directories.first() {
                let id = FileTreeIdentifier {
                    root: active_dir.clone(),
                    index: 0,
                };
                self.select_id(&id, ctx);
            }
        }
    }

    /// Selects and expands the most recent directory (the current terminal session's working
    /// directory) in the file tree. Since each terminal CWD is now a top-level root, this
    /// expands the root directory and selects its first item.
    ///
    /// Preserves any existing selection so a prior focus-follow (from
    /// `set_root_directories`) or an active-file scroll (from
    /// `scroll_to_file`) is not visibly clobbered by a fallback selection
    /// on the root header.
    pub fn auto_expand_to_most_recent_directory(&mut self, ctx: &mut ViewContext<Self>) {
        let Some(most_recent_dir) = self.displayed_directories.first().cloned() else {
            return;
        };

        if !self.root_directories.contains_key(&most_recent_dir) {
            return;
        }

        // Expand the root directory if it isn't already expanded.
        // Respect explicit user collapse of the root header.
        let needs_rebuild = if self.is_explicitly_collapsed(&most_recent_dir, &most_recent_dir) {
            false
        } else if let Some(root_dir) = self.root_directories.get_mut(&most_recent_dir) {
            if !root_dir.expanded_folders.contains(&most_recent_dir) {
                root_dir.expanded_folders.insert(most_recent_dir.clone());
                true
            } else {
                false
            }
        } else {
            false
        };

        if needs_rebuild {
            self.ensure_loaded_path(&most_recent_dir, &most_recent_dir, ctx);
            self.rebuild_flattened_items();
        }

        if let Some(root_dir) = self.root_directories.get(&most_recent_dir) {
            if root_dir.items.is_empty() {
                return;
            }
        }

        // Override selection only when there is none, or when the current
        // selection lives under a different root than the most-recent one
        // (e.g. the user cd'd to a brand-new root that isn't an ancestor
        // of the previous selection). Preserving the selection when it's
        // already under the most-recent root avoids a visible flash on
        // the root header before a pending focus-follow or active-file
        // scroll lands.
        let should_override_selection = self
            .selected_item
            .as_ref()
            .is_none_or(|id| id.root != most_recent_dir);

        if should_override_selection {
            let id = FileTreeIdentifier {
                root: most_recent_dir,
                index: 0,
            };
            self.selected_item = Some(id.clone());
            self.perform_scroll(&id);
        }
        ctx.notify();
    }

    pub fn on_left_panel_focused(&mut self, ctx: &mut ViewContext<Self>) {
        // Keep the focus entry behavior: auto-expand, then ensure selection is visible.
        self.auto_expand_to_most_recent_directory(ctx);

        if let Some(id) = &self.selected_item {
            self.perform_scroll(&id.clone());
        } else {
            self.select_first_item_if_no_selection(ctx);
        }
    }

    #[cfg(not(feature = "local_fs"))]
    fn open_file(
        &self,
        _path: &Path,
        _editor_layout: Option<EditorLayout>,
        _ctx: &mut ViewContext<Self>,
    ) {
    }

    #[cfg(feature = "local_fs")]
    fn open_file(
        &self,
        path: &Path,
        editor_layout: Option<EditorLayout>,
        ctx: &mut ViewContext<Self>,
    ) {
        let settings = EditorSettings::as_ref(ctx);
        let target = if editor_layout.is_some() {
            resolve_file_target_to_open_in_warp(path, settings, editor_layout)
        } else {
            resolve_file_target_with_editor_choice(
                path,
                *settings.open_code_panels_file_editor,
                *settings.prefer_markdown_viewer,
                *settings.open_file_layout,
                editor_layout,
            )
        };

        send_telemetry_from_ctx!(
            TelemetryEvent::CodePanelsFileOpened {
                entrypoint: CodePanelsFileOpenEntrypoint::ProjectExplorer,
                target: target.clone(),
            },
            ctx
        );

        ctx.emit(FileTreeEvent::OpenFile {
            path: LocalOrRemotePath::Local(path.to_path_buf()),
            target,
            line_col: None,
        });
    }

    fn select_and_execute_item_at_id(
        &mut self,
        id: &FileTreeIdentifier,
        ctx: &mut ViewContext<Self>,
    ) {
        let Some(root_dir) = self.root_directories.get(&id.root) else {
            return;
        };
        let Some(item) = root_dir.items.get(id.index) else {
            return;
        };

        let is_remote = root_dir.is_remote();

        match item {
            FileTreeItem::File { metadata, .. } => {
                if is_remote {
                    // Emit a remote open event if we have a host ID.
                    if let Some(host_id) = &root_dir.remote_host_id {
                        let remote_path = warp_util::remote_path::RemotePath::new(
                            host_id.clone(),
                            (*metadata.path).clone(),
                        );
                        let path_str = metadata.path.as_str();
                        let remote_file_path = Path::new(path_str);
                        let target = if is_jupyter_notebook_file(remote_file_path)
                            && FeatureFlag::JupyterNotebookRendering.is_enabled()
                        {
                            FileTarget::MarkdownViewer(EditorLayout::SplitPane)
                        } else if is_markdown_file(remote_file_path) {
                            #[cfg(feature = "local_fs")]
                            {
                                let prefer_md = *EditorSettings::as_ref(ctx).prefer_markdown_viewer;
                                if prefer_md {
                                    FileTarget::MarkdownViewer(EditorLayout::SplitPane)
                                } else {
                                    FileTarget::CodeEditor(EditorLayout::SplitPane)
                                }
                            }
                            #[cfg(not(feature = "local_fs"))]
                            {
                                FileTarget::CodeEditor(EditorLayout::SplitPane)
                            }
                        } else {
                            FileTarget::CodeEditor(EditorLayout::SplitPane)
                        };
                        ctx.emit(FileTreeEvent::OpenFile {
                            path: LocalOrRemotePath::Remote(remote_path),
                            target,
                            line_col: None,
                        });
                    }
                } else {
                    let path = metadata.path.to_local_path_lossy();
                    self.open_file(&path, None, ctx);
                }
            }
            FileTreeItem::DirectoryHeader { directory, .. } => {
                let dir_std = (*directory.path).clone();
                self.toggle_folder_expansion(&id.root, &dir_std, ctx);
            }
        }

        self.select_id(id, ctx);
    }

    fn select_id(&mut self, id: &FileTreeIdentifier, ctx: &mut ViewContext<Self>) {
        // An explicit selection (user click, keyboard nav, editor focus)
        // supersedes any pending focus-follow target.
        self.pending_focus_target = None;
        self.selected_item = Some(id.clone());
        self.perform_scroll(id);
        ctx.notify();
    }

    /// Handles context menu events (like menu closing)
    fn handle_menu_event(&mut self, event: &crate::menu::Event, ctx: &mut ViewContext<Self>) {
        if let crate::menu::Event::Close { .. } = event {
            self.context_menu_state.take();
        }
        ctx.notify();
    }

    /// Creates menu items for the context menu based on the current item
    fn context_menu_items(
        &self,
        item: &FileTreeItem,
        id: &FileTreeIdentifier,
    ) -> Vec<MenuItem<FileTreeAction>> {
        let is_remote = self.is_remote_item(id);

        let mut items = vec![];

        if is_remote {
            // Remote file trees only support a limited set of actions:
            // copying paths and attaching as context. File opening,
            // creation, rename, delete, cd, and reveal are unavailable
            // because there is no local filesystem or editor support.
        } else {
            match item {
                FileTreeItem::File { .. } => {
                    let path_local = item.path().to_local_path_lossy();
                    if !is_file_content_binary(&path_local) {
                        items.extend([
                            MenuItemFields::new("Open in new pane")
                                .with_on_select_action(FileTreeAction::OpenInNewPane {
                                    id: id.clone(),
                                })
                                .into_item(),
                            MenuItemFields::new("Open in new tab")
                                .with_on_select_action(FileTreeAction::OpenInNewTab {
                                    id: id.clone(),
                                })
                                .into_item(),
                        ]);
                    } else {
                        items.push(
                            MenuItemFields::new("Open file")
                                .with_on_select_action(FileTreeAction::ItemClicked {
                                    id: id.clone(),
                                })
                                .into_item(),
                        );
                    }
                }
                FileTreeItem::DirectoryHeader { .. } => {
                    items.push(
                        MenuItemFields::new("New file")
                            .with_on_select_action(FileTreeAction::NewFileBelowDirectory {
                                id: id.clone(),
                            })
                            .into_item(),
                    );
                    items.push(MenuItem::Separator);
                    if self.has_terminal_session {
                        items.push(
                            MenuItemFields::new("cd to directory")
                                .with_on_select_action(FileTreeAction::CDToDirectory {
                                    id: id.clone(),
                                })
                                .into_item(),
                        );
                    }
                    items.push(
                        MenuItemFields::new("Open in new tab")
                            .with_on_select_action(FileTreeAction::OpenInNewTab { id: id.clone() })
                            .into_item(),
                    );
                }
            };

            let open_text = if cfg!(target_os = "macos") {
                "Reveal in Finder"
            } else if cfg!(target_os = "windows") {
                "Reveal in Explorer"
            } else {
                "Reveal in file manager"
            };
            items.push(
                MenuItemFields::new(open_text)
                    .with_on_select_action(FileTreeAction::OpenInFinder { id: id.clone() })
                    .into_item(),
            );

            // No displayed root, and no folder holding one, can be renamed or deleted from the
            // tree: either would pull the ground out from under the tree itself. This goes by
            // path, since a root that sits inside another root's list isn't at row 0 there. The
            // "New file" row whose name is still being typed isn't on disk yet, so it can't be
            // either; a real file that happens to have the placeholder's name would be the one
            // to go.
            if !self.holds_a_root(item.path()) && !self.is_new_file_placeholder(id) {
                items.push(
                    MenuItemFields::new("Rename")
                        .with_on_select_action(FileTreeAction::Rename { id: id.clone() })
                        .into_item(),
                );
                items.push(
                    MenuItemFields::new("Delete…")
                        .with_on_select_action(FileTreeAction::Delete {
                            id: id.clone(),
                            path: item.path().clone(),
                        })
                        .into_item(),
                );
            }
        }

        if self.has_terminal_session {
            if !items.is_empty() {
                items.push(MenuItem::Separator);
            }
            items.push(
                MenuItemFields::new("Attach as context")
                    .with_on_select_action(FileTreeAction::AttachAsContext { id: id.clone() })
                    .into_item(),
            );
        }

        if !items.is_empty() {
            items.push(MenuItem::Separator);
        }
        items.extend([
            MenuItemFields::new("Copy path")
                .with_on_select_action(FileTreeAction::CopyPath { id: id.clone() })
                .into_item(),
            MenuItemFields::new("Copy relative path")
                .with_on_select_action(FileTreeAction::CopyRelativePath { id: id.clone() })
                .into_item(),
        ]);

        items
    }

    /// Returns the repository root for a specific root directory path.
    fn root_for_path(&self, root_path: &StandardizedPath) -> Option<StandardizedPath> {
        self.root_directories
            .get(root_path)
            .map(|root_dir| (**root_dir.entry.root_directory()).clone())
    }

    fn copy_relative_path_for_id(&mut self, id: &FileTreeIdentifier, ctx: &mut ViewContext<Self>) {
        let Some(relative_path) = self.relative_path_for_item(id) else {
            return;
        };

        ctx.clipboard().write(ClipboardContent::plain_text(
            relative_path.to_string_lossy().to_string(),
        ));
    }

    fn copy_absolute_path_for_id(&mut self, id: &FileTreeIdentifier, ctx: &mut ViewContext<Self>) {
        let Some(root_dir) = self.root_directories.get(&id.root) else {
            return;
        };
        let Some(item) = root_dir.items.get(id.index) else {
            return;
        };

        let path = item.path().as_str();

        ctx.clipboard()
            .write(ClipboardContent::plain_text(path.to_string()));
        ctx.notify();
    }

    fn attach_as_context(&mut self, id: &FileTreeIdentifier, ctx: &mut ViewContext<Self>) {
        let Some(root_dir) = self.root_directories.get(&id.root) else {
            return;
        };
        let Some(item) = root_dir.items.get(id.index) else {
            return;
        };

        let Some(relative_path) = self.relative_path_for_item(id) else {
            return;
        };

        let is_directory = matches!(item, FileTreeItem::DirectoryHeader { .. });
        send_telemetry_from_ctx!(
            TelemetryEvent::FileTreeItemAttachedAsContext { is_directory },
            ctx
        );

        ctx.emit(FileTreeEvent::AttachAsContext {
            path: relative_path,
        });
    }

    fn open_in_new_pane(&mut self, id: &FileTreeIdentifier, ctx: &mut ViewContext<Self>) {
        let Some(root_dir) = self.root_directories.get(&id.root) else {
            return;
        };
        let Some(item) = root_dir.items.get(id.index) else {
            return;
        };

        self.open_file(
            &item.path().to_local_path_lossy(),
            Some(EditorLayout::SplitPane),
            ctx,
        );
    }

    fn open_in_new_tab(&mut self, id: &FileTreeIdentifier, ctx: &mut ViewContext<Self>) {
        let Some(root_dir) = self.root_directories.get(&id.root) else {
            return;
        };
        let Some(item) = root_dir.items.get(id.index) else {
            return;
        };

        let path = item.path().to_local_path_lossy();
        if path.is_dir() {
            ctx.emit(FileTreeEvent::OpenDirectoryInNewTab { path: path.clone() });
        } else {
            self.open_file(&path, Some(EditorLayout::NewTab), ctx);
        }
    }

    fn cd_to_directory(&mut self, id: &FileTreeIdentifier, ctx: &mut ViewContext<Self>) {
        let Some(root_dir) = self.root_directories.get(&id.root) else {
            return;
        };
        let Some(item) = root_dir.items.get(id.index) else {
            return;
        };

        let path = item.path().to_local_path_lossy();

        if !path.is_dir() {
            log::warn!(
                "CDToDirectory called on non-directory path: {}",
                path.display()
            );
            return;
        }

        ctx.emit(FileTreeEvent::CDToDirectory { path });
    }

    fn rename_item(&mut self, id: &FileTreeIdentifier, ctx: &mut ViewContext<Self>) {
        let exists = self
            .root_directories
            .get(&id.root)
            .and_then(|root_dir| root_dir.items.get(id.index))
            .is_some();
        if exists {
            self.start_rename(id, ctx);
        }
    }

    /// Opens the delete dialog for the item the context menu was opened on. The menu captured the
    /// row and the item's path. If the row no longer holds that path, the list was rebuilt in the
    /// meantime, and the request is refused rather than retargeted.
    fn request_delete(
        &mut self,
        id: &FileTreeIdentifier,
        path: &StandardizedPath,
        ctx: &mut ViewContext<Self>,
    ) {
        // Neither is offered Delete… (see `context_menu_items`). Refuse here too, so no route
        // can delete a displayed root, a folder holding one, or the "New file" placeholder.
        if self.holds_a_root(path) || self.is_new_file_placeholder(id) {
            log::warn!("Not deleting {path}: it's a root, holds one, or isn't on disk yet");
            return;
        }
        let row_holds_path = self
            .root_directories
            .get(&id.root)
            .and_then(|root_dir| root_dir.items.get(id.index))
            .is_some_and(|item| item.path() == path);
        if !row_holds_path {
            log::warn!("Not deleting {path}: the file tree changed after its menu opened");
            Self::show_delete_toast(path, STALE_MENU_MESSAGE.to_owned(), ctx);
            return;
        }
        if self.deletes_in_flight.contains(path) {
            return;
        }

        let display_name = path
            .file_name()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| path.to_string());
        let local_path = path.to_local_path_lossy();
        // `lstat` rather than `Path::is_dir`, which follows symbolic links: a link must be
        // deleted as a link, never as the folder it points to.
        let metadata = match std::fs::symlink_metadata(&local_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // Something else removed it. Say so, and stop showing it.
                self.forget_paths(std::slice::from_ref(path));
                ctx.notify();
                Self::show_delete_toast(path, no_longer_exists_message(&display_name), ctx);
                return;
            }
            Err(error) => {
                let message = delete_failed_message(&display_name, &error);
                Self::show_delete_toast(path, message, ctx);
                return;
            }
        };
        let kind = ItemKind::from_file_type(metadata.file_type());
        let child_count = match kind {
            ItemKind::Directory => self
                .root_directories
                .get(&id.root)
                .and_then(|root_dir| loaded_child_count(&root_dir.entry, path)),
            ItemKind::File | ItemKind::Symlink => None,
        };

        let target = PendingDelete {
            std_path: path.clone(),
            local_path,
            kind,
            display_name,
            child_count,
            identity: ItemIdentity::from_metadata(&metadata),
        };
        self.delete_dialog.update(ctx, |dialog, ctx| {
            dialog.set_target(target.clone(), ctx);
        });
        self.pending_delete = Some(target);
        self.pending_delete_roots = self.displayed_directories.clone();
        ctx.focus(&self.delete_dialog);
        ctx.notify();
    }

    /// Whether `path` is a displayed root, or a folder with one inside it. This compares paths,
    /// not rows: a root that sits inside another root's list isn't at row 0 there.
    fn holds_a_root(&self, path: &StandardizedPath) -> bool {
        self.root_directories
            .keys()
            .any(|root| root.starts_with(path))
    }

    /// Whether `id` is the row "New file" adds while the new file's name is being typed. Nothing
    /// is on disk for it yet.
    fn is_new_file_placeholder(&self, id: &FileTreeIdentifier) -> bool {
        self.pending_edit
            .as_ref()
            .is_some_and(|edit| edit.kind == PendingEditKind::CreateNewFile && edit.id == *id)
    }

    fn handle_delete_dialog_event(
        &mut self,
        event: &DeleteFileConfirmationEvent,
        ctx: &mut ViewContext<Self>,
    ) {
        match event {
            DeleteFileConfirmationEvent::Confirm => {
                // The dialog closes as soon as the tree stops showing its list or its roots
                // change, so it shouldn't be confirmed in either state. If it is, nothing is
                // deleted.
                let target = self
                    .pending_delete
                    .clone()
                    .filter(|_| self.delete_dialog_fits_the_tree());
                self.close_delete_dialog(ctx);
                // The file tree takes focus back so arrow-key navigation keeps working.
                ctx.focus_self();
                if let Some(target) = target {
                    self.perform_delete(target, ctx);
                }
            }
            DeleteFileConfirmationEvent::Cancel => {
                self.close_delete_dialog(ctx);
                ctx.focus_self();
            }
            DeleteFileConfirmationEvent::FocusLost => {
                // Focus went somewhere else, so leave it there.
                if self.pending_delete.is_some() {
                    self.close_delete_dialog(ctx);
                }
            }
        }
    }

    /// Hides the delete dialog and forgets what it was asking about.
    fn close_delete_dialog(&mut self, ctx: &mut ViewContext<Self>) {
        self.pending_delete = None;
        self.pending_delete_roots.clear();
        self.delete_dialog
            .update(ctx, |dialog, ctx| dialog.clear_target(ctx));
        ctx.notify();
    }

    /// Whether the open delete dialog still fits what the file tree shows: the tree is showing
    /// its list, not a loading or error message in its place, and displays the same roots as
    /// when the dialog opened.
    fn delete_dialog_fits_the_tree(&self) -> bool {
        self.shows_list()
            && self.displayed_directories.len() == self.pending_delete_roots.len()
            && self
                .displayed_directories
                .iter()
                .all(|root| self.pending_delete_roots.contains(root))
    }

    /// Closes the delete dialog, deleting nothing, once it no longer fits what the file tree
    /// shows. It runs wherever the tree's state changes in a way that could hide the list or
    /// change the roots.
    fn close_delete_dialog_if_stale(&mut self, ctx: &mut ViewContext<Self>) {
        if self.pending_delete.is_some() && !self.delete_dialog_fits_the_tree() {
            log::info!("Closing the delete dialog: the file tree changed while it was open");
            self.close_delete_dialog(ctx);
        }
    }

    /// Deletes the item the dialog showed, in the background. The removal happens only if the
    /// path still names that item: the same kind, with the same identity, as when the dialog
    /// opened. In a test build, it also happens only inside the temp folder.
    fn perform_delete(&mut self, target: PendingDelete, ctx: &mut ViewContext<Self>) {
        #[cfg(any(test, feature = "integration_tests"))]
        if let Some(reason) =
            delete_confirmation_dialog::test_build_delete_refusal(&target.local_path)
        {
            log::error!("A test build refused to delete: {reason}");
            let message = format!(
                "A test build refused to delete \"{}\": {reason}",
                target.display_name
            );
            Self::show_delete_toast(&target.std_path, message, ctx);
            return;
        }
        if !self.deletes_in_flight.insert(target.std_path.clone()) {
            return;
        }
        // For a folder, what the tree has loaded of it, so a delete that stops partway can
        // report which of those items are gone.
        let loaded_items = match target.kind {
            ItemKind::Directory => self.folder_and_loaded_contents(&target.std_path),
            ItemKind::File | ItemKind::Symlink => Vec::new(),
        };
        let local_path = target.local_path.clone();
        let (kind, identity) = (target.kind, target.identity);
        // The check and the removal run in this one job, back to back.
        ctx.spawn(
            blocking::unblock(move || {
                delete_if_unchanged(&local_path, kind, identity, loaded_items)
            }),
            move |me, outcome, ctx| me.finish_delete(target, outcome, ctx),
        );
    }

    fn finish_delete(
        &mut self,
        target: PendingDelete,
        outcome: DeleteOutcome,
        ctx: &mut ViewContext<Self>,
    ) {
        self.deletes_in_flight.remove(&target.std_path);
        match outcome {
            DeleteOutcome::Deleted => {
                // Update in-memory tree immediately so the UI reflects the deletion.
                // Find all root directories that contain this path (could be nested roots).
                let affected_roots: Vec<StandardizedPath> = self
                    .root_directories
                    .keys()
                    .filter(|root_path| target.std_path.starts_with(root_path))
                    .cloned()
                    .collect();

                // Update each affected root
                for root_path in &affected_roots {
                    let removed = self.rebuild_flattened_items_without(&target.std_path);
                    if !removed {
                        log::warn!(
                            "FileTreeView.delete: did not find {} in root {} in-memory model to remove",
                            target.local_path.display(),
                            root_path
                        );
                    }
                }

                // Emit event to notify workspace that a file was deleted
                ctx.emit(FileTreeEvent::FileDeleted {
                    path: target.local_path,
                });
            }
            DeleteOutcome::Changed => {
                log::warn!(
                    "Not deleting {}: it changed after the delete dialog opened",
                    target.local_path.display()
                );
                let message = changed_since_request_message(&target.display_name);
                Self::show_delete_toast(&target.std_path, message, ctx);
            }
            DeleteOutcome::Missing => {
                // Something else removed it. Say so, and stop showing it. No `FileDeleted`: an
                // editor tab open on it keeps whatever it holds.
                self.forget_paths(std::slice::from_ref(&target.std_path));
                let message = no_longer_exists_message(&target.display_name);
                Self::show_delete_toast(&target.std_path, message, ctx);
            }
            DeleteOutcome::Failed(error) => {
                log::warn!("Failed to delete {}: {error}", target.local_path.display());
                let message = delete_failed_message(&target.display_name, &error);
                Self::show_delete_toast(&target.std_path, message, ctx);
            }
            DeleteOutcome::FolderPartlyDeleted { error, gone } => {
                log::warn!(
                    "Deleting {} stopped partway: {error}",
                    target.local_path.display()
                );
                // Stop showing what's gone, and close editor tabs open on it.
                self.forget_paths(&gone);
                for path in &gone {
                    ctx.emit(FileTreeEvent::FileDeleted {
                        path: path.to_local_path_lossy(),
                    });
                }
                let message = folder_partly_deleted_message(&target.display_name);
                Self::show_delete_toast(&target.std_path, message, ctx);
            }
        }
        ctx.notify();
    }

    /// The folder at `folder` and every item inside it that the tree has loaded, in any local
    /// root. It reads the in-memory tree only.
    fn folder_and_loaded_contents(&self, folder: &StandardizedPath) -> Vec<StandardizedPath> {
        let mut found = vec![folder.clone()];
        for root_dir in self
            .root_directories
            .values()
            .filter(|root_dir| !root_dir.is_remote())
        {
            let mut folders = vec![folder.clone()];
            while let Some(parent) = folders.pop() {
                for child in root_dir.entry.child_paths(&parent) {
                    folders.push((**child).clone());
                    found.push((**child).clone());
                }
            }
        }
        // Nested roots can share an item; list it once.
        found.into_iter().unique().collect()
    }

    /// Takes `paths`, which are gone from disk, out of the in-memory tree and rebuilds the list,
    /// rather than wait for the file watcher, which may not be watching them.
    fn forget_paths(&mut self, paths: &[StandardizedPath]) {
        if paths.is_empty() {
            return;
        }
        for root_dir in self
            .root_directories
            .values_mut()
            .filter(|root_dir| !root_dir.is_remote())
        {
            for path in paths {
                if root_dir.entry.contains(path) {
                    root_dir.entry.remove(path);
                }
            }
        }
        self.rebuild_flattened_items();
    }

    /// Shows an error toast about deleting `path`. A newer toast about the same path replaces an
    /// older one.
    fn show_delete_toast(path: &StandardizedPath, message: String, ctx: &mut ViewContext<Self>) {
        Self::show_error_toast(format!("file_tree_delete:{path}"), message, ctx);
    }

    /// Shows an error toast. A newer toast with the same `object_id` replaces an older one.
    fn show_error_toast(object_id: String, message: String, ctx: &mut ViewContext<Self>) {
        let window_id = ctx.window_id();
        ToastStack::handle(ctx).update(ctx, |toast_stack, ctx| {
            let toast = DismissibleToast::error(message).with_object_id(object_id);
            toast_stack.add_ephemeral_toast(toast, window_id, ctx);
        });
    }

    /// Returns an iterator over the displayed root directories and their associated data.
    fn displayed_root_directories(
        &self,
    ) -> impl Iterator<Item = (&StandardizedPath, &RootDirectory)> + '_ {
        self.displayed_directories.iter().filter_map(|path| {
            self.root_directories
                .get(path)
                .map(|root_dir| (path, root_dir))
        })
    }

    /// Get total count of items across all roots
    fn total_item_count(&self) -> usize {
        self.displayed_root_directories()
            .map(|(_, root_dir)| root_dir.items.len())
            .sum()
    }

    /// Creates a FileTreeIdentifier from a global index by finding which root it belongs to.
    fn identifier_from_global_index(&self, global_index: usize) -> Option<FileTreeIdentifier> {
        let mut current_index = 0;
        for (root, root_dir) in self.displayed_root_directories() {
            let end_index = current_index + root_dir.items.len();
            if global_index < end_index {
                return Some(FileTreeIdentifier {
                    root: root.clone(),
                    index: global_index - current_index,
                });
            }
            current_index = end_index;
        }
        None
    }

    /// Converts a FileTreeIdentifier to a global index within the flattened list.
    fn identifier_to_global_index(&self, identifier: &FileTreeIdentifier) -> Option<usize> {
        let mut current_index = 0;
        for (root, data) in self.displayed_root_directories() {
            if root == &identifier.root {
                return Some(current_index + identifier.index);
            }
            current_index += data.items.len();
        }
        None
    }

    fn render_file_tree(&self, app: &AppContext) -> Box<dyn Element> {
        let appearance = Appearance::as_ref(app);
        let theme = appearance.theme();
        let num_items = self.total_item_count();
        if num_items == 0 {
            return self.render_loading_state(app);
        }

        let view_handle = self.view_handle.clone();
        let uniform_list = UniformList::new(
            self.list_state.clone(),
            num_items,
            move |range: Range<usize>, app: &AppContext| {
                let appearance = Appearance::as_ref(app);
                let view_handle = view_handle
                    .upgrade(app)
                    .expect("view handle should be valid");
                let view = view_handle.as_ref(app);

                range
                    .filter_map(|global_index| {
                        let item_id = view.identifier_from_global_index(global_index)?;
                        Some(view.render_item(&item_id, appearance))
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
            },
        )
        .finish_scrollable();
        let content_column = Flex::column()
            .with_main_axis_size(MainAxisSize::Max)
            .with_child(
                Shrinkable::new(
                    1.,
                    Scrollable::vertical(
                        self.scroll_state.clone(),
                        uniform_list,
                        ScrollbarWidth::Auto,
                        theme.nonactive_ui_detail().into(),
                        theme.active_ui_detail().into(),
                        warpui::elements::Fill::None,
                    )
                    .with_overlayed_scrollbar()
                    .finish(),
                )
                .finish(),
            )
            .finish();

        let main_content = Align::new(content_column).top_center().finish();
        let container = Container::new(main_content)
            // we have padding from the left panel toolbelt so we don't need top padding here
            .with_padding_bottom(12.)
            .with_horizontal_padding(8.)
            .finish();

        let positioned_content = SavePosition::new(container, &self.position_id).finish();
        let mut stack = Stack::new();
        stack.add_child(positioned_content);

        if let Some(context_menu_state) = &self.context_menu_state {
            stack.add_positioned_overlay_child(
                ChildView::new(&self.context_menu).finish(),
                OffsetPositioning::offset_from_parent(
                    context_menu_state.position,
                    ParentOffsetBounds::WindowByPosition,
                    ParentAnchor::TopLeft,
                    ChildAnchor::TopLeft,
                ),
            );
        }

        stack.finish()
    }

    /// What the file tree shows apart from the delete dialog: its list, or a message when there
    /// is no list to show.
    #[cfg(feature = "local_fs")]
    fn render_content(&self, app: &AppContext) -> Box<dyn Element> {
        if self.shows_list() {
            return self.render_file_tree(app);
        }

        if matches!(self.enablement, CodingPanelEnablementState::Disabled) {
            return self.render_error_state(DISABLED_TEXT.to_string(), app);
        }

        if matches!(
            self.enablement,
            CodingPanelEnablementState::PendingRemoteSession
        ) {
            return self.render_loading_state(app);
        }

        if self.displayed_directories.is_empty() {
            if let CodingPanelEnablementState::RemoteSession { has_remote_server } = self.enablement
            {
                // When the session has a remote server connection (Auto SSH
                // Warpification / mode 1), show a loading state — the server
                // may push repo metadata momentarily. For other SSH modes
                // (tmux, subshell) no data will arrive, so show the disabled
                // error instead.
                return if has_remote_server {
                    self.render_loading_state(app)
                } else {
                    self.render_error_state(REMOTE_TEXT.to_string(), app)
                };
            }

            if matches!(
                self.enablement,
                CodingPanelEnablementState::UnsupportedSession
            ) {
                return self.render_error_state(WSL_TEXT.to_string(), app);
            }

            return self.render_loading_state(app);
        }

        // Roots are displayed, but none has an item to show yet.
        self.render_loading_state(app)
    }

    /// Whether the file tree is showing its list, rather than a loading or error message in its
    /// place. `render_content` draws the list exactly when this holds, and the delete dialog stays
    /// open only while it does.
    #[cfg(feature = "local_fs")]
    fn shows_list(&self) -> bool {
        !matches!(
            self.enablement,
            CodingPanelEnablementState::Disabled | CodingPanelEnablementState::PendingRemoteSession
        ) && self.total_item_count() > 0
    }

    #[cfg(not(feature = "local_fs"))]
    fn shows_list(&self) -> bool {
        false
    }

    /// Draws the delete dialog over `content` while one is open. It's added here, whatever the
    /// file tree is showing, rather than inside the list. The dialog closes whenever the list
    /// gives way to a message (see `close_delete_dialog_if_stale`), and should some change ever
    /// slip past that, the dialog stays in sight rather than pending unseen.
    fn with_delete_dialog(&self, content: Box<dyn Element>) -> Box<dyn Element> {
        if self.pending_delete.is_none() {
            return content;
        }
        let mut stack = Stack::new();
        stack.add_child(content);
        // The dialog's view takes all the space it's offered, which here is the whole window.
        // Keeping it inside the window then pins it to the window's top-left corner, wherever
        // the file tree sits, and the dialog centres its box within that.
        stack.add_positioned_overlay_child(
            ChildView::new(&self.delete_dialog).finish(),
            OffsetPositioning::offset_from_parent(
                Vector2F::zero(),
                ParentOffsetBounds::WindowByPosition,
                ParentAnchor::TopLeft,
                ChildAnchor::TopLeft,
            ),
        );
        stack.finish()
    }

    fn render_error_state(&self, text: String, app: &AppContext) -> Box<dyn Element> {
        let appearance = Appearance::as_ref(app);
        let theme = appearance.theme();

        let main_column = Flex::column()
            .with_main_axis_size(MainAxisSize::Max)
            .with_main_axis_alignment(MainAxisAlignment::Center)
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_child(
                Container::new(
                    ConstrainedBox::new(
                        Icon::AlertTriangle
                            .to_warpui_icon(Fill::Solid(internal_colors::neutral_6(theme)))
                            .finish(),
                    )
                    .with_width(24.)
                    .with_height(24.)
                    .finish(),
                )
                .with_margin_bottom(12.)
                .finish(),
            )
            .with_child(
                Text::new(
                    "Project explorer unavailable",
                    appearance.ui_font_family(),
                    appearance.ui_font_size() + 2.,
                )
                .with_style(Properties::default().weight(Weight::Semibold))
                .with_color(theme.sub_text_color(theme.background()).into())
                .finish(),
            )
            .with_child(
                ConstrainedBox::new(
                    Shrinkable::new(
                        1.,
                        Container::new(
                            Shrinkable::new(
                                1.,
                                FormattedTextElement::from_str(
                                    text,
                                    appearance.ui_font_family(),
                                    appearance.ui_font_size() + 2.,
                                )
                                .with_alignment(TextAlignment::Center)
                                .with_color(theme.disabled_text_color(theme.background()).into())
                                .finish(),
                            )
                            .finish(),
                        )
                        .finish(),
                    )
                    .finish(),
                )
                .with_max_width(425.)
                .finish(),
            )
            .finish();

        Container::new(
            Flex::row()
                .with_main_axis_size(MainAxisSize::Max)
                .with_main_axis_alignment(MainAxisAlignment::Center)
                .with_cross_axis_alignment(CrossAxisAlignment::Center)
                .with_child(Shrinkable::new(1., main_column).finish())
                .finish(),
        )
        .with_horizontal_margin(16.)
        .finish()
    }

    fn render_loading_state(&self, app: &AppContext) -> Box<dyn Element> {
        let appearance = Appearance::as_ref(app);
        let theme = appearance.theme();

        // Create loading icon
        let loading_icon = Icon::Loading
            .to_warpui_icon(warp_core::ui::theme::Fill::Solid(
                internal_colors::neutral_6(theme),
            ))
            .finish();
        let loading_icon = Container::new(
            ConstrainedBox::new(loading_icon)
                .with_height(FOLDER_INDENT)
                .with_width(FOLDER_INDENT)
                .finish(),
        )
        .with_margin_right(8.)
        .finish();

        // Reuse the RenderState function by creating a header row similar to render_item_with_hover
        let mut header_row = Flex::row()
            .with_main_axis_size(MainAxisSize::Max)
            .with_cross_axis_alignment(CrossAxisAlignment::Center);

        // Add loading icon first (where expand/collapse would be), then folder icon
        header_row.add_child(loading_icon);

        let folder_icon = Icon::Folder
            .to_warpui_icon(warp_core::ui::theme::Fill::Solid(
                internal_colors::neutral_6(theme),
            ))
            .finish();
        header_row.add_child(
            Container::new(
                ConstrainedBox::new(folder_icon)
                    .with_width(FOLDER_INDENT)
                    .with_height(FOLDER_INDENT)
                    .finish(),
            )
            .with_margin_right(8.)
            .finish(),
        );

        // Create placeholder rectangles similar to render_code_diff_placeholder
        let base_gradient_color_start = internal_colors::neutral_3(theme);

        // Make the end color neutral_3 at 10% opacity (approximately 26 alpha out of 255)
        let mut base_gradient_color_end = base_gradient_color_start;
        base_gradient_color_end.a = 26;

        // Percent widths for placeholder lines
        let percent_widths = vec![
            1., 1., 0.89, 0.77, 0.66, 0.89, 1., 1., 1., 0.89, 0.89, 0.77, 0.77, 0.89, 0.89, 0.89,
            1., 1., 1., 1., 0.89, 0.89, 0.89, 0.77, 0.77, 0.89, 0.77, 0.77, 0.89, 0.77, 0.66, 0.89,
            0.89, 1.,
        ];
        let placeholder_lines = percent_widths.into_iter().map(|percent_width| {
            let rect = ConstrainedBox::new(
                Rect::new()
                    .with_horizontal_background_gradient(
                        base_gradient_color_start,
                        base_gradient_color_end,
                    )
                    .finish(),
            )
            .with_height(16.)
            .finish();

            let rect = Align::new(Percentage::width(percent_width, rect).finish())
                .right()
                .finish();

            Shrinkable::new(
                1.,
                Container::new(rect)
                    .with_vertical_padding(1.)
                    .with_margin_bottom(4.)
                    .finish(),
            )
            .finish()
        });

        // Create placeholder lines with appropriate styling
        let placeholder_lines_with_styling =
            placeholder_lines.map(|line| Container::new(line).with_horizontal_padding(8.).finish());

        // Create the main flex column with the placeholder item and all placeholder lines
        let mut main_column = Flex::column()
            .with_main_axis_size(MainAxisSize::Max)
            .with_child(header_row.finish());

        // Add separator between header and placeholder lines
        main_column.add_child(
            Container::new(Empty::new().finish())
                .with_margin_bottom(8.)
                .finish(),
        );

        // Add all placeholder lines directly to the main column
        main_column = main_column.with_children(placeholder_lines_with_styling);

        Container::new(Clipped::new(Shrinkable::new(1., main_column.finish()).finish()).finish())
            .with_uniform_margin(12.)
            .finish()
    }
}

/// How many items the folder at `path` holds directly, according to the in-memory tree. It's
/// known only when the folder is loaded and not ignored; this never reads the disk.
fn loaded_child_count(entry: &FileTreeEntry, path: &StandardizedPath) -> Option<usize> {
    match entry.get(path)? {
        FileTreeEntryState::Directory(directory) if directory.loaded && !directory.ignored => {
            Some(entry.child_paths(path).count())
        }
        FileTreeEntryState::Directory(_) | FileTreeEntryState::File(_) => None,
    }
}

/// What happened when the file tree tried to delete an item.
enum DeleteOutcome {
    Deleted,
    /// The path now names a different item from the one the dialog showed.
    Changed,
    /// Nothing exists at the path any more.
    Missing,
    Failed(io::Error),
    /// A folder's delete stopped partway. `gone` lists what no longer exists of the folder and
    /// the items the tree had loaded inside it.
    FolderPartlyDeleted {
        error: io::Error,
        gone: Vec<StandardizedPath>,
    },
}

/// Deletes the item at `path` if it is still the item the dialog showed. It runs on a thread
/// from the blocking pool, off the main thread, and makes the check and the removal back to back
/// there, so the gap between them is a couple of system calls rather than a trip through the
/// executor. The gap can't be closed entirely, because no portable call removes a path only if
/// it still names a given item. The removal never follows a symbolic link, so a link swapped in
/// during the gap is removed as a link; a real folder swapped in would still be removed.
///
/// For a folder, `loaded_items` is the folder and what the tree has loaded inside it. If the
/// folder's delete stops partway, they're checked with `lstat` in this same job, to report which
/// are gone.
fn delete_if_unchanged(
    path: &Path,
    kind: ItemKind,
    identity: ItemIdentity,
    loaded_items: Vec<StandardizedPath>,
) -> DeleteOutcome {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return DeleteOutcome::Missing,
        Err(error) => return DeleteOutcome::Failed(error),
    };
    if ItemKind::from_file_type(metadata.file_type()) != kind
        || ItemIdentity::from_metadata(&metadata) != identity
    {
        return DeleteOutcome::Changed;
    }

    let result = match kind {
        // Only ever a real folder here: a link to a folder is a `Symlink`.
        //
        // Like `rm -rf`, this doesn't stop at mount points. Anything mounted somewhere inside
        // the folder (a disk image, an SSHFS or other FUSE mount, a network share) has its
        // contents deleted along with the rest, and the dialog gives no warning of it: its count
        // covers only the items directly inside the folder that the tree has loaded. Stopping at
        // mount points would take a recursion of our own in place of `remove_dir_all`, which is
        // left for a separate change.
        ItemKind::Directory => std::fs::remove_dir_all(path),
        // Windows stores a link to a folder as a directory entry, which `remove_file` refuses.
        #[cfg(windows)]
        ItemKind::Symlink
            if std::os::windows::fs::FileTypeExt::is_symlink_dir(&metadata.file_type()) =>
        {
            std::fs::remove_dir(path)
        }
        // For a link, this removes the link itself, never the item it points to.
        ItemKind::File | ItemKind::Symlink => std::fs::remove_file(path),
    };
    match (result, kind) {
        (Ok(()), _) => DeleteOutcome::Deleted,
        // Deleting a folder removes what's inside it first, so by the time an error stops it,
        // some of its contents may be gone.
        (Err(error), ItemKind::Directory) => {
            let gone = loaded_items
                .into_iter()
                .filter(|item| {
                    matches!(
                        std::fs::symlink_metadata(item.to_local_path_lossy()),
                        Err(lstat_error) if lstat_error.kind() == io::ErrorKind::NotFound
                    )
                })
                .collect();
            DeleteOutcome::FolderPartlyDeleted { error, gone }
        }
        (Err(error), ItemKind::File | ItemKind::Symlink) => DeleteOutcome::Failed(error),
    }
}

pub enum FileTreeEvent {
    #[cfg_attr(not(feature = "local_fs"), allow(dead_code))]
    AttachAsContext { path: PathBuf },
    #[cfg_attr(not(feature = "local_fs"), allow(dead_code))]
    OpenFile {
        path: LocalOrRemotePath,
        target: FileTarget,
        line_col: Option<LineAndColumnArg>,
    },
    #[cfg_attr(not(feature = "local_fs"), allow(dead_code))]
    FileRenamed {
        old_path: PathBuf,
        new_path: PathBuf,
    },
    #[cfg_attr(not(feature = "local_fs"), allow(dead_code))]
    FileDeleted { path: PathBuf },
    #[cfg_attr(not(feature = "local_fs"), allow(dead_code))]
    CDToDirectory { path: PathBuf },
    #[cfg_attr(not(feature = "local_fs"), allow(dead_code))]
    OpenDirectoryInNewTab { path: PathBuf },
}

impl Entity for FileTreeView {
    type Event = FileTreeEvent;
}

impl View for FileTreeView {
    fn ui_name() -> &'static str {
        "FilePicker"
    }

    #[cfg(not(feature = "local_fs"))]
    fn render(&self, app: &AppContext) -> Box<dyn Element> {
        self.render_error_state(REMOTE_TEXT.to_string(), app)
    }

    #[cfg(feature = "local_fs")]
    fn render(&self, app: &AppContext) -> Box<dyn Element> {
        self.with_delete_dialog(self.render_content(app))
    }

    fn on_blur(&mut self, _: &BlurContext, ctx: &mut ViewContext<Self>) {
        if !ctx.is_self_or_child_focused() {
            self.handle_pending_edit(ctx);
            // Focus has left the file tree and its delete dialog, for example because a shortcut
            // opened a new tab. The dialog would otherwise stay on screen, blocking the mouse,
            // while its keys went somewhere else. Close it, and leave focus where it went.
            if self.pending_delete.is_some() {
                self.close_delete_dialog(ctx);
            }
        }
    }
}

impl TypedActionView for FileTreeView {
    type Action = FileTreeAction;

    fn handle_action(&mut self, action: &Self::Action, ctx: &mut ViewContext<Self>) {
        // While the delete dialog is open, the tree behind it takes no action at all. The dialog
        // has focus, but keys it doesn't bind, such as the arrows, fall through to the tree, and
        // the tree mustn't move, open or change anything under the dialog's question. Every
        // action is held back, rather than a list of known ones, so a binding added later is
        // held back too. Nothing else sends the tree actions meanwhile: the menu that opened the
        // dialog has closed, and the dialog's backdrop takes the mouse.
        if self.pending_delete.is_some() {
            log::debug!("Ignoring {action:?}: the delete dialog is open");
            return;
        }
        match action {
            FileTreeAction::ItemClicked { id } => {
                ctx.focus_self();
                self.select_and_execute_item_at_id(id, ctx);
                ctx.notify();
            }
            FileTreeAction::SelectPreviousItem => {
                if let Some(selected_item) = &self.selected_item {
                    if selected_item.index > 0 {
                        let new_id = FileTreeIdentifier {
                            root: selected_item.root.clone(),
                            index: selected_item.index - 1,
                        };
                        self.select_id(&new_id, ctx);
                    }
                } else if let Some(first_dir) = self.displayed_directories.first() {
                    let id = FileTreeIdentifier {
                        root: first_dir.clone(),
                        index: 0,
                    };
                    self.select_id(&id, ctx);
                }

                ctx.notify();
            }
            FileTreeAction::SelectNextItem => {
                if let Some(selected_item) = &self.selected_item {
                    if let Some(root_dir) = self.root_directories.get(&selected_item.root) {
                        let new_index =
                            (selected_item.index + 1).min(root_dir.items.len().saturating_sub(1));
                        let new_id = FileTreeIdentifier {
                            root: selected_item.root.clone(),
                            index: new_index,
                        };
                        self.select_id(&new_id, ctx);
                    }
                } else if let Some(first_dir) = self.displayed_directories.first() {
                    let id = FileTreeIdentifier {
                        root: first_dir.clone(),
                        index: 0,
                    };
                    self.select_id(&id, ctx);
                }
                ctx.notify();
            }
            FileTreeAction::Expand => {
                if let Some(selected_item) = self.selected_item.clone() {
                    if let Some(sp) = self.selected_item_std_path() {
                        if !self.is_folder_expanded(&selected_item.root, &sp) {
                            self.toggle_folder_expansion(&selected_item.root, &sp, ctx);
                            ctx.notify();
                        }
                    }
                }
            }
            FileTreeAction::Collapse => {
                if let Some(selected_item) = self.selected_item.clone() {
                    if let Some(sp) = self.selected_item_std_path() {
                        if self.is_folder_expanded(&selected_item.root, &sp) {
                            self.toggle_folder_expansion(&selected_item.root, &sp, ctx);
                            ctx.notify();
                        }
                    }
                }
            }
            FileTreeAction::ExecuteSelectedItem => {
                if let Some(id) = self.selected_item.clone() {
                    self.select_and_execute_item_at_id(&id, ctx);
                }
            }
            FileTreeAction::OpenContextMenu { position, id } => {
                let Some(root_dir) = self.root_directories.get(&id.root) else {
                    return;
                };
                let Some(item) = root_dir.items.get(id.index) else {
                    return;
                };

                self.context_menu_state = Some(ContextMenuState {
                    position: *position,
                });
                let menu_items = self.context_menu_items(item, id);
                self.context_menu.update(ctx, move |menu, ctx| {
                    menu.set_items(menu_items, ctx);
                    ctx.notify();
                });
                self.select_id(id, ctx);
                ctx.notify();
            }
            FileTreeAction::CopyPath { id } => {
                self.copy_absolute_path_for_id(id, ctx);
                self.context_menu_state.take();
            }
            FileTreeAction::CopyRelativePath { id } => {
                self.copy_relative_path_for_id(id, ctx);
                self.context_menu_state.take();
            }
            FileTreeAction::AttachAsContext { id } => {
                self.attach_as_context(id, ctx);
            }
            FileTreeAction::NewFileBelowDirectory { id } => {
                if !self.is_remote_item(id) {
                    self.create_new_file(id, ctx);
                }
            }
            FileTreeAction::OpenInNewPane { id } => {
                if !self.is_remote_item(id) {
                    self.open_in_new_pane(id, ctx);
                }
            }
            FileTreeAction::OpenInNewTab { id } => {
                if !self.is_remote_item(id) {
                    self.open_in_new_tab(id, ctx);
                }
            }
            FileTreeAction::CDToDirectory { id } => {
                if !self.is_remote_item(id) {
                    self.cd_to_directory(id, ctx);
                }
                self.context_menu_state.take();
            }
            FileTreeAction::OpenInFinder { id } => {
                if !self.is_remote_item(id) {
                    if let Some(root_dir) = self.root_directories.get(&id.root) {
                        if let Some(item) = root_dir.items.get(id.index) {
                            let path = item.path().to_local_path_lossy();
                            ctx.open_file_path_in_explorer(&path);
                        }
                    }
                }
                self.context_menu_state.take();
            }
            FileTreeAction::Rename { id } => {
                if !self.is_remote_item(id) {
                    self.rename_item(id, ctx);
                }
                self.context_menu_state.take();
            }
            FileTreeAction::Delete { id, path } => {
                if !self.is_remote_item(id) {
                    self.request_delete(id, path, ctx);
                }
                self.context_menu_state.take();
            }
            FileTreeAction::DismissEditor => {
                self.handle_pending_edit(ctx);
            }
            FileTreeAction::ItemDroppedOnInput {
                id,
                terminal_input_data,
            } => {
                let Some(relative_path) = self.relative_path_for_item(id) else {
                    return;
                };

                let weak_view_handle = terminal_input_data.weak_view_handle();
                let Some(input_view) = weak_view_handle.upgrade(ctx) else {
                    return;
                };

                let file_path = relative_path.to_string_lossy();
                input_view.update(ctx, |input_view, ctx| {
                    input_view.append_to_buffer(&file_path, ctx);
                });
            }
            FileTreeAction::ItemDroppedOnTerminal { id, terminal_view } => {
                let Some(root_dir) = self.root_directories.get(&id.root) else {
                    return;
                };
                let Some(item) = root_dir.items.get(id.index) else {
                    return;
                };

                let path_str = item.path().as_str();

                let Some(terminal_view) = terminal_view.upgrade(ctx) else {
                    return;
                };

                let file_path = path_str.to_string();
                terminal_view.update(ctx, |view, ctx| {
                    view.handle_file_tree_drop_on_active_command(&file_path, ctx);
                });
            }
        }
    }
}
#[cfg(test)]
mod view_tests;
