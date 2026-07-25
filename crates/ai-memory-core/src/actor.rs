//! Per-request actor identity — who triggered an operation.
//!
//! ai-memory's data is single-tenant (no RBAC; everyone with auth sees the
//! same pages), but writes can be **attributed** to the user who made them.
//! [`ActorContext`] is the typed carrier for that identity, injected once
//! per request by the auth middleware and threaded through to the writer
//! actor so attribution lands in the same SQL transaction as the data.
//!
//! ## Resolution rungs
//!
//! 1. **Anonymous** — no `Authorization` header configured at all.
//!    `ActorContext::default()` (all fields `None`). The pre-multi-user
//!    behaviour; backward-compatible for every existing single-user setup.
//! 2. **Identified single-user (root)** — `AI_MEMORY_AUTH_TOKEN` matches
//!    `config.auth.bearer_token`. Middleware fills `user` / `email` / `name`
//!    from `[auth].root_username` / `root_email` (and optional `root_name`).
//! 3. **Identified multi-user** — bearer token matches an active
//!    `users.token_hash` row. Middleware fills the actor from the row.
//! 4. **External auth proxy** — operator runs an auth sidecar that injects
//!    pre-validated `X-Memory-Actor-*` headers; the middleware overlays
//!    them onto the rung 2/3 actor. (Scaffolding only in v1 — the `sub`
//!    and `client` fields below exist for this use case and the eventual
//!    admission webhook chain payload contract.)
//!
//! ## Why not RBAC
//!
//! ai-memory v1's data model is single-tenant by design. Attribution
//! records *who* did a write; it does not gate *whether* they could do it.
//! That keeps the engine focused on "shared memory for a household /
//! small team" without bringing in roles, groups, or per-page ACLs.
//!
//! ## Field choice
//!
//! Every field is `Option<String>` so:
//! - `Default::default()` is a valid anonymous actor (no allocation).
//! - Partial identity (e.g. agent known via hook payload, user not yet
//!   authenticated) is representable.
//! - Serialised payloads omit absent fields rather than emitting `null`
//!   noise — see the `#[serde(skip_serializing_if = "Option::is_none")]`
//!   attributes.

use serde::{Deserialize, Serialize};

/// Identity of the actor that triggered an operation.
///
/// Populated by the auth middleware. Pure data — no I/O, no resolution
/// logic lives here. Cloneable + cheap; threaded through request handlers
/// via `Extension<ActorContext>` and forwarded into the writer actor as
/// part of the write command so attribution and data land atomically.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorContext {
    /// Which client triggered the write — `claude-code`, `codex`,
    /// `opencode`, `gemini-cli`, `cursor`, `cli`, `hook`, … Sourced from
    /// the MCP client info or the hook payload's `agent` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Human-readable username (e.g. `boss`, `alice`). The stable
    /// attribution key surfaced in the audit log + page frontmatter
    /// `last_modified_by`. `None` = anonymous.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Optional display name (e.g. `Alice Smith`). For UIs that want to
    /// show "Last edited by Alice Smith" instead of `alice`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional email. Surfaced alongside the username in the web UI +
    /// `/api/v1` responses so reviewers know who to ask about a page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Session id from the agent (when known via the hook payload).
    /// Lets per-session timelines reconstruct "what did this agent do
    /// in this session" against `audit_log` + `observations`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Reserved for the external-auth-proxy rung: the JWT `sub` claim
    /// (stable user UUID). Kept `Option<String>` so payloads to the
    /// future admission webhook chain stay forward-compatible with the
    /// shape PR #55 documents — we don't fill it from rungs 0-3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub: Option<String>,
    /// Reserved for the external-auth-proxy rung: the DCR client UUID
    /// identifying which install of an agent made the request. Same
    /// forward-compat rationale as [`Self::sub`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

/// Authorization tier the auth middleware resolved this request to.
///
/// Identity ([`ActorContext`]) carries *who* the request is from;
/// `AuthLevel` carries *what they're allowed to do*. The two are
/// distinct so a handler can guard on "must be root" without also
/// having to inspect or compare username strings against config.
///
/// Available as `Extension<AuthLevel>` on every request after the
/// auth middleware runs. In multi-user mode every `/admin/*` route
/// checks this against [`AuthLevel::Root`] and returns 403 for
/// `User` / 401 for `Anonymous`; normal DB users are allowed on the
/// MCP and read-only API surfaces where writes are attributed to them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthLevel {
    /// Rung 0: no auth configured. Read-mostly setups; root-only
    /// routes refuse this tier (no root → no user management).
    Anonymous,
    /// Rung 1: authenticated as the configured root user.
    /// Allowed everywhere including user-management endpoints.
    Root,
    /// Rung 2: authenticated via the `users` table.
    /// Allowed on regular routes (write_page, query, etc.) but
    /// refused on root-only admin routes.
    User,
}

/// Coarse-grained capabilities guarded by the auth layer.
///
/// This is intentionally smaller than a role/RBAC system: ai-memory v1 is
/// single-tenant, so the policy surface is "normal read/write is allowed"
/// versus "this operational action needs root". Keeping that decision in one
/// enum prevents future handlers from open-coding subtly different checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Operational `/admin/*` routes. In multi-user mode these are root-only;
    /// in single-user/no-auth mode historical behavior is preserved.
    Admin,
    /// User lifecycle routes (`/admin/users*`). These are root-only even when
    /// multi-user mode is not fully configured.
    UserManagement,
    /// Regular read surfaces (MCP/query/API/wiki reads).
    NormalRead,
    /// Regular write surfaces that attribute to the resolved actor.
    NormalWrite,
    /// Loop-prevention admission-chain skip header.
    SkipAdmissionChain,
}

/// Authorization failure independent of any HTTP framework.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthzError {
    /// The caller must authenticate before the capability can be used.
    AuthenticationRequired(&'static str),
    /// The caller authenticated, but the capability is root-only.
    Forbidden(&'static str),
}

impl AuthzError {
    /// Human-readable policy message for HTTP/MCP responses.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            AuthzError::AuthenticationRequired(msg) | AuthzError::Forbidden(msg) => msg,
        }
    }

    /// True when the response should be an authentication challenge (HTTP 401).
    #[must_use]
    pub fn is_authentication_required(self) -> bool {
        matches!(self, AuthzError::AuthenticationRequired(_))
    }
}

impl AuthLevel {
    /// Check whether this auth tier can use `capability`.
    ///
    /// `multi_user_enabled` is the store-backed presence of any user row:
    /// operational admin routes keep their historical bootstrap behavior until
    /// the first user is created, while user-management is always root-only.
    pub fn authorize(
        self,
        capability: Capability,
        multi_user_enabled: bool,
    ) -> Result<(), AuthzError> {
        match capability {
            Capability::NormalRead | Capability::NormalWrite => Ok(()),
            Capability::SkipAdmissionChain => match self {
                AuthLevel::User => {
                    Err(AuthzError::Forbidden("admission webhook skip is root-only"))
                }
                AuthLevel::Anonymous | AuthLevel::Root => Ok(()),
            },
            Capability::Admin if !multi_user_enabled => Ok(()),
            Capability::Admin => self.require_root(
                "admin operation requires authentication in multi-user mode",
                "admin operation is root-only in multi-user mode",
            ),
            Capability::UserManagement => self.require_root(
                "user management requires authentication",
                "user management is root-only",
            ),
        }
    }

    fn require_root(
        self,
        anonymous_message: &'static str,
        user_message: &'static str,
    ) -> Result<(), AuthzError> {
        match self {
            AuthLevel::Root => Ok(()),
            AuthLevel::Anonymous => Err(AuthzError::AuthenticationRequired(anonymous_message)),
            AuthLevel::User => Err(AuthzError::Forbidden(user_message)),
        }
    }
}

impl ActorContext {
    /// `true` if at least one identity field is set.
    ///
    /// Cheap predicate for "should we record attribution?" — when this
    /// returns `false` the writer can skip the audit-log author_id
    /// stamp (saves a column write per operation) and emit pages without
    /// the `last_modified_by` frontmatter block.
    #[must_use]
    pub fn has_any(&self) -> bool {
        self.agent.is_some()
            || self.user.is_some()
            || self.name.is_some()
            || self.email.is_some()
            || self.session_id.is_some()
            || self.sub.is_some()
            || self.client.is_some()
    }

    /// Construct the canonical anonymous actor — same as
    /// [`Default::default`], but more readable at call sites where the
    /// intent is "this is an anonymous request".
    #[must_use]
    pub fn anonymous() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_anonymous() {
        let a = ActorContext::default();
        assert!(!a.has_any(), "default actor must be fully anonymous");
        assert_eq!(a, ActorContext::anonymous());
    }

    #[test]
    fn admin_capability_preserves_single_user_mode() {
        for level in [AuthLevel::Anonymous, AuthLevel::Root, AuthLevel::User] {
            assert_eq!(level.authorize(Capability::Admin, false), Ok(()));
        }
    }

    #[test]
    fn admin_capability_is_root_only_in_multi_user_mode() {
        assert_eq!(AuthLevel::Root.authorize(Capability::Admin, true), Ok(()));
        assert!(matches!(
            AuthLevel::Anonymous.authorize(Capability::Admin, true),
            Err(AuthzError::AuthenticationRequired(
                "admin operation requires authentication in multi-user mode"
            ))
        ));
        assert!(matches!(
            AuthLevel::User.authorize(Capability::Admin, true),
            Err(AuthzError::Forbidden(
                "admin operation is root-only in multi-user mode"
            ))
        ));
    }

    #[test]
    fn user_management_is_always_root_only() {
        assert_eq!(
            AuthLevel::Root.authorize(Capability::UserManagement, false),
            Ok(())
        );
        assert_eq!(
            AuthLevel::Root.authorize(Capability::UserManagement, true),
            Ok(())
        );
        assert!(matches!(
            AuthLevel::Anonymous.authorize(Capability::UserManagement, false),
            Err(AuthzError::AuthenticationRequired(
                "user management requires authentication"
            ))
        ));
        assert!(matches!(
            AuthLevel::User.authorize(Capability::UserManagement, true),
            Err(AuthzError::Forbidden("user management is root-only"))
        ));
    }

    #[test]
    fn skip_admission_chain_rejects_db_users() {
        assert_eq!(
            AuthLevel::Root.authorize(Capability::SkipAdmissionChain, true),
            Ok(())
        );
        assert_eq!(
            AuthLevel::Anonymous.authorize(Capability::SkipAdmissionChain, true),
            Ok(())
        );
        assert!(matches!(
            AuthLevel::User.authorize(Capability::SkipAdmissionChain, true),
            Err(AuthzError::Forbidden("admission webhook skip is root-only"))
        ));
    }

    #[test]
    fn has_any_truth_table() {
        // Each field individually flips has_any() to true. Catches an
        // accidental omission if someone adds a new field and forgets
        // to update the predicate.
        let mut a = ActorContext::default();
        assert!(!a.has_any());

        a.agent = Some("claude-code".into());
        assert!(a.has_any());
        a = ActorContext::default();

        a.user = Some("alice".into());
        assert!(a.has_any());
        a = ActorContext::default();

        a.name = Some("Alice Smith".into());
        assert!(a.has_any());
        a = ActorContext::default();

        a.email = Some("alice@home".into());
        assert!(a.has_any());
        a = ActorContext::default();

        a.session_id = Some("s-1".into());
        assert!(a.has_any());
        a = ActorContext::default();

        a.sub = Some("8f3a".into());
        assert!(a.has_any());
        a = ActorContext::default();

        a.client = Some("72836f52".into());
        assert!(a.has_any());
    }

    #[test]
    fn anonymous_serialises_to_empty_object() {
        // Every absent field is omitted (not `null`) — keeps the
        // webhook payload + /api/v1 response shape lean.
        let json = serde_json::to_string(&ActorContext::default()).unwrap();
        assert_eq!(json, "{}");
    }

    #[test]
    fn partial_actor_serialises_only_set_fields() {
        let a = ActorContext {
            user: Some("boss".into()),
            email: Some("boss@example.com".into()),
            ..ActorContext::default()
        };
        let json = serde_json::to_string(&a).unwrap();
        // Stable field order is set by the struct definition; serde
        // emits fields in declaration order.
        assert_eq!(json, r#"{"user":"boss","email":"boss@example.com"}"#);
    }

    #[test]
    fn round_trip_preserves_all_set_fields() {
        let original = ActorContext {
            agent: Some("codex".into()),
            user: Some("alice".into()),
            name: Some("Alice Smith".into()),
            email: Some("alice@home".into()),
            session_id: Some("019e6d".into()),
            sub: Some("8f3a-uuid".into()),
            client: Some("72836f52-uuid".into()),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: ActorContext = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn missing_fields_deserialise_to_none() {
        // Forward-compat: a payload from an older sender that omits
        // newly-added fields still deserialises cleanly.
        let parsed: ActorContext = serde_json::from_str(r#"{"user":"boss"}"#).unwrap();
        assert_eq!(parsed.user.as_deref(), Some("boss"));
        assert!(parsed.agent.is_none());
        assert!(parsed.email.is_none());
        assert!(parsed.sub.is_none());
    }

    #[test]
    fn explicit_null_fields_deserialise_to_none() {
        // Some senders (older webhooks, hand-written JSON) emit `null`
        // for absent fields instead of omitting them. Both forms must
        // round-trip to the same anonymous actor.
        let parsed: ActorContext = serde_json::from_str(r#"{"user":null,"agent":null}"#).unwrap();
        assert!(!parsed.has_any());
    }
}
