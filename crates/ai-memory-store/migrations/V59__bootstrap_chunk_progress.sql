-- Durable per-chunk progress so a crashed or hard-failed bootstrap does not
-- discard the completed chunks' LLM work (#621). A multi-chunk run holds every
-- page in memory until the single post-loop apply_batch, so losing the process
-- loses every chunk paid for so far.
--
-- `fingerprint` is a hash of the pruned source set plus the chunk budget, not
-- the project alone: chunks are addressed by position, and sources are re-read
-- from git/docs at run time, so a stored chunk_index only means the same thing
-- while those inputs are unchanged. A resume whose fingerprint differs is a
-- different chunk plan and must start over rather than skip the wrong work.
CREATE TABLE bootstrap_chunk_progress (
    fingerprint  TEXT NOT NULL,
    workspace_id BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id   BLOB NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    chunk_index  INTEGER NOT NULL CHECK (chunk_index > 0),
    pages_json   TEXT NOT NULL,
    rationale    TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    PRIMARY KEY (fingerprint, chunk_index)
) WITHOUT ROWID;

-- Resume loads one fingerprint's chunks in order; the stale prune sweeps by
-- age across fingerprints.
CREATE INDEX idx_bootstrap_chunk_progress_sweep
    ON bootstrap_chunk_progress (created_at);
