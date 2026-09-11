use diesel::sql_types::{Bool, Nullable, Text};
use diesel::sqlite::SqliteConnection;
use diesel::{Connection, QueryDsl, QueryableByName, RunQueryDsl, SelectableHelper};
use diesel_migrations::MigrationHarness;
use persistence::model::{NewPaneMark, PaneMark};
use persistence::schema::pane_marks;
use warp_core::features::FeatureFlag;
use warpui::{App, EntityId};

use super::{read_pane_marks, repair_starred_prefix, write_pane_marks, MarkColumns, PaneMarks};
use crate::ai::agent_management::AgentNotificationsModel;
use crate::app_state::{
    AppState, BranchSnapshot, LeafContents, LeafSnapshot, PaneFlex, PaneNodeSnapshot,
    SplitDirection, TabGroupSnapshot, TabSnapshot, TerminalPaneSnapshot, WindowSnapshot,
};
use crate::tab::SelectedTabColor;
use crate::workspace::tab_group::TabGroupId;

/// A fresh database with every migration applied.
fn migrated_connection() -> SqliteConnection {
    let mut conn = SqliteConnection::establish(":memory:").expect("in-memory database should open");
    conn.run_pending_migrations(persistence::MIGRATIONS)
        .expect("migrations should apply");
    conn
}

/// One row of `PRAGMA table_info`.
#[derive(Debug, PartialEq, QueryableByName)]
struct Column {
    #[diesel(sql_type = Text)]
    name: String,
    #[diesel(sql_type = Text)]
    column_type: String,
    #[diesel(sql_type = Bool)]
    not_null: bool,
    #[diesel(sql_type = Nullable<Text>)]
    default_value: Option<String>,
    #[diesel(sql_type = Bool)]
    primary_key: bool,
}

#[test]
fn migration_creates_pane_marks_with_false_defaults() {
    let mut conn = migrated_connection();
    let columns: Vec<Column> = diesel::sql_query(
        "SELECT name, type AS column_type, \"notnull\" AS not_null, \
         dflt_value AS default_value, pk AS primary_key \
         FROM pragma_table_info('pane_marks') ORDER BY cid",
    )
    .load(&mut conn)
    .expect("pane_marks should exist");

    let column = |name: &str, column_type: &str, default_value: Option<&str>| Column {
        name: name.to_owned(),
        column_type: column_type.to_owned(),
        not_null: true,
        default_value: default_value.map(str::to_owned),
        primary_key: name == "pane_uuid",
    };
    assert_eq!(
        columns,
        vec![
            column("pane_uuid", "BLOB", None),
            column("starred", "BOOLEAN", Some("FALSE")),
            column("marked_unread", "BOOLEAN", Some("FALSE")),
        ]
    );
}

#[test]
fn schema_round_trips_marks_and_unnamed_columns_default_to_false() {
    let mut conn = migrated_connection();
    diesel::insert_into(pane_marks::table)
        .values(NewPaneMark {
            pane_uuid: vec![1],
            starred: true,
            marked_unread: false,
        })
        .execute(&mut conn)
        .expect("a full row should insert");
    diesel::insert_into(pane_marks::table)
        .values(NewPaneMark {
            pane_uuid: vec![2],
            starred: true,
            marked_unread: true,
        })
        .execute(&mut conn)
        .expect("a full row should insert");
    // Names only the key, the way a build that predates a column would insert.
    diesel::sql_query("INSERT INTO pane_marks (pane_uuid) VALUES (x'03')")
        .execute(&mut conn)
        .expect("a key-only row should insert");

    let rows: Vec<(Vec<u8>, bool, bool)> = pane_marks::table
        .select(PaneMark::as_select())
        .order(pane_marks::pane_uuid)
        .load(&mut conn)
        .expect("pane_marks should load")
        .into_iter()
        .map(|mark| (mark.pane_uuid, mark.starred, mark.marked_unread))
        .collect();
    assert_eq!(
        rows,
        vec![
            (vec![1], true, false),
            (vec![2], true, true),
            (vec![3], false, false),
        ]
    );
}

const BOTH_MARKS: MarkColumns = MarkColumns {
    starred: true,
    marked_unread: true,
};

fn terminal(uuid: u8, marked_unread: bool) -> PaneNodeSnapshot {
    PaneNodeSnapshot::Leaf(LeafSnapshot {
        is_focused: false,
        custom_vertical_tabs_title: None,
        contents: LeafContents::Terminal(TerminalPaneSnapshot {
            uuid: vec![uuid],
            cwd: None,
            shell_launch_data: None,
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

fn split(children: Vec<PaneNodeSnapshot>) -> PaneNodeSnapshot {
    PaneNodeSnapshot::Branch(BranchSnapshot {
        direction: SplitDirection::Horizontal,
        children: children
            .into_iter()
            .map(|child| (PaneFlex(1.), child))
            .collect(),
    })
}

fn tab(root: PaneNodeSnapshot, pinned: bool, group_id: Option<TabGroupId>) -> TabSnapshot {
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

fn group(id: TabGroupId, pinned: bool) -> TabGroupSnapshot {
    TabGroupSnapshot {
        id,
        name: None,
        color: SelectedTabColor::default(),
        collapsed: false,
        pinned,
    }
}

fn window(
    tabs: Vec<TabSnapshot>,
    tab_groups: Vec<TabGroupSnapshot>,
    active_tab_index: usize,
) -> WindowSnapshot {
    WindowSnapshot {
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
    }
}

fn app_state(windows: Vec<WindowSnapshot>) -> AppState {
    AppState {
        windows,
        active_window_index: Some(0),
        block_lists: Default::default(),
        running_mcp_servers: Default::default(),
    }
}

/// Every row of `pane_marks`, by uuid.
fn stored_marks(conn: &mut SqliteConnection) -> Vec<(Vec<u8>, bool, bool)> {
    pane_marks::table
        .select(PaneMark::as_select())
        .order(pane_marks::pane_uuid)
        .load(conn)
        .expect("pane_marks should load")
        .into_iter()
        .map(|mark| (mark.pane_uuid, mark.starred, mark.marked_unread))
        .collect()
}

#[test]
fn write_stores_one_row_per_marked_terminal_pane() {
    let mut conn = migrated_connection();
    let grouped = TabGroupId::new();
    let state = app_state(vec![window(
        vec![
            // A starred tab stars each of its terminal panes.
            tab(
                split(vec![terminal(1, false), terminal(2, false)]),
                true,
                None,
            ),
            tab(terminal(3, true), false, None),
            // A tab can't be starred and grouped; if one says so, the group wins.
            tab(terminal(4, false), true, Some(grouped)),
            tab(terminal(5, false), false, None),
        ],
        vec![group(grouped, true)],
        0,
    )]);

    write_pane_marks(&mut conn, &state, BOTH_MARKS).expect("marks should save");

    assert_eq!(
        stored_marks(&mut conn),
        vec![
            (vec![1], true, false),
            (vec![2], true, false),
            (vec![3], false, true),
        ]
    );
}

/// Persistence test 7.
#[test]
fn duplicate_uuids_never_trip_the_primary_key() {
    let mut conn = migrated_connection();
    let state = app_state(vec![
        window(
            vec![tab(
                split(vec![terminal(7, false), terminal(7, false)]),
                true,
                None,
            )],
            vec![],
            0,
        ),
        window(vec![tab(terminal(7, true), false, None)], vec![], 0),
    ]);

    write_pane_marks(&mut conn, &state, BOTH_MARKS).expect("marks should save");

    assert_eq!(stored_marks(&mut conn), vec![(vec![7], true, true)]);
}

/// Persistence test 9.
#[test]
fn a_pane_both_starred_and_unread_gets_one_row_with_both() {
    let mut conn = migrated_connection();
    let state = app_state(vec![window(
        vec![tab(terminal(9, true), true, None)],
        vec![],
        0,
    )]);

    write_pane_marks(&mut conn, &state, BOTH_MARKS).expect("marks should save");

    assert_eq!(stored_marks(&mut conn), vec![(vec![9], true, true)]);
}

#[test]
fn a_mark_whose_flag_is_off_keeps_the_value_stored_for_each_pane() {
    let seed = |conn: &mut SqliteConnection| {
        diesel::delete(pane_marks::table)
            .execute(conn)
            .expect("pane_marks should clear");
        for (uuid, starred, marked_unread) in [(1, true, true), (2, false, true), (3, true, false)]
        {
            diesel::insert_into(pane_marks::table)
                .values(NewPaneMark {
                    pane_uuid: vec![uuid],
                    starred,
                    marked_unread,
                })
                .execute(conn)
                .expect("a seed row should insert");
        }
    };
    // Pane 3 has closed since; panes 1 and 2 are unstarred and read now.
    let state = app_state(vec![window(
        vec![
            tab(terminal(1, false), false, None),
            tab(terminal(2, false), false, None),
        ],
        vec![],
        0,
    )]);
    let mut conn = migrated_connection();

    seed(&mut conn);
    let stars_only = MarkColumns {
        starred: true,
        marked_unread: false,
    };
    write_pane_marks(&mut conn, &state, stars_only).expect("marks should save");
    assert_eq!(
        stored_marks(&mut conn),
        vec![(vec![1], false, true), (vec![2], false, true)],
        "stars come from the snapshot and unread marks carry over"
    );

    seed(&mut conn);
    let unread_only = MarkColumns {
        starred: false,
        marked_unread: true,
    };
    write_pane_marks(&mut conn, &state, unread_only).expect("marks should save");
    assert_eq!(
        stored_marks(&mut conn),
        vec![(vec![1], true, false)],
        "unread marks come from the snapshot and stars carry over"
    );
}

#[test]
fn read_gives_each_mark_only_while_its_flag_is_on() {
    let mut conn = migrated_connection();
    write_pane_marks(
        &mut conn,
        &app_state(vec![window(
            vec![
                tab(terminal(1, false), true, None),
                tab(terminal(2, true), false, None),
            ],
            vec![],
            0,
        )]),
        BOTH_MARKS,
    )
    .expect("marks should save");

    let sets = |columns: MarkColumns, conn: &mut SqliteConnection| {
        let marks = read_pane_marks(conn, columns);
        let mut starred: Vec<_> = marks.starred.into_iter().collect();
        let mut marked_unread: Vec<_> = marks.marked_unread.into_iter().collect();
        starred.sort();
        marked_unread.sort();
        (starred, marked_unread)
    };
    let none: Vec<Vec<u8>> = vec![];
    assert_eq!(sets(BOTH_MARKS, &mut conn), (vec![vec![1]], vec![vec![2]]));
    assert_eq!(
        sets(
            MarkColumns {
                starred: false,
                marked_unread: true
            },
            &mut conn
        ),
        (none.clone(), vec![vec![2]])
    );
    assert_eq!(
        sets(
            MarkColumns {
                starred: true,
                marked_unread: false
            },
            &mut conn
        ),
        (vec![vec![1]], none)
    );
}

/// Persistence test 6, at the mirror: a missing table reads as no marks.
#[test]
fn read_gives_no_marks_when_the_table_is_missing() {
    let mut conn = migrated_connection();
    diesel::sql_query("DROP TABLE pane_marks")
        .execute(&mut conn)
        .expect("pane_marks should drop");

    let marks = read_pane_marks(&mut conn, BOTH_MARKS);

    assert!(marks.starred.is_empty());
    assert!(marks.marked_unread.is_empty());
}

#[test]
fn restored_marks_light_their_panes_and_star_only_ungrouped_tabs() {
    let marks = PaneMarks {
        starred: [vec![1]].into_iter().collect(),
        marked_unread: [vec![2]].into_iter().collect(),
    };

    let mut ungrouped = tab(
        split(vec![terminal(1, false), terminal(2, false)]),
        false,
        None,
    );
    marks.apply_to_tab(&mut ungrouped);
    assert!(ungrouped.pinned);
    let unread: Vec<(Vec<u8>, bool)> = ungrouped
        .root
        .terminal_leaves()
        .into_iter()
        .map(|terminal| (terminal.uuid.clone(), terminal.marked_unread))
        .collect();
    assert_eq!(unread, vec![(vec![1], false), (vec![2], true)]);

    // A grouped tab keeps its place in the group rather than taking a star.
    let mut grouped = tab(terminal(1, false), false, Some(TabGroupId::new()));
    marks.apply_to_tab(&mut grouped);
    assert!(!grouped.pinned);
}

/// Persistence test 10: a snapshot taken where no `AgentNotificationsModel`
/// was ever registered records no unread mark, and doesn't panic.
#[test]
fn snapshot_without_the_notifications_model_records_no_unread_mark() {
    let _unread = FeatureFlag::TabMarkUnread.override_enabled(true);
    App::test((), |app| async move {
        app.read(|ctx| {
            assert!(!AgentNotificationsModel::unread_for_snapshot(
                EntityId::new(),
                ctx
            ));
        });
    });
}

/// One run of tabs in a generated window: a lone tab, or a group of them.
#[derive(Clone, Copy, Debug)]
enum Run {
    Tab { pinned: bool },
    Group { size: usize, pinned: bool },
}

/// Every way to lay out `tab_count` tabs as lone tabs and contiguous groups,
/// each starred or not.
fn arrangements(tab_count: usize) -> Vec<Vec<Run>> {
    if tab_count == 0 {
        return vec![vec![]];
    }
    let mut all = vec![];
    for first_size in 1..=tab_count {
        let mut firsts = vec![
            Run::Group {
                size: first_size,
                pinned: false,
            },
            Run::Group {
                size: first_size,
                pinned: true,
            },
        ];
        if first_size == 1 {
            firsts.push(Run::Tab { pinned: false });
            firsts.push(Run::Tab { pinned: true });
        }
        for rest in arrangements(tab_count - first_size) {
            for first in &firsts {
                let mut runs = vec![*first];
                runs.extend(rest.iter().copied());
                all.push(runs);
            }
        }
    }
    all
}

/// A window laid out as `runs`, each tab titled with its starting position.
fn window_of(runs: &[Run]) -> WindowSnapshot {
    let titled = |index: usize, pinned: bool, group_id: Option<TabGroupId>| TabSnapshot {
        custom_title: Some(index.to_string()),
        ..tab(
            PaneNodeSnapshot::Leaf(LeafSnapshot {
                is_focused: false,
                custom_vertical_tabs_title: None,
                contents: LeafContents::NetworkLog,
            }),
            pinned,
            group_id,
        )
    };
    let mut tabs = vec![];
    let mut groups = vec![];
    for run in runs {
        match *run {
            Run::Tab { pinned } => tabs.push(titled(tabs.len(), pinned, None)),
            Run::Group { size, pinned } => {
                let id = TabGroupId::new();
                groups.push(group(id, pinned));
                for _ in 0..size {
                    tabs.push(titled(tabs.len(), false, Some(id)));
                }
            }
        }
    }
    window(tabs, groups, 0)
}

/// Whether the tab sits in the starred block: its group's star if it's
/// grouped, its own otherwise.
fn in_starred_block(window: &WindowSnapshot, tab: &TabSnapshot) -> bool {
    match tab.group_id {
        Some(group_id) => window
            .tab_groups
            .iter()
            .any(|group| group.id == group_id && group.pinned),
        None => tab.pinned,
    }
}

fn titles(tabs: &[TabSnapshot]) -> Vec<String> {
    tabs.iter()
        .map(|tab| {
            tab.custom_title
                .clone()
                .expect("every generated tab is titled")
        })
        .collect()
}

/// Sweeps every layout of up to seven tabs, lone or grouped and starred or
/// not, with every active tab, through `repair_starred_prefix`.
#[test]
fn repair_starred_prefix_sweeps_every_arrangement_of_up_to_seven_tabs() {
    let mut cases = 0;
    for tab_count in 0..=7 {
        for runs in arrangements(tab_count) {
            let layout = window_of(&runs);
            for active in 0..tab_count.max(1) {
                let mut before = layout.clone();
                before.active_tab_index = active;
                let mut after = before.clone();
                repair_starred_prefix(&mut after);
                cases += 1;

                let block: Vec<bool> = after
                    .tabs
                    .iter()
                    .map(|tab| in_starred_block(&after, tab))
                    .collect();
                assert!(
                    block.windows(2).all(|pair| pair[0] || !pair[1]),
                    "starred tabs form one block at the front: {runs:?}"
                );

                let side = |starred: bool, window: &WindowSnapshot| -> Vec<String> {
                    titles(&window.tabs)
                        .into_iter()
                        .zip(&window.tabs)
                        .filter(|(_, tab)| in_starred_block(window, tab) == starred)
                        .map(|(title, _)| title)
                        .collect()
                };
                for starred in [true, false] {
                    assert_eq!(
                        side(starred, &after),
                        side(starred, &before),
                        "order within each side is kept: {runs:?}"
                    );
                }

                for group in &after.tab_groups {
                    let members: Vec<usize> = after
                        .tabs
                        .iter()
                        .enumerate()
                        .filter(|(_, tab)| tab.group_id == Some(group.id))
                        .map(|(index, _)| index)
                        .collect();
                    assert!(
                        members.windows(2).all(|pair| pair[1] == pair[0] + 1),
                        "each group stays contiguous: {runs:?}"
                    );
                }

                if tab_count > 0 {
                    assert_eq!(
                        after.tabs[after.active_tab_index].custom_title,
                        before.tabs[active].custom_title,
                        "the active tab stays active: {runs:?}"
                    );
                }

                let already_a_block = before
                    .tabs
                    .iter()
                    .map(|tab| in_starred_block(&before, tab))
                    .collect::<Vec<_>>()
                    .windows(2)
                    .all(|pair| pair[0] || !pair[1]);
                if already_a_block {
                    assert_eq!(after, before, "a block already in place is left alone");
                }

                let mut sorted_before = titles(&before.tabs);
                let mut sorted_after = titles(&after.tabs);
                sorted_before.sort();
                sorted_after.sort();
                assert_eq!(sorted_after, sorted_before, "no tab is lost or added");
            }
        }
    }
    // Layouts times active tabs: 1 + 4 + 18×2 + 82×3 + 374×4 + 1706×5 + 7782×6
    // + 35498×7.
    assert_eq!(cases, 305_491);
}

#[test]
fn repair_follows_the_group_for_a_grouped_tab_marked_pinned() {
    let loose = TabGroupId::new();
    let mut repaired = window(
        vec![
            tab(terminal(1, false), false, None),
            // Inconsistent on disk: pinned, but in an unstarred group.
            tab(terminal(2, false), true, Some(loose)),
            tab(terminal(3, false), false, Some(loose)),
            tab(terminal(4, false), true, None),
        ],
        vec![group(loose, false)],
        2,
    );

    repair_starred_prefix(&mut repaired);

    let order: Vec<Vec<u8>> = repaired
        .tabs
        .iter()
        .flat_map(|tab| tab.root.terminal_leaf_uuids())
        .map(<[u8]>::to_vec)
        .collect();
    assert_eq!(order, vec![vec![4], vec![1], vec![2], vec![3]]);
    assert_eq!(repaired.active_tab_index, 3);
}
