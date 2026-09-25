-- Record when a session was linked to a managed run.
--
-- `managed_runs.native_session_id` starts as the workstream's current session
-- when the run is prepared, and a hook in the launched child (or the launcher
-- itself, before the spawn) replaces it through the link path. A link that
-- repeats the prepared session left no trace, so after exit `ai-memory run`
-- could not tell "the child confirmed this session" from "nothing linked", and
-- fell back to the newest session in the checkout, which a concurrent launch
-- may own.
--
-- NULL means nothing linked during this run; rows written by an older version
-- stay NULL, the reading those runs already had.

ALTER TABLE managed_runs ADD COLUMN native_session_linked_at INTEGER;
