//! An API key read from a file the operator keeps current.
//!
//! Gateways that hand out short-lived bearer tokens (a login CLI rewriting a
//! token file every day, a secrets agent rendering one) rotate the key under
//! a running server. The file is read on first use and the key cached; after
//! the endpoint answers `401`, the file is read once more and the caller
//! retries only when the key in it changed. A file that still holds the
//! rejected key costs no second request, so a stale token never turns into a
//! retry loop.

use std::path::{Path, PathBuf};

use secrecy::{ExposeSecret as _, SecretString};
use tokio::sync::Mutex;

use crate::error::{LlmError, LlmResult};

/// API key source backed by an operator-maintained file.
pub(crate) struct ApiKeyFile {
    path: PathBuf,
    cached: Mutex<Option<SecretString>>,
}

impl ApiKeyFile {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            cached: Mutex::new(None),
        }
    }

    /// The cached key, reading the file when nothing is cached yet.
    pub(crate) async fn current(&self) -> LlmResult<SecretString> {
        let mut cached = self.cached.lock().await;
        if let Some(key) = cached.as_ref() {
            return Ok(key.clone());
        }
        let key = read_key(&self.path)?;
        *cached = Some(key.clone());
        Ok(key)
    }

    /// Re-read the file after the endpoint refused `rejected`. Returns the
    /// new key only when it differs; `None` means the file still holds the
    /// rejected key, and the caller must not retry.
    pub(crate) async fn rotated(&self, rejected: &SecretString) -> LlmResult<Option<SecretString>> {
        let key = read_key(&self.path)?;
        if key.expose_secret() == rejected.expose_secret() {
            return Ok(None);
        }
        *self.cached.lock().await = Some(key.clone());
        Ok(Some(key))
    }
}

/// Read and trim the key. Errors name the file, never its contents.
fn read_key(path: &Path) -> LlmResult<SecretString> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| LlmError::Auth(format!("read API key file {}: {e}", path.display())))?;
    let key = raw.trim();
    if key.is_empty() {
        return Err(LlmError::Auth(format!(
            "API key file {} is empty",
            path.display()
        )));
    }
    Ok(SecretString::from(key.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Caching, rotation and a missing file are covered end to end in
    // tests/suite/openai_compat_api_key_file.rs; this is the parsing.
    #[test]
    fn the_key_is_trimmed_and_a_blank_file_is_an_error_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");

        std::fs::write(&path, "  key-a\r\n").unwrap();
        assert_eq!(read_key(&path).unwrap().expose_secret(), "key-a");

        std::fs::write(&path, " \n").unwrap();
        let err = read_key(&path).unwrap_err();
        assert!(matches!(err, LlmError::Auth(_)), "{err:?}");
        assert!(err.to_string().contains(&path.display().to_string()));
    }
}
