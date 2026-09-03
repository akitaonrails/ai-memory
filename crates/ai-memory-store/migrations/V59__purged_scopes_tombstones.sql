-- Tombstones for project/workspace scopes removed by `purge_project` /
-- `delete_workspace` (#607, item 2).
--
-- A purge commits the DB deletion first, then removes the on-disk wiki files
-- best-effort. A crash (or a file-removal failure) in that window leaves the
-- project's markdown directory — `_meta.md` included — on disk with no row in
-- `projects`. A later `ai-memory reindex` rebuilds the DB from markdown and
-- calls `ensure_project_with_id` / `ensure_workspace_with_id` from each
-- `_meta.md`, which would recreate the scope and resurrect its pages, silently
-- undoing the purge.
--
-- The tombstone makes the deletion terminal: `reindex_all` checks it and
-- refuses to recreate a scope that was purged. Unlike `purged_sessions`, this
-- table carries NO foreign keys — the workspace/project rows it names are the
-- ones being deleted, so an `ON DELETE CASCADE` reference would either fail or
-- cascade the tombstone away with them.
--
-- Keyed by (workspace_id, project_id). A project purge inserts the real
-- project id. A workspace delete inserts a 16-byte all-zero project id
-- (`zeroblob(16)`) as a whole-workspace tombstone, which never collides with a
-- real UUIDv7 project id. Ids are terminal: a legitimately recreated
-- workspace/project gets a fresh UUID, so a tombstone never blocks new work.
--
-- `purged_at` exists so the ledger can be pruned; a tombstone only has to
-- outlive any surviving on-disk directory that a reindex might otherwise
-- resurrect.
CREATE TABLE purged_scopes (
    workspace_id BLOB NOT NULL,
    project_id   BLOB NOT NULL,
    purged_at    INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, project_id)
);

CREATE INDEX idx_purged_scopes_at ON purged_scopes(purged_at);
