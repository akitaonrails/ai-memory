-- Track the exact observation generation covered by the latest SessionEnd.
-- Wall-clock comparisons can remain true forever after clock skew, repeatedly
-- re-ending and re-consolidating a session with no new observations.
ALTER TABLE sessions
    ADD COLUMN ended_observation_count INTEGER NOT NULL DEFAULT 0
    CHECK (ended_observation_count >= 0);

-- Treat existing ended sessions as the upgrade baseline. Reprocessing every
-- historical session would create an unbounded provider backlog on upgrade.
UPDATE sessions
SET ended_observation_count = (
    SELECT COUNT(*)
    FROM observations
    WHERE observations.session_id = sessions.id
)
WHERE ended_at IS NOT NULL;
