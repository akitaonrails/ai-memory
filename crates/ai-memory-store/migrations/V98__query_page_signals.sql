-- Query-scoped implicit ranking signals (#1941, slice 2): one row per
-- (workspace, project, normalized query, page version) recording that a page
-- returned for that query was actually used — a memory_read_page of the hit
-- or a `helpful` feedback signal followed the query in the same agent
-- session. Later queries lexically similar to the stored query give the
-- page's current version a bounded, similarity-capped authority boost
-- (`[retrieval] query_signal_weight`, OFF by default).
--
-- Keying on the query (not just the page) is the point: a page that keeps
-- being read for one specific question is not thereby promoted as an answer
-- to every other question.
--
-- `query_hash` is the first 16 bytes of SHA-256 over the sanitized,
-- normalized query text (`query_text`, bounded at 256 chars by the writer).
-- Storing the text beside the hash is what lets ranking compare *similar*
-- queries, not just identical ones; the text is user-supplied, so the server
-- secret-scrubs it exactly like observation bodies before it reaches here.
--
-- `signal_count` increments only when the writing session differs from
-- `last_session` (one association per session/query/page), saturating in
-- ranking at QUERY_SIGNAL_COUNT_CAP — the bound on the read-more → boost-more
-- feedback loop. `last_session` is a bounded client session key.
--
-- Like `page_feedback`, rows attach to a page *version*: a rewrite supersedes
-- the version, the new version's id has no association, and the boost clears
-- until the new version earns its own. `ON DELETE CASCADE` follows both the
-- project and the page version, so purge, project move/merge and graveyard
-- cleanup never leave orphaned queries behind.

CREATE TABLE query_page_signals (
    workspace_id  BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id    BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    query_hash    BLOB NOT NULL,
    page_id       BLOB NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    query_text    TEXT NOT NULL,
    signal_count  INTEGER NOT NULL DEFAULT 1 CHECK (signal_count >= 1),
    last_session  TEXT,
    last_seen     INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, project_id, query_hash, page_id)
) WITHOUT ROWID;

-- Reverse lookup for ranking: fetch a candidate page's associations.
CREATE INDEX idx_query_page_signals_page
    ON query_page_signals(workspace_id, project_id, page_id);
