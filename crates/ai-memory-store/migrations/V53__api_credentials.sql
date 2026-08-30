-- Native API credentials. Copy existing 32-byte users.token_hash
-- values losslessly (id = user_id, label = legacy-user-token).
-- Abort — no partial commit — if any non-NULL hash is the wrong length.
-- Keep users.token_hash / token_expired_at as the deprecated 1.x
-- compatibility write path. These mirror triggers are load-bearing until
-- the user token shims are removed in 2.0; auth lookup uses api_credentials.

CREATE TABLE v53_token_hash_guard (
    ok INTEGER NOT NULL CHECK (ok = 0)
);
INSERT INTO v53_token_hash_guard (ok)
SELECT COUNT(*) FROM users WHERE token_hash IS NOT NULL AND length(token_hash) != 32;
DROP TABLE v53_token_hash_guard;

CREATE TABLE api_credentials (
    id            BLOB NOT NULL PRIMARY KEY,
    user_id       BLOB NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    label         TEXT NOT NULL,
    token_hash    BLOB NOT NULL UNIQUE CHECK (length(token_hash) = 32),
    preview       TEXT,
    created_at    INTEGER NOT NULL,
    last_used_at  INTEGER,
    expires_at    INTEGER,
    revoked_at    INTEGER
);

CREATE INDEX idx_api_credentials_user ON api_credentials(user_id);

INSERT INTO api_credentials (
    id, user_id, label, token_hash, preview, created_at, last_used_at, expires_at, revoked_at
)
SELECT
    id,
    id,
    'legacy-user-token',
    token_hash,
    NULL,
    created_at,
    last_seen_at,
    NULL,
    token_expired_at
FROM users
WHERE token_hash IS NOT NULL AND length(token_hash) = 32;

CREATE TRIGGER users_token_hash_length_ai
BEFORE INSERT ON users
FOR EACH ROW
WHEN NEW.token_hash IS NOT NULL AND length(NEW.token_hash) != 32
BEGIN
    SELECT RAISE(ABORT, 'users.token_hash must be 32 bytes');
END;

CREATE TRIGGER users_token_hash_length_au
BEFORE UPDATE OF token_hash ON users
FOR EACH ROW
WHEN NEW.token_hash IS NOT NULL AND length(NEW.token_hash) != 32
BEGIN
    SELECT RAISE(ABORT, 'users.token_hash must be 32 bytes');
END;

CREATE TRIGGER users_token_hash_mirror_ai
AFTER INSERT ON users
FOR EACH ROW
WHEN NEW.token_hash IS NOT NULL AND length(NEW.token_hash) = 32
BEGIN
    INSERT INTO api_credentials (
        id, user_id, label, token_hash, preview, created_at, last_used_at, expires_at, revoked_at
    ) VALUES (
        NEW.id, NEW.id, 'legacy-user-token', NEW.token_hash, NULL,
        NEW.created_at, NULL, NULL, NEW.token_expired_at
    )
    ON CONFLICT(id) DO UPDATE SET
        token_hash = excluded.token_hash,
        revoked_at = excluded.revoked_at;
END;

CREATE TRIGGER users_token_hash_mirror_au
AFTER UPDATE OF token_hash, token_expired_at ON users
FOR EACH ROW
WHEN NEW.token_hash IS NOT NULL AND length(NEW.token_hash) = 32
BEGIN
    INSERT INTO api_credentials (
        id, user_id, label, token_hash, preview, created_at, last_used_at, expires_at, revoked_at
    ) VALUES (
        NEW.id, NEW.id, 'legacy-user-token', NEW.token_hash, NULL,
        NEW.created_at, NULL, NULL, NEW.token_expired_at
    )
    ON CONFLICT(id) DO UPDATE SET
        token_hash = excluded.token_hash,
        revoked_at = excluded.revoked_at;
END;
