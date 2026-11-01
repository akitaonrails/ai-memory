-- `/` leaves tokenchars: `crates/store/src/ops.rs` used to index as the single
-- term `crates/store/src/ops`, so `ops.rs` and `ops` never matched it. `_` and
-- `-` stay so `ai-memory` remains one token. The tokenizer is fixed at CREATE
-- time: drop, recreate, rebuild. External-content tables; source rows untouched.

-- ── pages_fts ────────────────────────────────────────────────────────────
DROP TRIGGER IF EXISTS pages_fts_ai;
DROP TRIGGER IF EXISTS pages_fts_ad;
DROP TRIGGER IF EXISTS pages_fts_au;
DROP TABLE IF EXISTS pages_fts;

CREATE VIRTUAL TABLE pages_fts USING fts5(
    title, body, path_search,
    content='pages',
    content_rowid='rowid',
    tokenize="unicode61 remove_diacritics 2 tokenchars '_-'"
);

CREATE TRIGGER pages_fts_ai AFTER INSERT ON pages BEGIN
    INSERT INTO pages_fts(rowid, title, body, path_search)
        VALUES (new.rowid, new.title, new.body, new.path_search);
END;

CREATE TRIGGER pages_fts_ad AFTER DELETE ON pages BEGIN
    INSERT INTO pages_fts(pages_fts, rowid, title, body, path_search)
        VALUES ('delete', old.rowid, old.title, old.body, old.path_search);
END;

CREATE TRIGGER pages_fts_au AFTER UPDATE OF title, body, path_search ON pages BEGIN
    INSERT INTO pages_fts(pages_fts, rowid, title, body, path_search)
        VALUES ('delete', old.rowid, old.title, old.body, old.path_search);
    INSERT INTO pages_fts(rowid, title, body, path_search)
        VALUES (new.rowid, new.title, new.body, new.path_search);
END;

INSERT INTO pages_fts(pages_fts) VALUES('rebuild');

-- ── observations_fts ──────────────────────────────────────────────────────
DROP TRIGGER IF EXISTS observations_fts_ai;
DROP TRIGGER IF EXISTS observations_fts_ad;
DROP TRIGGER IF EXISTS observations_fts_au;
DROP TABLE IF EXISTS observations_fts;

CREATE VIRTUAL TABLE observations_fts USING fts5(
    title, body,
    content='observations',
    content_rowid='rowid',
    tokenize="unicode61 remove_diacritics 2 tokenchars '_-'"
);

CREATE TRIGGER observations_fts_ai AFTER INSERT ON observations BEGIN
    INSERT INTO observations_fts(rowid, title, body)
        VALUES (new.rowid, new.title, new.body);
END;

CREATE TRIGGER observations_fts_ad AFTER DELETE ON observations BEGIN
    INSERT INTO observations_fts(observations_fts, rowid, title, body)
        VALUES ('delete', old.rowid, old.title, old.body);
END;

CREATE TRIGGER observations_fts_au AFTER UPDATE ON observations BEGIN
    INSERT INTO observations_fts(observations_fts, rowid, title, body)
        VALUES ('delete', old.rowid, old.title, old.body);
    INSERT INTO observations_fts(rowid, title, body)
        VALUES (new.rowid, new.title, new.body);
END;

INSERT INTO observations_fts(observations_fts) VALUES('rebuild');
