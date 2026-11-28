-- Durable per-block progress for opt-in map-reduce consolidation. Keyed by
-- the full typed scope plus a content-derived fingerprint: a foreign
-- workspace/project/session can neither read nor clobber another scope's
-- stages. `created_at` is bookkeeping only; reuse decisions compare
-- fingerprints, never timestamps. A successful publish prunes the session's
-- rows; a crash after publish but before prune is reconciled by the anchor
-- page's exact `consolidation_marker` (prompt versions, model, mode,
-- resolved instructions and length-prefixed observation fields).
CREATE TABLE IF NOT EXISTS consolidation_chunk_progress (
    workspace_id      TEXT    NOT NULL,
    project_id        TEXT    NOT NULL,
    session_id        TEXT    NOT NULL,
    chunk_fingerprint TEXT    NOT NULL,
    extraction_json   TEXT    NOT NULL,
    created_at        INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, project_id, session_id, chunk_fingerprint)
);
CREATE INDEX IF NOT EXISTS idx_consolidation_chunk_progress_at
    ON consolidation_chunk_progress(created_at);
