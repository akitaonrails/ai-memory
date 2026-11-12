-- Per-session retrieval history (#2038, Workstream 2). When
-- `[retrieval] record_history` is on, each single-project `memory_query` call
-- records what it actually returned — the query, resolved scope, active
-- streams, effective filter settings, duration, result count, any soft notice,
-- and every returned page's version reference, position, calibrated relevance
-- score and ranking explanation — so an operator can inspect retrieval accuracy
-- and debug noisy results from the read-only `/web` dashboard. Recording is
-- opt-in and additive: an install that never turns it on writes nothing and
-- these tables stay empty.
--
-- Rows are keyed by the same `(workspace_id, project_id, session_id)` coordinate
-- as every other session-scoped row, so the dashboard's reads pass the existing
-- per-project authorization and session-owner filters unchanged. `session_id` is
-- not a foreign key to `sessions`: a query can arrive before the session's
-- `begin_session` row exists (or from a client that never opens one), and that
-- history is still worth keeping. Project deletion cascades these rows away;
-- `purge_session` deletes them explicitly.
--
-- Evidence is stored, never recomputed: a later rerun cannot reproduce a past
-- query's ranking, so the original scores and explanations are captured here.
-- Only a version *reference* (path + version label) is kept per hit, not the
-- page body. The query text is scrubbed and length-bounded before it reaches
-- this table (the same scrubber the query-signal path uses).

CREATE TABLE retrieval_history (
    id            BLOB PRIMARY KEY NOT NULL,
    workspace_id  BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id    BLOB NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    session_id    BLOB NOT NULL,
    -- Microseconds since the Unix epoch, matching `sessions.started_at`.
    created_at    INTEGER NOT NULL,
    -- Scrubbed, length-bounded query expression the caller sent.
    query         TEXT NOT NULL,
    -- Resolved scope shape. v1 records the single-project path only: 'project'.
    scope_kind    TEXT NOT NULL,
    -- Comma-separated active retrieval streams (e.g. 'fts,entity,vector,rerank').
    source        TEXT NOT NULL,
    -- Effective result limit after #2038 precedence resolution.
    result_limit  INTEGER,
    -- Effective calibrated relevance floor after #2038 precedence (NULL = none).
    min_relevance REAL,
    -- Compact JSON of the active pre-ranking filters (kind/tier/path_prefix/tag
    -- /include_expired/include_superseded); NULL when none were active.
    filters       TEXT,
    -- Wall time the retrieval took, in microseconds.
    duration_us   INTEGER NOT NULL,
    -- Number of memory hits returned to the caller (0 is valid, not an error).
    result_count  INTEGER NOT NULL,
    -- A soft notice attached to the call (e.g. a relevance filter that could not
    -- be applied), not a hard failure; NULL when the call was clean.
    notice        TEXT
);

CREATE INDEX idx_retrieval_history_session
    ON retrieval_history(workspace_id, project_id, session_id, created_at DESC);

CREATE TABLE retrieval_history_hits (
    history_id    BLOB NOT NULL REFERENCES retrieval_history(id) ON DELETE CASCADE,
    -- 0-based rank in the returned order.
    position      INTEGER NOT NULL,
    -- Project-relative page path (a version reference, not the body).
    page_path     TEXT NOT NULL,
    -- The page version id the hit resolved to, when known.
    page_version  TEXT,
    -- Calibrated reranker relevance in 0.0..=1.0; NULL when the hit was not
    -- scored (no reranker, the fused tail, or a pinned page).
    relevance     REAL,
    -- Compact JSON ranking explanation (the query's `score_details`), when the
    -- call ran with `explain`; NULL otherwise.
    explanation   TEXT,
    PRIMARY KEY (history_id, position)
);
