//! Web sessions: SHA-256 of `ams_…` secrets (no pepper) plus CSRF hashes.

use ai_memory_core::{SESSION_SECRET_PREFIX, User, UserId};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{StoreError, StoreResult};
use crate::users::{TOKEN_HASH_LEN, TOKEN_RAW_LEN, row_to_user};

/// Absolute session lifetime (seven days).
pub const SESSION_TTL_MICROS: i64 = 7 * 24 * 60 * 60 * 1_000_000;
/// Skip `last_used_at` writes more often than once per minute.
pub const SESSION_TOUCH_MIN_MICROS: i64 = 60 * 1_000_000;

/// SHA-256 of a session secret or CSRF value. No pepper — the secret is
/// already 256 bits of CSPRNG with a reserved prefix.
#[must_use]
pub fn hash_session_secret(secret: &str) -> [u8; TOKEN_HASH_LEN] {
    Sha256::digest(secret.as_bytes()).into()
}

/// `ams_` + 43 URL-safe characters.
///
/// # Errors
/// [`StoreError::Os`] when the CSPRNG fails.
pub fn generate_session_secret() -> StoreResult<String> {
    let mut bytes = [0u8; TOKEN_RAW_LEN];
    getrandom::fill(&mut bytes)
        .map_err(|e| StoreError::Os(format!("os csprng failed during session secret: {e}")))?;
    Ok(format!(
        "{SESSION_SECRET_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(bytes)
    ))
}

/// 32 URL-safe characters for the readable CSRF cookie.
///
/// # Errors
/// [`StoreError::Os`] when the CSPRNG fails.
pub fn generate_csrf_secret() -> StoreResult<String> {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes)
        .map_err(|e| StoreError::Os(format!("os csprng failed during csrf secret: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Issued session row after a successful login/password rotation.
#[derive(Debug, Clone)]
pub struct WebSession {
    /// Session row id.
    pub id: Uuid,
    /// Owning user.
    pub user_id: UserId,
    /// Absolute expiry (microseconds since epoch).
    pub expires_at: i64,
}

/// Live session plus the owning user, used by `/auth/me` and dual-auth.
#[derive(Debug, Clone)]
pub struct LiveWebSession {
    /// Session row id.
    pub id: Uuid,
    /// CSRF hash stored on the row.
    pub csrf_hash: [u8; TOKEN_HASH_LEN],
    /// Absolute expiry.
    pub expires_at: i64,
    /// Last persisted touch.
    pub last_used_at: i64,
    /// Owning user.
    pub user: User,
}

/// Insert a new session. Caller hashed the secret and CSRF off-thread.
///
/// # Errors
/// SQL / unique-hash collisions.
pub fn insert_web_session(
    conn: &Connection,
    user_id: UserId,
    session_hash: &[u8; TOKEN_HASH_LEN],
    csrf_hash: &[u8; TOKEN_HASH_LEN],
) -> StoreResult<WebSession> {
    let id = Uuid::now_v7();
    let created_at = Timestamp::now().as_microsecond();
    let expires_at = created_at.saturating_add(SESSION_TTL_MICROS);
    conn.execute(
        "INSERT INTO web_sessions \
         (id, session_hash, user_id, csrf_hash, created_at, last_used_at, expires_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
        params![
            id.as_bytes().as_slice(),
            session_hash.as_slice(),
            user_id.as_bytes(),
            csrf_hash.as_slice(),
            created_at,
            expires_at,
        ],
    )?;
    Ok(WebSession {
        id,
        user_id,
        expires_at,
    })
}

/// Replace the current session's secret and CSRF hashes (password change).
///
/// # Errors
/// SQL failures.
pub fn rotate_web_session_secrets(
    conn: &Connection,
    session_id: Uuid,
    session_hash: &[u8; TOKEN_HASH_LEN],
    csrf_hash: &[u8; TOKEN_HASH_LEN],
) -> StoreResult<bool> {
    let rows = conn.execute(
        "UPDATE web_sessions SET session_hash = ?1, csrf_hash = ?2 \
         WHERE id = ?3 AND revoked_at IS NULL",
        params![
            session_hash.as_slice(),
            csrf_hash.as_slice(),
            session_id.as_bytes().as_slice(),
        ],
    )?;
    Ok(rows > 0)
}

/// Revoke every session for `user_id`, optionally keeping `except`.
///
/// # Errors
/// SQL failures.
pub fn revoke_user_sessions(
    conn: &Connection,
    user_id: UserId,
    except: Option<Uuid>,
) -> StoreResult<u64> {
    let now = Timestamp::now().as_microsecond();
    let rows = if let Some(keep) = except {
        conn.execute(
            "UPDATE web_sessions SET revoked_at = COALESCE(revoked_at, ?1) \
             WHERE user_id = ?2 AND id != ?3",
            params![now, user_id.as_bytes(), keep.as_bytes().as_slice()],
        )?
    } else {
        conn.execute(
            "UPDATE web_sessions SET revoked_at = COALESCE(revoked_at, ?1) \
             WHERE user_id = ?2",
            params![now, user_id.as_bytes()],
        )?
    };
    Ok(rows as u64)
}

/// Revoke one session by hash (logout).
///
/// # Errors
/// SQL failures.
pub fn revoke_session_by_hash(
    conn: &Connection,
    session_hash: &[u8; TOKEN_HASH_LEN],
) -> StoreResult<bool> {
    let now = Timestamp::now().as_microsecond();
    let rows = conn.execute(
        "UPDATE web_sessions SET revoked_at = COALESCE(revoked_at, ?1) \
         WHERE session_hash = ?2",
        params![now, session_hash.as_slice()],
    )?;
    Ok(rows > 0)
}

/// Look up a live (unrevoked, unexpired) session by secret hash.
///
/// # Errors
/// SQL failures.
pub fn find_live_session_by_hash(
    conn: &Connection,
    session_hash: &[u8; TOKEN_HASH_LEN],
    now: i64,
) -> StoreResult<Option<LiveWebSession>> {
    conn.query_row(
        "SELECT s.id, s.csrf_hash, s.expires_at, s.last_used_at, \
                u.id, u.username, u.name, u.email, u.created_at, u.last_seen_at, \
                u.role, u.must_change_password, u.disabled_at, u.password_hash, \
                u.token_expired_at \
         FROM web_sessions s \
         JOIN users u ON u.id = s.user_id \
         WHERE s.session_hash = ?1 AND s.revoked_at IS NULL AND s.expires_at > ?2",
        params![session_hash.as_slice(), now],
        |row| {
            let id_bytes: Vec<u8> = row.get(0)?;
            let id = Uuid::from_slice(&id_bytes).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Blob,
                    Box::new(std::io::Error::other(e.to_string())),
                )
            })?;
            let csrf: Vec<u8> = row.get(1)?;
            let csrf_hash: [u8; TOKEN_HASH_LEN] = csrf.as_slice().try_into().map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Blob,
                    Box::new(std::io::Error::other("csrf_hash must be 32 bytes")),
                )
            })?;
            Ok(LiveWebSession {
                id,
                csrf_hash,
                expires_at: row.get(2)?,
                last_used_at: row.get(3)?,
                user: row_to_user(row, 4)?,
            })
        },
    )
    .optional()
    .map_err(StoreError::from)
}

/// Touch `last_used_at` at most once per minute.
///
/// # Errors
/// SQL failures.
pub fn touch_web_session(
    conn: &Connection,
    session_id: Uuid,
    last_used_at: i64,
) -> StoreResult<bool> {
    let rows = conn.execute(
        "UPDATE web_sessions SET last_used_at = ?1 \
         WHERE id = ?2 AND (?1 - last_used_at) >= ?3",
        params![
            last_used_at,
            session_id.as_bytes().as_slice(),
            SESSION_TOUCH_MIN_MICROS
        ],
    )?;
    Ok(rows > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_core::SESSION_SECRET_PREFIX;
    use sha2::{Digest, Sha256};

    #[test]
    fn generate_session_secret_uses_ams_prefix() {
        let secret = generate_session_secret().unwrap();
        assert!(secret.starts_with(SESSION_SECRET_PREFIX), "{secret}");
        assert_eq!(secret.len(), SESSION_SECRET_PREFIX.len() + 43);
    }

    #[test]
    fn hash_session_secret_is_unpeppered_sha256() {
        let secret = "ams_not-a-real-session-secret";
        let got = hash_session_secret(secret);
        let expected: [u8; TOKEN_HASH_LEN] = Sha256::digest(secret.as_bytes()).into();
        assert_eq!(got, expected);
    }
}
