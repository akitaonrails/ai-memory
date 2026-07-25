-- Optional `ai-memory run` workstreams. Direct harness launches continue to
-- use sessions/observations/handoffs and never touch these tables.

CREATE TABLE workstreams (
    id                    BLOB PRIMARY KEY NOT NULL,
    workspace_id          BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id            BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    repo_fingerprint      TEXT NOT NULL,
    worktree_fingerprint  TEXT NOT NULL,
    name                  TEXT NOT NULL,
    cwd                   TEXT NOT NULL,
    created_at            INTEGER NOT NULL,
    selected_at           INTEGER NOT NULL,
    updated_at            INTEGER NOT NULL,
    UNIQUE (workspace_id, project_id, repo_fingerprint, worktree_fingerprint, name)
);

CREATE TRIGGER workstreams_ws_proj_pairing_ai
BEFORE INSERT ON workstreams
FOR EACH ROW
WHEN NEW.workspace_id IS NOT (SELECT workspace_id FROM projects WHERE id = NEW.project_id)
BEGIN
    SELECT RAISE(ABORT, 'workstreams.workspace_id does not match the project''s workspace');
END;

CREATE INDEX idx_workstreams_selected
    ON workstreams(workspace_id, project_id, repo_fingerprint, worktree_fingerprint, selected_at DESC);

CREATE TABLE workstream_native_sessions (
    workstream_id       BLOB NOT NULL REFERENCES workstreams(id) ON DELETE CASCADE,
    agent_kind          TEXT NOT NULL,
    native_session_id   TEXT NOT NULL,
    is_current          INTEGER NOT NULL DEFAULT 1 CHECK (is_current IN (0, 1)),
    source_cursor       TEXT,
    delivery_cursor     INTEGER NOT NULL DEFAULT 0,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    PRIMARY KEY (workstream_id, agent_kind, native_session_id)
);

CREATE UNIQUE INDEX idx_workstream_native_current
    ON workstream_native_sessions(workstream_id, agent_kind)
    WHERE is_current = 1;

CREATE TABLE managed_runs (
    id                  BLOB PRIMARY KEY NOT NULL,
    workstream_id       BLOB NOT NULL REFERENCES workstreams(id) ON DELETE CASCADE,
    agent_kind          TEXT NOT NULL,
    lease_owner         TEXT NOT NULL,
    native_session_id   TEXT,
    state               TEXT NOT NULL CHECK (state IN ('active', 'finished', 'expired')),
    sync_after          INTEGER NOT NULL DEFAULT 0,
    sync_through        INTEGER NOT NULL DEFAULT 0,
    context_delivered   INTEGER NOT NULL DEFAULT 0 CHECK (context_delivered IN (0, 1)),
    lease_expires_at    INTEGER NOT NULL,
    started_at          INTEGER NOT NULL,
    ended_at            INTEGER,
    exit_code           INTEGER
);

CREATE UNIQUE INDEX idx_managed_runs_one_active
    ON managed_runs(workstream_id)
    WHERE state = 'active';

CREATE INDEX idx_managed_runs_lease
    ON managed_runs(state, lease_expires_at);

CREATE TABLE workstream_events (
    workstream_id       BLOB NOT NULL REFERENCES workstreams(id) ON DELETE CASCADE,
    sequence            INTEGER NOT NULL,
    event_id            TEXT NOT NULL,
    agent_kind          TEXT NOT NULL,
    native_session_id   TEXT NOT NULL,
    source_record_id    TEXT,
    kind                TEXT NOT NULL CHECK (kind IN (
                            'message', 'tool_call', 'tool_result',
                            'compaction', 'checkpoint', 'annotation'
                        )),
    role                TEXT,
    content             TEXT NOT NULL,
    occurred_at         TEXT,
    metadata_json       TEXT NOT NULL DEFAULT '{}',
    segment_path        TEXT,
    created_at          INTEGER NOT NULL,
    PRIMARY KEY (workstream_id, sequence),
    UNIQUE (workstream_id, event_id)
);

CREATE INDEX idx_workstream_events_recent
    ON workstream_events(workstream_id, sequence DESC);

CREATE VIRTUAL TABLE workstream_events_fts USING fts5(
    content,
    role UNINDEXED,
    agent_kind UNINDEXED,
    content='workstream_events',
    content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 2'
);

CREATE TRIGGER workstream_events_fts_ai AFTER INSERT ON workstream_events BEGIN
    INSERT INTO workstream_events_fts(rowid, content, role, agent_kind)
    VALUES (new.rowid, new.content, new.role, new.agent_kind);
END;

CREATE TRIGGER workstream_events_fts_ad AFTER DELETE ON workstream_events BEGIN
    INSERT INTO workstream_events_fts(workstream_events_fts, rowid, content, role, agent_kind)
    VALUES ('delete', old.rowid, old.content, old.role, old.agent_kind);
END;

CREATE TRIGGER workstream_events_fts_au AFTER UPDATE ON workstream_events BEGIN
    INSERT INTO workstream_events_fts(workstream_events_fts, rowid, content, role, agent_kind)
    VALUES ('delete', old.rowid, old.content, old.role, old.agent_kind);
    INSERT INTO workstream_events_fts(rowid, content, role, agent_kind)
    VALUES (new.rowid, new.content, new.role, new.agent_kind);
END;
