//! Registered-user domain types + validation.
//!
//! The storage shape lives in `ai-memory-store`; this module owns the
//! **pure types** + **input validation** that both the writer (insert
//! path) and the CLI / admin endpoint (caller path) share. Keeping the
//! rules in core means the same `MemoryError::InvalidUsername`
//! materialises whether a username is rejected by the CLI, the HTTP
//! handler, or the writer actor — one source of truth.
//!
//! ## Attribution, not RBAC
//!
//! See [`crate::actor`] for the broader rationale: ai-memory's data is
//! single-tenant by design. A `User` row records *who* a write came
//! from; it does not gate *whether* the write was allowed. Human
//! `role=root` vs `role=user` only affects web-session capabilities;
//! a native API credential always authenticates as [`crate::AuthLevel::User`].
//!
//! ## Credential classes (referenced here, implemented in `ai-memory-store`)
//!
//! Human passwords are Argon2id PHC strings on `users.password_hash` and
//! never authenticate as Bearer. Native machine keys are 256-bit secrets
//! with prefix [`NATIVE_API_KEY_PREFIX`]; the store keeps
//! `SHA-256(token || ":" || pepper)` in `api_credentials`. Web sessions
//! use prefix [`SESSION_SECRET_PREFIX`] and store only SHA-256 of the
//! secret (no pepper). See `ai_memory_store::users` / `api_credentials`
//! / `web_sessions`.

use serde::{Deserialize, Serialize};

use crate::{ApiCredentialId, MemoryError, UserId};

/// Web-session secret prefix. A human password that equals this prefix
/// is rejected so a leaked session cookie cannot be reused as a password.
pub const SESSION_SECRET_PREFIX: &str = "ams_";

/// Native engine-issued API key prefix (`aim_…`). Distinct from the
/// external `amk_` keys issued by mcp-auth.
pub const NATIVE_API_KEY_PREFIX: &str = "aim_";

/// External mcp-auth consumer-key prefix. Human passwords must not
/// impersonate this class either.
pub const EXTERNAL_API_KEY_PREFIX: &str = "amk_";

/// Minimum UTF-8 byte length of a human password.
pub const MIN_HUMAN_PASSWORD_BYTES: usize = 12;

/// Maximum UTF-8 byte length of a human password.
pub const MAX_HUMAN_PASSWORD_BYTES: usize = 1024;

/// Maximum username length. Anything longer is a misconfiguration; the
/// engine, CLI, and web UI all assume a small username fits in a single
/// terminal column / table cell.
pub const MAX_USERNAME_LEN: usize = 64;

/// Maximum email length per RFC 5321 §4.5.3.1.3 (path length).
pub const MAX_EMAIL_LEN: usize = 254;

/// Human role stored on `users.role`. Only two values exist; a native
/// API credential never inherits Root from this field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserRole {
    /// Console operator. A web session for this role is
    /// [`crate::AuthLevel::Root`].
    Root,
    /// Default human role. Web sessions and native API keys are
    /// [`crate::AuthLevel::User`].
    #[default]
    User,
}

impl UserRole {
    /// Wire / SQL value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::User => "user",
        }
    }

    /// Parse a stored `users.role` value.
    ///
    /// # Errors
    /// Returns [`MemoryError::MalformedRecord`] for any other string.
    pub fn parse(value: &str) -> Result<Self, MemoryError> {
        match value {
            "root" => Ok(Self::Root),
            "user" => Ok(Self::User),
            other => Err(MemoryError::MalformedRecord(format!(
                "unknown user role {other:?}"
            ))),
        }
    }
}

/// A registered user as stored. `id` is the UUIDv7 primary key; the
/// other fields mirror the public `users` columns. Password hashes,
/// session hashes, CSRF hashes, and API-key hashes never appear here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    /// Stable identity. Used by `audit_log.author_id` and
    /// `pages.author_id` foreign keys.
    pub id: UserId,
    /// Validated username (see [`validate_username`]).
    pub username: String,
    /// Optional display name for "Last edited by Alice Smith" style UIs.
    pub name: Option<String>,
    /// Optional email; validated by [`validate_email`] before insert.
    pub email: Option<String>,
    /// Microseconds since epoch — V01 convention.
    pub created_at: i64,
    /// Microseconds since epoch; `None` until the first authenticated
    /// request from this user. Updated fire-and-forget per request.
    pub last_seen_at: Option<i64>,
    /// Microseconds since epoch; `None` means the deprecated single-token
    /// compatibility credential is active. Native `api_credentials` have
    /// independent revocation metadata.
    #[serde(default)]
    pub token_expired_at: Option<i64>,
    /// Human role. Native API credentials ignore this for AuthLevel.
    pub role: UserRole,
    /// The next web login must change the password before `/admin` or
    /// `/api/v1` become available.
    pub must_change_password: bool,
    /// Microseconds since epoch; `None` means human login is enabled.
    pub disabled_at: Option<i64>,
    /// `true` when `users.password_hash` is populated. Token-only
    /// brownfield rows are `false` ("API only") until reset-password.
    pub has_password: bool,
}

impl User {
    /// `true` when human login is currently allowed.
    #[must_use]
    pub fn is_human_enabled(&self) -> bool {
        self.disabled_at.is_none() && self.has_password
    }

    /// `true` when the deprecated single-token compatibility credential is
    /// active.
    #[must_use]
    pub fn is_token_active(&self) -> bool {
        self.token_expired_at.is_none()
    }
}

/// Public metadata for one native API credential. The authenticating
/// hash is never serialised.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiCredential {
    /// Stable credential id. Brownfield copies use the parent `user_id`.
    pub id: ApiCredentialId,
    /// Owning human (or brownfield) identity.
    pub user_id: UserId,
    /// Operator-supplied label.
    pub label: String,
    /// Optional non-authenticating preview (e.g. last four of `aim_…`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Microseconds since epoch.
    pub created_at: i64,
    /// Microseconds since epoch; `None` until first use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<i64>,
    /// Optional absolute expiry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// Microseconds since epoch when revoked; `None` means active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<i64>,
}

/// Pre-insert shape for a new user. Carries the inputs the caller
/// (CLI / admin endpoint) collected; the actual token generation +
/// hashing lives in `ai-memory-store::users` since it owns the
/// per-server pepper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewUser {
    /// Required. Will be trimmed + validated; rejected if empty after
    /// trim, longer than [`MAX_USERNAME_LEN`], or contains whitespace /
    /// control characters / common separator chars.
    pub username: String,
    /// Optional display name. Trimmed; empty string is normalised to
    /// `None` so the DB doesn't store distinct `""` and `NULL`.
    pub name: Option<String>,
    /// Optional email. Trimmed + lowercased + validated by
    /// [`validate_email`]; empty string is normalised to `None`.
    pub email: Option<String>,
}

impl NewUser {
    /// In-place validate + normalise. Returns the same error kinds the
    /// CLI and admin endpoint surface to the operator.
    ///
    /// # Errors
    /// - [`MemoryError::InvalidUsername`] when the username is empty
    ///   after trim, too long, or contains forbidden characters.
    /// - [`MemoryError::InvalidEmail`] when an email is supplied but
    ///   fails the basic format check.
    pub fn validate(&mut self) -> Result<(), MemoryError> {
        self.username = self.username.trim().to_string();
        validate_username(&self.username)?;

        if let Some(name) = self.name.as_mut() {
            *name = name.trim().to_string();
        }
        if self.name.as_deref() == Some("") {
            self.name = None;
        }

        if let Some(email) = self.email.as_mut() {
            *email = email.trim().to_lowercase();
        }
        match self.email.as_deref() {
            Some("") => self.email = None,
            Some(e) => validate_email(e)?,
            None => {}
        }

        Ok(())
    }
}

/// Validate a username after trim. Rules — kept minimal per the v0.8
/// design call (no complex policies, just enough to keep weird
/// characters out):
///
/// - non-empty;
/// - at most [`MAX_USERNAME_LEN`] characters (code points, not bytes);
/// - no control characters (`is_control`);
/// - no whitespace anywhere (usernames are identifiers, not free text);
/// - no common separator / quoting characters that would make CLI
///   quoting + URL embedding painful: `/ \ : ; , " ' ` `.
///
/// UTF-8 letters / digits / `.` / `-` / `_` / `@` are allowed, so
/// emails-as-usernames (`alice@home`) work for operators who want them.
///
/// # Errors
/// Returns [`MemoryError::InvalidUsername`] on any rule violation.
pub fn validate_username(s: &str) -> Result<(), MemoryError> {
    if s.is_empty() {
        return Err(MemoryError::InvalidUsername("empty after trim".into()));
    }
    let len = s.chars().count();
    if len > MAX_USERNAME_LEN {
        return Err(MemoryError::InvalidUsername(format!(
            "{len} characters, exceeds max {MAX_USERNAME_LEN}"
        )));
    }
    for ch in s.chars() {
        if ch.is_control() {
            return Err(MemoryError::InvalidUsername(format!(
                "control character U+{:04X}",
                ch as u32
            )));
        }
        if ch.is_whitespace() {
            return Err(MemoryError::InvalidUsername(format!(
                "whitespace character {ch:?}"
            )));
        }
        if matches!(ch, '/' | '\\' | ':' | ';' | ',' | '"' | '\'' | '`') {
            return Err(MemoryError::InvalidUsername(format!(
                "separator character {ch:?}"
            )));
        }
    }
    Ok(())
}

/// Basic email-format check. Intentionally permissive — operators may
/// use intranet-style addresses (`alice@home`) without a public TLD, so
/// we don't require a dot in the domain. Rules:
///
/// - at most [`MAX_EMAIL_LEN`] characters;
/// - exactly one `@`;
/// - non-empty local + non-empty domain;
/// - no whitespace anywhere;
/// - no control characters.
///
/// This is the "doesn't look obviously wrong" check, not RFC 5322
/// compliance. Quoted local parts, IP-literal domains in brackets, and
/// other rare-but-valid forms are not supported.
///
/// # Errors
/// Returns [`MemoryError::InvalidEmail`] on any rule violation.
pub fn validate_email(s: &str) -> Result<(), MemoryError> {
    let len = s.chars().count();
    if len > MAX_EMAIL_LEN {
        return Err(MemoryError::InvalidEmail(format!(
            "{len} characters, exceeds max {MAX_EMAIL_LEN}"
        )));
    }
    for ch in s.chars() {
        if ch.is_control() {
            return Err(MemoryError::InvalidEmail(format!(
                "control character U+{:04X}",
                ch as u32
            )));
        }
        if ch.is_whitespace() {
            return Err(MemoryError::InvalidEmail(format!(
                "whitespace character {ch:?}"
            )));
        }
    }
    let mut parts = s.split('@');
    let local = parts.next().unwrap_or_default();
    let domain = parts.next().unwrap_or_default();
    if parts.next().is_some() {
        return Err(MemoryError::InvalidEmail("multiple `@` characters".into()));
    }
    if local.is_empty() {
        return Err(MemoryError::InvalidEmail("empty local part".into()));
    }
    if domain.is_empty() {
        return Err(MemoryError::InvalidEmail("empty domain part".into()));
    }
    Ok(())
}

/// Validate a human password before Argon2id hashing.
///
/// Policy (AUTH-010): 12–1024 UTF-8 bytes; not a reserved credential
/// prefix ([`SESSION_SECRET_PREFIX`], [`NATIVE_API_KEY_PREFIX`],
/// [`EXTERNAL_API_KEY_PREFIX`]); not equal to `username` or any
/// configured reserved secret (recovery token, machine-root bearer,
/// actor-proxy bearer). Token-hash collision is checked by the store
/// against `api_credentials` hashes only.
///
/// Failures always use the same generic [`MemoryError::InvalidPassword`]
/// text so callers cannot distinguish which rule fired.
///
/// # Errors
/// Returns [`MemoryError::InvalidPassword`] on any policy violation.
pub fn validate_human_password(
    password: &str,
    username: Option<&str>,
    reserved_plaintext: &[&str],
) -> Result<(), MemoryError> {
    const GENERIC: &str = "does not meet policy";
    let bytes = password.len();
    if !(MIN_HUMAN_PASSWORD_BYTES..=MAX_HUMAN_PASSWORD_BYTES).contains(&bytes) {
        return Err(MemoryError::InvalidPassword(GENERIC.into()));
    }
    if password.starts_with(SESSION_SECRET_PREFIX)
        || password.starts_with(NATIVE_API_KEY_PREFIX)
        || password.starts_with(EXTERNAL_API_KEY_PREFIX)
    {
        return Err(MemoryError::InvalidPassword(GENERIC.into()));
    }
    if username.is_some_and(|name| password == name) {
        return Err(MemoryError::InvalidPassword(GENERIC.into()));
    }
    if reserved_plaintext
        .iter()
        .any(|secret| !secret.is_empty() && password == *secret)
    {
        return Err(MemoryError::InvalidPassword(GENERIC.into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── username validation ───────────────────────────────────────

    #[test]
    fn username_accepts_simple() {
        assert!(validate_username("alice").is_ok());
        assert!(validate_username("boss").is_ok());
        assert!(validate_username("user_1").is_ok());
        assert!(validate_username("a.b-c").is_ok());
    }

    #[test]
    fn username_accepts_email_style() {
        // Emails-as-usernames must work for operators who want them.
        assert!(validate_username("alice@home").is_ok());
        assert!(validate_username("boss@example.com").is_ok());
    }

    #[test]
    fn username_accepts_utf8_letters() {
        assert!(validate_username("bóss").is_ok());
        assert!(validate_username("ボス").is_ok());
    }

    #[test]
    fn username_rejects_empty() {
        assert!(validate_username("").is_err());
    }

    #[test]
    fn username_rejects_whitespace() {
        for s in ["boss man", "boss\tman", "boss\nman", " boss", "boss "] {
            assert!(
                validate_username(s).is_err(),
                "whitespace must be rejected: {s:?}"
            );
        }
    }

    #[test]
    fn username_rejects_control_chars() {
        assert!(validate_username("boss\x00").is_err());
        assert!(validate_username("boss\x07man").is_err());
        assert!(validate_username("boss\x7fman").is_err());
    }

    #[test]
    fn username_rejects_separator_chars() {
        for ch in ['/', '\\', ':', ';', ',', '"', '\'', '`'] {
            let s = format!("boss{ch}man");
            assert!(
                validate_username(&s).is_err(),
                "{ch:?} must be rejected: {s:?}"
            );
        }
    }

    #[test]
    fn username_rejects_over_length() {
        let too_long = "a".repeat(MAX_USERNAME_LEN + 1);
        assert!(validate_username(&too_long).is_err());
        let just_right = "a".repeat(MAX_USERNAME_LEN);
        assert!(validate_username(&just_right).is_ok());
    }

    // ── email validation ──────────────────────────────────────────

    #[test]
    fn email_accepts_typical() {
        assert!(validate_email("alice@example.com").is_ok());
        assert!(validate_email("a.b.c@subdomain.example.com").is_ok());
        assert!(validate_email("alice+filter@example.com").is_ok());
    }

    #[test]
    fn email_accepts_intranet_no_tld() {
        // Homelab users have `alice@home` setups; the basic check must
        // not require a dot in the domain.
        assert!(validate_email("alice@home").is_ok());
        assert!(validate_email("boss@localhost").is_ok());
    }

    #[test]
    fn email_rejects_missing_at() {
        assert!(validate_email("alice.example.com").is_err());
    }

    #[test]
    fn email_rejects_multiple_at() {
        assert!(validate_email("alice@@example.com").is_err());
        assert!(validate_email("a@b@c").is_err());
    }

    #[test]
    fn email_rejects_empty_parts() {
        assert!(validate_email("@example.com").is_err());
        assert!(validate_email("alice@").is_err());
        assert!(validate_email("@").is_err());
    }

    #[test]
    fn email_rejects_whitespace() {
        for s in ["alice @example.com", "alice@ example.com", " alice@x.y"] {
            assert!(
                validate_email(s).is_err(),
                "whitespace must be rejected: {s:?}"
            );
        }
    }

    #[test]
    fn email_rejects_control_chars() {
        assert!(validate_email("alice\x00@example.com").is_err());
    }

    #[test]
    fn email_rejects_over_length() {
        let local = "a".repeat(MAX_EMAIL_LEN);
        let too_long = format!("{local}@x");
        assert!(validate_email(&too_long).is_err());
    }

    // ── NewUser::validate normalisation ───────────────────────────

    #[test]
    fn new_user_trims_username() {
        let mut nu = NewUser {
            username: "  alice  ".into(),
            name: None,
            email: None,
        };
        nu.validate().unwrap();
        assert_eq!(nu.username, "alice");
    }

    #[test]
    fn new_user_normalises_empty_optionals_to_none() {
        let mut nu = NewUser {
            username: "alice".into(),
            name: Some("   ".into()),
            email: Some("   ".into()),
        };
        nu.validate().unwrap();
        assert_eq!(nu.name, None);
        assert_eq!(nu.email, None);
    }

    #[test]
    fn new_user_lowercases_email() {
        let mut nu = NewUser {
            username: "alice".into(),
            name: None,
            email: Some("Alice@Example.COM".into()),
        };
        nu.validate().unwrap();
        assert_eq!(nu.email.as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn new_user_trims_name() {
        let mut nu = NewUser {
            username: "alice".into(),
            name: Some("  Alice Smith  ".into()),
            email: None,
        };
        nu.validate().unwrap();
        // Internal whitespace is fine in a display name — only the
        // edges get trimmed.
        assert_eq!(nu.name.as_deref(), Some("Alice Smith"));
    }

    #[test]
    fn new_user_rejects_invalid_username() {
        let mut nu = NewUser {
            username: "boss man".into(),
            name: None,
            email: None,
        };
        assert!(matches!(
            nu.validate(),
            Err(MemoryError::InvalidUsername(_))
        ));
    }

    #[test]
    fn new_user_rejects_invalid_email() {
        let mut nu = NewUser {
            username: "alice".into(),
            name: None,
            email: Some("not-an-email".into()),
        };
        assert!(matches!(nu.validate(), Err(MemoryError::InvalidEmail(_))));
    }
    #[test]
    fn human_password_accepts_typical() {
        assert!(validate_human_password("twelve chars!!", Some("alice"), &[]).is_ok());
    }

    #[test]
    fn human_password_rejects_short_long_prefix_and_reserved() {
        let ok = "twelve chars!!";
        assert!(validate_human_password(&ok[..11], None, &[]).is_err());
        let too_long = "a".repeat(MAX_HUMAN_PASSWORD_BYTES + 1);
        assert!(validate_human_password(&too_long, None, &[]).is_err());
        assert!(validate_human_password(&format!("ams_{ok}"), None, &[]).is_err());
        assert!(validate_human_password(&format!("aim_{ok}"), None, &[]).is_err());
        assert!(validate_human_password(&format!("amk_{ok}"), None, &[]).is_err());
        assert!(validate_human_password(ok, Some(ok), &[]).is_err());
        assert!(validate_human_password(ok, Some("alice"), &[ok]).is_err());
    }

    #[test]
    fn user_role_round_trips() {
        assert_eq!(UserRole::parse("root").unwrap(), UserRole::Root);
        assert_eq!(UserRole::parse("user").unwrap(), UserRole::User);
        assert!(UserRole::parse("admin").is_err());
        assert_eq!(UserRole::Root.as_str(), "root");
        assert_eq!(UserRole::default(), UserRole::User);
    }
}
