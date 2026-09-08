-- Page-grain ingestion windows (issue #656,
-- docs/design-page-ingestion-windows.md): materialize each page version's
-- ingestion window so `as_of` can run version-filtered FTS alongside the
-- V56 entity-link windows. Same dimension, same predicate shape, page
-- grain: `valid_from` is the version's own `created_at`; `valid_to` is the
-- superseding version's `created_at` (NULL while the version is latest).
--
-- Additive like V56: two nullable columns + index + one-shot backfill. No
-- row is deleted or rewritten beyond the two new columns, and re-running
-- the UPDATEs converges on the same values. Like every refinery step, an
-- older binary fails closed on the migrated store (DataSchemaAhead) —
-- rollback is the boot-path pre-migration snapshot (serve.rs, #633), not
-- a downgrade read. No per-migration gate beyond the standard refinery
-- step, matching V56-V61.
--
-- `valid_to`, not `superseded_at`: `pages.superseded_at` already means
-- the V03 decay-tombstone eviction marker (written exactly when
-- `supersedes IS NULL`), so reusing the name would conflate "evicted by
-- the forget sweep" with "replaced by a newer version".

ALTER TABLE pages ADD COLUMN valid_from INTEGER;
ALTER TABLE pages ADD COLUMN valid_to INTEGER;

UPDATE pages SET valid_from = created_at;

-- Ordinary supersession: close at the earliest successor's birth.
UPDATE pages SET valid_to = (
    SELECT MIN(s.created_at) FROM pages s WHERE s.supersedes = pages.id
) WHERE EXISTS (SELECT 1 FROM pages s WHERE s.supersedes = pages.id);

-- Successor-less retirements (decay tombstones, graveyard merges,
-- move-regenerate rows — the V58 class): close at the decay eviction
-- marker when present, else the row's own updated_at — the best
-- available retirement instant, the same rule V58 used for the link
-- grain. Latest versions keep valid_to NULL (still current).
UPDATE pages SET valid_to = COALESCE(superseded_at, updated_at)
WHERE is_latest = 0 AND valid_to IS NULL
  AND NOT EXISTS (SELECT 1 FROM pages s WHERE s.supersedes = pages.id);

-- The as_of window scan over page versions: (scope, window) probes ride
-- this, mirroring idx_entity_page_links_validity at link grain.
CREATE INDEX idx_pages_validity
    ON pages(workspace_id, project_id, valid_from, valid_to);
