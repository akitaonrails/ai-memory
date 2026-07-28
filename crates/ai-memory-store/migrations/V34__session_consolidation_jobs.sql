-- Durable work queue for opt-in SessionEnd LLM consolidation. The hook path
-- only enqueues a job; one bounded server worker claims jobs outside the HTTP
-- request lifetime and retries transient failures. `generation` is the number
-- of observations present when the session was ended, so resumed sessions can
-- be consolidated again without duplicating work for the same observation set.
CREATE TABLE session_consolidation_jobs (
    session_id      BLOB NOT NULL REFERENCES sessions(id)   ON DELETE CASCADE,
    workspace_id    BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id      BLOB NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    generation      INTEGER NOT NULL CHECK (generation > 0),
    state           TEXT NOT NULL CHECK (state IN ('pending', 'running', 'completed', 'failed', 'superseded')),
    requested_at    INTEGER NOT NULL,
    next_attempt_at INTEGER NOT NULL,
    started_at      INTEGER,
    completed_at    INTEGER,
    attempts        INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    claim_id        BLOB,
    last_error      TEXT,
    PRIMARY KEY (session_id, generation)
) WITHOUT ROWID;

CREATE INDEX idx_session_consolidation_jobs_due
    ON session_consolidation_jobs (state, next_attempt_at, requested_at);

CREATE TRIGGER session_consolidation_jobs_session_pairing_ai
BEFORE INSERT ON session_consolidation_jobs
FOR EACH ROW
WHEN NEW.workspace_id IS NOT (SELECT workspace_id FROM sessions WHERE id = NEW.session_id)
  OR NEW.project_id IS NOT (SELECT project_id FROM sessions WHERE id = NEW.session_id)
BEGIN
    SELECT RAISE(ABORT, 'session_consolidation_jobs scope does not match the session scope');
END;
