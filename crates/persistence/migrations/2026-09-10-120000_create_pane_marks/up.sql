-- A terminal pane's marks (a star, a manual unread mark), keyed by
-- terminal_panes.uuid, which a pane keeps across restores. They get a table of
-- their own because save_app_state deletes and re-inserts tabs and
-- terminal_panes on every save, so a build that doesn't know about a column
-- there erases it; that delete never touches a table the build doesn't know.
CREATE TABLE pane_marks (
    pane_uuid BLOB PRIMARY KEY NOT NULL,
    starred BOOLEAN NOT NULL DEFAULT FALSE,
    marked_unread BOOLEAN NOT NULL DEFAULT FALSE
);
