use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::Arc;

use ai::workspace::WorkspaceMetadata;
use chrono::Utc;
use cloud_object_persistence::to_cloud_object_permissions;
use diesel::connection::SimpleConnection;
use diesel::sqlite::SqliteConnection;
use diesel::{ExpressionMethods, QueryDsl, RunQueryDsl, SelectableHelper};
use pathfinder_geometry::rect::RectF;
use pathfinder_geometry::vector::Vector2F;
use warp_core::features::FeatureFlag;
use warp_graphql::scalars::time::ServerTimestamp;

use super::{
    app_database_file_path, database_file_path_for_current_scope, database_file_path_for_scope,
    decode_path, deduplicate_events, encode_path, get_all_codebase_index_metadata,
    read_sqlite_data, save_app_state, save_codebase_index_metadata, setup_database, start_writer,
};
use crate::app_state::{
    AppState, BranchSnapshot, CodePaneSnapShot, CodePaneTabSnapshot, LeafContents, LeafSnapshot,
    PaneFlex, PaneNodeSnapshot, SplitDirection, TabGroupSnapshot, TabSnapshot,
    TerminalPaneSnapshot, WindowSnapshot,
};
use crate::cloud_object::{CloudObjectPermissions, Owner};
use crate::code::editor_management::CodeSource;
use crate::notebooks::{CloudNotebook, CloudNotebookModel};
use crate::persistence::model::{ObjectPermissions, PaneMark};
use crate::persistence::pane_marks::take_mirror_warnings;
use crate::persistence::schema;
use crate::persistence::{BlockCompleted, ModelEvent, PersistedDataScope, PersistenceScope};
use crate::server::ids::ClientId;
use crate::tab::SelectedTabColor;
use crate::terminal::model::block::SerializedBlock;
use crate::terminal::ShellLaunchData;
use crate::themes::theme::AnsiColorIdentifier;
use crate::workspace::tab_group::TabGroupId;

#[test]
fn app_scope_database_path_matches_app_database_path() {
    assert_eq!(
        database_file_path_for_scope(&PersistenceScope::App),
        app_database_file_path()
    );
}

#[test]
fn tui_scope_database_path_is_tui_subdirectory_of_app_database_dir() {
    let tui_path = database_file_path_for_scope(&PersistenceScope::Tui);
    let app_path = database_file_path_for_scope(&PersistenceScope::App);

    assert_ne!(tui_path, app_path);
    assert_eq!(
        tui_path,
        warp_core::paths::tui_state_dir().join("warp.sqlite")
    );

    // The TUI database lives in a `tui` subdirectory of the same base
    // directory that holds the GUI database, so the two front-ends never
    // share (or migrate) each other's database.
    let tui_dir = tui_path
        .parent()
        .expect("TUI database path should have a parent");
    assert_eq!(tui_dir.file_name(), Some(OsStr::new("tui")));
    assert_eq!(tui_dir.parent(), app_path.parent());
}

#[test]
fn database_path_for_current_scope_defaults_to_app_scope() {
    // Unit tests never call `persistence::initialize`, so the process-wide
    // scope defaults to `App` and ad-hoc read-only connections resolve to
    // the GUI database. (nextest runs each test in its own process, so no
    // other test can have set the scope.)
    assert_eq!(
        database_file_path_for_current_scope(),
        app_database_file_path()
    );
}

#[test]
fn remote_server_daemon_scope_database_path_uses_identity_data_dir() {
    let path = database_file_path_for_scope(&PersistenceScope::RemoteServerDaemon {
        identity_key: "user@example.com/ssh host".to_string(),
    });
    let expected_data_dir =
        remote_server::setup::remote_server_daemon_data_dir("user@example.com/ssh host");

    assert!(path.is_absolute());
    assert_eq!(
        path,
        PathBuf::from(shellexpand::tilde(&expected_data_dir).into_owned()).join("warp.sqlite")
    );
}

#[test]
fn remote_server_daemon_scope_database_path_handles_empty_identity_key() {
    let path = database_file_path_for_scope(&PersistenceScope::RemoteServerDaemon {
        identity_key: String::new(),
    });
    let expected_data_dir = remote_server::setup::remote_server_daemon_data_dir("");

    assert_eq!(
        path,
        PathBuf::from(shellexpand::tilde(&expected_data_dir).into_owned()).join("warp.sqlite")
    );
}

#[cfg(unix)]
#[test]
fn remote_server_daemon_database_permissions_are_owner_only() {
    use std::fs::Permissions;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let daemon_dir = tempdir.path().join("daemon");
    let database_path = daemon_dir.join("warp.sqlite");

    std::fs::create_dir_all(&daemon_dir).expect("daemon dir should be created");
    std::fs::set_permissions(&daemon_dir, Permissions::from_mode(0o755))
        .expect("daemon dir permissions should be set");
    std::fs::write(&database_path, b"").expect("database file should be created");
    std::fs::set_permissions(&database_path, Permissions::from_mode(0o644))
        .expect("database file permissions should be set");

    super::ensure_owner_only_dir(&daemon_dir).expect("daemon dir should be owner-only");
    super::ensure_owner_only_file(&database_path).expect("database file should be owner-only");

    assert_eq!(daemon_dir.metadata().unwrap().mode() & 0o777, 0o700);
    assert_eq!(database_path.metadata().unwrap().mode() & 0o777, 0o600);
}

fn test_codebase_metadata(path: &str) -> WorkspaceMetadata {
    WorkspaceMetadata {
        path: PathBuf::from(path),
        navigated_ts: Some(Utc::now()),
        modified_ts: None,
        queried_ts: None,
    }
}

#[test]
fn sqlite_read_restores_app_state_and_codebase_metadata() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let database_path = tempdir.path().join("warp.sqlite");
    let mut conn = setup_database(&database_path).expect("database should initialize");

    let app_state = AppState {
        windows: vec![test_terminal_window_snapshot(false)],
        active_window_index: Some(0),
        block_lists: Default::default(),
        running_mcp_servers: Default::default(),
    };
    save_app_state(&mut conn, &app_state).expect("app state should save");

    let metadata = test_codebase_metadata("/tmp/remote-repo");
    save_codebase_index_metadata(&mut conn, metadata.clone())
        .expect("codebase index metadata should save");
    let restored = read_sqlite_data(&mut conn, None, PersistedDataScope::Full)
        .expect("persisted data should load");
    let restored_app_state = restored
        .app_state
        .expect("app state should be present for the full scope");
    assert_eq!(restored_app_state.windows.len(), 1);
    assert_eq!(restored.codebase_indices.len(), 1);
    assert_eq!(restored.codebase_indices[0].path, metadata.path);
}

/// Mirrors `init_db(&PersistenceScope::Tui)` in an isolated tempdir: the TUI
/// database lives in a `tui/` subdirectory, runs the same migrations, and
/// round-trips a write+read using the TUI's `PersistedDataScope`.
#[test]
fn tui_database_in_tui_subdirectory_round_trips_data() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let database_path = tempdir.path().join("tui").join("warp.sqlite");
    std::fs::create_dir_all(
        database_path
            .parent()
            .expect("database path should have a parent"),
    )
    .expect("tui subdirectory should be created");
    let mut conn = setup_database(&database_path).expect("database should initialize");

    let metadata = test_codebase_metadata("/tmp/tui-repo");
    save_codebase_index_metadata(&mut conn, metadata.clone())
        .expect("codebase index metadata should save");

    let restored = read_sqlite_data(&mut conn, None, PersistedDataScope::TuiFrontend)
        .expect("persisted data should load");
    // The TUI data scope skips GUI session restoration and history...
    assert!(restored.app_state.is_none());
    assert!(restored.command_history.is_empty());
    // ...but still round-trips shared data like codebase index metadata.
    assert_eq!(restored.codebase_indices.len(), 1);
    assert_eq!(restored.codebase_indices[0].path, metadata.path);
}

#[test]
fn sqlite_writer_reuses_codebase_index_metadata_events() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let database_path = tempdir.path().join("warp.sqlite");
    let conn = setup_database(&database_path).expect("database should initialize");

    let writer = start_writer(conn, database_path.clone()).expect("writer should start");
    let metadata = test_codebase_metadata("/tmp/writer-repo");
    writer
        .sender
        .send(ModelEvent::UpsertCodebaseIndexMetadata {
            index_metadata: Box::new(metadata.clone()),
        })
        .expect("upsert event should send");
    writer
        .sender
        .send(ModelEvent::Terminate)
        .expect("terminate event should send");
    writer.handle.join().expect("writer should terminate");

    let mut conn = setup_database(&database_path).expect("database should reopen");
    let restored = get_all_codebase_index_metadata(&mut conn).expect("metadata should load");
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].path, metadata.path);

    let writer = start_writer(conn, database_path.clone()).expect("writer should restart");
    writer
        .sender
        .send(ModelEvent::DeleteCodebaseIndexMetadata {
            repo_path: metadata.path,
        })
        .expect("delete event should send");
    writer
        .sender
        .send(ModelEvent::Terminate)
        .expect("terminate event should send");
    writer.handle.join().expect("writer should terminate");

    let mut conn = setup_database(&database_path).expect("database should reopen");
    let restored = get_all_codebase_index_metadata(&mut conn).expect("metadata should load");
    assert!(restored.is_empty());
}
#[test]
fn test_deduplicate_snapshots() {
    let local_notebook = CloudNotebook::new_local(
        CloudNotebookModel {
            title: "Hello".to_string(),
            data: "World".to_string(),
            ai_document_id: None,
            conversation_id: None,
        },
        Owner::mock_current_user(),
        None,
        ClientId::new(),
    );
    let completed_block_1 = BlockCompleted {
        pane_id: vec![1, 2, 3],
        block: Arc::new(SerializedBlock::default()),
        is_local: true,
    };
    let completed_block_2 = BlockCompleted {
        pane_id: vec![4, 5, 6],
        block: Arc::new(SerializedBlock::default()),
        is_local: true,
    };
    let snapshot_1 = AppState {
        active_window_index: Some(1),
        block_lists: Default::default(),
        windows: Default::default(),
        running_mcp_servers: Default::default(),
    };
    let snapshot_2 = AppState {
        active_window_index: Some(2),
        block_lists: Default::default(),
        windows: Default::default(),
        running_mcp_servers: Default::default(),
    };
    let snapshot_3 = AppState {
        active_window_index: Some(3),
        block_lists: Default::default(),
        windows: Default::default(),
        running_mcp_servers: Default::default(),
    };

    let original_events = vec![
        ModelEvent::UpsertNotebook {
            notebook: local_notebook.clone(),
        },
        ModelEvent::Snapshot(snapshot_1.clone()),
        ModelEvent::SaveBlock(completed_block_1.clone()),
        ModelEvent::Snapshot(snapshot_2.clone()),
        ModelEvent::SaveBlock(completed_block_2.clone()),
        ModelEvent::Snapshot(snapshot_3.clone()),
        ModelEvent::UpsertNotebook {
            notebook: local_notebook.clone(),
        },
    ];

    let filtered_events = deduplicate_events(original_events);
    assert_eq!(filtered_events.len(), 5);

    assert!(matches!(
        &filtered_events[0],
        &ModelEvent::UpsertNotebook { .. }
    ));
    // The first snapshot should have been filtered out.
    assert!(matches!(&filtered_events[1], &ModelEvent::SaveBlock(_)));
    // The second snapshot should have been filtered out.
    assert!(matches!(&filtered_events[2], &ModelEvent::SaveBlock(_)));
    // The third snapshot should be preserved.
    match &filtered_events[3] {
        ModelEvent::Snapshot(snapshot) => assert_eq!(snapshot, &snapshot_3),
        other => panic!("Expected ModelEvent::Snapshot, got {other:?}"),
    }
    assert!(matches!(
        &filtered_events[4],
        &ModelEvent::UpsertNotebook { .. }
    ));
}

#[test]
fn test_deduplicate_no_snapshots() {
    let original_events = vec![ModelEvent::SaveBlock(BlockCompleted {
        pane_id: vec![1, 2, 3],
        block: Default::default(),
        is_local: true,
    })];
    let filtered_events = deduplicate_events(original_events);
    assert_eq!(filtered_events.len(), 1);
    assert!(matches!(&filtered_events[0], &ModelEvent::SaveBlock(_)));
}

fn test_terminal_window_snapshot(vertical_tabs_panel_open: bool) -> WindowSnapshot {
    WindowSnapshot {
        tabs: vec![TabSnapshot {
            custom_title: None,
            root: PaneNodeSnapshot::Leaf(LeafSnapshot {
                is_focused: true,
                custom_vertical_tabs_title: None,
                contents: LeafContents::Terminal(TerminalPaneSnapshot {
                    uuid: vec![u8::from(vertical_tabs_panel_open) + 1],
                    cwd: Some("/tmp".to_string()),
                    shell_launch_data: Some(ShellLaunchData::Executable {
                        executable_path: PathBuf::from("/bin/zsh"),
                        shell_type: crate::terminal::shell::ShellType::Zsh,
                    }),
                    is_active: true,
                    is_read_only: false,
                    input_config: None,
                    llm_model_override: None,
                    active_profile_id: None,
                    conversation_ids_to_restore: vec![],
                    active_conversation_id: None,
                    claude_session_id: None,
                    marked_unread: false,
                }),
            }),
            default_directory_color: None,
            selected_color: SelectedTabColor::default(),
            left_panel: None,
            right_panel: None,
            group_id: None,
            pinned: false,
        }],
        active_tab_index: 0,
        bounds: None,
        fullscreen_state: Default::default(),
        quake_mode: false,
        universal_search_width: None,
        warp_ai_width: None,
        voltron_width: None,
        warp_drive_index_width: None,
        left_panel_open: false,
        vertical_tabs_panel_open,
        left_panel_width: None,
        right_panel_width: None,
        agent_management_filters: None,
        tab_groups: vec![],
    }
}

#[test]
fn test_sqlite_round_trips_vertical_tabs_panel_open() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let database_path = tempdir.path().join("warp.sqlite");
    let mut conn = setup_database(&database_path).expect("database should initialize");

    let app_state = AppState {
        windows: vec![
            test_terminal_window_snapshot(false),
            test_terminal_window_snapshot(true),
        ],
        active_window_index: Some(1),
        block_lists: Default::default(),
        running_mcp_servers: Default::default(),
    };

    save_app_state(&mut conn, &app_state).expect("app state should save");

    let restored = read_sqlite_data(&mut conn, None, PersistedDataScope::Full)
        .expect("app state should load")
        .app_state
        .expect("app state should be present for the full scope");

    assert_eq!(restored.active_window_index, Some(1));
    assert_eq!(
        restored
            .windows
            .iter()
            .map(|window| window.vertical_tabs_panel_open)
            .collect::<Vec<_>>(),
        vec![false, true]
    );
}

#[test]
fn test_sqlite_round_trips_custom_vertical_tabs_title() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let database_path = tempdir.path().join("warp.sqlite");
    let mut conn = setup_database(&database_path).expect("database should initialize");

    let app_state = AppState {
        windows: vec![WindowSnapshot {
            tabs: vec![TabSnapshot {
                custom_title: None,
                root: PaneNodeSnapshot::Leaf(LeafSnapshot {
                    is_focused: true,
                    custom_vertical_tabs_title: Some("Production API".to_string()),
                    contents: LeafContents::Terminal(TerminalPaneSnapshot {
                        uuid: vec![42],
                        cwd: Some("/tmp".to_string()),
                        shell_launch_data: Some(ShellLaunchData::Executable {
                            executable_path: PathBuf::from("/bin/zsh"),
                            shell_type: crate::terminal::shell::ShellType::Zsh,
                        }),
                        is_active: true,
                        is_read_only: false,
                        input_config: None,
                        llm_model_override: None,
                        active_profile_id: None,
                        conversation_ids_to_restore: vec![],
                        active_conversation_id: None,
                        claude_session_id: None,
                        marked_unread: false,
                    }),
                }),
                default_directory_color: None,
                selected_color: SelectedTabColor::default(),
                left_panel: None,
                right_panel: None,
                group_id: None,
                pinned: false,
            }],
            active_tab_index: 0,
            bounds: None,
            fullscreen_state: Default::default(),
            quake_mode: false,
            universal_search_width: None,
            warp_ai_width: None,
            voltron_width: None,
            warp_drive_index_width: None,
            left_panel_open: false,
            vertical_tabs_panel_open: false,
            left_panel_width: None,
            right_panel_width: None,
            agent_management_filters: None,
            tab_groups: vec![],
        }],
        active_window_index: Some(0),
        block_lists: Default::default(),
        running_mcp_servers: Default::default(),
    };

    save_app_state(&mut conn, &app_state).expect("app state should save");

    let restored = read_sqlite_data(&mut conn, None, PersistedDataScope::Full)
        .expect("app state should load")
        .app_state
        .expect("app state should be present for the full scope");

    let PaneNodeSnapshot::Leaf(LeafSnapshot {
        custom_vertical_tabs_title,
        ..
    }) = &restored.windows[0].tabs[0].root
    else {
        panic!("Expected terminal pane leaf");
    };
    assert_eq!(
        custom_vertical_tabs_title.as_deref(),
        Some("Production API")
    );
}

#[test]
fn test_sqlite_round_trips_code_pane_with_multiple_tabs() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let database_path = tempdir.path().join("warp.sqlite");
    let mut conn = setup_database(&database_path).expect("database should initialize");

    let app_state = AppState {
        windows: vec![WindowSnapshot {
            tabs: vec![TabSnapshot {
                custom_title: None,
                root: PaneNodeSnapshot::Leaf(LeafSnapshot {
                    is_focused: true,
                    custom_vertical_tabs_title: None,
                    contents: LeafContents::Code(CodePaneSnapShot::Local {
                        tabs: vec![
                            CodePaneTabSnapshot {
                                path: Some(PathBuf::from("/tmp/main.rs")),
                            },
                            CodePaneTabSnapshot {
                                path: Some(PathBuf::from("/tmp/lib.rs")),
                            },
                            CodePaneTabSnapshot { path: None },
                        ],
                        active_tab_index: 1,
                        source: Some(CodeSource::FileTree {
                            location: crate::code::buffer_location::LocalOrRemotePath::Local(
                                PathBuf::from("/tmp/main.rs"),
                            ),
                        }),
                    }),
                }),
                default_directory_color: None,
                selected_color: SelectedTabColor::default(),
                left_panel: None,
                right_panel: None,
                group_id: None,
                pinned: false,
            }],
            active_tab_index: 0,
            bounds: None,
            fullscreen_state: Default::default(),
            quake_mode: false,
            universal_search_width: None,
            warp_ai_width: None,
            voltron_width: None,
            warp_drive_index_width: None,
            left_panel_open: false,
            vertical_tabs_panel_open: false,
            left_panel_width: None,
            right_panel_width: None,
            agent_management_filters: None,
            tab_groups: vec![],
        }],
        active_window_index: Some(0),
        block_lists: Default::default(),
        running_mcp_servers: Default::default(),
    };

    save_app_state(&mut conn, &app_state).expect("app state should save");

    let restored = read_sqlite_data(&mut conn, None, PersistedDataScope::Full)
        .expect("app state should load")
        .app_state
        .expect("app state should be present for the full scope");

    assert_eq!(restored.windows.len(), 1);
    let restored_tab = &restored.windows[0].tabs[0];
    let PaneNodeSnapshot::Leaf(LeafSnapshot {
        contents:
            LeafContents::Code(CodePaneSnapShot::Local {
                tabs,
                active_tab_index,
                source,
            }),
        ..
    }) = &restored_tab.root
    else {
        panic!("Expected code pane leaf");
    };

    assert_eq!(tabs.len(), 3);
    assert_eq!(*active_tab_index, 1);
    assert_eq!(tabs[0].path, Some(PathBuf::from("/tmp/main.rs")));
    assert_eq!(tabs[1].path, Some(PathBuf::from("/tmp/lib.rs")));
    assert_eq!(tabs[2].path, None);
    assert!(matches!(source, Some(CodeSource::FileTree { .. })));
}

/// Verifies that a tab group and its membership round-trip through save/restore.
#[test]
fn test_sqlite_round_trips_tab_groups() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let database_path = tempdir.path().join("warp.sqlite");
    let mut conn = setup_database(&database_path).expect("database should initialize");

    let group_id = TabGroupId::new();
    let tab_in_group = TabSnapshot {
        custom_title: None,
        root: PaneNodeSnapshot::Leaf(LeafSnapshot {
            is_focused: true,
            custom_vertical_tabs_title: None,
            contents: LeafContents::Terminal(TerminalPaneSnapshot {
                uuid: vec![1],
                cwd: Some("/tmp/grouped".to_string()),
                shell_launch_data: Some(ShellLaunchData::Executable {
                    executable_path: PathBuf::from("/bin/zsh"),
                    shell_type: crate::terminal::shell::ShellType::Zsh,
                }),
                is_active: true,
                is_read_only: false,
                input_config: None,
                llm_model_override: None,
                active_profile_id: None,
                conversation_ids_to_restore: vec![],
                active_conversation_id: None,
                claude_session_id: None,
                marked_unread: false,
            }),
        }),
        default_directory_color: None,
        selected_color: SelectedTabColor::default(),
        left_panel: None,
        right_panel: None,
        group_id: Some(group_id),
        pinned: false,
    };
    let tab_outside_group = TabSnapshot {
        custom_title: None,
        root: PaneNodeSnapshot::Leaf(LeafSnapshot {
            is_focused: false,
            custom_vertical_tabs_title: None,
            contents: LeafContents::Terminal(TerminalPaneSnapshot {
                uuid: vec![2],
                cwd: Some("/tmp/ungrouped".to_string()),
                shell_launch_data: Some(ShellLaunchData::Executable {
                    executable_path: PathBuf::from("/bin/zsh"),
                    shell_type: crate::terminal::shell::ShellType::Zsh,
                }),
                is_active: false,
                is_read_only: false,
                input_config: None,
                llm_model_override: None,
                active_profile_id: None,
                conversation_ids_to_restore: vec![],
                active_conversation_id: None,
                claude_session_id: None,
                marked_unread: false,
            }),
        }),
        default_directory_color: None,
        selected_color: SelectedTabColor::default(),
        left_panel: None,
        right_panel: None,
        group_id: None,
        pinned: false,
    };

    let app_state = AppState {
        windows: vec![WindowSnapshot {
            tabs: vec![tab_in_group, tab_outside_group],
            active_tab_index: 0,
            bounds: None,
            fullscreen_state: Default::default(),
            quake_mode: false,
            universal_search_width: None,
            warp_ai_width: None,
            voltron_width: None,
            warp_drive_index_width: None,
            left_panel_open: false,
            vertical_tabs_panel_open: false,
            left_panel_width: None,
            right_panel_width: None,
            agent_management_filters: None,
            tab_groups: vec![TabGroupSnapshot {
                id: group_id,
                name: Some("Backend".to_string()),
                color: SelectedTabColor::Color(AnsiColorIdentifier::Blue),
                collapsed: true,
                pinned: false,
            }],
        }],
        active_window_index: Some(0),
        block_lists: Default::default(),
        running_mcp_servers: Default::default(),
    };

    save_app_state(&mut conn, &app_state).expect("app state should save");

    let restored = read_sqlite_data(&mut conn, None, PersistedDataScope::Full)
        .expect("app state should load")
        .app_state
        .expect("app state should be present for the full scope");

    assert_eq!(restored.windows.len(), 1);
    let restored_window = &restored.windows[0];
    assert_eq!(restored_window.tab_groups.len(), 1);
    let restored_group = &restored_window.tab_groups[0];
    assert_eq!(restored_group.name.as_deref(), Some("Backend"));
    assert_eq!(
        restored_group.color,
        SelectedTabColor::Color(AnsiColorIdentifier::Blue)
    );
    assert!(restored_group.collapsed);

    // The in-memory `TabGroupId` is minted fresh on restore, so we check that
    // the grouped tab points at the restored group, and the ungrouped tab
    // remains ungrouped.
    assert_eq!(restored_window.tabs.len(), 2);
    assert_eq!(restored_window.tabs[0].group_id, Some(restored_group.id));
    assert_eq!(restored_window.tabs[1].group_id, None);
}

/// Verifies that the `pinned` flag on tabs and tab groups round-trips through
/// save/restore so the user's pinned layout survives an app restart.
#[test]
fn test_sqlite_round_trips_pinned_state() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let database_path = tempdir.path().join("warp.sqlite");
    let mut conn = setup_database(&database_path).expect("database should initialize");

    let pinned_group_id = TabGroupId::new();
    let unpinned_group_id = TabGroupId::new();

    let pinned_tab = TabSnapshot {
        custom_title: None,
        root: PaneNodeSnapshot::Leaf(LeafSnapshot {
            is_focused: true,
            custom_vertical_tabs_title: None,
            contents: LeafContents::Terminal(TerminalPaneSnapshot {
                uuid: vec![10],
                cwd: Some("/tmp/pinned".to_string()),
                shell_launch_data: Some(ShellLaunchData::Executable {
                    executable_path: PathBuf::from("/bin/zsh"),
                    shell_type: crate::terminal::shell::ShellType::Zsh,
                }),
                is_active: true,
                is_read_only: false,
                input_config: None,
                llm_model_override: None,
                active_profile_id: None,
                conversation_ids_to_restore: vec![],
                active_conversation_id: None,
                claude_session_id: None,
                marked_unread: false,
            }),
        }),
        default_directory_color: None,
        selected_color: SelectedTabColor::default(),
        left_panel: None,
        right_panel: None,
        group_id: None,
        pinned: true,
    };
    let unpinned_tab = TabSnapshot {
        custom_title: None,
        root: PaneNodeSnapshot::Leaf(LeafSnapshot {
            is_focused: false,
            custom_vertical_tabs_title: None,
            contents: LeafContents::Terminal(TerminalPaneSnapshot {
                uuid: vec![11],
                cwd: Some("/tmp/unpinned".to_string()),
                shell_launch_data: Some(ShellLaunchData::Executable {
                    executable_path: PathBuf::from("/bin/zsh"),
                    shell_type: crate::terminal::shell::ShellType::Zsh,
                }),
                is_active: false,
                is_read_only: false,
                input_config: None,
                llm_model_override: None,
                active_profile_id: None,
                conversation_ids_to_restore: vec![],
                active_conversation_id: None,
                claude_session_id: None,
                marked_unread: false,
            }),
        }),
        default_directory_color: None,
        selected_color: SelectedTabColor::default(),
        left_panel: None,
        right_panel: None,
        group_id: Some(unpinned_group_id),
        pinned: false,
    };
    let tab_in_pinned_group = TabSnapshot {
        custom_title: None,
        root: PaneNodeSnapshot::Leaf(LeafSnapshot {
            is_focused: false,
            custom_vertical_tabs_title: None,
            contents: LeafContents::Terminal(TerminalPaneSnapshot {
                uuid: vec![12],
                cwd: Some("/tmp/pinned-group".to_string()),
                shell_launch_data: Some(ShellLaunchData::Executable {
                    executable_path: PathBuf::from("/bin/zsh"),
                    shell_type: crate::terminal::shell::ShellType::Zsh,
                }),
                is_active: false,
                is_read_only: false,
                input_config: None,
                llm_model_override: None,
                active_profile_id: None,
                conversation_ids_to_restore: vec![],
                active_conversation_id: None,
                claude_session_id: None,
                marked_unread: false,
            }),
        }),
        default_directory_color: None,
        selected_color: SelectedTabColor::default(),
        left_panel: None,
        right_panel: None,
        group_id: Some(pinned_group_id),
        pinned: false,
    };

    let app_state = AppState {
        windows: vec![WindowSnapshot {
            tabs: vec![pinned_tab, tab_in_pinned_group, unpinned_tab],
            active_tab_index: 0,
            bounds: None,
            fullscreen_state: Default::default(),
            quake_mode: false,
            universal_search_width: None,
            warp_ai_width: None,
            voltron_width: None,
            warp_drive_index_width: None,
            left_panel_open: false,
            vertical_tabs_panel_open: false,
            left_panel_width: None,
            right_panel_width: None,
            agent_management_filters: None,
            tab_groups: vec![
                TabGroupSnapshot {
                    id: pinned_group_id,
                    name: Some("Pinned".to_string()),
                    color: SelectedTabColor::default(),
                    collapsed: false,
                    pinned: true,
                },
                TabGroupSnapshot {
                    id: unpinned_group_id,
                    name: Some("Loose".to_string()),
                    color: SelectedTabColor::default(),
                    collapsed: false,
                    pinned: false,
                },
            ],
        }],
        active_window_index: Some(0),
        block_lists: Default::default(),
        running_mcp_servers: Default::default(),
    };

    save_app_state(&mut conn, &app_state).expect("app state should save");

    let restored = read_sqlite_data(&mut conn, None, PersistedDataScope::Full)
        .expect("app state should load")
        .app_state
        .expect("app state should be present for the full scope");

    assert_eq!(restored.windows.len(), 1);
    let restored_window = &restored.windows[0];

    // Tabs come back in insertion order; pinned flag should match what we saved.
    assert_eq!(restored_window.tabs.len(), 3);
    assert!(restored_window.tabs[0].pinned);
    assert!(!restored_window.tabs[1].pinned);
    assert!(!restored_window.tabs[2].pinned);

    // Both groups round-trip with their pinned state preserved. Group ids are
    // minted fresh on restore, so we look them up by name.
    assert_eq!(restored_window.tab_groups.len(), 2);
    let restored_pinned_group = restored_window
        .tab_groups
        .iter()
        .find(|group| group.name.as_deref() == Some("Pinned"))
        .expect("pinned group should restore");
    let restored_loose_group = restored_window
        .tab_groups
        .iter()
        .find(|group| group.name.as_deref() == Some("Loose"))
        .expect("unpinned group should restore");
    assert!(restored_pinned_group.pinned);
    assert!(!restored_loose_group.pinned);
}

fn assert_encode_then_decode_preserves_original_path(original_path: PathBuf) {
    let bytes = encode_path(original_path.clone());
    let decoded_path = decode_path(bytes);
    assert_eq!(original_path, decoded_path);
}

/// Test that a local path can be encoded and decoded. We use this when persisting a local
/// file path for notebooks in sqlite. We need this test because Windows `OsString`s are
/// often arbitrary sequences of 16-bit values, unlike Unix which uses sequences of 8-bit
/// values (bytes). Since `diesel::sql_types::Binary` deals with sequences of bytes (`u8`)
/// we need to perform special casting on `OsString`s on Windows.
#[test]
fn test_path_encode_decode() {
    // Empty path
    assert_encode_then_decode_preserves_original_path(PathBuf::new());

    // Windows-style paths
    assert_encode_then_decode_preserves_original_path(PathBuf::from(r"C:\windows\system32.dll"));
    assert_encode_then_decode_preserves_original_path(PathBuf::from("c:temp"));
    assert_encode_then_decode_preserves_original_path(PathBuf::from(r"\temp"));
    assert_encode_then_decode_preserves_original_path(PathBuf::from(r"\temp\emoji\🙈.txt"));
    assert_encode_then_decode_preserves_original_path(PathBuf::from(r"\temp\ñoñàscii\temp.txt"));
    assert_encode_then_decode_preserves_original_path(PathBuf::from(r"\temp\hindi\हिन्दी"));
    assert_encode_then_decode_preserves_original_path(PathBuf::from(r"\temp\cjk\狗没有耐心"));

    // Unix-style paths
    assert_encode_then_decode_preserves_original_path(PathBuf::from(
        "/home/persistence/example.sql",
    ));
    assert_encode_then_decode_preserves_original_path(PathBuf::from("./database/log.txt"));
    assert_encode_then_decode_preserves_original_path(PathBuf::from("/temp/emoji/🙈.txt"));
    assert_encode_then_decode_preserves_original_path(PathBuf::from("/temp/ñoñàscii/temp.txt"));
    assert_encode_then_decode_preserves_original_path(PathBuf::from("/temp/hindi/हिन्दी"));
    assert_encode_then_decode_preserves_original_path(PathBuf::from("/temp/cjk/狗没有耐心"));
}

#[test]
fn test_deserialize_corrupted_guests() {
    let _ = FeatureFlag::SharedWithMe.override_enabled(true);
    // Use a hardcoded timestamp to ensure this test works on systems with more-than-microsecond
    // precision.
    let permissions_ts_micros = 123456;
    let permissions_ts =
        ServerTimestamp::from_unix_timestamp_micros(permissions_ts_micros).unwrap();

    let db_permissions = ObjectPermissions {
        id: 42,
        object_metadata_id: 10,
        subject_type: "TEAM".to_string(),
        subject_id: Some("7".to_string()),
        subject_uid: "team_uid12345678912345".to_string(),
        permissions_last_updated_at: Some(permissions_ts_micros),
        // This is not a valid set of encoded object guests.
        object_guests: Some(vec![1, 2, 3]),
        anyone_with_link_access_level: None,
        anyone_with_link_source: None,
    };

    // The overall permissions should successfully convert, minus the object guests.
    let cloud_permissions = to_cloud_object_permissions(&db_permissions, None);
    assert_eq!(
        cloud_permissions,
        Some(CloudObjectPermissions {
            owner: Owner::Team {
                team_uid: crate::server::ids::ServerId::from_string_lossy("team_uid12345678912345"),
            },
            permissions_last_updated_ts: Some(permissions_ts),
            anyone_with_link: None,
            guests: vec![],
        })
    );
}

// Regression: GH#10083. The macOS green-tile button could leave a 1px-wide
// window bound in `AppContext::window_bounds`, which previously round-tripped
// through SQLite and restored as an unusable 1px sliver. Bounds below the
// platform minimum window size must be dropped on save.
#[test]
fn test_sqlite_drops_too_small_bounds_on_save() {
    use diesel::prelude::*;

    use crate::persistence::schema::windows;

    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let database_path = tempdir.path().join("warp.sqlite");
    let mut conn = setup_database(&database_path).expect("database should initialize");

    let mut snapshot = test_terminal_window_snapshot(false);
    snapshot.bounds = Some(RectF::new(
        Vector2F::new(0.0, -1410.0),
        Vector2F::new(1.0, 1410.0),
    ));

    let app_state = AppState {
        windows: vec![snapshot],
        active_window_index: Some(0),
        block_lists: Default::default(),
        running_mcp_servers: Default::default(),
    };

    save_app_state(&mut conn, &app_state).expect("app state should save");

    // Query the row directly so the assertion isolates the save guard and is
    // not masked by the read-side guard in `read_sqlite_data`.
    let row: (Option<f32>, Option<f32>, Option<f32>, Option<f32>) = windows::dsl::windows
        .select((
            windows::columns::window_width,
            windows::columns::window_height,
            windows::columns::origin_x,
            windows::columns::origin_y,
        ))
        .first(&mut conn)
        .expect("a windows row should have been inserted");

    assert_eq!(
        row,
        (None, None, None, None),
        "save-path guard must persist NULL bound columns for sub-minimum geometry"
    );
}

// Regression: GH#10083. Users whose warp.sqlite already contains a 1px row
// (because they hit the bug on an earlier build) must still recover to default
// geometry on next launch rather than restoring the sliver.
#[test]
fn test_sqlite_drops_too_small_bounds_on_read() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let database_path = tempdir.path().join("warp.sqlite");
    let mut conn = setup_database(&database_path).expect("database should initialize");

    // Save with no bounds so a row exists, then corrupt it directly to bypass
    // the save-path guard and simulate a pre-existing bad row.
    let app_state = AppState {
        windows: vec![test_terminal_window_snapshot(false)],
        active_window_index: Some(0),
        block_lists: Default::default(),
        running_mcp_servers: Default::default(),
    };
    save_app_state(&mut conn, &app_state).expect("app state should save");

    conn.batch_execute(
        "UPDATE windows \
         SET window_width = 1.0, window_height = 1410.0, \
             origin_x = 0.0, origin_y = -1410.0",
    )
    .expect("corrupting update should succeed");

    let restored = read_sqlite_data(&mut conn, None, PersistedDataScope::Full)
        .expect("app state should load")
        .app_state
        .expect("app state should be present for the full scope");

    assert_eq!(restored.windows.len(), 1);
    assert!(
        restored.windows[0].bounds.is_none(),
        "tiny persisted bounds must be discarded on read so users recover from a corrupt DB"
    );
}

/// Runs `f` with pinned tabs, stars and Mark as Unread all on or all off: a
/// build of the fork with tab marks, or one from before them.
fn with_tab_mark_flags<T>(enabled: bool, f: impl FnOnce() -> T) -> T {
    let _pins = FeatureFlag::PinnedTabs.override_enabled(enabled);
    let _stars = FeatureFlag::StarredTabs.override_enabled(enabled);
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(enabled);
    f()
}

fn marked_terminal(uuid: u8, marked_unread: bool) -> PaneNodeSnapshot {
    PaneNodeSnapshot::Leaf(LeafSnapshot {
        is_focused: false,
        custom_vertical_tabs_title: None,
        contents: LeafContents::Terminal(TerminalPaneSnapshot {
            uuid: vec![uuid],
            cwd: Some("/tmp".to_string()),
            shell_launch_data: Some(ShellLaunchData::Executable {
                executable_path: PathBuf::from("/bin/zsh"),
                shell_type: crate::terminal::shell::ShellType::Zsh,
            }),
            is_active: false,
            is_read_only: false,
            input_config: None,
            llm_model_override: None,
            active_profile_id: None,
            conversation_ids_to_restore: vec![],
            active_conversation_id: None,
            claude_session_id: None,
            marked_unread,
        }),
    })
}

fn marked_split(children: Vec<PaneNodeSnapshot>) -> PaneNodeSnapshot {
    PaneNodeSnapshot::Branch(BranchSnapshot {
        direction: SplitDirection::Horizontal,
        children: children
            .into_iter()
            .map(|child| (PaneFlex(1.), child))
            .collect(),
    })
}

fn marked_tab(root: PaneNodeSnapshot, pinned: bool, group_id: Option<TabGroupId>) -> TabSnapshot {
    TabSnapshot {
        custom_title: None,
        root,
        default_directory_color: None,
        selected_color: SelectedTabColor::default(),
        left_panel: None,
        right_panel: None,
        group_id,
        pinned,
    }
}

fn marked_app_state(
    tabs: Vec<TabSnapshot>,
    tab_groups: Vec<TabGroupSnapshot>,
    active_tab_index: usize,
) -> AppState {
    AppState {
        windows: vec![WindowSnapshot {
            tabs,
            active_tab_index,
            bounds: None,
            fullscreen_state: Default::default(),
            quake_mode: false,
            universal_search_width: None,
            warp_ai_width: None,
            voltron_width: None,
            warp_drive_index_width: None,
            left_panel_open: false,
            vertical_tabs_panel_open: false,
            left_panel_width: None,
            right_panel_width: None,
            agent_management_filters: None,
            tab_groups,
        }],
        active_window_index: Some(0),
        block_lists: Default::default(),
        running_mcp_servers: Default::default(),
    }
}

fn marks_database() -> (tempfile::TempDir, SqliteConnection) {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let conn =
        setup_database(&tempdir.path().join("warp.sqlite")).expect("database should initialize");
    (tempdir, conn)
}

/// Every row of `pane_marks`, by uuid.
fn stored_pane_marks(conn: &mut SqliteConnection) -> Vec<(Vec<u8>, bool, bool)> {
    schema::pane_marks::table
        .select(PaneMark::as_select())
        .order(schema::pane_marks::pane_uuid)
        .load(conn)
        .expect("pane_marks should load")
        .into_iter()
        .map(|mark| (mark.pane_uuid, mark.starred, mark.marked_unread))
        .collect()
}

/// The one window a test saved, read back.
fn restore_window(conn: &mut SqliteConnection) -> WindowSnapshot {
    let mut restored = read_sqlite_data(conn, None, PersistedDataScope::Full)
        .expect("app state should load")
        .app_state
        .expect("app state should be present for the full scope");
    assert_eq!(restored.windows.len(), 1);
    restored.windows.remove(0)
}

/// Each restored tab as its terminal panes' uuids, whether it's starred, and
/// each of those panes' unread marks.
fn restored_tabs(window: &WindowSnapshot) -> Vec<(Vec<u8>, bool, Vec<bool>)> {
    window
        .tabs
        .iter()
        .map(|tab| {
            let terminals = tab.root.terminal_leaves();
            (
                terminals
                    .iter()
                    .flat_map(|terminal| terminal.uuid.clone())
                    .collect(),
                tab.pinned,
                terminals
                    .iter()
                    .map(|terminal| terminal.marked_unread)
                    .collect(),
            )
        })
        .collect()
}

/// Persistence test 1.
#[test]
fn stars_and_unread_marks_round_trip_through_pane_marks() {
    let (_tempdir, mut conn) = marks_database();
    let starred_group = TabGroupId::new();
    let app_state = marked_app_state(
        vec![
            marked_tab(
                marked_split(vec![marked_terminal(1, false), marked_terminal(2, false)]),
                true,
                None,
            ),
            marked_tab(marked_terminal(3, false), false, Some(starred_group)),
            marked_tab(marked_terminal(4, true), false, None),
            marked_tab(marked_terminal(5, false), false, None),
        ],
        vec![TabGroupSnapshot {
            id: starred_group,
            name: Some("Starred group".to_string()),
            color: SelectedTabColor::default(),
            collapsed: false,
            pinned: true,
        }],
        2,
    );

    with_tab_mark_flags(true, || {
        save_app_state(&mut conn, &app_state).expect("app state should save");

        // The starred tab's terminal panes and the unread one; a starred
        // group's members aren't mirrored.
        assert_eq!(
            stored_pane_marks(&mut conn),
            vec![
                (vec![1], true, false),
                (vec![2], true, false),
                (vec![4], false, true),
            ]
        );
        let restored = restore_window(&mut conn);
        assert_eq!(
            restored_tabs(&restored),
            vec![
                (vec![1, 2], true, vec![false, false]),
                (vec![3], false, vec![false]),
                (vec![4], false, vec![true]),
                (vec![5], false, vec![false]),
            ]
        );
        assert_eq!(restored.active_tab_index, 2);
    });
}

/// Persistence test 2: an older build's save drops `tabs.pinned` but never
/// touches `pane_marks`, so the star comes back, repaired to the front.
#[test]
fn a_star_survives_an_older_builds_save_and_returns_to_the_front() {
    let (_tempdir, mut conn) = marks_database();
    let starred = |pinned| marked_tab(marked_terminal(1, false), pinned, None);
    let plain = |uuid| marked_tab(marked_terminal(uuid, false), false, None);

    with_tab_mark_flags(true, || {
        save_app_state(
            &mut conn,
            &marked_app_state(vec![starred(true), plain(2), plain(3)], vec![], 1),
        )
        .expect("app state should save");
    });
    // The older build saves every tab unpinned, after the starred tab was
    // dragged to the end there.
    with_tab_mark_flags(false, || {
        save_app_state(
            &mut conn,
            &marked_app_state(vec![plain(2), plain(3), starred(false)], vec![], 0),
        )
        .expect("app state should save");
        assert_eq!(
            stored_pane_marks(&mut conn),
            vec![(vec![1], true, false)],
            "an older build leaves pane_marks alone"
        );
        assert!(
            restore_window(&mut conn).tabs.iter().all(|tab| !tab.pinned),
            "tabs.pinned no longer has the star"
        );
    });

    with_tab_mark_flags(true, || {
        let restored = restore_window(&mut conn);
        assert_eq!(
            restored_tabs(&restored),
            vec![
                (vec![1], true, vec![false]),
                (vec![2], false, vec![false]),
                (vec![3], false, vec![false]),
            ]
        );
        assert_eq!(
            restored.active_tab_index, 1,
            "the tab that was active is still active"
        );
    });
}

/// Persistence test 3, in the database. Restoring the mark as staged and then
/// committing it is covered with the workspace, in `tab_unread_tests.rs`.
#[test]
fn an_unread_mark_survives_an_older_builds_save() {
    let (_tempdir, mut conn) = marks_database();
    let state = |marked_unread| {
        marked_app_state(
            vec![
                marked_tab(marked_terminal(1, marked_unread), false, None),
                marked_tab(marked_terminal(2, false), false, None),
            ],
            vec![],
            1,
        )
    };

    with_tab_mark_flags(true, || {
        save_app_state(&mut conn, &state(true)).expect("app state should save");
    });
    with_tab_mark_flags(false, || {
        save_app_state(&mut conn, &state(false)).expect("app state should save");
        assert_eq!(stored_pane_marks(&mut conn), vec![(vec![1], false, true)]);
        assert_eq!(
            restored_tabs(&restore_window(&mut conn))[0],
            (vec![1], false, vec![false]),
            "an older build doesn't read the mark"
        );
    });

    with_tab_mark_flags(true, || {
        assert_eq!(
            restored_tabs(&restore_window(&mut conn))[0],
            (vec![1], false, vec![true])
        );
    });
}

/// Persistence test 4.
#[test]
fn a_save_with_both_marks_off_leaves_pane_marks_alone() {
    let (_tempdir, mut conn) = marks_database();
    with_tab_mark_flags(true, || {
        save_app_state(
            &mut conn,
            &marked_app_state(
                vec![
                    marked_tab(marked_terminal(1, false), true, None),
                    marked_tab(marked_terminal(2, true), false, None),
                ],
                vec![],
                0,
            ),
        )
        .expect("app state should save");
    });

    with_tab_mark_flags(false, || {
        save_app_state(
            &mut conn,
            &marked_app_state(
                vec![marked_tab(marked_terminal(9, true), true, None)],
                vec![],
                0,
            ),
        )
        .expect("app state should save");
    });

    // Neither deleted nor added to.
    assert_eq!(
        stored_pane_marks(&mut conn),
        vec![(vec![1], true, false), (vec![2], false, true)]
    );
}

/// Persistence test 5: the mirror write is a savepoint inside the save's
/// transaction. With `pane_marks` gone it fails, the failure is logged, and
/// the rest of the save commits.
#[test]
fn a_failed_mirror_write_leaves_the_save_committed() {
    let (_tempdir, mut conn) = marks_database();
    conn.batch_execute("DROP TABLE pane_marks")
        .expect("pane_marks should drop");
    take_mirror_warnings();

    with_tab_mark_flags(true, || {
        save_app_state(
            &mut conn,
            &marked_app_state(
                vec![
                    marked_tab(marked_terminal(1, false), true, None),
                    marked_tab(marked_terminal(2, true), false, None),
                ],
                vec![],
                1,
            ),
        )
        .expect("the save commits without its marks");

        let warnings = take_mirror_warnings();
        assert_eq!(warnings.len(), 1, "one warning: {warnings:?}");
        assert!(warnings[0].contains("pane_marks"), "{warnings:?}");

        let restored = restore_window(&mut conn);
        assert_eq!(
            restored_tabs(&restored),
            vec![(vec![1], true, vec![false]), (vec![2], false, vec![false])],
            "the tabs round-trip; only the unread mark, which lives in the mirror, is lost"
        );
        assert_eq!(restored.active_tab_index, 1);
    });
}

/// The savepoint also undoes the mirror's own partial work: here the insert
/// fails after the delete has run, and the marks from the last good save are
/// still there while the rest of the new save commits.
#[test]
fn a_mirror_write_that_fails_halfway_keeps_the_last_good_marks() {
    let (_tempdir, mut conn) = marks_database();
    with_tab_mark_flags(true, || {
        save_app_state(
            &mut conn,
            &marked_app_state(
                vec![marked_tab(marked_terminal(1, false), true, None)],
                vec![],
                0,
            ),
        )
        .expect("app state should save");
    });
    conn.batch_execute(
        "CREATE TRIGGER refuse_pane_marks BEFORE INSERT ON pane_marks \
         BEGIN SELECT RAISE(ABORT, 'refused'); END;",
    )
    .expect("the trigger should install");
    take_mirror_warnings();

    with_tab_mark_flags(true, || {
        save_app_state(
            &mut conn,
            &marked_app_state(
                vec![
                    marked_tab(marked_terminal(2, true), false, None),
                    marked_tab(marked_terminal(1, false), false, None),
                ],
                vec![],
                0,
            ),
        )
        .expect("the save commits without its marks");
        assert_eq!(take_mirror_warnings().len(), 1);
        assert_eq!(
            stored_pane_marks(&mut conn),
            vec![(vec![1], true, false)],
            "the mirror's delete was rolled back with its failed insert"
        );

        // The new save's tabs, with the last good star: tab 2 is new.
        let restored = restore_window(&mut conn);
        assert_eq!(
            restored_tabs(&restored),
            vec![(vec![1], true, vec![false]), (vec![2], false, vec![false])]
        );
        assert_eq!(restored.active_tab_index, 1);
    });
}

/// Persistence test 6.
#[test]
fn restore_goes_ahead_without_pane_marks() {
    let (_tempdir, mut conn) = marks_database();
    with_tab_mark_flags(true, || {
        save_app_state(
            &mut conn,
            &marked_app_state(
                vec![
                    marked_tab(marked_terminal(1, false), true, None),
                    marked_tab(marked_terminal(2, true), false, None),
                ],
                vec![],
                0,
            ),
        )
        .expect("app state should save");
        conn.batch_execute("DROP TABLE pane_marks")
            .expect("pane_marks should drop");

        assert_eq!(
            restored_tabs(&restore_window(&mut conn)),
            vec![(vec![1], true, vec![false]), (vec![2], false, vec![false])]
        );
    });
}

/// Persistence test 8. `pane_marks` itself matches its migration
/// (`pane_marks::tests::migration_creates_pane_marks_with_false_defaults`);
/// here, a `tabs` row inserted the way a build from before `pinned` would,
/// naming no value for it, reads back unpinned.
#[test]
fn a_tab_saved_without_a_pinned_value_reads_back_unpinned() {
    let (_tempdir, mut conn) = marks_database();
    save_app_state(
        &mut conn,
        &marked_app_state(
            vec![marked_tab(marked_terminal(1, false), false, None)],
            vec![],
            0,
        ),
    )
    .expect("app state should save");
    conn.batch_execute("INSERT INTO tabs (window_id) SELECT id FROM windows LIMIT 1")
        .expect("an old-shape tab should insert");

    let pinned: bool = schema::tabs::table
        .select(schema::tabs::pinned)
        .order(schema::tabs::id.desc())
        .first(&mut conn)
        .expect("the tab should read back");
    assert!(!pinned);
}
