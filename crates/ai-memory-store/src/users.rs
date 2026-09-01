//! User identity SQL plus human-auth lifecycle.
//!
//! Native API-key hashing stays SHA-256(token || ":" || pepper) for the
//! hot-path UNIQUE lookup. Human passwords never live in this module's
//! KDF — see [`crate::password`].

use ai_memory_core::{NewUser, User, UserId, UserRole};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::error::{StoreError, StoreResult};
use crate::web_sessions;

/// Length of a SHA-256 digest in bytes.
pub const TOKEN_HASH_LEN: usize = 32;

/// Length of a raw token in bytes (256 bits of entropy).
pub const TOKEN_RAW_LEN: usize = 32;

/// Per-server token pepper, newtyped so it is not mixed with other secrets.
#[derive(Debug, Clone)]
pub struct TokenPepper {
    inner: String,
}

impl TokenPepper {
    /// Wrap a config-loaded pepper.
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self { inner: s.into() }
    }

    fn as_bytes(&self) -> &[u8] {
        self.inner.as_bytes()
    }
}

/// `SHA-256(token || ":" || pepper)`.
#[must_use]
pub fn hash_token(token: &str, pepper: &TokenPepper) -> [u8; TOKEN_HASH_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hasher.update(b":");
    hasher.update(pepper.as_bytes());
    hasher.finalize().into()
}

/// Constant-time comparison of two hashes.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

/// Generate a fresh URL-safe-base64 secret (43 characters). Prefer
/// [`crate::api_credentials::generate_api_key`] for native keys so the
/// `aim_` prefix is applied.
///
/// # Errors
/// [`StoreError::Os`] if the OS CSPRNG fails.
pub fn generate_token() -> StoreResult<String> {
    let mut bytes = [0u8; TOKEN_RAW_LEN];
    getrandom::fill(&mut bytes)
        .map_err(|e| StoreError::Os(format!("os csprng failed during token generation: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Login lookup: public user plus the stored PHC (never serialised).
#[derive(Debug, Clone)]
pub struct LoginUser {
    /// Public identity.
    pub user: User,
    /// Argon2id PHC, if the human has a password.
    pub password_hash: Option<String>,
}

/// Insert a token-only compatibility identity. V53 mirrors
/// `users.token_hash` into the reserved `legacy-user-token`
/// `api_credentials` row in the same transaction.
///
/// # Errors
/// Duplicate username/email/token or SQL.
pub fn insert_user(
    conn: &Connection,
    new_user: &NewUser,
    token_hash: &[u8; TOKEN_HASH_LEN],
) -> StoreResult<UserId> {
    let id = UserId::new();
    let now = Timestamp::now().as_microsecond();
    conn.execute(
        "INSERT INTO users \
         (id, username, name, email, token_hash, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id.as_bytes(),
            &new_user.username,
            &new_user.name,
            &new_user.email,
            token_hash.as_slice(),
            now,
        ],
    )
    .map_err(map_unique_violation)?;
    Ok(id)
}

/// Replace and reactivate the deprecated single-token credential.
///
/// # Errors
/// SQL failures.
pub fn rotate_user_token(
    conn: &Connection,
    id: UserId,
    new_token_hash: &[u8; TOKEN_HASH_LEN],
) -> StoreResult<bool> {
    let rows = conn.execute(
        "UPDATE users SET token_hash = ?1, token_expired_at = NULL WHERE id = ?2",
        params![new_token_hash.as_slice(), id.as_bytes()],
    )?;
    Ok(rows > 0)
}

/// Idempotently revoke the deprecated single-token credential.
///
/// # Errors
/// SQL failures.
pub fn expire_user_token(conn: &Connection, id: UserId) -> StoreResult<bool> {
    let now = Timestamp::now().as_microsecond();
    let rows = conn.execute(
        "UPDATE users SET token_expired_at = COALESCE(token_expired_at, ?1) WHERE id = ?2",
        params![now, id.as_bytes()],
    )?;
    Ok(rows > 0)
}

/// Idempotently reactivate the deprecated single-token credential.
///
/// # Errors
/// SQL failures.
pub fn revive_user_token(conn: &Connection, id: UserId) -> StoreResult<bool> {
    let rows = conn.execute(
        "UPDATE users SET token_expired_at = NULL WHERE id = ?1",
        params![id.as_bytes()],
    )?;
    Ok(rows > 0)
}

/// Insert a human identity. `password_hash` is already Argon2id PHC (or
/// `None` for API-only brownfield-style rows). No API credential is created.
///
/// # Errors
/// Duplicate username/email, SQL.
pub fn insert_human_user(
    conn: &Connection,
    new_user: &NewUser,
    role: UserRole,
    password_hash: Option<&str>,
    must_change_password: bool,
) -> StoreResult<UserId> {
    let id = UserId::new();
    let now = Timestamp::now().as_microsecond();
    conn.execute(
        "INSERT INTO users \
         (id, username, name, email, created_at, role, password_hash, must_change_password) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id.as_bytes(),
            &new_user.username,
            &new_user.name,
            &new_user.email,
            now,
            role.as_str(),
            password_hash,
            i64::from(must_change_password),
        ],
    )
    .map_err(map_unique_violation)?;
    Ok(id)
}

/// One-shot root bootstrap. No-op error when already completed — the
/// caller must not invoke this after the marker is set.
///
/// # Errors
/// Duplicate / SQL / already completed.
pub fn bootstrap_root(
    conn: &mut Connection,
    username: &str,
    name: Option<&str>,
    email: Option<&str>,
    password_hash: &str,
) -> StoreResult<UserId> {
    let tx = conn.transaction()?;
    if bootstrap_completed(&tx)? {
        return Err(StoreError::InvalidState(
            "bootstrap already completed".into(),
        ));
    }
    let now = Timestamp::now().as_microsecond();
    let id = if let Some(existing) = find_user_by_username(&tx, username)? {
        tx.execute(
            "UPDATE users SET role = 'root', password_hash = ?1, must_change_password = 1, \
             disabled_at = NULL, name = COALESCE(?2, name), email = COALESCE(?3, email) \
             WHERE id = ?4",
            params![password_hash, name, email, existing.id.as_bytes()],
        )?;
        existing.id
    } else {
        let id = UserId::new();
        tx.execute(
            "INSERT INTO users \
             (id, username, name, email, created_at, role, password_hash, must_change_password) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'root', ?6, 1)",
            params![id.as_bytes(), username, name, email, now, password_hash],
        )
        .map_err(map_unique_violation)?;
        id
    };
    set_bootstrap_completed(&tx, true)?;
    tx.commit()?;
    Ok(id)
}

/// Break-glass root reset: create/promote `username`, write PHC with
/// `must_change_password=false`, revoke that identity's web sessions,
/// mark bootstrap complete. Does not touch API credentials.
///
/// # Errors
/// Duplicate / SQL.
pub fn recover_root(
    conn: &mut Connection,
    username: &str,
    name: Option<&str>,
    email: Option<&str>,
    password_hash: &str,
) -> StoreResult<UserId> {
    let tx = conn.transaction()?;
    let now = Timestamp::now().as_microsecond();
    let id = if let Some(existing) = find_user_by_username(&tx, username)? {
        tx.execute(
            "UPDATE users SET role = 'root', password_hash = ?1, must_change_password = 0, \
             disabled_at = NULL, name = COALESCE(?2, name), email = COALESCE(?3, email) \
             WHERE id = ?4",
            params![password_hash, name, email, existing.id.as_bytes()],
        )?;
        existing.id
    } else {
        let id = UserId::new();
        tx.execute(
            "INSERT INTO users \
             (id, username, name, email, created_at, role, password_hash, must_change_password) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'root', ?6, 0)",
            params![id.as_bytes(), username, name, email, now, password_hash],
        )
        .map_err(map_unique_violation)?;
        id
    };
    web_sessions::revoke_user_sessions(&tx, id, None)?;
    set_bootstrap_completed(&tx, true)?;
    tx.commit()?;
    Ok(id)
}

/// Replace the stored PHC, optionally force a password change, and revoke
/// sessions. Used by admin reset-password.
///
/// # Errors
/// Not found / SQL.
pub fn reset_human_password(
    conn: &mut Connection,
    user_id: UserId,
    password_hash: &str,
    must_change_password: bool,
) -> StoreResult<bool> {
    let tx = conn.transaction()?;
    let rows = tx.execute(
        "UPDATE users SET password_hash = ?1, must_change_password = ?2, disabled_at = disabled_at \
         WHERE id = ?3",
        params![
            password_hash,
            i64::from(must_change_password),
            user_id.as_bytes()
        ],
    )?;
    if rows == 0 {
        return Ok(false);
    }
    web_sessions::revoke_user_sessions(&tx, user_id, None)?;
    tx.commit()?;
    Ok(true)
}

/// Change password for the current live session: write PHC, clear
/// must_change, revoke other sessions, and rotate this session's hashes.
///
/// # Errors
/// Invalid state when the PHC changed, the user was disabled, or the
/// current session was revoked/expired concurrently.
pub fn change_password(
    conn: &mut Connection,
    user_id: UserId,
    expected_password_hash: &str,
    new_password_hash: &str,
    session_id: uuid::Uuid,
    new_session_hash: &[u8; TOKEN_HASH_LEN],
    new_csrf_hash: &[u8; TOKEN_HASH_LEN],
) -> StoreResult<bool> {
    let tx = conn.transaction()?;
    let current: Option<(Option<String>, Option<i64>)> = tx
        .query_row(
            "SELECT password_hash, disabled_at FROM users WHERE id = ?1",
            params![user_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match current {
        Some((Some(phc), None)) if phc == expected_password_hash => {}
        _ => {
            return Err(StoreError::InvalidState(
                "credentials changed concurrently".into(),
            ));
        }
    }
    let now = Timestamp::now().as_microsecond();
    let session_live: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM web_sessions \
         WHERE id = ?1 AND user_id = ?2 AND revoked_at IS NULL AND expires_at > ?3)",
        params![session_id.as_bytes(), user_id.as_bytes(), now],
        |row| row.get(0),
    )?;
    if !session_live {
        return Err(StoreError::InvalidState(
            "session changed concurrently".into(),
        ));
    }
    tx.execute(
        "UPDATE users SET password_hash = ?1, must_change_password = 0 WHERE id = ?2",
        params![new_password_hash, user_id.as_bytes()],
    )?;
    web_sessions::revoke_user_sessions(&tx, user_id, Some(session_id))?;
    if !web_sessions::rotate_web_session_secrets(&tx, session_id, new_session_hash, new_csrf_hash)?
    {
        return Err(StoreError::InvalidState(
            "session changed concurrently".into(),
        ));
    }
    tx.commit()?;
    Ok(true)
}

/// Recheck stored PHC/role/disabled then insert a session.
///
/// # Errors
/// Invalid state when the row no longer matches the verified PHC.
pub fn issue_web_session(
    conn: &mut Connection,
    user_id: UserId,
    expected_password_hash: &str,
    expected_role: UserRole,
    expected_must_change: bool,
    session_hash: &[u8; TOKEN_HASH_LEN],
    csrf_hash: &[u8; TOKEN_HASH_LEN],
) -> StoreResult<web_sessions::WebSession> {
    let tx = conn.transaction()?;
    let row = tx.query_row(
        "SELECT password_hash, role, must_change_password, disabled_at FROM users WHERE id = ?1",
        params![user_id.as_bytes()],
        |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        },
    );
    let (phc, role, must_change, disabled_at) = match row {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(StoreError::NotFound("user".into()));
        }
        Err(e) => return Err(e.into()),
    };
    if disabled_at.is_some()
        || phc.as_deref() != Some(expected_password_hash)
        || UserRole::parse(&role)? != expected_role
        || must_change != expected_must_change
    {
        return Err(StoreError::InvalidState(
            "credentials no longer valid".into(),
        ));
    }
    let session = web_sessions::insert_web_session(&tx, user_id, session_hash, csrf_hash)?;
    tx.commit()?;
    Ok(session)
}

/// Enable or disable human login. Disable revokes sessions. Never touches
/// API credentials. Refuses to disable the last recoverable root.
///
/// # Errors
/// Last-root invariant / not found / SQL.
pub fn set_user_disabled(
    conn: &mut Connection,
    user_id: UserId,
    disabled: bool,
) -> StoreResult<bool> {
    let tx = conn.transaction()?;
    let Some(user) = find_user_by_id(&tx, user_id)? else {
        return Ok(false);
    };
    if disabled && user.role == UserRole::Root && user.has_password && user.disabled_at.is_none() {
        ensure_not_last_recoverable_root(&tx, user_id)?;
    }
    let now = Timestamp::now().as_microsecond();
    let rows = if disabled {
        tx.execute(
            "UPDATE users SET disabled_at = COALESCE(disabled_at, ?1) WHERE id = ?2",
            params![now, user_id.as_bytes()],
        )?
    } else {
        tx.execute(
            "UPDATE users SET disabled_at = NULL WHERE id = ?1",
            params![user_id.as_bytes()],
        )?
    };
    if disabled {
        web_sessions::revoke_user_sessions(&tx, user_id, None)?;
    }
    tx.commit()?;
    Ok(rows > 0)
}

/// Patch name/email/role with a last-root recheck.
///
/// # Errors
/// Last-root invariant / not found / duplicate email / SQL.
pub fn patch_user(
    conn: &mut Connection,
    user_id: UserId,
    name: Option<Option<String>>,
    email: Option<Option<String>>,
    role: Option<UserRole>,
) -> StoreResult<bool> {
    let tx = conn.transaction()?;
    let Some(user) = find_user_by_id(&tx, user_id)? else {
        return Ok(false);
    };
    if let Some(UserRole::User) = role
        && user.role == UserRole::Root
        && user.has_password
        && user.disabled_at.is_none()
    {
        ensure_not_last_recoverable_root(&tx, user_id)?;
    }
    if let Some(name) = name {
        tx.execute(
            "UPDATE users SET name = ?1 WHERE id = ?2",
            params![name, user_id.as_bytes()],
        )?;
    }
    if let Some(email) = email {
        tx.execute(
            "UPDATE users SET email = ?1 WHERE id = ?2",
            params![email, user_id.as_bytes()],
        )
        .map_err(map_unique_violation)?;
    }
    if let Some(role) = role {
        tx.execute(
            "UPDATE users SET role = ?1 WHERE id = ?2",
            params![role.as_str(), user_id.as_bytes()],
        )?;
    }
    tx.commit()?;
    Ok(true)
}

/// Update `last_seen_at = now()`.
///
/// # Errors
/// SQL failures.
pub fn touch_user_last_seen(conn: &Connection, id: UserId) -> StoreResult<bool> {
    let now = Timestamp::now().as_microsecond();
    let rows = conn.execute(
        "UPDATE users SET last_seen_at = ?1 WHERE id = ?2",
        params![now, id.as_bytes()],
    )?;
    Ok(rows > 0)
}

/// Look up by username.
///
/// # Errors
/// SQL failures.
pub fn find_user_by_username(conn: &Connection, username: &str) -> StoreResult<Option<User>> {
    conn.query_row(
        "SELECT id, username, name, email, created_at, last_seen_at, \
                role, must_change_password, disabled_at, password_hash, token_expired_at \
         FROM users WHERE username = ?1",
        params![username],
        |row| row_to_user(row, 0),
    )
    .optional()
    .map_err(StoreError::from)
}

/// Look up by id, including disabled / password-less rows.
///
/// # Errors
/// SQL failures.
pub fn find_user_by_id(conn: &Connection, id: UserId) -> StoreResult<Option<User>> {
    conn.query_row(
        "SELECT id, username, name, email, created_at, last_seen_at, \
                role, must_change_password, disabled_at, password_hash, token_expired_at \
         FROM users WHERE id = ?1",
        params![id.as_bytes()],
        |row| row_to_user(row, 0),
    )
    .optional()
    .map_err(StoreError::from)
}

/// Login lookup including the PHC.
///
/// # Errors
/// SQL failures.
pub fn find_login_user_by_username(
    conn: &Connection,
    username: &str,
) -> StoreResult<Option<LoginUser>> {
    conn.query_row(
        "SELECT id, username, name, email, created_at, last_seen_at, \
                role, must_change_password, disabled_at, password_hash, token_expired_at \
         FROM users WHERE username = ?1",
        params![username],
        |row| {
            let password_hash: Option<String> = row.get(9)?;
            Ok(LoginUser {
                user: row_to_user(row, 0)?,
                password_hash,
            })
        },
    )
    .optional()
    .map_err(StoreError::from)
}

/// All users, created_at ascending.
///
/// # Errors
/// SQL failures.
pub fn list_users(conn: &Connection) -> StoreResult<Vec<User>> {
    let mut stmt = conn.prepare(
        "SELECT id, username, name, email, created_at, last_seen_at, \
                role, must_change_password, disabled_at, password_hash, token_expired_at \
         FROM users ORDER BY created_at ASC",
    )?;
    let rows = stmt
        .query_map([], |row| row_to_user(row, 0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Whether any user row exists.
///
/// # Errors
/// SQL failures.
pub fn users_exist(conn: &Connection) -> StoreResult<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM users)", [], |row| row.get(0))
        .map_err(StoreError::from)
}

/// Whether bootstrap has been marked complete.
///
/// # Errors
/// SQL failures.
pub fn bootstrap_completed(conn: &Connection) -> StoreResult<bool> {
    conn.query_row(
        "SELECT bootstrap_completed FROM human_auth_state WHERE id = 1",
        [],
        |row| {
            let v: i64 = row.get(0)?;
            Ok(v != 0)
        },
    )
    .map_err(StoreError::from)
}

/// Persist the bootstrap marker.
///
/// # Errors
/// SQL failures.
pub fn set_bootstrap_completed(conn: &Connection, completed: bool) -> StoreResult<()> {
    conn.execute(
        "UPDATE human_auth_state SET bootstrap_completed = ?1 WHERE id = 1",
        params![i64::from(completed)],
    )?;
    Ok(())
}

/// True when any user has a password hash.
///
/// # Errors
/// SQL failures.
pub fn any_password_hash(conn: &Connection) -> StoreResult<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM users WHERE password_hash IS NOT NULL)",
        [],
        |row| row.get(0),
    )
    .map_err(StoreError::from)
}

/// Recoverable roots: `role=root AND password_hash IS NOT NULL AND disabled_at IS NULL`.
///
/// # Errors
/// SQL failures.
pub fn count_recoverable_roots(conn: &Connection) -> StoreResult<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM users \
         WHERE role = 'root' AND password_hash IS NOT NULL AND disabled_at IS NULL",
        [],
        |row| row.get(0),
    )
    .map_err(StoreError::from)
}

fn ensure_not_last_recoverable_root(conn: &Connection, user_id: UserId) -> StoreResult<()> {
    let count = count_recoverable_roots(conn)?;
    if count <= 1 {
        let is_recoverable: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM users WHERE id = ?1 \
             AND role = 'root' AND password_hash IS NOT NULL AND disabled_at IS NULL)",
            params![user_id.as_bytes()],
            |row| row.get(0),
        )?;
        if is_recoverable {
            return Err(StoreError::InvalidState(
                "cannot demote or disable the last recoverable root".into(),
            ));
        }
    }
    Ok(())
}

/// Map SQLite UNIQUE-violation errors to a typed `Duplicate`.
pub(crate) fn map_unique_violation(e: rusqlite::Error) -> StoreError {
    if let rusqlite::Error::SqliteFailure(sqlite_err, msg) = &e
        && sqlite_err.code == rusqlite::ErrorCode::ConstraintViolation
    {
        let text = msg.clone().unwrap_or_else(|| sqlite_err.to_string());
        if text.contains("users.username") {
            return StoreError::Duplicate("username already taken".into());
        }
        if text.contains("users.email") {
            return StoreError::Duplicate("email already in use".into());
        }
        if text.contains("token_hash") {
            return StoreError::Duplicate("token hash collision".into());
        }
        return StoreError::Duplicate(text);
    }
    StoreError::from(e)
}

/// Decode a user row starting at `offset`. Relative columns 9 and 10 are
/// `password_hash` and the deprecated `token_expired_at`.
pub(crate) fn row_to_user(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<User> {
    let id_bytes: Vec<u8> = row.get(offset)?;
    let id = UserId::from_slice(&id_bytes).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            offset,
            rusqlite::types::Type::Blob,
            Box::new(std::io::Error::other(e.to_string())),
        )
    })?;
    let role_s: String = row.get(offset + 6)?;
    let role = UserRole::parse(&role_s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            offset + 6,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(e.to_string())),
        )
    })?;
    let must: i64 = row.get(offset + 7)?;
    let password_hash: Option<String> = row.get(offset + 9)?;
    Ok(User {
        id,
        username: row.get(offset + 1)?,
        name: row.get(offset + 2)?,
        email: row.get(offset + 3)?,
        created_at: row.get(offset + 4)?,
        last_seen_at: row.get(offset + 5)?,
        token_expired_at: row.get(offset + 10)?,
        role,
        must_change_password: must != 0,
        disabled_at: row.get(offset + 8)?,
        has_password: password_hash.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_credentials;
    use ai_memory_core::NewUser;
    use rusqlite::Connection;

    fn fresh_conn() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrations::run(&mut conn).expect("migrations::run");
        conn
    }

    fn sample(name: &str) -> NewUser {
        NewUser {
            username: name.to_string(),
            name: None,
            email: None,
        }
    }

    fn schema_version(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT MAX(version) FROM refinery_schema_history",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn assert_foreign_keys_clean(conn: &Connection) {
        let mut stmt = conn.prepare("PRAGMA foreign_key_check").unwrap();
        let violations: Vec<(String, i64, String, i64)> = stmt
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            violations.is_empty(),
            "PRAGMA foreign_key_check: {violations:?}"
        );
    }

    #[test]
    fn hash_token_is_deterministic_and_isolates_pepper() {
        let p = TokenPepper::new("test-pepper-1234567890abcdef");
        let h1 = hash_token("hunter2", &p);
        let h2 = hash_token("hunter2", &p);
        assert_eq!(h1, h2);
        let p2 = TokenPepper::new("different-pepper");
        assert_ne!(h1, hash_token("hunter2", &p2));
        assert_ne!(h1, hash_token("hunter3", &p));
    }

    #[test]
    fn insert_human_user_has_no_api_credential() {
        let conn = fresh_conn();
        let mut nu = sample("alice");
        nu.validate().unwrap();
        let id = insert_human_user(&conn, &nu, UserRole::User, Some("phc"), true).unwrap();
        let found = find_user_by_username(&conn, "alice").unwrap().unwrap();
        assert_eq!(found.id, id);
        assert!(found.has_password);
        assert!(found.must_change_password);
        assert_eq!(found.role, UserRole::User);
        assert!(
            api_credentials::list_api_credentials_for_user(&conn, id)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn last_recoverable_root_cannot_be_disabled_or_demoted() {
        let mut conn = fresh_conn();
        let mut nu = sample("root");
        nu.validate().unwrap();
        let id = insert_human_user(&conn, &nu, UserRole::Root, Some("phc"), false).unwrap();
        let err = set_user_disabled(&mut conn, id, true).unwrap_err();
        assert!(
            matches!(&err, StoreError::InvalidState(m) if m.contains("last recoverable root")),
            "{err:?}"
        );
        let err = patch_user(&mut conn, id, None, None, Some(UserRole::User)).unwrap_err();
        assert!(matches!(err, StoreError::InvalidState(_)));
    }

    #[test]
    fn second_root_allows_demoting_the_first() {
        let mut conn = fresh_conn();
        let mut a = sample("root");
        a.validate().unwrap();
        let first = insert_human_user(&conn, &a, UserRole::Root, Some("phc"), false).unwrap();
        let mut b = sample("boss");
        b.validate().unwrap();
        insert_human_user(&conn, &b, UserRole::Root, Some("phc2"), false).unwrap();
        assert!(patch_user(&mut conn, first, None, None, Some(UserRole::User)).unwrap());
        assert_eq!(
            find_user_by_id(&conn, first).unwrap().unwrap().role,
            UserRole::User
        );
    }

    #[test]
    fn recovery_creates_root_and_marks_bootstrap() {
        let mut conn = fresh_conn();
        assert!(!bootstrap_completed(&conn).unwrap());
        let id = recover_root(&mut conn, "root", None, None, "phc").unwrap();
        let user = find_user_by_id(&conn, id).unwrap().unwrap();
        assert_eq!(user.role, UserRole::Root);
        assert!(user.has_password);
        assert!(!user.must_change_password);
        assert!(bootstrap_completed(&conn).unwrap());
    }

    #[test]
    fn bootstrap_root_is_one_shot() {
        let mut conn = fresh_conn();
        bootstrap_root(&mut conn, "root", None, None, "phc-one").unwrap();
        let err = bootstrap_root(&mut conn, "root", None, None, "phc-two").unwrap_err();
        assert!(
            matches!(&err, StoreError::InvalidState(m) if m.contains("already completed")),
            "{err:?}"
        );
        let login = find_login_user_by_username(&conn, "root").unwrap().unwrap();
        assert_eq!(login.password_hash.as_deref(), Some("phc-one"));
        assert!(login.user.must_change_password);
    }

    #[test]
    fn token_only_row_is_not_a_recoverable_root() {
        let mut conn = fresh_conn();
        let mut nu = sample("legacy");
        nu.validate().unwrap();
        let id = insert_human_user(&conn, &nu, UserRole::Root, None, false).unwrap();
        let found = find_user_by_id(&conn, id).unwrap().unwrap();
        assert!(!found.has_password);
        assert_eq!(count_recoverable_roots(&conn).unwrap(), 0);
        assert!(set_user_disabled(&mut conn, id, true).unwrap());
    }

    #[test]
    fn password_change_rejects_a_revoked_current_session_without_updating_phc() {
        let mut conn = fresh_conn();
        let mut nu = sample("alice");
        nu.validate().unwrap();
        let id = insert_human_user(&conn, &nu, UserRole::User, Some("old-phc"), true).unwrap();
        let session =
            web_sessions::insert_web_session(&conn, id, &[1; TOKEN_HASH_LEN], &[2; TOKEN_HASH_LEN])
                .unwrap();
        web_sessions::revoke_session_by_hash(&conn, &[1; TOKEN_HASH_LEN]).unwrap();

        assert!(
            change_password(
                &mut conn,
                id,
                "old-phc",
                "new-phc",
                session.id,
                &[3; TOKEN_HASH_LEN],
                &[4; TOKEN_HASH_LEN],
            )
            .is_err()
        );
        let login = find_login_user_by_username(&conn, "alice")
            .unwrap()
            .unwrap();
        assert_eq!(login.password_hash.as_deref(), Some("old-phc"));
        assert!(login.user.must_change_password);
    }

    #[test]
    fn session_issuance_rechecks_role_and_must_change_state() {
        let mut conn = fresh_conn();
        let mut nu = sample("alice");
        nu.validate().unwrap();
        let id = insert_human_user(&conn, &nu, UserRole::User, Some("phc"), true).unwrap();

        conn.execute(
            "UPDATE users SET role = 'root' WHERE id = ?1",
            params![id.as_bytes()],
        )
        .unwrap();
        assert!(
            issue_web_session(
                &mut conn,
                id,
                "phc",
                UserRole::User,
                true,
                &[1; TOKEN_HASH_LEN],
                &[2; TOKEN_HASH_LEN],
            )
            .is_err()
        );

        conn.execute(
            "UPDATE users SET role = 'user', must_change_password = 0 WHERE id = ?1",
            params![id.as_bytes()],
        )
        .unwrap();
        assert!(
            issue_web_session(
                &mut conn,
                id,
                "phc",
                UserRole::User,
                true,
                &[3; TOKEN_HASH_LEN],
                &[4; TOKEN_HASH_LEN],
            )
            .is_err()
        );
        assert!(
            issue_web_session(
                &mut conn,
                id,
                "phc",
                UserRole::User,
                false,
                &[5; TOKEN_HASH_LEN],
                &[6; TOKEN_HASH_LEN],
            )
            .is_ok()
        );
    }

    #[test]
    fn human_auth_v54_migrates_token_only_user() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrations::run_to(&mut conn, 53).unwrap();
        let id = UserId::new();
        let hash = [7u8; 32];
        conn.execute(
            "INSERT INTO users (id, username, name, email, token_hash, created_at) \
             VALUES (?1, 'alice', NULL, NULL, ?2, 1)",
            params![id.as_bytes(), hash.as_slice()],
        )
        .unwrap();
        crate::migrations::run_to(&mut conn, 54).unwrap();
        let (username, role, token, bootstrap): (String, String, Vec<u8>, i64) = conn
            .query_row(
                "SELECT u.username, u.role, u.token_hash, s.bootstrap_completed \
                 FROM users u, human_auth_state s WHERE u.username = 'alice'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(username, "alice");
        assert_eq!(role, "user");
        assert_eq!(token, hash);
        assert_eq!(bootstrap, 0);
        crate::migrations::run(&mut conn).unwrap();
        let user = find_user_by_id(&conn, id).unwrap().unwrap();
        assert!(!user.has_password);
        assert_eq!(user.username, "alice");
    }

    #[test]
    fn human_auth_v54_empty_store_leaves_bootstrap_incomplete() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrations::run_to(&mut conn, 53).unwrap();
        // SQLite ignores PRAGMA foreign_keys inside refinery's transaction;
        // Store::open disables FKs around migrations for the same reason.
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        crate::migrations::run_to(&mut conn, 54).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        assert_eq!(schema_version(&conn), 54);
        assert!(!users_exist(&conn).unwrap());
        assert!(!bootstrap_completed(&conn).unwrap());
        assert_foreign_keys_clean(&conn);
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        crate::migrations::run(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        assert!(!bootstrap_completed(&conn).unwrap());
        assert!(!users_exist(&conn).unwrap());
        assert_foreign_keys_clean(&conn);
    }

    #[test]
    fn human_auth_v54_rejects_orphaned_page_author_ids() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrations::run_to(&mut conn, 53).unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();

        let workspace = [1u8; 16];
        let project = [2u8; 16];
        let page = [3u8; 16];
        let missing_author = UserId::new();
        conn.execute(
            "INSERT INTO workspaces (id, name, created_at) VALUES (?1, 'ws', 1)",
            params![workspace.as_slice()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO projects (id, workspace_id, name, created_at) \
             VALUES (?1, ?2, 'p', 1)",
            params![project.as_slice(), workspace.as_slice()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pages \
             (id, workspace_id, project_id, path, title, tier, body, body_sha256, \
              frontmatter_json, is_latest, pinned, created_at, updated_at, author_id) \
             VALUES (?1, ?2, ?3, 'notes/orphan.md', 'title', 'semantic', 'body', ?4, '{}', 1, 0, 1, 1, ?5)",
            params![
                page.as_slice(),
                workspace.as_slice(),
                project.as_slice(),
                [0u8; 32].as_slice(),
                missing_author.as_bytes(),
            ],
        )
        .unwrap();

        let error = crate::migrations::run_to(&mut conn, 54)
            .expect_err("V52 must abort rather than preserve an orphaned page author");
        assert!(
            error.to_string().contains("CHECK constraint failed"),
            "unexpected V52 guard error: {error}"
        );
        assert_eq!(schema_version(&conn), 53, "failed V54 must roll back");
    }

    #[test]
    fn human_auth_v54_ignores_unrelated_preexisting_foreign_key_violations() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrations::run_to(&mut conn, 53).unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        conn.execute(
            "INSERT INTO projects (id, workspace_id, name, created_at) \
             VALUES (?1, ?2, 'preexisting-orphan', 1)",
            params![[1u8; 16].as_slice(), [2u8; 16].as_slice()],
        )
        .unwrap();

        crate::migrations::run_to(&mut conn, 54)
            .expect("V52 must guard only references affected by its users-table rebuild");
        assert_eq!(schema_version(&conn), 54);
    }

    #[test]
    fn human_auth_v54_preserves_page_and_audit_author_ids() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrations::run_to(&mut conn, 53).unwrap();
        let author = UserId::new();
        let hash = [9u8; 32];
        conn.execute(
            "INSERT INTO users (id, username, name, email, token_hash, created_at) \
             VALUES (?1, 'alice', NULL, NULL, ?2, 1)",
            params![author.as_bytes(), hash.as_slice()],
        )
        .unwrap();
        let workspace = [1u8; 16];
        let project = [2u8; 16];
        let page = [3u8; 16];
        let body_hash = [0u8; 32];
        conn.execute(
            "INSERT INTO workspaces (id, name, created_at) VALUES (?1, 'ws', 1)",
            params![workspace.as_slice()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO projects (id, workspace_id, name, created_at) \
             VALUES (?1, ?2, 'p', 1)",
            params![project.as_slice(), workspace.as_slice()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pages \
             (id, workspace_id, project_id, path, title, tier, body, body_sha256, \
              frontmatter_json, is_latest, pinned, created_at, updated_at, author_id) \
             VALUES (?1, ?2, ?3, 'notes/a.md', 'title', 'semantic', 'body', ?4, '{}', 1, 0, 1, 1, ?5)",
            params![
                page.as_slice(),
                workspace.as_slice(),
                project.as_slice(),
                body_hash.as_slice(),
                author.as_bytes(),
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO audit_log (at, op, workspace_id, project_id, page_id, detail, author_id) \
             VALUES (1, 'upsert', ?1, ?2, ?3, '{}', ?4)",
            params![
                workspace.as_slice(),
                project.as_slice(),
                page.as_slice(),
                author.as_bytes(),
            ],
        )
        .unwrap();

        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        crate::migrations::run_to(&mut conn, 54).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        assert_eq!(schema_version(&conn), 54);
        let (user_id, role): (Vec<u8>, String) = conn
            .query_row(
                "SELECT id, role FROM users WHERE username = 'alice'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(user_id, author.as_bytes());
        assert_eq!(role, "user");
        let page_author: Vec<u8> = conn
            .query_row(
                "SELECT author_id FROM pages WHERE id = ?1",
                params![page.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        let audit_author: Vec<u8> = conn
            .query_row(
                "SELECT author_id FROM audit_log WHERE page_id = ?1",
                params![page.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(page_author, author.as_bytes());
        assert_eq!(audit_author, author.as_bytes());
        assert_foreign_keys_clean(&conn);

        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        crate::migrations::run(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let page_author: Vec<u8> = conn
            .query_row(
                "SELECT author_id FROM pages WHERE id = ?1",
                params![page.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        let audit_author: Vec<u8> = conn
            .query_row(
                "SELECT author_id FROM audit_log WHERE page_id = ?1",
                params![page.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(page_author, author.as_bytes());
        assert_eq!(audit_author, author.as_bytes());
        let user = find_user_by_id(&conn, author).unwrap().unwrap();
        assert_eq!(user.id, author);
        assert_eq!(user.username, "alice");
        assert_foreign_keys_clean(&conn);
    }
}
