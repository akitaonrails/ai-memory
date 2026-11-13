-- Source provenance, step 1 of #2164 (docs/design-memory-poisoning-provenance.md):
-- which captured observations a page version was compiled from. A consolidated
-- page lists exactly the observations whose text reached the consolidation
-- prompt; a `memory_write_page` that cites a `session_id` lists the
-- observations that session had in the page's project when the write
-- committed. Rows are written in the page-upsert transaction, from the
-- session citations the write already carries in `NewPage::evidence`.
--
-- This is not `page_evidence` (V63). That table counts what supports a page
-- (sessions, feedback, reconsolidation passes) under opaque text ids and feeds
-- belief strength. This one names typed observation ids, so a later step can
-- derive a page's trust class from the kinds of its inputs.
--
-- Rows key on the page *version* (`pages.id`), so the set is re-derived per
-- compiled version: a new version starts with the sources of its own compile
-- and the superseded version keeps its own. A write that reproduces the
-- current version byte for byte adds its sources to that version, which then
-- lists every compile that produced it. A version written with no session
-- citation (a hand edit, a lint or compaction rewrite, a reindex from disk)
-- has no rows, which reads as "not recorded", not as "no inputs".
--
-- `workspace_id` / `project_id` repeat the page's coordinate (invariant #4)
-- and follow the page through move-project, merge-project and move-session.
-- The project and page foreign keys cascade, so purge and hard deletes of a
-- version take its rows along.
--
-- `observation_id` is deliberately not a foreign key. Observation retention
-- prunes old rows, and a cascade would erase a page's provenance on the
-- retention schedule. The id outlives the observation; a reader reports such a
-- source as no longer stored.
--
-- Additive, no backfill: a page compiled before this migration has no rows.
-- Its `page_evidence` session citation cannot be expanded honestly, because
-- the observations the consolidation prompt actually selected were not kept.

CREATE TABLE page_sources (
    workspace_id   BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id     BLOB NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    page_id        BLOB NOT NULL REFERENCES pages(id)      ON DELETE CASCADE,
    observation_id BLOB NOT NULL,
    -- Microseconds since the Unix epoch: when this source was first recorded
    -- against this page version.
    created_at     INTEGER NOT NULL,
    PRIMARY KEY (page_id, observation_id)
) WITHOUT ROWID;

-- "Which pages were compiled from this observation", scoped like every read.
CREATE INDEX idx_page_sources_observation
    ON page_sources(workspace_id, project_id, observation_id);
