use diesel::sql_types::{Bool, Nullable, Text};
use diesel::sqlite::SqliteConnection;
use diesel::{Connection, QueryDsl, QueryableByName, RunQueryDsl, SelectableHelper};
use diesel_migrations::MigrationHarness;
use persistence::model::{NewPaneMark, PaneMark};
use persistence::schema::pane_marks;

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
