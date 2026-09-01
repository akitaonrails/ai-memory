//! Native `aim_` API credentials. Auth lookup never reads `users.token_hash`.

use ai_memory_core::{ApiCredential, ApiCredentialId, NATIVE_API_KEY_PREFIX, User, UserId};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{StoreError, StoreResult};
use crate::users::{TOKEN_HASH_LEN, TOKEN_RAW_LEN, map_unique_violation, row_to_user};

/// `aim_` + 43 URL-safe characters.
///
/// # Errors
/// [`StoreError::Os`] when the CSPRNG fails.
pub fn generate_api_key() -> StoreResult<String> {
    let mut bytes = [0u8; TOKEN_RAW_LEN];
    getrandom::fill(&mut bytes)
        .map_err(|e| StoreError::Os(format!("os csprng failed during api key: {e}")))?;
    Ok(format!(
        "{NATIVE_API_KEY_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(bytes)
    ))
}

/// Non-authenticating preview (last four of the secret).
#[must_use]
pub fn preview_for(secret: &str) -> String {
    let tail = secret
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("…{tail}")
}

/// User authenticated by an active API credential. Always `AuthLevel::User`.
#[derive(Debug, Clone)]
pub struct AuthenticatedApiUser {
    /// Owning identity.
    pub user: User,
    /// Credential that matched.
    pub credential_id: ApiCredentialId,
}

/// Insert a credential. `id` is caller-chosen so brownfield copies can
/// reuse `user_id`.
///
/// # Errors
/// Duplicate hash / SQL.
pub fn insert_api_credential(
    conn: &Connection,
    id: ApiCredentialId,
    user_id: UserId,
    label: &str,
    token_hash: &[u8; TOKEN_HASH_LEN],
    preview: Option<&str>,
) -> StoreResult<ApiCredentialId> {
    let now = Timestamp::now().as_microsecond();
    conn.execute(
        "INSERT INTO api_credentials \
         (id, user_id, label, token_hash, preview, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id.as_bytes(),
            user_id.as_bytes(),
            label,
            token_hash.as_slice(),
            preview,
            now,
        ],
    )
    .map_err(map_unique_violation)?;
    Ok(id)
}

/// Rotate: write a new hash on the same id, clear revoked_at.
///
/// # Errors
/// Duplicate hash / SQL / not found.
pub fn rotate_api_credential(
    conn: &Connection,
    id: ApiCredentialId,
    new_token_hash: &[u8; TOKEN_HASH_LEN],
    preview: Option<&str>,
) -> StoreResult<bool> {
    let rows = conn
        .execute(
            "UPDATE api_credentials \
         SET token_hash = ?1, preview = ?2, revoked_at = NULL \
         WHERE id = ?3",
            params![new_token_hash.as_slice(), preview, id.as_bytes()],
        )
        .map_err(map_unique_violation)?;
    Ok(rows > 0)
}

/// Revoke. Idempotent.
///
/// # Errors
/// SQL failures.
pub fn revoke_api_credential(conn: &Connection, id: ApiCredentialId) -> StoreResult<bool> {
    let now = Timestamp::now().as_microsecond();
    let rows = conn.execute(
        "UPDATE api_credentials SET revoked_at = COALESCE(revoked_at, ?1) WHERE id = ?2",
        params![now, id.as_bytes()],
    )?;
    Ok(rows > 0)
}

/// Touch last_used_at.
///
/// # Errors
/// SQL failures.
pub fn touch_api_credential(conn: &Connection, id: ApiCredentialId) -> StoreResult<bool> {
    let now = Timestamp::now().as_microsecond();
    let rows = conn.execute(
        "UPDATE api_credentials SET last_used_at = ?1 WHERE id = ?2",
        params![now, id.as_bytes()],
    )?;
    Ok(rows > 0)
}

/// Hot-path auth lookup. Does **not** filter `users.disabled_at`.
///
/// # Errors
/// SQL failures.
pub fn find_active_user_by_token_hash(
    conn: &Connection,
    token_hash: &[u8; TOKEN_HASH_LEN],
    now: i64,
) -> StoreResult<Option<AuthenticatedApiUser>> {
    conn.query_row(
        "SELECT u.id, u.username, u.name, u.email, u.created_at, u.last_seen_at, \
                u.role, u.must_change_password, u.disabled_at, u.password_hash, \
                u.token_expired_at, c.id \
         FROM api_credentials c \
         JOIN users u ON u.id = c.user_id \
         WHERE c.token_hash = ?1 AND c.revoked_at IS NULL \
           AND (c.expires_at IS NULL OR c.expires_at > ?2)",
        params![token_hash.as_slice(), now],
        |row| {
            let cred_bytes: Vec<u8> = row.get(11)?;
            let credential_id = ApiCredentialId::from_slice(&cred_bytes).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    11,
                    rusqlite::types::Type::Blob,
                    Box::new(std::io::Error::other(e.to_string())),
                )
            })?;
            Ok(AuthenticatedApiUser {
                user: row_to_user(row, 0)?,
                credential_id,
            })
        },
    )
    .optional()
    .map_err(StoreError::from)
}

/// True when any credential (active or revoked) stores this hash.
/// Used by password-reuse detection.
///
/// # Errors
/// SQL failures.
pub fn token_hash_exists(
    conn: &Connection,
    token_hash: &[u8; TOKEN_HASH_LEN],
) -> StoreResult<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM api_credentials WHERE token_hash = ?1)",
        params![token_hash.as_slice()],
        |row| row.get(0),
    )
    .map_err(StoreError::from)
}

/// List credentials for one user, newest first.
///
/// # Errors
/// SQL failures.
pub fn list_api_credentials_for_user(
    conn: &Connection,
    user_id: UserId,
) -> StoreResult<Vec<ApiCredential>> {
    let mut stmt = conn.prepare(
        "SELECT id, user_id, label, preview, created_at, last_used_at, expires_at, revoked_at \
         FROM api_credentials WHERE user_id = ?1 ORDER BY created_at DESC",
    )?;
    let rows = stmt
        .query_map(params![user_id.as_bytes()], row_to_credential)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// List every credential.
///
/// # Errors
/// SQL failures.
pub fn list_api_credentials(conn: &Connection) -> StoreResult<Vec<ApiCredential>> {
    let mut stmt = conn.prepare(
        "SELECT id, user_id, label, preview, created_at, last_used_at, expires_at, revoked_at \
         FROM api_credentials ORDER BY created_at DESC",
    )?;
    let rows = stmt
        .query_map([], row_to_credential)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Look up one credential by id.
///
/// # Errors
/// SQL failures.
pub fn find_api_credential(
    conn: &Connection,
    id: ApiCredentialId,
) -> StoreResult<Option<ApiCredential>> {
    conn.query_row(
        "SELECT id, user_id, label, preview, created_at, last_used_at, expires_at, revoked_at \
         FROM api_credentials WHERE id = ?1",
        params![id.as_bytes()],
        row_to_credential,
    )
    .optional()
    .map_err(StoreError::from)
}

/// True when any native credential row exists (pepper required at startup).
///
/// # Errors
/// SQL failures.
pub fn api_credentials_exist(conn: &Connection) -> StoreResult<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM api_credentials)", [], |row| {
        row.get(0)
    })
    .map_err(StoreError::from)
}

fn row_to_credential(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApiCredential> {
    let id_bytes: Vec<u8> = row.get(0)?;
    let id = ApiCredentialId::from_slice(&id_bytes).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Blob,
            Box::new(std::io::Error::other(e.to_string())),
        )
    })?;
    let user_bytes: Vec<u8> = row.get(1)?;
    let user_id = UserId::from_slice(&user_bytes).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Blob,
            Box::new(std::io::Error::other(e.to_string())),
        )
    })?;
    Ok(ApiCredential {
        id,
        user_id,
        label: row.get(2)?,
        preview: row.get(3)?,
        created_at: row.get(4)?,
        last_used_at: row.get(5)?,
        expires_at: row.get(6)?,
        revoked_at: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users::{self, TokenPepper, hash_token};
    use ai_memory_core::{NewUser, UserId, UserRole};
    use rusqlite::{Connection, params};

    fn open_to(version: u32) -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrations::run_to(&mut conn, version).unwrap();
        conn
    }

    fn insert_pre_human_user(
        conn: &Connection,
        username: &str,
        hash: &[u8],
        expired: bool,
    ) -> UserId {
        let id = UserId::new();
        conn.execute(
            "INSERT INTO users (id, username, name, email, token_hash, created_at, token_expired_at) \
             VALUES (?1, ?2, NULL, NULL, ?3, 1, ?4)",
            params![
                id.as_bytes(),
                username,
                hash,
                if expired { Some(99i64) } else { None }
            ],
        )
        .unwrap();
        id
    }

    #[test]
    fn api_credentials_v55_copies_active_and_expired_hashes() {
        let mut conn = open_to(50);
        let active = [0xAAu8; 32];
        let expired = [0xBBu8; 32];
        let alice = insert_pre_human_user(&conn, "alice", &active, false);
        let bob = insert_pre_human_user(&conn, "bob", &expired, true);
        crate::migrations::run_to(&mut conn, 55).unwrap();

        let (label, hash, revoked): (String, Vec<u8>, Option<i64>) = conn
            .query_row(
                "SELECT label, token_hash, revoked_at FROM api_credentials WHERE id = ?1",
                params![alice.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(label, "legacy-user-token");
        assert_eq!(hash, active);
        assert!(revoked.is_none());

        let (hash, revoked): (Vec<u8>, Option<i64>) = conn
            .query_row(
                "SELECT token_hash, revoked_at FROM api_credentials WHERE id = ?1",
                params![bob.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(hash, expired);
        assert_eq!(revoked, Some(99));
    }

    #[test]
    fn api_credentials_v55_aborts_on_malformed_hash_length() {
        let mut conn = open_to(54);
        let id = UserId::new();
        conn.execute(
            "INSERT INTO users (id, username, name, email, token_hash, created_at, role) \
             VALUES (?1, 'bad', NULL, NULL, ?2, 1, 'user')",
            params![id.as_bytes(), [1u8; 16].as_slice()],
        )
        .unwrap();
        let err = crate::migrations::run_to(&mut conn, 55).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("CHECK") || msg.contains("constraint") || msg.contains("ok"),
            "{msg}"
        );
    }

    #[test]
    fn api_credentials_v55_skips_password_only_users() {
        let mut conn = open_to(54);
        let id = UserId::new();
        conn.execute(
            "INSERT INTO users (id, username, name, email, token_hash, created_at, role, password_hash) \
             VALUES (?1, 'human', NULL, NULL, NULL, 1, 'user', 'phc')",
            params![id.as_bytes()],
        )
        .unwrap();
        crate::migrations::run_to(&mut conn, 55).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM api_credentials", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn api_credentials_v55_mirror_trigger_copies_pre_v54_write() {
        let mut conn = open_to(55);
        let id = UserId::new();
        let hash = [0xCCu8; 32];
        conn.execute(
            "INSERT INTO users (id, username, name, email, token_hash, created_at, role) \
             VALUES (?1, 'legacy', NULL, NULL, ?2, 1, 'user')",
            params![id.as_bytes(), hash.as_slice()],
        )
        .unwrap();
        let copied: Vec<u8> = conn
            .query_row(
                "SELECT token_hash FROM api_credentials WHERE id = ?1",
                params![id.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(copied, hash);
        crate::migrations::run(&mut conn).unwrap();
        assert!(
            find_active_user_by_token_hash(&conn, &hash, 0)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn schema_stops_at_v55_rollback_bridge() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrations::run(&mut conn).unwrap();
        let version: i64 = conn
            .query_row(
                "SELECT MAX(version) FROM refinery_schema_history",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, 55);
        let cols: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('users') WHERE name = 'token_hash'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cols, 1);
    }

    #[test]
    fn api_credentials_v55_human_create_has_no_key() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrations::run(&mut conn).unwrap();
        let mut nu = NewUser {
            username: "carol".into(),
            name: None,
            email: None,
        };
        nu.validate().unwrap();
        let id = users::insert_human_user(&conn, &nu, UserRole::User, Some("phc"), true).unwrap();
        assert!(list_api_credentials_for_user(&conn, id).unwrap().is_empty());
        assert!(!api_credentials_exist(&conn).unwrap());
    }

    #[test]
    fn api_credentials_auth_lookup_ignores_revoked_and_disabled() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrations::run(&mut conn).unwrap();
        let mut nu = NewUser {
            username: "dave".into(),
            name: None,
            email: None,
        };
        nu.validate().unwrap();
        let user_id =
            users::insert_human_user(&conn, &nu, UserRole::User, Some("phc"), false).unwrap();
        let pepper = TokenPepper::new("test-pepper-1234567890abcdef");
        let secret = generate_api_key().unwrap();
        let hash = hash_token(&secret, &pepper);
        let cred_id = ApiCredentialId::new();
        insert_api_credential(&conn, cred_id, user_id, "cli", &hash, Some("…key")).unwrap();
        let found = find_active_user_by_token_hash(&conn, &hash, 0)
            .unwrap()
            .unwrap();
        assert_eq!(found.user.id, user_id);
        revoke_api_credential(&conn, cred_id).unwrap();
        assert!(
            find_active_user_by_token_hash(&conn, &hash, 0)
                .unwrap()
                .is_none()
        );
        assert!(token_hash_exists(&conn, &hash).unwrap());
    }

    #[test]
    fn api_credentials_v55_expire_then_revive_mirrors_legacy_token() {
        let mut conn = open_to(50);
        let hash = [0xDDu8; 32];
        let id = insert_pre_human_user(&conn, "eve", &hash, false);
        crate::migrations::run_to(&mut conn, 55).unwrap();

        conn.execute(
            "UPDATE users SET token_expired_at = ?1 WHERE id = ?2",
            params![123i64, id.as_bytes()],
        )
        .unwrap();
        let (copied, revoked): (Vec<u8>, Option<i64>) = conn
            .query_row(
                "SELECT token_hash, revoked_at FROM api_credentials WHERE id = ?1",
                params![id.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(copied, hash);
        assert_eq!(revoked, Some(123));
        assert!(
            find_active_user_by_token_hash(&conn, &hash, 0)
                .unwrap()
                .is_none()
        );

        conn.execute(
            "UPDATE users SET token_expired_at = NULL WHERE id = ?1",
            params![id.as_bytes()],
        )
        .unwrap();
        let (copied, revoked): (Vec<u8>, Option<i64>) = conn
            .query_row(
                "SELECT token_hash, revoked_at FROM api_credentials WHERE id = ?1",
                params![id.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(copied, hash);
        assert!(revoked.is_none());
        let found = find_active_user_by_token_hash(&conn, &hash, 0)
            .unwrap()
            .unwrap();
        assert_eq!(found.user.id, id);
        assert_eq!(found.credential_id.as_bytes(), id.as_bytes());
    }

    #[test]
    fn api_credentials_v55_post_v55_malformed_hash_write_aborts() {
        let conn = open_to(55);
        let bad_id = UserId::new();
        let err = conn
            .execute(
                "INSERT INTO users (id, username, name, email, token_hash, created_at, role) \
                 VALUES (?1, 'bad-post', NULL, NULL, ?2, 1, 'user')",
                params![bad_id.as_bytes(), [1u8; 16].as_slice()],
            )
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("users.token_hash must be 32 bytes"), "{msg}");
        let users: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM users WHERE username = 'bad-post'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let creds: i64 = conn
            .query_row("SELECT COUNT(*) FROM api_credentials", [], |row| row.get(0))
            .unwrap();
        assert_eq!(users, 0);
        assert_eq!(creds, 0);

        let id = UserId::new();
        let good = [0xEEu8; 32];
        conn.execute(
            "INSERT INTO users (id, username, name, email, token_hash, created_at, role) \
             VALUES (?1, 'ok-then-bad', NULL, NULL, ?2, 1, 'user')",
            params![id.as_bytes(), good.as_slice()],
        )
        .unwrap();
        let err = conn
            .execute(
                "UPDATE users SET token_hash = ?1 WHERE id = ?2",
                params![[2u8; 8].as_slice(), id.as_bytes()],
            )
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("users.token_hash must be 32 bytes"), "{msg}");
        let (user_hash, cred_hash): (Vec<u8>, Vec<u8>) = conn
            .query_row(
                "SELECT u.token_hash, c.token_hash \
                 FROM users u JOIN api_credentials c ON c.id = u.id \
                 WHERE u.id = ?1",
                params![id.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(user_hash, good);
        assert_eq!(cred_hash, good);
        assert!(
            find_active_user_by_token_hash(&conn, &good, 0)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn api_credentials_v55_v50_peppered_secret_still_authenticates() {
        let mut conn = open_to(50);
        let pepper = TokenPepper::new("test-pepper-1234567890abcdef");
        let active_secret = "v50-plaintext-active-secret";
        let expired_secret = "v50-plaintext-expired-secret";
        let active_hash = hash_token(active_secret, &pepper);
        let expired_hash = hash_token(expired_secret, &pepper);
        let alice = insert_pre_human_user(&conn, "alice-v50", &active_hash, false);
        let bob = insert_pre_human_user(&conn, "bob-v50", &expired_hash, true);
        crate::migrations::run_to(&mut conn, 55).unwrap();

        let copied_active: Vec<u8> = conn
            .query_row(
                "SELECT token_hash FROM api_credentials WHERE id = ?1",
                params![alice.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        let copied_expired: Vec<u8> = conn
            .query_row(
                "SELECT token_hash FROM api_credentials WHERE id = ?1",
                params![bob.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(copied_active, active_hash);
        assert_eq!(copied_expired, expired_hash);

        let lookup_hash = hash_token(active_secret, &pepper);
        assert_eq!(lookup_hash, active_hash);
        let found = find_active_user_by_token_hash(&conn, &lookup_hash, 0)
            .unwrap()
            .unwrap();
        assert_eq!(found.user.id, alice);
        assert_eq!(found.credential_id.as_bytes(), alice.as_bytes());

        let expired_lookup = hash_token(expired_secret, &pepper);
        assert_eq!(expired_lookup, expired_hash);
        assert!(
            find_active_user_by_token_hash(&conn, &expired_lookup, 0)
                .unwrap()
                .is_none()
        );
    }
}
