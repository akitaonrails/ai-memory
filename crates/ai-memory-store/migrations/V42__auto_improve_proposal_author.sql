-- V42: record who staged an auto-improvement proposal, and scope the
-- one-pending-per-target rule to that person.
--
-- `auto_improve_proposals` recorded `decided_by_author_id` (who approved or
-- rejected) but never who STAGED the proposal, so a reviewer could not tell
-- whose suggestion they were looking at.
--
-- The bigger problem is the uniqueness rule. `idx_auto_improve_one_pending_target`
-- allowed exactly one pending proposal per (workspace, project, target_path).
-- That is right for one operator and wrong for several: one person's pending
-- suggestion for `_rules/style.md` blocked everybody else's for the same page.
--
-- Correct in BOTH modes without a runtime switch, by folding the author into
-- the key with a NULL-collapsing expression:
--
--   * Single operator (or any unattributed caller): every row has
--     `staged_by_actor_user IS NULL`, so `COALESCE(...)` maps them all onto one
--     bucket and the existing "one pending per target" invariant holds exactly
--     as before.
--   * Several operators: each one gets their own bucket, so proposals stop
--     blocking each other.
--
-- `staged_by_actor_user` is TEXT rather than a FK to `users(id)` for the same
-- reason `handoffs.owner_user`, `sessions.actor_user` and `page_access.actor`
-- are: the identity that reaches a request is usually a username asserted by an
-- authenticating proxy, with no `users` row behind it. Keying on the row id
-- would make every proxied operator NULL and collapse them all into one bucket,
-- which is precisely the collision this index exists to break.
--
-- The COALESCE matters. A plain `UNIQUE (…, staged_by_actor_user)` would NOT
-- work: SQLite treats NULLs as distinct in unique indexes, so every existing
-- single-operator database would silently lose the guarantee and start
-- accepting unlimited pending proposals per page.
ALTER TABLE auto_improve_proposals ADD COLUMN staged_by_actor_user TEXT;

DROP INDEX IF EXISTS idx_auto_improve_one_pending_target;

CREATE UNIQUE INDEX idx_auto_improve_one_pending_target
    ON auto_improve_proposals(
        workspace_id,
        project_id,
        target_path,
        COALESCE(staged_by_actor_user, '')
    )
    WHERE status = 'pending';
