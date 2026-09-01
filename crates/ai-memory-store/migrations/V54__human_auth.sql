-- Human authentication: nullable legacy token_hash, role/password
-- columns, bootstrap marker, and web sessions.
--
-- `Store::open` disables foreign keys around refinery's migration runner;
-- migration-local PRAGMA toggles are ineffective inside refinery's
-- transaction. Rebuild `users` while preserving pages.author_id /
-- audit_log.author_id blob values.
--
-- token_hash stays nullable and UNIQUE (SQLite UNIQUE allows many
-- NULLs) without a length CHECK so a pre-V14 malformed blob can still
-- be copied; V53 aborts on length != 32. Backfill every existing row
-- as role='user'. Defaults let a restored pre-V52 binary INSERT.


CREATE TABLE users_new (
    id                    BLOB NOT NULL PRIMARY KEY,
    username              TEXT NOT NULL UNIQUE,
    name                  TEXT,
    email                 TEXT UNIQUE COLLATE NOCASE,
    token_hash            BLOB UNIQUE,
    created_at            INTEGER NOT NULL,
    last_seen_at          INTEGER,
    token_expired_at      INTEGER,
    role                  TEXT NOT NULL DEFAULT 'user' CHECK (role IN ('root', 'user')),
    password_hash         TEXT,
    must_change_password  INTEGER NOT NULL DEFAULT 0 CHECK (must_change_password IN (0, 1)),
    disabled_at           INTEGER
);

INSERT INTO users_new (
    id, username, name, email, token_hash, created_at, last_seen_at,
    token_expired_at, role, password_hash, must_change_password, disabled_at
)
SELECT
    id, username, name, email, token_hash, created_at, last_seen_at,
    token_expired_at, 'user', NULL, 0, NULL
FROM users;

DROP TABLE users;
ALTER TABLE users_new RENAME TO users;

CREATE TABLE human_auth_state (
    id                   INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    bootstrap_completed  INTEGER NOT NULL DEFAULT 0 CHECK (bootstrap_completed IN (0, 1))
);
INSERT INTO human_auth_state (id, bootstrap_completed) VALUES (1, 0);

CREATE TABLE web_sessions (
    id            BLOB NOT NULL PRIMARY KEY,
    session_hash  BLOB NOT NULL UNIQUE CHECK (length(session_hash) = 32),
    user_id       BLOB NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    csrf_hash     BLOB NOT NULL CHECK (length(csrf_hash) = 32),
    created_at    INTEGER NOT NULL,
    last_used_at  INTEGER NOT NULL,
    expires_at    INTEGER NOT NULL,
    revoked_at    INTEGER
);

CREATE INDEX idx_web_sessions_user ON web_sessions(user_id);
CREATE INDEX idx_web_sessions_expires ON web_sessions(expires_at);

-- `PRAGMA foreign_key_check` alone only returns rows under execute_batch and
-- cannot abort. Fail the migration if this rebuild damaged either V52-owned
-- author reference, without scanning unrelated pre-existing violations.
CREATE TABLE v52_fk_guard (
    ok INTEGER NOT NULL CHECK (ok = 0)
);
INSERT INTO v52_fk_guard (ok)
SELECT
    (SELECT COUNT(*) FROM pragma_foreign_key_check('pages')) +
    (SELECT COUNT(*) FROM pragma_foreign_key_check('audit_log'));
DROP TABLE v52_fk_guard;
