-- Durable session-processing (chunking) jobs. A job snapshots the identities
-- and body hashes of one session's observations (never the bodies), then a
-- worker checkpoints map/reduce nodes, reserves provider call attempts before
-- every send, and publishes through the existing page/proposal paths. All of
-- it is derived, private state: reindex or purge may drop it without touching
-- published pages.
-- The service creates new jobs only while `[chunking].enabled = true`.
-- Existing jobs can still be paused, queried or cleaned up with the flag off.
CREATE TABLE processing_jobs (
    id                      BLOB PRIMARY KEY NOT NULL,
    session_id              BLOB NOT NULL REFERENCES sessions(id)   ON DELETE CASCADE,
    workspace_id            BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id              BLOB NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    kind                    TEXT NOT NULL
        CHECK (kind IN ('consolidate', 'consolidate_multi', 'auto_improve')),
    state                   TEXT NOT NULL
        CHECK (state IN ('queued', 'running', 'reducing', 'publishing', 'completed',
                         'paused', 'budget_exceeded', 'failed', 'invalidated')),
    -- Phase to restore when a flag-off pause is lifted.
    paused_from             TEXT
        CHECK (paused_from IS NULL OR paused_from IN ('queued', 'running', 'reducing', 'publishing')),
    -- Qualified identity storage key; '' is the shared/unattributed bucket.
    owner_key               TEXT NOT NULL DEFAULT '',
    owner_json              TEXT,
    actor_json              TEXT NOT NULL,
    author_id               BLOB,
    viewer_id               BLOB,
    options_json            TEXT NOT NULL,
    context_fingerprint     TEXT NOT NULL,
    manifest_fingerprint    TEXT NOT NULL,
    manifest_count          INTEGER NOT NULL CHECK (manifest_count > 0),
    manifest_body_bytes     INTEGER NOT NULL CHECK (manifest_body_bytes >= 0),
    -- sha256 over scope, session, kind, owner bucket, options, context and
    -- manifest fingerprints. Excludes max_calls so a budget increase resumes.
    equivalence_key         TEXT NOT NULL,
    chunks_total            INTEGER NOT NULL DEFAULT 0 CHECK (chunks_total >= 0),
    chunks_done             INTEGER NOT NULL DEFAULT 0 CHECK (chunks_done >= 0),
    calls_used              INTEGER NOT NULL DEFAULT 0 CHECK (calls_used >= 0),
    max_calls               INTEGER NOT NULL CHECK (max_calls > 0),
    -- Cumulative provider-call timing in ms (queue wait / execution) of calls
    -- that returned; a call lost to a crash is never measured.
    wait_ms                 INTEGER NOT NULL DEFAULT 0 CHECK (wait_ms >= 0),
    exec_ms                 INTEGER NOT NULL DEFAULT 0 CHECK (exec_ms >= 0),
    claim_id                BLOB,
    fence                   INTEGER NOT NULL DEFAULT 0 CHECK (fence >= 0),
    claim_count             INTEGER NOT NULL DEFAULT 0 CHECK (claim_count >= 0),
    lease_expires_at        INTEGER,
    next_run_at             INTEGER NOT NULL,
    publication_identity    TEXT,
    publication_plan_json   TEXT,
    publication_started_at  INTEGER,
    result_json             TEXT,
    last_error              TEXT,
    created_at              INTEGER NOT NULL,
    updated_at              INTEGER NOT NULL,
    completed_at            INTEGER
);

-- At most one live-or-completed job per equivalent request: racing starts
-- coalesce instead of duplicating provider calls.
CREATE UNIQUE INDEX idx_processing_jobs_equivalence
    ON processing_jobs (equivalence_key)
    WHERE state NOT IN ('failed', 'invalidated');
CREATE INDEX idx_processing_jobs_due
    ON processing_jobs (state, next_run_at, created_at);
CREATE INDEX idx_processing_jobs_session ON processing_jobs (session_id);
CREATE INDEX idx_processing_jobs_scope ON processing_jobs (workspace_id, project_id);

-- Snapshot manifest: identity, order and hash of each observation body.
CREATE TABLE processing_job_observations (
    job_id          BLOB NOT NULL REFERENCES processing_jobs(id) ON DELETE CASCADE,
    ordinal         INTEGER NOT NULL CHECK (ordinal >= 0),
    observation_id  BLOB NOT NULL,
    body_sha256     BLOB NOT NULL CHECK (length(body_sha256) = 32),
    body_bytes      INTEGER NOT NULL CHECK (body_bytes >= 0),
    PRIMARY KEY (job_id, ordinal)
) WITHOUT ROWID;

-- Sanitized, bounded map/reduce checkpoints.
CREATE TABLE processing_job_nodes (
    job_id             BLOB NOT NULL REFERENCES processing_jobs(id) ON DELETE CASCADE,
    node_key           TEXT NOT NULL,
    level              INTEGER NOT NULL CHECK (level >= 0),
    state              TEXT NOT NULL CHECK (state IN ('completed', 'recoverable')),
    input_fingerprint  TEXT NOT NULL,
    json               TEXT NOT NULL,
    updated_at         INTEGER NOT NULL,
    PRIMARY KEY (job_id, node_key)
) WITHOUT ROWID;

-- Confirmed publication/staging effects, kept until the job expires so a
-- restart after the effect commit but before the checkpoint reconciles.
CREATE TABLE processing_job_effects (
    job_id       BLOB NOT NULL REFERENCES processing_jobs(id) ON DELETE CASCADE,
    effect_key   TEXT NOT NULL,
    json         TEXT NOT NULL,
    recorded_at  INTEGER NOT NULL,
    PRIMARY KEY (job_id, effect_key)
) WITHOUT ROWID;

-- Page, evidence, proposal and staging writes made through a fenced writer
-- handle are guarded by CONNECTION-LOCAL temp triggers the writer installs on
-- its own connection (`processing_jobs::install_write_fence`). They are not
-- part of this schema on purpose: a persistent fence row would be shared by
-- every writer connection to the file, so a second Store (or process) could
-- clear or replace another writer's armed claim.

-- Mutation boundary for the snapshot. Any change to a stored observation an
-- active job depends on (body, title, kind, capture time, or scope) or its
-- removal invalidates that job inside the mutating transaction, dropping its
-- intermediates and claim. A worker holding a cached response can then no
-- longer publish: the fence trigger sees a job that is no longer running.
CREATE TRIGGER observations_processing_invalidate_au
AFTER UPDATE OF body, title, kind, created_at, session_id, workspace_id, project_id
ON observations
WHEN EXISTS (
    SELECT 1 FROM processing_jobs
     WHERE session_id IN (OLD.session_id, NEW.session_id)
       AND state NOT IN ('completed', 'failed', 'invalidated'))
BEGIN
    DELETE FROM processing_job_nodes WHERE job_id IN (
        SELECT id FROM processing_jobs
         WHERE session_id IN (OLD.session_id, NEW.session_id)
           AND state NOT IN ('completed', 'failed', 'invalidated'));
    DELETE FROM processing_job_observations WHERE job_id IN (
        SELECT id FROM processing_jobs
         WHERE session_id IN (OLD.session_id, NEW.session_id)
           AND state NOT IN ('completed', 'failed', 'invalidated'));
    UPDATE processing_jobs
       SET state = 'invalidated', paused_from = NULL, claim_id = NULL,
           lease_expires_at = NULL,
           completed_at = CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER),
           updated_at = CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER),
           last_error = 'source observations changed or were removed'
     WHERE session_id IN (OLD.session_id, NEW.session_id)
       AND state NOT IN ('completed', 'failed', 'invalidated');
END;

CREATE TRIGGER observations_processing_invalidate_ad
AFTER DELETE ON observations
WHEN EXISTS (
    SELECT 1 FROM processing_jobs
     WHERE session_id = OLD.session_id
       AND state NOT IN ('completed', 'failed', 'invalidated'))
BEGIN
    DELETE FROM processing_job_nodes WHERE job_id IN (
        SELECT id FROM processing_jobs
         WHERE session_id = OLD.session_id
           AND state NOT IN ('completed', 'failed', 'invalidated'));
    DELETE FROM processing_job_observations WHERE job_id IN (
        SELECT id FROM processing_jobs
         WHERE session_id = OLD.session_id
           AND state NOT IN ('completed', 'failed', 'invalidated'));
    UPDATE processing_jobs
       SET state = 'invalidated', paused_from = NULL, claim_id = NULL,
           lease_expires_at = NULL,
           completed_at = CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER),
           updated_at = CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER),
           last_error = 'source observations changed or were removed'
     WHERE session_id = OLD.session_id
       AND state NOT IN ('completed', 'failed', 'invalidated');
END;
