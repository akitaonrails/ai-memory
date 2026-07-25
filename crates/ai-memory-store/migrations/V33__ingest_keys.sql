-- Idempotency keys for native hook ingest. A spooled event carries a stable
-- client-generated key. The writer claims it in the same transaction as the
-- observation, then marks it complete after downstream hook effects finish.
-- Incomplete replays resume those effects; completed replays are skipped.
-- Keys are project-scoped and swept opportunistically after the client spool's
-- retry horizon.
CREATE TABLE ingest_keys (
    project_id   BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    key          TEXT NOT NULL,
    seen_at      INTEGER NOT NULL,
    completed_at INTEGER,
    PRIMARY KEY (project_id, key)
) WITHOUT ROWID;

CREATE INDEX idx_ingest_keys_seen_at ON ingest_keys (seen_at);
