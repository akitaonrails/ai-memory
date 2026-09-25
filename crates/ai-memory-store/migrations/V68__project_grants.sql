-- Per-project authorization: inert schema (#708, slice 2).
--
-- On a multi-user server a DB-user token *attributes* writes but does not
-- *scope* which projects the user may read or write: any authenticated caller
-- can resolve any `(workspace, project)`. This migration lays the storage for
-- closing that gap without changing any behaviour yet.
--
-- Two additive pieces:
--
--   1. `project_grants` — an explicit `(user, workspace, project) -> level`
--      grant. `read` allows reads in the project; `write` allows reads and
--      writes. Project-level `admin` stays out of scope for v1 (admin remains
--      global/root).
--
--   2. `projects.access_mode` — per-project enforcement switch. `open`
--      (the default, and every existing project on upgrade) means today's
--      behaviour: any authenticated user is admitted. `restricted` means only
--      root, the project creator, and users with a matching grant are admitted.
--
-- Ship inert: nothing sets `restricted` in this slice, the grants table starts
-- empty, and the choke point short-circuits `open` projects to ALLOW, so this
-- is a pure pass-through. A missing/unreadable grants table degrades to `open`
-- (never a lockout — the #678 degrade-don't-fail-closed lesson). Enforcement of
-- `restricted` across the two bypass classes (unscoped/global reads, raw-id
-- entry points) and the root-only management surface land in the follow-up.

CREATE TABLE project_grants (
    workspace_id BLOB    NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id   BLOB    NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    user_id      BLOB    NOT NULL REFERENCES users(id)      ON DELETE CASCADE,
    level        TEXT    NOT NULL CHECK (level IN ('read', 'write')),
    granted_by   BLOB             REFERENCES users(id)      ON DELETE SET NULL,
    granted_at   INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, project_id, user_id)
) WITHOUT ROWID;

-- Every existing project defaults to `open`, so nothing changes on upgrade.
ALTER TABLE projects
    ADD COLUMN access_mode TEXT NOT NULL DEFAULT 'open'
    CHECK (access_mode IN ('open', 'restricted'));
