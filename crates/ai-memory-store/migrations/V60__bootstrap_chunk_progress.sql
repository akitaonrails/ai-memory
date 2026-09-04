-- Durable per-chunk bootstrap progress so `bootstrap --resume` recovers the
-- pages earlier chunks already produced instead of re-paying for every LLM
-- call after a crash or a chunk that failed past its retry budget (#621).
-- Keyed by a fingerprint of the run's pruned source set + chunk budget, so a
-- resume only reuses progress when the inputs are byte-identical (a re-run
-- after new commits re-chunks and starts fresh). No foreign key: the
-- fingerprint is content-derived, not a row.
CREATE TABLE bootstrap_chunk_progress (
    fingerprint  TEXT    NOT NULL,
    chunk_index  INTEGER NOT NULL,
    pages_json   TEXT    NOT NULL,
    rationale    TEXT    NOT NULL,
    created_at   INTEGER NOT NULL,
    PRIMARY KEY (fingerprint, chunk_index)
);
CREATE INDEX idx_bootstrap_chunk_progress_at ON bootstrap_chunk_progress(created_at);
