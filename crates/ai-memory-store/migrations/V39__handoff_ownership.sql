-- V39: give handoffs an owner so a shared server stops delivering one
-- operator's baton to another.
--
-- Before this, `handoffs` carried no human identity at all: the open-handoff
-- lookup filtered on (workspace_id, project_id, state) only, and the session
-- start hook claims whatever it finds. On a single-operator server that is the
-- intended 1:1 behaviour; with several operators in one project it means the
-- next session to start — whoever it belongs to — consumes the pending handoff
-- and the author never receives it.
--
-- Additive on purpose:
--   * Both columns are nullable. Every pre-existing row keeps `owner_user
--     IS NULL`, which the reader treats as "project-wide / shared" — exactly
--     the old delivery semantics, so existing deployments see no change.
--   * A single-operator server whose requests carry no actor keeps writing
--     NULL owners and keeps behaving as before.
--   * `ALTER TABLE ... ADD COLUMN` with a NULL default rewrites no rows, so
--     this is O(1) even on a large table.
ALTER TABLE handoffs ADD COLUMN owner_user TEXT;

-- Who actually consumed it. `accepted_by` records the accepting AGENT kind
-- ('claude-code', …), which cannot answer "which teammate took my baton".
ALTER TABLE handoffs ADD COLUMN accepted_by_user TEXT;

-- The open-handoff lookup is always (workspace, project, state='open') and now
-- additionally discriminates on owner. Partial index: only open rows are ever
-- queried this way, and accepted/expired rows are the overwhelming majority
-- over time.
CREATE INDEX idx_handoffs_open_owner
    ON handoffs (workspace_id, project_id, owner_user, created_at DESC)
    WHERE state = 'open';
