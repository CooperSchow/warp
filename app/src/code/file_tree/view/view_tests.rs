use std::any::Any;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pathfinder_geometry::rect::RectF;
use pathfinder_geometry::vector::{vec2f, Vector2F};
use repo_metadata::entry::{DirectoryEntry, Entry, FileMetadata};
use repo_metadata::file_tree_store::{FileTreeEntryState, FileTreeState};
use repo_metadata::local_model::IndexedRepoState;
use repo_metadata::repositories::DetectedRepositories;
use repo_metadata::watcher::DirectoryWatcher;
use repo_metadata::RepoMetadataModel;
use settings::Setting;
use virtual_fs::{Stub, VirtualFS};
use warp_core::ui::appearance::Appearance;
use warpui::elements::{ChildView, ConstrainedBox, Container};
use warpui::event::ModifiersState;
use warpui::keymap::{Keystroke, Trigger};
use warpui::platform::WindowStyle;
use warpui::r#async::Timer;
use warpui::{
    App, AppContext, Element, Entity, EntityId, Event, ModelHandle, SingletonEntity,
    TypedActionView, View, ViewHandle, WindowId,
};

use super::{FileTreeAction, FileTreeEvent, FileTreeIdentifier, FileTreeView, RootDirectory};
use crate::auth::AuthStateProvider;
use crate::code::file_tree::delete_confirmation_dialog::{
    DeleteFileConfirmationAction, ItemIdentity, ItemKind, PendingDelete,
};
use crate::coding_panel_enablement_state::CodingPanelEnablementState;
use crate::menu::MenuAction;
use crate::server::server_api::team::MockTeamClient;
use crate::server::server_api::workspace::MockWorkspaceClient;
use crate::settings::CodeSettings;
use crate::settings_view::keybindings::KeybindingChangedNotifier;
use crate::test_util::settings::initialize_settings_for_tests;
use crate::view_components::action_button::{ActionButton, SecondaryTheme};
use crate::vim_registers::VimRegisters;
use crate::workspace::sync_inputs::SyncedInputState;
use crate::workspace::ToastStack;
use crate::workspaces::user_workspaces::UserWorkspaces;

fn std_path(path: &std::path::Path) -> warp_util::standardized_path::StandardizedPath {
    warp_util::standardized_path::StandardizedPath::try_from_local(path).unwrap()
}

fn initialize_app(
    app: &mut App,
) -> (
    ModelHandle<DetectedRepositories>,
    ModelHandle<RepoMetadataModel>,
) {
    initialize_settings_for_tests(app);

    app.add_singleton_model(|_| Appearance::mock());
    app.add_singleton_model(|_| ToastStack);
    app.add_singleton_model(|_| SyncedInputState::mock());
    app.add_singleton_model(|_| VimRegisters::new());
    app.add_singleton_model(|_| KeybindingChangedNotifier::mock());
    app.add_singleton_model(|_| AuthStateProvider::new_for_test());

    let team_client = Arc::new(MockTeamClient::new());
    let workspace_client = Arc::new(MockWorkspaceClient::new());
    app.add_singleton_model(|ctx| {
        UserWorkspaces::mock(team_client.clone(), workspace_client.clone(), vec![], ctx)
    });

    let detected_repositories = app.add_singleton_model(|_| DetectedRepositories::default());
    let repository_metadata_model = app.add_singleton_model(RepoMetadataModel::new);

    (detected_repositories, repository_metadata_model)
}

fn build_repo_state(repo_root: &std::path::Path) -> FileTreeState {
    let source_file = Entry::File(FileMetadata::new(
        repo_root.join("packages/app/src/main.rs"),
        false,
    ));
    let src_dir = Entry::Directory(DirectoryEntry {
        path: warp_util::standardized_path::StandardizedPath::try_from_local(
            &repo_root.join("packages/app/src"),
        )
        .unwrap(),
        children: vec![source_file],
        ignored: false,
        loaded: true,
    });
    let app_dir = Entry::Directory(DirectoryEntry {
        path: warp_util::standardized_path::StandardizedPath::try_from_local(
            &repo_root.join("packages/app"),
        )
        .unwrap(),
        children: vec![src_dir],
        ignored: false,
        loaded: true,
    });
    let packages_dir = Entry::Directory(DirectoryEntry {
        path: warp_util::standardized_path::StandardizedPath::try_from_local(
            &repo_root.join("packages"),
        )
        .unwrap(),
        children: vec![app_dir],
        ignored: false,
        loaded: true,
    });
    let root = Entry::Directory(DirectoryEntry {
        path: std_path(repo_root),
        children: vec![packages_dir],
        ignored: false,
        loaded: true,
    });
    FileTreeState::new(root, vec![], None)
}

fn build_repo_state_with_unloaded_directory(repo_root: &std::path::Path) -> FileTreeState {
    let unloaded_src_dir = Entry::Directory(DirectoryEntry {
        path: warp_util::standardized_path::StandardizedPath::try_from_local(
            &repo_root.join("src"),
        )
        .unwrap(),
        children: vec![],
        ignored: false,
        loaded: false,
    });
    let root = Entry::Directory(DirectoryEntry {
        path: std_path(repo_root),
        children: vec![unloaded_src_dir],
        ignored: false,
        loaded: true,
    });
    FileTreeState::new(root, vec![], None)
}

fn flattened_paths(
    view: &FileTreeView,
    root: &std::path::Path,
) -> Vec<warp_util::standardized_path::StandardizedPath> {
    view.root_directories
        .get(&std_path(root))
        .expect("root directory is tracked")
        .items
        .iter()
        .map(|item| item.path().clone())
        .collect()
}

fn set_show_hidden_files(app: &mut App, show_hidden_files: bool) {
    CodeSettings::handle(app).update(app, |settings, ctx| {
        Setting::set_value(&mut settings.show_hidden_files, show_hidden_files, ctx)
            .expect("show hidden files setting updates");
    });
}

#[test]
fn hidden_files_are_filtered_until_setting_is_enabled() {
    VirtualFS::test("file_tree_hidden_files_setting", |dirs, mut vfs| {
        vfs.mkdir("tree/.config").with_files(vec![
            Stub::FileWithContent("tree/.env", "SECRET=value\n"),
            Stub::FileWithContent("tree/.config/settings.toml", ""),
            Stub::FileWithContent("tree/visible.txt", "content\n"),
        ]);
        let tree = dirs.tests().join("tree");
        let hidden_file = tree.join(".env");
        let hidden_dir = tree.join(".config");
        let visible_file = tree.join("visible.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            set_show_hidden_files(&mut app, false);
            let (_, file_tree_view) = app.add_window(WindowStyle::NotStealFocus, FileTreeView::new);

            file_tree_view.update(&mut app, |view, ctx| {
                view.set_is_active(true, ctx);
                view.set_root_directories(vec![tree.clone()], ctx);
            });

            file_tree_view.read(&app, |view, _ctx| {
                let paths = flattened_paths(view, &tree);
                assert!(paths.contains(&std_path(&tree)));
                assert!(paths.contains(&std_path(&visible_file)));
                assert!(!paths.contains(&std_path(&hidden_file)));
                assert!(!paths.contains(&std_path(&hidden_dir)));
            });

            set_show_hidden_files(&mut app, true);

            file_tree_view.read(&app, |view, _ctx| {
                let paths = flattened_paths(view, &tree);
                assert!(paths.contains(&std_path(&hidden_file)));
                assert!(paths.contains(&std_path(&hidden_dir)));
            });
        });
    });
}

#[test]
fn hidden_root_directory_is_not_filtered() {
    VirtualFS::test("file_tree_hidden_root_directory", |dirs, mut vfs| {
        vfs.mkdir(".config").with_files(vec![
            Stub::FileWithContent(".config/settings.toml", ""),
            Stub::FileWithContent(".config/.secret", ""),
        ]);
        let hidden_root = dirs.tests().join(".config");
        let visible_file = hidden_root.join("settings.toml");
        let hidden_file = hidden_root.join(".secret");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            set_show_hidden_files(&mut app, false);
            let (_, file_tree_view) = app.add_window(WindowStyle::NotStealFocus, FileTreeView::new);

            file_tree_view.update(&mut app, |view, ctx| {
                view.set_is_active(true, ctx);
                view.set_root_directories(vec![hidden_root.clone()], ctx);
            });

            file_tree_view.read(&app, |view, _ctx| {
                let paths = flattened_paths(view, &hidden_root);
                assert!(paths.contains(&std_path(&hidden_root)));
                assert!(paths.contains(&std_path(&visible_file)));
                assert!(!paths.contains(&std_path(&hidden_file)));
            });
        });
    });
}

#[test]
fn selected_hidden_file_is_cleared_when_filtered() {
    VirtualFS::test(
        "file_tree_selected_hidden_file_filtered",
        |dirs, mut vfs| {
            vfs.mkdir("tree").with_files(vec![
                Stub::FileWithContent("tree/.env", "SECRET=value\n"),
                Stub::FileWithContent("tree/visible.txt", "content\n"),
            ]);
            let tree = dirs.tests().join("tree");
            let hidden_file = tree.join(".env");

            App::test((), |mut app| async move {
                let _ = initialize_app(&mut app);
                set_show_hidden_files(&mut app, true);

                let (_, file_tree_view) =
                    app.add_window(WindowStyle::NotStealFocus, FileTreeView::new);
                file_tree_view.update(&mut app, |view, ctx| {
                    view.set_is_active(true, ctx);
                    view.set_root_directories(vec![tree.clone()], ctx);

                    let root_dir = view.root_directories.get(&std_path(&tree)).unwrap();
                    let (index, _) = root_dir
                        .items
                        .iter()
                        .enumerate()
                        .find(|(_, item)| item.path() == &std_path(&hidden_file))
                        .expect("hidden file is visible");
                    let id = super::FileTreeIdentifier {
                        root: std_path(&tree),
                        index,
                    };
                    view.select_id(&id, ctx);
                });

                set_show_hidden_files(&mut app, false);

                file_tree_view.read(&app, |view, _ctx| {
                    let paths = flattened_paths(view, &tree);
                    assert!(!paths.contains(&std_path(&hidden_file)));
                    assert!(view.selected_item.is_none());
                });
            });
        },
    );
}

#[test]
fn repo_transition_unregisters_lazy_loaded_path() {
    VirtualFS::test("file_tree_repo_transition", |dirs, mut vfs| {
        vfs.mkdir("repo/.git/objects")
            .mkdir("repo/packages/app/src")
            .with_files(vec![
                Stub::FileWithContent("repo/.git/HEAD", "ref: refs/heads/main"),
                Stub::FileWithContent("repo/.git/config", "[core]\n\trepositoryformatversion = 0"),
                Stub::FileWithContent("repo/packages/app/src/main.rs", "fn main() {}\n"),
            ]);

        let repo_root = dirs.tests().join("repo");
        let displayed_root = repo_root.join("packages/app");
        let canonical_repo_root =
            warp_util::standardized_path::StandardizedPath::from_local_canonicalized(&repo_root)
                .unwrap();

        App::test((), |mut app| async move {
            let (detected_repositories, repository_metadata_model) = initialize_app(&mut app);

            let (_, file_tree_view) = app.add_window(WindowStyle::NotStealFocus, FileTreeView::new);

            detected_repositories.update(&mut app, |repositories, _ctx| {
                repositories.insert_test_repo_root(canonical_repo_root.clone());
            });

            file_tree_view.update(&mut app, |view, ctx| {
                view.set_is_active(true, ctx);
                view.set_root_directories(vec![displayed_root.clone()], ctx);
            });

            file_tree_view.read(&app, |view, _ctx| {
                assert!(view.registered_lazy_loaded_paths.contains(
                    &warp_util::standardized_path::StandardizedPath::try_from_local(
                        &displayed_root
                    )
                    .unwrap()
                ));
                let displayed_std =
                    warp_util::standardized_path::StandardizedPath::try_from_local(&displayed_root)
                        .unwrap();
                assert_eq!(
                    view.root_directories
                        .get(&displayed_std)
                        .map(|root_dir| root_dir.entry.root_directory().to_local_path_lossy()),
                    Some(displayed_root.clone())
                );
            });
            repository_metadata_model.read(&app, |model, ctx| {
                assert!(model.is_lazy_loaded_path(
                    &warp_util::standardized_path::StandardizedPath::try_from_local(
                        &displayed_root
                    )
                    .unwrap(),
                    ctx
                ));
            });

            repository_metadata_model.update(&mut app, |model, ctx| {
                model.insert_test_state(canonical_repo_root, build_repo_state(&repo_root), ctx);
            });

            file_tree_view.update(&mut app, |view, ctx| {
                view.set_root_directories(vec![displayed_root.clone()], ctx);
            });

            file_tree_view.read(&app, |view, _ctx| {
                let displayed_std =
                    warp_util::standardized_path::StandardizedPath::try_from_local(&displayed_root)
                        .unwrap();
                let repo_std =
                    warp_util::standardized_path::StandardizedPath::try_from_local(&repo_root)
                        .unwrap();
                assert!(!view.registered_lazy_loaded_paths.contains(&displayed_std));
                assert_eq!(view.root_for_path(&displayed_std), Some(repo_std.clone()));
                assert_eq!(
                    view.root_directories
                        .get(&displayed_std)
                        .map(|root_dir| (**root_dir.entry.root_directory()).clone()),
                    Some(repo_std)
                );
            });
            repository_metadata_model.read(&app, |model, ctx| {
                assert!(!model.is_lazy_loaded_path(
                    &warp_util::standardized_path::StandardizedPath::try_from_local(
                        &displayed_root
                    )
                    .unwrap(),
                    ctx
                ));
            });
        });
    });
}

#[test]
fn repo_backed_unloaded_directory_loads_through_model() {
    VirtualFS::test("file_tree_repo_backed_load", |dirs, mut vfs| {
        vfs.mkdir("repo/.git/objects")
            .mkdir("repo/src/nested")
            .with_files(vec![
                Stub::FileWithContent("repo/.git/HEAD", "ref: refs/heads/main"),
                Stub::FileWithContent(
                    "repo/.git/config",
                    "[core]
\trepositoryformatversion = 0",
                ),
                Stub::FileWithContent(
                    "repo/src/nested/main.rs",
                    "fn main() {}
",
                ),
            ]);

        let repo_root = dirs.tests().join("repo");
        let src_dir = repo_root.join("src");
        let nested_dir = repo_root.join("src/nested");
        let source_file = repo_root.join("src/nested/main.rs");
        let canonical_repo_root =
            warp_util::standardized_path::StandardizedPath::from_local_canonicalized(&repo_root)
                .unwrap();

        App::test((), |mut app| async move {
            let (detected_repositories, repository_metadata_model) = initialize_app(&mut app);

            let (_, file_tree_view) = app.add_window(WindowStyle::NotStealFocus, FileTreeView::new);

            detected_repositories.update(&mut app, |repositories, _ctx| {
                repositories.insert_test_repo_root(canonical_repo_root.clone());
            });
            repository_metadata_model.update(&mut app, |model, ctx| {
                model.insert_test_state(
                    canonical_repo_root,
                    build_repo_state_with_unloaded_directory(&repo_root),
                    ctx,
                );
            });

            file_tree_view.update(&mut app, |view, ctx| {
                view.set_is_active(true, ctx);
                view.set_root_directories(vec![repo_root.clone()], ctx);
            });

            file_tree_view.read(&app, |view, _ctx| {
                assert!(!view
                    .root_directories
                    .get(
                        &warp_util::standardized_path::StandardizedPath::try_from_local(&repo_root)
                            .unwrap()
                    )
                    .is_some_and(|root_dir| root_dir.entry.contains(
                        &warp_util::standardized_path::StandardizedPath::try_from_local(
                            &source_file
                        )
                        .unwrap()
                    )));
            });

            file_tree_view.update(&mut app, |view, ctx| {
                view.ensure_loaded_path(
                    &warp_util::standardized_path::StandardizedPath::try_from_local(&repo_root)
                        .unwrap(),
                    &warp_util::standardized_path::StandardizedPath::try_from_local(&src_dir)
                        .unwrap(),
                    ctx,
                );
            });

            file_tree_view.read(&app, |view, _ctx| {
                assert!(view
                    .root_directories
                    .get(
                        &warp_util::standardized_path::StandardizedPath::try_from_local(&repo_root)
                            .unwrap()
                    )
                    .is_some_and(|root_dir| root_dir.entry.contains(
                        &warp_util::standardized_path::StandardizedPath::try_from_local(
                            &nested_dir
                        )
                        .unwrap()
                    )));
            });

            file_tree_view.update(&mut app, |view, ctx| {
                view.ensure_loaded_path(
                    &warp_util::standardized_path::StandardizedPath::try_from_local(&repo_root)
                        .unwrap(),
                    &warp_util::standardized_path::StandardizedPath::try_from_local(&nested_dir)
                        .unwrap(),
                    ctx,
                );
            });

            file_tree_view.read(&app, |view, _ctx| {
                assert!(view
                    .root_directories
                    .get(
                        &warp_util::standardized_path::StandardizedPath::try_from_local(&repo_root)
                            .unwrap()
                    )
                    .is_some_and(|root_dir| root_dir.entry.contains(
                        &warp_util::standardized_path::StandardizedPath::try_from_local(
                            &source_file
                        )
                        .unwrap()
                    )));
            });
            repository_metadata_model.read(&app, |model, ctx| {
                assert!(!model.is_lazy_loaded_path(
                    &warp_util::standardized_path::StandardizedPath::try_from_local(&repo_root)
                        .unwrap(),
                    ctx
                ));
                let id = repo_metadata::RepositoryIdentifier::local(
                    warp_util::standardized_path::StandardizedPath::try_from_local(&repo_root)
                        .unwrap(),
                );
                assert!(model.get_repository(&id, ctx).is_some_and(|state| {
                    state.entry.contains(
                        &warp_util::standardized_path::StandardizedPath::try_from_local(
                            &source_file,
                        )
                        .unwrap(),
                    )
                }));
            });
        });
    });
}

#[test]
fn pending_repository_root_does_not_register_lazy_loaded_path() {
    VirtualFS::test("file_tree_pending_repo_root", |dirs, mut vfs| {
        vfs.mkdir("repo/.git/objects").with_files(vec![
            Stub::FileWithContent("repo/.git/HEAD", "ref: refs/heads/main"),
            Stub::FileWithContent("repo/.git/config", "[core]\n\trepositoryformatversion = 0"),
        ]);

        let repo_root = dirs.tests().join("repo");
        let canonical_repo_root =
            warp_util::standardized_path::StandardizedPath::from_local_canonicalized(&repo_root)
                .unwrap();

        App::test((), |mut app| async move {
            let (detected_repositories, repository_metadata_model) = initialize_app(&mut app);
            let directory_watcher = app.add_singleton_model(DirectoryWatcher::new);

            let (_, file_tree_view) = app.add_window(WindowStyle::NotStealFocus, FileTreeView::new);
            let repository_handle = directory_watcher.update(&mut app, |watcher, ctx| {
                watcher
                    .add_directory(canonical_repo_root.clone(), ctx)
                    .unwrap()
            });

            detected_repositories.update(&mut app, |repositories, _ctx| {
                repositories.insert_test_repo_root(canonical_repo_root.clone());
            });
            repository_metadata_model.update(&mut app, |model, ctx| {
                model.index_directory(repository_handle, ctx).unwrap();
            });
            repository_metadata_model.read(&app, |model, ctx| {
                let id = repo_metadata::RepositoryIdentifier::local(
                    warp_util::standardized_path::StandardizedPath::try_from_local(&repo_root)
                        .unwrap(),
                );
                assert!(matches!(
                    model.repository_state(&id, ctx),
                    Some(IndexedRepoState::Pending(_))
                ));
            });

            file_tree_view.update(&mut app, |view, ctx| {
                view.set_is_active(true, ctx);
                view.set_root_directories(vec![repo_root.clone()], ctx);
            });

            file_tree_view.read(&app, |view, _ctx| {
                assert!(!view.registered_lazy_loaded_paths.contains(
                    &warp_util::standardized_path::StandardizedPath::try_from_local(&repo_root)
                        .unwrap()
                ));
            });
            repository_metadata_model.read(&app, |model, ctx| {
                assert!(!model.is_lazy_loaded_path(
                    &warp_util::standardized_path::StandardizedPath::try_from_local(&repo_root)
                        .unwrap(),
                    ctx
                ));
                let id = repo_metadata::RepositoryIdentifier::local(
                    warp_util::standardized_path::StandardizedPath::try_from_local(&repo_root)
                        .unwrap(),
                );
                assert!(matches!(
                    model.repository_state(&id, ctx),
                    Some(IndexedRepoState::Pending(_))
                ));
            });
        });
    });
}

#[test]
fn failed_lazy_loaded_path_registration_is_retried() {
    VirtualFS::test("file_tree_lazy_loaded_path_retry", |dirs, mut vfs| {
        let displayed_root = dirs.tests().join("late_dir");

        App::test((), |mut app| async move {
            let (_detected_repositories, repository_metadata_model) = initialize_app(&mut app);

            let (_, file_tree_view) = app.add_window(WindowStyle::NotStealFocus, FileTreeView::new);

            file_tree_view.update(&mut app, |view, ctx| {
                view.set_is_active(true, ctx);
                view.set_root_directories(vec![displayed_root.clone()], ctx);
            });

            file_tree_view.read(&app, |view, _ctx| {
                assert!(!view.registered_lazy_loaded_paths.contains(
                    &warp_util::standardized_path::StandardizedPath::try_from_local(
                        &displayed_root
                    )
                    .unwrap()
                ));
            });
            repository_metadata_model.read(&app, |model, ctx| {
                assert!(!model.is_lazy_loaded_path(
                    &warp_util::standardized_path::StandardizedPath::try_from_local(
                        &displayed_root
                    )
                    .unwrap(),
                    ctx
                ));
            });

            vfs.mkdir("late_dir")
                .with_files(vec![Stub::FileWithContent("late_dir/file.txt", "content")]);

            file_tree_view.update(&mut app, |view, ctx| {
                view.set_root_directories(vec![displayed_root.clone()], ctx);
            });

            file_tree_view.read(&app, |view, _ctx| {
                assert!(view.registered_lazy_loaded_paths.contains(&std_path(&displayed_root)));
                assert!(matches!(
                    view.root_directories.get(&std_path(&displayed_root)).map(|root_dir| &root_dir.entry),
                    Some(entry)
                        if entry.contains(&std_path(&displayed_root.join("file.txt")))
                ));
            });
            repository_metadata_model.read(&app, |model, ctx| {
                assert!(model.is_lazy_loaded_path(
                    &warp_util::standardized_path::StandardizedPath::try_from_local(
                        &displayed_root
                    )
                    .unwrap(),
                    ctx
                ));
            });
        });
    });
}

// ── Ancestor grouping (APP-4106) ────────────────────────────────────

#[test]
fn sibling_roots_are_preserved() {
    VirtualFS::test("file_tree_sibling_roots", |dirs, mut vfs| {
        vfs.mkdir("tree/a").mkdir("tree/b").with_files(vec![
            Stub::FileWithContent("tree/a/x.txt", "x"),
            Stub::FileWithContent("tree/b/y.txt", "y"),
        ]);
        let a = dirs.tests().join("tree/a");
        let b = dirs.tests().join("tree/b");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (_, file_tree_view) = app.add_window(WindowStyle::NotStealFocus, FileTreeView::new);

            file_tree_view.update(&mut app, |view, ctx| {
                view.set_is_active(true, ctx);
                view.set_root_directories(vec![a.clone(), b.clone()], ctx);
            });

            file_tree_view.read(&app, |view, _ctx| {
                assert_eq!(view.displayed_directories, vec![std_path(&a), std_path(&b)]);
            });
        });
    });
}

#[test]
fn auto_expand_overrides_selection_when_most_recent_root_changes() {
    VirtualFS::test(
        "file_tree_auto_expand_overrides_on_new_root",
        |dirs, mut vfs| {
            vfs.mkdir("code/foo").mkdir("other").with_files(vec![
                Stub::FileWithContent("code/foo/file.txt", "x"),
                Stub::FileWithContent("other/file.txt", "y"),
            ]);
            let code = dirs.tests().join("code");
            let other = dirs.tests().join("other");

            App::test((), |mut app| async move {
                let _ = initialize_app(&mut app);
                let (_, file_tree_view) =
                    app.add_window(WindowStyle::NotStealFocus, FileTreeView::new);

                // Start with `code` as the only root and select its header.
                file_tree_view.update(&mut app, |view, ctx| {
                    view.set_is_active(true, ctx);
                    view.set_root_directories(vec![code.clone()], ctx);
                    view.auto_expand_to_most_recent_directory(ctx);
                });
                file_tree_view.read(&app, |view, _ctx| {
                    let selected = view.selected_item.as_ref().unwrap();
                    assert_eq!(selected.root, std_path(&code));
                });

                // Now cd to a brand-new root. `other` becomes most-recent.
                // Selection must move to `other`, not stay on `code`.
                file_tree_view.update(&mut app, |view, ctx| {
                    view.set_root_directories(vec![other.clone(), code.clone()], ctx);
                    view.auto_expand_to_most_recent_directory(ctx);
                });

                file_tree_view.read(&app, |view, _ctx| {
                    let selected = view.selected_item.as_ref().expect("selection set");
                    assert_eq!(selected.root, std_path(&other));
                });
            });
        },
    );
}

#[test]
fn auto_expand_preserves_existing_selection() {
    VirtualFS::test(
        "file_tree_auto_expand_preserves_selection",
        |dirs, mut vfs| {
            vfs.mkdir("tree/sub")
                .with_files(vec![Stub::FileWithContent("tree/sub/file.txt", "content")]);
            let tree = dirs.tests().join("tree");
            let sub = tree.join("sub");

            App::test((), |mut app| async move {
                let _ = initialize_app(&mut app);
                let (_, file_tree_view) =
                    app.add_window(WindowStyle::NotStealFocus, FileTreeView::new);

                file_tree_view.update(&mut app, |view, ctx| {
                    view.set_is_active(true, ctx);
                    view.set_root_directories(vec![tree.clone()], ctx);
                });

                // Simulate a prior explicit selection (e.g. user focused a
                // file in the code editor and `scroll_to_file` selected it).
                file_tree_view.update(&mut app, |view, ctx| {
                    view.toggle_folder_expansion(&std_path(&tree), &std_path(&sub), ctx);
                    let root_dir = view.root_directories.get(&std_path(&tree)).unwrap();
                    let (index, _) = root_dir
                        .items
                        .iter()
                        .enumerate()
                        .find(|(_, item)| item.path() == &std_path(&sub))
                        .expect("sub directory is flattened");
                    let id = super::FileTreeIdentifier {
                        root: std_path(&tree),
                        index,
                    };
                    view.select_id(&id, ctx);
                });

                // Auto-expand must not override that selection with the root header.
                file_tree_view.update(&mut app, |view, ctx| {
                    view.auto_expand_to_most_recent_directory(ctx);
                });

                file_tree_view.read(&app, |view, _ctx| {
                    let selected = view.selected_item.clone().expect("selection set");
                    let root_dir = view.root_directories.get(&std_path(&tree)).unwrap();
                    let selected_path = root_dir.items.get(selected.index).unwrap().path();
                    assert_eq!(selected_path, &std_path(&sub));
                });
            });
        },
    );
}

#[test]
fn click_on_file_under_absorbed_descendant_keeps_file_selected() {
    // Simulates: user clicks a file in the tree. The code view opens it,
    // which causes `DirectoriesChanged` to fire with the file's
    // parent/repo added. The resulting `set_root_directories` must NOT
    // override the user's file selection with the cwd-follow parent.
    VirtualFS::test(
        "file_tree_click_file_preserves_selection",
        |dirs, mut vfs| {
            vfs.mkdir("code/warp-server")
                .with_files(vec![Stub::FileWithContent(
                    "code/warp-server/main.rs",
                    "fn main() {}\n",
                )]);
            let code = dirs.tests().join("code");
            let warp_server = code.join("warp-server");
            let main_rs = warp_server.join("main.rs");

            App::test((), |mut app| async move {
                let _ = initialize_app(&mut app);
                let (_, file_tree_view) =
                    app.add_window(WindowStyle::NotStealFocus, FileTreeView::new);

                // Seed with `code` as the only root and expand warp-server so
                // main.rs is materialized in the flattened items.
                file_tree_view.update(&mut app, |view, ctx| {
                    view.set_is_active(true, ctx);
                    view.set_root_directories(vec![code.clone()], ctx);
                    view.toggle_folder_expansion(&std_path(&code), &std_path(&warp_server), ctx);
                });

                // Simulate a click on main.rs (select_id is what the click
                // action and the active-file scroll both go through).
                file_tree_view.update(&mut app, |view, ctx| {
                    let root_dir = view.root_directories.get(&std_path(&code)).unwrap();
                    let (index, _) = root_dir
                        .items
                        .iter()
                        .enumerate()
                        .find(|(_, item)| item.path() == &std_path(&main_rs))
                        .expect("main.rs materialized");
                    let id = super::FileTreeIdentifier {
                        root: std_path(&code),
                        index,
                    };
                    view.select_id(&id, ctx);
                });

                // Now `DirectoriesChanged` fires as a side effect of the file
                // opening in a code view — the working-directories-model adds
                // the file's repo/parent (warp-server) to the active set.
                file_tree_view.update(&mut app, |view, ctx| {
                    view.set_root_directories(vec![warp_server.clone(), code.clone()], ctx);
                });

                file_tree_view.read(&app, |view, _ctx| {
                    // Selection is still on main.rs, not on warp-server.
                    let selected = view.selected_item.clone().expect("selection");
                    let root_dir = view.root_directories.get(&std_path(&code)).unwrap();
                    let path = root_dir.items.get(selected.index).unwrap().path();
                    assert_eq!(path, &std_path(&main_rs));
                    // And we didn't set a pending focus target that could
                    // later steal focus back to the parent directory.
                    assert!(view.pending_focus_target.is_none());
                });
            });
        },
    );
}

#[test]
fn pending_focus_target_does_not_re_scroll_after_first_apply() {
    // After the initial focus-follow scrolls to the cwd, subsequent
    // rebuilds (e.g. from repo-metadata updates) must keep the
    // selection but NOT re-scroll, so user scrolling is respected.
    VirtualFS::test("file_tree_pending_respects_user_scroll", |dirs, mut vfs| {
        vfs.mkdir("tree/warp-server")
            .with_files(vec![Stub::FileWithContent(
                "tree/warp-server/main.rs",
                "fn main() {}\n",
            )]);
        let tree = dirs.tests().join("tree");
        let warp_server = tree.join("warp-server");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (_, file_tree_view) = app.add_window(WindowStyle::NotStealFocus, FileTreeView::new);

            file_tree_view.update(&mut app, |view, ctx| {
                view.set_is_active(true, ctx);
                view.set_root_directories(vec![warp_server.clone(), tree.clone()], ctx);
            });

            // Initial apply should have scrolled once.
            file_tree_view.read(&app, |view, _ctx| {
                let pending = view.pending_focus_target.as_ref().expect("pending");
                assert!(pending.scrolled);
            });

            // Simulate a later rebuild (e.g. metadata update). Selection
            // should still land on warp-server, but `scrolled` must stay
            // true (no re-scroll).
            file_tree_view.update(&mut app, |view, _ctx| {
                view.rebuild_flattened_items();
                view.apply_pending_focus_target();
            });

            file_tree_view.read(&app, |view, _ctx| {
                let selected = view.selected_item.clone().expect("selection");
                let root_dir = view.root_directories.get(&std_path(&tree)).unwrap();
                let path = root_dir.items.get(selected.index).unwrap().path();
                assert_eq!(path, &std_path(&warp_server));
                let pending = view.pending_focus_target.as_ref().expect("pending");
                assert!(pending.scrolled, "scrolled flag stays set after re-apply");
            });
        });
    });
}

#[test]
fn focus_follows_absorbed_descendant_once_its_item_is_materialized() {
    VirtualFS::test("file_tree_focus_follow_deferred", |dirs, mut vfs| {
        vfs.mkdir("tree/warp-server")
            .with_files(vec![Stub::FileWithContent(
                "tree/warp-server/main.rs",
                "fn main() {}\n",
            )]);
        let tree = dirs.tests().join("tree");
        let warp_server = tree.join("warp-server");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (_, file_tree_view) = app.add_window(WindowStyle::NotStealFocus, FileTreeView::new);

            // User cd's into warp-server with ~/tree as the ancestor root.
            // The warp-server entry should be materialized by indexing and
            // selected as the focus-follow target.
            file_tree_view.update(&mut app, |view, ctx| {
                view.set_is_active(true, ctx);
                view.set_root_directories(vec![warp_server.clone(), tree.clone()], ctx);
            });

            file_tree_view.read(&app, |view, _ctx| {
                // Single displayed root, descendant absorbed.
                assert_eq!(view.displayed_directories, vec![std_path(&tree)]);
                // Selection landed on warp-server's directory header.
                let selected = view.selected_item.clone().expect("selection set");
                assert_eq!(selected.root, std_path(&tree));
                let root_dir = view.root_directories.get(&std_path(&tree)).unwrap();
                let selected_item = root_dir
                    .items
                    .get(selected.index)
                    .expect("selected index in range");
                assert_eq!(selected_item.path(), &std_path(&warp_server));
                // Pending target is preserved across rebuilds so later
                // repo-metadata updates don't override the cwd-follow
                // selection. It clears when the user interacts explicitly
                // (see pending_focus_target_cleared_on_user_select).
                let pending = view
                    .pending_focus_target
                    .as_ref()
                    .expect("pending target preserved");
                assert_eq!(pending.root, std_path(&tree));
                assert_eq!(pending.path, std_path(&warp_server));
                // The initial apply scrolled; later applies must not
                // re-scroll so user scrolling is respected.
                assert!(pending.scrolled, "initial apply scrolls the tree");
            });

            // User clicks somewhere else (simulated via select_id). Pending
            // target must clear so future rebuilds don't re-steal focus.
            file_tree_view.update(&mut app, |view, ctx| {
                let root_dir = view.root_directories.get(&std_path(&tree)).unwrap();
                let id = super::FileTreeIdentifier {
                    root: std_path(&tree),
                    index: 0,
                };
                // Sanity: the first item is the root header, not warp-server.
                assert_ne!(
                    root_dir.items.first().unwrap().path(),
                    &std_path(&warp_server)
                );
                view.select_id(&id, ctx);
            });

            file_tree_view.read(&app, |view, _ctx| {
                assert!(view.pending_focus_target.is_none());
            });
        });
    });
}

#[test]
fn descendant_is_absorbed_into_ancestor() {
    VirtualFS::test("file_tree_absorb_descendant", |dirs, mut vfs| {
        vfs.mkdir("tree/a")
            .with_files(vec![Stub::FileWithContent("tree/a/x.txt", "x")]);
        let tree = dirs.tests().join("tree");
        let a = tree.join("a");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (_, file_tree_view) = app.add_window(WindowStyle::NotStealFocus, FileTreeView::new);

            file_tree_view.update(&mut app, |view, ctx| {
                view.set_is_active(true, ctx);
                // Input in most-recent-first order: descendant first.
                view.set_root_directories(vec![a.clone(), tree.clone()], ctx);
            });

            file_tree_view.read(&app, |view, _ctx| {
                // Only the ancestor survives as a displayed root.
                assert_eq!(view.displayed_directories, vec![std_path(&tree)]);
                assert!(view.root_directories.contains_key(&std_path(&tree)));
                assert!(!view.root_directories.contains_key(&std_path(&a)));
                // The absorbed descendant is expanded inside the surviving root.
                let root_dir = view.root_directories.get(&std_path(&tree)).unwrap();
                assert!(root_dir.expanded_folders.contains(&std_path(&a)));
            });
        });
    });
}

#[test]
fn cd_into_descendant_absorbs_into_existing_ancestor_root() {
    VirtualFS::test("file_tree_cd_into_descendant", |dirs, mut vfs| {
        vfs.mkdir("tree/a/z")
            .with_files(vec![Stub::FileWithContent("tree/a/z/file.txt", "f")]);
        let tree = dirs.tests().join("tree");
        let z = tree.join("a/z");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (_, file_tree_view) = app.add_window(WindowStyle::NotStealFocus, FileTreeView::new);

            // Start with only the ancestor displayed.
            file_tree_view.update(&mut app, |view, ctx| {
                view.set_is_active(true, ctx);
                view.set_root_directories(vec![tree.clone()], ctx);
            });

            // Simulate cd-ing into ~/tree/a/z by emitting the descendant as the
            // most-recent path.
            file_tree_view.update(&mut app, |view, ctx| {
                view.set_root_directories(vec![z.clone(), tree.clone()], ctx);
            });

            file_tree_view.read(&app, |view, _ctx| {
                // Still a single root, no new top-level entry.
                assert_eq!(view.displayed_directories, vec![std_path(&tree)]);
                // Ancestor chain is auto-expanded down to the cwd.
                let root_dir = view.root_directories.get(&std_path(&tree)).unwrap();
                assert!(root_dir
                    .expanded_folders
                    .contains(&std_path(&tree.join("a"))));
                assert!(root_dir.expanded_folders.contains(&std_path(&z)));
            });
        });
    });
}

#[test]
fn explicit_collapse_blocks_auto_expand_on_absorption() {
    VirtualFS::test("file_tree_collapse_blocks_expand", |dirs, mut vfs| {
        vfs.mkdir("tree/a/z")
            .with_files(vec![Stub::FileWithContent("tree/a/z/file.txt", "f")]);
        let tree = dirs.tests().join("tree");
        let a = tree.join("a");
        let z = a.join("z");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (_, file_tree_view) = app.add_window(WindowStyle::NotStealFocus, FileTreeView::new);

            // Start with the ancestor displayed and explicitly collapse `a`.
            file_tree_view.update(&mut app, |view, ctx| {
                view.set_is_active(true, ctx);
                view.set_root_directories(vec![tree.clone()], ctx);
                // First expand so the toggle records a collapse.
                view.toggle_folder_expansion(&std_path(&tree), &std_path(&a), ctx);
                view.toggle_folder_expansion(&std_path(&tree), &std_path(&a), ctx);
            });

            file_tree_view.read(&app, |view, _ctx| {
                assert!(view.is_explicitly_collapsed(&std_path(&tree), &std_path(&a)));
            });

            // Now cd into ~/tree/a/z. Auto-expansion must not re-open `a`.
            file_tree_view.update(&mut app, |view, ctx| {
                view.set_root_directories(vec![z.clone(), tree.clone()], ctx);
            });

            file_tree_view.read(&app, |view, _ctx| {
                let root_dir = view.root_directories.get(&std_path(&tree)).unwrap();
                assert!(!root_dir.expanded_folders.contains(&std_path(&a)));
                assert!(!root_dir.expanded_folders.contains(&std_path(&z)));
                assert!(view.is_explicitly_collapsed(&std_path(&tree), &std_path(&a)));
            });
        });
    });
}

#[test]
fn absorption_migrates_expanded_and_explicitly_collapsed_state() {
    VirtualFS::test("file_tree_absorb_migrates_state", |dirs, mut vfs| {
        vfs.mkdir("tree/a/z")
            .with_files(vec![Stub::FileWithContent("tree/a/z/file.txt", "f")]);
        let tree = dirs.tests().join("tree");
        let a = tree.join("a");
        let z = a.join("z");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (_, file_tree_view) = app.add_window(WindowStyle::NotStealFocus, FileTreeView::new);

            // Start with `a` as a standalone top-level root and record
            // an explicit collapse on `a/z` under that standalone root.
            file_tree_view.update(&mut app, |view, ctx| {
                view.set_is_active(true, ctx);
                view.set_root_directories(vec![a.clone()], ctx);
                // Expand then collapse z so the toggle records a collapse on it.
                view.toggle_folder_expansion(&std_path(&a), &std_path(&z), ctx);
                view.toggle_folder_expansion(&std_path(&a), &std_path(&z), ctx);
            });

            file_tree_view.read(&app, |view, _ctx| {
                assert!(view.is_explicitly_collapsed(&std_path(&a), &std_path(&z)));
            });

            // Now absorb `a` into `tree` by adding the ancestor.
            file_tree_view.update(&mut app, |view, ctx| {
                view.set_root_directories(vec![a.clone(), tree.clone()], ctx);
            });

            file_tree_view.read(&app, |view, _ctx| {
                // Standalone absorbed-root entry is gone.
                assert!(!view.root_directories.contains_key(&std_path(&a)));
                // Its explicit-collapse state moved over to the ancestor.
                assert!(view.is_explicitly_collapsed(&std_path(&tree), &std_path(&z)));
            });
        });
    });
}

#[test]
fn absorbed_descendant_is_unregistered_from_lazy_loaded_paths() {
    VirtualFS::test("file_tree_absorb_unregisters_lazy", |dirs, mut vfs| {
        vfs.mkdir("tree/a")
            .with_files(vec![Stub::FileWithContent("tree/a/x.txt", "x")]);
        let tree = dirs.tests().join("tree");
        let a = tree.join("a");

        App::test((), |mut app| async move {
            let (_, repository_metadata_model) = initialize_app(&mut app);
            let (_, file_tree_view) = app.add_window(WindowStyle::NotStealFocus, FileTreeView::new);

            // Initial state: `a` alone is a standalone lazy-loaded root.
            file_tree_view.update(&mut app, |view, ctx| {
                view.set_is_active(true, ctx);
                view.set_root_directories(vec![a.clone()], ctx);
            });

            file_tree_view.read(&app, |view, _ctx| {
                assert!(view.registered_lazy_loaded_paths.contains(&std_path(&a)));
            });
            repository_metadata_model.read(&app, |model, ctx| {
                assert!(model.is_lazy_loaded_path(&std_path(&a), ctx));
            });

            // Add the ancestor. `a` should be absorbed and its lazy-loaded
            // registration should be cleaned up.
            file_tree_view.update(&mut app, |view, ctx| {
                view.set_root_directories(vec![a.clone(), tree.clone()], ctx);
            });

            file_tree_view.read(&app, |view, _ctx| {
                assert!(!view.registered_lazy_loaded_paths.contains(&std_path(&a)));
                assert!(view.registered_lazy_loaded_paths.contains(&std_path(&tree)));
            });
            repository_metadata_model.read(&app, |model, ctx| {
                assert!(!model.is_lazy_loaded_path(&std_path(&a), ctx));
            });
        });
    });
}

// ── Deleting from the context menu ──────────────────────────────────

/// Every path under `dir`, found without following symbolic links.
fn paths_under(dir: &Path) -> BTreeSet<PathBuf> {
    let mut paths = BTreeSet::new();
    let mut folders = vec![dir.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder).expect("the test folder is readable") {
            let path = entry.expect("the test folder entry is readable").path();
            if std::fs::symlink_metadata(&path).expect("lstat").is_dir() {
                folders.push(path.clone());
            }
            paths.insert(path);
        }
    }
    paths
}

/// Panics unless `path` lies strictly inside `test_dir`, and `test_dir` inside the system temp
/// folder. Every delete these tests cause, directly or through the file tree, is checked here
/// first.
fn assert_inside_test_dir(path: &Path, test_dir: &Path) {
    let temp_root = dunce::canonicalize(std::env::temp_dir()).expect("the temp folder resolves");
    assert!(
        test_dir.starts_with(&temp_root),
        "{} is not inside the system temp folder {}",
        test_dir.display(),
        temp_root.display()
    );
    assert!(
        path.starts_with(test_dir)
            && path != test_dir
            && !path
                .components()
                .any(|component| component == Component::ParentDir),
        "{} is not inside the test folder {}",
        path.display(),
        test_dir.display()
    );
}

/// Records what the file tree reports while deleting: the toasts it raises and the paths it
/// says it deleted.
#[derive(Default)]
struct DeleteObserver {
    toasts: usize,
    deleted: Vec<PathBuf>,
}

impl Entity for DeleteObserver {
    type Event = ();
}

fn observe_deletes(app: &mut App, view: &ViewHandle<FileTreeView>) -> ModelHandle<DeleteObserver> {
    app.add_model(|ctx| {
        ctx.subscribe_to_model(
            &ToastStack::handle(ctx),
            |observer: &mut DeleteObserver, _, _, _| {
                observer.toasts += 1;
            },
        );
        ctx.subscribe_to_view(view, |observer: &mut DeleteObserver, _, event, _| {
            if let FileTreeEvent::FileDeleted { path } = event {
                observer.deleted.push(path.clone());
            }
        });
        DeleteObserver::default()
    })
}

/// Opens a file tree on `tree` in a new window, with the file tree's key bindings registered.
/// The dialog's Delete button is armed the moment the dialog opens, so tests can click it
/// straight away; the tests of the arming delay turn the delay back on.
fn open_file_tree(app: &mut App, tree: &Path) -> (WindowId, ViewHandle<FileTreeView>) {
    app.update(super::init);
    let (window_id, view) = app.add_window(WindowStyle::NotStealFocus, FileTreeView::new);
    view.update(app, |view, ctx| {
        view.set_is_active(true, ctx);
        view.set_root_directories(vec![tree.to_path_buf()], ctx);
    });
    set_arm_delay(app, &view, Duration::ZERO);
    (window_id, view)
}

/// Sets how long the dialog's Delete button stays disabled after the dialog opens.
fn set_arm_delay(app: &mut App, view: &ViewHandle<FileTreeView>, arm_delay: Duration) {
    view.update(app, |view, ctx| {
        view.delete_dialog
            .update(ctx, |dialog, ctx| dialog.set_arm_delay(arm_delay, ctx));
    });
}

/// The row in `root`'s list that shows `path`.
fn row_of(view: &FileTreeView, root: &Path, path: &Path) -> FileTreeIdentifier {
    let index = view
        .root_directories
        .get(&std_path(root))
        .expect("root directory is tracked")
        .items
        .iter()
        .position(|item| item.path() == &std_path(path))
        .expect("the item is in the flattened list");
    FileTreeIdentifier {
        root: std_path(root),
        index,
    }
}

/// The action the context menu's "Delete…" item carries for `path`, as the list is now.
fn delete_action(view: &FileTreeView, root: &Path, path: &Path) -> FileTreeAction {
    FileTreeAction::Delete {
        id: row_of(view, root, path),
        path: std_path(path),
    }
}

/// Chooses "Delete…" for `path`, the way the context menu does.
fn choose_delete(
    app: &mut App,
    window_id: WindowId,
    view: &ViewHandle<FileTreeView>,
    root: &Path,
    path: &Path,
) {
    let action = view.read(app, |view, _| delete_action(view, root, path));
    app.dispatch_typed_action(window_id, &[view.id()], &action);
}

/// The focus path to the delete dialog: the file tree, then the dialog inside it.
fn dialog_chain(app: &App, view: &ViewHandle<FileTreeView>) -> [EntityId; 2] {
    [view.id(), view.read(app, |view, _| view.delete_dialog.id())]
}

/// Clicks the dialog's Cancel button, which dispatches `Cancel` to the dialog.
fn click_cancel(app: &mut App, window_id: WindowId, view: &ViewHandle<FileTreeView>) {
    let chain = dialog_chain(app, view);
    app.dispatch_typed_action(window_id, &chain, &DeleteFileConfirmationAction::Cancel);
}

/// Clicks the dialog's Delete button, after checking that the dialog targets a path inside the
/// test folder.
fn click_delete(
    app: &mut App,
    window_id: WindowId,
    view: &ViewHandle<FileTreeView>,
    test_dir: &Path,
) {
    let target = view
        .read(app, |view, _| view.pending_delete.clone())
        .expect("the delete dialog is open");
    assert_inside_test_dir(&target.local_path, test_dir);
    // The press, then the release, as the Delete button reports them.
    let chain = dialog_chain(app, view);
    app.dispatch_typed_action(
        window_id,
        &chain,
        &DeleteFileConfirmationAction::DeletePressed,
    );
    app.dispatch_typed_action(window_id, &chain, &DeleteFileConfirmationAction::Confirm);
}

/// Presses `key` while the delete dialog has focus. Returns whether anything handled it.
fn press(app: &mut App, window_id: WindowId, view: &ViewHandle<FileTreeView>, key: &str) -> bool {
    let chain = dialog_chain(app, view);
    app.dispatch_keystroke(
        window_id,
        &chain,
        &Keystroke::parse(key).expect("valid keystroke"),
        false,
    )
    .expect("the keystroke dispatches")
}

/// Waits until every background delete has called back.
async fn wait_for_deletes(app: &mut App, view: &ViewHandle<FileTreeView>) {
    for _ in 0..500 {
        if view.read(app, |view, _| view.deletes_in_flight.is_empty()) {
            return;
        }
        Timer::after(Duration::from_millis(10)).await;
    }
    panic!("a background delete never called back");
}

/// Checks that the dialog is closed with nothing pending or running, and that the file tree has
/// focus back.
fn assert_dialog_closed(app: &App, window_id: WindowId, view: &ViewHandle<FileTreeView>) {
    view.read(app, |view, _| {
        assert!(view.pending_delete.is_none(), "the dialog is closed");
        assert!(view.deletes_in_flight.is_empty(), "no delete was started");
    });
    assert_eq!(
        app.focused_view_id(window_id),
        Some(view.id()),
        "the file tree has focus back"
    );
}

/// Adds `file` to the in-memory tree and rebuilds the list, the way a file-watcher update does.
/// These tests run without a watcher, so they stand in for it.
fn add_file_to_tree(app: &mut App, view: &ViewHandle<FileTreeView>, root: &Path, file: &Path) {
    view.update(app, |view, _| {
        let root_dir = view
            .root_directories
            .get_mut(&std_path(root))
            .expect("root directory is tracked");
        root_dir.entry.insert_child_state(
            &std_path(file.parent().expect("the file has a parent")),
            FileTreeEntryState::File(FileMetadata::from_standardized(std_path(file), false).into()),
        );
        view.rebuild_flattened_items();
    });
}

#[test]
fn delete_menu_item_carries_the_items_path() {
    VirtualFS::test("file_tree_delete_menu_item", |dirs, mut vfs| {
        vfs.mkdir("tree")
            .with_files(vec![Stub::FileWithContent("tree/victim.txt", "victim\n")]);
        let tree = dirs.tests().join("tree");
        let victim = tree.join("victim.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (_, view) = open_file_tree(&mut app, &tree);

            view.read(&app, |view, _| {
                let delete_actions = |path: &Path| -> Vec<FileTreeAction> {
                    let row = row_of(view, &tree, path);
                    let item = &view.root_directories[&row.root].items[row.index];
                    view.context_menu_items(item, &row)
                        .iter()
                        .filter_map(|menu_item| menu_item.fields())
                        .filter(|fields| fields.label().starts_with("Delete"))
                        .map(|fields| {
                            assert_eq!(fields.label(), "Delete…");
                            fields
                                .on_select_action()
                                .cloned()
                                .expect("Delete… has an action")
                        })
                        .collect()
                };

                let actions = delete_actions(&victim);
                assert!(
                    matches!(
                        actions.as_slice(),
                        [FileTreeAction::Delete { path, .. }] if *path == std_path(&victim)
                    ),
                    "Delete… carries the item's path"
                );
                assert!(
                    delete_actions(&tree).is_empty(),
                    "the root item has no Delete"
                );
            });
        });
    });
}

#[test]
fn delete_opens_the_dialog_and_touches_nothing_on_disk() {
    VirtualFS::test("file_tree_delete_opens_dialog", |dirs, mut vfs| {
        vfs.mkdir("tree").with_files(vec![
            Stub::FileWithContent("tree/keep.txt", "keep\n"),
            Stub::FileWithContent("tree/victim.txt", "victim\n"),
        ]);
        let tree = dirs.tests().join("tree");
        let victim = tree.join("victim.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            let before = paths_under(&tree);

            choose_delete(&mut app, window_id, &view, &tree, &victim);

            let dialog_id = view.read(&app, |view, _| {
                let target = view
                    .pending_delete
                    .as_ref()
                    .expect("the delete dialog is open");
                assert_eq!(target.std_path, std_path(&victim));
                assert_eq!(target.local_path, victim);
                assert_eq!(target.kind, ItemKind::File);
                assert_eq!(target.display_name, "victim.txt");
                assert_eq!(target.child_count, None);
                assert!(
                    view.deletes_in_flight.is_empty(),
                    "nothing runs before a confirm"
                );
                view.delete_dialog.id()
            });
            assert_eq!(
                app.focused_view_id(window_id),
                Some(dialog_id),
                "the dialog has focus"
            );

            // Leave time for anything that might have been started in the background.
            Timer::after(Duration::from_millis(50)).await;
            assert_eq!(paths_under(&tree), before);
        });
    });
}

#[test]
fn cancel_and_escape_close_the_dialog_and_keep_the_item() {
    VirtualFS::test("file_tree_delete_cancel", |dirs, mut vfs| {
        vfs.mkdir("tree")
            .with_files(vec![Stub::FileWithContent("tree/victim.txt", "victim\n")]);
        let tree = dirs.tests().join("tree");
        let victim = tree.join("victim.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            let before = paths_under(&tree);

            choose_delete(&mut app, window_id, &view, &tree, &victim);
            click_cancel(&mut app, window_id, &view);
            assert_dialog_closed(&app, window_id, &view);

            choose_delete(&mut app, window_id, &view, &tree, &victim);
            assert!(
                press(&mut app, window_id, &view, "escape"),
                "the dialog handles Escape"
            );
            assert_dialog_closed(&app, window_id, &view);

            view.read(&app, |view, _| {
                assert!(flattened_paths(view, &tree).contains(&std_path(&victim)));
            });
            Timer::after(Duration::from_millis(50)).await;
            assert_eq!(paths_under(&tree), before);
        });
    });
}

/// Whether `action` is one of the delete dialog's actions other than Cancel, the only one a key
/// may trigger.
fn is_dialog_action_other_than_cancel(action: &dyn Any) -> bool {
    action
        .downcast_ref::<DeleteFileConfirmationAction>()
        .is_some_and(|action| !matches!(action, DeleteFileConfirmationAction::Cancel))
}

#[test]
fn return_cancels_and_no_key_binding_can_delete() {
    VirtualFS::test("file_tree_delete_keys", |dirs, mut vfs| {
        vfs.mkdir("tree/victim-folder")
            .with_files(vec![Stub::FileWithContent(
                "tree/victim-folder/leaf.txt",
                "leaf\n",
            )]);
        let tree = dirs.tests().join("tree");
        let folder = tree.join("victim-folder");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            let before = paths_under(&tree);
            // Select the folder, as right-clicking it does, so a Return that leaked through to
            // the file tree would visibly expand it.
            view.update(&mut app, |view, ctx| {
                let row = row_of(view, &tree, &folder);
                view.select_id(&row, ctx);
            });
            let is_expanded = |app: &App| {
                view.read(app, |view, _| {
                    view.is_folder_expanded(&std_path(&tree), &std_path(&folder))
                })
            };
            let expanded_before = is_expanded(&app);

            choose_delete(&mut app, window_id, &view, &tree, &folder);
            assert!(
                press(&mut app, window_id, &view, "enter"),
                "the dialog handles Return"
            );
            assert_dialog_closed(&app, window_id, &view);
            assert_eq!(
                is_expanded(&app),
                expanded_before,
                "Return never reached the file tree"
            );

            // Deleting is click-only. That's a rule about every key, so rather than try a sample of
            // keys, check every binding registered, fixed or editable, whatever its trigger: none
            // of them may confirm the dialog.
            let confirming_bindings: Vec<String> = app.read(|ctx| {
                let fixed = ctx
                    .get_key_bindings()
                    .filter(|binding| {
                        is_dialog_action_other_than_cancel(binding.action.as_ref().as_any())
                    })
                    .map(|binding| binding.name.to_owned());
                let editable = ctx
                    .editable_bindings()
                    .filter(|binding| {
                        is_dialog_action_other_than_cancel(binding.action.as_ref().as_any())
                    })
                    .map(|binding| binding.name.to_owned());
                fixed.chain(editable).collect()
            });
            assert!(
                confirming_bindings.is_empty(),
                "these bindings do more than cancel the delete dialog: {confirming_bindings:?}"
            );
            // The sweep only covers what's registered, so check the dialog's own bindings are.
            let dialog_bindings = app.read(|ctx| {
                ctx.get_key_bindings()
                    .filter(|binding| {
                        binding
                            .action
                            .as_ref()
                            .as_any()
                            .is::<DeleteFileConfirmationAction>()
                    })
                    .count()
            });
            assert_eq!(dialog_bindings, 3, "Escape, Return and the keypad's Enter");

            Timer::after(Duration::from_millis(50)).await;
            assert_eq!(paths_under(&tree), before);
        });
    });
}

#[test]
fn no_key_reaches_the_tree_behind_the_dialog() {
    VirtualFS::test("file_tree_delete_unbound_keys", |dirs, mut vfs| {
        vfs.mkdir("tree/folder").with_files(vec![
            Stub::FileWithContent("tree/folder/leaf.txt", "leaf\n"),
            Stub::FileWithContent("tree/victim.txt", "victim\n"),
        ]);
        let tree = dirs.tests().join("tree");
        let folder = tree.join("folder");
        let victim = tree.join("victim.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            // Select the folder, so a key that leaked through to the file tree would visibly
            // move the selection, expand the folder or open it.
            view.update(&mut app, |view, ctx| {
                let row = row_of(view, &tree, &folder);
                view.select_id(&row, ctx);
            });
            let is_expanded = |app: &App| {
                view.read(app, |view, _| {
                    view.is_folder_expanded(&std_path(&tree), &std_path(&folder))
                })
            };
            let tree_state = |app: &App| {
                (
                    view.read(app, |view, _| view.selected_item.clone()),
                    is_expanded(app),
                )
            };
            let before = tree_state(&app);
            let before_on_disk = paths_under(&tree);

            // Every key bound to one of the tree's own actions, rather than a sample of them.
            let tree_keys: Vec<Keystroke> = app.read(|ctx| {
                ctx.get_key_bindings()
                    .filter(|binding| binding.action.as_ref().as_any().is::<FileTreeAction>())
                    .filter_map(|binding| match binding.trigger {
                        Trigger::Keystrokes(keystrokes) => keystrokes.first().cloned(),
                        _ => None,
                    })
                    .collect()
            });
            assert!(
                tree_keys.len() >= 5,
                "the tree's keys are all registered: {tree_keys:?}"
            );

            for keystroke in &tree_keys {
                // Return cancels the dialog, so it may need opening again.
                if view.read(&app, |view, _| view.pending_delete.is_none()) {
                    choose_delete(&mut app, window_id, &view, &tree, &victim);
                }
                let chain = dialog_chain(&app, &view);
                app.dispatch_keystroke(window_id, &chain, keystroke, false)
                    .expect("the keystroke dispatches");
                assert_eq!(
                    tree_state(&app),
                    before,
                    "{keystroke:?} reached the file tree behind the dialog"
                );
                view.read(&app, |view, _| {
                    assert!(
                        view.deletes_in_flight.is_empty(),
                        "{keystroke:?} started a delete"
                    );
                });
            }

            // Once the dialog has closed, the same keys work the file tree again.
            if view.read(&app, |view, _| view.pending_delete.is_some()) {
                click_cancel(&mut app, window_id, &view);
            }
            app.dispatch_keystroke(
                window_id,
                &[view.id()],
                &Keystroke::parse("right").expect("valid keystroke"),
                false,
            )
            .expect("the keystroke dispatches");
            assert!(is_expanded(&app), "right expands the folder again");
            assert_eq!(paths_under(&tree), before_on_disk);
        });
    });
}

/// Whether the dialog's Delete button is enabled, which it is once the dialog is armed.
fn is_delete_armed(app: &App, view: &ViewHandle<FileTreeView>) -> bool {
    view.read(app, |view, ctx| {
        view.delete_dialog.as_ref(ctx).is_delete_button_enabled(ctx)
    })
}

/// Waits until the dialog's Delete button is armed.
async fn wait_until_armed(app: &mut App, view: &ViewHandle<FileTreeView>) {
    for _ in 0..500 {
        if is_delete_armed(app, view) {
            return;
        }
        Timer::after(Duration::from_millis(10)).await;
    }
    panic!("the Delete button was never armed");
}

/// How long Delete stays disabled in the tests of the arming delay. It is long enough that no
/// pause on a busy machine between opening the dialog and clicking lets an early click through.
const TEST_ARM_DELAY: Duration = Duration::from_secs(1);

#[test]
fn delete_is_disabled_for_a_moment_after_the_dialog_opens() {
    VirtualFS::test("file_tree_delete_arming", |dirs, mut vfs| {
        vfs.mkdir("tree")
            .with_files(vec![Stub::FileWithContent("tree/victim.txt", "victim\n")]);
        let test_dir = dirs.tests().clone();
        let tree = test_dir.join("tree");
        let victim = tree.join("victim.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            set_arm_delay(&mut app, &view, TEST_ARM_DELAY);
            let before = paths_under(&tree);

            choose_delete(&mut app, window_id, &view, &tree, &victim);
            assert!(!is_delete_armed(&app, &view), "Delete starts disabled");

            // The second click of a double-click on "Delete…" arrives straight away. It's
            // dropped, and the dialog stays open.
            click_delete(&mut app, window_id, &view, &test_dir);
            view.read(&app, |view, _| {
                assert!(view.pending_delete.is_some(), "the dialog is still open");
                assert!(view.deletes_in_flight.is_empty(), "no delete was started");
            });

            // Once the delay is up, Delete is armed and a click deletes.
            wait_until_armed(&mut app, &view).await;
            click_delete(&mut app, window_id, &view, &test_dir);
            wait_for_deletes(&mut app, &view).await;

            let mut expected = before.clone();
            expected.remove(&victim);
            assert_eq!(
                paths_under(&tree),
                expected,
                "a click after the delay deletes"
            );
        });
    });
}

/// Where the element saved as `position_id` was drawn in the window's last frame.
fn drawn_at(app: &App, window_id: WindowId, position_id: &str) -> Option<RectF> {
    app.read(|ctx| ctx.element_position_by_id_at_last_frame(window_id, position_id))
}

/// Presses the left mouse button at `position`. A `click_count` of 2 makes it the second press
/// of a double-click.
fn mouse_down_at(app: &mut App, window_id: WindowId, position: Vector2F, click_count: u32) {
    let presenter = app
        .presenter(window_id)
        .expect("the window has a presenter");
    app.update(|ctx| {
        ctx.simulate_window_event(
            Event::LeftMouseDown {
                position,
                modifiers: ModifiersState::default(),
                click_count,
                is_first_mouse: false,
            },
            window_id,
            presenter,
        );
    });
}

/// Releases the left mouse button at `position`.
fn mouse_up_at(app: &mut App, window_id: WindowId, position: Vector2F) {
    let presenter = app
        .presenter(window_id)
        .expect("the window has a presenter");
    app.update(|ctx| {
        ctx.simulate_window_event(
            Event::LeftMouseUp {
                position,
                modifiers: ModifiersState::default(),
            },
            window_id,
            presenter,
        );
    });
}

/// Presses and releases the left mouse button at `position`. A `click_count` of 2 makes it the
/// second click of a double-click.
fn click_at(app: &mut App, window_id: WindowId, position: Vector2F, click_count: u32) {
    mouse_down_at(app, window_id, position, click_count);
    mouse_up_at(app, window_id, position);
}

/// The ids the dialog saves its box's and its Delete button's positions under.
fn dialog_position_ids(app: &App, view: &ViewHandle<FileTreeView>) -> (String, String) {
    view.read(app, |view, ctx| {
        let dialog = view.delete_dialog.as_ref(ctx);
        (
            dialog.box_position_id().to_owned(),
            dialog.delete_button_position_id().to_owned(),
        )
    })
}

/// The window's size, as the platform reports it.
fn window_size(app: &App, window_id: WindowId) -> Vector2F {
    app.read(|ctx| {
        ctx.windows()
            .platform_window(window_id)
            .map(|window| window.as_ctx().size())
    })
    .expect("the window is open")
}

#[test]
fn only_the_dialogs_buttons_answer_the_pointer() {
    VirtualFS::test("file_tree_delete_pointer", |dirs, mut vfs| {
        vfs.mkdir("tree").with_files(vec![
            Stub::FileWithContent("tree/keep.txt", "keep\n"),
            Stub::FileWithContent("tree/victim.txt", "victim\n"),
        ]);
        let test_dir = dirs.tests().clone();
        let tree = test_dir.join("tree");
        let victim = tree.join("victim.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            let observer = observe_deletes(&mut app, &view);
            set_arm_delay(&mut app, &view, TEST_ARM_DELAY);
            let (box_id, delete_id) = dialog_position_ids(&app, &view);
            let keep_row = drawn_at(&app, window_id, "file_tree_item:keep.txt")
                .expect("keep.txt's row is drawn");
            let selection = view.read(&app, |view, _| view.selected_item.clone());
            let before = paths_under(&tree);

            choose_delete(&mut app, window_id, &view, &tree, &victim);
            let dialog_box = drawn_at(&app, window_id, &box_id).expect("the dialog is drawn");
            assert!(
                !dialog_box.contains_point(keep_row.center()),
                "keep.txt's row is outside the dialog box"
            );

            // The second click of a double-click on "Delete…" lands on Delete before it's armed,
            // and does nothing. Neither does a click on the dialog's text.
            let delete_button = drawn_at(&app, window_id, &delete_id).expect("Delete is drawn");
            click_delete_at(
                &mut app,
                window_id,
                &view,
                &test_dir,
                delete_button.center(),
                2,
            );
            click_at(
                &mut app,
                window_id,
                dialog_box.origin() + vec2f(24., 24.),
                1,
            );
            view.read(&app, |view, _| {
                assert!(view.pending_delete.is_some(), "the dialog is still open");
                assert!(view.deletes_in_flight.is_empty(), "no delete was started");
            });

            // A click on the tree behind the dialog doesn't reach the tree; it only cancels.
            click_at(&mut app, window_id, keep_row.center(), 1);
            assert_dialog_closed(&app, window_id, &view);
            assert_eq!(
                view.read(&app, |view, _| view.selected_item.clone()),
                selection,
                "the click didn't select keep.txt"
            );
            assert!(
                drawn_at(&app, window_id, &box_id).is_none(),
                "the dialog is gone"
            );

            // Once Delete is armed, clicking it deletes.
            choose_delete(&mut app, window_id, &view, &tree, &victim);
            wait_until_armed(&mut app, &view).await;
            let delete_button = drawn_at(&app, window_id, &delete_id).expect("Delete is drawn");
            click_delete_at(
                &mut app,
                window_id,
                &view,
                &test_dir,
                delete_button.center(),
                1,
            );
            wait_for_deletes(&mut app, &view).await;

            let mut expected = before.clone();
            expected.remove(&victim);
            assert_eq!(paths_under(&tree), expected, "only victim.txt is gone");
            observer.read(&app, |observer, _| {
                assert_eq!(observer.deleted, vec![victim.clone()]);
            });
        });
    });
}

/// Clicks the dialog's Delete button with the pointer at `position`, after checking that the
/// dialog targets a path inside the test folder. A `click_count` of 2 makes it the second click
/// of a double-click.
fn click_delete_at(
    app: &mut App,
    window_id: WindowId,
    view: &ViewHandle<FileTreeView>,
    test_dir: &Path,
    position: Vector2F,
    click_count: u32,
) {
    let target = view
        .read(app, |view, _| view.pending_delete.clone())
        .expect("the delete dialog is open");
    assert_inside_test_dir(&target.local_path, test_dir);
    click_at(app, window_id, position, click_count);
}

/// A window's root view that puts the file tree in a panel of `size` at `origin`, away from the
/// window's top-left corner, the way the workspace puts its left panel below the tab bar.
struct OffsetPanel {
    file_tree: ViewHandle<FileTreeView>,
    origin: Vector2F,
    size: Vector2F,
}

impl Entity for OffsetPanel {
    type Event = ();
}

impl View for OffsetPanel {
    fn ui_name() -> &'static str {
        "OffsetPanel"
    }

    fn render(&self, _: &AppContext) -> Box<dyn Element> {
        Container::new(
            ConstrainedBox::new(ChildView::new(&self.file_tree).finish())
                .with_width(self.size.x())
                .with_height(self.size.y())
                .finish(),
        )
        .with_margin_left(self.origin.x())
        .with_margin_top(self.origin.y())
        .finish()
    }
}

impl TypedActionView for OffsetPanel {
    type Action = ();
}

#[test]
fn the_dialog_is_centred_in_the_window_wherever_the_file_tree_sits() {
    VirtualFS::test("file_tree_delete_centred", |dirs, mut vfs| {
        vfs.mkdir("tree")
            .with_files(vec![Stub::FileWithContent("tree/victim.txt", "victim\n")]);
        let tree = dirs.tests().join("tree");
        let victim = tree.join("victim.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            app.update(super::init);
            // The Project Explorer's usual place, along the left edge below the tab bar, and two
            // others, so the file tree's own position can't pass for the window's.
            for (origin, size) in [
                (vec2f(48., 40.), vec2f(300., 700.)),
                (vec2f(0., 0.), vec2f(240., 768.)),
                (vec2f(640., 360.), vec2f(360., 380.)),
            ] {
                let (window_id, panel) = app.add_window(WindowStyle::NotStealFocus, move |ctx| {
                    let file_tree = ctx.add_typed_action_view(FileTreeView::new);
                    OffsetPanel {
                        file_tree,
                        origin,
                        size,
                    }
                });
                let view = panel.read(&app, |panel, _| panel.file_tree.clone());
                view.update(&mut app, |view, ctx| {
                    view.set_is_active(true, ctx);
                    view.set_root_directories(vec![tree.clone()], ctx);
                });
                let (box_id, _) = dialog_position_ids(&app, &view);
                let tree_id = view.read(&app, |view, _| view.position_id.clone());
                let window_size = window_size(&app, window_id);

                choose_delete(&mut app, window_id, &view, &tree, &victim);
                let file_tree =
                    drawn_at(&app, window_id, &tree_id).expect("the file tree is drawn");
                assert!(
                    (file_tree.origin() - origin).length() < 1.,
                    "the file tree is in its panel at {origin:?}: {file_tree:?}"
                );
                let dialog_box = drawn_at(&app, window_id, &box_id).expect("the dialog is drawn");
                let off_centre = dialog_box.center() - window_size * 0.5;
                assert!(
                    off_centre.x().abs() < 1. && off_centre.y().abs() < 1.,
                    "the dialog is centred in the {window_size:?} window, not on the file tree \
                     at {file_tree:?}: {dialog_box:?}"
                );
                assert!(
                    dialog_box.min_x() >= 0.
                        && dialog_box.min_y() >= 0.
                        && dialog_box.max_x() <= window_size.x()
                        && dialog_box.max_y() <= window_size.y()
                        && dialog_box.width() > 0.
                        && dialog_box.height() > 0.,
                    "the whole dialog is inside the window: {dialog_box:?}"
                );
                click_cancel(&mut app, window_id, &view);
            }
        });
    });
}

#[test]
fn a_release_on_delete_whose_press_began_before_the_dialog_opened_does_nothing() {
    VirtualFS::test("file_tree_delete_stale_press", |dirs, mut vfs| {
        vfs.mkdir("tree").with_files(vec![
            Stub::FileWithContent("tree/keep.txt", "keep\n"),
            Stub::FileWithContent("tree/victim.txt", "victim\n"),
        ]);
        let test_dir = dirs.tests().clone();
        let tree = test_dir.join("tree");
        let victim = tree.join("victim.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            // Delete is armed the moment the dialog opens: this is about where the press began,
            // not about the delay.
            let (window_id, view) = open_file_tree(&mut app, &tree);
            let observer = observe_deletes(&mut app, &view);
            let (_, delete_id) = dialog_position_ids(&app, &view);
            let before = paths_under(&tree);

            // The button goes down on Delete, and the dialog closes before it comes back up.
            choose_delete(&mut app, window_id, &view, &tree, &victim);
            let delete_button = drawn_at(&app, window_id, &delete_id).expect("Delete is drawn");
            mouse_down_at(&mut app, window_id, delete_button.center(), 1);
            assert!(
                press(&mut app, window_id, &view, "escape"),
                "Escape closes the dialog"
            );

            // A new dialog opens, and the button, still down, comes up on its Delete.
            choose_delete(&mut app, window_id, &view, &tree, &victim);
            assert!(is_delete_armed(&app, &view), "Delete is armed");
            let delete_button = drawn_at(&app, window_id, &delete_id).expect("Delete is drawn");
            mouse_up_at(&mut app, window_id, delete_button.center());
            view.read(&app, |view, _| {
                assert!(view.pending_delete.is_some(), "the dialog is still open");
                assert!(view.deletes_in_flight.is_empty(), "no delete was started");
            });
            assert_eq!(paths_under(&tree), before);

            // A whole click on Delete, press and release, deletes.
            click_delete_at(
                &mut app,
                window_id,
                &view,
                &test_dir,
                delete_button.center(),
                1,
            );
            wait_for_deletes(&mut app, &view).await;
            let mut expected = before.clone();
            expected.remove(&victim);
            assert_eq!(paths_under(&tree), expected, "only victim.txt is gone");
            observer.read(&app, |observer, _| {
                assert_eq!(observer.deleted, vec![victim.clone()]);
            });
        });
    });
}

#[test]
fn a_press_on_delete_before_it_is_armed_does_nothing_even_when_released_after() {
    VirtualFS::test("file_tree_delete_early_press", |dirs, mut vfs| {
        vfs.mkdir("tree")
            .with_files(vec![Stub::FileWithContent("tree/victim.txt", "victim\n")]);
        let test_dir = dirs.tests().clone();
        let tree = test_dir.join("tree");
        let victim = tree.join("victim.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            set_arm_delay(&mut app, &view, TEST_ARM_DELAY);
            let (_, delete_id) = dialog_position_ids(&app, &view);
            let before = paths_under(&tree);

            choose_delete(&mut app, window_id, &view, &tree, &victim);
            assert!(!is_delete_armed(&app, &view), "Delete starts disabled");
            let delete_button = drawn_at(&app, window_id, &delete_id).expect("Delete is drawn");
            mouse_down_at(&mut app, window_id, delete_button.center(), 1);
            // The button is still down when Delete is armed, and then comes up on it.
            wait_until_armed(&mut app, &view).await;
            let delete_button = drawn_at(&app, window_id, &delete_id).expect("Delete is drawn");
            mouse_up_at(&mut app, window_id, delete_button.center());
            view.read(&app, |view, _| {
                assert!(view.pending_delete.is_some(), "the dialog is still open");
                assert!(view.deletes_in_flight.is_empty(), "no delete was started");
            });
            assert_eq!(paths_under(&tree), before);

            // A click that begins once Delete is armed deletes.
            click_delete_at(
                &mut app,
                window_id,
                &view,
                &test_dir,
                delete_button.center(),
                1,
            );
            wait_for_deletes(&mut app, &view).await;
            let mut expected = before.clone();
            expected.remove(&victim);
            assert_eq!(paths_under(&tree), expected, "victim.txt is gone");
        });
    });
}

/// Opens the context menu on `path`'s row, the way a right-click does.
fn open_context_menu(
    app: &mut App,
    window_id: WindowId,
    view: &ViewHandle<FileTreeView>,
    root: &Path,
    path: &Path,
) {
    let id = view.read(app, |view, _| row_of(view, root, path));
    app.dispatch_typed_action(
        window_id,
        &[view.id()],
        &FileTreeAction::OpenContextMenu {
            position: vec2f(40., 40.),
            id,
        },
    );
    view.read(app, |view, _| {
        assert!(view.context_menu_state.is_some(), "the menu is open");
    });
}

#[test]
fn choosing_delete_in_the_context_menu_opens_the_dialog_with_focus() {
    VirtualFS::test("file_tree_delete_through_menu", |dirs, mut vfs| {
        vfs.mkdir("tree")
            .with_files(vec![Stub::FileWithContent("tree/victim.txt", "victim\n")]);
        let tree = dirs.tests().join("tree");
        let victim = tree.join("victim.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            let before = paths_under(&tree);
            let (menu_id, dialog_id) = view.read(&app, |view, _| {
                (view.context_menu.id(), view.delete_dialog.id())
            });
            let assert_dialog_open_with_focus = |app: &App, how: &str| {
                view.read(app, |view, _| {
                    let target = view
                        .pending_delete
                        .as_ref()
                        .unwrap_or_else(|| panic!("{how}: the dialog is open"));
                    assert_eq!(target.local_path, victim, "{how}");
                    assert!(view.context_menu_state.is_none(), "{how}: the menu closed");
                    assert!(
                        view.deletes_in_flight.is_empty(),
                        "{how}: nothing was deleted"
                    );
                });
                assert_eq!(
                    app.focused_view_id(window_id),
                    Some(dialog_id),
                    "{how}: the dialog has focus"
                );
            };

            // By keyboard: Return on the menu's "Delete…" row.
            open_context_menu(&mut app, window_id, &view, &tree, &victim);
            view.update(&mut app, |view, ctx| {
                view.context_menu.update(ctx, |menu, ctx| {
                    assert!(
                        menu.set_selected_by_name("Delete…", ctx),
                        "the menu has Delete…"
                    );
                });
            });
            app.dispatch_typed_action(window_id, &[view.id(), menu_id], &MenuAction::Enter);
            assert_dialog_open_with_focus(&app, "Return");
            click_cancel(&mut app, window_id, &view);

            // By mouse: what a click on the row sends after selecting it, in order.
            open_context_menu(&mut app, window_id, &view, &tree, &victim);
            let delete = view.read(&app, |view, _| delete_action(view, &tree, &victim));
            app.dispatch_typed_action(window_id, &[view.id(), menu_id], &delete);
            app.dispatch_typed_action(window_id, &[view.id(), menu_id], &MenuAction::Close(true));
            assert_dialog_open_with_focus(&app, "a click");

            Timer::after(Duration::from_millis(50)).await;
            assert_eq!(paths_under(&tree), before);
        });
    });
}

#[test]
fn the_dialog_closes_when_focus_leaves_the_file_tree() {
    VirtualFS::test("file_tree_delete_focus_leaves", |dirs, mut vfs| {
        vfs.mkdir("tree")
            .with_files(vec![Stub::FileWithContent("tree/victim.txt", "victim\n")]);
        let tree = dirs.tests().join("tree");
        let victim = tree.join("victim.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            // Something outside the file tree that can take focus, as a new tab's terminal does.
            let elsewhere = app.add_view(window_id, |_| {
                ActionButton::new("Elsewhere", SecondaryTheme)
            });
            let before = paths_under(&tree);

            choose_delete(&mut app, window_id, &view, &tree, &victim);
            elsewhere.update(&mut app, |_, ctx| ctx.focus_self());

            view.read(&app, |view, _| {
                assert!(view.pending_delete.is_none(), "the dialog closed");
                assert!(view.deletes_in_flight.is_empty(), "no delete was started");
            });
            assert_eq!(
                app.focused_view_id(window_id),
                Some(elsewhere.id()),
                "the file tree didn't take focus back"
            );

            // The same when focus moves from the dialog to the file tree itself.
            choose_delete(&mut app, window_id, &view, &tree, &victim);
            view.update(&mut app, |_, ctx| ctx.focus_self());
            view.read(&app, |view, _| {
                assert!(view.pending_delete.is_none(), "the dialog closed");
                assert!(view.deletes_in_flight.is_empty(), "no delete was started");
            });
            assert_eq!(app.focused_view_id(window_id), Some(view.id()));

            Timer::after(Duration::from_millis(50)).await;
            assert_eq!(paths_under(&tree), before);
        });
    });
}

#[test]
fn the_dialog_closes_when_the_file_tree_swaps_its_list_for_a_message() {
    VirtualFS::test("file_tree_delete_list_hidden", |dirs, mut vfs| {
        vfs.mkdir("tree")
            .with_files(vec![Stub::FileWithContent("tree/victim.txt", "victim\n")]);
        let tree = dirs.tests().join("tree");
        let victim = tree.join("victim.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            let (box_id, _) = dialog_position_ids(&app, &view);
            let before = paths_under(&tree);

            for message_state in [
                CodingPanelEnablementState::PendingRemoteSession,
                CodingPanelEnablementState::Disabled,
            ] {
                choose_delete(&mut app, window_id, &view, &tree, &victim);
                assert!(
                    drawn_at(&app, window_id, &box_id).is_some(),
                    "the dialog is drawn over the list"
                );

                // The session becomes one the tree can't show, so a message replaces the list.
                view.update(&mut app, |view, ctx| {
                    view.set_enablement_state(message_state, ctx);
                });
                view.read(&app, |view, _| {
                    assert!(view.pending_delete.is_none(), "the dialog closed");
                    assert!(view.deletes_in_flight.is_empty(), "nothing was deleted");
                });
                assert!(
                    drawn_at(&app, window_id, &box_id).is_none(),
                    "the dialog is gone"
                );

                // The list comes back, and the dialog doesn't.
                view.update(&mut app, |view, ctx| {
                    view.set_enablement_state(CodingPanelEnablementState::Enabled, ctx);
                });
                view.read(&app, |view, _| {
                    assert!(view.pending_delete.is_none(), "the dialog stays closed");
                });
                assert!(
                    drawn_at(&app, window_id, &box_id).is_none(),
                    "the dialog doesn't reappear"
                );
            }

            Timer::after(Duration::from_millis(50)).await;
            assert_eq!(paths_under(&tree), before);
        });
    });
}

#[test]
fn the_dialog_closes_when_the_file_tree_changes_roots() {
    VirtualFS::test("file_tree_delete_roots_change", |dirs, mut vfs| {
        vfs.mkdir("tree").mkdir("other").with_files(vec![
            Stub::FileWithContent("tree/victim.txt", "victim\n"),
            Stub::FileWithContent("other/other.txt", "other\n"),
        ]);
        let tree = dirs.tests().join("tree");
        let other = dirs.tests().join("other");
        let victim = tree.join("victim.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            let before = paths_under(&tree);

            // Being told the same root again, as happens whenever the tab reports its folder,
            // leaves the dialog open.
            choose_delete(&mut app, window_id, &view, &tree, &victim);
            view.update(&mut app, |view, ctx| {
                view.set_root_directories(vec![tree.clone()], ctx);
            });
            view.read(&app, |view, _| {
                assert!(view.pending_delete.is_some(), "the dialog is still open");
            });

            // A cd in the tab changes the root, and the dialog closes.
            view.update(&mut app, |view, ctx| {
                view.set_root_directories(vec![other.clone()], ctx);
            });
            view.read(&app, |view, _| {
                assert!(view.pending_delete.is_none(), "the dialog closed");
                assert!(view.deletes_in_flight.is_empty(), "nothing was deleted");
            });

            Timer::after(Duration::from_millis(50)).await;
            assert_eq!(paths_under(&tree), before);
        });
    });
}

#[test]
fn confirm_deletes_the_captured_path_after_the_list_shifts() {
    VirtualFS::test("file_tree_delete_index_shift", |dirs, mut vfs| {
        vfs.mkdir("tree").with_files(vec![
            Stub::FileWithContent("tree/b.txt", "b\n"),
            Stub::FileWithContent("tree/c.txt", "c\n"),
        ]);
        let test_dir = dirs.tests().clone();
        let tree = test_dir.join("tree");
        let a0 = tree.join("a0.txt");
        let b = tree.join("b.txt");
        let c = tree.join("c.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            let observer = observe_deletes(&mut app, &view);
            let captured_row = view.read(&app, |view, _| row_of(view, &tree, &c));

            choose_delete(&mut app, window_id, &view, &tree, &c);

            // A new file sorts in above c.txt, the way a file-watcher rebuild would add it, so
            // the row the menu captured now holds b.txt.
            std::fs::write(&a0, "a0\n").expect("create a0.txt");
            add_file_to_tree(&mut app, &view, &tree, &a0);
            view.read(&app, |view, _| {
                let items = &view.root_directories[&std_path(&tree)].items;
                assert_eq!(items[captured_row.index].path(), &std_path(&b));
            });
            let before = paths_under(&tree);

            click_delete(&mut app, window_id, &view, &test_dir);
            // While that delete runs, another request for the same file is ignored.
            choose_delete(&mut app, window_id, &view, &tree, &c);
            view.read(&app, |view, _| assert!(view.pending_delete.is_none()));
            wait_for_deletes(&mut app, &view).await;

            let mut expected = before.clone();
            expected.remove(&c);
            assert_eq!(paths_under(&tree), expected, "only c.txt is gone");
            view.read(&app, |view, _| {
                let paths = flattened_paths(view, &tree);
                assert!(!paths.contains(&std_path(&c)));
                assert!(paths.contains(&std_path(&b)));
                assert!(paths.contains(&std_path(&a0)));
            });
            observer.read(&app, |observer, _| {
                assert_eq!(observer.deleted, vec![c.clone()]);
                assert_eq!(observer.toasts, 0);
            });
            assert_eq!(app.focused_view_id(window_id), Some(view.id()));
        });
    });
}

#[test]
fn confirm_aborts_when_the_file_was_replaced() {
    VirtualFS::test("file_tree_delete_replaced", |dirs, mut vfs| {
        vfs.mkdir("tree")
            .with_files(vec![Stub::FileWithContent("tree/victim.txt", "old\n")]);
        let test_dir = dirs.tests().clone();
        let tree = test_dir.join("tree");
        let victim = tree.join("victim.txt");
        let replacement = tree.join("victim.txt.new");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            let observer = observe_deletes(&mut app, &view);

            choose_delete(&mut app, window_id, &view, &tree, &victim);

            // Replace the file the way an editor's atomic save does. Both files exist at once,
            // so the new one can't reuse the old inode, and its size differs too.
            std::fs::write(&replacement, "replacement contents\n").expect("write the replacement");
            assert_inside_test_dir(&victim, &test_dir);
            std::fs::rename(&replacement, &victim).expect("replace victim.txt");
            let before = paths_under(&tree);

            click_delete(&mut app, window_id, &view, &test_dir);
            wait_for_deletes(&mut app, &view).await;

            assert_eq!(paths_under(&tree), before, "nothing was deleted");
            assert_eq!(
                std::fs::read_to_string(&victim).expect("victim.txt is still there"),
                "replacement contents\n"
            );
            view.read(&app, |view, _| {
                assert!(flattened_paths(view, &tree).contains(&std_path(&victim)));
            });
            observer.read(&app, |observer, _| {
                assert_eq!(observer.toasts, 1, "an error toast explains why");
                assert!(observer.deleted.is_empty());
            });
        });
    });
}

#[cfg(unix)]
#[test]
fn confirm_on_a_symlink_removes_only_the_link() {
    VirtualFS::test("file_tree_delete_symlink", |dirs, mut vfs| {
        vfs.mkdir("tree")
            .with_files(vec![Stub::FileWithContent("tree/keep.txt", "keep\n")]);
        vfs.ln("tree/keep.txt", "tree/link-to-keep");
        let test_dir = dirs.tests().clone();
        let tree = test_dir.join("tree");
        let target = tree.join("keep.txt");
        let link = tree.join("link-to-keep");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            let observer = observe_deletes(&mut app, &view);
            let before = paths_under(&tree);

            choose_delete(&mut app, window_id, &view, &tree, &link);
            view.read(&app, |view, _| {
                let pending = view.pending_delete.as_ref().expect("the dialog is open");
                assert_eq!(pending.kind, ItemKind::Symlink);
            });
            click_delete(&mut app, window_id, &view, &test_dir);
            wait_for_deletes(&mut app, &view).await;

            let mut expected = before.clone();
            expected.remove(&link);
            assert_eq!(paths_under(&tree), expected, "only the link is gone");
            assert_eq!(
                std::fs::read_to_string(&target).expect("the link's target survives"),
                "keep\n"
            );
            observer.read(&app, |observer, _| {
                assert_eq!(observer.deleted, vec![link.clone()]);
            });
        });
    });
}

#[test]
fn confirm_on_a_folder_removes_it_and_everything_in_it() {
    VirtualFS::test("file_tree_delete_folder", |dirs, mut vfs| {
        vfs.mkdir("tree/victim-folder/nested")
            .mkdir("tree/keep-folder")
            .with_files(vec![
                Stub::FileWithContent("tree/victim-folder/a.txt", "a\n"),
                Stub::FileWithContent("tree/victim-folder/nested/leaf.txt", "leaf\n"),
                Stub::FileWithContent("tree/keep-folder/k.txt", "k\n"),
                Stub::FileWithContent("tree/sibling.txt", "sibling\n"),
            ]);
        let test_dir = dirs.tests().clone();
        let tree = test_dir.join("tree");
        let folder = tree.join("victim-folder");
        let keep_folder = tree.join("keep-folder");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            let observer = observe_deletes(&mut app, &view);

            // A folder that was never expanded isn't loaded, so its count is left out.
            choose_delete(&mut app, window_id, &view, &tree, &keep_folder);
            view.read(&app, |view, _| {
                let pending = view.pending_delete.as_ref().expect("the dialog is open");
                assert_eq!(pending.kind, ItemKind::Directory);
                assert_eq!(pending.child_count, None);
            });
            click_cancel(&mut app, window_id, &view);

            // Expanding the folder loads it, and the count comes from the in-memory tree.
            view.update(&mut app, |view, ctx| {
                view.toggle_folder_expansion(&std_path(&tree), &std_path(&folder), ctx);
            });
            choose_delete(&mut app, window_id, &view, &tree, &folder);
            view.read(&app, |view, _| {
                let pending = view.pending_delete.as_ref().expect("the dialog is open");
                assert_eq!(pending.kind, ItemKind::Directory);
                assert_eq!(pending.child_count, Some(2));
            });
            let before = paths_under(&tree);

            click_delete(&mut app, window_id, &view, &test_dir);
            wait_for_deletes(&mut app, &view).await;

            let expected: BTreeSet<PathBuf> = before
                .iter()
                .filter(|path| !path.starts_with(&folder))
                .cloned()
                .collect();
            assert_eq!(
                before.len() - expected.len(),
                4,
                "the folder and its 3 entries"
            );
            assert_eq!(paths_under(&tree), expected, "siblings are intact");
            observer.read(&app, |observer, _| {
                assert_eq!(observer.deleted, vec![folder.clone()]);
                assert_eq!(observer.toasts, 0);
            });
        });
    });
}

#[test]
fn a_stale_menu_action_is_refused() {
    VirtualFS::test("file_tree_delete_stale_menu", |dirs, mut vfs| {
        vfs.mkdir("tree").with_files(vec![
            Stub::FileWithContent("tree/b.txt", "b\n"),
            Stub::FileWithContent("tree/c.txt", "c\n"),
        ]);
        let tree = dirs.tests().join("tree");
        let a0 = tree.join("a0.txt");
        let c = tree.join("c.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            let observer = observe_deletes(&mut app, &view);

            // The menu is built for c.txt, then the list shifts before Delete is chosen.
            let stale = view.read(&app, |view, _| delete_action(view, &tree, &c));
            std::fs::write(&a0, "a0\n").expect("create a0.txt");
            add_file_to_tree(&mut app, &view, &tree, &a0);
            let before = paths_under(&tree);

            app.dispatch_typed_action(window_id, &[view.id()], &stale);

            view.read(&app, |view, _| {
                assert!(view.pending_delete.is_none(), "no dialog opens");
                assert!(view.deletes_in_flight.is_empty(), "no delete was started");
            });
            observer.read(&app, |observer, _| {
                assert_eq!(observer.toasts, 1, "a toast explains why");
            });
            Timer::after(Duration::from_millis(50)).await;
            assert_eq!(paths_under(&tree), before);
        });
    });
}

/// Makes a folder read-only for as long as it lives, then restores it so the test folder can be
/// cleaned up even when an assertion fails. Gives `None` when the tests run as root, which
/// permissions don't stop, so a read-only folder would prove nothing.
#[cfg(unix)]
struct ReadOnlyFolder(PathBuf);

#[cfg(unix)]
impl ReadOnlyFolder {
    fn new(path: PathBuf, test_dir: &Path) -> Option<Self> {
        use std::os::unix::fs::PermissionsExt;

        if nix::unistd::geteuid().is_root() {
            eprintln!("Skipping: running as root, so a read-only folder doesn't stop a delete");
            return None;
        }
        assert_inside_test_dir(&path, test_dir);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o555))
            .expect("make the folder read-only");
        Some(Self(path))
    }
}

#[cfg(unix)]
impl Drop for ReadOnlyFolder {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;

        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

#[cfg(unix)]
#[test]
fn a_failed_delete_keeps_the_item_and_shows_an_error() {
    VirtualFS::test("file_tree_delete_fails", |dirs, mut vfs| {
        vfs.mkdir("tree/locked")
            .with_files(vec![Stub::FileWithContent(
                "tree/locked/file.txt",
                "stay\n",
            )]);
        let test_dir = dirs.tests().clone();
        let tree = test_dir.join("tree");
        let locked = tree.join("locked");
        let file = locked.join("file.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            let observer = observe_deletes(&mut app, &view);
            view.update(&mut app, |view, ctx| {
                view.toggle_folder_expansion(&std_path(&tree), &std_path(&locked), ctx);
            });
            // Removing a file needs write access to its folder.
            let Some(_read_only) = ReadOnlyFolder::new(locked.clone(), &test_dir) else {
                return;
            };
            let before = paths_under(&tree);

            choose_delete(&mut app, window_id, &view, &tree, &file);
            click_delete(&mut app, window_id, &view, &test_dir);
            wait_for_deletes(&mut app, &view).await;

            assert_eq!(paths_under(&tree), before, "nothing was deleted");
            view.read(&app, |view, _| {
                assert!(flattened_paths(view, &tree).contains(&std_path(&file)));
            });
            observer.read(&app, |observer, _| {
                assert_eq!(observer.toasts, 1, "an error toast explains why");
                assert!(observer.deleted.is_empty());
            });
        });
    });
}

#[test]
fn confirm_aborts_when_the_file_was_deleted_and_recreated() {
    VirtualFS::test("file_tree_delete_recreated", |dirs, mut vfs| {
        vfs.mkdir("tree")
            .with_files(vec![Stub::FileWithContent("tree/victim.txt", "old\n")]);
        let test_dir = dirs.tests().clone();
        let tree = test_dir.join("tree");
        let victim = tree.join("victim.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            let observer = observe_deletes(&mut app, &view);
            if let Err(error) = std::fs::symlink_metadata(&victim).expect("lstat").created() {
                // Without a birth time the check has only the inode, which Linux can reuse.
                eprintln!("Skipping: this file system doesn't record birth times ({error})");
                return;
            }

            choose_delete(&mut app, window_id, &view, &tree, &victim);

            // `rm victim.txt; echo new > victim.txt`: the old file is gone before the new one
            // exists, so the new one can be given the old inode. Linux stamps birth times at the
            // kernel's clock tick, so let one pass in between.
            assert_inside_test_dir(&victim, &test_dir);
            std::fs::remove_file(&victim).expect("delete victim.txt");
            Timer::after(Duration::from_millis(50)).await;
            std::fs::write(&victim, "new\n").expect("recreate victim.txt");
            let before = paths_under(&tree);

            click_delete(&mut app, window_id, &view, &test_dir);
            wait_for_deletes(&mut app, &view).await;

            assert_eq!(paths_under(&tree), before, "nothing was deleted");
            assert_eq!(
                std::fs::read_to_string(&victim).expect("victim.txt is still there"),
                "new\n"
            );
            observer.read(&app, |observer, _| {
                assert_eq!(observer.toasts, 1, "an error toast explains why");
                assert!(observer.deleted.is_empty());
            });
        });
    });
}

#[cfg(unix)]
#[test]
fn a_folder_delete_that_stops_partway_says_so_and_drops_what_went() {
    VirtualFS::test("file_tree_delete_partial", |dirs, mut vfs| {
        vfs.mkdir("tree/locked/victim-folder/nested")
            .with_files(vec![
                Stub::FileWithContent("tree/locked/victim-folder/a.txt", "a\n"),
                Stub::FileWithContent("tree/locked/victim-folder/nested/leaf.txt", "leaf\n"),
            ]);
        let test_dir = dirs.tests().clone();
        let tree = test_dir.join("tree");
        let locked = tree.join("locked");
        let folder = locked.join("victim-folder");
        let a = folder.join("a.txt");
        let nested = folder.join("nested");
        let leaf = nested.join("leaf.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            let observer = observe_deletes(&mut app, &view);
            view.update(&mut app, |view, ctx| {
                for path in [&locked, &folder, &nested] {
                    view.toggle_folder_expansion(&std_path(&tree), &std_path(path), ctx);
                }
            });
            view.read(&app, |view, _| {
                assert!(
                    flattened_paths(view, &tree).contains(&std_path(&leaf)),
                    "the whole folder is loaded"
                );
            });
            // Everything inside the folder can be removed, but the folder itself can't, because
            // removing it needs write access to `locked`. So the delete gets through all of the
            // contents before it fails.
            let Some(_read_only) = ReadOnlyFolder::new(locked.clone(), &test_dir) else {
                return;
            };

            choose_delete(&mut app, window_id, &view, &tree, &folder);
            click_delete(&mut app, window_id, &view, &test_dir);
            wait_for_deletes(&mut app, &view).await;

            assert!(folder.is_dir(), "the folder itself is still there");
            assert_eq!(
                paths_under(&folder),
                BTreeSet::new(),
                "everything inside it is gone"
            );
            view.read(&app, |view, _| {
                let paths = flattened_paths(view, &tree);
                assert!(
                    paths.contains(&std_path(&folder)),
                    "the tree still shows the folder"
                );
                for gone in [&a, &nested, &leaf] {
                    assert!(
                        !paths.contains(&std_path(gone)),
                        "the tree still shows {}",
                        gone.display()
                    );
                }
            });
            observer.read(&app, |observer, _| {
                assert_eq!(
                    observer.toasts, 1,
                    "a toast says the delete stopped partway"
                );
                assert!(
                    !observer.deleted.contains(&folder),
                    "the folder wasn't deleted"
                );
                assert!(
                    observer.deleted.contains(&a),
                    "tabs open on what was deleted are closed"
                );
            });
        });
    });
}

/// Renames the item at `path` to `new_name`, the way typing a new name into the file tree's
/// rename field and pressing Return does.
fn rename(
    app: &mut App,
    view: &ViewHandle<FileTreeView>,
    root: &Path,
    path: &Path,
    new_name: &str,
) {
    view.update(app, |view, ctx| {
        let row = row_of(view, root, path);
        view.start_rename(&row, ctx);
        view.editor_view
            .update(ctx, |editor, ctx| editor.set_buffer_text(new_name, ctx));
        view.commit_pending_edit(ctx);
    });
}

#[test]
fn rename_refuses_to_replace_an_item_that_has_the_new_name() {
    VirtualFS::test("file_tree_rename_taken", |dirs, mut vfs| {
        vfs.mkdir("tree").with_files(vec![
            Stub::FileWithContent("tree/a.txt", "a\n"),
            Stub::FileWithContent("tree/b.txt", "b\n"),
        ]);
        let tree = dirs.tests().join("tree");
        let a = tree.join("a.txt");
        let b = tree.join("b.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (_, view) = open_file_tree(&mut app, &tree);
            let observer = observe_deletes(&mut app, &view);
            let before = paths_under(&tree);

            rename(&mut app, &view, &tree, &a, "b.txt");

            assert_eq!(paths_under(&tree), before, "nothing was renamed");
            assert_eq!(
                std::fs::read_to_string(&b).expect("b.txt is still there"),
                "b\n",
                "b.txt wasn't replaced"
            );
            view.read(&app, |view, _| {
                let paths = flattened_paths(view, &tree);
                assert!(paths.contains(&std_path(&a)));
                assert!(paths.contains(&std_path(&b)));
            });
            observer.read(&app, |observer, _| {
                assert_eq!(observer.toasts, 1, "a toast explains why");
            });
        });
    });
}

#[test]
fn rename_can_change_only_the_case_of_a_name() {
    VirtualFS::test("file_tree_rename_case", |dirs, mut vfs| {
        vfs.mkdir("tree")
            .with_files(vec![Stub::FileWithContent("tree/notes.txt", "notes\n")]);
        let tree = dirs.tests().join("tree");
        let notes = tree.join("notes.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (_, view) = open_file_tree(&mut app, &tree);
            let observer = observe_deletes(&mut app, &view);

            rename(&mut app, &view, &tree, &notes, "NOTES.txt");

            let names: Vec<String> = std::fs::read_dir(&tree)
                .expect("the test folder is readable")
                .map(|entry| {
                    entry
                        .expect("the test folder entry is readable")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            assert_eq!(names, ["NOTES.txt"]);
            assert_eq!(
                std::fs::read_to_string(tree.join("NOTES.txt")).expect("NOTES.txt is there"),
                "notes\n"
            );
            observer.read(&app, |observer, _| assert_eq!(observer.toasts, 0));
        });
    });
}

#[test]
fn the_new_file_placeholder_offers_no_delete_and_cant_be_deleted() {
    VirtualFS::test("file_tree_delete_placeholder", |dirs, mut vfs| {
        // A real file already has the name the placeholder shows until a name is typed.
        vfs.mkdir("tree")
            .with_files(vec![Stub::FileWithContent("tree/new_file", "real\n")]);
        let tree = dirs.tests().join("tree");
        let real = tree.join("new_file");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            let before = paths_under(&tree);

            // "New file" on the root adds a row for the file whose name is being typed.
            let placeholder = view.update(&mut app, |view, ctx| {
                let root_row = row_of(view, &tree, &tree);
                view.create_new_file(&root_row, ctx);
                view.pending_edit
                    .as_ref()
                    .expect("a new file is being named")
                    .id
                    .clone()
            });
            view.read(&app, |view, _| {
                let item = &view.root_directories[&placeholder.root].items[placeholder.index];
                assert_eq!(
                    item.path(),
                    &std_path(&real),
                    "the placeholder has the real file's path"
                );
                let labels: Vec<String> = view
                    .context_menu_items(item, &placeholder)
                    .iter()
                    .filter_map(|menu_item| menu_item.fields())
                    .map(|fields| fields.label().to_owned())
                    .collect();
                assert!(
                    !labels
                        .iter()
                        .any(|label| label.starts_with("Delete") || label == "Rename"),
                    "the placeholder offers neither Delete… nor Rename: {labels:?}"
                );
            });

            // A Delete for the placeholder's row, however it arrives, is refused.
            app.dispatch_typed_action(
                window_id,
                &[view.id()],
                &FileTreeAction::Delete {
                    id: placeholder.clone(),
                    path: std_path(&real),
                },
            );
            view.read(&app, |view, _| {
                assert!(view.pending_delete.is_none(), "no dialog opens");
            });
            Timer::after(Duration::from_millis(50)).await;
            assert_eq!(paths_under(&tree), before);
            assert_eq!(
                std::fs::read_to_string(&real).expect("new_file is still there"),
                "real\n"
            );
        });
    });
}

#[test]
fn a_folder_that_is_also_a_displayed_root_cant_be_deleted_from_another_roots_list() {
    VirtualFS::test("file_tree_delete_nested_root", |dirs, mut vfs| {
        vfs.mkdir("tree/sub").with_files(vec![
            Stub::FileWithContent("tree/sub/inner.txt", "inner\n"),
            Stub::FileWithContent("tree/victim.txt", "victim\n"),
        ]);
        let tree = dirs.tests().join("tree");
        let sub = tree.join("sub");
        let victim = tree.join("victim.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            // Display `sub` as a root of its own as well. Given both, the tree folds a folder
            // inside a root into that root, so the state is set up directly.
            view.update(&mut app, |view, _| {
                let sub_root = std_path(&sub);
                view.root_directories.insert(
                    sub_root.clone(),
                    RootDirectory {
                        entry: FileTreeView::create_empty_entry(&sub_root),
                        expanded_folders: HashSet::new(),
                        items: Vec::new(),
                        item_states: HashMap::new(),
                        remote_host_id: None,
                    },
                );
                view.displayed_directories.push(sub_root);
                view.rebuild_flattened_items();
            });
            let before = paths_under(&tree);

            view.read(&app, |view, _| {
                let row = row_of(view, &tree, &sub);
                assert_ne!(row.index, 0, "in tree's list, sub isn't at row 0");
                let item = &view.root_directories[&row.root].items[row.index];
                let labels: Vec<String> = view
                    .context_menu_items(item, &row)
                    .iter()
                    .filter_map(|menu_item| menu_item.fields())
                    .map(|fields| fields.label().to_owned())
                    .collect();
                assert!(
                    !labels
                        .iter()
                        .any(|label| label.starts_with("Delete") || label == "Rename"),
                    "a displayed root offers neither Delete… nor Rename: {labels:?}"
                );
            });

            choose_delete(&mut app, window_id, &view, &tree, &sub);
            view.read(&app, |view, _| {
                assert!(view.pending_delete.is_none(), "no dialog opens");
            });

            // Its sibling, which holds no root, still can be.
            choose_delete(&mut app, window_id, &view, &tree, &victim);
            view.read(&app, |view, _| {
                assert!(view.pending_delete.is_some(), "the dialog opens");
            });
            click_cancel(&mut app, window_id, &view);

            Timer::after(Duration::from_millis(50)).await;
            assert_eq!(paths_under(&tree), before);
        });
    });
}

#[test]
fn an_item_that_is_already_gone_is_reported_and_taken_out_of_the_tree() {
    VirtualFS::test("file_tree_delete_already_gone", |dirs, mut vfs| {
        vfs.mkdir("tree").with_files(vec![
            Stub::FileWithContent("tree/keep.txt", "keep\n"),
            Stub::FileWithContent("tree/victim.txt", "victim\n"),
        ]);
        let test_dir = dirs.tests().clone();
        let tree = test_dir.join("tree");
        let keep = tree.join("keep.txt");
        let victim = tree.join("victim.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            let observer = observe_deletes(&mut app, &view);

            // Something else deletes it while the dialog is open.
            choose_delete(&mut app, window_id, &view, &tree, &victim);
            assert_inside_test_dir(&victim, &test_dir);
            std::fs::remove_file(&victim).expect("delete victim.txt");
            click_delete(&mut app, window_id, &view, &test_dir);
            wait_for_deletes(&mut app, &view).await;

            view.read(&app, |view, _| {
                let paths = flattened_paths(view, &tree);
                assert!(
                    !paths.contains(&std_path(&victim)),
                    "the tree stops showing it"
                );
                assert!(paths.contains(&std_path(&keep)));
            });
            observer.read(&app, |observer, _| {
                assert_eq!(observer.toasts, 1, "a toast says it's gone");
                assert!(
                    observer.deleted.is_empty(),
                    "no editor tab is closed for it"
                );
            });

            // The same when it's already gone as the dialog would open: the tree still has a row
            // for it, as it does when the file watcher misses a delete.
            add_file_to_tree(&mut app, &view, &tree, &victim);
            choose_delete(&mut app, window_id, &view, &tree, &victim);
            view.read(&app, |view, _| {
                assert!(view.pending_delete.is_none(), "no dialog opens");
                assert!(
                    !flattened_paths(view, &tree).contains(&std_path(&victim)),
                    "the tree stops showing it"
                );
            });
            observer.read(&app, |observer, _| {
                assert_eq!(observer.toasts, 2, "a toast says it's gone");
                assert!(observer.deleted.is_empty());
            });
            assert!(keep.exists());
        });
    });
}

#[cfg(unix)]
#[test]
fn a_test_build_refuses_to_delete_outside_the_temp_folder() {
    VirtualFS::test("file_tree_delete_test_guard", |dirs, mut vfs| {
        vfs.mkdir("tree")
            .with_files(vec![Stub::FileWithContent("tree/victim.txt", "victim\n")]);
        let test_dir = dirs.tests().clone();
        let tree = test_dir.join("tree");
        let victim = tree.join("victim.txt");

        App::test((), |mut app| async move {
            let _ = initialize_app(&mut app);
            let (window_id, view) = open_file_tree(&mut app, &tree);
            let observer = observe_deletes(&mut app, &view);

            // A confirmed delete of something outside the temp folder. Nothing exists at the
            // path, so even a guard that let it through would find nothing to delete.
            let outside = PathBuf::from("/usr/warp-file-tree-test-guard-probe");
            let identity =
                ItemIdentity::from_metadata(&std::fs::symlink_metadata(&victim).expect("lstat"));
            let target = PendingDelete {
                std_path: std_path(&outside),
                local_path: outside.clone(),
                kind: ItemKind::File,
                display_name: "warp-file-tree-test-guard-probe".to_owned(),
                child_count: None,
                identity,
            };
            view.update(&mut app, |view, ctx| view.perform_delete(target, ctx));
            view.read(&app, |view, _| {
                assert!(
                    view.deletes_in_flight.is_empty(),
                    "the delete was refused before it started"
                );
            });
            observer.read(&app, |observer, _| {
                assert_eq!(observer.toasts, 1, "a toast says why");
                assert!(observer.deleted.is_empty());
            });

            // Inside the temp folder, the same route goes ahead.
            choose_delete(&mut app, window_id, &view, &tree, &victim);
            click_delete(&mut app, window_id, &view, &test_dir);
            wait_for_deletes(&mut app, &view).await;
            assert!(
                !victim.exists(),
                "victim.txt, inside the temp folder, is deleted"
            );
        });
    });
}
