-- Workspace-wide grants: one grant for every project of a workspace.
--
-- A `project_grants` row (V68) admits one user to one project. A team that
-- works on every project of a workspace then needs one grant per project, and
-- a project created later admits nobody but its creator until an operator
-- grants it again.
--
-- `workspace_grants` holds `(user, workspace) -> level`. The choke point
-- (`project_authz::resolve`) and the read filter (`readable_repository_sql`)
-- treat it as a grant at that level on every project whose
-- `projects.workspace_id` is this workspace, present or future. A user who
-- holds both kinds of grant on a project gets the higher level.
--
-- Additive and inert on upgrade: the table starts empty, so every existing
-- decision is unchanged until an operator grants a workspace. A project that
-- moves to another workspace leaves the old workspace's grants behind and
-- comes under the new one's, because the join is on the project's current
-- workspace and not on a copied id.

CREATE TABLE workspace_grants (
    workspace_id BLOB    NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    user_id      BLOB    NOT NULL REFERENCES users(id)      ON DELETE CASCADE,
    level        TEXT    NOT NULL CHECK (level IN ('read', 'write')),
    granted_by   BLOB             REFERENCES users(id)      ON DELETE SET NULL,
    granted_at   INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, user_id)
) WITHOUT ROWID;

-- The read filter looks grants up by user, which the primary key does not lead with.
CREATE INDEX idx_workspace_grants_user ON workspace_grants(user_id);
