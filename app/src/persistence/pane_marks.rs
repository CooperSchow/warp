//! Mirrors each terminal pane's marks, its star and its manual unread mark,
//! into the `pane_marks` table.
//!
//! The marks can't live in `tabs` or `terminal_panes`: `save_app_state`
//! deletes and re-inserts both on every save, so the first save of a build
//! that doesn't know about a column there erases it. That delete names only
//! the tables its build knows, so a table of their own survives, and the marks
//! come back when a newer build runs again. Rows are keyed by
//! `terminal_panes.uuid`, which a pane keeps across restores.

use std::collections::{BTreeMap, HashMap, HashSet};

use diesel::sqlite::SqliteConnection;
use diesel::{Connection, QueryDsl, RunQueryDsl, SelectableHelper};
use warp_core::features::FeatureFlag;

use super::model::{NewPaneMark, PaneMark};
use super::schema::pane_marks;
use crate::app_state::{AppState, TabSnapshot, WindowSnapshot};
use crate::workspace::tab_group::TabGroupId;
use crate::workspace::view::starred_tabs::starred_tabs_enabled;

/// Rows per insert statement, well inside SQLite's limit on bound values.
const ROWS_PER_INSERT: usize = 256;

/// Which marks this build keeps. Stars need the pinned-tabs engine as well as
/// the star re-skin of it; manual unread marks need `TabMarkUnread`.
#[derive(Clone, Copy, Debug)]
pub(super) struct MarkColumns {
    pub starred: bool,
    pub marked_unread: bool,
}

impl MarkColumns {
    pub(super) fn from_flags() -> Self {
        Self {
            starred: starred_tabs_enabled(),
            marked_unread: FeatureFlag::TabMarkUnread.is_enabled(),
        }
    }

    fn any(self) -> bool {
        self.starred || self.marked_unread
    }
}

/// The panes carrying each mark, by `terminal_panes.uuid`.
#[derive(Debug, Default)]
pub(super) struct PaneMarks {
    pub starred: HashSet<Vec<u8>>,
    pub marked_unread: HashSet<Vec<u8>>,
}

impl PaneMarks {
    /// Puts the marks back on a restored tab: each terminal pane's unread
    /// mark, and a star on an ungrouped tab one of whose terminal panes is
    /// starred, which `tabs.pinned` alone loses to an older build's save.
    pub(super) fn apply_to_tab(&self, tab: &mut TabSnapshot) {
        tab.root.for_each_terminal_leaf_mut(&mut |terminal| {
            terminal.marked_unread |= self.marked_unread.contains(&terminal.uuid);
        });
        if tab.group_id.is_none() && !tab.pinned {
            tab.pinned = tab
                .root
                .terminal_leaf_uuids()
                .into_iter()
                .any(|uuid| self.starred.contains(uuid));
        }
    }
}

/// Mirrors the marks from inside `save_app_state`'s transaction, and leaves the
/// table alone while neither mark is on. The write is a nested transaction,
/// which diesel runs as a savepoint, so a failure rolls back only the mirror:
/// it's logged, and the rest of the save commits.
pub(super) fn save_pane_marks(conn: &mut SqliteConnection, app_state: &AppState) {
    let columns = MarkColumns::from_flags();
    if !columns.any() {
        return;
    }
    if let Err(err) = conn.transaction::<(), diesel::result::Error, _>(|conn| {
        write_pane_marks(conn, app_state, columns)
    }) {
        warn_mirror_failure(format!(
            "Couldn't save tab marks to pane_marks; the rest of the save goes ahead: {err}"
        ));
    }
}

/// Rewrites `pane_marks` from `app_state`: one row per terminal pane that
/// carries any mark. A pane is starred when its tab is starred and ungrouped.
/// A column whose mark is off keeps the value already stored for each pane,
/// so turning a flag off never erases that mark.
pub(super) fn write_pane_marks(
    conn: &mut SqliteConnection,
    app_state: &AppState,
    columns: MarkColumns,
) -> diesel::QueryResult<()> {
    let stored: HashMap<Vec<u8>, (bool, bool)> = if columns.starred && columns.marked_unread {
        HashMap::new()
    } else {
        pane_marks::table
            .select(PaneMark::as_select())
            .load::<PaneMark>(conn)?
            .into_iter()
            .map(|mark| (mark.pane_uuid, (mark.starred, mark.marked_unread)))
            .collect()
    };

    // Keyed by uuid, so a pane listed twice still gets one row.
    let mut marks: BTreeMap<&[u8], (bool, bool)> = BTreeMap::new();
    for tab in app_state.windows.iter().flat_map(|window| &window.tabs) {
        // Only an ungrouped tab's star is mirrored. A starred group keeps its
        // star through ordinary saves, in tab_groups.pinned, but not through
        // a rollback to a build without stars, whose first save writes that
        // column false and leaves nothing here to bring the star back.
        let tab_starred = tab.pinned && tab.group_id.is_none();
        for terminal in tab.root.terminal_leaves() {
            let uuid = terminal.uuid.as_slice();
            let (stored_starred, stored_unread) = stored.get(uuid).copied().unwrap_or_default();
            let (starred, marked_unread) = marks.entry(uuid).or_default();
            *starred |= if columns.starred {
                tab_starred
            } else {
                stored_starred
            };
            *marked_unread |= if columns.marked_unread {
                terminal.marked_unread
            } else {
                stored_unread
            };
        }
    }

    diesel::delete(pane_marks::table).execute(conn)?;
    let mut rows = marks
        .into_iter()
        .filter(|(_, (starred, marked_unread))| *starred || *marked_unread)
        .map(|(uuid, (starred, marked_unread))| NewPaneMark {
            pane_uuid: uuid.to_vec(),
            starred,
            marked_unread,
        })
        .peekable();
    while rows.peek().is_some() {
        let chunk: Vec<NewPaneMark> = rows.by_ref().take(ROWS_PER_INSERT).collect();
        diesel::insert_into(pane_marks::table)
            .values(chunk)
            .execute(conn)?;
    }
    Ok(())
}

/// Reads the marked panes back for restore. A set stays empty while its mark
/// is off, and both are empty when the table can't be read, so restore then
/// goes ahead as if the mirror didn't exist.
pub(super) fn read_pane_marks(conn: &mut SqliteConnection, columns: MarkColumns) -> PaneMarks {
    let mut marks = PaneMarks::default();
    if !columns.any() {
        return marks;
    }
    let rows = match pane_marks::table
        .select(PaneMark::as_select())
        .load::<PaneMark>(conn)
    {
        Ok(rows) => rows,
        Err(err) => {
            log::warn!("Couldn't read tab marks from pane_marks; restoring without them: {err}");
            return marks;
        }
    };
    for row in rows {
        if columns.starred && row.starred {
            marks.starred.insert(row.pane_uuid.clone());
        }
        if columns.marked_unread && row.marked_unread {
            marks.marked_unread.insert(row.pane_uuid);
        }
    }
    marks
}

/// Puts a window's starred tabs back into one block at the front, after stars
/// recovered from the mirror land wherever an older build left their tabs. A
/// stable partition: the order on each side, group contiguity and the active
/// tab are all kept, and it's a no-op when the block is already in place.
pub(super) fn repair_starred_prefix(window: &mut WindowSnapshot) {
    let pinned_groups: HashSet<TabGroupId> = window
        .tab_groups
        .iter()
        .filter(|group| group.pinned)
        .map(|group| group.id)
        .collect();
    // A grouped tab sits wherever its group does, so every member of a group
    // lands on the same side and the group stays together.
    let in_block: Vec<bool> = window
        .tabs
        .iter()
        .map(|tab| match tab.group_id {
            Some(group_id) => pinned_groups.contains(&group_id),
            None => tab.pinned,
        })
        .collect();
    if in_block.windows(2).all(|pair| pair[0] || !pair[1]) {
        return;
    }

    let (block, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut window.tabs)
        .into_iter()
        .enumerate()
        .partition(|(index, _)| in_block[*index]);
    let reordered: Vec<(usize, TabSnapshot)> = block.into_iter().chain(rest).collect();
    if let Some(active) = reordered
        .iter()
        .position(|(index, _)| *index == window.active_tab_index)
    {
        window.active_tab_index = active;
    }
    window.tabs = reordered.into_iter().map(|(_, tab)| tab).collect();
}

/// Logs a failed mirror write, which the save itself survives.
fn warn_mirror_failure(message: String) {
    log::warn!("{message}");
    #[cfg(test)]
    MIRROR_WARNINGS.with(|warnings| warnings.borrow_mut().push(message));
}

#[cfg(test)]
thread_local! {
    /// What `warn_mirror_failure` logged on this thread. Unit tests share one
    /// process-wide logger, so they read these instead.
    static MIRROR_WARNINGS: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The mirror warnings logged on this thread since the last call.
#[cfg(test)]
pub(super) fn take_mirror_warnings() -> Vec<String> {
    MIRROR_WARNINGS.with(|warnings| warnings.take())
}

#[cfg(test)]
#[path = "pane_marks_tests.rs"]
mod tests;
