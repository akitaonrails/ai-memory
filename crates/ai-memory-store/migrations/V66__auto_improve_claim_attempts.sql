-- Make a scheduler claim releasable (#833).
--
-- `auto_improve_scheduler_claims` was written once, by `INSERT OR IGNORE`, and
-- never deleted or expired. The candidate query in `auto_improve_candidate_sessions`
-- excludes a session that has a claim OR a run, so a review that failed — a hung
-- provider call, or a proposal the reviewer could not stage — left a claim with no
-- run and removed that session from every future tick. The state was silent: the
-- tick reported `errors=1` once and clean runs forever after.
--
-- A claim now carries its own outcome:
--
--   attempts = 0                          in flight (just claimed)
--   0 < attempts < max                    failed and retryable; the next tick picks it up
--   attempts >= max                       parked, with `last_error` for the operator
--
-- `attempts` defaults to 0 so existing rows keep meaning "in flight". That is the
-- conservative reading for a row written by an older version: those claims are the
-- leaked ones this fixes, and a release path that resurrected them silently would
-- re-review sessions an operator may have already handled by hand. `ai-memory
-- auto-improve --session-id` remains the way to drive one of them.

ALTER TABLE auto_improve_scheduler_claims ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE auto_improve_scheduler_claims ADD COLUMN last_error TEXT;
ALTER TABLE auto_improve_scheduler_claims ADD COLUMN last_failed_at INTEGER;

-- The candidate query filters claims by `attempts`, and the parked-claim listing
-- reads the scope. Both ride the existing scope+session index for lookup; this
-- index keeps the "which claims are parked" scan off a table scan.
CREATE INDEX idx_auto_improve_scheduler_claims_attempts
    ON auto_improve_scheduler_claims(workspace_id, project_id, attempts);
