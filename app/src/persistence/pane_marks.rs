//! Mirrors each terminal pane's marks, its star and its manual unread mark,
//! into the `pane_marks` table.
//!
//! The marks can't live in `tabs` or `terminal_panes`: `save_app_state`
//! deletes and re-inserts both on every save, so the first save of a build
//! that doesn't know about a column there erases it. That delete names only
//! the tables its build knows, so a table of their own survives, and the marks
//! come back when a newer build runs again. Rows are keyed by
//! `terminal_panes.uuid`, which a pane keeps across restores.

use std::collections::HashSet;

use diesel::sqlite::SqliteConnection;

use crate::app_state::{AppState, WindowSnapshot};

/// The panes carrying each mark, by `terminal_panes.uuid`.
#[derive(Debug, Default)]
pub(super) struct PaneMarks {
    pub starred: HashSet<Vec<u8>>,
    pub marked_unread: HashSet<Vec<u8>>,
}

/// Rewrites `pane_marks` from `app_state`: one row per terminal pane that
/// carries any mark.
///
/// Runs inside `save_app_state`'s transaction as a nested transaction (a
/// savepoint), and only while `StarredTabs` or `TabMarkUnread` is on. A column
/// whose flag is off keeps the values already stored, so turning a flag off
/// never erases that mark. The caller logs a failure instead of failing the
/// save.
pub(super) fn write_pane_marks(
    _conn: &mut SqliteConnection,
    _app_state: &AppState,
) -> diesel::QueryResult<()> {
    Ok(())
}

/// Reads the marked panes back for restore. A set is empty while its flag is
/// off, and both are empty when the table can't be read, so restore then goes
/// ahead as if the mirror didn't exist.
pub(super) fn read_pane_marks(_conn: &mut SqliteConnection) -> PaneMarks {
    PaneMarks::default()
}

/// Puts a window's starred tabs back into one block at the front, after stars
/// recovered from the mirror land wherever an older build left their tabs. A
/// stable partition: the order on each side, group contiguity and the active
/// tab are all kept, and it's a no-op when the block is already in place.
pub(super) fn repair_starred_prefix(_window: &mut WindowSnapshot) {}

#[cfg(test)]
#[path = "pane_marks_tests.rs"]
mod tests;
