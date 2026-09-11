-- Belief-strength evidence substrate (P2, docs/design-hindsight-borrowings.md
-- §3): record what produced or reaffirmed each page version, so retrieval
-- can eventually surface "how many independent sources support this" as a
-- confidence signal. Additive only, no backfill: evidence accrues going
-- forward from the write path (V63 threads it through
-- `NewPage`/`WritePageRequest`), and a page with zero rows simply reads as
-- "unknown" (count 0), not "unsupported".
--
-- This release ships the substrate and an explain-only surface
-- (`SearchExplain::evidence_count`) — nothing here feeds `PageAuthority` or
-- ranking. The confidence -> authority flip is deferred behind the R2 eval.
--
-- `source_kind` is a closed vocabulary matching
-- `ai_memory_core::PageEvidenceKind`: 'session' (a consolidated session),
-- 'observation' (a raw observation cited directly), 'feedback' (a
-- reaffirming `page_feedback` row), 'reconsolidation' (a rewrite/re-confirm
-- pass). `source_id` is opaque to the store — whatever id the citing kind
-- uses (a session id, observation id, etc.) — and the PK makes re-citing the
-- same source from the same page idempotent (`INSERT OR IGNORE`), matching
-- V56/V58's WITHOUT ROWID join-table style.
--
-- `ON DELETE CASCADE` follows `page_id` through `pages` deletes (purge,
-- graveyard), so evidence never outlives the page version it supports.

CREATE TABLE page_evidence (
    page_id     BLOB NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    source_kind TEXT NOT NULL CHECK (source_kind IN ('session','observation','feedback','reconsolidation')),
    source_id   TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (page_id, source_kind, source_id)
) WITHOUT ROWID;
