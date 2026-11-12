-- Per-project retrieval criteria, forwarded from the nearest `.ai-memory.toml`
-- `[retrieval]` section at session start (#2038). `default_result_limit` is the
-- number of hits `memory_query` returns for the project when a call gives no
-- explicit `limit`. NULL (the default for every existing row) inherits: the
-- project defers to the global `[retrieval] default_result_limit`, then to the
-- built-in default of 10. Stored values are 1..=100, matching the per-call
-- clamp, so a forwarded out-of-range marker cannot widen or empty a query.

ALTER TABLE projects ADD COLUMN default_result_limit INTEGER
    CHECK (default_result_limit IS NULL OR default_result_limit BETWEEN 1 AND 100);
