//! Per-project authorization choke point (#708).
//!
//! On a multi-user server, attribution ([`ai_memory_core::ActorContext`]) records
//! *who* made a write but does not gate *whether* they were allowed to touch a
//! given project. This module is the single typed decision point that closes
//! that gap. It is consulted by [`crate::ScopeResolver`] read/write resolution
//! (the one place every scoped MCP/API/web path funnels through) and by the
//! writer actor as defense in depth for writes.
//!
//! ## Open until an operator restricts
//!
//! The V68 schema is additive and every project defaults to `open`, so this is a
//! pure pass-through until the root operator opts a project into `restricted`
//! (`ai-memory project access`), or sets `[auth] new_projects_restricted`. The
//! gate short-circuits to ALLOW for:
//! - a deployment that does not distinguish operators (single-user / loopback),
//!   where there is no notion of "another user" to gate against, and
//! - any `open` project.
//!
//! ## The reserved shared scopes are read-open, write-gated
//!
//! `default/_global` and a workspace's `_profile` stay `open` because their
//! pages are unioned into every project's reads. Writing it therefore reaches every other user's results,
//! so on a deployment that distinguishes operators a write needs root or an
//! explicit `write` grant on it, whatever its access mode. Creating it first
//! grants nothing: the first writer must not become a permanent one.
//!
//! ## Compatibility gaps are not store failures
//!
//! A `restricted` project with **zero** grants still admits root and the
//! project creator — an operator is never locked out of their own data. If the
//! Unknown projects and access-mode values remain compatible with older/newer
//! callers by reading as `open`. A store failure while reading an existing
//! project's access mode or grants propagates and refuses the request: an
//! authorization dependency must never fail open.
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

/// Denial message surfaced when a write to the reserved global scope is refused.
pub const GLOBAL_SCOPE_WRITE_FORBIDDEN: &str = "the global scope is shared with every project; writing it requires root or a write grant on it";

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
/// Root is supplied by the auth layer; the grant is looked up from
/// `project_grants` by [`Self::user_id`]. The creator is derived per project
/// from `projects.created_by` (V69) when [`resolve_project_authz`] runs, so a
/// principal built once per request is right for every project it reaches;
/// [`Self::is_creator`] only forces it on.
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

    /// A database user: not root, and the creator only of the projects whose
    /// `created_by` names them.
    #[must_use]
    pub fn user(user_id: UserId) -> Self {
        Self {
            is_root: false,
            is_creator: false,
            user_id: Some(user_id),
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
    /// The caller's grant on the project's whole workspace, if any
    /// (`workspace_grants`, V115). It counts as a grant at that level on every
    /// project of the workspace, except for a write to a reserved shared scope.
    pub workspace_grant: Option<GrantLevel>,
    /// The project is a shared reserved scope: the global preferences scope
    /// (`default/_global`) or a workspace profile (`<workspace>/_profile`).
    pub reserved_global: bool,
}

impl ProjectAuthz {
    /// Decide whether the caller may perform `need` in this project.
    ///
    /// - A deployment that does not distinguish operators skips the gate.
    /// - A write to the reserved global scope needs root or a `write` grant
    ///   on that project, in either mode; its creator gets nothing extra, and
    ///   neither does a workspace grant: a workspace profile reaches every
    ///   user of the workspace, so writing it stays a per-project decision.
    /// - An `open` project admits everyone (today's behaviour).
    /// - A `restricted` project admits root and the creator unconditionally;
    ///   otherwise a read needs any grant and a write needs a `write` grant,
    ///   held on the project or on its workspace.
    ///   Anyone else — including an anonymous caller — is [`AuthzError::Forbidden`].
    ///
    /// # Errors
    /// [`AuthzError::Forbidden`] when a restricted project refuses the caller.
    pub fn authorize(&self, need: ProjectAccess) -> Result<(), AuthzError> {
        if !self.distinguishes_operators {
            return Ok(());
        }
        if self.reserved_global && need == ProjectAccess::Write {
            return if self.is_root || self.grant == Some(GrantLevel::Write) {
                Ok(())
            } else {
                Err(AuthzError::Forbidden(GLOBAL_SCOPE_WRITE_FORBIDDEN))
            };
        }
        match self.access_mode {
            AccessMode::Open => Ok(()),
            AccessMode::Restricted => {
                if self.is_root || self.is_creator {
                    return Ok(());
                }
                // A project grant and a workspace grant both count; the
                // higher level decides.
                let admitted = match need {
                    // A read or write grant both admit a read.
                    ProjectAccess::Read => self.grant.is_some() || self.workspace_grant.is_some(),
                    ProjectAccess::Write => {
                        self.grant == Some(GrantLevel::Write)
                            || self.workspace_grant == Some(GrantLevel::Write)
                    }
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

/// The facts [`resolve_project_authz`] reads from a project's row.
struct ProjectRow {
    access_mode: AccessMode,
    creator: Option<UserId>,
    reserved_global: bool,
}

/// Read `projects.access_mode`, `projects.created_by` and whether the project
/// is the reserved global scope. An unknown project remains compatible with
/// older callers by reading as [`AccessMode::Open`]; store failures propagate.
fn read_project_row(conn: &Connection, project_id: ProjectId) -> StoreResult<ProjectRow> {
    match conn.query_row(
        "SELECT p.access_mode, p.created_by, p.name, w.name \
         FROM projects p JOIN workspaces w ON w.id = p.workspace_id WHERE p.id = ?1",
        params![project_id.as_bytes()],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<Vec<u8>>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        },
    ) {
        Ok((mode, creator, project, workspace)) => Ok(ProjectRow {
            access_mode: AccessMode::from_db(mode.as_deref()),
            creator: creator.and_then(|raw| UserId::from_slice(&raw).ok()),
            reserved_global: is_reserved_global(&workspace, &project),
        }),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(ProjectRow {
            access_mode: AccessMode::Open,
            creator: None,
            reserved_global: false,
        }),
        Err(err) => Err(err.into()),
    }
}

/// [`read_project_row`] without degrading: the project must exist in the
/// given workspace, its access mode must be a known value and its creator a
/// well-formed id.
fn read_project_row_strict(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> StoreResult<ProjectRow> {
    let (mode, creator, project, workspace) = conn.query_row(
        "SELECT p.access_mode, p.created_by, p.name, w.name FROM projects p \
         JOIN workspaces w ON w.id = p.workspace_id \
         WHERE p.workspace_id = ?1 AND p.id = ?2",
        params![workspace_id.as_bytes(), project_id.as_bytes()],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<Vec<u8>>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        },
    )?;
    let access_mode = match mode.as_str() {
        "open" => AccessMode::Open,
        "restricted" => AccessMode::Restricted,
        _ => {
            return Err(crate::StoreError::MalformedRecord(
                "unknown project access mode".into(),
            ));
        }
    };
    Ok(ProjectRow {
        access_mode,
        creator: creator.map(|raw| UserId::from_slice(&raw)).transpose()?,
        reserved_global: is_reserved_global(&workspace, &project),
    })
}

/// The shared reserved scopes every project's reads union in: the global
/// preferences scope, and a workspace's `_profile`. A private profile is not
/// one of them; it is restricted to its creator instead.
fn is_reserved_global(workspace: &str, project: &str) -> bool {
    (workspace == ai_memory_core::DEFAULT_WORKSPACE_NAME
        && project == ai_memory_core::GLOBAL_SCOPE_PROJECT)
        || project == ai_memory_core::profile::WORKSPACE_PROFILE_PROJECT
}

/// Look up a caller's grant on a project. Returns the failure so the caller can
/// preserve the reserved global scope's read-open/write-denied degraded mode.
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

/// Look up a caller's grant on the whole workspace a project lives in
/// (`workspace_grants`, V115).
///
/// The join is on the project's own `workspace_id`, and the caller's
/// `workspace_id` must equal it: a grant on one workspace never admits a
/// project of another, whatever pair of ids a request carries.
fn read_workspace_grant(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    user_id: UserId,
) -> rusqlite::Result<Option<GrantLevel>> {
    let level: Option<String> = conn
        .query_row(
            "SELECT wg.level FROM workspace_grants wg \
               JOIN projects p ON p.workspace_id = wg.workspace_id \
              WHERE p.id = ?2 AND wg.workspace_id = ?1 AND wg.user_id = ?3",
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
/// `open` short-circuits without touching `project_grants`. The caller is the
/// creator when [`ProjectPrincipal::is_creator`] says so or the project's
/// `created_by` is their [`ProjectPrincipal::user_id`]. A `restricted`
/// project's grant lookup failure propagates and refuses the request.
///
/// # Errors
/// Propagates store failures while retaining compatibility for an unknown
/// project or access-mode token.
pub fn resolve_project_authz(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    principal: &ProjectPrincipal,
    distinguishes_operators: bool,
) -> StoreResult<ProjectAuthz> {
    resolve(
        conn,
        workspace_id,
        project_id,
        principal,
        distinguishes_operators,
        Resolution::Compatible,
    )
}

/// [`resolve_project_authz`] for a managed-run write that releases or replaces
/// another operator's state (finish, cancel, heartbeat, link, context
/// acceptance). The decision is the same; only a resolution gap differs: an
/// unreadable or unknown project, an unknown access mode, a deleted user or an
/// unreadable grant is an error instead of degrading to `open`, and a server
/// with database users distinguishes operators even when the caller's auth
/// level does not say so.
pub(crate) fn resolve_project_authz_strict(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    principal: &ProjectPrincipal,
    distinguishes_operators: bool,
) -> StoreResult<ProjectAuthz> {
    resolve(
        conn,
        workspace_id,
        project_id,
        principal,
        distinguishes_operators,
        Resolution::Strict,
    )
}

/// How [`resolve`] treats a gap in what it can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resolution {
    /// Preserve compatibility for absent rows and unknown access-mode tokens.
    Compatible,
    /// Fail the request.
    Strict,
}

fn resolve(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    principal: &ProjectPrincipal,
    mut distinguishes_operators: bool,
    resolution: Resolution,
) -> StoreResult<ProjectAuthz> {
    let ProjectRow {
        access_mode,
        creator,
        reserved_global,
    } = match resolution {
        Resolution::Compatible => read_project_row(conn, project_id)?,
        Resolution::Strict => {
            let row = read_project_row_strict(conn, workspace_id, project_id)?;
            distinguishes_operators |= crate::users::users_exist(conn)?;
            if let Some(user_id) = principal.user_id {
                let exists: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM users WHERE id = ?1)",
                    params![user_id.as_bytes()],
                    |row| row.get(0),
                )?;
                if !exists {
                    return Err(crate::StoreError::Forbidden(
                        "authenticated user no longer exists",
                    ));
                }
            }
            row
        }
    };
    let is_creator = principal.is_creator || (creator.is_some() && creator == principal.user_id);
    let grant = if reserved_global || access_mode == AccessMode::Restricted {
        match principal.user_id {
            Some(user_id) => match read_grant(conn, workspace_id, project_id, user_id) {
                Ok(grant) => grant,
                Err(err) if resolution == Resolution::Strict => return Err(err.into()),
                // The global scope's write gate needs the grant in either
                // mode; an unreadable grant admits no write there, while reads
                // stay open.
                Err(err) if reserved_global => {
                    tracing::warn!(
                        error = %err,
                        "could not read project_grants for the global scope; refusing writes",
                    );
                    None
                }
                Err(err) => return Err(err.into()),
            },
            None => None,
        }
    } else {
        None
    };
    // Only a restricted project consults grants for entry; the reserved
    // scopes' write gate deliberately ignores the workspace grant.
    let workspace_grant = match principal.user_id {
        Some(user_id) if access_mode == AccessMode::Restricted => {
            match read_workspace_grant(conn, workspace_id, project_id, user_id) {
                Ok(grant) => grant,
                Err(err) if resolution == Resolution::Strict => return Err(err.into()),
                // Same degraded mode as the project grant above: no grant is
                // invented for a table that cannot be read.
                Err(err) if reserved_global => {
                    tracing::warn!(
                        error = %err,
                        "could not read workspace_grants for a shared scope; admitting no workspace grant",
                    );
                    None
                }
                Err(err) => return Err(err.into()),
            }
        }
        _ => None,
    };
    Ok(ProjectAuthz {
        distinguishes_operators,
        access_mode,
        is_root: principal.is_root,
        is_creator,
        grant,
        workspace_grant,
        reserved_global,
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
            workspace_grant: None,
            reserved_global: false,
        }
    }

    /// The strict resolution managed-run mutations use is the canonical
    /// decision, not a parallel one: the reserved global scope's write gate
    /// holds there too, and only a resolution gap is treated differently (an
    /// unknown project fails instead of degrading to open).
    #[tokio::test]
    async fn strict_resolution_shares_the_global_gate_and_fails_closed_on_a_gap() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::Store::open(tmp.path()).unwrap();
        let global = crate::create_global_scope(&store.writer).await.unwrap();
        let user = store
            .writer
            .create_human_user(
                ai_memory_core::NewUser {
                    username: "alice".into(),
                    name: None,
                    email: None,
                },
                ai_memory_core::UserRole::User,
                None,
                false,
            )
            .await
            .unwrap();
        let conn = Connection::open(store.db_path()).unwrap();
        let principal = ProjectPrincipal::user(user);

        let strict = resolve_project_authz_strict(
            &conn,
            global.workspace_id,
            global.project_id,
            &principal,
            false,
        )
        .unwrap();
        assert!(strict.reserved_global);
        assert!(
            strict.distinguishes_operators,
            "a server with database users distinguishes operators"
        );
        assert!(strict.authorize(ProjectAccess::Write).is_err());
        assert!(strict.authorize(ProjectAccess::Read).is_ok());

        let unknown = ProjectId::new();
        assert!(
            resolve_project_authz_strict(&conn, global.workspace_id, unknown, &principal, true)
                .is_err()
        );
        let lenient =
            resolve_project_authz(&conn, global.workspace_id, unknown, &principal, true).unwrap();
        assert_eq!(lenient.access_mode, AccessMode::Open);
    }

    #[tokio::test]
    async fn an_unreadable_restricted_project_fails_instead_of_opening() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::Store::open(tmp.path()).unwrap();
        let workspace = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let project = store
            .writer
            .get_or_create_project(workspace, "secret", None)
            .await
            .unwrap();
        store
            .writer
            .set_access_mode(project, AccessMode::Restricted)
            .await
            .unwrap();
        let user = store
            .writer
            .create_human_user(
                ai_memory_core::NewUser {
                    username: "outsider".into(),
                    name: None,
                    email: None,
                },
                ai_memory_core::UserRole::User,
                None,
                false,
            )
            .await
            .unwrap();
        let principal = ProjectPrincipal::user(user);
        let conn = Connection::open(store.db_path()).unwrap();

        let control = resolve_project_authz(&conn, workspace, project, &principal, true).unwrap();
        assert_eq!(control.access_mode, AccessMode::Restricted);
        assert!(control.authorize(ProjectAccess::Read).is_err());

        conn.execute_batch("ALTER TABLE project_grants RENAME TO unavailable_grants;")
            .unwrap();
        assert!(resolve_project_authz(&conn, workspace, project, &principal, true).is_err());
        conn.execute_batch("ALTER TABLE unavailable_grants RENAME TO project_grants;")
            .unwrap();

        conn.execute_batch("ALTER TABLE projects RENAME TO unavailable_projects;")
            .unwrap();
        assert!(resolve_project_authz(&conn, workspace, project, &principal, true).is_err());
    }

    /// GHSA-7qj3-7wqw-m5w6: writing the reserved global scope needs root or a
    /// write grant in either mode; its creator and an open mode admit nothing
    /// extra, reads stay open, and an install without operators is unchanged.
    #[test]
    fn the_global_scope_admits_writes_only_from_root_or_a_write_grant() {
        let global = |mode, is_root, is_creator, grant| ProjectAuthz {
            reserved_global: true,
            ..ctx(true, mode, is_root, is_creator, grant)
        };
        for mode in [AccessMode::Open, AccessMode::Restricted] {
            let refused = [
                global(mode, false, false, None),
                global(mode, false, true, None),
                global(mode, false, false, Some(GrantLevel::Read)),
            ];
            for ctx in refused {
                assert_eq!(
                    ctx.authorize(ProjectAccess::Write),
                    Err(AuthzError::Forbidden(GLOBAL_SCOPE_WRITE_FORBIDDEN)),
                    "{ctx:?}"
                );
            }
            assert!(
                global(mode, true, false, None)
                    .authorize(ProjectAccess::Write)
                    .is_ok()
            );
            assert!(
                global(mode, false, false, Some(GrantLevel::Write))
                    .authorize(ProjectAccess::Write)
                    .is_ok()
            );
        }
        assert!(
            global(AccessMode::Open, false, false, None)
                .authorize(ProjectAccess::Read)
                .is_ok()
        );
        let single_user = ProjectAuthz {
            distinguishes_operators: false,
            ..global(AccessMode::Open, false, false, None)
        };
        assert!(single_user.authorize(ProjectAccess::Write).is_ok());
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
