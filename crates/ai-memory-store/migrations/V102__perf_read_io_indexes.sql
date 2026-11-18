-- Covering index for handoffs by scope, creation time, and source session,
-- keeping created_at range bounds in `latest_interrupted_session_candidate`
-- and `recent_session_activity` without reading the handoffs table.
CREATE INDEX IF NOT EXISTS idx_handoffs_from_session
    ON handoffs(workspace_id, project_id, created_at DESC, from_session_id)
    WHERE from_session_id IS NOT NULL;

-- Partial index for open sessions per project regardless of agent_kind,
-- avoiding scanning closed session rows in `list_projects_with_stats`.
CREATE INDEX IF NOT EXISTS idx_sessions_open_started
    ON sessions(workspace_id, project_id, started_at DESC)
    WHERE ended_at IS NULL;
