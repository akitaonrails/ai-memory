//! This install's own root bearer: minted by `init` on a fresh data dir and by
//! `ai-memory auth enable`, stored at `<data_dir>/auth-tokens/default`, and
//! named by `[auth].bearer_token_file` so `Config::load` reads it once.
//!
//! The token is read from that file by the server and every thin client, and
//! copied into the hook credential files (`auth-token`, `auth-header`) that
//! every hook front door already reads. It never goes into a harness config or
//! onto a command line (#552).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use ai_memory_mcp::auth::generate_token_hex;

/// Bytes of OS CSPRNG entropy in a generated root bearer: 64 hex characters,
/// the same strength `ai-memory generate-auth-token` prints by default.
const ROOT_TOKEN_BYTES: usize = 32;

/// The root bearer after [`provision_root_token`].
pub(crate) struct RootToken {
    pub(crate) path: PathBuf,
    pub(crate) token: String,
    /// False when an existing token was reused.
    pub(crate) created: bool,
}

/// Reuse the token at `path`, or mint one there (`0600`, atomic) when the file
/// is missing or blank. Reuse is what makes `auth enable` idempotent and keeps
/// a forced `init` from rotating a credential every client already holds.
///
/// # Errors
/// Fails when the OS RNG fails or the file cannot be written.
pub(crate) fn provision_root_token(path: &Path) -> Result<RootToken> {
    if let Some(token) = crate::config::read_trimmed_secret(path) {
        return Ok(RootToken {
            path: path.to_path_buf(),
            token,
            created: false,
        });
    }
    let token = generate_token_hex(ROOT_TOKEN_BYTES)
        .map_err(|e| anyhow::anyhow!("generating the root bearer token: {e}"))?;
    crate::server_profiles::store_root_token(path, &token)
        .with_context(|| format!("writing the root bearer token to {}", path.display()))?;
    Ok(RootToken {
        path: path.to_path_buf(),
        token,
        created: true,
    })
}

/// What [`seed_hook_credential`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookCredential {
    /// No hook credential was stored; the root bearer now is.
    Seeded,
    /// The stored hook credential already is the root bearer.
    Current,
    /// A different credential is stored (an `ai-memory api-key` token for
    /// attribution, say). It is left alone.
    KeptOther,
}

/// Give the installed hooks the root bearer through the files they already
/// read when `AI_MEMORY_AUTH_TOKEN` is unset: `<data_dir>/auth-token` and
/// `<data_dir>/auth-header`, both `0600`, the pair `install-hooks --apply`
/// writes. A different stored credential wins and is kept: it was put there
/// on purpose.
///
/// # Errors
/// Propagates write failures.
pub(crate) fn seed_hook_credential(data_dir: &Path, token: &str) -> Result<HookCredential> {
    match crate::config::read_hook_auth_token(data_dir) {
        Some(existing) if existing == token => Ok(HookCredential::Current),
        Some(_) => Ok(HookCredential::KeptOther),
        None => {
            crate::config::store_hook_auth_token(data_dir, token).with_context(|| {
                format!(
                    "writing the hook credential files under {}",
                    data_dir.display()
                )
            })?;
            Ok(HookCredential::Seeded)
        }
    }
}

/// The command `status` and `doctor` name as the fix for an unauthenticated
/// loopback server.
pub(crate) const AUTH_ENABLE_FIX: &str = "ai-memory auth enable";

/// The server's auth posture as `status` and `doctor` report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuthPosture {
    /// The server requires a bearer token (or the client sends one).
    Token,
    /// The server answered without a credential on a loopback address: safe
    /// from the network, open to every local process and browser page.
    NoneLoopback,
    /// The server answered without a credential on a non-loopback address.
    None,
}

impl AuthPosture {
    /// `server_open` is whether the server is known to answer without a
    /// credential.
    pub(crate) fn classify(server_open: bool, server_url: &str) -> Self {
        match (server_open, crate::http_client::is_loopback_url(server_url)) {
            (false, _) => Self::Token,
            (true, true) => Self::NoneLoopback,
            (true, false) => Self::None,
        }
    }

    /// `auth: ...` text, with the fix on the same line when there is one.
    pub(crate) fn line(self) -> String {
        match self {
            Self::Token => "bearer token".to_owned(),
            Self::NoneLoopback => format!("none (loopback) — fix: `{AUTH_ENABLE_FIX}`"),
            Self::None => "none — set AI_MEMORY_AUTH_TOKEN on the server".to_owned(),
        }
    }

    pub(crate) fn localized_line(self, i18n: &ai_memory_core::i18n::Localizer) -> String {
        if i18n.language() == ai_memory_core::i18n::Language::English {
            return self.line();
        }
        match self {
            Self::Token => i18n.text("cli-auth-posture-token"),
            Self::NoneLoopback => i18n.one("cli-auth-posture-loopback", AUTH_ENABLE_FIX),
            Self::None => i18n.text("cli-auth-posture-none"),
        }
    }

    /// The fix command, for JSON output.
    pub(crate) fn fix(self) -> Option<&'static str> {
        matches!(self, Self::NoneLoopback).then_some(AUTH_ENABLE_FIX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posture_names_the_fix_only_for_an_open_loopback_server() {
        let open_local = AuthPosture::classify(true, "http://127.0.0.1:49374");
        assert_eq!(open_local, AuthPosture::NoneLoopback);
        assert_eq!(
            open_local.line(),
            "none (loopback) — fix: `ai-memory auth enable`"
        );
        assert_eq!(open_local.fix(), Some("ai-memory auth enable"));
        assert_eq!(
            AuthPosture::classify(true, "http://localhost:49374/"),
            AuthPosture::NoneLoopback
        );
        assert_eq!(
            AuthPosture::classify(true, "https://memory.example"),
            AuthPosture::None
        );
        let closed = AuthPosture::classify(false, "http://127.0.0.1:49374");
        assert_eq!(closed, AuthPosture::Token);
        assert_eq!(closed.fix(), None);
    }

    #[test]
    fn provisioning_mints_once_then_reuses() {
        let dd = tempfile::tempdir().unwrap();
        let path = crate::server_profiles::root_token_path(dd.path());

        let first = provision_root_token(&path).unwrap();
        assert!(first.created);
        assert_eq!(first.token.len(), ROOT_TOKEN_BYTES * 2);
        assert!(first.token.chars().all(|c| c.is_ascii_hexdigit()));

        let second = provision_root_token(&path).unwrap();
        assert!(
            !second.created,
            "an existing token is reused, never rotated"
        );
        assert_eq!(second.token, first.token);

        // A blank file is not a token.
        std::fs::write(&path, "  \n").unwrap();
        let third = provision_root_token(&path).unwrap();
        assert!(third.created);
        assert_ne!(third.token, first.token);
    }

    #[cfg(unix)]
    #[test]
    fn the_root_bearer_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dd = tempfile::tempdir().unwrap();
        let path = crate::server_profiles::root_token_path(dd.path());
        provision_root_token(&path).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    #[test]
    fn seeding_never_replaces_a_different_hook_credential() {
        let dd = tempfile::tempdir().unwrap();
        assert_eq!(
            seed_hook_credential(dd.path(), "root").unwrap(),
            HookCredential::Seeded
        );
        assert_eq!(
            crate::config::read_hook_auth_token(dd.path()).as_deref(),
            Some("root")
        );
        assert_eq!(
            seed_hook_credential(dd.path(), "root").unwrap(),
            HookCredential::Current
        );

        crate::config::store_hook_auth_token(dd.path(), "aim_user_key").unwrap();
        assert_eq!(
            seed_hook_credential(dd.path(), "root").unwrap(),
            HookCredential::KeptOther
        );
        assert_eq!(
            crate::config::read_hook_auth_token(dd.path()).as_deref(),
            Some("aim_user_key")
        );
    }
}
