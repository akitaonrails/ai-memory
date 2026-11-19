-- Trust classification on observations and pages, step 2 of #2164
-- (docs/design-memory-poisoning-provenance.md).
--
-- Every observation receives its trust class once at hook ingress from its
-- `ObservationKind` (`user-prompt` -> `operator_authored`,
-- `post-tool-use` -> `tool_result`, assistant/lifecycle -> `model_generated`)
-- and is never rewritten afterwards.
--
-- Every page stores its derived trust class at write/compile time so it
-- survives observation retention pruning:
--   * Compiled pages (`page_sources` present) take the least-trusted class
--     among their source observations (`tool_result < model_generated < operator_authored`).
--   * Citationless rewrites (`memory_patch_page`, lint/compaction/curator)
--     inherit the superseded version's trust class so a `tool_result` page
--     cannot be laundered into `operator_authored` with a patch.
--   * Explicit `memory_write_page` without `session_id` and disk hand-edits
--     set `operator_authored`.
--   * Zero-LLM rule-based `sessions/<id>.md` pages derive their class from
--     the session's least-trusted observation without synthetic `page_evidence`.
--
-- Existing rows created before V113 default to `model_generated` (no backfill).

ALTER TABLE observations
    ADD COLUMN trust_class TEXT NOT NULL DEFAULT 'model_generated'
    CHECK (trust_class IN ('operator_authored', 'model_generated', 'tool_result'));

ALTER TABLE pages
    ADD COLUMN trust_class TEXT NOT NULL DEFAULT 'model_generated'
    CHECK (trust_class IN ('operator_authored', 'model_generated', 'tool_result'));
