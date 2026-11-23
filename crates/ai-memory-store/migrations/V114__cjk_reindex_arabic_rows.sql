-- #2426 made the auxiliary CJK indexes (V87) also hold normalized Arabic-script
-- runs: tashkeel, tatweel and BiDi marks stripped, letter variants folded,
-- Arabic-Indic digits mapped to ASCII. Every page version and observation
-- stored before it already has an auxiliary row, written with no terms for its
-- Arabic text. The startup backfill only inserts rows that are missing and
-- resumes above its watermark (V104), so those rows never got the new terms and
-- a normalized Arabic query could not reach them (#2439).
--
-- Drop the auxiliary rows of the source rows that hold a character from the
-- blocks `is_arabic_block` (fts_query.rs) accepts, and lower each backfill
-- watermark to just below the first of them, so the next open rebuilds exactly
-- those rows through the existing anti-join. Rows without Arabic-script text
-- keep their auxiliary rows, and a store holding none keeps its watermarks. An
-- absent watermark row leaves the `UPDATE` a no-op, which is correct: the
-- backfill then scans from the first row anyway.
CREATE TEMP TABLE v114_arabic_pages AS
WITH arabic(pattern) AS (
    SELECT '*[' || char(0x0600) || '-' || char(0x06FF)
                || char(0x0750) || '-' || char(0x077F)
                || char(0x08A0) || '-' || char(0x08FF)
                || char(0xFB50) || '-' || char(0xFDFF)
                || char(0xFE70) || '-' || char(0xFEFF) || ']*'
)
SELECT p.rowid AS source_rowid
  FROM pages AS p, arabic
 WHERE p.title GLOB arabic.pattern
    OR p.body GLOB arabic.pattern
    OR p.path_search GLOB arabic.pattern;

UPDATE backfill_watermarks
   SET max_rowid = min(max_rowid, (SELECT min(source_rowid) FROM v114_arabic_pages) - 1)
 WHERE name = 'cjk_pages'
   AND EXISTS (SELECT 1 FROM v114_arabic_pages);

DELETE FROM pages_cjk_fts
 WHERE rowid IN (SELECT source_rowid FROM v114_arabic_pages);

DROP TABLE v114_arabic_pages;

CREATE TEMP TABLE v114_arabic_observations AS
WITH arabic(pattern) AS (
    SELECT '*[' || char(0x0600) || '-' || char(0x06FF)
                || char(0x0750) || '-' || char(0x077F)
                || char(0x08A0) || '-' || char(0x08FF)
                || char(0xFB50) || '-' || char(0xFDFF)
                || char(0xFE70) || '-' || char(0xFEFF) || ']*'
)
SELECT o.rowid AS source_rowid
  FROM observations AS o, arabic
 WHERE o.title GLOB arabic.pattern
    OR o.body GLOB arabic.pattern;

UPDATE backfill_watermarks
   SET max_rowid = min(max_rowid, (SELECT min(source_rowid) FROM v114_arabic_observations) - 1)
 WHERE name = 'cjk_observations'
   AND EXISTS (SELECT 1 FROM v114_arabic_observations);

DELETE FROM observations_cjk_fts
 WHERE rowid IN (SELECT source_rowid FROM v114_arabic_observations);

DROP TABLE v114_arabic_observations;
