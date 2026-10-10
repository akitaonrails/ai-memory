ALTER TABLE ingest_keys ADD COLUMN session_id BLOB REFERENCES sessions(id) ON DELETE CASCADE;
ALTER TABLE ingest_keys ADD COLUMN agent_kind TEXT;
ALTER TABLE ingest_keys ADD COLUMN observation_kind TEXT;
ALTER TABLE ingest_keys ADD COLUMN event_identity TEXT;
ALTER TABLE ingest_keys ADD COLUMN recovery_retained INTEGER NOT NULL DEFAULT 0 CHECK (recovery_retained IN (0, 1));

CREATE INDEX idx_ingest_keys_session ON ingest_keys (session_id);
