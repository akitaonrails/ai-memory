-- V40: record which operator a session belongs to.
--
-- `sessions` never carried a human identity, so anything that picks "the open
-- session" for a scope picks somebody else's just as easily as your own. That
-- is how `finalize-session` could end a teammate's LIVE session, synthesise a
-- page from their observations, and mint an auto handoff containing their raw
-- prompts.
--
-- Additive and nullable: existing rows and any caller without an authenticated
-- actor keep `actor_user IS NULL`, which every reader treats as shared — the
-- pre-ownership behaviour a single-operator server depends on.
ALTER TABLE sessions ADD COLUMN actor_user TEXT;

-- Open-session lookups are (workspace, project, agent_kind, ended_at IS NULL)
-- and now additionally discriminate on the owner.
CREATE INDEX idx_sessions_open_owner
    ON sessions (workspace_id, project_id, agent_kind, actor_user)
    WHERE ended_at IS NULL;
