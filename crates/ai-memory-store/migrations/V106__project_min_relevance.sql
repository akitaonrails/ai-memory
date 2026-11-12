-- Per-project minimum relevance threshold, forwarded from the nearest
-- `.ai-memory.toml` `[retrieval]` section at session start (#2038, second W1
-- criterion). `min_relevance` is the lowest calibrated reranker relevance a
-- `memory_query` hit may have for this project when a call gives no explicit
-- `min_relevance`; results below it are excluded before the count limit is
-- applied. NULL (the default for every existing row) inherits: the project
-- defers to the global `[retrieval] min_relevance`, then to no floor at all.
-- Stored values are 0.0..=1.0, matching the reranker's calibrated score range,
-- so a forwarded out-of-range marker cannot pass a meaningless threshold.
--
-- The threshold keys off the reranker's calibrated score only; a query with no
-- reranker configured rejects a `min_relevance` explicitly rather than applying
-- it to the uncalibrated RRF rank.

ALTER TABLE projects ADD COLUMN min_relevance REAL
    CHECK (min_relevance IS NULL OR (min_relevance >= 0.0 AND min_relevance <= 1.0));
