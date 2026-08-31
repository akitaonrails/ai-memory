-- Tombstones for sessions removed by `purge_session` (#387).
--
-- A purge deletes the session's rows, but the events that produced them may
-- still be sitting in a client hook spool, undelivered. When that spool
-- eventually drains, `begin_session` recreates the session row and the
-- observations land again — the purge is silently undone, and an application
-- that promised to forget the conversation has not.
--
-- The tombstone makes the deletion terminal: ingest checks it and refuses to
-- recreate a session that was purged. Keyed by session id, with the scope kept
-- so a purge in one project cannot suppress a same-id session elsewhere (ids
-- are UUIDv7 and collisions are not expected, but the check costs nothing and
-- the alternative is a cross-scope denial of ingest).
--
-- `purged_at` exists so the ledger can be pruned. A tombstone only has to
-- outlive the spool that might replay the session, and `MAX_AGE_MS` in the
-- drain caps that at 7 days.
CREATE TABLE purged_sessions (
    session_id   BLOB PRIMARY KEY NOT NULL,
    workspace_id BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id   BLOB NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    purged_at    INTEGER NOT NULL
);

CREATE INDEX idx_purged_sessions_at ON purged_sessions(purged_at);
