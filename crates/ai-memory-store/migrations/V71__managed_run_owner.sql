-- Managed-run recovery may replace a stale harness-exported run id with the
-- one live launch in the current repository. Keep that recovery in the same
-- operator bucket as the launch so one user cannot bind another user's run.
-- NULL retains the historical single-operator/shared-server behavior.

ALTER TABLE managed_runs ADD COLUMN owner_user TEXT;

CREATE INDEX idx_managed_runs_adoption
    ON managed_runs(state, agent_kind, owner_user, context_delivered,
                    native_session_linked_at, lease_expires_at);
