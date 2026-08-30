//! Argon2id password hashing, always off the writer thread.
//!
//! Human passwords use the crate-default Argon2id PHC string. The KDF
//! runs in `spawn_blocking` behind a two-permit semaphore so a login
//! flood cannot stall the SQLite writer. Unknown and disabled users
//! still pay a dummy verify so the timing of "no such user" matches a
//! real miss.

use std::sync::LazyLock;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use base64::Engine;
use tokio::sync::Semaphore;

use crate::error::{StoreError, StoreResult};

/// Concurrent Argon2id slots. Login IP rate-limit fires first; this
/// bound prevents password work from saturating the blocking pool.
const KDF_PERMITS: usize = 2;
/// Active plus queued KDF jobs. Exceeding this bound returns HTTP 429.
const KDF_PENDING_LIMIT: usize = 32;

static KDF_SLOTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(KDF_PERMITS));
static KDF_PENDING: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(KDF_PENDING_LIMIT));

fn dummy_phc() -> &'static str {
    "$argon2id$v=19$m=19456,t=2,p=1$YWktbWVtb3J5LWR1bW15$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
}

fn hash_password_sync(password: &str) -> StoreResult<String> {
    let mut salt_bytes = [0u8; 16];
    getrandom::fill(&mut salt_bytes)
        .map_err(|e| StoreError::Os(format!("os csprng failed during password salt: {e}")))?;
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|e| StoreError::InvalidState(format!("password salt: {e}")))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| StoreError::InvalidState(format!("password hash: {e}")))
}

fn verify_password_sync(password: &str, phc: &str) -> StoreResult<bool> {
    let parsed = PasswordHash::new(phc)
        .map_err(|e| StoreError::MalformedRecord(format!("password hash: {e}")))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

async fn with_kdf_slot<T, F>(work: F) -> StoreResult<T>
where
    F: FnOnce() -> StoreResult<T> + Send + 'static,
    T: Send + 'static,
{
    let pending_permit = KDF_PENDING
        .try_acquire()
        .map_err(|_| StoreError::InvalidState("kdf saturated".into()))?;
    let slot_permit = KDF_SLOTS
        .acquire()
        .await
        .map_err(|_| StoreError::InvalidState("kdf unavailable".into()))?;
    let result = tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| StoreError::PoolPanic(format!("password kdf task: {e}")))?;
    drop(slot_permit);
    drop(pending_permit);
    result
}

/// Hash `password` to a PHC string. Runs off the writer thread.
///
/// # Errors
/// [`StoreError::InvalidState`] when the KDF queue is saturated (HTTP 429)
/// or hashing fails. [`StoreError::Os`] when the CSPRNG fails.
pub async fn hash_password(password: String) -> StoreResult<String> {
    with_kdf_slot(move || hash_password_sync(&password)).await
}

/// Verify `password` against a stored PHC string. Runs off the writer thread.
///
/// # Errors
/// [`StoreError::InvalidState`] when saturated; [`StoreError::MalformedRecord`]
/// when the stored PHC cannot be parsed.
pub async fn verify_password(password: String, phc: String) -> StoreResult<bool> {
    with_kdf_slot(move || verify_password_sync(&password, &phc)).await
}

/// Always run a KDF against the process-wide dummy PHC so unknown and
/// disabled logins spend the same time as a real miss.
///
/// # Errors
/// Same as [`verify_password`].
pub async fn dummy_verify(password: String) -> StoreResult<()> {
    let phc = dummy_phc().to_string();
    let _ = verify_password(password, phc).await?;
    Ok(())
}

/// 24 CSPRNG bytes encoded as 32 URL-safe characters. Callers still run
/// [`ai_memory_core::validate_human_password`] and retry on collision.
///
/// # Errors
/// [`StoreError::Os`] when the CSPRNG fails.
pub fn generate_temporary_password() -> StoreResult<String> {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes)
        .map_err(|e| StoreError::Os(format!("os csprng failed during temporary password: {e}")))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_password_sync_round_trips() {
        let phc = hash_password_sync("twelve chars!!").unwrap();
        assert!(phc.starts_with("$argon2id$"));
        assert!(verify_password_sync("twelve chars!!", &phc).unwrap());
        assert!(!verify_password_sync("twelve chars??", &phc).unwrap());
    }

    #[test]
    fn dummy_phc_is_valid_and_never_matches_login_input() {
        assert!(!verify_password_sync("anything", dummy_phc()).unwrap());
    }

    #[test]
    fn generate_temporary_password_is_url_safe_32() {
        let pw = generate_temporary_password().unwrap();
        assert_eq!(pw.len(), 32);
        assert!(
            pw.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }
}
