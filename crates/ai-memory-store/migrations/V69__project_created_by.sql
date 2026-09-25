-- Per-project authorization: the project creator (#708, slice 3).
--
-- A `restricted` project admits root, its creator, and matching grants. V68
-- left the creator to the caller (`ProjectPrincipal::is_creator`); this column
-- lets the choke point derive it from the project row instead, so it cannot
-- disagree with who actually created the project.
--
-- Additive and nullable: every project that exists on upgrade has no recorded
-- creator, and so admits only root and grants once restricted. A project
-- created by the root token or on an install with no database users has none
-- either — nobody but root could have been its creator. Deleting the user
-- clears the column rather than the project.

ALTER TABLE projects
    ADD COLUMN created_by BLOB REFERENCES users(id) ON DELETE SET NULL;
