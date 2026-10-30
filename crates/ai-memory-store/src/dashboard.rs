//! Scoped metadata for the human operations dashboard.

use ai_memory_core::{
    AgentKind, AgentMessage, MessageBox, MessageState, OwnerFilter, ProjectId, SessionId, UserId,
    WorkspaceId,
};
use rusqlite::params;

use crate::{ReaderPool, SessionSummary, StoreResult, TokenUsageReport};

/// One registered harness in a visible project.
#[derive(Debug, Clone)]
pub struct RegisteredAgent {
    /// Existing workspace name.
    pub workspace: String,
    /// Existing project name.
    pub project: String,
    /// Harness recorded by at least one session.
    pub agent: AgentKind,
}

/// Session and token aggregates for one harness, with ownership applied first.
#[derive(Debug, Clone, Default)]
pub struct AgentUsage {
    /// Stored harness discriminator.
    pub agent: String,
    /// Sessions recorded in this scope.
    pub sessions: u64,
    /// Sessions without an end event; not a process-liveness assertion.
    pub open: u64,
    /// Sessions whose usage was reported.
    pub measured_sessions: u64,
    /// Fresh input tokens reported across sessions.
    pub input: u64,
    /// Reported output tokens.
    pub output: u64,
    /// Reported cache reads.
    pub cache_read: u64,
    /// Reported cache writes.
    pub cache_write: u64,
    /// Captured response count, separate from legacy session totals.
    pub calls: u64,
}

impl ReaderPool {
    /// List registered project/harness recipients in one access-filtered query.
    pub async fn registered_agents(
        &self,
        viewer: Option<UserId>,
    ) -> StoreResult<Vec<RegisteredAgent>> {
        self.with_conn(move |conn| {
            let visible = crate::reader::readable_repository_predicate("p.id", viewer);
            let mut stmt = conn.prepare(&format!(
                "SELECT w.name,p.name,s.agent_kind FROM sessions s \
                 JOIN projects p ON p.id=s.project_id AND p.workspace_id=s.workspace_id \
                 JOIN workspaces w ON w.id=p.workspace_id WHERE 1=1{visible} \
                 GROUP BY p.id,s.agent_kind ORDER BY w.name,p.name,s.agent_kind"
            ))?;
            let rows = stmt.query_map([], |r| {
                Ok(RegisteredAgent {
                    workspace: r.get(0)?,
                    project: r.get(1)?,
                    agent: AgentKind::from_wire(&r.get::<_, String>(2)?),
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// Aggregate sessions without multiplying their usage by response count.
    pub async fn agent_usage(
        &self,
        ws: WorkspaceId,
        project: ProjectId,
        owner: OwnerFilter,
    ) -> StoreResult<Vec<AgentUsage>> {
        self.with_conn(move |conn| {
            let (mode, actor) = match owner {
                OwnerFilter::Any => (0,String::new()),
                OwnerFilter::User(a) => (1,a),
                OwnerFilter::Unattributed => (2,String::new()),
            };
            let mut stmt=conn.prepare(
                "WITH scoped AS (SELECT * FROM sessions WHERE workspace_id=?1 AND project_id=?2 \
                   AND (?3=0 OR actor_user IS NULL OR (?3=1 AND actor_user=?4))), \
                 calls AS (SELECT c.session_id,COUNT(*) n FROM token_calls c \
                   JOIN scoped s ON s.id=c.session_id GROUP BY c.session_id) \
                 SELECT s.agent_kind,COUNT(*),SUM(s.ended_at IS NULL),COUNT(u.session_id), \
                 COALESCE(SUM(u.input_tokens),0),COALESCE(SUM(u.output_tokens),0), \
                 COALESCE(SUM(u.cache_read_tokens),0),COALESCE(SUM(u.cache_write_tokens),0), \
                 COALESCE(SUM(c.n),0) FROM scoped s \
                 LEFT JOIN session_usage u ON u.session_id=s.id LEFT JOIN calls c ON c.session_id=s.id \
                 GROUP BY s.agent_kind ORDER BY COUNT(*) DESC,s.agent_kind"
            )?;
            let rows=stmt.query_map(params![ws.as_bytes(),project.as_bytes(),mode,actor],|r| Ok(AgentUsage {
                agent:r.get(0)?,sessions:r.get(1)?,open:r.get(2)?,measured_sessions:r.get(3)?,
                input:r.get(4)?,output:r.get(5)?,cache_read:r.get(6)?,cache_write:r.get(7)?,calls:r.get(8)?,
            }))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        }).await
    }

    /// Page through one harness's sessions using the standard scope and owner guards.
    pub async fn agent_sessions(
        &self,
        ws: WorkspaceId,
        project: ProjectId,
        agent: AgentKind,
        owner: OwnerFilter,
        offset: usize,
    ) -> StoreResult<Vec<SessionSummary>> {
        self.sessions_for_scope_filtered(
            ws,
            project,
            owner,
            true,
            51,
            offset.min(i64::MAX as usize),
            None,
            Some(agent.as_str().to_owned()),
            false,
            false,
        )
        .await
    }

    /// Pending shared mail and mail addressed to this harness, without claiming.
    pub async fn pending_messages_for_agent(
        &self,
        ws: WorkspaceId,
        project: ProjectId,
        agent: AgentKind,
    ) -> StoreResult<u64> {
        self.with_conn(move |conn| Ok(conn.query_row(
            "SELECT COUNT(*) FROM agent_messages WHERE to_workspace_id=?1 AND to_project_id=?2 AND state='pending' AND (to_agent IS NULL OR to_agent=?3)",
            params![ws.as_bytes(),project.as_bytes(),agent.as_str()],|r|r.get(0)
        )?)).await
    }

    /// Read queued and historical mail without claiming it. The opposite
    /// project's name is deliberately absent; callers resolve visible names.
    pub async fn message_history(
        &self,
        ws: WorkspaceId,
        project: ProjectId,
        mailbox: MessageBox,
        limit: usize,
        offset: usize,
    ) -> StoreResult<Vec<AgentMessage>> {
        self.message_history_filtered(ws, project, mailbox, None, limit, offset)
            .await
    }

    /// Read mailbox history with its state predicate applied before pagination.
    pub async fn message_history_filtered(
        &self,
        ws: WorkspaceId,
        project: ProjectId,
        mailbox: MessageBox,
        state: Option<MessageState>,
        limit: usize,
        offset: usize,
    ) -> StoreResult<Vec<AgentMessage>> {
        self.with_conn(move |conn| {
            let scope = match mailbox {
                MessageBox::Inbox => "to_workspace_id",
                MessageBox::Outbox => "from_workspace_id",
            };
            let proj = match mailbox {
                MessageBox::Inbox => "to_project_id",
                MessageBox::Outbox => "from_project_id",
            };
            let mut stmt = conn.prepare(&format!(
                "SELECT {} FROM agent_messages WHERE {scope}=?1 AND {proj}=?2 \
                 AND (?5 IS NULL OR state=?5) \
                 ORDER BY created_at DESC,id DESC LIMIT ?3 OFFSET ?4",
                crate::ops::MESSAGE_COLUMNS
            ))?;
            let rows = stmt.query_map(
                params![
                    ws.as_bytes(),
                    project.as_bytes(),
                    limit.clamp(1, 100) as i64,
                    offset.min(i64::MAX as usize) as i64,
                    state.map(|s| s.as_str()),
                ],
                crate::ops::row_to_agent_message,
            )?;
            rows.map(|r| r?).collect()
        })
        .await
    }

    /// Read one message addressed to this recipient project, in any state.
    /// The recipient coordinate is part of the predicate, so an id belonging to
    /// another project reads as absent.
    pub async fn message_in_mailbox(
        &self,
        ws: WorkspaceId,
        project: ProjectId,
        id: ai_memory_core::MessageId,
    ) -> StoreResult<Option<AgentMessage>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {} FROM agent_messages \
                 WHERE id=?1 AND to_workspace_id=?2 AND to_project_id=?3",
                crate::ops::MESSAGE_COLUMNS
            ))?;
            let mut rows = stmt.query_map(
                params![id.as_bytes(), ws.as_bytes(), project.as_bytes()],
                crate::ops::row_to_agent_message,
            )?;
            rows.next().map(|r| r?).transpose()
        })
        .await
    }

    /// Read this session's response ledger after applying scope and ownership.
    pub async fn session_token_usage(
        &self,
        ws: WorkspaceId,
        project: ProjectId,
        sid: SessionId,
        owner: OwnerFilter,
    ) -> StoreResult<TokenUsageReport> {
        self.token_usage_report_filtered(ws, project, owner, Some(sid))
            .await
    }
}
