//! Store operations for optional `ai-memory run` workstreams.

use std::str::FromStr as _;

use ai_memory_core::{
    AgentKind, ManagedRunId, NativeSessionIdentity, NewWorkstreamEvent, ProjectId, Sanitizer,
    WorkspaceId, WorkstreamEvent, WorkstreamEventKind, WorkstreamId, scrub_workstream_provenance,
};
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};

use crate::{StoreError, StoreResult};

const LEASE_MICROS: i64 = 90 * 1_000_000;
const MAX_WORKSTREAM_NAME_CHARS: usize = 128;
/// Store-boundary bound on a workstream event's free-text `content`
/// (16 KiB, matching the observation body). A backstop against unbounded
/// persistence, not a functional cap — the hook layer caps tighter.
const WORKSTREAM_CONTENT_MAX_BYTES: usize = 16 * 1024;

/// How a prepare request chooses the workstream for a repository/worktree.
#[derive(Debug, Clone)]
pub enum WorkstreamSelection {
    /// Continue the most recently selected workstream, creating `default` when
    /// the worktree has never used managed mode.
    Current,
    /// Select an existing named workstream.
    Named(String),
    /// Create and select a fresh named workstream.
    New(String),
}

impl WorkstreamSelection {
    /// Validate caller-controlled selection data before any scope mutation.
    pub fn validate(&self) -> StoreResult<()> {
        match self {
            Self::Current => Ok(()),
            Self::Named(name) | Self::New(name) => validate_workstream_name(name),
        }
    }
}

/// Authenticated authority for a managed-run mutation: finish (its preflight
/// and its import transaction), cancel, heartbeat, native-session link and
/// context acceptance.
///
/// Construct only from middleware extensions, never transcript metadata. Owner
/// filters are derived here so this boundary cannot request `OwnerFilter::Any`.
#[derive(Debug, Clone)]
pub struct ManagedRunAuthority {
    principal: crate::ProjectPrincipal,
    owner: ai_memory_core::OwnerFilter,
    distinguishes_operators: bool,
    project_policy: RunProjectPolicy,
}

#[derive(Debug, Clone, Copy)]
enum RunProjectPolicy {
    Grants,
    TrustedProxyLegacy,
}

impl ManagedRunAuthority {
    /// Reduce the actual HTTP auth extensions to one immutable finish authority.
    /// A DB user's attribution id remains authoritative without a viewer marker.
    #[must_use]
    pub fn from_auth(
        level: ai_memory_core::AuthLevel,
        viewer: Option<ai_memory_core::AuthorizedViewer>,
        user_id: Option<ai_memory_core::UserId>,
        actor: &ai_memory_core::ActorContext,
        trusted_proxy_identity: bool,
    ) -> Self {
        use ai_memory_core::AuthLevel;
        let user = viewer
            .map(ai_memory_core::AuthorizedViewer::user)
            .or(user_id);
        let principal = match level {
            AuthLevel::Root => crate::ProjectPrincipal::root(),
            AuthLevel::User => user.map_or_else(
                crate::ProjectPrincipal::anonymous,
                crate::ProjectPrincipal::user,
            ),
            AuthLevel::Anonymous => crate::ProjectPrincipal::anonymous(),
        };
        let owner = if level == AuthLevel::Anonymous {
            ai_memory_core::OwnerFilter::Unattributed
        } else {
            ai_memory_core::OwnerFilter::for_actor_context(actor)
        };
        // The real proxy middleware has an identity and User capability, but
        // no DB principal. Preserve its viewer-less project policy only when
        // this server actually enables trusted proxy authentication.
        let project_policy = match (level, user, actor.identity_key(), trusted_proxy_identity) {
            (AuthLevel::User, None, Some(_), true) => RunProjectPolicy::TrustedProxyLegacy,
            _ => RunProjectPolicy::Grants,
        };
        Self {
            principal,
            owner,
            distinguishes_operators: trusted_proxy_identity || level == AuthLevel::User,
            project_policy,
        }
    }
}

/// Resolve only the real run scope and owner, without creating a scope or
/// changing the active-project pointer. Both callers supply a SQL snapshot.
pub(crate) fn authorize_run_mutation(
    conn: &Connection,
    run_id: ManagedRunId,
    authority: &ManagedRunAuthority,
) -> StoreResult<()> {
    let (owner, workspace, project) = conn
        .query_row(
            "SELECT r.owner_user, w.workspace_id, w.project_id FROM managed_runs r \
             LEFT JOIN workstreams w ON w.id = r.workstream_id WHERE r.id = ?1",
            params![run_id.as_bytes()],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound("managed run not found".into()))?;
    if !authority.owner.admits(owner.as_deref()) {
        return Err(StoreError::Forbidden(
            "managed run belongs to another operator",
        ));
    }
    let workspace = WorkspaceId::from_slice(&workspace)?;
    let project = ProjectId::from_slice(&project)?;
    let project_authz = crate::project_authz::resolve_project_authz_strict(
        conn,
        workspace,
        project,
        &authority.principal,
        authority.distinguishes_operators,
    )?;
    match authority.project_policy {
        RunProjectPolicy::TrustedProxyLegacy => Ok(()),
        RunProjectPolicy::Grants => project_authz
            .authorize(crate::ProjectAccess::Write)
            .map_err(|failure| StoreError::Forbidden(failure.message())),
    }
}

/// Store-level input for opening a managed run.
#[derive(Debug, Clone)]
pub struct PrepareWorkstreamRun {
    /// Resolved workspace.
    pub workspace_id: WorkspaceId,
    /// Resolved project.
    pub project_id: ProjectId,
    /// Repository identity hash.
    pub repo_fingerprint: String,
    /// Worktree identity hash.
    pub worktree_fingerprint: String,
    /// Host working directory.
    pub cwd: String,
    /// Harness being launched.
    pub agent: AgentKind,
    /// Prefer the newest linked harness that is also available locally.
    pub automatic_harness: bool,
    /// Harnesses with checkout-local resumable sessions.
    pub available_agents: Vec<AgentKind>,
    /// Workstream selection behavior.
    pub selection: WorkstreamSelection,
    /// Diagnostic lease owner.
    pub lease_owner: String,
}

/// Store-level result for a prepared managed run.
#[derive(Debug, Clone)]
pub struct PreparedWorkstreamRun {
    /// Selected workstream.
    pub workstream_id: WorkstreamId,
    /// Selected name.
    pub workstream_name: String,
    /// New invocation/lease id.
    pub run_id: ManagedRunId,
    /// Harness selected for this invocation.
    pub agent: AgentKind,
    /// Current native session for the requested harness.
    pub native_session_id: Option<String>,
    /// Last adapter cursor for that native session.
    pub source_cursor: Option<String>,
    /// Last sequence already delivered to that native session.
    pub sync_after: i64,
    /// Workstream high-water mark assigned to the launch.
    pub sync_through: i64,
    /// Whether no harness has established this workstream yet, allowing the
    /// launcher to offer checkout-local native session adoption.
    pub may_adopt_existing_session: bool,
}

/// Result of binding a SessionStart to an exact or recovered managed run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedRunSessionLink {
    /// The supplied run was active and accepted the native session.
    Exact(ManagedRunId),
    /// A stale supplied id was replaced by the sole safe current candidate.
    Adopted(ManagedRunId),
    /// No live run matched the current repository and operator.
    NoMatch,
    /// More than one run matched, so choosing one would be unsafe.
    Ambiguous,
    /// The supplied active run disagreed with the request boundary.
    Refused,
}

/// Exact boundary used to link or recover one managed SessionStart.
#[derive(Debug, Clone)]
pub struct LinkOrAdoptManagedRunSession {
    /// Run id supplied by the child hook; it may be terminal for Codex.
    pub supplied_run_id: ManagedRunId,
    /// Already-resolved workspace receiving the SessionStart.
    pub workspace_id: WorkspaceId,
    /// Already-resolved project receiving the SessionStart.
    pub project_id: ProjectId,
    /// Canonical checkout cwd recorded by both launcher and hook.
    pub cwd: String,
    /// Harness reporting the native session.
    pub agent: AgentKind,
    /// Harness-native session id to bind exactly once.
    pub native_session_id: String,
    /// Topology-aware qualified operator identity, or shared `None`.
    pub owner_user: Option<String>,
}

/// Scope a later hook event of a managed run resolved to.
///
/// The SessionStart binding goes through [`LinkOrAdoptManagedRunSession`];
/// every later event carrying the run id goes through this, and both enforce
/// the same run boundary.
#[derive(Debug, Clone)]
pub struct LinkManagedRunSessionInScope {
    /// Run id the hook event carried.
    pub run_id: ManagedRunId,
    /// Workspace the event was admitted to.
    pub workspace_id: WorkspaceId,
    /// Project the event was admitted to.
    pub project_id: ProjectId,
    /// Harness reporting the native session.
    pub agent: AgentKind,
    /// Harness-native session id to bind.
    pub native_session_id: String,
    /// Topology-aware qualified operator identity, or shared `None`.
    pub owner_user: Option<String>,
}

/// Store-level finish input after the raw segment has been made durable.
#[derive(Debug, Clone)]
pub struct FinishWorkstreamRun {
    /// Configured, reusable privacy strip for the provenance storage boundary.
    pub sanitizer: Sanitizer,
    /// Managed invocation.
    pub run_id: ManagedRunId,
    /// Actual native session, when observed.
    pub native_session_id: Option<String>,
    /// Adapter cursor after export.
    pub source_cursor: Option<String>,
    /// Sanitized normalized events.
    pub events: Vec<NewWorkstreamEvent>,
    /// Close the run and advance its source cursor after this batch.
    pub complete: bool,
    /// Relative immutable raw-segment path.
    pub segment_path: Option<String>,
    /// Child exit code when available.
    pub exit_code: Option<i32>,
}

/// Result of an idempotent finish operation.
#[derive(Debug, Clone, Copy)]
pub struct FinishedWorkstreamRun {
    /// Number of newly indexed events.
    pub imported_events: usize,
    /// Current workstream high-water mark.
    pub latest_sequence: i64,
}

/// State needed to render the SessionStart synchronization packet.
#[derive(Debug, Clone)]
pub struct ManagedRunContext {
    /// Invocation id.
    pub run_id: ManagedRunId,
    /// Workstream id.
    pub workstream_id: WorkstreamId,
    /// Human-readable workstream name.
    pub workstream_name: String,
    /// Harness being launched.
    pub agent: AgentKind,
    /// Linked native session when SessionStart has already reported it.
    pub native_session_id: Option<String>,
    /// Lower delivery cursor.
    pub sync_after: i64,
    /// Assigned high-water mark.
    pub sync_through: i64,
    /// Whether context has already been returned for this invocation.
    pub context_delivered: bool,
    /// Repository checkpoint recorded by the latest event, if any.
    pub events: Vec<WorkstreamEvent>,
}

/// Lightweight status used by the host wrapper after the child exits.
#[derive(Debug, Clone)]
pub struct StoredManagedRunStatus {
    /// Invocation id.
    pub run_id: ManagedRunId,
    /// Workstream id.
    pub workstream_id: WorkstreamId,
    /// Harness.
    pub agent: AgentKind,
    /// Native session observed by hooks.
    pub native_session_id: Option<String>,
    /// Whether a session was linked during this run, rather than carried
    /// over from the workstream when the run was prepared.
    pub native_session_linked: bool,
    /// SessionStart delivery acknowledgement.
    pub context_delivered: bool,
    /// State string.
    pub state: String,
}

/// Checkout-local workstream metadata used by read-only discovery surfaces.
#[derive(Debug, Clone)]
pub struct StoredWorkstreamSummary {
    /// Stable workstream identifier.
    pub workstream_id: WorkstreamId,
    /// Human-readable workstream name.
    pub name: String,
    /// Creation timestamp in microseconds since the Unix epoch.
    pub created_at: i64,
    /// Most recent selection or import timestamp in microseconds.
    pub last_active_at: i64,
    /// Whether this is the checkout's most recently selected workstream.
    pub current: bool,
    /// Harnesses with a current native session, newest link first.
    pub linked_harnesses: Vec<AgentKind>,
}

/// How a rename addresses the workstream it retitles.
///
/// Both forms stay inside the caller's resolved scope: an id that belongs to
/// another workspace, project, or worktree is treated as absent rather than
/// renamed, so a guessed identifier cannot reach across the scope boundary
/// that `workstreams` is keyed on.
#[derive(Debug, Clone)]
pub enum WorkstreamSelector {
    /// Address by current name, unique within one checkout.
    Name(String),
    /// Address by stable id, as printed by the discovery listing.
    Id(WorkstreamId),
}

/// Store-level input for retitling one managed workstream.
#[derive(Debug, Clone)]
pub struct RenameWorkstream {
    /// Workspace holding the workstream.
    pub workspace_id: WorkspaceId,
    /// Project holding the workstream.
    pub project_id: ProjectId,
    /// Stable repository identity hash.
    pub repo_fingerprint: String,
    /// Stable worktree identity hash.
    pub worktree_fingerprint: String,
    /// Which workstream to retitle.
    pub selector: WorkstreamSelector,
    /// Replacement name, validated exactly like a `--new` name.
    pub new_name: String,
}

/// Outcome of a successful rename.
#[derive(Debug, Clone)]
pub struct RenamedWorkstream {
    /// The workstream that was retitled.
    pub workstream_id: WorkstreamId,
    /// Name before the rename.
    pub from: String,
    /// Name after the rename.
    pub to: String,
}

struct FinishRunRow {
    workstream: Vec<u8>,
    agent_wire: String,
    linked_session: Option<String>,
    state: String,
    sync_after: i64,
    sync_through: i64,
    context_delivered: bool,
    exit_code: Option<i32>,
}

struct LinkRunRow {
    workstream: Vec<u8>,
    agent: String,
    native_session: Option<String>,
    native_session_linked: bool,
    sync_through: i64,
    context_delivered: bool,
}

struct ManagedRunBoundaryRow {
    state: String,
    workspace: Vec<u8>,
    project: Vec<u8>,
    cwd: String,
    agent: String,
    owner_user: Option<String>,
    native_session: Option<String>,
}

impl ManagedRunBoundaryRow {
    const SELECT: &str = "SELECT mr.state, w.workspace_id, w.project_id, w.cwd, mr.agent_kind, \
                mr.owner_user, mr.native_session_id \
         FROM managed_runs mr JOIN workstreams w ON w.id = mr.workstream_id \
         WHERE mr.id = ?1";

    fn read(tx: &Transaction<'_>, run_id: ManagedRunId) -> StoreResult<Option<Self>> {
        Ok(tx
            .query_row(Self::SELECT, params![run_id.as_bytes()], |row| {
                Ok(Self {
                    state: row.get(0)?,
                    workspace: row.get(1)?,
                    project: row.get(2)?,
                    cwd: row.get(3)?,
                    agent: row.get(4)?,
                    owner_user: row.get(5)?,
                    native_session: row.get(6)?,
                })
            })
            .optional()?)
    }

    /// The scope and operator a native session may join this run from: the
    /// run's own project and its launching operator (a shared run admits only
    /// shared callers). SessionStart also pins the checkout `cwd`; a later
    /// event's `cwd` follows the agent around the checkout, so it is not
    /// compared there.
    fn admits(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        cwd: Option<&str>,
        owner_user: Option<&str>,
    ) -> bool {
        self.workspace.as_slice() == workspace_id.as_bytes()
            && self.project.as_slice() == project_id.as_bytes()
            && cwd.is_none_or(|cwd| self.cwd == cwd)
            && self.owner_user.as_deref() == owner_user
    }
}

/// Atomically select a workstream, expire stale leases, and open one run.
pub(crate) fn prepare_run(
    conn: &mut Connection,
    input: &PrepareWorkstreamRun,
    owner_user: Option<&str>,
    force_unlock: bool,
) -> StoreResult<PreparedWorkstreamRun> {
    if owner_user
        .is_some_and(|owner| ai_memory_core::IdentityKey::from_storage_key(owner).is_none())
    {
        return Err(StoreError::InvalidState(
            "invalid managed-run owner identity".into(),
        ));
    }
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE managed_runs SET state = 'expired', ended_at = ?1 \
         WHERE state = 'active' AND lease_expires_at <= ?1",
        params![now],
    )?;

    let (workstream_id, workstream_name) = select_workstream(&tx, input, now)?;
    let busy: Option<(Vec<u8>, String, i64, Option<String>)> = tx
        .query_row(
            "SELECT id, lease_owner, lease_expires_at, owner_user FROM managed_runs \
             WHERE workstream_id = ?1 AND state = 'active'",
            params![workstream_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    if let Some((run_id, owner, expires, active_owner_user)) = busy {
        if force_unlock && active_owner_user.as_deref() == owner_user {
            tx.execute(
                "UPDATE managed_runs SET state = 'expired', ended_at = ?1, \
                 lease_expires_at = ?1 WHERE id = ?2 AND state = 'active'",
                params![now, run_id],
            )?;
        } else {
            return Err(StoreError::WorkstreamBusy(format!(
                "owned by {owner} until {}",
                Timestamp::from_microsecond(expires)
                    .map(|t| t.to_string())
                    .unwrap_or_else(|_| expires.to_string())
            )));
        }
    }

    let latest_sequence: i64 = tx.query_row(
        "SELECT COALESCE(MAX(sequence), 0) FROM workstream_events WHERE workstream_id = ?1",
        params![workstream_id.as_bytes()],
        |row| row.get(0),
    )?;
    let agent = if input.automatic_harness {
        newest_available_agent(&tx, workstream_id, &input.available_agents)?.unwrap_or(input.agent)
    } else {
        input.agent
    };
    let native: Option<(String, Option<String>, i64)> = tx
        .query_row(
            "SELECT native_session_id, source_cursor, delivery_cursor \
             FROM workstream_native_sessions \
             WHERE workstream_id = ?1 AND agent_kind = ?2 AND is_current = 1",
            params![workstream_id.as_bytes(), agent.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let (native_session_id, source_cursor, sync_after) = native
        .map_or((None, None, 0), |(session, cursor, delivery)| {
            (Some(session), cursor, delivery)
        });
    let established: i64 = tx.query_row(
        "SELECT CASE WHEN \
             EXISTS(SELECT 1 FROM workstream_native_sessions WHERE workstream_id = ?1) \
             OR EXISTS(SELECT 1 FROM workstream_events \
                       WHERE workstream_id = ?1 \
                         AND kind IN ('message', 'tool_call', 'tool_result', 'compaction')) \
             THEN 1 ELSE 0 END",
        params![workstream_id.as_bytes()],
        |row| row.get(0),
    )?;

    let run_id = ManagedRunId::new();
    tx.execute(
        "INSERT INTO managed_runs( \
             id, workstream_id, agent_kind, lease_owner, native_session_id, owner_user, state, \
             sync_after, sync_through, context_delivered, lease_expires_at, started_at \
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, 0, ?9, ?10)",
        params![
            run_id.as_bytes(),
            workstream_id.as_bytes(),
            agent.as_str(),
            input.lease_owner,
            native_session_id,
            owner_user,
            sync_after,
            latest_sequence,
            now + LEASE_MICROS,
            now,
        ],
    )?;
    tx.execute(
        "UPDATE workstreams SET selected_at = ?1, updated_at = ?1, cwd = ?2 WHERE id = ?3",
        params![now, input.cwd, workstream_id.as_bytes()],
    )?;
    tx.commit()?;

    Ok(PreparedWorkstreamRun {
        workstream_id,
        workstream_name,
        run_id,
        agent,
        native_session_id,
        source_cursor,
        sync_after,
        sync_through: latest_sequence,
        may_adopt_existing_session: established == 0,
    })
}

fn newest_available_agent(
    tx: &Transaction<'_>,
    workstream_id: WorkstreamId,
    available: &[AgentKind],
) -> StoreResult<Option<AgentKind>> {
    let mut statement = tx.prepare(
        "SELECT agent_kind FROM workstream_native_sessions \
         WHERE workstream_id = ?1 AND is_current = 1 ORDER BY updated_at DESC",
    )?;
    let rows = statement.query_map(params![workstream_id.as_bytes()], |row| {
        row.get::<_, String>(0)
    })?;
    for row in rows {
        let agent = AgentKind::from_wire(&row?);
        if available.contains(&agent) {
            return Ok(Some(agent));
        }
    }
    Ok(None)
}

fn select_workstream(
    tx: &Transaction<'_>,
    input: &PrepareWorkstreamRun,
    now: i64,
) -> StoreResult<(WorkstreamId, String)> {
    match &input.selection {
        WorkstreamSelection::New(name) => {
            validate_workstream_name(name)?;
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM workstreams \
                 WHERE workspace_id = ?1 AND project_id = ?2 \
                   AND repo_fingerprint = ?3 AND worktree_fingerprint = ?4 AND name = ?5)",
                params![
                    input.workspace_id.as_bytes(),
                    input.project_id.as_bytes(),
                    input.repo_fingerprint,
                    input.worktree_fingerprint,
                    name,
                ],
                |row| row.get(0),
            )?;
            if exists {
                return Err(StoreError::Duplicate(format!(
                    "workstream '{name}' already exists; select it with --workstream"
                )));
            }
            insert_workstream(tx, input, name, now)
        }
        WorkstreamSelection::Named(name) => {
            validate_workstream_name(name)?;
            find_named_workstream(tx, input, name)?
                .ok_or_else(|| StoreError::NotFound(format!("managed workstream '{name}'")))
        }
        WorkstreamSelection::Current => {
            let current = tx
                .query_row(
                    "SELECT id, name FROM workstreams \
                     WHERE workspace_id = ?1 AND project_id = ?2 \
                       AND repo_fingerprint = ?3 AND worktree_fingerprint = ?4 \
                     ORDER BY selected_at DESC, id DESC LIMIT 1",
                    params![
                        input.workspace_id.as_bytes(),
                        input.project_id.as_bytes(),
                        input.repo_fingerprint,
                        input.worktree_fingerprint,
                    ],
                    |row| {
                        let id: Vec<u8> = row.get(0)?;
                        let name: String = row.get(1)?;
                        Ok((id, name))
                    },
                )
                .optional()?;
            match current {
                Some((id, name)) => Ok((WorkstreamId::from_slice(&id)?, name)),
                None => insert_workstream(tx, input, "default", now),
            }
        }
    }
}

fn find_named_workstream(
    tx: &Transaction<'_>,
    input: &PrepareWorkstreamRun,
    name: &str,
) -> StoreResult<Option<(WorkstreamId, String)>> {
    let row = tx
        .query_row(
            "SELECT id, name FROM workstreams \
             WHERE workspace_id = ?1 AND project_id = ?2 \
               AND repo_fingerprint = ?3 AND worktree_fingerprint = ?4 AND name = ?5",
            params![
                input.workspace_id.as_bytes(),
                input.project_id.as_bytes(),
                input.repo_fingerprint,
                input.worktree_fingerprint,
                name,
            ],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    row.map(|(id, name)| Ok((WorkstreamId::from_slice(&id)?, name)))
        .transpose()
}

fn insert_workstream(
    tx: &Transaction<'_>,
    input: &PrepareWorkstreamRun,
    name: &str,
    now: i64,
) -> StoreResult<(WorkstreamId, String)> {
    let id = WorkstreamId::new();
    tx.execute(
        "INSERT INTO workstreams( \
             id, workspace_id, project_id, repo_fingerprint, worktree_fingerprint, \
             name, cwd, created_at, selected_at, updated_at \
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?8)",
        params![
            id.as_bytes(),
            input.workspace_id.as_bytes(),
            input.project_id.as_bytes(),
            input.repo_fingerprint,
            input.worktree_fingerprint,
            name,
            input.cwd,
            now,
        ],
    )?;
    Ok((id, name.to_string()))
}

fn validate_workstream_name(name: &str) -> StoreResult<()> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.chars().count() > MAX_WORKSTREAM_NAME_CHARS
        || trimmed.chars().any(char::is_control)
        || trimmed.contains(['/', '\\'])
    {
        return Err(StoreError::InvalidState(format!(
            "invalid workstream name '{name}'"
        )));
    }
    Ok(())
}

/// Extend the lease for an active managed run.
///
/// The timestamp may have lapsed during a server outage. It remains renewable
/// until another prepare transaction atomically expires this row and grants a
/// replacement run. Finished, cancelled, and superseded runs stay terminal.
pub(crate) fn heartbeat(conn: &mut Connection, run_id: ManagedRunId) -> StoreResult<bool> {
    let now = Timestamp::now().as_microsecond();
    let changed = conn.execute(
        "UPDATE managed_runs SET lease_expires_at = ?1 \
         WHERE id = ?2 AND state = 'active'",
        params![now + LEASE_MICROS, run_id.as_bytes()],
    )?;
    Ok(changed > 0)
}

/// Release an active managed-run lease without importing any events.
pub(crate) fn cancel_run(conn: &mut Connection, run_id: ManagedRunId) -> StoreResult<bool> {
    let now = Timestamp::now().as_microsecond();
    let changed = conn.execute(
        "UPDATE managed_runs SET state = 'expired', ended_at = ?1, lease_expires_at = ?1 \
         WHERE id = ?2 AND state = 'active'",
        params![now, run_id.as_bytes()],
    )?;
    Ok(changed > 0)
}

/// Link the native session reported by a managed child hook.
pub(crate) fn link_native_session(
    conn: &mut Connection,
    run_id: ManagedRunId,
    agent: AgentKind,
    native_session_id: &str,
) -> StoreResult<bool> {
    let native_identity =
        match NativeSessionIdentity::parse(native_session_id, &Sanitizer::builtin()) {
            Ok(id) => id,
            Err(_) => return Ok(false),
        };
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let linked = link_native_session_in_transaction(&tx, run_id, agent, &native_identity, now)?;
    tx.commit()?;
    Ok(linked)
}

fn link_native_session_in_transaction(
    tx: &Transaction<'_>,
    run_id: ManagedRunId,
    agent: AgentKind,
    native_session_id: &NativeSessionIdentity,
    now: i64,
) -> StoreResult<bool> {
    let native_session_id = native_session_id.as_str();
    let run: Option<LinkRunRow> = tx
        .query_row(
            "SELECT workstream_id, agent_kind, native_session_id, \
                    native_session_linked_at IS NOT NULL, sync_through, context_delivered \
             FROM managed_runs WHERE id = ?1 AND state = 'active'",
            params![run_id.as_bytes()],
            |row| {
                Ok(LinkRunRow {
                    workstream: row.get(0)?,
                    agent: row.get(1)?,
                    native_session: row.get(2)?,
                    native_session_linked: row.get(3)?,
                    sync_through: row.get(4)?,
                    context_delivered: row.get(5)?,
                })
            },
        )
        .optional()?;
    let Some(run) = run else {
        return Ok(false);
    };
    if run.agent != agent.as_str() {
        return Ok(false);
    }
    if (run.context_delivered || run.native_session_linked)
        && run
            .native_session
            .as_deref()
            .is_some_and(|linked| linked != native_session_id)
    {
        return Ok(false);
    }
    let workstream = run.workstream;
    let sync_through = run.sync_through;
    let delivered = run.context_delivered;
    tx.execute(
        "UPDATE workstream_native_sessions SET is_current = 0, updated_at = ?1 \
         WHERE workstream_id = ?2 AND agent_kind = ?3 AND native_session_id <> ?4",
        params![now, workstream, agent.as_str(), native_session_id],
    )?;
    let prior_delivery = tx
        .query_row(
            "SELECT delivery_cursor FROM workstream_native_sessions \
             WHERE workstream_id = ?1 AND agent_kind = ?2 AND native_session_id = ?3",
            params![workstream, agent.as_str(), native_session_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    let initial_delivery = prior_delivery.unwrap_or(if delivered { sync_through } else { 0 });
    tx.execute(
        "INSERT INTO workstream_native_sessions( \
             workstream_id, agent_kind, native_session_id, is_current, delivery_cursor, \
             created_at, updated_at \
         ) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?5) \
         ON CONFLICT(workstream_id, agent_kind, native_session_id) DO UPDATE SET \
             is_current = 1, \
             delivery_cursor = MAX(workstream_native_sessions.delivery_cursor, excluded.delivery_cursor), \
             updated_at = excluded.updated_at",
        params![
            workstream,
            agent.as_str(),
            native_session_id,
            initial_delivery,
            now,
        ],
    )?;
    tx.execute(
        "UPDATE managed_runs SET native_session_id = ?1, sync_after = ?2, \
             native_session_linked_at = ?3 WHERE id = ?4",
        params![native_session_id, initial_delivery, now, run_id.as_bytes()],
    )?;
    Ok(true)
}

/// Bind a SessionStart to its exact run, or recover one stale Codex daemon id.
///
/// Candidate selection and linking share one writer transaction. Recovery is
/// deliberately narrow: only a missing/terminal Codex id can be replaced, and
/// only by the sole live, undelivered, never-linked run in the same repository
/// and operator bucket.
pub(crate) fn link_or_adopt_native_session(
    conn: &mut Connection,
    input: &LinkOrAdoptManagedRunSession,
) -> StoreResult<ManagedRunSessionLink> {
    if input.native_session_id.trim().is_empty()
        || input.cwd.trim().is_empty()
        || input
            .owner_user
            .as_deref()
            .is_some_and(|owner| ai_memory_core::IdentityKey::from_storage_key(owner).is_none())
    {
        return Ok(ManagedRunSessionLink::Refused);
    }

    let native_identity =
        match NativeSessionIdentity::parse(&input.native_session_id, &Sanitizer::builtin()) {
            Ok(id) => id,
            Err(_) => return Ok(ManagedRunSessionLink::Refused),
        };
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let supplied = ManagedRunBoundaryRow::read(&tx, input.supplied_run_id)?;

    let selected = match supplied {
        Some(run) if run.state == "active" => {
            let boundary_matches = run.admits(
                input.workspace_id,
                input.project_id,
                Some(&input.cwd),
                input.owner_user.as_deref(),
            ) && run.agent == input.agent.as_str();
            if !boundary_matches {
                tx.commit()?;
                return Ok(ManagedRunSessionLink::Refused);
            }
            (input.supplied_run_id, false)
        }
        _ if input.agent == AgentKind::Codex => {
            let mut statement = tx.prepare(
                "SELECT mr.id FROM managed_runs mr \
                 JOIN workstreams w ON w.id = mr.workstream_id \
                 WHERE w.workspace_id = ?1 AND w.project_id = ?2 AND w.cwd = ?3 \
                   AND mr.agent_kind = ?4 AND mr.owner_user IS ?5 \
                   AND mr.state = 'active' AND mr.lease_expires_at > ?6 \
                   AND mr.context_delivered = 0 AND mr.native_session_linked_at IS NULL \
                 ORDER BY mr.started_at DESC, mr.id DESC LIMIT 2",
            )?;
            let rows = statement.query_map(
                params![
                    input.workspace_id.as_bytes(),
                    input.project_id.as_bytes(),
                    input.cwd.as_str(),
                    input.agent.as_str(),
                    input.owner_user.as_deref(),
                    now,
                ],
                |row| row.get::<_, Vec<u8>>(0),
            )?;
            let candidates = rows.collect::<Result<Vec<_>, _>>()?;
            drop(statement);
            match candidates.as_slice() {
                [only] => (ManagedRunId::from_slice(only)?, true),
                [] => {
                    tx.commit()?;
                    return Ok(ManagedRunSessionLink::NoMatch);
                }
                _ => {
                    tx.commit()?;
                    return Ok(ManagedRunSessionLink::Ambiguous);
                }
            }
        }
        _ => {
            tx.commit()?;
            return Ok(ManagedRunSessionLink::NoMatch);
        }
    };

    if !link_native_session_in_transaction(&tx, selected.0, input.agent, &native_identity, now)? {
        tx.commit()?;
        return Ok(ManagedRunSessionLink::Refused);
    }
    tx.commit()?;
    Ok(if selected.1 {
        ManagedRunSessionLink::Adopted(selected.0)
    } else {
        ManagedRunSessionLink::Exact(selected.0)
    })
}

/// Bind the native session a later hook event of a managed run reported.
///
/// The event's admitted scope and operator must match the run's boundary, as
/// at SessionStart: a run id alone, obtained out of band, must not let another
/// operator or another project repoint the run's current session. A boundary
/// mismatch that would change nothing (the run already holds this session) is
/// a quiet no-op rather than a refusal.
pub(crate) fn link_native_session_in_scope(
    conn: &mut Connection,
    input: &LinkManagedRunSessionInScope,
) -> StoreResult<ManagedRunSessionLink> {
    if input
        .owner_user
        .as_deref()
        .is_some_and(|owner| ai_memory_core::IdentityKey::from_storage_key(owner).is_none())
    {
        return Ok(ManagedRunSessionLink::Refused);
    }
    let native_identity =
        match NativeSessionIdentity::parse(&input.native_session_id, &Sanitizer::builtin()) {
            Ok(id) => id,
            Err(_) => return Ok(ManagedRunSessionLink::NoMatch),
        };
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let Some(run) =
        ManagedRunBoundaryRow::read(&tx, input.run_id)?.filter(|run| run.state == "active")
    else {
        tx.commit()?;
        return Ok(ManagedRunSessionLink::NoMatch);
    };
    if !run.admits(
        input.workspace_id,
        input.project_id,
        None,
        input.owner_user.as_deref(),
    ) {
        tx.commit()?;
        return Ok(
            if run.native_session.as_deref() == Some(native_identity.as_str()) {
                ManagedRunSessionLink::NoMatch
            } else {
                ManagedRunSessionLink::Refused
            },
        );
    }
    let linked =
        link_native_session_in_transaction(&tx, input.run_id, input.agent, &native_identity, now)?;
    tx.commit()?;
    Ok(if linked {
        ManagedRunSessionLink::Exact(input.run_id)
    } else {
        ManagedRunSessionLink::NoMatch
    })
}

/// Mark the assigned synchronization packet delivered to SessionStart.
pub(crate) fn accept_context(conn: &mut Connection, run_id: ManagedRunId) -> StoreResult<bool> {
    let tx = conn.transaction()?;
    let accepted = accept_context_in_transaction(&tx, run_id, false)?;
    tx.commit()?;
    Ok(accepted)
}

pub(crate) fn claim_context_in_transaction(
    tx: &Transaction<'_>,
    run_id: ManagedRunId,
) -> StoreResult<bool> {
    accept_context_in_transaction(tx, run_id, true)
}

fn accept_context_in_transaction(
    tx: &Transaction<'_>,
    run_id: ManagedRunId,
    require_undelivered: bool,
) -> StoreResult<bool> {
    let now = Timestamp::now().as_microsecond();
    let run: Option<(Vec<u8>, String, Option<String>, i64)> = tx
        .query_row(
            "SELECT workstream_id, agent_kind, native_session_id, sync_through \
             FROM managed_runs WHERE id = ?1 AND state = 'active' \
               AND (?2 = 0 OR context_delivered = 0)",
            params![run_id.as_bytes(), i64::from(require_undelivered)],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((workstream, agent, native_session, sync_through)) = run else {
        return Ok(false);
    };
    tx.execute(
        "UPDATE managed_runs SET context_delivered = 1, lease_expires_at = ?1 WHERE id = ?2",
        params![now + LEASE_MICROS, run_id.as_bytes()],
    )?;
    if let Some(native_session) = native_session {
        tx.execute(
            "UPDATE workstream_native_sessions \
             SET delivery_cursor = MAX(delivery_cursor, ?1), updated_at = ?2 \
             WHERE workstream_id = ?3 AND agent_kind = ?4 \
               AND native_session_id = ?5",
            params![sync_through, now, workstream, agent, native_session],
        )?;
    }
    Ok(true)
}

/// Index one immutable source segment and close the run atomically.
pub(crate) fn finish_run(
    conn: &mut Connection,
    authority: &ManagedRunAuthority,
    input: &FinishWorkstreamRun,
) -> StoreResult<FinishedWorkstreamRun> {
    finish_run_inner(conn, authority, input, false)
}

pub(crate) fn recover_run(
    conn: &mut Connection,
    authority: &ManagedRunAuthority,
    input: &FinishWorkstreamRun,
) -> StoreResult<FinishedWorkstreamRun> {
    finish_run_inner(conn, authority, input, true)
}

fn finish_run_inner(
    conn: &mut Connection,
    authority: &ManagedRunAuthority,
    input: &FinishWorkstreamRun,
    recover_expired: bool,
) -> StoreResult<FinishedWorkstreamRun> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    authorize_run_mutation(&tx, input.run_id, authority)?;
    let run: Option<FinishRunRow> = tx
        .query_row(
            "SELECT workstream_id, agent_kind, native_session_id, state, \
                    sync_after, sync_through, context_delivered, exit_code \
             FROM managed_runs WHERE id = ?1",
            params![input.run_id.as_bytes()],
            |row| {
                Ok(FinishRunRow {
                    workstream: row.get(0)?,
                    agent_wire: row.get(1)?,
                    linked_session: row.get(2)?,
                    state: row.get(3)?,
                    sync_after: row.get(4)?,
                    sync_through: row.get(5)?,
                    context_delivered: row.get(6)?,
                    exit_code: row.get(7)?,
                })
            },
        )
        .optional()?;
    let Some(run) = run else {
        return Err(StoreError::NotFound(format!(
            "managed run {}",
            input.run_id
        )));
    };
    let FinishRunRow {
        workstream,
        agent_wire,
        linked_session,
        state,
        sync_after,
        sync_through,
        context_delivered,
        exit_code,
    } = run;
    let latest_before: i64 = tx.query_row(
        "SELECT COALESCE(MAX(sequence), 0) FROM workstream_events WHERE workstream_id = ?1",
        params![workstream],
        |row| row.get(0),
    )?;
    if state == "finished" {
        if recover_expired {
            validate_finished_recovery(
                &tx,
                &workstream,
                &agent_wire,
                linked_session.as_deref(),
                exit_code,
                input,
            )?;
        }
        tx.commit()?;
        return Ok(FinishedWorkstreamRun {
            imported_events: 0,
            latest_sequence: latest_before,
        });
    }
    if state == "expired" && recover_expired {
        let superseded: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM managed_runs newer \
             JOIN managed_runs current ON current.id = ?2 \
             WHERE newer.workstream_id = ?1 AND \
               (newer.started_at > current.started_at OR \
                (newer.started_at = current.started_at AND newer.rowid > current.rowid)))",
            params![workstream, input.run_id.as_bytes()],
            |row| row.get(0),
        )?;
        if superseded {
            return Err(StoreError::InvalidState(format!(
                "managed run {} was superseded and cannot be recovered",
                input.run_id
            )));
        }
        tx.execute(
            "UPDATE managed_runs SET state = 'active', ended_at = NULL, lease_expires_at = ?1 \
             WHERE id = ?2 AND state = 'expired'",
            params![now + LEASE_MICROS, input.run_id.as_bytes()],
        )?;
    } else if state != "active" {
        return Err(StoreError::InvalidState(format!(
            "managed run {} is {state}",
            input.run_id
        )));
    }
    let agent = AgentKind::from_wire(&agent_wire);
    let native_session = input
        .native_session_id
        .as_deref()
        .or(linked_session.as_deref());

    let sanitizer = Sanitizer::builtin();
    let validate = |id: &str| {
        NativeSessionIdentity::parse(id, &sanitizer).map_err(|_| {
            StoreError::InvalidState(
                "native session identity is UNKNOWN; refusing managed binding".into(),
            )
        })
    };
    let native_identity = native_session.map(validate).transpose()?;
    let native_session = native_identity.as_ref().map(NativeSessionIdentity::as_str);
    let event_identities = input
        .events
        .iter()
        .map(|event| validate(&event.native_session_id))
        .collect::<StoreResult<Vec<_>>>()?;
    let mut latest = latest_before;
    let mut imported = 0_usize;
    for (event, event_identity) in input.events.iter().zip(event_identities) {
        if event.agent != agent {
            return Err(StoreError::InvalidState(format!(
                "event {} belongs to {}, managed run expects {}",
                event.event_id,
                event.agent.as_str(),
                agent.as_str()
            )));
        }
        if let Some(native_session) = native_session
            && event.native_session_id != native_session
        {
            return Err(StoreError::InvalidState(format!(
                "event {} belongs to native session {}, managed run expects {native_session}",
                event.event_id, event.native_session_id
            )));
        }
        let existing: Option<(String, String, String)> = tx
            .query_row(
                "SELECT agent_kind, native_session_id, kind FROM workstream_events \
             WHERE workstream_id = ?1 AND event_id = ?2",
                params![workstream, event.event_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some(existing) = existing {
            // Privacy patterns and bounds can change between uploads. Replay
            // keeps the first indexed data and compares only stable identity.
            if existing.0 != event.agent.as_str()
                || existing.1 != event.native_session_id
                || existing.2 != event.kind.as_str()
            {
                return Err(StoreError::InvalidState(
                    "workstream event id reused with different identity".into(),
                ));
            }
            continue;
        }
        let (source_record_id, metadata) = scrub_workstream_provenance(
            &input.sanitizer,
            event.source_record_id.as_deref(),
            &event.metadata,
        );
        let metadata_json = serde_json::to_string(&metadata)?;
        // Store-boundary bound (defense in depth): the hook layer already
        // scrubs and caps event content, but the store is the last gate
        // before durable persistence — scrub with the caller's sanitizer and
        // then bound the free-text `content`, so a caller that ever forgets
        // cannot write unbounded prose (or a secret straddling the cap, cut
        // into an unmatched prefix, #1113) to the DB.
        let content = ai_memory_core::truncate_utf8_bytes(
            &input.sanitizer.scrub(&event.content),
            WORKSTREAM_CONTENT_MAX_BYTES,
        );
        latest += 1;
        tx.execute(
            "INSERT INTO workstream_events( \
                 workstream_id, sequence, event_id, agent_kind, native_session_id, \
                 source_record_id, kind, role, content, occurred_at, metadata_json, \
                 segment_path, created_at \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                workstream,
                latest,
                event.event_id,
                event.agent.as_str(),
                event_identity.as_str(),
                source_record_id,
                event.kind.as_str(),
                event.role,
                content,
                event.occurred_at,
                metadata_json,
                input.segment_path,
                now,
            ],
        )?;
        imported += 1;
    }

    let known_through = if context_delivered || sync_after >= sync_through {
        latest
    } else {
        0
    };
    if input.complete
        && let Some(native_session) = native_session
    {
        tx.execute(
            "UPDATE workstream_native_sessions SET is_current = 0, updated_at = ?1 \
             WHERE workstream_id = ?2 AND agent_kind = ?3 AND native_session_id <> ?4",
            params![now, workstream, agent.as_str(), native_session],
        )?;
        tx.execute(
            "INSERT INTO workstream_native_sessions( \
                 workstream_id, agent_kind, native_session_id, is_current, source_cursor, \
                 delivery_cursor, created_at, updated_at \
             ) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?6) \
             ON CONFLICT(workstream_id, agent_kind, native_session_id) DO UPDATE SET \
                 is_current = 1, source_cursor = excluded.source_cursor, \
                 delivery_cursor = MAX(workstream_native_sessions.delivery_cursor, excluded.delivery_cursor), \
                 updated_at = excluded.updated_at",
            params![
                workstream,
                agent.as_str(),
                native_session,
                input.source_cursor,
                known_through,
                now,
            ],
        )?;
    }
    if input.complete {
        tx.execute(
            "UPDATE managed_runs SET state = 'finished', native_session_id = COALESCE(?1, native_session_id), \
                 native_session_linked_at = CASE WHEN ?1 IS NULL OR ?1 = native_session_id \
                     THEN native_session_linked_at END, \
                 ended_at = ?2, lease_expires_at = ?2, exit_code = ?3 WHERE id = ?4",
            params![
                native_session,
                now,
                input.exit_code,
                input.run_id.as_bytes()
            ],
        )?;
    } else {
        tx.execute(
            "UPDATE managed_runs SET native_session_id = COALESCE(?1, native_session_id), \
                 native_session_linked_at = CASE WHEN ?1 IS NULL OR ?1 = native_session_id \
                     THEN native_session_linked_at END, \
                 lease_expires_at = ?2 WHERE id = ?3",
            params![native_session, now + LEASE_MICROS, input.run_id.as_bytes()],
        )?;
    }
    tx.execute(
        "UPDATE workstreams SET updated_at = ?1 WHERE id = ?2",
        params![now, workstream],
    )?;
    tx.commit()?;
    Ok(FinishedWorkstreamRun {
        imported_events: imported,
        latest_sequence: latest,
    })
}

fn validate_finished_recovery(
    tx: &Transaction<'_>,
    workstream: &[u8],
    agent_wire: &str,
    linked_session: Option<&str>,
    exit_code: Option<i32>,
    input: &FinishWorkstreamRun,
) -> StoreResult<()> {
    if input.native_session_id.as_deref() != linked_session
        || (input.complete && input.exit_code != exit_code)
        || (!input.complete && (input.source_cursor.is_some() || input.exit_code.is_some()))
    {
        return Err(StoreError::InvalidState(
            "finished managed run recovery is not an exact replay".into(),
        ));
    }
    for event in &input.events {
        type StoredEvent = (
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            String,
            Option<String>,
            String,
        );
        let stored: Option<StoredEvent> = tx
            .query_row(
                "SELECT agent_kind, native_session_id, kind, source_record_id, role, content, \
                        occurred_at, metadata_json FROM workstream_events \
                 WHERE workstream_id = ?1 AND event_id = ?2",
                params![workstream, event.event_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .optional()?;
        let (source_record_id, metadata) = scrub_workstream_provenance(
            &input.sanitizer,
            event.source_record_id.as_deref(),
            &event.metadata,
        );
        let expected = (
            event.agent.as_str(),
            event.native_session_id.as_str(),
            event.kind.as_str(),
            source_record_id.as_deref(),
            event.role.as_deref(),
            ai_memory_core::truncate_utf8_bytes(&event.content, WORKSTREAM_CONTENT_MAX_BYTES),
            event.occurred_at.as_deref(),
            serde_json::to_string(&metadata)?,
        );
        let exact = stored.as_ref().is_some_and(|stored| {
            stored.0 == expected.0
                && stored.1 == expected.1
                && stored.2 == expected.2
                && stored.3.as_deref() == expected.3
                && stored.4.as_deref() == expected.4
                && stored.5 == expected.5
                && stored.6.as_deref() == expected.6
                && stored.7 == expected.7
        });
        if !exact || event.agent.as_str() != agent_wire {
            return Err(StoreError::InvalidState(
                "finished managed run recovery contains uncommitted or changed events".into(),
            ));
        }
    }
    let stored_cursor = if let Some(native_session) = linked_session {
        tx.query_row(
            "SELECT source_cursor FROM workstream_native_sessions \
             WHERE workstream_id = ?1 AND agent_kind = ?2 AND native_session_id = ?3",
            params![workstream, agent_wire, native_session],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::InvalidState(
                "finished managed run recovery has no durable native session".into(),
            )
        })?
    } else {
        None
    };
    if input.complete && stored_cursor != input.source_cursor {
        return Err(StoreError::InvalidState(
            "finished managed run recovery cannot advance its source cursor".into(),
        ));
    }
    Ok(())
}

/// Reader-side managed-run status.
pub(crate) fn run_status(
    conn: &Connection,
    run_id: ManagedRunId,
) -> StoreResult<Option<StoredManagedRunStatus>> {
    let row = conn
        .query_row(
            "SELECT workstream_id, agent_kind, native_session_id, \
             native_session_linked_at IS NOT NULL, context_delivered, state \
             FROM managed_runs WHERE id = ?1",
            params![run_id.as_bytes()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, bool>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .optional()?;
    row.map(
        |(
            workstream,
            agent,
            native_session_id,
            native_session_linked,
            context_delivered,
            state,
        )| {
            Ok(StoredManagedRunStatus {
                run_id,
                workstream_id: WorkstreamId::from_slice(&workstream)?,
                agent: AgentKind::from_wire(&agent),
                native_session_id,
                native_session_linked,
                context_delivered,
                state,
            })
        },
    )
    .transpose()
}

/// List recent workstreams selectable from one exact repository/worktree.
pub(crate) fn list_recent(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    repo_fingerprint: &str,
    worktree_fingerprint: &str,
    limit: usize,
    offset: usize,
) -> StoreResult<Vec<StoredWorkstreamSummary>> {
    let limit = i64::try_from(limit.clamp(1, 100)).unwrap_or(100);
    let offset = i64::try_from(offset).unwrap_or(i64::MAX);
    let mut statement = conn.prepare(
        "WITH scoped AS ( \
             SELECT id, name, created_at, selected_at, updated_at \
             FROM workstreams \
             WHERE workspace_id = ?1 AND project_id = ?2 \
               AND repo_fingerprint = ?3 AND worktree_fingerprint = ?4 \
         ), current_workstream AS ( \
             SELECT id FROM scoped ORDER BY selected_at DESC, id DESC LIMIT 1 \
         ), recent AS ( \
             SELECT scoped.id, scoped.name, scoped.created_at, scoped.updated_at, \
                    EXISTS(SELECT 1 FROM current_workstream \
                           WHERE current_workstream.id = scoped.id) AS is_current \
             FROM scoped \
              ORDER BY is_current DESC, scoped.updated_at DESC, scoped.id DESC LIMIT ?5 OFFSET ?6 \
         ) \
         SELECT recent.id, recent.name, recent.created_at, recent.updated_at, \
                recent.is_current, native.agent_kind \
         FROM recent \
         LEFT JOIN workstream_native_sessions native \
           ON native.workstream_id = recent.id AND native.is_current = 1 \
          ORDER BY recent.is_current DESC, recent.updated_at DESC, recent.id DESC, \
                  native.updated_at DESC, native.agent_kind ASC",
    )?;
    let rows = statement.query_map(
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            repo_fingerprint,
            worktree_fingerprint,
            limit,
            offset,
        ],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, bool>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        },
    )?;

    let mut summaries: Vec<StoredWorkstreamSummary> = Vec::new();
    for row in rows {
        let (raw_id, name, created_at, last_active_at, current, agent) = row?;
        let workstream_id = WorkstreamId::from_slice(&raw_id)?;
        let is_new = summaries
            .last()
            .is_none_or(|summary| summary.workstream_id != workstream_id);
        if is_new {
            summaries.push(StoredWorkstreamSummary {
                workstream_id,
                name,
                created_at,
                last_active_at,
                current,
                linked_harnesses: Vec::new(),
            });
        }
        if let Some(agent) = agent
            && let Some(summary) = summaries.last_mut()
        {
            summary.linked_harnesses.push(AgentKind::from_wire(&agent));
        }
    }
    Ok(summaries)
}

/// Retitle one checkout-local workstream.
///
/// Names are metadata: `workstream_events`, `managed_runs`, and
/// `workstream_native_sessions` all key on `workstreams.id`, so a rename is a
/// single-row update with nothing to cascade. `selected_at` and `updated_at`
/// are deliberately left alone — relabelling is not activity, and bumping
/// either would reorder the discovery listing (and, for `selected_at`, change
/// which workstream a bare `ai-memory run` resumes) as a side effect of
/// fixing a typo.
///
/// A rename onto the workstream's own current name succeeds without writing,
/// so a repeated command is not an error. A rename onto a name another
/// workstream in the same checkout already holds is refused before the
/// update, turning the `UNIQUE` constraint into a named error rather than a
/// bare SQLite failure.
pub(crate) fn rename(
    conn: &mut Connection,
    input: &RenameWorkstream,
) -> StoreResult<RenamedWorkstream> {
    validate_workstream_name(&input.new_name)?;
    let new_name = input.new_name.trim();
    let tx = conn.transaction()?;
    let found = match &input.selector {
        WorkstreamSelector::Name(name) => tx
            .query_row(
                "SELECT id, name FROM workstreams \
                 WHERE workspace_id = ?1 AND project_id = ?2 \
                   AND repo_fingerprint = ?3 AND worktree_fingerprint = ?4 AND name = ?5",
                params![
                    input.workspace_id.as_bytes(),
                    input.project_id.as_bytes(),
                    input.repo_fingerprint,
                    input.worktree_fingerprint,
                    name,
                ],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?,
        // The scope predicate stays on the id lookup too: `workstreams.id` is
        // globally unique, so without it a caller holding an id from another
        // checkout could retitle a workstream their request never named.
        WorkstreamSelector::Id(id) => tx
            .query_row(
                "SELECT id, name FROM workstreams \
                 WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3 \
                   AND repo_fingerprint = ?4 AND worktree_fingerprint = ?5",
                params![
                    id.as_bytes(),
                    input.workspace_id.as_bytes(),
                    input.project_id.as_bytes(),
                    input.repo_fingerprint,
                    input.worktree_fingerprint,
                ],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?,
    };
    let (id_bytes, current_name) = found.ok_or_else(|| {
        StoreError::NotFound(match &input.selector {
            WorkstreamSelector::Name(name) => format!("managed workstream '{name}'"),
            WorkstreamSelector::Id(id) => format!("managed workstream {id}"),
        })
    })?;
    let workstream_id = WorkstreamId::from_slice(&id_bytes)?;
    if current_name == new_name {
        return Ok(RenamedWorkstream {
            workstream_id,
            from: current_name.clone(),
            to: current_name,
        });
    }
    let taken: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM workstreams \
         WHERE workspace_id = ?1 AND project_id = ?2 \
           AND repo_fingerprint = ?3 AND worktree_fingerprint = ?4 \
           AND name = ?5 AND id IS NOT ?6)",
        params![
            input.workspace_id.as_bytes(),
            input.project_id.as_bytes(),
            input.repo_fingerprint,
            input.worktree_fingerprint,
            new_name,
            workstream_id.as_bytes(),
        ],
        |row| row.get(0),
    )?;
    if taken {
        return Err(StoreError::WorkstreamNameTaken(new_name.to_string()));
    }
    tx.execute(
        "UPDATE workstreams SET name = ?1 WHERE id = ?2",
        params![new_name, workstream_id.as_bytes()],
    )?;
    tx.commit()?;
    Ok(RenamedWorkstream {
        workstream_id,
        from: current_name,
        to: new_name.to_string(),
    })
}

/// Reader-side context range assigned to one managed run.
pub(crate) fn run_context(
    conn: &Connection,
    run_id: ManagedRunId,
    max_events: usize,
) -> StoreResult<Option<ManagedRunContext>> {
    let row = conn
        .query_row(
            "SELECT mr.workstream_id, w.name, mr.agent_kind, mr.native_session_id, \
                    mr.sync_after, mr.sync_through, mr.context_delivered \
             FROM managed_runs mr JOIN workstreams w ON w.id = mr.workstream_id \
             WHERE mr.id = ?1 AND mr.state = 'active'",
            params![run_id.as_bytes()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, bool>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((workstream, name, agent, native, after, through, delivered)) = row else {
        return Ok(None);
    };
    let workstream_id = WorkstreamId::from_slice(&workstream)?;
    let mut stmt = conn.prepare(
        "SELECT sequence, event_id, agent_kind, native_session_id, kind, role, content, occurred_at \
         FROM workstream_events \
         WHERE workstream_id = ?1 AND sequence > ?2 AND sequence <= ?3 \
         ORDER BY sequence DESC LIMIT ?4",
    )?;
    let rows = stmt.query_map(
        params![
            workstream,
            after,
            through,
            i64::try_from(max_events).unwrap_or(i64::MAX)
        ],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        },
    )?;
    let mut events = Vec::new();
    for row in rows {
        let (sequence, event_id, agent, native_session_id, kind, role, content, occurred_at) = row?;
        events.push(WorkstreamEvent {
            sequence,
            event_id,
            agent: AgentKind::from_wire(&agent),
            native_session_id,
            source_record_id: None,
            metadata: serde_json::Value::Null,
            kind: WorkstreamEventKind::from_str(&kind)?,
            role,
            content,
            occurred_at,
        });
    }
    events.reverse();
    Ok(Some(ManagedRunContext {
        run_id,
        workstream_id,
        workstream_name: name,
        agent: AgentKind::from_wire(&agent),
        native_session_id: native,
        sync_after: after,
        sync_through: through,
        context_delivered: delivered,
        events,
    }))
}

/// Search or tail the portable ledger for explicit managed-session recovery.
pub(crate) fn search_events(
    conn: &Connection,
    workstream_id: WorkstreamId,
    query: &str,
    limit: usize,
    stopwords: &crate::fts_query::FtsStopwords,
    sanitizer: &Sanitizer,
) -> StoreResult<Vec<WorkstreamEvent>> {
    let limit = i64::try_from(limit.clamp(1, 100)).unwrap_or(100);
    let free_text = query
        .replace("title:", "")
        .replace("body:", "")
        .replace("content:", "");
    let fts_query = crate::prepare_fts5_query(&free_text, stopwords);
    let sql = if fts_query.is_empty() {
        "SELECT sequence, event_id, agent_kind, native_session_id, kind, role, content, occurred_at, \
                source_record_id, metadata_json \
         FROM workstream_events WHERE workstream_id = ?1 ORDER BY sequence DESC LIMIT ?2"
    } else {
        "SELECT e.sequence, e.event_id, e.agent_kind, e.native_session_id, e.kind, \
                e.role, e.content, e.occurred_at, e.source_record_id, e.metadata_json \
         FROM workstream_events_fts f \
         JOIN workstream_events e ON e.rowid = f.rowid \
         WHERE workstream_events_fts MATCH ?2 AND e.workstream_id = ?1 \
         ORDER BY f.rank LIMIT ?3"
    };
    let mut statement = conn.prepare(sql)?;
    let mut events = Vec::new();
    if fts_query.is_empty() {
        let rows = statement.query_map(params![workstream_id.as_bytes(), limit], read_event_row)?;
        for row in rows {
            events.push(stored_event(row?, sanitizer)?);
        }
    } else {
        let rows = statement.query_map(
            params![workstream_id.as_bytes(), fts_query, limit],
            read_event_row,
        )?;
        for row in rows {
            events.push(stored_event(row?, sanitizer)?);
        }
    }
    Ok(events)
}

type EventRow = (
    i64,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    String,
);

fn read_event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
    ))
}

fn stored_event(row: EventRow, sanitizer: &Sanitizer) -> StoreResult<WorkstreamEvent> {
    let (
        sequence,
        event_id,
        agent,
        native_session_id,
        kind,
        role,
        content,
        occurred_at,
        source,
        metadata,
    ) = row;
    // Oversized or malformed legacy dumps carry no trusted correlation data.
    let metadata = if metadata.len() <= 16 * 1024 {
        serde_json::from_str(&metadata).unwrap_or(serde_json::Value::Null)
    } else {
        serde_json::Value::Null
    };
    let (source_record_id, metadata) =
        scrub_workstream_provenance(sanitizer, source.as_deref(), &metadata);
    Ok(WorkstreamEvent {
        sequence,
        event_id,
        agent: AgentKind::from_wire(&agent),
        native_session_id: NativeSessionIdentity::project(&native_session_id, sanitizer),
        source_record_id,
        metadata,
        kind: WorkstreamEventKind::from_str(&kind)?,
        role,
        content,
        occurred_at,
    })
}
