-- Watermarks for idempotent startup backfills so subsequent restarts do not
-- rescan hundreds of thousands of already-indexed rows.
CREATE TABLE IF NOT EXISTS backfill_watermarks (
    name      TEXT PRIMARY KEY,
    max_rowid INTEGER NOT NULL
) WITHOUT ROWID;
