-- A tab's emoji tags, written onto each of its terminal panes and keyed by
-- terminal_panes.uuid, as pane_marks is. `emojis` is a JSON array of up to
-- three emoji, in the order they were added. A table of their own, so a build
-- that doesn't know about tags never erases them: save_app_state only deletes
-- the tables its own build knows.
CREATE TABLE pane_tags (
    pane_uuid BLOB PRIMARY KEY NOT NULL,
    emojis TEXT NOT NULL
);
