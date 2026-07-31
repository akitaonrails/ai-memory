//! Cross-agent handoff type.
//!
//! A handoff is a typed snapshot of "where we are" — created when one
//! agent CLI ends a session, accepted when the next one starts in the
//! same project. Stored explicitly (vs. inferring from the
//! observations log) because cross-agent continuity is the project's
//! headline feature and deserves a first-class schema.

use std::path::PathBuf;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::ids::{AgentKind, HandoffId, ProjectId, SessionId, WorkspaceId};

/// State machine of a single handoff row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffState {
    /// Created, not yet picked up by the next agent.
    Open,
    /// Another agent has called `memory_handoff_accept` on it.
    Accepted,
    /// Aged out (decay sweep).
    Expired,
}

impl HandoffState {
    /// Canonical wire string.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Accepted => "accepted",
            Self::Expired => "expired",
        }
    }
}

impl std::str::FromStr for HandoffState {
    type Err = crate::MemoryError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "open" => Ok(Self::Open),
            "accepted" => Ok(Self::Accepted),
            "expired" => Ok(Self::Expired),
            other => Err(crate::MemoryError::MalformedRecord(format!(
                "unknown handoff state: {other}"
            ))),
        }
    }
}

/// Which owned rows a caller is allowed to see or act on.
///
/// Used for handoffs and for sessions. A row whose owner is `None` is *shared*:
/// it predates ownership, was written by a caller with no actor, or was
/// deliberately published to the whole project. Shared rows stay visible to
/// everyone, which is what keeps single-operator servers behaving exactly as
/// before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerFilter {
    /// An identified caller: their own rows plus the shared ones.
    User(String),
    /// A caller with no actor: shared rows only. Prevents an unattributed
    /// reader (or a browser hitting a read-only API) from draining or reading
    /// something that belongs to a specific operator.
    Unattributed,
    /// Explicit opt-in to every owner, for operator recovery ("give me whatever
    /// is pending here, even if it is not mine").
    Any,
}

impl OwnerFilter {
    /// Build the filter for an optional actor: `Some(user)` identifies the
    /// caller, `None` means unattributed.
    #[must_use]
    pub fn for_actor(user: Option<&str>) -> Self {
        match user {
            Some(user) if !user.trim().is_empty() => Self::User(user.to_string()),
            _ => Self::Unattributed,
        }
    }

    /// Build the filter for a whole actor, applying the one identity rule in
    /// [`crate::ActorContext::identity_key`] instead of re-deriving it here.
    #[must_use]
    pub fn for_actor_context(actor: &crate::ActorContext) -> Self {
        Self::for_actor(actor.identity_key())
    }

    /// Does a row owned by `owner` pass this filter?
    #[must_use]
    pub fn admits(&self, owner: Option<&str>) -> bool {
        match (self, owner) {
            // Shared handoffs are visible to everyone (legacy + single-user).
            (_, None) => true,
            (Self::Any, _) => true,
            (Self::User(me), Some(owner)) => me == owner,
            (Self::Unattributed, Some(_)) => false,
        }
    }
}

/// The owner to stamp on a row a request creates — the write-side mirror of
/// [`OwnerFilter::for_actor`], which decides what that same request may read.
///
/// `identity` is the caller's [`crate::ActorContext::identity_key`].
/// `distinguishes_operators` is the deployment-level question the admin gates
/// already ask (`users` rows exist, or a trusted proxy asserts identities), and
/// it is what makes this more than `identity.map(to_owned)`.
///
/// # Why a deployment that names one operator must still stamp nothing
///
/// On a server with `[auth].bearer_token` + `[auth].root_username` and no users
/// and no proxy, every HTTP request resolves to that single name. Stamping it
/// does not separate anybody from anybody — there is only one operator — but it
/// does split that operator in two, because the transports disagree about
/// whether a request carries an actor at all: the HTTP write lands as
/// `owner = "<root_username>"`, while the SAME person's stdio / in-process MCP
/// transport carries no actor, filters as [`OwnerFilter::Unattributed`], and
/// can no longer see what they just wrote. One operator, one data directory,
/// two transports, and the rows written through one invisible to the other.
/// Producing the pre-ownership `NULL` there is what keeps them agreeing.
///
/// The read side deliberately does **not** take this gate. A caller still
/// filters as `OwnerFilter::User(name)`, which admits both the shared rows and
/// any row already stamped with that name, so rows written while a deployment
/// did distinguish operators stay reachable to that operator after it stops
/// (the last `users` row is deleted). Gating both sides would strand them.
#[must_use]
pub fn owner_stamp(identity: Option<&str>, distinguishes_operators: bool) -> Option<String> {
    if !distinguishes_operators {
        return None;
    }
    identity
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// Input for inserting a new handoff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewHandoff {
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Session this handoff captures (None for manual handoffs).
    pub from_session_id: Option<SessionId>,
    /// Agent CLI that produced this handoff.
    pub from_agent: AgentKind,
    /// Optional explicit target hint (`claude-code`, `codex`, …).
    pub to_agent: Option<AgentKind>,
    /// Working directory at handoff time. Used to match the next
    /// session's `memory_handoff_accept` call.
    pub cwd: Option<PathBuf>,
    /// One-paragraph summary of where we left off.
    pub summary: String,
    /// Open questions for the next agent.
    pub open_questions: Vec<String>,
    /// Suggested next steps.
    pub next_steps: Vec<String>,
    /// Files touched in the session.
    pub files_touched: Vec<String>,
    /// Operator this handoff belongs to. `None` publishes it to the whole
    /// project (the pre-ownership behaviour, and what a caller with no actor
    /// produces).
    #[serde(default)]
    pub owner_user: Option<String>,
}

/// Materialised view of a handoff row.
#[derive(Debug, Clone, Serialize)]
pub struct Handoff {
    /// Stable identifier.
    pub id: HandoffId,
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Session that produced this handoff, if any.
    pub from_session_id: Option<SessionId>,
    /// Agent CLI that produced this handoff.
    pub from_agent: AgentKind,
    /// Optional target hint.
    pub to_agent: Option<AgentKind>,
    /// Working directory at handoff time.
    pub cwd: Option<String>,
    /// Summary.
    pub summary: String,
    /// Open questions.
    pub open_questions: Vec<String>,
    /// Next steps.
    pub next_steps: Vec<String>,
    /// Files touched.
    pub files_touched: Vec<String>,
    /// State.
    pub state: HandoffState,
    /// Creation timestamp.
    pub created_at: Timestamp,
    /// Agent CLI that accepted, if any.
    pub accepted_by: Option<AgentKind>,
    /// Acceptance timestamp.
    pub accepted_at: Option<Timestamp>,
    /// Session that accepted, if any.
    pub accepted_by_session: Option<SessionId>,
    /// Operator this handoff belongs to; `None` means shared with the project.
    pub owner_user: Option<String>,
    /// Operator that accepted it. Unlike [`Handoff::accepted_by`] (the agent
    /// CLI), this answers "which teammate took the baton".
    pub accepted_by_user: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{OwnerFilter, owner_stamp};
    use crate::ActorContext;

    /// A deployment that cannot tell its operators apart stamps nothing, even
    /// when the request carries `[auth].root_username`. Otherwise the same
    /// operator's HTTP writes land under a name their stdio transport — which
    /// carries no actor at all — filters itself out of.
    #[test]
    fn single_operator_deployment_stamps_no_owner() {
        assert_eq!(owner_stamp(Some("dj"), false), None);
        assert_eq!(owner_stamp(None, false), None);
    }

    /// Once operators are actually told apart, the row belongs to whoever
    /// wrote it — and a caller that names nobody still writes a shared row.
    #[test]
    fn distinguishing_deployment_stamps_the_caller() {
        assert_eq!(owner_stamp(Some("alice"), true), Some("alice".to_string()));
        assert_eq!(owner_stamp(None, true), None);
        assert_eq!(owner_stamp(Some("   "), true), None);
    }

    /// The stamp and the read filter must agree on which field names the
    /// human, or a proxied operator writes rows they cannot read back.
    #[test]
    fn stamp_and_filter_agree_on_a_sub_only_actor() {
        let actor = ActorContext {
            sub: Some("oidc-subject-123".into()),
            ..ActorContext::default()
        };
        let owner = owner_stamp(actor.identity_key(), true);
        assert_eq!(owner, Some("oidc-subject-123".to_string()));
        assert!(OwnerFilter::for_actor_context(&actor).admits(owner.as_deref()));
    }

    /// A handoff with no owner is the pre-ownership shape: every reader,
    /// identified or not, still sees it. This is what keeps single-operator
    /// servers and every already-stored row behaving exactly as before.
    #[test]
    fn shared_handoffs_are_visible_to_every_filter() {
        assert!(OwnerFilter::User("alice".into()).admits(None));
        assert!(OwnerFilter::Unattributed.admits(None));
        assert!(OwnerFilter::Any.admits(None));
    }

    /// The core of the fix: an owned handoff is invisible to anybody else.
    #[test]
    fn owned_handoffs_are_only_visible_to_their_owner() {
        let alice = OwnerFilter::User("alice".into());
        assert!(alice.admits(Some("alice")));
        assert!(!alice.admits(Some("bob")));
    }

    /// A caller with no actor must not drain a baton that belongs to someone —
    /// this is what closes the read-only API and the no-arg accept path.
    #[test]
    fn unattributed_readers_cannot_see_owned_handoffs() {
        assert!(!OwnerFilter::Unattributed.admits(Some("alice")));
    }

    /// Recovery escape hatch stays explicit.
    #[test]
    fn any_sees_every_owner() {
        assert!(OwnerFilter::Any.admits(Some("alice")));
        assert!(OwnerFilter::Any.admits(Some("bob")));
    }

    /// A blank or whitespace-only user is not an identity: it must degrade to
    /// unattributed rather than creating an owner literally named "" that a
    /// second blank-actor caller would then match.
    #[test]
    fn blank_actor_degrades_to_unattributed() {
        assert_eq!(
            OwnerFilter::for_actor(Some("   ")),
            OwnerFilter::Unattributed
        );
        assert_eq!(OwnerFilter::for_actor(None), OwnerFilter::Unattributed);
        assert_eq!(
            OwnerFilter::for_actor(Some("alice")),
            OwnerFilter::User("alice".into())
        );
    }
}
