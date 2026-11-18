-- #2344: an offline maintenance edit — a direct SQL `UPDATE` of a page's or an
-- observation's indexed columns — fires the V87 `_au` triggers, which delete the
-- derived CJK row defensively. Since #2337 the startup backfill resumes from a
-- persisted watermark (V104), so a row at or below that watermark whose CJK
-- entry was removed this way is never rebuilt, and CJK substring search silently
-- misses it.
--
-- Pages are versioned and observations immutable, so these `UPDATE OF` triggers
-- never fire on the normal online write path (which inserts a new, higher
-- rowid) — only direct maintenance SQL fires them. Lowering the CJK backfill
-- watermark to just below the edited rowid here makes the next open rescan from
-- that point and repair the hole through the existing anti-join, while an idle
-- restart (no edit, so no trigger) keeps #2337's O(1) resume. An absent
-- watermark row (no backfill has run yet) leaves the `UPDATE` a no-op, which is
-- correct: a full scan runs anyway.
DROP TRIGGER pages_cjk_fts_au;
CREATE TRIGGER pages_cjk_fts_au AFTER UPDATE OF title, body, path_search ON pages BEGIN
    DELETE FROM pages_cjk_fts WHERE rowid = old.rowid;
    UPDATE backfill_watermarks
       SET max_rowid = min(max_rowid, old.rowid - 1)
     WHERE name = 'cjk_pages';
END;

DROP TRIGGER observations_cjk_fts_au;
CREATE TRIGGER observations_cjk_fts_au AFTER UPDATE OF title, body ON observations BEGIN
    DELETE FROM observations_cjk_fts WHERE rowid = old.rowid;
    UPDATE backfill_watermarks
       SET max_rowid = min(max_rowid, old.rowid - 1)
     WHERE name = 'cjk_observations';
END;
