-- Persisted file fingerprints for the wiki watcher's periodic reconcile pass,
-- allowing server restarts to skip re-reading unchanged markdown files on the
-- first reconcile tick while still catching offline edits.
CREATE TABLE IF NOT EXISTS wiki_file_fingerprints (
    workspace_id BLOB NOT NULL,
    project_id   BLOB NOT NULL,
    path         TEXT NOT NULL,
    file_len     INTEGER NOT NULL,
    mtime_secs   INTEGER NOT NULL,
    mtime_nanos  INTEGER NOT NULL,
    inode        INTEGER NOT NULL DEFAULT 0,
    ctime_secs   INTEGER NOT NULL DEFAULT 0,
    ctime_nanos  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (workspace_id, project_id, path)
) WITHOUT ROWID;
