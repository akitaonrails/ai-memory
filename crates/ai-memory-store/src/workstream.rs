//! Store operations for optional `ai-memory run` workstreams.

use std::str::FromStr as _;

use ai_memory_core::{
    AgentKind, ManagedRunId, NewWorkstreamEvent, ProjectId, WorkspaceId, WorkstreamEvent,
    WorkstreamEventKind, WorkstreamId,
};
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};

use crate::{StoreError, StoreResult};

const LEASE_MICROS: i64 = 90 * 1_000_000;
const MAX_WORKSTREAM_NAME_CHARS: usize = 128;

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

/// Store-level finish input after the raw segment has been made durable.
#[derive(Debug, Clone)]
pub struct FinishWorkstreamRun {
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
    /// SessionStart delivery acknowledgement.
    pub context_delivered: bool,
    /// State string.
    pub state: String,
}

struct FinishRunRow {
    workstream: Vec<u8>,
    agent_wire: String,
    linked_session: Option<String>,
    state: String,
    sync_after: i64,
    sync_through: i64,
    context_delivered: bool,
}

struct LinkRunRow {
    workstream: Vec<u8>,
    agent: String,
    native_session: Option<String>,
    sync_through: i64,
    context_delivered: bool,
}

/// Atomically select a workstream, expire stale leases, and open one run.
pub(crate) fn prepare_run(
    conn: &mut Connection,
    input: &PrepareWorkstreamRun,
) -> StoreResult<PreparedWorkstreamRun> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE managed_runs SET state = 'expired', ended_at = ?1 \
         WHERE state = 'active' AND lease_expires_at <= ?1",
        params![now],
    )?;

    let (workstream_id, workstream_name) = select_workstream(&tx, input, now)?;
    let busy: Option<(String, i64)> = tx
        .query_row(
            "SELECT lease_owner, lease_expires_at FROM managed_runs \
             WHERE workstream_id = ?1 AND state = 'active'",
            params![workstream_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((owner, expires)) = busy {
        return Err(StoreError::WorkstreamBusy(format!(
            "owned by {owner} until {}",
            Timestamp::from_microsecond(expires)
                .map(|t| t.to_string())
                .unwrap_or_else(|_| expires.to_string())
        )));
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
             id, workstream_id, agent_kind, lease_owner, native_session_id, state, \
             sync_after, sync_through, context_delivered, lease_expires_at, started_at \
         ) VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6, ?7, 0, ?8, ?9)",
        params![
            run_id.as_bytes(),
            workstream_id.as_bytes(),
            agent.as_str(),
            input.lease_owner,
            native_session_id,
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
                     ORDER BY selected_at DESC LIMIT 1",
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

/// Extend the lease for a live managed run.
pub(crate) fn heartbeat(conn: &mut Connection, run_id: ManagedRunId) -> StoreResult<bool> {
    let now = Timestamp::now().as_microsecond();
    let changed = conn.execute(
        "UPDATE managed_runs SET lease_expires_at = ?1 \
         WHERE id = ?2 AND state = 'active' AND lease_expires_at > ?3",
        params![now + LEASE_MICROS, run_id.as_bytes(), now],
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
    if native_session_id.trim().is_empty() {
        return Ok(false);
    }
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let run: Option<LinkRunRow> = tx
        .query_row(
            "SELECT workstream_id, agent_kind, native_session_id, sync_through, context_delivered \
             FROM managed_runs WHERE id = ?1 AND state = 'active'",
            params![run_id.as_bytes()],
            |row| {
                Ok(LinkRunRow {
                    workstream: row.get(0)?,
                    agent: row.get(1)?,
                    native_session: row.get(2)?,
                    sync_through: row.get(3)?,
                    context_delivered: row.get(4)?,
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
    if run.context_delivered
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
        "UPDATE managed_runs SET native_session_id = ?1, sync_after = ?2 WHERE id = ?3",
        params![native_session_id, initial_delivery, run_id.as_bytes()],
    )?;
    tx.commit()?;
    Ok(true)
}

/// Mark the assigned synchronization packet delivered to SessionStart.
pub(crate) fn accept_context(conn: &mut Connection, run_id: ManagedRunId) -> StoreResult<bool> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let run: Option<(Vec<u8>, String, Option<String>, i64)> = tx
        .query_row(
            "SELECT workstream_id, agent_kind, native_session_id, sync_through \
             FROM managed_runs WHERE id = ?1 AND state = 'active'",
            params![run_id.as_bytes()],
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
    tx.commit()?;
    Ok(true)
}

/// Index one immutable source segment and close the run atomically.
pub(crate) fn finish_run(
    conn: &mut Connection,
    input: &FinishWorkstreamRun,
) -> StoreResult<FinishedWorkstreamRun> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let run: Option<FinishRunRow> = tx
        .query_row(
            "SELECT workstream_id, agent_kind, native_session_id, state, \
                    sync_after, sync_through, context_delivered \
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
    } = run;
    let latest_before: i64 = tx.query_row(
        "SELECT COALESCE(MAX(sequence), 0) FROM workstream_events WHERE workstream_id = ?1",
        params![workstream],
        |row| row.get(0),
    )?;
    if state == "finished" {
        return Ok(FinishedWorkstreamRun {
            imported_events: 0,
            latest_sequence: latest_before,
        });
    }
    if state != "active" {
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

    let mut latest = latest_before;
    let mut imported = 0_usize;
    for event in &input.events {
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
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM workstream_events \
             WHERE workstream_id = ?1 AND event_id = ?2)",
            params![workstream, event.event_id],
            |row| row.get(0),
        )?;
        if exists {
            continue;
        }
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
                event.native_session_id,
                event.source_record_id,
                event.kind.as_str(),
                event.role,
                event.content,
                event.occurred_at,
                serde_json::to_string(&event.metadata)?,
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

/// Reader-side managed-run status.
pub(crate) fn run_status(
    conn: &Connection,
    run_id: ManagedRunId,
) -> StoreResult<Option<StoredManagedRunStatus>> {
    let row = conn
        .query_row(
            "SELECT workstream_id, agent_kind, native_session_id, context_delivered, state \
             FROM managed_runs WHERE id = ?1",
            params![run_id.as_bytes()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    row.map(
        |(workstream, agent, native_session_id, context_delivered, state)| {
            Ok(StoredManagedRunStatus {
                run_id,
                workstream_id: WorkstreamId::from_slice(&workstream)?,
                agent: AgentKind::from_wire(&agent),
                native_session_id,
                context_delivered,
                state,
            })
        },
    )
    .transpose()
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
) -> StoreResult<Vec<WorkstreamEvent>> {
    let limit = i64::try_from(limit.clamp(1, 100)).unwrap_or(100);
    let free_text = query
        .replace("title:", "")
        .replace("body:", "")
        .replace("content:", "");
    let fts_query = crate::prepare_fts5_query(&free_text);
    let sql = if fts_query.is_empty() {
        "SELECT sequence, event_id, agent_kind, native_session_id, kind, role, content, occurred_at \
         FROM workstream_events WHERE workstream_id = ?1 ORDER BY sequence DESC LIMIT ?2"
    } else {
        "SELECT e.sequence, e.event_id, e.agent_kind, e.native_session_id, e.kind, \
                e.role, e.content, e.occurred_at \
         FROM workstream_events_fts f \
         JOIN workstream_events e ON e.rowid = f.rowid \
         WHERE workstream_events_fts MATCH ?2 AND e.workstream_id = ?1 \
         ORDER BY f.rank LIMIT ?3"
    };
    let mut statement = conn.prepare(sql)?;
    let read_row = |row: &rusqlite::Row<'_>| {
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
    };
    let mut events = Vec::new();
    if fts_query.is_empty() {
        let rows = statement.query_map(params![workstream_id.as_bytes(), limit], read_row)?;
        for row in rows {
            events.push(stored_event(row?)?);
        }
    } else {
        let rows = statement.query_map(
            params![workstream_id.as_bytes(), fts_query, limit],
            read_row,
        )?;
        for row in rows {
            events.push(stored_event(row?)?);
        }
    }
    Ok(events)
}

fn stored_event(
    row: (
        i64,
        String,
        String,
        String,
        String,
        Option<String>,
        String,
        Option<String>,
    ),
) -> StoreResult<WorkstreamEvent> {
    let (sequence, event_id, agent, native_session_id, kind, role, content, occurred_at) = row;
    Ok(WorkstreamEvent {
        sequence,
        event_id,
        agent: AgentKind::from_wire(&agent),
        native_session_id,
        kind: WorkstreamEventKind::from_str(&kind)?,
        role,
        content,
        occurred_at,
    })
}
