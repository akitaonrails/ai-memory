-- V70: give a project an identity that is not its folder name (#708).
--
-- A project is keyed by `(workspace_id, name)`, and the name is the basename
-- of whatever directory the agent happened to run in. That fails in three ways
-- that matter on a shared server:
--
--   * Two repositories called `api`, in different organisations, collapse into
--     one project and read each other's memory.
--   * Renaming a folder orphans its memory under the old name.
--   * `repo_path` is an absolute path, so it cannot be the thing two people on
--     two machines agree on.
--
-- With per-project authorization (V68/V69) the first is an access-control
-- hole, not untidiness: grants are held against a project, so an unrelated
-- `api/` checkout resolving to the same row reaches the same grants.
--
-- Additive on purpose. `identity` defaults to empty, and every existing query
-- keeps working untouched: nothing reads this column until the resolution path
-- is taught to, and the partial index below ignores rows that never are.
--
-- Existing rows are deliberately NOT backfilled. Identities are case-folded,
-- so `API` and `api` are one repository to the resolver, while upstream's
-- `UNIQUE (workspace_id, name)` is case-sensitive and may already hold both.
-- Backfilling `lower(name)` failed the whole upgrade on such an install, and
-- keeping one of the pair would have the migration silently decide which
-- project — and so which grants — a future `api/` checkout lands in. That is
-- the resolution path's decision to make on first sighting, not this file's.

ALTER TABLE projects ADD COLUMN identity TEXT NOT NULL DEFAULT '';

-- Which rung of the resolution chain produced it: `explicit`, `git_remote`,
-- `manifest`, or `folder_name`. Kept because the rungs are ranked — a later
-- sighting may only ever upgrade an identity to a more trustworthy source,
-- never downgrade it, and that comparison needs to know where this one came
-- from. An empty string means no rung has claimed the row yet.
ALTER TABLE projects ADD COLUMN identity_source TEXT NOT NULL DEFAULT '';

-- Partial, so rows with no identity do not collide with each other on the
-- empty string. Two projects may share a name across workspaces, and within a
-- workspace an identity is the thing that must be unique — that is the point
-- of having it.
CREATE UNIQUE INDEX idx_projects_identity
    ON projects(workspace_id, identity)
 WHERE identity <> '';
