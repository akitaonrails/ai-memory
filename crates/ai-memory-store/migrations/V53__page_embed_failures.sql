-- Durable record of an embed attempt that did NOT produce an embedding (#528).
--
-- `page_embeddings` records only successes, and its upsert rewrites
-- `created_at`, so a global `embed --force` erases any signal about which row
-- was previously stale. A failed or skipped embed left nothing at all: the
-- inline write path warns and returns success, and the backfill's per-page
-- warnings live only in container logs that a restart takes with them. The
-- result was that `status` could report a page missing an embedding with no
-- way to ask when it was last attempted or why it did not take.
--
-- Deliberately failure-only. A success writes nothing here — it is already
-- recorded by the existence of a `page_embeddings` row — so the common path
-- pays no extra write. A row therefore means "the last attempt on this page
-- did not produce an embedding", and joining against `page_embeddings` tells
-- an operator whether that is still true or whether a later pass recovered it.
-- Keeping the row after recovery is intentional: it is the history that
-- `--force` used to destroy.
--
-- One row per page, cascading with it, so the ledger cannot outgrow the corpus.
CREATE TABLE page_embed_failures (
    page_id  BLOB PRIMARY KEY NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    at       INTEGER NOT NULL,
    outcome  TEXT NOT NULL CHECK (outcome IN ('failed', 'unreadable', 'skipped_empty')),
    -- Truncated provider/IO error. NULL for `skipped_empty`, which has no error.
    detail   TEXT
);

CREATE INDEX idx_page_embed_failures_at ON page_embed_failures(at DESC);
