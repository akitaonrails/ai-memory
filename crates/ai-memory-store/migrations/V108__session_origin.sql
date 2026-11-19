-- How a session was launched, when a launcher declares it (#2180). NULL for
-- every existing row and for ordinary interactive sessions; set to a known
-- token (today only 'automation') by the hook ingest path from a validated
-- AI_MEMORY_SESSION_ORIGIN. The profile harvester can be told to skip prompts
-- from automation-origin sessions ([profile] harvest_prompts = "interactive"),
-- so a generated orchestrator brief is not mistaken for the operator's own
-- preferences. Capture, handoff and recall ignore this column.
ALTER TABLE sessions ADD COLUMN origin TEXT;
