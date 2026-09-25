//! Per-project authorization choke point (#708).
//!
//! On a multi-user server, attribution ([`ai_memory_core::ActorContext`]) records
//! *who* made a write but does not gate *whether* they were allowed to touch a
//! given project. This module is the single typed decision point that closes
//! that gap. It is consulted by [`crate::ScopeResolver`] read/write resolution
//! (the one place every scoped MCP/API/web path funnels through) and by the
//! writer actor as defense in depth for writes.
//!
//! ## Ship inert (slice 2)
//!
//! The V68 schema is additive and every project defaults to `open`, so this is a
//! pure pass-through until an operator opts a project into `restricted` (a
//! follow-up slice adds the management surface). The gate short-circuits to
//! ALLOW for:
//! - a deployment that does not distinguish operators (single-user / loopback),
//!   where there is no notion of "another user" to gate against, and
//! - any `open` project.
//!
//! ## Never fail closed on a resolution gap (#678)
//!
//! A `restricted` project with **zero** grants still admits root and the
//! project creator — an operator is never locked out of their own data. If the
//! grants table cannot be read (an older schema mid-migration), the decision
//! degrades to `open` with a loud warning rather than denying every read.
//!
//! ## Invariant #16
//!
//! This is an *authorization* check that returns allow/deny for **entry** to a
//! project. It is never an [`ai_memory_core::OwnerFilter`] on page rows and it
//! never scopes page reads by user: a team with grants sees the same shared
//! pages. `pages.author_id` must never become a read filter.

use ai_memory_core::{AuthzError, ProjectId, UserId, WorkspaceId};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::StoreResult;

/// Denial message surfaced when a `restricted` project refuses a caller.
pub const RESTRICTED_PROJECT_FORBIDDEN: &str =
    "project access is restricted; root, the project creator, or a matching grant is required";

/// A project's enforcement mode, stored in `projects.access_mode` (V68).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    /// Today's behaviour: any authenticated user is admitted.
    Open,
    /// Only root, the project creator, and matching grants are admitted.
    Restricted,
}

impl AccessMode {
    /// Stable snake_case token stored in SQL.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AccessMode::Open => "open",
            AccessMode::Restricted => "restricted",
        }
    }

    /// Parse the stored value, degrading anything unexpected to [`Self::Open`].
    ///
    /// A `NULL`/absent column, an unknown token, or a value written by a newer
    /// build all read as `open`. Never fail closed on a value we cannot make
    /// sense of (the #678 degrade-don't-lock-out rule).
    #[must_use]
    pub fn from_db(value: Option<&str>) -> Self {
        match value {
            Some("restricted") => AccessMode::Restricted,
            _ => AccessMode::Open,
        }
    }
}

/// A user's grant level on a project (`project_grants.level`, V68).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantLevel {
    /// Read the project's pages/observations/status.
    Read,
    /// Read plus write/consolidate/handoff/message/delete.
    Write,
}

impl GrantLevel {
    /// Stable snake_case token stored in SQL.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            GrantLevel::Read => "read",
            GrantLevel::Write => "write",
        }
    }

    /// Parse the stored value; an unknown token is no grant.
    #[must_use]
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "read" => Some(GrantLevel::Read),
            "write" => Some(GrantLevel::Write),
            _ => None,
        }
    }
}

/// What a caller is attempting to do in a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectAccess {
    /// A read (query, read page/observations, status, briefing).
    Read,
    /// A write (write page, consolidate, handoff, message, delete).
    Write,
}

/// The caller, reduced to the facts the gate needs.
///
/// Root and creator are supplied by the auth layer; the grant is looked up from
/// `project_grants` by [`Self::user_id`]. No `projects.created_by` column exists
/// yet, so `is_creator` is threaded through the type for complete, testable
/// semantics and populated by the management surface in a follow-up slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPrincipal {
    /// Authenticated as the configured root operator.
    pub is_root: bool,
    /// The creator of this project.
    pub is_creator: bool,
    /// The caller's `users.id`, used to look up a grant. `None` for anonymous
    /// callers and for identities not mapped to a DB user.
    pub user_id: Option<UserId>,
}

impl ProjectPrincipal {
    /// An anonymous, ungranted, non-privileged caller.
    #[must_use]
    pub fn anonymous() -> Self {
        Self {
            is_root: false,
            is_creator: false,
            user_id: None,
        }
    }

    /// A root caller.
    #[must_use]
    pub fn root() -> Self {
        Self {
            is_root: true,
            is_creator: false,
            user_id: None,
        }
    }
}

/// A project's authorization state, fully resolved from SQL, ready to decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectAuthz {
    /// Whether the deployment distinguishes operators (DB users / trusted proxy).
    pub distinguishes_operators: bool,
    /// The project's enforcement mode.
    pub access_mode: AccessMode,
    /// Whether the caller is root.
    pub is_root: bool,
    /// Whether the caller created the project.
    pub is_creator: bool,
    /// The caller's grant on the project, if any.
    pub grant: Option<GrantLevel>,
}

impl ProjectAuthz {
    /// Decide whether the caller may perform `need` in this project.
    ///
    /// - A deployment that does not distinguish operators skips the gate.
    /// - An `open` project admits everyone (today's behaviour).
    /// - A `restricted` project admits root and the creator unconditionally;
    ///   otherwise a read needs any grant and a write needs a `write` grant.
    ///   Anyone else — including an anonymous caller — is [`AuthzError::Forbidden`].
    ///
    /// # Errors
    /// [`AuthzError::Forbidden`] when a restricted project refuses the caller.
    pub fn authorize(&self, need: ProjectAccess) -> Result<(), AuthzError> {
        if !self.distinguishes_operators {
            return Ok(());
        }
        match self.access_mode {
            AccessMode::Open => Ok(()),
            AccessMode::Restricted => {
                if self.is_root || self.is_creator {
                    return Ok(());
                }
                let admitted = match need {
                    // A read or write grant both admit a read.
                    ProjectAccess::Read => self.grant.is_some(),
                    ProjectAccess::Write => self.grant == Some(GrantLevel::Write),
                };
                if admitted {
                    Ok(())
                } else {
                    Err(AuthzError::Forbidden(RESTRICTED_PROJECT_FORBIDDEN))
                }
            }
        }
    }
}

/// The choke point: decide `need` for `ctx`. Free-function form of
/// [`ProjectAuthz::authorize`] so call sites read as `authorize_project(...)`.
///
/// # Errors
/// [`AuthzError::Forbidden`] when a restricted project refuses the caller.
pub fn authorize_project(ctx: &ProjectAuthz, need: ProjectAccess) -> Result<(), AuthzError> {
    ctx.authorize(need)
}

/// Read `projects.access_mode`, degrading to [`AccessMode::Open`] on any read
/// failure and for an unknown project. Never fails closed.
fn read_access_mode(conn: &Connection, project_id: ProjectId) -> AccessMode {
    match conn.query_row(
        "SELECT access_mode FROM projects WHERE id = ?1",
        params![project_id.as_bytes()],
        |row| row.get::<_, Option<String>>(0),
    ) {
        Ok(value) => AccessMode::from_db(value.as_deref()),
        Err(rusqlite::Error::QueryReturnedNoRows) => AccessMode::Open,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "could not read projects.access_mode; degrading project authz to open",
            );
            AccessMode::Open
        }
    }
}

/// Look up a caller's grant on a project. Returns the failure so the caller can
/// decide how to degrade (a restricted project degrades to open when its grants
/// cannot be read).
fn read_grant(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    user_id: UserId,
) -> rusqlite::Result<Option<GrantLevel>> {
    let level: Option<String> = conn
        .query_row(
            "SELECT level FROM project_grants \
             WHERE workspace_id = ?1 AND project_id = ?2 AND user_id = ?3",
            params![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                user_id.as_bytes()
            ],
            |row| row.get(0),
        )
        .optional()?;
    Ok(level.as_deref().and_then(GrantLevel::from_db))
}

/// Resolve the full [`ProjectAuthz`] for a project from a live connection.
///
/// `open` short-circuits without touching `project_grants`. A `restricted`
/// project whose grants cannot be read degrades to `open` with a warning rather
/// than denying every caller (#678).
///
/// # Errors
/// Propagates only failures that are not the grant-read degrade path; the
/// access-mode read itself never errors (it degrades to open).
pub fn resolve_project_authz(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    principal: &ProjectPrincipal,
    distinguishes_operators: bool,
) -> StoreResult<ProjectAuthz> {
    let mut access_mode = read_access_mode(conn, project_id);
    let grant = if access_mode == AccessMode::Restricted {
        match principal.user_id {
            Some(user_id) => match read_grant(conn, workspace_id, project_id, user_id) {
                Ok(grant) => grant,
                Err(err) => {
                    // The grants table cannot be read: degrade to open rather
                    // than lock everyone out of a restricted project (#678).
                    tracing::warn!(
                        error = %err,
                        "could not read project_grants; degrading project authz to open",
                    );
                    access_mode = AccessMode::Open;
                    None
                }
            },
            None => None,
        }
    } else {
        None
    };
    Ok(ProjectAuthz {
        distinguishes_operators,
        access_mode,
        is_root: principal.is_root,
        is_creator: principal.is_creator,
        grant,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(
        distinguishes_operators: bool,
        access_mode: AccessMode,
        is_root: bool,
        is_creator: bool,
        grant: Option<GrantLevel>,
    ) -> ProjectAuthz {
        ProjectAuthz {
            distinguishes_operators,
            access_mode,
            is_root,
            is_creator,
            grant,
        }
    }

    #[test]
    fn access_mode_round_trips_and_degrades_open() {
        assert_eq!(AccessMode::from_db(Some("open")), AccessMode::Open);
        assert_eq!(
            AccessMode::from_db(Some("restricted")),
            AccessMode::Restricted
        );
        // Anything unexpected degrades to open, never fail closed.
        for weird in [None, Some(""), Some("RESTRICTED"), Some("locked")] {
            assert_eq!(AccessMode::from_db(weird), AccessMode::Open, "{weird:?}");
        }
        assert_eq!(AccessMode::Open.as_str(), "open");
        assert_eq!(AccessMode::Restricted.as_str(), "restricted");
    }

    #[test]
    fn grant_level_parse() {
        assert_eq!(GrantLevel::from_db("read"), Some(GrantLevel::Read));
        assert_eq!(GrantLevel::from_db("write"), Some(GrantLevel::Write));
        assert_eq!(GrantLevel::from_db("admin"), None);
    }

    /// The pure decision table. Every row must flip if the gate were removed or
    /// inverted, so this is the adversarial core the SQL wrapper reuses.
    #[test]
    fn authorize_decision_matrix() {
        use AccessMode::{Open, Restricted};
        use GrantLevel::{Read, Write};
        use ProjectAccess as A;

        // (distinguishes, mode, is_root, is_creator, grant, need, expect_ok)
        let rows = [
            // Single-user / loopback: the gate does not exist.
            (false, Restricted, false, false, None, A::Read, true),
            (false, Restricted, false, false, None, A::Write, true),
            // Open: everyone is admitted, even anonymous.
            (true, Open, false, false, None, A::Read, true),
            (true, Open, false, false, None, A::Write, true),
            // Restricted + root: always admitted.
            (true, Restricted, true, false, None, A::Read, true),
            (true, Restricted, true, false, None, A::Write, true),
            // Restricted + creator, zero grants: still admitted.
            (true, Restricted, false, true, None, A::Read, true),
            (true, Restricted, false, true, None, A::Write, true),
            // Restricted + no grant, non-root, non-creator: denied both ways.
            (true, Restricted, false, false, None, A::Read, false),
            (true, Restricted, false, false, None, A::Write, false),
            // Restricted + read grant: read OK, write denied.
            (true, Restricted, false, false, Some(Read), A::Read, true),
            (true, Restricted, false, false, Some(Read), A::Write, false),
            // Restricted + write grant: both OK.
            (true, Restricted, false, false, Some(Write), A::Read, true),
            (true, Restricted, false, false, Some(Write), A::Write, true),
        ];

        for (distinguishes, mode, root, creator, grant, need, expect_ok) in rows {
            let decision = authorize_project(&ctx(distinguishes, mode, root, creator, grant), need);
            assert_eq!(
                decision.is_ok(),
                expect_ok,
                "row d={distinguishes} mode={mode:?} root={root} creator={creator} \
                 grant={grant:?} need={need:?}",
            );
            if !expect_ok {
                assert_eq!(
                    decision.unwrap_err(),
                    AuthzError::Forbidden(RESTRICTED_PROJECT_FORBIDDEN),
                );
            }
        }
    }
}
