-- FTS5's `unicode61 ... tokenchars '_-'` keeps `foo-bar-alpha` as ONE token
-- (V12 then V97 chose that on purpose, so `ai-memory` stays a single term).
-- The cost surfaced in #2013: the token's component words are unreachable
-- lexically — `alpha` never matched `foo-bar-alpha`, only the quoted whole
-- token did, and `global` search (FTS-only) missed the page entirely.
--
-- Dropping `_-` from `tokenchars` would fix the parts but lose the whole
-- token as a single term. Instead keep the tokenizer and index a second
-- spelling of the same text, with `-`/`_` turned into spaces, so both forms
-- match: the whole token still hits `body`, and its parts hit `body_parts`.
--
-- The two extra columns are VIRTUAL generated columns: the value is computed
-- on read, which is exactly what FTS5's external-content lookup and
-- `('rebuild')` do, and what `new.<col>` resolves to inside the triggers.
-- Nothing is duplicated on disk and the source rows are untouched.
--
-- The tokenizer is fixed at CREATE time: drop, recreate, rebuild. Column
-- order is append-only — `body` stays at index 1 because `snippet(...)` call
-- sites address it positionally, and `path_search` is already split by
-- `path_search_text` so it needs no parts column.

-- ── pages ────────────────────────────────────────────────────────────────
ALTER TABLE pages ADD COLUMN title_parts TEXT
    GENERATED ALWAYS AS (replace(replace(title, '-', ' '), '_', ' ')) VIRTUAL;
ALTER TABLE pages ADD COLUMN body_parts TEXT
    GENERATED ALWAYS AS (replace(replace(body, '-', ' '), '_', ' ')) VIRTUAL;

DROP TRIGGER IF EXISTS pages_fts_ai;
DROP TRIGGER IF EXISTS pages_fts_ad;
DROP TRIGGER IF EXISTS pages_fts_au;
DROP TABLE IF EXISTS pages_fts;

CREATE VIRTUAL TABLE pages_fts USING fts5(
    title, body, path_search, title_parts, body_parts,
    content='pages',
    content_rowid='rowid',
    tokenize="unicode61 remove_diacritics 2 tokenchars '_-'"
);

CREATE TRIGGER pages_fts_ai AFTER INSERT ON pages BEGIN
    INSERT INTO pages_fts(rowid, title, body, path_search, title_parts, body_parts)
        VALUES (new.rowid, new.title, new.body, new.path_search, new.title_parts, new.body_parts);
END;

CREATE TRIGGER pages_fts_ad AFTER DELETE ON pages BEGIN
    INSERT INTO pages_fts(pages_fts, rowid, title, body, path_search, title_parts, body_parts)
        VALUES ('delete', old.rowid, old.title, old.body, old.path_search, old.title_parts, old.body_parts);
END;

CREATE TRIGGER pages_fts_au AFTER UPDATE OF title, body, path_search ON pages BEGIN
    INSERT INTO pages_fts(pages_fts, rowid, title, body, path_search, title_parts, body_parts)
        VALUES ('delete', old.rowid, old.title, old.body, old.path_search, old.title_parts, old.body_parts);
    INSERT INTO pages_fts(rowid, title, body, path_search, title_parts, body_parts)
        VALUES (new.rowid, new.title, new.body, new.path_search, new.title_parts, new.body_parts);
END;

INSERT INTO pages_fts(pages_fts) VALUES('rebuild');

-- ── observations ─────────────────────────────────────────────────────────
ALTER TABLE observations ADD COLUMN title_parts TEXT
    GENERATED ALWAYS AS (replace(replace(title, '-', ' '), '_', ' ')) VIRTUAL;
ALTER TABLE observations ADD COLUMN body_parts TEXT
    GENERATED ALWAYS AS (replace(replace(body, '-', ' '), '_', ' ')) VIRTUAL;

DROP TRIGGER IF EXISTS observations_fts_ai;
DROP TRIGGER IF EXISTS observations_fts_ad;
DROP TRIGGER IF EXISTS observations_fts_au;
DROP TABLE IF EXISTS observations_fts;

CREATE VIRTUAL TABLE observations_fts USING fts5(
    title, body, title_parts, body_parts,
    content='observations',
    content_rowid='rowid',
    tokenize="unicode61 remove_diacritics 2 tokenchars '_-'"
);

CREATE TRIGGER observations_fts_ai AFTER INSERT ON observations BEGIN
    INSERT INTO observations_fts(rowid, title, body, title_parts, body_parts)
        VALUES (new.rowid, new.title, new.body, new.title_parts, new.body_parts);
END;

CREATE TRIGGER observations_fts_ad AFTER DELETE ON observations BEGIN
    INSERT INTO observations_fts(observations_fts, rowid, title, body, title_parts, body_parts)
        VALUES ('delete', old.rowid, old.title, old.body, old.title_parts, old.body_parts);
END;

CREATE TRIGGER observations_fts_au AFTER UPDATE ON observations BEGIN
    INSERT INTO observations_fts(observations_fts, rowid, title, body, title_parts, body_parts)
        VALUES ('delete', old.rowid, old.title, old.body, old.title_parts, old.body_parts);
    INSERT INTO observations_fts(rowid, title, body, title_parts, body_parts)
        VALUES (new.rowid, new.title, new.body, new.title_parts, new.body_parts);
END;

INSERT INTO observations_fts(observations_fts) VALUES('rebuild');
