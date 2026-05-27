//! Unified on-disk token store for OAuth / device-flow credentials.
//!
//! Both OpenAI OAuth and GitHub Copilot tokens live in a single
//! `oauth_token.json` under different keys.  Users manage one file, one
//! `chmod 0600`.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{LlmError, LlmResult};

/// Per-provider OAuth entry stored in the unified token file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthEntry {
    /// Always `"oauth"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Short-lived access token.  JWT for OpenAI; `gho_…` for Copilot.
    pub access: String,
    /// Long-lived refresh token.  For Copilot this equals `access` (GitHub
    /// tokens do not expire and there is no separate refresh token).
    pub refresh: String,
    /// Expiry in **milliseconds** since Unix epoch. `0` = non-expiring.
    pub expires: u64,
    /// OpenAI account UUID, present on OpenAI entries only.
    #[serde(rename = "accountId", skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

/// All OAuth / device-flow tokens stored in one file.
///
/// Keys are **stable** — do not rename them after shipping.
/// Compatible with OpenCode's token file format.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TokenFile {
    /// OpenAI OAuth 2.0 PKCE token (ChatGPT subscription).
    #[serde(rename = "openai", skip_serializing_if = "Option::is_none")]
    pub openai: Option<OAuthEntry>,
    /// GitHub Copilot device-flow token.
    #[serde(rename = "github-copilot", skip_serializing_if = "Option::is_none")]
    pub copilot: Option<OAuthEntry>,
}

impl TokenFile {
    /// `true` when no provider has a saved token.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.openai.is_none() && self.copilot.is_none()
    }

    /// Load from disk, auto-migrating legacy formats.
    ///
    /// Returns a default (all-`None`) value when the path doesn't exist.
    ///
    /// # Errors
    /// `LlmError::AuthExpired` if the file exists but cannot be parsed.
    pub fn load(path: &Path) -> LlmResult<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = std::fs::read(path)
            .map_err(|e| LlmError::AuthExpired(format!("read token file: {e}")))?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| LlmError::AuthExpired(format!("parse token file: {e}")))?;

        serde_json::from_value(value)
            .map_err(|e| LlmError::AuthExpired(format!("parse token file: {e}")))
    }

    /// Atomically write to disk (tmp → rename + fsync) with mode 0600.
    ///
    /// # Errors
    /// Propagates IO errors as `LlmError::AuthExpired`.
    pub fn save(&self, path: &Path) -> LlmResult<()> {
        use std::io::Write as _;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| LlmError::AuthExpired(format!("create token dir: {e}")))?;
        }
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(self).map_err(LlmError::from)?;
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)
                .map_err(|e| LlmError::AuthExpired(format!("open tmp token file: {e}")))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                f.set_permissions(std::fs::Permissions::from_mode(0o600))
                    .map_err(|e| LlmError::AuthExpired(format!("chmod token file: {e}")))?;
            }
            f.write_all(&json)
                .map_err(|e| LlmError::AuthExpired(format!("write token file: {e}")))?;
            f.sync_all()
                .map_err(|e| LlmError::AuthExpired(format!("fsync token file: {e}")))?;
        }
        std::fs::rename(&tmp, path)
            .map_err(|e| LlmError::AuthExpired(format!("rename token file: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::copilot::CopilotToken;
    use crate::openai_oauth::OAuthToken;

    fn far_future_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + 3600
    }

    #[test]
    fn empty_file_on_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let file = TokenFile::load(&path).unwrap();
        assert!(file.is_empty());
    }

    #[test]
    fn roundtrip_both_providers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth_token.json");

        let oauth = OAuthToken {
            access_token: "openai_tok".into(),
            refresh_token: "openai_ref".into(),
            expires_at: far_future_secs(),
            token_type: "Bearer".into(),
            scope: "openid".into(),
            account_id: None,
        };
        let cop = CopilotToken {
            github_token: "gho_copilot".into(),
        };

        oauth.save(&path).unwrap();
        cop.save(&path).unwrap();

        let loaded = TokenFile::load(&path).unwrap();
        let loaded_openai = loaded.openai.unwrap();
        let loaded_copilot = loaded.copilot.unwrap();

        assert_eq!(loaded_openai.access, "openai_tok");
        assert_eq!(loaded_copilot.access, "gho_copilot");
    }

    #[test]
    fn saves_preserve_other_provider() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth_token.json");

        // Save only copilot first.
        let cop = CopilotToken {
            github_token: "gho_first".into(),
        };
        cop.save(&path).unwrap();

        // Save openai — copilot entry must survive.
        let oauth = OAuthToken {
            access_token: "openai_access".into(),
            refresh_token: "openai_refresh".into(),
            expires_at: far_future_secs(),
            token_type: "Bearer".into(),
            scope: "openid".into(),
            account_id: None,
        };
        oauth.save(&path).unwrap();

        let file = TokenFile::load(&path).unwrap();
        assert_eq!(file.openai.unwrap().access, "openai_access");
        assert_eq!(file.copilot.unwrap().access, "gho_first");
    }

    #[cfg(unix)]
    #[test]
    fn save_sets_mode_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth_token.json");
        let file = TokenFile {
            copilot: Some(OAuthEntry {
                kind: "oauth".into(),
                access: "gho_test".into(),
                refresh: "gho_test".into(),
                expires: 0,
                account_id: None,
            }),
            ..TokenFile::default()
        };
        file.save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "token file must be mode 0600");
    }
}
