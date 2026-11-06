-- An explicit `memory_handoff_begin` baton can now carry the caller's own
-- session (#2014). Until now a NULL `from_session_id` was the only mark of a
-- manual handoff, so this flag keeps such a baton manual: project-wide for cwd
-- matching, preferred over automatic ones, and never refreshed or swept by the
-- automatic SessionEnd and turn-checkpoint paths. Existing rows read 0, and a
-- row with no source session stays manual exactly as before.
ALTER TABLE handoffs ADD COLUMN manual INTEGER NOT NULL DEFAULT 0 CHECK (manual IN (0, 1));
