//! Shared JSON auth-file helpers for token-backed providers.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Serialize, de::DeserializeOwned};

use crate::error::{LlmError, LlmResult};

pub(crate) fn load_entry<T>(path: &Path, key: &str) -> LlmResult<Option<T>>
where
    T: DeserializeOwned,
{
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)
        .map_err(|e| LlmError::Auth(format!("read auth file {}: {e}", path.display())))?;
    let mut value = serde_json::from_slice::<serde_json::Value>(&bytes)
        .map_err(|e| LlmError::Auth(format!("parse auth file {}: {e}", path.display())))?;
    let Some(entry) = value.as_object_mut().and_then(|obj| obj.remove(key)) else {
        return Ok(None);
    };
    if entry.get("type").and_then(serde_json::Value::as_str) != Some("oauth") {
        return Ok(None);
    }
    serde_json::from_value::<T>(entry)
        .map(Some)
        .map_err(|e| LlmError::Auth(format!("parse {key} auth entry: {e}")))
}

pub(crate) fn save_entry<T>(path: &Path, key: &str, entry: Option<T>) -> LlmResult<()>
where
    T: Serialize,
{
    let mut root = if path.exists() {
        let bytes = std::fs::read(path)
            .map_err(|e| LlmError::Auth(format!("read auth file {}: {e}", path.display())))?;
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|e| LlmError::Auth(format!("parse auth file {}: {e}", path.display())))?
    } else {
        serde_json::json!({})
    };
    if !root.is_object() {
        return Err(LlmError::Auth(format!(
            "auth file {} must contain a JSON object",
            path.display()
        )));
    }
    let Some(obj) = root.as_object_mut() else {
        return Err(LlmError::Auth(format!(
            "auth file {} must contain a JSON object",
            path.display()
        )));
    };
    match entry {
        Some(entry) => {
            obj.insert(key.to_string(), serde_json::to_value(entry)?);
        }
        None => {
            obj.remove(key);
        }
    }
    if obj.is_empty() {
        if path.exists() {
            std::fs::remove_file(path)
                .map_err(|e| LlmError::Auth(format!("remove auth file {}: {e}", path.display())))?;
        }
        return Ok(());
    }
    write_auth_file(path, &root)
}

/// Atomic write for credential files, mirroring the canonical
/// `ai_memory_wiki::write_atomic` dance (write tmp → fsync → rename)
/// with two deliberate differences: a predictable `<name>.json.tmp`
/// sibling name, and chmod 0600 on the tempfile *before* any bytes land
/// so auth material is never group/world-readable, even transiently.
///
/// `ai-memory-llm` deliberately does not depend on `ai-memory-wiki`
/// (git2/notify are too heavy to pull in for a write helper), so this
/// stays a copy — keep it in step with the canonical implementation.
fn write_auth_file(path: &Path, value: &serde_json::Value) -> LlmResult<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| LlmError::Auth(format!("create auth dir {}: {e}", parent.display())))?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value)?;
    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)
            .map_err(|e| LlmError::Auth(format!("open auth tmp {}: {e}", tmp.display())))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|e| LlmError::Auth(format!("chmod auth tmp {}: {e}", tmp.display())))?;
        }
        file.write_all(&bytes)
            .map_err(|e| LlmError::Auth(format!("write auth tmp {}: {e}", tmp.display())))?;
        file.sync_all()
            .map_err(|e| LlmError::Auth(format!("fsync auth tmp {}: {e}", tmp.display())))?;
    }
    std::fs::rename(&tmp, path)
        .map_err(|e| LlmError::Auth(format!("rename auth file {}: {e}", path.display())))?;
    Ok(())
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;

    /// Mirrors the entry shape stored by `openai_oauth` / `oidc` callers.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct TestEntry {
        #[serde(rename = "type")]
        kind: String,
        access: String,
        refresh: String,
        expires: u64,
    }

    fn sample_entry() -> TestEntry {
        TestEntry {
            kind: "oauth".into(),
            access: "access-x".into(),
            refresh: "refresh-y".into(),
            expires: 1_700_000_000_000,
        }
    }

    #[test]
    fn round_trip_preserves_entry_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        save_entry(&path, "test", Some(sample_entry())).unwrap();
        let loaded = load_entry::<TestEntry>(&path, "test").unwrap();
        assert_eq!(loaded, Some(sample_entry()));
    }

    #[test]
    fn load_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let loaded = load_entry::<TestEntry>(&path, "test").unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn load_malformed_json_returns_auth_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(&path, b"not json {{{").unwrap();
        let err = load_entry::<TestEntry>(&path, "test").unwrap_err();
        assert!(
            matches!(err, LlmError::Auth(ref msg) if msg.contains("parse auth file")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn load_returns_none_when_key_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        save_entry(&path, "other", Some(sample_entry())).unwrap();
        let loaded = load_entry::<TestEntry>(&path, "test").unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn load_returns_none_for_non_oauth_entry_type() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "test": { "type": "api_key", "access": "a", "refresh": "b", "expires": 1 }
            }))
            .unwrap(),
        )
        .unwrap();
        let loaded = load_entry::<TestEntry>(&path, "test").unwrap();
        assert!(loaded.is_none(), "non-oauth entries must be ignored");
    }

    #[test]
    fn load_errors_when_oauth_entry_has_wrong_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "test": { "type": "oauth", "access": "a" }
            }))
            .unwrap(),
        )
        .unwrap();
        let err = load_entry::<TestEntry>(&path, "test").unwrap_err();
        assert!(
            matches!(err, LlmError::Auth(ref msg) if msg.contains("parse test auth entry")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn save_preserves_sibling_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        save_entry(&path, "other", Some(sample_entry())).unwrap();
        save_entry(&path, "test", Some(sample_entry())).unwrap();
        let loaded_other = load_entry::<TestEntry>(&path, "other").unwrap();
        let loaded_test = load_entry::<TestEntry>(&path, "test").unwrap();
        assert_eq!(loaded_other, Some(sample_entry()));
        assert_eq!(loaded_test, Some(sample_entry()));
    }

    #[test]
    fn save_rejects_non_object_auth_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(&path, b"[1, 2, 3]").unwrap();
        let err = save_entry(&path, "test", Some(sample_entry())).unwrap_err();
        assert!(
            matches!(err, LlmError::Auth(ref msg) if msg.contains("must contain a JSON object")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn removing_last_entry_deletes_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        save_entry(&path, "test", Some(sample_entry())).unwrap();
        assert!(path.exists());
        save_entry::<TestEntry>(&path, "test", None).unwrap();
        assert!(!path.exists(), "last-entry removal deletes the auth file");
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_has_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        save_entry(&path, "test", Some(sample_entry())).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "auth file must be owner-read/write only");
    }

    #[test]
    fn successful_save_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        save_entry(&path, "test", Some(sample_entry())).unwrap();
        let tmp = path.with_extension("json.tmp");
        assert!(!tmp.exists(), "leftover tmp file: {}", tmp.display());
    }

    #[test]
    fn now_ms_returns_plausible_epoch_millis() {
        let ms = now_ms();
        assert!(ms > 1_000_000_000_000, "expected epoch millis, got {ms}");
    }
}
