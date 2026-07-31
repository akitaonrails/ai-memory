-- V43: record page reinforcement per operator, alongside the existing counter.
--
-- `pages.access_count` is a single number per page. It cannot distinguish
-- "50 reads by one person" from "one read by each of 50 people", yet those mean
-- very different things for whether a page is load-bearing for a team. On a
-- shared server the scalar also mixes everyone's reading into one signal.
--
-- Purely additive, and deliberately NOT a replacement:
--   * `pages.access_count` keeps being bumped exactly as before, so every
--     existing query, the retention formula and the hard-delete predicate are
--     untouched.
--   * Pages that existed before this simply have no rows here, which reads as
--     "breadth unknown" and is scored identically to breadth 1 — so there is no
--     eviction cliff and no backfill to run.
--
-- `actor` is TEXT rather than a FK to `users(id)` on purpose: the identity that
-- reaches a request is often a username asserted by an authenticating proxy,
-- with no `users` row behind it. Same reasoning as `handoffs.owner_user`.
CREATE TABLE page_access (
    page_id           BLOB NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    actor             TEXT NOT NULL,
    count             INTEGER NOT NULL DEFAULT 0,
    last_accessed_at  INTEGER,
    PRIMARY KEY (page_id, actor)
) WITHOUT ROWID;

CREATE INDEX idx_page_access_page ON page_access (page_id);
