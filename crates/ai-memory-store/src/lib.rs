//! SQLite storage layer for ai-memory.
//!
//! The crate owns a single SQLite file under `<data_dir>/db/memory.sqlite`,
//! opens it in WAL mode with foreign keys on, runs all pending migrations
//! at startup, and exposes a [`WriterHandle`] that serialises every mutation
//! through a dedicated OS thread.
//!
//! Reader-side APIs land in milestone M1-B; the writer + migrations are
//! sufficient for M1-A's "drop a page in, see it persisted" demo.

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use rusqlite::Connection;

mod auto_improve;
pub mod decay;
mod error;
mod fts_query;
mod maintenance;
mod migrations;
mod ops;
mod reader;
mod scope;
mod session_consolidation;
pub mod users;
mod workstream;
mod writer;

pub use fts_query::prepare_fts5_query;

pub use auto_improve::{
    ApproveAutoImproveProposal, ApproveAutoImproveProposalResult, AutoImproveProposalDetail,
    AutoImproveProposalEvent, AutoImproveProposalOperation, AutoImproveProposalStatus,
    AutoImproveProposalSummary, AutoImproveRejectionSummary, AutoImproveTelemetryAggregate,
    AutoImproveTelemetryCount, FailAutoImproveProposal, NewAutoImproveProposal,
    OwnedAutoImproveProposalDetail, RejectAutoImproveProposal, SkippedProposal,
    StageAutoImproveRun, StagedAutoImproveRun, StagedAutoImproveRunReport, artifact_path_for,
};
pub use decay::{
    DecayParams, SALIENCE_MAX, SALIENCE_MIN, SALIENCE_STEP, retention_score,
    retention_score_with_breadth, salience_after_feedback,
};
pub use error::{StoreError, StoreResult};
pub use maintenance::MaintenanceJob;
pub use ops::{
    AdmittedSession, CompactSummary, Compaction, DeleteWorkspaceSummary, EmbedOutcome,
    EmbeddingWrite, HookSessionAdmission, IngestObservationOutcome, LifecycleOnlyEndOutcome,
    MoveSessionSummary, MoveSummary, ObservationPruneOutcome, PagesMode, PurgeSessionSummary,
    PurgeSummary, ReorgSummary, purge_session, record_embed_failure,
};
pub use reader::{
    ActivityWindow, AgentSessionCount, AuditEvent, AuditLogFilter, AutoImproveCandidateSession,
    BriefPageBody, BriefingPage, BriefingSnapshot, ClientActivity, ContaminationFinding,
    ContaminationReport, ContaminationSummary, DecayCandidate, DecayTombstone, DerivedIndexStatus,
    EmbeddingTripleCount, FeedbackFinding, GraphVia, HealthDetail, HealthPage, ObservationHit,
    ObservationOrder, ObservationPage, ObservationPageResult, ObservationRecord, OpenSession,
    PageAuthor, PageHit, PageHitWithMeta, PageLinks, PageMeta, PageSummary, ProjectSummary,
    ReaderPool, ReindexTargetStatus, RelatedPage, RrfContributions, ScopeRow, SearchExplain,
    SessionDependentRows, SessionEndDisposition, SessionSummary, StatusCounts, StorageStatus,
    StoredEmbedding, StoredPageBody, WorkspaceScopeRow, WorkspaceSummary, f32_vec_to_bytes,
};
pub use scope::{
    ResolvedScope, ScopeName, ScopeResolutionError, ScopeResolver, WORKSPACE_PROJECT_PAIR_REQUIRED,
    create_explicit_scope, create_global_scope, lookup_existing_scope, lookup_existing_workspace,
    lookup_global_scope, resolve_many_existing_scopes,
};
pub use session_consolidation::{SESSION_CONSOLIDATION_MAX_ATTEMPTS, SessionConsolidationJob};
pub use users::{TOKEN_HASH_LEN, TOKEN_RAW_LEN, TokenPepper, generate_token, hash_token};
pub use workstream::{
    FinishWorkstreamRun, FinishedWorkstreamRun, ManagedRunContext, PrepareWorkstreamRun,
    PreparedWorkstreamRun, RenameWorkstream, RenamedWorkstream, StoredManagedRunStatus,
    StoredWorkstreamSummary, WorkstreamSelection, WorkstreamSelector,
};
pub use writer::{StartupContextAcceptance, WriterHandle};

/// Filename used inside the data dir's `db/` subdirectory.
pub const DB_FILENAME: &str = "memory.sqlite";

/// Maximum Unicode scalar count accepted for a persisted MCP client label.
pub const CLIENT_ACTIVITY_MAX_NAME_CHARS: usize = 64;
/// Maximum distinct named MCP clients persisted for one UTC day. Calls from
/// additional labels are folded into [`CLIENT_ACTIVITY_OVERFLOW_CLIENT`].
pub const CLIENT_ACTIVITY_MAX_CLIENTS_PER_DAY: usize = 128;
/// Stable label used when a UTC day's client-cardinality budget is exhausted.
pub const CLIENT_ACTIVITY_OVERFLOW_CLIENT: &str = "other";

/// Default soft cap for the read-only connection pool.
const READER_POOL_SOFT_CAP: usize = 4;

/// Open (and migrate) a [`Store`] rooted at the given data directory.
pub struct Store {
    /// Cloneable handle to submit mutations.
    pub writer: WriterHandle,
    /// Cloneable handle for read-only queries.
    pub reader: ReaderPool,
    db_path: PathBuf,
}

impl Store {
    /// Open the SQLite file at `<data_dir>/db/memory.sqlite`, applying any
    /// outstanding migrations, then spawn the writer thread and prepare
    /// the read-only connection pool.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the file cannot be opened, migrations
    /// cannot be applied, or the writer thread fails to start.
    pub fn open(data_dir: &Path) -> StoreResult<Self> {
        let db_dir = data_dir.join("db");
        create_private_dir_all(&db_dir)?;
        let db_path = db_dir.join(DB_FILENAME);
        create_private_file_if_missing(&db_path)?;

        let mut conn = Connection::open(&db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5_000)?; // ms

        // SQLite cannot disable FK enforcement inside refinery's per-migration
        // transaction. Keep it off while migrations rebuild tables, then enable
        // it for all runtime reads/writes below.
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        migrations::run(&mut conn)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let writer = WriterHandle::spawn(conn);
        let reader = ReaderPool::new(&db_path, READER_POOL_SOFT_CAP)?;
        Ok(Self {
            writer,
            reader,
            db_path,
        })
    }

    /// Path of the SQLite file on disk.
    #[must_use]
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

/// Create missing database-directory components with owner-only access while
/// leaving an existing installation's permissions untouched.
fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path)
}

/// Reserve a new SQLite file with restrictive permissions before SQLite writes
/// its first page. Existing databases retain their current permissions.
fn create_private_file_if_missing(path: &Path) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    match options.open(path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_core::{
        ActorContext, AgentKind, HandoffAcceptance, HandoffId, HandoffState, LinkTarget,
        ManagedRunId, NewHandoff, NewObservation, NewPage, NewSession, NewWorkstreamEvent,
        ObservationId, ObservationKind, PageId, PagePath, ProjectId, Sanitized, Sanitizer,
        SessionId, Tier, UserId, WorkspaceId, WorkstreamEventKind, WorkstreamId,
    };
    use rusqlite::{Connection, params};
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    #[cfg(unix)]
    #[test]
    fn open_creates_private_database_directory_and_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("data");
        let store = Store::open(&root).unwrap();

        assert_eq!(
            std::fs::metadata(root.join("db"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(store.db_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    fn sample_page(ws: WorkspaceId, proj: ProjectId, path: &str, body: &str) -> NewPage {
        NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path).unwrap(),
            title: "test".into(),
            body: body.into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
        }
    }

    fn proposal(
        path: &str,
        op: AutoImproveProposalOperation,
        body: &str,
    ) -> NewAutoImproveProposal {
        NewAutoImproveProposal {
            operation: op,
            target_path: PagePath::new(path).unwrap(),
            kind: "note".into(),
            title: "Proposed".into(),
            confidence: 0.9,
            rationale: "rationale".into(),
            evidence_json: serde_json::json!([{"source":"test"}]),
            body_markdown: body.into(),
            artifact_sha256: None,
            edit_mode: None,
            patch_json: None,
            expected_base_body_sha256: None,
        }
    }

    fn delete_scheduler_state(store: &Store, ws: WorkspaceId, proj: ProjectId) {
        let conn = Connection::open(store.db_path()).unwrap();
        conn.execute(
            "DELETE FROM auto_improve_scheduler_state WHERE workspace_id = ?1 AND project_id = ?2",
            params![ws.as_bytes(), proj.as_bytes()],
        )
        .unwrap();
    }

    fn stage_input(
        ws: WorkspaceId,
        proj: ProjectId,
        proposals: Vec<NewAutoImproveProposal>,
    ) -> StageAutoImproveRun {
        StageAutoImproveRun {
            workspace_id: ws,
            project_id: proj,
            session_id: None,
            provider: Some("test".into()),
            model: Some("model".into()),
            summary: Some("summary".into()),
            warnings_json: serde_json::json!([]),
            rejected_candidates_json: serde_json::json!([]),
            config_json: serde_json::json!({"mode":"stage"}),
            proposal_actor: ActorContext {
                agent: Some("auto_improve".into()),
                ..ActorContext::default()
            },
            proposals,
        }
    }

    fn sha256(body: &str) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        hasher.finalize().into()
    }

    fn latest_snapshot(
        db_path: &std::path::Path,
        ws: WorkspaceId,
        proj: ProjectId,
        path: &str,
    ) -> (PageId, [u8; 32], i64) {
        let conn = Connection::open(db_path).unwrap();
        let (id, hash, updated): (Vec<u8>, Vec<u8>, i64) = conn.query_row(
            "SELECT id, body_sha256, updated_at FROM pages WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![ws.as_bytes(), proj.as_bytes(), path],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        (
            PageId::from_slice(&id).unwrap(),
            hash.try_into().unwrap(),
            updated,
        )
    }

    #[tokio::test]
    async fn maintenance_scheduler_state_round_trips_per_job() {
        let tmp = TempDir::new().unwrap();
        {
            let store = Store::open(tmp.path()).unwrap();
            assert_eq!(
                store
                    .reader
                    .maintenance_job_last_success(MaintenanceJob::ForgetSweep)
                    .await
                    .unwrap(),
                None
            );

            store
                .writer
                .record_maintenance_job_success(MaintenanceJob::ForgetSweep)
                .await
                .unwrap();
        }
        let store = Store::open(tmp.path()).unwrap();
        assert!(
            store
                .reader
                .maintenance_job_last_success(MaintenanceJob::ForgetSweep)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            store
                .reader
                .maintenance_job_last_success(MaintenanceJob::RuleLint)
                .await
                .unwrap(),
            None
        );
    }

    fn telemetry_count(rows: &[AutoImproveTelemetryCount], key: &str) -> usize {
        rows.iter()
            .find(|row| row.key == key)
            .map(|row| row.count)
            .unwrap_or(0)
    }

    // Issue #157: the documented safety invariant "pinned pages are never
    // rewritten by auto-improvement" is enforced at the single apply point
    // every flow shares (manual approval AND require_approval=false
    // auto-apply), so no prompt phrasing or approval policy can bypass it.
    #[tokio::test]
    async fn approve_refuses_update_proposals_against_pinned_pages() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();

        // A pinned decision record and an unpinned sibling.
        let mut pinned = sample_page(ws, proj, "decisions/adr-0001.md", "immutable decision");
        pinned.pinned = true;
        store.writer.upsert_page(pinned).await.unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "notes/mutable.md", "old body"))
            .await
            .unwrap();

        let staged = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![
                    proposal(
                        "decisions/adr-0001.md",
                        AutoImproveProposalOperation::Update,
                        "rewritten decision",
                    ),
                    proposal(
                        "notes/mutable.md",
                        AutoImproveProposalOperation::Update,
                        "new body",
                    ),
                ],
            ))
            .await
            .unwrap();
        let actor = ActorContext::default();

        // Pinned target: refused as a conflict, nothing written.
        let approve = |proposal_id, path: &str, body: &str| ApproveAutoImproveProposal {
            workspace_id: ws,
            project_id: proj,
            proposal_id,
            page: sample_page(ws, proj, path, body),
            actor: actor.clone(),
            author_id: None,
            checkpoint: None,
        };
        assert_eq!(
            store
                .writer
                .approve_auto_improve_proposal(approve(
                    staged.proposal_ids[0],
                    "decisions/adr-0001.md",
                    "rewritten decision",
                ))
                .await
                .unwrap(),
            ApproveAutoImproveProposalResult::Conflict,
            "pinned target must be refused"
        );
        let detail = store
            .reader
            .auto_improve_proposal_detail(ws, proj, staged.proposal_ids[0])
            .await
            .unwrap()
            .unwrap();
        assert!(
            detail
                .decision_reason
                .as_deref()
                .is_some_and(|r| r.contains("pinned")),
            "decision reason must say WHY: {:?}",
            detail.decision_reason
        );
        let (_, body_hash, _) = latest_snapshot(store.db_path(), ws, proj, "decisions/adr-0001.md");
        assert_eq!(
            body_hash,
            sha256("immutable decision"),
            "pinned body untouched"
        );

        // Unpinned sibling still approves normally.
        assert!(matches!(
            store
                .writer
                .approve_auto_improve_proposal(approve(
                    staged.proposal_ids[1],
                    "notes/mutable.md",
                    "new body",
                ))
                .await
                .unwrap(),
            ApproveAutoImproveProposalResult::Approved { .. }
        ));
    }

    #[tokio::test]
    async fn auto_improve_migration_and_stage_persist_reopen_list_detail_scope() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let other = store
            .writer
            .get_or_create_project(ws, "other", None)
            .await
            .unwrap();
        let staged = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![proposal(
                    "notes/a.md",
                    AutoImproveProposalOperation::Create,
                    "# A",
                )],
            ))
            .await
            .unwrap();
        assert_eq!(staged.proposal_ids.len(), 1);
        let pending = store
            .reader
            .list_auto_improve_proposals(ws, proj, Some(AutoImproveProposalStatus::Pending), 10)
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].target_path.as_str(), "notes/a.md");
        let detail = store
            .reader
            .auto_improve_proposal_detail(ws, proj, staged.proposal_ids[0])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(detail.events.len(), 1);
        assert_eq!(detail.edit_mode, "full_page");
        assert!(detail.patch_json.is_none());
        assert!(detail.expected_base_body_sha256.is_none());
        assert_eq!(
            detail.artifact_path,
            format!("_pending/auto-improve/{}.md", staged.proposal_ids[0])
        );
        assert!(
            store
                .reader
                .auto_improve_proposal_detail(ws, other, staged.proposal_ids[0])
                .await
                .unwrap()
                .is_none()
        );
        drop(store);
        let reopened = Store::open(tmp.path()).unwrap();
        assert_eq!(
            reopened
                .reader
                .list_auto_improve_proposals(ws, proj, None, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn auto_improve_reject_pending_only_records_event() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let id = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![proposal(
                    "notes/r.md",
                    AutoImproveProposalOperation::Create,
                    "# R",
                )],
            ))
            .await
            .unwrap()
            .proposal_ids[0];
        let actor = ActorContext {
            user: Some("reviewer".into()),
            ..ActorContext::default()
        };
        store
            .writer
            .reject_auto_improve_proposal(RejectAutoImproveProposal {
                workspace_id: ws,
                project_id: proj,
                proposal_id: id,
                reason: "nope".into(),
                actor: actor.clone(),
                author_id: None,
            })
            .await
            .unwrap();
        let detail = store
            .reader
            .auto_improve_proposal_detail(ws, proj, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(detail.summary.status, AutoImproveProposalStatus::Rejected);
        assert_eq!(detail.decision_reason.as_deref(), Some("nope"));
        assert_eq!(detail.events.last().unwrap().event, "rejected");
        let rejections = store
            .reader
            .recent_auto_improve_rejections(ws, proj, 10, None)
            .await
            .unwrap();
        assert_eq!(rejections.len(), 1);
        assert_eq!(rejections[0].target_path.as_deref(), Some("notes/r.md"));
        assert_eq!(rejections[0].reason, "nope");
        assert_eq!(rejections[0].source_proposal_id, Some(id));
        assert_eq!(rejections[0].normalized_fingerprint.len(), 64);
        assert!(
            store
                .writer
                .reject_auto_improve_proposal(RejectAutoImproveProposal {
                    workspace_id: ws,
                    project_id: proj,
                    proposal_id: id,
                    reason: "again".into(),
                    actor,
                    author_id: None
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn auto_improve_old_pending_proposal_survives_rejection_buffer_migration() {
        let tmp = TempDir::new().unwrap();
        let db_dir = tmp.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join(DB_FILENAME);
        let mut conn = Connection::open(&db_path).unwrap();
        migrations::run_to(&mut conn, 23).unwrap();
        let ws = ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let proj = ops::get_or_create_project(&mut conn, &ws, "app", None).unwrap();
        // Era-appropriate raw insert: this fixture stops at V23 on purpose,
        // while `stage_run` writes whatever columns the CURRENT schema has, and
        // every later migration that adds one would break a test about an older
        // era.
        let id = ai_memory_core::AutoImproveProposalId::new();
        let run_id = ai_memory_core::AutoImproveRunId::new();
        let now = jiff::Timestamp::now().as_microsecond();
        conn.execute(
            "INSERT INTO auto_improve_runs \
             (id, workspace_id, project_id, warnings_json, rejected_candidates_json, \
              config_json, proposal_actor_json, created_at) \
             VALUES (?1, ?2, ?3, '[]', '[]', '{}', '{}', ?4)",
            params![run_id.as_bytes(), ws.as_bytes(), proj.as_bytes(), now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_improve_proposals \
             (id, run_id, workspace_id, project_id, status, operation, target_path, kind, \
              title, confidence, rationale, evidence_json, body_markdown, body_sha256, \
              artifact_path, staged_at) \
             VALUES (?1, ?2, ?3, ?4, 'pending', 'create', 'notes/old.md', 'note', 'old', \
                     0.9, 'r', '[]', 'body', ?5, ?6, ?7)",
            params![
                id.as_bytes(),
                run_id.as_bytes(),
                ws.as_bytes(),
                proj.as_bytes(),
                &sha256("body")[..],
                auto_improve::artifact_path_for(id),
                now,
            ],
        )
        .unwrap();
        drop(conn);

        let store = Store::open(tmp.path()).unwrap();
        store
            .writer
            .reject_auto_improve_proposal(RejectAutoImproveProposal {
                workspace_id: ws,
                project_id: proj,
                proposal_id: id,
                reason: "old pending still rejectable".into(),
                actor: ActorContext::default(),
                author_id: None,
            })
            .await
            .unwrap();

        let detail = store
            .reader
            .auto_improve_proposal_detail(ws, proj, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(detail.summary.status, AutoImproveProposalStatus::Rejected);
        assert_eq!(detail.edit_mode, "full_page");
        assert_eq!(
            store
                .reader
                .recent_auto_improve_rejections(ws, proj, 10, None)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn auto_improve_stage_derives_snapshots_and_validates_sessions() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let other = store
            .writer
            .get_or_create_project(ws, "other", None)
            .await
            .unwrap();

        store
            .writer
            .upsert_page(sample_page(ws, proj, "notes/update.md", "old"))
            .await
            .unwrap();
        let (latest_id, latest_hash, latest_updated) =
            latest_snapshot(store.db_path(), ws, proj, "notes/update.md");
        let staged = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![
                    proposal(
                        "notes/create.md",
                        AutoImproveProposalOperation::Create,
                        "new",
                    ),
                    proposal(
                        "notes/update.md",
                        AutoImproveProposalOperation::Update,
                        "newer",
                    ),
                ],
            ))
            .await
            .unwrap();
        let create = store
            .reader
            .auto_improve_proposal_detail(ws, proj, staged.proposal_ids[0])
            .await
            .unwrap()
            .unwrap();
        assert!(create.target_latest_page_id_at_stage.is_none());
        assert!(create.target_body_sha256_at_stage.is_none());
        assert!(create.target_updated_at_at_stage.is_none());
        let update = store
            .reader
            .auto_improve_proposal_detail(ws, proj, staged.proposal_ids[1])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(update.target_latest_page_id_at_stage, Some(latest_id));
        assert_eq!(update.target_body_sha256_at_stage, Some(latest_hash));
        assert_eq!(update.target_updated_at_at_stage, Some(latest_updated));

        assert!(
            store
                .writer
                .stage_auto_improve_run(stage_input(
                    ws,
                    proj,
                    vec![proposal(
                        "notes/update.md",
                        AutoImproveProposalOperation::Create,
                        "bad"
                    )],
                ))
                .await
                .is_err()
        );

        let out_of_scope_session = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: out_of_scope_session,
                workspace_id: ws,
                project_id: other,
                agent_kind: AgentKind::Codex,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        let mut input = stage_input(
            ws,
            proj,
            vec![proposal(
                "notes/session.md",
                AutoImproveProposalOperation::Create,
                "session",
            )],
        );
        input.session_id = Some(out_of_scope_session);
        assert!(store.writer.stage_auto_improve_run(input).await.is_err());
    }

    #[tokio::test]
    async fn auto_improve_duplicate_pending_target_rolls_back_run() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        assert!(
            store
                .writer
                .stage_auto_improve_run(stage_input(
                    ws,
                    proj,
                    vec![
                        proposal("notes/dupe.md", AutoImproveProposalOperation::Create, "one"),
                        proposal("notes/dupe.md", AutoImproveProposalOperation::Create, "two"),
                    ],
                ))
                .await
                .is_err()
        );
        assert!(
            store
                .reader
                .list_auto_improve_proposals(ws, proj, None, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn auto_improve_stage_persists_validator_rejections_with_scope_isolation() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let other = store
            .writer
            .get_or_create_project(ws, "other", None)
            .await
            .unwrap();

        let mut input = stage_input(ws, proj, Vec::new());
        input.rejected_candidates_json = serde_json::json!([{
            "reason": "duplicate_existing_path",
            "evidence": "notes/repeat.md",
            "target_path": "notes/repeat.md",
            "kind": "note",
            "operation": "create_or_update",
            "edit_mode": "full_page"
        }]);
        let staged = store.writer.stage_auto_improve_run(input).await.unwrap();

        let rejections = store
            .reader
            .recent_auto_improve_rejections(ws, proj, 10, None)
            .await
            .unwrap();
        assert_eq!(rejections.len(), 1);
        assert_eq!(
            rejections[0].target_path.as_deref(),
            Some("notes/repeat.md")
        );
        assert_eq!(rejections[0].kind.as_deref(), Some("note"));
        assert_eq!(rejections[0].source_run_id, Some(staged.run_id));
        assert_eq!(rejections[0].source_proposal_id, None);
        assert!(
            store
                .reader
                .recent_auto_improve_rejections(ws, other, 10, None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn auto_improve_fail_pending_only_records_event() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let id = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![proposal(
                    "notes/fail.md",
                    AutoImproveProposalOperation::Create,
                    "fail",
                )],
            ))
            .await
            .unwrap()
            .proposal_ids[0];
        let actor = ActorContext {
            agent: Some("admission".into()),
            ..ActorContext::default()
        };
        store
            .writer
            .fail_auto_improve_proposal(FailAutoImproveProposal {
                workspace_id: ws,
                project_id: proj,
                proposal_id: id,
                reason: "admission denied".into(),
                actor: actor.clone(),
                author_id: None,
            })
            .await
            .unwrap();
        let detail = store
            .reader
            .auto_improve_proposal_detail(ws, proj, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(detail.summary.status, AutoImproveProposalStatus::Failed);
        assert_eq!(detail.events.last().unwrap().event, "failed");
        let rejections = store
            .reader
            .recent_auto_improve_rejections(ws, proj, 10, None)
            .await
            .unwrap();
        assert_eq!(rejections.len(), 1);
        assert_eq!(rejections[0].target_path.as_deref(), Some("notes/fail.md"));
        assert_eq!(rejections[0].reason, "admission denied");
        assert!(
            store
                .writer
                .fail_auto_improve_proposal(FailAutoImproveProposal {
                    workspace_id: ws,
                    project_id: proj,
                    proposal_id: id,
                    reason: "again".into(),
                    actor,
                    author_id: None,
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn auto_improve_telemetry_aggregate_counts_learning_activity_only() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let other = store
            .writer
            .get_or_create_project(ws, "other", None)
            .await
            .unwrap();

        store
            .writer
            .upsert_page(sample_page(ws, proj, "notes/update.md", "old update"))
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "procedures/patch.md", "old patch"))
            .await
            .unwrap();

        let mut update = proposal(
            "notes/update.md",
            AutoImproveProposalOperation::Update,
            "new update",
        );
        update.kind = "decision".into();
        let mut patch = proposal(
            "procedures/patch.md",
            AutoImproveProposalOperation::Update,
            "new patch",
        );
        patch.kind = "procedure".into();
        patch.edit_mode = Some("patch".into());
        patch.patch_json = Some(serde_json::json!([{ "op": "append", "content": "new" }]));
        patch.expected_base_body_sha256 = Some(sha256("old patch"));

        let staged = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![
                    proposal(
                        "notes/pending.md",
                        AutoImproveProposalOperation::Create,
                        "pending",
                    ),
                    proposal(
                        "notes/approved.md",
                        AutoImproveProposalOperation::Create,
                        "approved",
                    ),
                    proposal(
                        "notes/rejected.md",
                        AutoImproveProposalOperation::Create,
                        "rejected",
                    ),
                    proposal(
                        "notes/failed.md",
                        AutoImproveProposalOperation::Create,
                        "failed",
                    ),
                    proposal(
                        "notes/conflict.md",
                        AutoImproveProposalOperation::Create,
                        "proposal",
                    ),
                    update,
                    patch,
                ],
            ))
            .await
            .unwrap();
        let approved_id = staged.proposal_ids[1];
        let rejected_id = staged.proposal_ids[2];
        let failed_id = staged.proposal_ids[3];
        let conflict_id = staged.proposal_ids[4];
        let actor = ActorContext::default();

        store
            .writer
            .approve_auto_improve_proposal(ApproveAutoImproveProposal {
                workspace_id: ws,
                project_id: proj,
                proposal_id: approved_id,
                page: sample_page(ws, proj, "notes/approved.md", "approved"),
                actor: actor.clone(),
                author_id: None,
                checkpoint: None,
            })
            .await
            .unwrap();
        store
            .writer
            .reject_auto_improve_proposal(RejectAutoImproveProposal {
                workspace_id: ws,
                project_id: proj,
                proposal_id: rejected_id,
                reason: "human rejected".into(),
                actor: actor.clone(),
                author_id: None,
            })
            .await
            .unwrap();
        store
            .writer
            .fail_auto_improve_proposal(FailAutoImproveProposal {
                workspace_id: ws,
                project_id: proj,
                proposal_id: failed_id,
                reason: "admission denied".into(),
                actor: actor.clone(),
                author_id: None,
            })
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "notes/conflict.md", "external"))
            .await
            .unwrap();
        assert_eq!(
            store
                .writer
                .approve_auto_improve_proposal(ApproveAutoImproveProposal {
                    workspace_id: ws,
                    project_id: proj,
                    proposal_id: conflict_id,
                    page: sample_page(ws, proj, "notes/conflict.md", "proposal"),
                    actor: actor.clone(),
                    author_id: None,
                    checkpoint: None,
                })
                .await
                .unwrap(),
            ApproveAutoImproveProposalResult::Conflict
        );

        let mut curator_report = proposal(
            "reports/curator.md",
            AutoImproveProposalOperation::Create,
            "curator",
        );
        curator_report.kind = "curator_report".into();
        let mut telemetry_report = proposal(
            "reports/auto-improve.md",
            AutoImproveProposalOperation::Create,
            "telemetry",
        );
        telemetry_report.kind = "auto_improve_report".into();
        let maintenance_staged = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![curator_report, telemetry_report],
            ))
            .await
            .unwrap();
        for id in maintenance_staged.proposal_ids {
            store
                .writer
                .reject_auto_improve_proposal(RejectAutoImproveProposal {
                    workspace_id: ws,
                    project_id: proj,
                    proposal_id: id,
                    reason: "maintenance rejected".into(),
                    actor: actor.clone(),
                    author_id: None,
                })
                .await
                .unwrap();
        }

        let mut eval_rejections = stage_input(ws, proj, Vec::new());
        eval_rejections.rejected_candidates_json = serde_json::json!([
            {
                "reason": "eval_gate_failed",
                "target_path": "eval/repeat.md",
                "kind": "note",
                "operation": "create",
                "edit_mode": "full_page",
                "summary": "same eval failure"
            },
            {
                "reason": "eval_gate_failed",
                "target_path": "eval/repeat.md",
                "kind": "note",
                "operation": "create",
                "edit_mode": "full_page",
                "summary": "same eval failure"
            },
            {
                "reason": "eval_gate_timeout",
                "target_path": "eval/timeout.md",
                "summary": "timeout"
            },
            {
                "reason": "eval_gate_error",
                "target_path": "eval/error.md",
                "summary": "error"
            }
        ]);
        store
            .writer
            .stage_auto_improve_run(eval_rejections)
            .await
            .unwrap();

        store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                other,
                vec![proposal(
                    "notes/other.md",
                    AutoImproveProposalOperation::Create,
                    "other",
                )],
            ))
            .await
            .unwrap();

        let aggregate = store
            .reader
            .auto_improve_telemetry_aggregate(ws, proj, 0, 10)
            .await
            .unwrap();
        assert_eq!(aggregate.run_count, 3);
        assert_eq!(aggregate.runs_with_learning_proposals, 1);
        assert_eq!(
            telemetry_count(&aggregate.proposals_by_status, "pending"),
            3
        );
        assert_eq!(
            telemetry_count(&aggregate.proposals_by_status, "approved"),
            1
        );
        assert_eq!(
            telemetry_count(&aggregate.proposals_by_status, "rejected"),
            1
        );
        assert_eq!(telemetry_count(&aggregate.proposals_by_status, "failed"), 1);
        assert_eq!(
            telemetry_count(&aggregate.proposals_by_status, "conflict"),
            1
        );
        assert_eq!(
            telemetry_count(&aggregate.proposals_by_operation, "create"),
            5
        );
        assert_eq!(
            telemetry_count(&aggregate.proposals_by_operation, "update"),
            2
        );
        assert_eq!(
            telemetry_count(&aggregate.proposals_by_edit_mode, "full_page"),
            6
        );
        assert_eq!(
            telemetry_count(&aggregate.proposals_by_edit_mode, "patch"),
            1
        );
        assert_eq!(telemetry_count(&aggregate.proposals_by_kind, "note"), 5);
        assert_eq!(telemetry_count(&aggregate.proposals_by_kind, "decision"), 1);
        assert_eq!(
            telemetry_count(&aggregate.proposals_by_kind, "procedure"),
            1
        );
        assert_eq!(
            telemetry_count(&aggregate.maintenance_proposals_by_kind, "curator_report"),
            1
        );
        assert_eq!(
            telemetry_count(
                &aggregate.maintenance_proposals_by_kind,
                "auto_improve_report"
            ),
            1
        );
        assert_eq!(
            telemetry_count(&aggregate.rejections_by_reason, "human rejected"),
            1
        );
        assert_eq!(
            telemetry_count(&aggregate.rejections_by_reason, "admission denied"),
            1
        );
        assert_eq!(
            telemetry_count(
                &aggregate.rejections_by_reason,
                "target changed since proposal was staged"
            ),
            1
        );
        assert_eq!(
            telemetry_count(&aggregate.rejections_by_reason, "eval_gate_failed"),
            2
        );
        assert_eq!(
            telemetry_count(&aggregate.rejections_by_reason, "eval_gate_timeout"),
            1
        );
        assert_eq!(
            telemetry_count(&aggregate.rejections_by_reason, "eval_gate_error"),
            1
        );
        assert_eq!(
            telemetry_count(&aggregate.rejections_by_reason, "maintenance rejected"),
            0,
            "maintenance/report proposal rejections must not pollute learning telemetry"
        );
        assert_eq!(
            telemetry_count(&aggregate.rejected_targets, "eval/repeat.md"),
            2
        );
        assert_eq!(
            telemetry_count(&aggregate.rejected_targets, "reports/curator.md"),
            0
        );
        assert!(
            aggregate
                .repeated_rejection_fingerprints
                .iter()
                .any(|row| row.count == 2)
        );

        let other_aggregate = store
            .reader
            .auto_improve_telemetry_aggregate(ws, other, 0, 10)
            .await
            .unwrap();
        assert_eq!(other_aggregate.run_count, 1);
        assert_eq!(
            telemetry_count(&other_aggregate.proposals_by_status, "pending"),
            1
        );
    }

    #[tokio::test]
    async fn auto_improve_approve_upserts_page_and_conflicts_are_sql_atomic() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let actor = ActorContext {
            user: Some("approver".into()),
            ..ActorContext::default()
        };

        let create_id = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![proposal(
                    "notes/new.md",
                    AutoImproveProposalOperation::Create,
                    "approved body",
                )],
            ))
            .await
            .unwrap()
            .proposal_ids[0];
        let result = store
            .writer
            .approve_auto_improve_proposal(ApproveAutoImproveProposal {
                workspace_id: ws,
                project_id: proj,
                proposal_id: create_id,
                page: sample_page(ws, proj, "notes/new.md", "approved body"),
                actor: actor.clone(),
                author_id: None,
                checkpoint: Some("ck".into()),
            })
            .await
            .unwrap();
        assert!(matches!(
            result,
            ApproveAutoImproveProposalResult::Approved { .. }
        ));
        let detail = store
            .reader
            .auto_improve_proposal_detail(ws, proj, create_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(detail.summary.status, AutoImproveProposalStatus::Approved);
        assert!(detail.applied_page_id.is_some());
        assert_eq!(
            store
                .reader
                .page_body_by_ids(ws, proj, "notes/new.md")
                .await
                .unwrap()
                .unwrap()
                .body,
            "approved body"
        );

        let stale_create = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![proposal(
                    "notes/existing.md",
                    AutoImproveProposalOperation::Create,
                    "proposal",
                )],
            ))
            .await
            .unwrap()
            .proposal_ids[0];
        store
            .writer
            .upsert_page(sample_page(ws, proj, "notes/existing.md", "external"))
            .await
            .unwrap();
        let conflict = store
            .writer
            .approve_auto_improve_proposal(ApproveAutoImproveProposal {
                workspace_id: ws,
                project_id: proj,
                proposal_id: stale_create,
                page: sample_page(ws, proj, "notes/existing.md", "proposal"),
                actor: actor.clone(),
                author_id: None,
                checkpoint: None,
            })
            .await
            .unwrap();
        assert_eq!(conflict, ApproveAutoImproveProposalResult::Conflict);
        let rejections = store
            .reader
            .recent_auto_improve_rejections(ws, proj, 10, None)
            .await
            .unwrap();
        assert!(rejections.iter().any(|rejection| {
            rejection.target_path.as_deref() == Some("notes/existing.md")
                && rejection.reason == "target changed since proposal was staged"
                && rejection.source_proposal_id == Some(stale_create)
        }));
        assert_eq!(
            store
                .reader
                .page_body_by_ids(ws, proj, "notes/existing.md")
                .await
                .unwrap()
                .unwrap()
                .body,
            "external"
        );

        store
            .writer
            .upsert_page(sample_page(ws, proj, "notes/update.md", "old"))
            .await
            .unwrap();
        let update = proposal(
            "notes/update.md",
            AutoImproveProposalOperation::Update,
            "new",
        );
        let update_id = store
            .writer
            .stage_auto_improve_run(stage_input(ws, proj, vec![update]))
            .await
            .unwrap()
            .proposal_ids[0];
        store
            .writer
            .upsert_page(sample_page(
                ws,
                proj,
                "notes/update.md",
                "changed elsewhere",
            ))
            .await
            .unwrap();
        let conflict = store
            .writer
            .approve_auto_improve_proposal(ApproveAutoImproveProposal {
                workspace_id: ws,
                project_id: proj,
                proposal_id: update_id,
                page: sample_page(ws, proj, "notes/update.md", "new"),
                actor,
                author_id: None,
                checkpoint: None,
            })
            .await
            .unwrap();
        assert_eq!(conflict, ApproveAutoImproveProposalResult::Conflict);
        assert_eq!(
            store
                .reader
                .page_body_by_ids(ws, proj, "notes/update.md")
                .await
                .unwrap()
                .unwrap()
                .body,
            "changed elsewhere"
        );
        assert_eq!(
            sha256("approved body"),
            latest_snapshot(store.db_path(), ws, proj, "notes/new.md").1
        );
    }

    #[tokio::test]
    async fn auto_improve_stage_rejects_patch_base_hash_mismatch() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "procedures/release.md", "old"))
            .await
            .unwrap();
        let mut patch = proposal(
            "procedures/release.md",
            AutoImproveProposalOperation::Update,
            "new",
        );
        patch.edit_mode = Some("patch".into());
        patch.patch_json =
            Some(serde_json::json!([{ "op": "append", "anchor": "## Steps", "content": "new" }]));
        patch.expected_base_body_sha256 = Some(sha256("different old body"));

        let err = store
            .writer
            .stage_auto_improve_run(stage_input(ws, proj, vec![patch]))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("target changed since patch materialization")
        );
        assert!(
            store
                .reader
                .list_auto_improve_proposals(ws, proj, None, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn auto_improve_stage_rejects_patch_missing_base_hash_and_create() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "procedures/release.md", "old"))
            .await
            .unwrap();

        let mut missing_hash = proposal(
            "procedures/release.md",
            AutoImproveProposalOperation::Update,
            "new",
        );
        missing_hash.edit_mode = Some("patch".into());
        missing_hash.patch_json = Some(serde_json::json!([{ "op": "append" }]));
        let err = store
            .writer
            .stage_auto_improve_run(stage_input(ws, proj, vec![missing_hash]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing expected base body hash"));

        let mut create_patch = proposal(
            "procedures/new.md",
            AutoImproveProposalOperation::Create,
            "new",
        );
        create_patch.edit_mode = Some("patch".into());
        create_patch.patch_json = Some(serde_json::json!([{ "op": "append" }]));
        create_patch.expected_base_body_sha256 = Some(sha256("old"));
        let err = store
            .writer
            .stage_auto_improve_run(stage_input(ws, proj, vec![create_patch]))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("patch proposal must use update operation")
        );
    }

    #[tokio::test]
    async fn auto_improve_approval_rejects_mismatched_page_author() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let proposal_id = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![proposal(
                    "notes/author.md",
                    AutoImproveProposalOperation::Create,
                    "body",
                )],
            ))
            .await
            .unwrap()
            .proposal_ids[0];
        let mut page = sample_page(ws, proj, "notes/author.md", "body");
        page.author_id = Some(UserId::new());
        assert!(
            store
                .writer
                .approve_auto_improve_proposal(ApproveAutoImproveProposal {
                    workspace_id: ws,
                    project_id: proj,
                    proposal_id,
                    page,
                    actor: ActorContext::default(),
                    author_id: None,
                    checkpoint: None,
                })
                .await
                .is_err()
        );
        assert!(
            store
                .reader
                .page_body_by_ids(ws, proj, "notes/author.md")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn auto_improve_project_move_restamps_proposal_scope() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let src_ws = store
            .writer
            .get_or_create_workspace("source")
            .await
            .unwrap();
        let dst_ws = store.writer.get_or_create_workspace("dest").await.unwrap();
        let proj = store
            .writer
            .get_or_create_project(src_ws, "app", None)
            .await
            .unwrap();
        store
            .writer
            .ensure_auto_improve_scheduler_state(src_ws, proj)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        let claimed_session = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: claimed_session,
                workspace_id: src_ws,
                project_id: proj,
                agent_kind: AgentKind::OpenCode,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        store
            .writer
            .end_session(claimed_session, None)
            .await
            .unwrap();
        let candidate = store
            .reader
            .auto_improve_candidate_sessions(src_ws, proj, 0, 1)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert!(
            store
                .writer
                .claim_auto_improve_scheduler_session(
                    src_ws,
                    proj,
                    candidate.session_id,
                    candidate.ended_at,
                )
                .await
                .unwrap()
        );
        let proposal_id = store
            .writer
            .stage_auto_improve_run(stage_input(
                src_ws,
                proj,
                vec![proposal(
                    "notes/move.md",
                    AutoImproveProposalOperation::Create,
                    "move",
                )],
            ))
            .await
            .unwrap()
            .proposal_ids[0];
        let summary = store
            .writer
            .move_project_workspace(proj, src_ws, dst_ws)
            .await
            .unwrap();
        assert_eq!(summary.auto_improve_runs_moved, 1);
        assert_eq!(summary.auto_improve_proposals_moved, 1);
        assert_eq!(summary.auto_improve_scheduler_state_moved, 1);
        assert_eq!(summary.auto_improve_scheduler_claims_moved, 1);
        assert!(
            store
                .reader
                .auto_improve_proposal_detail(src_ws, proj, proposal_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .reader
                .auto_improve_proposal_detail(dst_ws, proj, proposal_id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .reader
                .auto_improve_candidate_sessions(dst_ws, proj, 0, 10)
                .await
                .unwrap()
                .is_empty(),
            "moved scheduler claims should keep claimed sessions suppressed"
        );
    }

    /// `session_brief_pages` returns pinned / `_rules/` / `_slots/` pages
    /// WITH bodies (pinned first, then path order), recent titles for the
    /// whole project, and never leaks a sibling project's pages.
    #[tokio::test]
    async fn session_brief_pages_selects_core_pages_and_isolates_projects() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let other = store
            .writer
            .get_or_create_project(ws, "other", None)
            .await
            .unwrap();

        let mut adr = sample_page(ws, proj, "decisions/adr-001.md", "single writer actor");
        adr.pinned = true;
        store.writer.upsert_page(adr).await.unwrap();
        store
            .writer
            .upsert_page(sample_page(
                ws,
                proj,
                "_rules/style.md",
                "no unwrap in runtime",
            ))
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "_slots/focus.md", "shipping v2 auth"))
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "concepts/queue.md", "ordinary page"))
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, other, "_rules/other.md", "sibling rule"))
            .await
            .unwrap();

        let (core, recent) = store
            .reader
            .session_brief_pages(ws, proj, 24, 10)
            .await
            .unwrap();

        let core_paths: Vec<&str> = core.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(
            core_paths,
            vec!["decisions/adr-001.md", "_rules/style.md", "_slots/focus.md"],
            "core = pinned first, then _rules/ + _slots/ by path; no ordinary pages"
        );
        assert!(core[0].pinned, "pinned flag must survive the round-trip");
        assert_eq!(
            core[1].body, "no unwrap in runtime",
            "core pages carry bodies"
        );

        let recent_paths: Vec<&str> = recent.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(
            recent.len(),
            4,
            "recent lists every latest page in the project"
        );
        assert!(
            recent_paths.contains(&"concepts/queue.md"),
            "ordinary pages appear as recent pointers"
        );
        assert!(
            !recent_paths.contains(&"_rules/other.md") && core.len() == 3,
            "sibling project pages must not leak into the brief"
        );
    }

    #[tokio::test]
    async fn cross_project_links_surface_in_graph_briefing_and_lint() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let app = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let infra = store
            .writer
            .get_or_create_project(ws, "infra", None)
            .await
            .unwrap();

        // Target page in `infra`, then a page in `app` that depends on it
        // plus a dangling link to a non-existent project.
        store
            .writer
            .upsert_page(sample_page(ws, infra, "runbooks/02.md", "the runbook"))
            .await
            .unwrap();
        let mut dep = sample_page(ws, app, "concepts/dep.md", "needs infra + a typo");
        dep.links = vec![
            LinkTarget {
                workspace: None,
                project: Some("infra".into()),
                path: PagePath::new("runbooks/02.md").unwrap(),
            },
            LinkTarget {
                workspace: None,
                project: Some("nope".into()),
                path: PagePath::new("ghost.md").unwrap(),
            },
        ];
        store.writer.upsert_page(dep).await.unwrap();

        // Graph: exactly one resolved cross-project edge, app -> infra.
        let edges = store.reader.cross_project_edges(None).await.unwrap();
        assert_eq!(edges.len(), 1, "one resolved cross-project edge");
        assert_eq!(edges[0].from_project, "app");
        assert_eq!(edges[0].to_project, "infra");

        // Briefing degree: app depends on 1 project; infra has 1 dependent.
        let app_brief = store
            .reader
            .briefing_for_project(ws, app, 5, ai_memory_core::OwnerFilter::Any)
            .await
            .unwrap();
        assert_eq!(app_brief.cross_project_dependencies, 1);
        assert_eq!(app_brief.cross_project_dependents, 0);
        let infra_brief = store
            .reader
            .briefing_for_project(ws, infra, 5, ai_memory_core::OwnerFilter::Any)
            .await
            .unwrap();
        assert_eq!(infra_brief.cross_project_dependents, 1);

        // Lint: the dangling link to project `nope` is reported as unknown.
        let dangling = store
            .reader
            .dangling_cross_project_links(ws, app)
            .await
            .unwrap();
        assert_eq!(dangling.len(), 1, "only the unresolved `nope` link");
        assert_eq!(dangling[0].project, "nope");
        assert!(!dangling[0].project_exists);
    }

    #[tokio::test]
    async fn open_and_upsert_page() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();
        let id_a = store
            .writer
            .upsert_page(sample_page(ws, proj, "foo.md", "hello"))
            .await
            .unwrap();
        // Same body again: returns the same id, no supersession.
        let id_b = store
            .writer
            .upsert_page(sample_page(ws, proj, "foo.md", "hello"))
            .await
            .unwrap();
        assert_eq!(id_a, id_b);
        // Different body: supersession produces a new id.
        let id_c = store
            .writer
            .upsert_page(sample_page(ws, proj, "foo.md", "hello world"))
            .await
            .unwrap();
        assert_ne!(id_b, id_c);
    }

    #[tokio::test]
    async fn get_or_create_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let a = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let b = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        assert_eq!(a, b);
        let pa = store
            .writer
            .get_or_create_project(a, "scratch", None)
            .await
            .unwrap();
        let pb = store
            .writer
            .get_or_create_project(a, "scratch", None)
            .await
            .unwrap();
        assert_eq!(pa, pb);
    }

    #[tokio::test]
    async fn session_agent_kind_migrations_preserve_observations() {
        let tmp = TempDir::new().unwrap();
        let db_dir = tmp.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join(DB_FILENAME);
        let mut conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        crate::migrations::run_to(&mut conn, 8).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        let ws = WorkspaceId::new();
        let proj = ProjectId::new();
        let sid = SessionId::new();
        let oid = ObservationId::new();
        conn.execute(
            "INSERT INTO workspaces (id, name, created_at) VALUES (?1, 'default', 1)",
            params![ws.as_bytes()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO projects (id, workspace_id, name, created_at) \
             VALUES (?1, ?2, 'scratch', 1)",
            params![proj.as_bytes(), ws.as_bytes()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, started_at) \
             VALUES (?1, ?2, ?3, 'open-code', 1)",
            params![sid.as_bytes(), ws.as_bytes(), proj.as_bytes()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO observations (id, session_id, workspace_id, project_id, kind, title, body, created_at) \
             VALUES (?1, ?2, ?3, ?4, 'user_prompt', 'keep', 'this observation must survive', 1)",
            params![oid.as_bytes(), sid.as_bytes(), ws.as_bytes(), proj.as_bytes()],
        )
        .unwrap();
        drop(conn);

        let store = Store::open(tmp.path()).unwrap();
        let count = store.reader.status_counts().await.unwrap().observations;
        assert_eq!(count, 1, "V09 must not cascade-delete observations");

        store
            .writer
            .begin_session(NewSession {
                id: SessionId::new(),
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::AntigravityCli,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn serialises_parallel_writes() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();
        // Spawn 16 concurrent writes; the writer must serialise them.
        let mut handles = Vec::new();
        for i in 0..16 {
            let w = store.writer.clone();
            handles.push(tokio::spawn(async move {
                w.upsert_page(sample_page(
                    ws,
                    proj,
                    &format!("p{i}.md"),
                    &format!("body-{i}"),
                ))
                .await
            }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn recent_pages_returns_latest_only_in_order() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        for i in 0..3u8 {
            store
                .writer
                .upsert_page(sample_page(
                    ws,
                    proj,
                    &format!("p{i}.md"),
                    &format!("body-{i}"),
                ))
                .await
                .unwrap();
        }
        // Bump the second page to force a later updated_at.
        store
            .writer
            .upsert_page(sample_page(ws, proj, "p1.md", "body-1-rev"))
            .await
            .unwrap();

        let hits = store.reader.recent_pages(10).await.unwrap();
        assert_eq!(hits.len(), 3, "is_latest filter should give us 3 pages");
        assert_eq!(
            hits[0].path.as_str(),
            "p1.md",
            "most-recently-updated first"
        );
    }

    #[tokio::test]
    async fn status_counts_zero_on_fresh_db() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let counts = store.reader.status_counts().await.unwrap();
        assert_eq!(counts.pages_latest, 0);
        assert_eq!(counts.pages_all, 0);
        assert_eq!(counts.sessions, 0);
        assert_eq!(counts.observations, 0);
    }

    #[tokio::test]
    async fn reindex_target_status_tracks_clean_and_dirty_store() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();

        let clean = store.reader.reindex_target_status().await.unwrap();
        assert!(clean.is_clean(), "fresh migrated DB must be reindex-clean");

        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "alpha.md", "body"))
            .await
            .unwrap();

        let dirty = store.reader.reindex_target_status().await.unwrap();
        assert!(
            !dirty.is_clean(),
            "existing rows must block lifecycle reindex"
        );
        assert!(dirty.nonzero_summary().contains("pages=1"));
    }

    #[tokio::test]
    async fn search_finds_inserted_page_and_counts_reflect_supersession() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();

        store
            .writer
            .upsert_page(sample_page(
                ws,
                proj,
                "alpha.md",
                "the quick brown fox jumps over the lazy dog",
            ))
            .await
            .unwrap();

        let hits = store.reader.search_pages("quick".into(), 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path.as_str(), "alpha.md");
        assert!(hits[0].snippet.contains("<mark>quick</mark>"));

        // Supersede: only the latest version should appear in counts'
        // pages_latest, and search should still return exactly one hit.
        store
            .writer
            .upsert_page(sample_page(
                ws,
                proj,
                "alpha.md",
                "a different sentence with quick still inside",
            ))
            .await
            .unwrap();

        let counts = store.reader.status_counts().await.unwrap();
        assert_eq!(counts.pages_latest, 1);
        assert_eq!(counts.pages_all, 2);

        let hits = store.reader.search_pages("quick".into(), 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].snippet.contains("different"),
            "snippet should come from the latest version, got: {}",
            hits[0].snippet
        );
    }

    #[tokio::test]
    async fn search_ranks_page_authority_without_hiding_session_evidence() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();

        let mut decision = sample_page(
            ws,
            proj,
            "decisions/embedding-policy.md",
            "The current decision keeps LLM embeddings optional and uses them only for reranking.",
        );
        decision.title = "LLM embedding policy".into();
        decision.pinned = true;
        decision.frontmatter_json =
            serde_json::json!({"tags": ["decision", "canonical", "active"]});
        store.writer.upsert_page(decision).await.unwrap();

        let mut session = sample_page(
            ws,
            proj,
            "sessions/2026-07-27.md",
            "What is the current decision about LLM embeddings? This session preserves historical evidence and chronicleonlytoken.",
        );
        session.title = "What is the current decision about LLM embeddings?".into();
        session.tier = Tier::Episodic;
        store.writer.upsert_page(session).await.unwrap();

        for idx in 0..5 {
            let mut noisy_session = sample_page(
                ws,
                proj,
                &format!("sessions/noisy-{idx}.md"),
                "What is the current decision about LLM embeddings? This session repeats the question as historical evidence.",
            );
            noisy_session.title = "What is the current decision about LLM embeddings?".into();
            noisy_session.tier = Tier::Episodic;
            store.writer.upsert_page(noisy_session).await.unwrap();
        }

        let mut rejected = sample_page(
            ws,
            proj,
            "sessions/rejected-fixture.md",
            "What is the current decision about LLM embeddings? This obsolete answer repeats LLM embeddings current decision.",
        );
        rejected.title = "Current decision about LLM embeddings".into();
        rejected.tier = Tier::Episodic;
        rejected.frontmatter_json =
            serde_json::json!({"tags": ["superseded", "do_not_answer_from"]});
        store.writer.upsert_page(rejected).await.unwrap();

        let query = "what is the current decision about LLM embeddings";
        let scoped = store
            .reader
            .search_pages_for_project(ws, proj, query.into(), 10, None)
            .await
            .unwrap();
        let scoped_paths: Vec<&str> = scoped.iter().map(|hit| hit.path.as_str()).collect();
        assert_eq!(
            scoped_paths[0], "decisions/embedding-policy.md",
            "authority-ranked hits: {scoped:?}"
        );
        assert_eq!(
            scoped_paths.last().copied(),
            Some("sessions/rejected-fixture.md")
        );
        assert!(scoped.windows(2).all(|pair| pair[0].rank <= pair[1].rank));

        let top_one = store
            .reader
            .search_pages_for_project(ws, proj, query.into(), 1, None)
            .await
            .unwrap();
        assert_eq!(top_one[0].path.as_str(), "decisions/embedding-policy.md");

        let global = store
            .reader
            .search_pages_with_meta(query.into(), 10, None)
            .await
            .unwrap();
        assert_eq!(global[0].path.as_str(), "decisions/embedding-policy.md");

        let hybrid = store
            .reader
            .hybrid_search(
                ws,
                proj,
                query.into(),
                None,
                String::new(),
                String::new(),
                0,
                1,
                None,
            )
            .await
            .unwrap();
        assert_eq!(hybrid[0].path.as_str(), "decisions/embedding-policy.md");

        let session_evidence = store
            .reader
            .search_pages_for_project(ws, proj, "chronicleonlytoken".into(), 10, None)
            .await
            .unwrap();
        assert_eq!(session_evidence.len(), 1);
        assert_eq!(session_evidence[0].path.as_str(), "sessions/2026-07-27.md");
    }

    /// Regression: bare `word:` in agent queries is FTS5 column syntax, not
    /// a literal token (`no such column: pick` / `memory`).
    #[tokio::test]
    async fn search_colon_tokens_do_not_error() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(
                ws,
                proj,
                "handoff.md",
                "pick up handoff context from ai-memory bootstrap",
            ))
            .await
            .unwrap();

        let hits = store
            .reader
            .search_pages("pick: handoff bootstrap".into(), 10)
            .await
            .unwrap();
        assert!(
            !hits.is_empty(),
            "colon-sanitized query should match without SQLite column error"
        );
    }

    #[tokio::test]
    async fn search_is_accent_insensitive() {
        // V13: an accent-free query matches accented stored text (PT-friendly).
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(
                ws,
                proj,
                "notes/decisao.md",
                "a descrição da sessão e a consolidação dos commits",
            ))
            .await
            .unwrap();

        let hits = store
            .reader
            .search_pages("descricao sessao".into(), 10)
            .await
            .unwrap();
        assert!(
            !hits.is_empty(),
            "accent-free query must match accented stored text"
        );
    }

    #[tokio::test]
    async fn search_boolean_or_still_works() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "quick.md", "quick answer"))
            .await
            .unwrap();

        let hits = store
            .reader
            .search_pages("quick OR slow".into(), 10)
            .await
            .unwrap();
        assert!(!hits.is_empty(), "OR must remain an FTS5 operator");
    }

    #[tokio::test]
    async fn search_quotes_hyphenated_tokens_for_fts5() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();

        store
            .writer
            .upsert_page(sample_page(
                ws,
                proj,
                "hyphen.md",
                "the ai-memory token should be searchable",
            ))
            .await
            .unwrap();

        let hits = store
            .reader
            .search_pages_for_project(ws, proj, "ai-memory".into(), 10, None)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path.as_str(), "hyphen.md");
    }

    #[tokio::test]
    async fn linked_neighbor_hit_describes_the_page_not_its_heading() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();

        // A compiled page opens with its title and a structural heading. The
        // graph-neighbour path has no matched term to centre an excerpt on,
        // so before the descriptor fix its snippet was exactly this preamble.
        let body = concat!(
            "# Session 2026-04-20\n",
            "\n",
            "## Decisions\n",
            "\n",
            "Dropped the retry queue because the writer actor already serialises.\n",
        );
        store
            .writer
            .upsert_page(sample_page(ws, proj, "target.md", body))
            .await
            .unwrap();
        let mut source = sample_page(ws, proj, "source.md", "needle source content");
        source.links = vec![PagePath::new("target.md").unwrap().into()];
        store.writer.upsert_page(source).await.unwrap();

        let hits = store
            .reader
            .hybrid_search(
                ws,
                proj,
                "needle".into(),
                None,
                String::new(),
                String::new(),
                0,
                10,
                None,
            )
            .await
            .unwrap();
        let neighbor = hits
            .iter()
            .find(|h| h.path.as_str() == "target.md")
            .expect("linked neighbor should be included");
        assert_eq!(
            neighbor.snippet,
            "Dropped the retry queue because the writer actor already serialises.",
            "neighbour snippet should describe the page, not repeat its heading"
        );
    }

    /// The body a session page is synthesised with, minus the frontmatter.
    /// Written out in full because the point of the test below is what a
    /// reader can learn from this page *without* opening it.
    const SESSION_BODY: &str = concat!(
        "# Bound the scheduler queue\n",
        "\n",
        "## Session metadata\n",
        "\n",
        "- **session_id:** `9f2c1d0e`\n",
        "- **started_at:** 2026-08-24T10:00:00Z\n",
        "- **ended_at:** 2026-08-24T10:18:00Z\n",
        "- **observations:** 41\n",
        "\n",
        "## Prompts\n",
        "\n",
        "1. Bound the scheduler queue\n",
        "2. Make the backpressure test deterministic\n",
    );

    /// #513: `memory_handoff_cancel` takes an exact id and nothing exposed
    /// one, so an accumulated backlog was visible as a count and impossible
    /// to address. Oldest-first because the operator's question is which are
    /// stale.
    #[tokio::test]
    async fn open_handoffs_are_listed_oldest_first_and_exclude_accepted() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();
        let new_handoff = |agent: AgentKind| NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: agent,
            to_agent: None,
            cwd: None,
            summary: "continue".into(),
            open_questions: Vec::new(),
            next_steps: Vec::new(),
            files_touched: Vec::new(),
            owner_user: None,
        };
        let first = store
            .writer
            .insert_handoff(new_handoff(AgentKind::ClaudeCode))
            .await
            .unwrap();
        let second = store
            .writer
            .insert_handoff(new_handoff(AgentKind::Codex))
            .await
            .unwrap();

        let open = store
            .reader
            .open_handoffs_for_project(ws, proj, 50)
            .await
            .unwrap();
        assert_eq!(open.len(), 2, "both handoffs are open");
        assert_eq!(
            open[0].id,
            first.to_string(),
            "oldest first: {:?}",
            open.iter().map(|h| &h.id).collect::<Vec<_>>()
        );
        assert_eq!(open[1].id, second.to_string());
        assert_eq!(open[0].from_agent, "claude-code");
        assert_eq!(open[1].from_agent, "codex");

        // The listing must answer "what is still pending", so an accepted
        // handoff has to drop out — otherwise it re-proposes work already
        // taken, which is the failure the backlog already causes.
        store
            .writer
            .accept_handoff(HandoffAcceptance {
                handoff_id: first,
                workspace_id: ws,
                project_id: proj,
                accepting_agent: AgentKind::Codex,
                accepting_session: None,
                accepting_user: None,
                owner_filter: ai_memory_core::OwnerFilter::Any,
                receiving_cwd: None,
            })
            .await
            .unwrap();
        let open = store
            .reader
            .open_handoffs_for_project(ws, proj, 50)
            .await
            .unwrap();
        assert_eq!(open.len(), 1, "the accepted handoff must not be listed");
        assert_eq!(open[0].id, second.to_string());
    }

    #[tokio::test]
    async fn a_written_summary_changes_what_the_descriptor_tells_the_reader() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();

        // Same page twice. The only difference is that one was written by a
        // path that fills `summary` and the other by one that does not.
        let mut without = sample_page(ws, proj, "without-summary.md", SESSION_BODY);
        without.title = "Bound the scheduler queue".into();
        store.writer.upsert_page(without).await.unwrap();

        let mut with = sample_page(ws, proj, "with-summary.md", SESSION_BODY);
        with.title = "Bound the scheduler queue".into();
        with.frontmatter_json = serde_json::json!({
            "summary": "2 prompts, 34 completed tool calls across Bash, Edit and Read, over 18m.",
        });
        store.writer.upsert_page(with).await.unwrap();

        let hits = store
            .reader
            .recent_pages_for_project(ws, proj, 10)
            .await
            .unwrap();
        let descriptor = |path: &str| {
            hits.iter()
                .find(|h| h.path.as_str() == path)
                .map(|h| h.snippet.clone())
                .unwrap_or_else(|| panic!("{path} should be listed"))
        };
        let without = descriptor("without-summary.md");
        let with = descriptor("with-summary.md");

        assert_ne!(without, with);

        // Without a summary the descriptor is the best the body affords: the
        // prompts after the first, since the first repeats the title. That is
        // real signal, and it is what #463 already delivers.
        assert!(
            without.contains("Make the backpressure test deterministic"),
            "body-derived descriptor should reach the prompts: {without:?}"
        );
        // What it cannot say is how much work the session was. Those counts
        // exist only at write time, and the summary is what carries them.
        assert!(
            !without.contains("34 completed tool calls"),
            "the body never states the totals: {without:?}"
        );
        assert!(
            with.contains("2 prompts") && with.contains("34 completed tool calls"),
            "summary-derived descriptor should state the session's shape: {with:?}"
        );
    }

    #[tokio::test]
    async fn frontmatter_summary_wins_over_the_body_lede() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();

        let mut page = sample_page(ws, proj, "curated.md", "Body prose that loses.");
        page.frontmatter_json = serde_json::json!({ "summary": "Curated one-liner." });
        store.writer.upsert_page(page).await.unwrap();

        let hits = store
            .reader
            .recent_pages_for_project(ws, proj, 10)
            .await
            .unwrap();
        let hit = hits
            .iter()
            .find(|h| h.path.as_str() == "curated.md")
            .expect("page should be listed");
        assert_eq!(hit.snippet, "Curated one-liner.");
    }

    #[tokio::test]
    async fn blank_frontmatter_summary_falls_back_to_the_body_lede() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();

        let mut page = sample_page(ws, proj, "blank.md", "Body prose that wins.");
        page.frontmatter_json = serde_json::json!({ "summary": "   " });
        store.writer.upsert_page(page).await.unwrap();

        let hits = store
            .reader
            .recent_pages_for_project(ws, proj, 10)
            .await
            .unwrap();
        let hit = hits
            .iter()
            .find(|h| h.path.as_str() == "blank.md")
            .expect("page should be listed");
        assert_eq!(hit.snippet, "Body prose that wins.");
    }

    #[tokio::test]
    async fn hybrid_search_includes_linked_neighbors() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();

        let target_id = store
            .writer
            .upsert_page(sample_page(ws, proj, "target.md", "neighbor-only content"))
            .await
            .unwrap();
        store
            .writer
            .store_embedding(
                target_id,
                f32_vec_to_bytes(&[1.0, 0.0]),
                "test".into(),
                "two-dim".into(),
                2,
            )
            .await
            .unwrap();
        let mut source = sample_page(ws, proj, "source.md", "needle source content");
        source.links = vec![PagePath::new("target.md").unwrap().into()];
        store.writer.upsert_page(source).await.unwrap();

        let hits = store
            .reader
            .hybrid_search(
                ws,
                proj,
                "needle".into(),
                None,
                String::new(),
                String::new(),
                0,
                10,
                None,
            )
            .await
            .unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert!(paths.contains(&"source.md"));
        assert!(
            paths.contains(&"target.md"),
            "linked neighbor should be included"
        );

        let explained = store
            .reader
            .hybrid_search_explained(
                ws,
                proj,
                "needle".into(),
                None,
                String::new(),
                String::new(),
                0,
                10,
                None,
            )
            .await
            .unwrap();
        let explained_paths: Vec<&str> =
            explained.iter().map(|(hit, _)| hit.path.as_str()).collect();
        assert_eq!(paths, explained_paths, "explain must not change ranking");

        let (source_hit, source_details) = explained
            .iter()
            .find(|(hit, _)| hit.path.as_str() == "source.md")
            .unwrap();
        assert_eq!(source_details.fts_rank, Some(1));
        assert!(source_details.vector_rank.is_none());
        assert!(source_details.graph_rank.is_none());
        // #486: the hydrate must fill only what is empty. An FTS hit carries
        // a `snippet()` excerpt centred on the matched term, which is more
        // useful than the generic page descriptor — re-describing every hit
        // would quietly downgrade it, so the fix keys on an empty title.
        assert!(
            source_hit.snippet.contains("needle"),
            "an FTS hit must keep its term-centred excerpt, got {:?}",
            source_hit.snippet
        );

        let (target_hit, target_details) = explained
            .iter()
            .find(|(hit, _)| hit.path.as_str() == "target.md")
            .unwrap();
        assert!(target_details.fts_rank.is_none());
        assert!(target_details.vector_rank.is_none());
        assert_eq!(target_details.graph_rank, Some(1));
        let via = target_details.graph_via.as_ref().unwrap();
        assert_eq!(via.seed_path, "source.md");
        assert_eq!(via.direction, "outgoing");

        for (hit, details) in [(source_hit, source_details), (target_hit, target_details)] {
            let authority = details.authority.unwrap();
            let expected_rank = -(details.fused * authority);
            assert!(
                (hit.rank - expected_rank).abs() < f64::EPSILON,
                "rank must equal -(fused * authority): {hit:?} {details:?}"
            );
            assert!(
                (details.fused
                    - (details.rrf.fts
                        + details.rrf.entity
                        + details.rrf.vector
                        + details.rrf.graph))
                    .abs()
                    < f64::EPSILON
            );
        }

        let vector_explained = store
            .reader
            .hybrid_search_explained(
                ws,
                proj,
                "needle".into(),
                Some(vec![1.0, 0.0]),
                "test".into(),
                "two-dim".into(),
                2,
                10,
                None,
            )
            .await
            .unwrap();
        let (_, target_vector_details) = vector_explained
            .iter()
            .find(|(hit, _)| hit.path.as_str() == "target.md")
            .unwrap();
        assert_eq!(target_vector_details.vector_rank, Some(1));
        assert_eq!(target_vector_details.cosine, Some(1.0));
        assert!(target_vector_details.rrf.vector > 0.0);

        // #486: a hit the vector stream ranked must still be describable.
        // The fusion map is built with `or_insert_with` and the vector loop
        // has only `(PageId, PagePath, f32)` to insert, so before the
        // post-fusion hydrate these came back as empty strings — and with a
        // reranker enabled an empty candidate is scored as an empty document
        // and can be truncated away. This test previously asserted the rank
        // and nothing about what the caller actually receives.
        let (target_vector_hit, _) = vector_explained
            .iter()
            .find(|(hit, _)| hit.path.as_str() == "target.md")
            .unwrap();
        assert!(
            !target_vector_hit.title.is_empty(),
            "a vector-ranked hit must carry a title, got {:?}",
            target_vector_hit.title
        );
        assert!(
            !target_vector_hit.snippet.is_empty(),
            "a vector-ranked hit must carry a snippet, got {:?}",
            target_vector_hit.snippet
        );
    }

    #[tokio::test]
    async fn observation_fts_finds_raw_fallback_hits() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::OpenCode,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "prompt".into(),
                    body: "the raw-only zebra detail lives here".into(),
                    importance: 5,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();

        let hits = store
            .reader
            .search_observations_for_project(ws, proj, "zebra".into(), 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, session_id);
        assert!(hits[0].snippet.contains("<mark>zebra</mark>"));
    }

    /// The public writer API only accepts `Sanitized<NewObservation>`, and
    /// `Sanitized::new` is the single constructor — so a secret passed in an
    /// observation body cannot reach disk unscrubbed. This locks the typed
    /// boundary end-to-end: through the writer, into the SQLite `body` column.
    #[tokio::test]
    async fn insert_observation_boundary_scrubs_before_disk() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "Authorization: Bearer abcdef0123456789ABCDEF0123456789".into(),
                    body: "leaked Authorization: Bearer abcdef0123456789ABCDEF0123456789 in transcript"
                        .into(),
                    importance: 5,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();

        let conn = Connection::open(store.db_path()).unwrap();
        let (title, body): (String, String) = conn
            .query_row("SELECT title, body FROM observations", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        for col in [&title, &body] {
            assert!(col.contains("[REDACTED]"), "expected scrub in: {col}");
            assert!(
                !col.contains("abcdef0123"),
                "secret reached disk unscrubbed: {col}"
            );
        }
    }

    /// Ingest idempotency distinguishes pending and completed replays, scopes
    /// keys per project, and permits reuse after the TTL.
    #[tokio::test]
    async fn insert_observation_ingest_dedups_on_key() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj_a = store
            .writer
            .get_or_create_project(ws, "project-a", None)
            .await
            .unwrap();
        let proj_b = store
            .writer
            .get_or_create_project(ws, "project-b", None)
            .await
            .unwrap();
        let session_a = SessionId::new();
        let session_b = SessionId::new();
        for (session_id, project_id) in [(session_a, proj_a), (session_b, proj_b)] {
            store
                .writer
                .begin_session(NewSession {
                    id: session_id,
                    workspace_id: ws,
                    project_id,
                    agent_kind: AgentKind::ClaudeCode,
                    cwd: None,
                    actor_user: None,
                })
                .await
                .unwrap();
        }
        let obs = |session_id, project_id| NewObservation {
            session_id,
            workspace_id: ws,
            project_id,
            kind: ObservationKind::UserPrompt,
            extension: None,
            source_event: None,
            title: "prompt".into(),
            body: "hello".into(),
            importance: 5,
        };

        let first = store
            .writer
            .insert_observation_ingest(
                Sanitized::new(obs(session_a, proj_a), &Sanitizer::builtin()),
                "entry-1".into(),
            )
            .await
            .unwrap();
        assert!(matches!(first, IngestObservationOutcome::Inserted(_)));
        let pending = store
            .writer
            .insert_observation_ingest(
                Sanitized::new(obs(session_a, proj_a), &Sanitizer::builtin()),
                "entry-1".into(),
            )
            .await
            .unwrap();
        assert_eq!(pending, IngestObservationOutcome::ResumePending);
        store
            .writer
            .complete_observation_ingest(proj_a, "entry-1".into())
            .await
            .unwrap();
        let complete = store
            .writer
            .insert_observation_ingest(
                Sanitized::new(obs(session_a, proj_a), &Sanitizer::builtin()),
                "entry-1".into(),
            )
            .await
            .unwrap();
        assert_eq!(complete, IngestObservationOutcome::AlreadyComplete);

        // The same untrusted token cannot suppress an event in another project.
        let other_project = store
            .writer
            .insert_observation_ingest(
                Sanitized::new(obs(session_b, proj_b), &Sanitizer::builtin()),
                "entry-1".into(),
            )
            .await
            .unwrap();
        assert!(matches!(
            other_project,
            IngestObservationOutcome::Inserted(_)
        ));
        let conn = Connection::open(store.db_path()).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM observations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 2, "replay must not append a row");

        // Sweep runs before lookup, so an expired current key can be reused
        // without waiting for an unrelated keyed insert.
        conn.execute(
            "UPDATE ingest_keys SET seen_at = 1 \
             WHERE project_id = ?1 AND key = 'entry-1'",
            params![proj_a.as_bytes()],
        )
        .unwrap();
        drop(conn);
        let reused = store
            .writer
            .insert_observation_ingest(
                Sanitized::new(obs(session_a, proj_a), &Sanitizer::builtin()),
                "entry-1".into(),
            )
            .await
            .unwrap();
        assert!(matches!(reused, IngestObservationOutcome::Inserted(_)));
    }

    #[tokio::test]
    async fn latest_completed_session_for_project_ignores_open_sessions() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();
        let first = SessionId::new();
        let open = SessionId::new();
        for id in [first, open] {
            store
                .writer
                .begin_session(NewSession {
                    id,
                    workspace_id: ws,
                    project_id: proj,
                    agent_kind: AgentKind::OpenCode,
                    cwd: None,
                    actor_user: None,
                })
                .await
                .unwrap();
        }
        store.writer.end_session(first, None).await.unwrap();

        assert_eq!(
            store
                .reader
                .latest_completed_session_for_project(ws, proj)
                .await
                .unwrap(),
            Some(first)
        );
    }

    #[tokio::test]
    async fn session_end_and_automatic_handoff_commit_atomically() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();
        let other = store
            .writer
            .get_or_create_project(ws, "other", None)
            .await
            .unwrap();
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Codex,
                cwd: None,
                actor_user: Some("user:alice".into()),
            })
            .await
            .unwrap();
        let handoff = |project_id, owner: Option<&str>| NewHandoff {
            workspace_id: ws,
            project_id,
            from_session_id: Some(session_id),
            from_agent: AgentKind::Codex,
            to_agent: None,
            cwd: None,
            summary: "continue".into(),
            open_questions: Vec::new(),
            next_steps: Vec::new(),
            files_touched: Vec::new(),
            owner_user: owner.map(str::to_string),
        };

        assert!(
            store
                .writer
                .end_session_with_handoff(session_id, None, handoff(other, Some("user:alice")),)
                .await
                .is_err(),
            "a mismatched handoff must reject the whole end transition"
        );
        assert_eq!(
            store
                .reader
                .session_end_disposition(session_id, ws, proj, AgentKind::Codex)
                .await
                .unwrap(),
            SessionEndDisposition::Open,
            "failed handoff insertion must leave the session open"
        );

        assert!(
            store
                .writer
                .end_session_with_handoff(session_id, None, handoff(proj, Some("user:bob")),)
                .await
                .is_err(),
            "a mismatched owner must reject the whole end transition"
        );
        assert_eq!(
            store
                .reader
                .session_end_disposition(session_id, ws, proj, AgentKind::Codex)
                .await
                .unwrap(),
            SessionEndDisposition::Open,
            "an owner mismatch must leave the session open"
        );

        store
            .writer
            .end_session_with_handoff(session_id, None, handoff(proj, Some("user:alice")))
            .await
            .unwrap();
        let conn = Connection::open(store.db_path()).unwrap();
        let ended: Option<i64> = conn
            .query_row(
                "SELECT ended_at FROM sessions WHERE id = ?1",
                params![session_id.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        let handoffs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM handoffs WHERE from_session_id = ?1",
                params![session_id.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(ended.is_some());
        assert_eq!(handoffs, 1);
    }

    #[tokio::test]
    async fn session_end_disposition_uses_observation_generation_not_wall_clock() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();
        let session_id = SessionId::new();
        assert_eq!(
            store
                .reader
                .session_end_disposition(session_id, ws, proj, AgentKind::ClaudeCode)
                .await
                .unwrap(),
            SessionEndDisposition::DropInvalid,
            "a missing session must not enter already-ended recovery"
        );
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        let other_proj = store
            .writer
            .get_or_create_project(ws, "other", None)
            .await
            .unwrap();
        assert_eq!(
            store
                .reader
                .session_end_disposition(session_id, ws, other_proj, AgentKind::ClaudeCode)
                .await
                .unwrap(),
            SessionEndDisposition::DropInvalid
        );
        assert_eq!(
            store
                .reader
                .session_end_disposition(session_id, ws, proj, AgentKind::Codex)
                .await
                .unwrap(),
            SessionEndDisposition::DropInvalid
        );
        let first_observation = store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "first".into(),
                    body: "initial work".into(),
                    importance: 8,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();
        store.writer.end_session(session_id, None).await.unwrap();

        let conn = Connection::open(store.db_path()).unwrap();
        let ended_at: i64 = conn
            .query_row(
                "SELECT ended_at FROM sessions WHERE id = ?1",
                params![session_id.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "UPDATE observations SET created_at = ?1 WHERE id = ?2",
            params![ended_at + 1_000_000, first_observation.as_bytes()],
        )
        .unwrap();
        assert_eq!(
            store
                .reader
                .session_end_disposition(session_id, ws, proj, AgentKind::ClaudeCode)
                .await
                .unwrap(),
            SessionEndDisposition::AlreadyEnded,
            "a covered observation must stay covered even if its timestamp is in the future"
        );

        let resumed_observation = store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::PostToolUse,
                    extension: None,
                    source_event: None,
                    title: "resumed".into(),
                    body: "new work".into(),
                    importance: 7,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();
        conn.execute(
            "UPDATE observations SET created_at = 1 WHERE id = ?1",
            params![resumed_observation.as_bytes()],
        )
        .unwrap();
        assert_eq!(
            store
                .reader
                .session_end_disposition(session_id, ws, proj, AgentKind::ClaudeCode)
                .await
                .unwrap(),
            SessionEndDisposition::ReEndWithNewWork,
            "a new observation must trigger re-end even if its timestamp predates ended_at"
        );

        store.writer.end_session(session_id, None).await.unwrap();
        assert_eq!(
            store
                .reader
                .session_end_disposition(session_id, ws, proj, AgentKind::ClaudeCode)
                .await
                .unwrap(),
            SessionEndDisposition::AlreadyEnded,
            "stamping the new generation must make the re-end converge"
        );
    }

    #[tokio::test]
    async fn auto_improve_scheduler_candidates_respect_watermark_age_and_runs() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();
        delete_scheduler_state(&store, ws, proj);

        let historical = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: historical,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::OpenCode,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        store.writer.end_session(historical, None).await.unwrap();

        store
            .writer
            .ensure_auto_improve_scheduler_state(ws, proj)
            .await
            .unwrap();
        // Restart/idempotency: the second call must not reset the watermark.
        store
            .writer
            .ensure_auto_improve_scheduler_state(ws, proj)
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(1)).await;

        let fresh_after_watermark = SessionId::new();
        let open_after_watermark = SessionId::new();
        for id in [fresh_after_watermark, open_after_watermark] {
            store
                .writer
                .begin_session(NewSession {
                    id,
                    workspace_id: ws,
                    project_id: proj,
                    agent_kind: AgentKind::OpenCode,
                    cwd: None,
                    actor_user: None,
                })
                .await
                .unwrap();
        }
        store
            .writer
            .end_session(fresh_after_watermark, None)
            .await
            .unwrap();

        assert!(
            store
                .reader
                .auto_improve_candidate_sessions(ws, proj, 86_400, 10)
                .await
                .unwrap()
                .is_empty(),
            "too-fresh completed sessions must not be candidates"
        );

        let candidates = store
            .reader
            .auto_improve_candidate_sessions(ws, proj, 0, 10)
            .await
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, fresh_after_watermark);

        assert!(
            store
                .writer
                .claim_auto_improve_scheduler_session(
                    ws,
                    proj,
                    candidates[0].session_id,
                    candidates[0].ended_at,
                )
                .await
                .unwrap(),
            "first scheduler claim should be recorded"
        );
        assert!(
            !store
                .writer
                .claim_auto_improve_scheduler_session(
                    ws,
                    proj,
                    candidates[0].session_id,
                    candidates[0].ended_at,
                )
                .await
                .unwrap(),
            "duplicate scheduler claims should be rejected"
        );
        assert!(
            store
                .reader
                .auto_improve_candidate_sessions(ws, proj, 0, 10)
                .await
                .unwrap()
                .is_empty(),
            "claimed sessions must not be retried if review fails before staging"
        );

        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        let reviewed_after_watermark = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: reviewed_after_watermark,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::OpenCode,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        store
            .writer
            .end_session(reviewed_after_watermark, None)
            .await
            .unwrap();

        store
            .writer
            .stage_auto_improve_run(StageAutoImproveRun {
                workspace_id: ws,
                project_id: proj,
                session_id: Some(reviewed_after_watermark),
                provider: Some("none".into()),
                model: Some("none".into()),
                summary: Some("reviewed".into()),
                warnings_json: serde_json::json!([]),
                rejected_candidates_json: serde_json::json!([]),
                config_json: serde_json::json!({ "trigger": "scheduler" }),
                proposal_actor: ActorContext {
                    agent: Some("auto_improve".into()),
                    ..ActorContext::default()
                },
                proposals: Vec::new(),
            })
            .await
            .unwrap();

        assert!(
            store
                .reader
                .auto_improve_candidate_sessions(ws, proj, 0, 10)
                .await
                .unwrap()
                .is_empty(),
            "any recorded run row suppresses scheduler retry for v1"
        );
    }

    #[tokio::test]
    async fn auto_improve_scheduler_state_and_candidates_are_per_project() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let first_project = store
            .writer
            .get_or_create_project(ws, "first", None)
            .await
            .unwrap();
        let second_project = store
            .writer
            .get_or_create_project(ws, "second", None)
            .await
            .unwrap();
        for project_id in [first_project, second_project] {
            delete_scheduler_state(&store, ws, project_id);
        }

        for project_id in [first_project, second_project] {
            let historical = SessionId::new();
            store
                .writer
                .begin_session(NewSession {
                    id: historical,
                    workspace_id: ws,
                    project_id,
                    agent_kind: AgentKind::OpenCode,
                    cwd: None,
                    actor_user: None,
                })
                .await
                .unwrap();
            store.writer.end_session(historical, None).await.unwrap();
        }

        for scope in store.reader.list_all_scopes().await.unwrap() {
            store
                .writer
                .ensure_auto_improve_scheduler_state(scope.workspace_id, scope.project_id)
                .await
                .unwrap();
        }

        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        let mut expected = Vec::new();
        for project_id in [first_project, second_project] {
            let session_id = SessionId::new();
            store
                .writer
                .begin_session(NewSession {
                    id: session_id,
                    workspace_id: ws,
                    project_id,
                    agent_kind: AgentKind::OpenCode,
                    cwd: None,
                    actor_user: None,
                })
                .await
                .unwrap();
            store.writer.end_session(session_id, None).await.unwrap();
            expected.push((project_id, session_id));
        }

        for (project_id, session_id) in expected {
            let candidates = store
                .reader
                .auto_improve_candidate_sessions(ws, project_id, 0, 10)
                .await
                .unwrap();
            assert_eq!(candidates.len(), 1);
            assert_eq!(candidates[0].session_id, session_id);
        }
    }

    #[tokio::test]
    async fn get_or_create_project_initializes_scheduler_state_before_first_session() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "brand-new", None)
            .await
            .unwrap();

        let first_session = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: first_session,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::OpenCode,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        store.writer.end_session(first_session, None).await.unwrap();

        let candidates = store
            .reader
            .auto_improve_candidate_sessions(ws, proj, 0, 10)
            .await
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, first_session);
    }

    #[tokio::test]
    async fn auto_improve_scheduler_claims_do_not_skip_same_ended_at_sessions() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();
        store
            .writer
            .ensure_auto_improve_scheduler_state(ws, proj)
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        let first = SessionId::new();
        let second = SessionId::new();
        for id in [first, second] {
            store
                .writer
                .begin_session(NewSession {
                    id,
                    workspace_id: ws,
                    project_id: proj,
                    agent_kind: AgentKind::OpenCode,
                    cwd: None,
                    actor_user: None,
                })
                .await
                .unwrap();
            store.writer.end_session(id, None).await.unwrap();
        }

        let same_ended_at = jiff::Timestamp::now().as_microsecond();
        let conn = Connection::open(store.db_path()).unwrap();
        conn.execute(
            "UPDATE sessions SET ended_at = ?1 WHERE id IN (?2, ?3)",
            params![same_ended_at, first.as_bytes(), second.as_bytes()],
        )
        .unwrap();

        let candidates = store
            .reader
            .auto_improve_candidate_sessions(ws, proj, 0, 10)
            .await
            .unwrap();
        assert_eq!(candidates.len(), 2);
        assert!(
            store
                .writer
                .claim_auto_improve_scheduler_session(
                    ws,
                    proj,
                    candidates[0].session_id,
                    candidates[0].ended_at,
                )
                .await
                .unwrap()
        );

        let remaining = store
            .reader
            .auto_improve_candidate_sessions(ws, proj, 0, 10)
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_ne!(remaining[0].session_id, candidates[0].session_id);
        assert_eq!(remaining[0].ended_at, same_ended_at);
    }

    #[tokio::test]
    async fn auto_improve_scheduler_claim_is_unique_across_store_instances() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();
        store
            .writer
            .ensure_auto_improve_scheduler_state(ws, proj)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;

        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::OpenCode,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        store.writer.end_session(session_id, None).await.unwrap();
        let ended_at = store
            .reader
            .auto_improve_candidate_sessions(ws, proj, 0, 1)
            .await
            .unwrap()[0]
            .ended_at;

        let second_store = Store::open(tmp.path()).unwrap();
        let (first, second) = tokio::join!(
            store
                .writer
                .claim_auto_improve_scheduler_session(ws, proj, session_id, ended_at),
            second_store
                .writer
                .claim_auto_improve_scheduler_session(ws, proj, session_id, ended_at),
        );
        let claimed = [first.unwrap(), second.unwrap()];
        assert_eq!(claimed.into_iter().filter(|claimed| *claimed).count(), 1);

        let conn = Connection::open(store.db_path()).unwrap();
        let claim_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM auto_improve_scheduler_claims WHERE session_id = ?1",
                params![session_id.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(claim_rows, 1);
    }

    #[tokio::test]
    async fn v103_schema_upgrades_to_current_without_backlog_or_integrity_breaks() {
        let tmp = TempDir::new().unwrap();
        let db_dir = tmp.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join(DB_FILENAME);

        let session_id = SessionId::new();
        {
            let mut conn = Connection::open(&db_path).unwrap();
            conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
            super::migrations::run_to(&mut conn, 19).unwrap();
            conn.pragma_update(None, "foreign_keys", "ON").unwrap();

            let ws = super::ops::get_or_create_workspace(&mut conn, "default").unwrap();
            let proj =
                super::ops::get_or_create_project(&mut conn, &ws, "ai-memory", None).unwrap();
            // Raw-SQL seeding: the v19-era schema predates the V36
            // `expires_at` column that current `upsert_page` writes.
            let page_id = super::ops::tests::insert_page_pre_v36(
                &conn,
                &sample_page(ws, proj, "notes/v103.md", "v1.0.3 upgrade fixture"),
            );
            // Era-appropriate raw insert: this fixture stops at V19 on
            // purpose, while `begin_session` writes whatever columns the
            // CURRENT schema has — including V40's `actor_user`, which a
            // v19-era `sessions` table does not have.
            conn.execute(
                "INSERT INTO sessions \
                 (id, workspace_id, project_id, agent_kind, cwd, started_at) \
                 VALUES (?1, ?2, ?3, 'open-code', NULL, ?4)",
                params![
                    session_id.as_bytes(),
                    ws.as_bytes(),
                    proj.as_bytes(),
                    jiff::Timestamp::now().as_microsecond(),
                ],
            )
            .unwrap();
            super::ops::insert_observation(
                &mut conn,
                &NewObservation {
                    session_id,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "prompt".into(),
                    body: "upgrade observation survives".into(),
                    importance: 5,
                },
            )
            .unwrap();
            conn.execute(
                "UPDATE sessions SET ended_at = 1, summary_page_id = ?1 WHERE id = ?2",
                params![page_id.as_bytes(), session_id.as_bytes()],
            )
            .unwrap();
        }

        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();

        assert_eq!(
            store
                .reader
                .latest_completed_session_for_project(ws, proj)
                .await
                .unwrap(),
            Some(session_id)
        );
        assert_eq!(
            store
                .reader
                .search_observations_for_project(ws, proj, "upgrade".into(), 10)
                .await
                .unwrap()
                .len(),
            1
        );
        let page = store
            .reader
            .page_body_by_ids(ws, proj, "notes/v103.md")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(page.body, "v1.0.3 upgrade fixture");

        store
            .writer
            .ensure_auto_improve_scheduler_state(ws, proj)
            .await
            .unwrap();
        assert!(
            store
                .reader
                .auto_improve_candidate_sessions(ws, proj, 0, 10)
                .await
                .unwrap()
                .is_empty(),
            "v1.0.3-era completed sessions must become the first-run watermark, not backlog"
        );

        let conn = Connection::open(store.db_path()).unwrap();
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(fk_violations, 0);
        for table in [
            "auto_improve_runs",
            "auto_improve_proposals",
            "auto_improve_proposal_events",
            "auto_improve_scheduler_state",
            "auto_improve_scheduler_claims",
        ] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    params![table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "{table} should exist after v1.0.3 upgrade");
        }
    }

    #[tokio::test]
    async fn list_projects_with_stats_returns_aggregates() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "my-project", None)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "a.md", "alpha"))
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "b.md", "beta"))
            .await
            .unwrap();

        let summaries = store.reader.list_projects_with_stats().await.unwrap();
        assert_eq!(summaries.len(), 1);
        let s = &summaries[0];
        assert_eq!(s.workspace_name, "default");
        assert_eq!(s.project_name, "my-project");
        assert_eq!(s.page_count, 2);
        assert!(s.last_updated.is_some());
    }

    #[tokio::test]
    async fn list_pages_returns_latest_pages_for_project() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "x.md", "body x"))
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "y.md", "body y"))
            .await
            .unwrap();
        // Supersede x.md — should still appear once (latest version).
        store
            .writer
            .upsert_page(sample_page(ws, proj, "x.md", "body x updated"))
            .await
            .unwrap();

        let pages = store.reader.list_pages("default", "scratch").await.unwrap();
        assert_eq!(pages.len(), 2, "only is_latest=1 pages");
        let paths: Vec<&str> = pages.iter().map(|p| p.path.as_str()).collect();
        assert!(paths.contains(&"x.md"));
        assert!(paths.contains(&"y.md"));
    }

    #[tokio::test]
    async fn reader_page_kinds_follow_canonical_paths_across_surfaces() {
        const CASES: [(&str, &str); 9] = [
            ("_rules/rule.md", "rule"),
            ("_slots/focus.md", "slot"),
            ("sessions/session.md", "session"),
            ("decisions/decision.md", "decision"),
            ("gotchas/gotcha.md", "gotcha"),
            ("concepts/concept.md", "concept"),
            ("procedures/procedure.md", "procedure"),
            ("notes/note.md", "note"),
            ("misc/fallback.md", "fact"),
        ];

        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "kind-test", None)
            .await
            .unwrap();

        let mut session_id = None;
        for (path, _) in CASES {
            let id = store
                .writer
                .upsert_page(sample_page(ws, proj, path, "body"))
                .await
                .unwrap();
            if path == "sessions/session.md" {
                session_id = Some(id);
            }
        }

        let mut override_page = sample_page(ws, proj, "sessions/override.md", "override");
        override_page.frontmatter_json = serde_json::json!({"kind": "note"});
        store.writer.upsert_page(override_page).await.unwrap();

        let mut source = sample_page(ws, proj, "notes/source.md", "linked source");
        source.links = vec![PagePath::new("sessions/session.md").unwrap().into()];
        store.writer.upsert_page(source).await.unwrap();

        let assert_briefing_kinds = |pages: &[BriefingPage]| {
            for (path, expected) in CASES {
                let page = pages.iter().find(|page| page.path == path).unwrap();
                assert_eq!(page.kind, expected, "wrong briefing kind for {path}");
            }
            let override_page = pages
                .iter()
                .find(|page| page.path == "sessions/override.md")
                .unwrap();
            assert_eq!(override_page.kind, "note", "explicit kind must win");
        };

        let listed = store
            .reader
            .list_pages("default", "kind-test")
            .await
            .unwrap();
        for (path, expected) in CASES {
            let page = listed.iter().find(|page| page.path == path).unwrap();
            assert_eq!(page.kind, expected, "wrong list kind for {path}");
        }
        assert_eq!(
            listed
                .iter()
                .find(|page| page.path == "sessions/override.md")
                .unwrap()
                .kind,
            "note",
            "explicit kind must win"
        );

        assert_briefing_kinds(
            &store
                .reader
                .briefing(100, ai_memory_core::OwnerFilter::Any)
                .await
                .unwrap()
                .recent_pages,
        );
        assert_briefing_kinds(
            &store
                .reader
                .briefing_for_workspace(ws, 100, ai_memory_core::OwnerFilter::Any)
                .await
                .unwrap()
                .recent_pages,
        );
        let project_briefing = store
            .reader
            .briefing_for_project(ws, proj, 100, ai_memory_core::OwnerFilter::Any)
            .await
            .unwrap();
        assert_briefing_kinds(&project_briefing.recent_pages);
        assert_eq!(project_briefing.rules[0].kind, "rule");
        assert_eq!(project_briefing.slots[0].kind, "slot");
        let (_, session_recent) = store
            .reader
            .session_brief_pages(ws, proj, 100, 100)
            .await
            .unwrap();
        assert_briefing_kinds(&session_recent);

        let session_id = session_id.unwrap();
        assert_eq!(
            store
                .reader
                .page_meta("default", "kind-test", "sessions/session.md")
                .await
                .unwrap()
                .unwrap()
                .kind,
            "session"
        );
        assert_eq!(
            store
                .reader
                .page_meta_by_id(session_id)
                .await
                .unwrap()
                .unwrap()
                .kind,
            "session"
        );
        assert_eq!(
            store
                .reader
                .page_meta_by_path("concepts/concept.md")
                .await
                .unwrap()
                .unwrap()
                .kind,
            "concept"
        );

        let source_links = store
            .reader
            .page_links(ws, proj, "notes/source.md".into())
            .await
            .unwrap();
        assert_eq!(source_links.links[0].kind, "session");
        let target_links = store
            .reader
            .page_links(ws, proj, "sessions/session.md".into())
            .await
            .unwrap();
        assert_eq!(target_links.backlinks[0].kind, "note");

        let health = store
            .reader
            .health_detail_for_project(ws, proj, 100)
            .await
            .unwrap();
        assert_eq!(
            health
                .orphans
                .iter()
                .find(|page| page.path == "procedures/procedure.md")
                .unwrap()
                .kind,
            "procedure"
        );
    }

    #[tokio::test]
    async fn page_meta_returns_metadata_for_existing_page() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "meta-test", None)
            .await
            .unwrap();
        let page = NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("decisions/d1.md").unwrap(),
            title: "Decision One".into(),
            body: "content here".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({"kind": "decision"}),
            pinned: true,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
        };
        store.writer.upsert_page(page).await.unwrap();

        let meta = store
            .reader
            .page_meta("default", "meta-test", "decisions/d1.md")
            .await
            .unwrap();
        let meta = meta.expect("page_meta should return Some for existing page");
        assert_eq!(meta.workspace_name, "default");
        assert_eq!(meta.project_name, "meta-test");
        assert_eq!(meta.path, "decisions/d1.md");
        assert_eq!(meta.title, "Decision One");
        assert_eq!(meta.kind, "decision");
        assert!(meta.pinned);

        // Non-existent page returns None.
        let none = store
            .reader
            .page_meta("default", "meta-test", "no-such.md")
            .await
            .unwrap();
        assert!(none.is_none());
    }

    #[tokio::test]
    async fn delete_stale_page_embeddings_removes_mismatched_rows() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "test", None)
            .await
            .unwrap();
        let p1 = store
            .writer
            .upsert_page(sample_page(ws, proj, "a.md", "body a"))
            .await
            .unwrap();
        let p2 = store
            .writer
            .upsert_page(sample_page(ws, proj, "b.md", "body b"))
            .await
            .unwrap();
        store
            .writer
            .store_embedding(
                p1,
                vec![0u8; 4],
                "google".into(),
                "models/gemini-embedding-001".into(),
                768,
            )
            .await
            .unwrap();
        store
            .writer
            .store_embedding(
                p2,
                vec![1u8; 4],
                "openai".into(),
                "openai/text-embedding-3-small".into(),
                1536,
            )
            .await
            .unwrap();
        let n = store
            .writer
            .delete_stale_page_embeddings(
                ws,
                Some(proj),
                "openai".into(),
                "openai/text-embedding-3-small".into(),
                1536,
            )
            .await
            .unwrap();
        assert_eq!(n, 1);
        let mismatch = store
            .reader
            .embedding_meta_for_mismatch(
                "openai".into(),
                "openai/text-embedding-3-small".into(),
                1536,
            )
            .await
            .unwrap();
        assert!(mismatch.is_empty());
    }

    #[tokio::test]
    async fn managed_workstream_batches_are_idempotent_and_release_the_lease() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let project = store
            .writer
            .get_or_create_project(ws, "managed", None)
            .await
            .unwrap();
        let prepare = PrepareWorkstreamRun {
            workspace_id: ws,
            project_id: project,
            repo_fingerprint: "repo".into(),
            worktree_fingerprint: "worktree".into(),
            cwd: "/repo".into(),
            agent: AgentKind::Codex,
            automatic_harness: false,
            available_agents: Vec::new(),
            selection: WorkstreamSelection::Current,
            lease_owner: "test:1".into(),
        };
        let run = store
            .writer
            .prepare_workstream_run(prepare.clone())
            .await
            .unwrap();
        assert!(run.may_adopt_existing_session);
        let busy = store
            .writer
            .prepare_workstream_run(prepare.clone())
            .await
            .unwrap_err();
        assert!(matches!(busy, StoreError::WorkstreamBusy(_)));
        assert!(
            store
                .writer
                .link_managed_run_session(run.run_id, AgentKind::Codex, "native-1")
                .await
                .unwrap()
        );

        let event = NewWorkstreamEvent {
            event_id: "event-1".into(),
            agent: AgentKind::Codex,
            native_session_id: "native-1".into(),
            source_record_id: Some("record-1".into()),
            kind: WorkstreamEventKind::Message,
            role: Some("assistant".into()),
            content: "portable context".into(),
            occurred_at: None,
            metadata: serde_json::json!({}),
        };
        let partial = FinishWorkstreamRun {
            run_id: run.run_id,
            native_session_id: Some("native-1".into()),
            source_cursor: None,
            events: vec![event.clone()],
            complete: false,
            segment_path: Some("segment-1.jsonl".into()),
            exit_code: None,
        };
        assert_eq!(
            store
                .writer
                .finish_workstream_run(partial.clone())
                .await
                .unwrap()
                .imported_events,
            1
        );
        assert_eq!(
            store
                .writer
                .finish_workstream_run(partial)
                .await
                .unwrap()
                .imported_events,
            0
        );
        assert_eq!(
            store
                .reader
                .managed_run_status(run.run_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "active"
        );

        let finished = store
            .writer
            .finish_workstream_run(FinishWorkstreamRun {
                run_id: run.run_id,
                native_session_id: Some("native-1".into()),
                source_cursor: Some("cursor-1".into()),
                events: vec![event],
                complete: true,
                segment_path: Some("segment-final.jsonl".into()),
                exit_code: Some(0),
            })
            .await
            .unwrap();
        assert_eq!(finished.imported_events, 0);
        let search = store
            .reader
            .search_workstream_events(run.workstream_id, "portable".into(), 10)
            .await
            .unwrap();
        assert_eq!(search.len(), 1);
        assert_eq!(search[0].event_id, "event-1");
        let next = store
            .writer
            .prepare_workstream_run(prepare.clone())
            .await
            .unwrap();
        assert!(!next.may_adopt_existing_session);
        assert_eq!(next.workstream_id, run.workstream_id);
        assert_eq!(next.native_session_id.as_deref(), Some("native-1"));
        assert_eq!(next.source_cursor.as_deref(), Some("cursor-1"));
        assert_eq!(next.sync_through, 1);
        assert!(
            store
                .writer
                .link_managed_run_session(next.run_id, AgentKind::Codex, "native-2")
                .await
                .unwrap()
        );
        let fresh_context = store
            .reader
            .managed_run_context(next.run_id, 256)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fresh_context.sync_after, 0);
        assert_eq!(fresh_context.events.len(), 1);
        assert!(
            store
                .writer
                .accept_managed_run_context(next.run_id)
                .await
                .unwrap()
        );
        assert!(
            !store
                .writer
                .link_managed_run_session(next.run_id, AgentKind::Codex, "native-3")
                .await
                .unwrap(),
            "a delivered run must not be rebound by a nested same-agent process"
        );
        store
            .writer
            .finish_workstream_run(FinishWorkstreamRun {
                run_id: next.run_id,
                native_session_id: Some("native-2".into()),
                source_cursor: Some("cursor-2".into()),
                events: Vec::new(),
                complete: true,
                segment_path: Some("segment-native-2.jsonl".into()),
                exit_code: Some(0),
            })
            .await
            .unwrap();
        let retry = store
            .writer
            .prepare_workstream_run(prepare.clone())
            .await
            .unwrap();
        assert_eq!(retry.native_session_id.as_deref(), Some("native-2"));
        assert_eq!(
            retry.sync_after, 1,
            "delivered context must stay acknowledged"
        );
        assert_eq!(retry.sync_through, 1);
        assert!(
            store
                .writer
                .link_managed_run_session(retry.run_id, AgentKind::Codex, "native-3")
                .await
                .unwrap()
        );
        store
            .writer
            .finish_workstream_run(FinishWorkstreamRun {
                run_id: retry.run_id,
                native_session_id: Some("native-3".into()),
                source_cursor: Some("cursor-3".into()),
                events: Vec::new(),
                complete: true,
                segment_path: Some("segment-native-3.jsonl".into()),
                exit_code: Some(0),
            })
            .await
            .unwrap();
        let undelivered = store.writer.prepare_workstream_run(prepare).await.unwrap();
        assert_eq!(undelivered.native_session_id.as_deref(), Some("native-3"));
        assert_eq!(
            undelivered.sync_after, 0,
            "undelivered context must be retried"
        );
        assert_eq!(undelivered.sync_through, 1);
    }

    #[tokio::test]
    async fn managed_run_cancel_releases_the_lease_immediately() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let workspace = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let project = store
            .writer
            .get_or_create_project(workspace, "managed-cancel", None)
            .await
            .unwrap();
        let prepare = PrepareWorkstreamRun {
            workspace_id: workspace,
            project_id: project,
            repo_fingerprint: "repo".into(),
            worktree_fingerprint: "worktree".into(),
            cwd: "/repo".into(),
            agent: AgentKind::Codex,
            automatic_harness: false,
            available_agents: Vec::new(),
            selection: WorkstreamSelection::Current,
            lease_owner: "test:1".into(),
        };
        let first = store
            .writer
            .prepare_workstream_run(prepare.clone())
            .await
            .unwrap();
        assert!(
            store
                .writer
                .prepare_workstream_run(prepare.clone())
                .await
                .is_err()
        );
        assert!(store.writer.cancel_managed_run(first.run_id).await.unwrap());
        assert!(
            !store.writer.cancel_managed_run(first.run_id).await.unwrap(),
            "cancel is idempotent once the run is no longer active"
        );
        assert_eq!(
            store
                .reader
                .managed_run_status(first.run_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "expired"
        );
        store.writer.prepare_workstream_run(prepare).await.unwrap();
    }

    #[tokio::test]
    async fn managed_adoption_stops_after_any_harness_links_the_workstream() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let workspace = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let project = store
            .writer
            .get_or_create_project(workspace, "managed", None)
            .await
            .unwrap();
        let prepare = |agent, owner: &str| PrepareWorkstreamRun {
            workspace_id: workspace,
            project_id: project,
            repo_fingerprint: "repo".into(),
            worktree_fingerprint: "worktree".into(),
            cwd: "/repo".into(),
            agent,
            automatic_harness: false,
            available_agents: Vec::new(),
            selection: WorkstreamSelection::Current,
            lease_owner: owner.into(),
        };

        let blank = store
            .writer
            .prepare_workstream_run(prepare(AgentKind::Codex, "blank"))
            .await
            .unwrap();
        assert!(blank.may_adopt_existing_session);
        store
            .writer
            .finish_workstream_run(FinishWorkstreamRun {
                run_id: blank.run_id,
                native_session_id: None,
                source_cursor: None,
                events: Vec::new(),
                complete: true,
                segment_path: None,
                exit_code: Some(0),
            })
            .await
            .unwrap();

        let first = store
            .writer
            .prepare_workstream_run(prepare(AgentKind::ClaudeCode, "claude"))
            .await
            .unwrap();
        assert!(
            first.may_adopt_existing_session,
            "a blank run with no native session or portable history remains adoptable"
        );
        assert!(
            store
                .writer
                .link_managed_run_session(first.run_id, AgentKind::ClaudeCode, "claude-native")
                .await
                .unwrap()
        );
        store
            .writer
            .finish_workstream_run(FinishWorkstreamRun {
                run_id: first.run_id,
                native_session_id: Some("claude-native".into()),
                source_cursor: None,
                events: Vec::new(),
                complete: true,
                segment_path: None,
                exit_code: Some(0),
            })
            .await
            .unwrap();

        let codex = store
            .writer
            .prepare_workstream_run(prepare(AgentKind::Codex, "codex"))
            .await
            .unwrap();
        assert!(codex.native_session_id.is_none());
        assert!(
            !codex.may_adopt_existing_session,
            "an established Claude workstream must start a fresh Codex session"
        );
    }

    #[tokio::test]
    async fn automatic_run_prefers_newest_linked_available_harness() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let workspace = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let project = store
            .writer
            .get_or_create_project(workspace, "managed", None)
            .await
            .unwrap();
        let base = |agent, owner: &str| PrepareWorkstreamRun {
            workspace_id: workspace,
            project_id: project,
            repo_fingerprint: "repo".into(),
            worktree_fingerprint: "worktree".into(),
            cwd: "/repo".into(),
            agent,
            automatic_harness: false,
            available_agents: Vec::new(),
            selection: WorkstreamSelection::Current,
            lease_owner: owner.into(),
        };
        let claude = store
            .writer
            .prepare_workstream_run(base(AgentKind::ClaudeCode, "claude"))
            .await
            .unwrap();
        store
            .writer
            .finish_workstream_run(FinishWorkstreamRun {
                run_id: claude.run_id,
                native_session_id: Some("claude-current".into()),
                source_cursor: Some("cursor".into()),
                events: Vec::new(),
                complete: true,
                segment_path: None,
                exit_code: Some(0),
            })
            .await
            .unwrap();

        let mut automatic = base(AgentKind::Codex, "automatic");
        automatic.automatic_harness = true;
        automatic.available_agents = vec![AgentKind::Codex, AgentKind::ClaudeCode];
        let resumed = store
            .writer
            .prepare_workstream_run(automatic)
            .await
            .unwrap();
        assert_eq!(resumed.agent, AgentKind::ClaudeCode);
        assert_eq!(resumed.native_session_id.as_deref(), Some("claude-current"));
        assert_eq!(resumed.source_cursor.as_deref(), Some("cursor"));
        assert!(!resumed.may_adopt_existing_session);
    }

    #[tokio::test]
    async fn managed_workstreams_are_isolated_across_workspaces() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let mut runs = Vec::new();

        for (workspace_name, content) in [
            ("workspace-a", "only visible in workspace a"),
            ("workspace-b", "only visible in workspace b"),
        ] {
            let workspace = store
                .writer
                .get_or_create_workspace(workspace_name)
                .await
                .unwrap();
            let project = store
                .writer
                .get_or_create_project(workspace, "shared-name", None)
                .await
                .unwrap();
            let run = store
                .writer
                .prepare_workstream_run(PrepareWorkstreamRun {
                    workspace_id: workspace,
                    project_id: project,
                    repo_fingerprint: "same-repository".into(),
                    worktree_fingerprint: "same-worktree".into(),
                    cwd: "/same/path".into(),
                    agent: AgentKind::Codex,
                    automatic_harness: false,
                    available_agents: Vec::new(),
                    selection: WorkstreamSelection::Current,
                    lease_owner: workspace_name.into(),
                })
                .await
                .unwrap();
            store
                .writer
                .finish_workstream_run(FinishWorkstreamRun {
                    run_id: run.run_id,
                    native_session_id: Some(format!("native-{workspace_name}")),
                    source_cursor: Some("cursor".into()),
                    events: vec![NewWorkstreamEvent {
                        event_id: "same-native-event-id".into(),
                        agent: AgentKind::Codex,
                        native_session_id: format!("native-{workspace_name}"),
                        source_record_id: None,
                        kind: WorkstreamEventKind::Message,
                        role: Some("assistant".into()),
                        content: content.into(),
                        occurred_at: None,
                        metadata: serde_json::json!({}),
                    }],
                    complete: true,
                    segment_path: None,
                    exit_code: Some(0),
                })
                .await
                .unwrap();
            runs.push(run);
        }

        assert_ne!(runs[0].workstream_id, runs[1].workstream_id);
        let first = store
            .reader
            .search_workstream_events(runs[0].workstream_id, "visible".into(), 10)
            .await
            .unwrap();
        let second = store
            .reader
            .search_workstream_events(runs[1].workstream_id, "visible".into(), 10)
            .await
            .unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].content, "only visible in workspace a");
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].content, "only visible in workspace b");
    }

    fn managed_prepare_input(
        ws: WorkspaceId,
        proj: ProjectId,
        owner: &str,
    ) -> PrepareWorkstreamRun {
        PrepareWorkstreamRun {
            workspace_id: ws,
            project_id: proj,
            repo_fingerprint: "repo".into(),
            worktree_fingerprint: "worktree".into(),
            cwd: "/repo".into(),
            agent: AgentKind::Codex,
            automatic_harness: false,
            available_agents: Vec::new(),
            selection: WorkstreamSelection::Current,
            lease_owner: owner.into(),
        }
    }

    async fn open_managed_scope(store: &Store, project: &str) -> (WorkspaceId, ProjectId) {
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, project, None)
            .await
            .unwrap();
        (ws, proj)
    }

    fn managed_event(
        event_id: &str,
        agent: AgentKind,
        native: &str,
        content: &str,
    ) -> NewWorkstreamEvent {
        NewWorkstreamEvent {
            event_id: event_id.into(),
            agent,
            native_session_id: native.into(),
            source_record_id: None,
            kind: WorkstreamEventKind::Message,
            role: Some("assistant".into()),
            content: content.into(),
            occurred_at: None,
            metadata: serde_json::json!({}),
        }
    }

    fn complete_finish(
        run_id: ManagedRunId,
        events: Vec<NewWorkstreamEvent>,
    ) -> FinishWorkstreamRun {
        FinishWorkstreamRun {
            run_id,
            native_session_id: None,
            source_cursor: None,
            events,
            complete: true,
            segment_path: None,
            exit_code: Some(0),
        }
    }

    fn set_managed_run_lease(db_path: &std::path::Path, run_id: ManagedRunId, lease: i64) {
        let conn = Connection::open(db_path).unwrap();
        conn.execute(
            "UPDATE managed_runs SET lease_expires_at = ?1 WHERE id = ?2",
            params![lease, run_id.as_bytes()],
        )
        .unwrap();
    }

    fn managed_run_lease(db_path: &std::path::Path, run_id: ManagedRunId) -> i64 {
        let conn = Connection::open(db_path).unwrap();
        conn.query_row(
            "SELECT lease_expires_at FROM managed_runs WHERE id = ?1",
            params![run_id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn managed_run_heartbeat_extends_only_an_active_run() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, proj) = open_managed_scope(&store, "managed-heartbeat").await;
        let run = store
            .writer
            .prepare_workstream_run(managed_prepare_input(ws, proj, "test:1"))
            .await
            .unwrap();

        // Pull the lease close to expiry; a heartbeat must push it back out.
        let initial = managed_run_lease(store.db_path(), run.run_id);
        set_managed_run_lease(store.db_path(), run.run_id, initial - 5_000_000);
        assert!(
            store
                .writer
                .heartbeat_managed_run(run.run_id)
                .await
                .unwrap()
        );
        assert!(
            managed_run_lease(store.db_path(), run.run_id) >= initial,
            "heartbeat must extend the lease"
        );

        // A still-active launcher can recover after the server was unavailable
        // for longer than one lease window.
        set_managed_run_lease(store.db_path(), run.run_id, 1);
        assert!(
            store
                .writer
                .heartbeat_managed_run(run.run_id)
                .await
                .unwrap()
        );

        // Unknown runs never heartbeat.
        assert!(
            !store
                .writer
                .heartbeat_managed_run(ManagedRunId::new())
                .await
                .unwrap()
        );

        // Neither do finished runs.
        store
            .writer
            .finish_workstream_run(complete_finish(run.run_id, Vec::new()))
            .await
            .unwrap();
        assert!(
            !store
                .writer
                .heartbeat_managed_run(run.run_id)
                .await
                .unwrap()
        );

        // Once another prepare transaction has claimed the workstream, the
        // superseded run is terminal and cannot displace the replacement.
        let superseded = store
            .writer
            .prepare_workstream_run(managed_prepare_input(ws, proj, "test:superseded"))
            .await
            .unwrap();
        set_managed_run_lease(store.db_path(), superseded.run_id, 1);
        let replacement = store
            .writer
            .prepare_workstream_run(managed_prepare_input(ws, proj, "test:replacement"))
            .await
            .unwrap();
        assert!(
            !store
                .writer
                .heartbeat_managed_run(superseded.run_id)
                .await
                .unwrap()
        );
        assert!(
            store
                .writer
                .cancel_managed_run(replacement.run_id)
                .await
                .unwrap()
        );

        // Nor cancelled runs.
        let cancelled = store
            .writer
            .prepare_workstream_run(managed_prepare_input(ws, proj, "test:2"))
            .await
            .unwrap();
        assert!(
            store
                .writer
                .cancel_managed_run(cancelled.run_id)
                .await
                .unwrap()
        );
        assert!(
            !store
                .writer
                .heartbeat_managed_run(cancelled.run_id)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn finish_run_after_complete_is_an_idempotent_no_op() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, proj) = open_managed_scope(&store, "managed-idempotent-finish").await;
        let run = store
            .writer
            .prepare_workstream_run(managed_prepare_input(ws, proj, "test:1"))
            .await
            .unwrap();
        assert!(
            store
                .writer
                .link_managed_run_session(run.run_id, AgentKind::Codex, "native-1")
                .await
                .unwrap()
        );

        let finish = FinishWorkstreamRun {
            run_id: run.run_id,
            native_session_id: Some("native-1".into()),
            source_cursor: Some("cursor-1".into()),
            events: vec![
                managed_event("ev-1", AgentKind::Codex, "native-1", "first"),
                managed_event("ev-2", AgentKind::Codex, "native-1", "second"),
            ],
            complete: true,
            segment_path: Some("segment.jsonl".into()),
            exit_code: Some(0),
        };
        let first = store
            .writer
            .finish_workstream_run(finish.clone())
            .await
            .unwrap();
        assert_eq!(first.imported_events, 2);
        assert_eq!(first.latest_sequence, 2);

        // Re-finishing a completed run imports nothing, even with new input.
        let second = store.writer.finish_workstream_run(finish).await.unwrap();
        assert_eq!(second.imported_events, 0);
        assert_eq!(second.latest_sequence, 2);
        assert_eq!(
            store
                .reader
                .managed_run_status(run.run_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "finished"
        );
        let events = store
            .reader
            .search_workstream_events(run.workstream_id, String::new(), 10)
            .await
            .unwrap();
        assert_eq!(
            events.len(),
            2,
            "the no-op finish must not duplicate events"
        );
    }

    #[tokio::test]
    async fn finish_run_rejects_cancelled_and_unknown_runs() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, proj) = open_managed_scope(&store, "managed-finish-closed").await;
        let run = store
            .writer
            .prepare_workstream_run(managed_prepare_input(ws, proj, "test:1"))
            .await
            .unwrap();
        assert!(store.writer.cancel_managed_run(run.run_id).await.unwrap());

        let cancelled = store
            .writer
            .finish_workstream_run(complete_finish(run.run_id, Vec::new()))
            .await
            .unwrap_err();
        assert!(
            matches!(cancelled, StoreError::InvalidState(ref msg) if msg.contains("expired")),
            "cancelled runs report their state: {cancelled}"
        );

        let unknown = store
            .writer
            .finish_workstream_run(complete_finish(ManagedRunId::new(), Vec::new()))
            .await
            .unwrap_err();
        assert!(matches!(unknown, StoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn finish_run_rejects_foreign_events_and_rolls_back_the_batch() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, proj) = open_managed_scope(&store, "managed-finish-foreign").await;
        let run = store
            .writer
            .prepare_workstream_run(managed_prepare_input(ws, proj, "test:1"))
            .await
            .unwrap();
        assert!(
            store
                .writer
                .link_managed_run_session(run.run_id, AgentKind::Codex, "native-1")
                .await
                .unwrap()
        );

        // A valid event followed by another agent's event must import nothing.
        let wrong_agent = store
            .writer
            .finish_workstream_run(FinishWorkstreamRun {
                run_id: run.run_id,
                native_session_id: Some("native-1".into()),
                source_cursor: None,
                events: vec![
                    managed_event("ev-1", AgentKind::Codex, "native-1", "valid"),
                    managed_event("ev-2", AgentKind::ClaudeCode, "native-1", "foreign agent"),
                ],
                complete: false,
                segment_path: None,
                exit_code: None,
            })
            .await
            .unwrap_err();
        assert!(
            matches!(wrong_agent, StoreError::InvalidState(ref msg) if msg.contains("ev-2")),
            "{wrong_agent}"
        );
        assert!(
            store
                .reader
                .search_workstream_events(run.workstream_id, String::new(), 10)
                .await
                .unwrap()
                .is_empty(),
            "a rejected batch must roll back its earlier inserts"
        );
        assert_eq!(
            store
                .reader
                .managed_run_status(run.run_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "active"
        );

        // Events from a different native session are rejected the same way.
        let wrong_session = store
            .writer
            .finish_workstream_run(FinishWorkstreamRun {
                run_id: run.run_id,
                native_session_id: Some("native-1".into()),
                source_cursor: None,
                events: vec![managed_event(
                    "ev-3",
                    AgentKind::Codex,
                    "native-2",
                    "foreign session",
                )],
                complete: false,
                segment_path: None,
                exit_code: None,
            })
            .await
            .unwrap_err();
        assert!(
            matches!(wrong_session, StoreError::InvalidState(ref msg) if msg.contains("native session")),
            "{wrong_session}"
        );

        // The run is still usable after the rejected batches.
        let finished = store
            .writer
            .finish_workstream_run(FinishWorkstreamRun {
                run_id: run.run_id,
                native_session_id: Some("native-1".into()),
                source_cursor: None,
                events: vec![managed_event("ev-1", AgentKind::Codex, "native-1", "valid")],
                complete: true,
                segment_path: None,
                exit_code: Some(0),
            })
            .await
            .unwrap();
        assert_eq!(finished.imported_events, 1);
        assert_eq!(finished.latest_sequence, 1);
    }

    #[tokio::test]
    async fn accept_context_marks_delivery_only_for_active_runs() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, proj) = open_managed_scope(&store, "managed-accept").await;

        assert!(
            !store
                .writer
                .accept_managed_run_context(ManagedRunId::new())
                .await
                .unwrap(),
            "unknown runs cannot accept context"
        );

        let run = store
            .writer
            .prepare_workstream_run(managed_prepare_input(ws, proj, "test:1"))
            .await
            .unwrap();
        assert!(
            store
                .writer
                .accept_managed_run_context(run.run_id)
                .await
                .unwrap()
        );
        let status = store
            .reader
            .managed_run_status(run.run_id)
            .await
            .unwrap()
            .unwrap();
        assert!(status.context_delivered);
        assert!(
            store
                .writer
                .accept_managed_run_context(run.run_id)
                .await
                .unwrap(),
            "re-acknowledging a live run stays idempotent"
        );

        store
            .writer
            .finish_workstream_run(complete_finish(run.run_id, Vec::new()))
            .await
            .unwrap();
        assert!(
            !store
                .writer
                .accept_managed_run_context(run.run_id)
                .await
                .unwrap(),
            "finished runs cannot accept context"
        );

        let cancelled = store
            .writer
            .prepare_workstream_run(managed_prepare_input(ws, proj, "test:2"))
            .await
            .unwrap();
        assert!(
            store
                .writer
                .cancel_managed_run(cancelled.run_id)
                .await
                .unwrap()
        );
        assert!(
            !store
                .writer
                .accept_managed_run_context(cancelled.run_id)
                .await
                .unwrap(),
            "cancelled runs cannot accept context"
        );
    }

    #[tokio::test]
    async fn startup_context_claim_is_atomic_and_single_use() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, proj) = open_managed_scope(&store, "startup-claim").await;
        let run = store
            .writer
            .prepare_workstream_run(managed_prepare_input(ws, proj, "test:1"))
            .await
            .unwrap();
        let insert_handoff = || NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "continue".into(),
            open_questions: Vec::new(),
            next_steps: Vec::new(),
            files_touched: Vec::new(),
            owner_user: None,
        };
        let acceptance = |handoff_id, receiving_cwd| HandoffAcceptance {
            handoff_id,
            workspace_id: ws,
            project_id: proj,
            accepting_agent: AgentKind::Codex,
            accepting_session: None,
            accepting_user: None,
            owner_filter: ai_memory_core::OwnerFilter::Any,
            receiving_cwd,
        };

        let first_handoff = store.writer.insert_handoff(insert_handoff()).await.unwrap();
        let accepted = store
            .writer
            .accept_startup_context(
                Some(acceptance(first_handoff, None)),
                Some(run.run_id),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            accepted,
            StartupContextAcceptance {
                handoff_accepted: true,
                managed_context_accepted: true,
            }
        );
        assert!(
            store
                .reader
                .latest_open_handoff(ws, proj, None, ai_memory_core::OwnerFilter::Any)
                .await
                .unwrap()
                .is_none()
        );

        let second_handoff = store.writer.insert_handoff(insert_handoff()).await.unwrap();
        let rejected = store
            .writer
            .accept_startup_context(
                Some(acceptance(second_handoff, None)),
                Some(run.run_id),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            rejected,
            StartupContextAcceptance::default(),
            "an already-delivered managed packet must reject the whole claim"
        );
        assert_eq!(
            store
                .reader
                .latest_open_handoff(ws, proj, None, ai_memory_core::OwnerFilter::Any)
                .await
                .unwrap()
                .map(|handoff| handoff.scope.id),
            Some(second_handoff),
            "a failed managed claim must roll back the handoff transition"
        );

        async fn insert_auto_handoff(
            store: &Store,
            workspace_id: WorkspaceId,
            project_id: ProjectId,
            cwd: &str,
        ) -> HandoffId {
            let session_id = SessionId::new();
            store
                .writer
                .begin_session(NewSession {
                    id: session_id,
                    workspace_id,
                    project_id,
                    agent_kind: AgentKind::ClaudeCode,
                    cwd: Some(cwd.into()),
                    actor_user: None,
                })
                .await
                .unwrap();
            store
                .writer
                .insert_handoff(NewHandoff {
                    workspace_id,
                    project_id,
                    from_session_id: Some(session_id),
                    from_agent: AgentKind::ClaudeCode,
                    to_agent: None,
                    cwd: Some(cwd.into()),
                    summary: cwd.into(),
                    open_questions: Vec::new(),
                    next_steps: Vec::new(),
                    files_touched: Vec::new(),
                    owner_user: None,
                })
                .await
                .unwrap()
        }

        let stale_auto = insert_auto_handoff(&store, ws, proj, "/repo/api").await;
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let selected_auto = insert_auto_handoff(&store, ws, proj, "/repo").await;
        let rejected_auto = store
            .writer
            .accept_startup_context(
                Some(acceptance(selected_auto, Some("/repo/api/src".into()))),
                Some(run.run_id),
                None,
            )
            .await
            .unwrap();
        assert_eq!(rejected_auto, StartupContextAcceptance::default());
        for handoff_id in [stale_auto, selected_auto] {
            assert_eq!(
                store
                    .reader
                    .handoff_by_id(handoff_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .lifecycle
                    .state,
                HandoffState::Open,
                "a rejected managed claim must not accept or expire automatic handoffs"
            );
        }
    }

    #[tokio::test]
    async fn run_context_delivers_newest_events_in_sequence_order() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, proj) = open_managed_scope(&store, "managed-context").await;
        let first = store
            .writer
            .prepare_workstream_run(managed_prepare_input(ws, proj, "test:1"))
            .await
            .unwrap();
        // Finish without a native session so the next run starts with an
        // undelivered context window spanning the whole ledger.
        store
            .writer
            .finish_workstream_run(complete_finish(
                first.run_id,
                vec![
                    managed_event("ev-1", AgentKind::Codex, "native-x", "one"),
                    managed_event("ev-2", AgentKind::Codex, "native-x", "two"),
                    managed_event("ev-3", AgentKind::Codex, "native-x", "three"),
                ],
            ))
            .await
            .unwrap();

        let second = store
            .writer
            .prepare_workstream_run(managed_prepare_input(ws, proj, "test:2"))
            .await
            .unwrap();
        assert_eq!(second.workstream_id, first.workstream_id);
        assert_eq!(second.sync_after, 0);
        assert_eq!(second.sync_through, 3);

        let context = store
            .reader
            .managed_run_context(second.run_id, 256)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(context.workstream_name, "default");
        assert_eq!(context.sync_after, 0);
        assert_eq!(context.sync_through, 3);
        assert!(!context.context_delivered);
        let ids: Vec<&str> = context.events.iter().map(|e| e.event_id.as_str()).collect();
        assert_eq!(ids, ["ev-1", "ev-2", "ev-3"], "ascending sequence order");
        let sequences: Vec<i64> = context.events.iter().map(|e| e.sequence).collect();
        assert_eq!(sequences, [1, 2, 3]);

        // A capped window keeps the newest events, still ascending.
        let capped = store
            .reader
            .managed_run_context(second.run_id, 2)
            .await
            .unwrap()
            .unwrap();
        let capped_ids: Vec<&str> = capped.events.iter().map(|e| e.event_id.as_str()).collect();
        assert_eq!(capped_ids, ["ev-2", "ev-3"]);

        // Accepting marks the delivery on the run's own context only.
        assert!(
            store
                .writer
                .accept_managed_run_context(second.run_id)
                .await
                .unwrap()
        );
        assert!(
            store
                .reader
                .managed_run_context(second.run_id, 256)
                .await
                .unwrap()
                .unwrap()
                .context_delivered
        );

        // Finished and unknown runs have no active context window.
        assert!(
            store
                .reader
                .managed_run_context(first.run_id, 256)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .reader
                .managed_run_context(ManagedRunId::new(), 256)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .reader
                .managed_run_status(ManagedRunId::new())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn search_events_tails_in_descending_order_and_clamps_the_limit() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, proj) = open_managed_scope(&store, "managed-search").await;
        let run = store
            .writer
            .prepare_workstream_run(managed_prepare_input(ws, proj, "test:1"))
            .await
            .unwrap();
        store
            .writer
            .finish_workstream_run(complete_finish(
                run.run_id,
                vec![
                    managed_event("ev-1", AgentKind::Codex, "native-x", "alpha one"),
                    managed_event("ev-2", AgentKind::Codex, "native-x", "alpha two"),
                    managed_event("ev-3", AgentKind::Codex, "native-x", "alpha zebra three"),
                    managed_event("ev-4", AgentKind::Codex, "native-x", "alpha four"),
                    managed_event("ev-5", AgentKind::Codex, "native-x", "alpha five"),
                ],
            ))
            .await
            .unwrap();

        let ids = |events: Vec<ai_memory_core::WorkstreamEvent>| -> Vec<String> {
            events.into_iter().map(|e| e.event_id).collect()
        };

        // An empty query tails the ledger, newest first.
        let all = store
            .reader
            .search_workstream_events(run.workstream_id, String::new(), 10)
            .await
            .unwrap();
        assert_eq!(ids(all), ["ev-5", "ev-4", "ev-3", "ev-2", "ev-1"]);

        let top_two = store
            .reader
            .search_workstream_events(run.workstream_id, String::new(), 2)
            .await
            .unwrap();
        assert_eq!(ids(top_two), ["ev-5", "ev-4"]);

        // The limit is clamped into 1..=100.
        let clamped_low = store
            .reader
            .search_workstream_events(run.workstream_id, String::new(), 0)
            .await
            .unwrap();
        assert_eq!(ids(clamped_low), ["ev-5"]);
        let clamped_high = store
            .reader
            .search_workstream_events(run.workstream_id, String::new(), 500)
            .await
            .unwrap();
        assert_eq!(clamped_high.len(), 5);

        // A text query matches through FTS only, and the `field:` prefixes
        // accepted by the search surface are stripped before matching.
        let fts = store
            .reader
            .search_workstream_events(run.workstream_id, "zebra".into(), 10)
            .await
            .unwrap();
        assert_eq!(ids(fts), ["ev-3"]);
        for prefixed in ["title:zebra", "body:zebra", "content:zebra"] {
            let hit = store
                .reader
                .search_workstream_events(run.workstream_id, prefixed.into(), 10)
                .await
                .unwrap();
            assert_eq!(ids(hit), ["ev-3"], "prefix must be stripped: {prefixed}");
        }
    }

    #[tokio::test]
    async fn link_native_session_rejects_blank_wrong_agent_and_inactive_targets() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, proj) = open_managed_scope(&store, "managed-link").await;
        let run = store
            .writer
            .prepare_workstream_run(managed_prepare_input(ws, proj, "test:1"))
            .await
            .unwrap();

        for blank in ["", "   "] {
            assert!(
                !store
                    .writer
                    .link_managed_run_session(run.run_id, AgentKind::Codex, blank)
                    .await
                    .unwrap(),
                "blank native session ids are ignored"
            );
        }
        assert!(
            !store
                .writer
                .link_managed_run_session(run.run_id, AgentKind::ClaudeCode, "native-1")
                .await
                .unwrap(),
            "a different harness cannot claim the run"
        );
        assert!(
            !store
                .writer
                .link_managed_run_session(ManagedRunId::new(), AgentKind::Codex, "native-1")
                .await
                .unwrap(),
            "unknown runs cannot be linked"
        );

        assert!(
            store
                .writer
                .link_managed_run_session(run.run_id, AgentKind::Codex, "native-1")
                .await
                .unwrap()
        );
        assert_eq!(
            store
                .reader
                .managed_run_status(run.run_id)
                .await
                .unwrap()
                .unwrap()
                .native_session_id
                .as_deref(),
            Some("native-1")
        );

        assert!(store.writer.cancel_managed_run(run.run_id).await.unwrap());
        assert!(
            !store
                .writer
                .link_managed_run_session(run.run_id, AgentKind::Codex, "native-2")
                .await
                .unwrap(),
            "inactive runs cannot be linked"
        );
    }

    #[tokio::test]
    async fn workstream_selection_validates_names_and_finds_existing() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, proj) = open_managed_scope(&store, "managed-selection").await;
        let prepare = |selection| PrepareWorkstreamRun {
            selection,
            ..managed_prepare_input(ws, proj, "test:1")
        };

        let created = store
            .writer
            .prepare_workstream_run(prepare(WorkstreamSelection::New("alpha".into())))
            .await
            .unwrap();
        assert_eq!(created.workstream_name, "alpha");
        assert!(
            store
                .writer
                .cancel_managed_run(created.run_id)
                .await
                .unwrap()
        );

        let named = store
            .writer
            .prepare_workstream_run(prepare(WorkstreamSelection::Named("alpha".into())))
            .await
            .unwrap();
        assert_eq!(named.workstream_id, created.workstream_id);
        assert!(store.writer.cancel_managed_run(named.run_id).await.unwrap());

        let duplicate = store
            .writer
            .prepare_workstream_run(prepare(WorkstreamSelection::New("alpha".into())))
            .await
            .unwrap_err();
        assert!(matches!(duplicate, StoreError::Duplicate(_)));

        let missing = store
            .writer
            .prepare_workstream_run(prepare(WorkstreamSelection::Named("missing".into())))
            .await
            .unwrap_err();
        assert!(matches!(missing, StoreError::NotFound(_)));

        for invalid in ["bad/name", "bad\\name", "   ", ""] {
            let err = store
                .writer
                .prepare_workstream_run(prepare(WorkstreamSelection::New(invalid.into())))
                .await
                .unwrap_err();
            assert!(
                matches!(err, StoreError::InvalidState(_)),
                "invalid name '{invalid}' must be rejected: {err}"
            );
        }
    }

    #[tokio::test]
    async fn recent_workstreams_are_checkout_local_and_include_linked_harnesses() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, proj) = open_managed_scope(&store, "managed-list").await;
        let prepare = |name: &str, agent| PrepareWorkstreamRun {
            agent,
            selection: WorkstreamSelection::New(name.into()),
            ..managed_prepare_input(ws, proj, name)
        };

        let older = store
            .writer
            .prepare_workstream_run(prepare("older", AgentKind::OpenCode))
            .await
            .unwrap();
        let current = store
            .writer
            .prepare_workstream_run(prepare("current", AgentKind::Codex))
            .await
            .unwrap();
        store
            .writer
            .finish_workstream_run(FinishWorkstreamRun {
                run_id: current.run_id,
                native_session_id: Some("codex-native".into()),
                source_cursor: None,
                events: Vec::new(),
                complete: true,
                segment_path: None,
                exit_code: Some(0),
            })
            .await
            .unwrap();
        let claude = store
            .writer
            .prepare_workstream_run(PrepareWorkstreamRun {
                agent: AgentKind::ClaudeCode,
                selection: WorkstreamSelection::Named("current".into()),
                ..managed_prepare_input(ws, proj, "claude")
            })
            .await
            .unwrap();
        store
            .writer
            .finish_workstream_run(FinishWorkstreamRun {
                run_id: claude.run_id,
                native_session_id: Some("claude-native".into()),
                source_cursor: None,
                events: Vec::new(),
                complete: true,
                segment_path: None,
                exit_code: Some(0),
            })
            .await
            .unwrap();
        // A non-current workstream may finish later and therefore be more
        // recently active; the selected workstream must still lead the list.
        store
            .writer
            .finish_workstream_run(FinishWorkstreamRun {
                run_id: older.run_id,
                native_session_id: Some("open-code-native".into()),
                source_cursor: None,
                events: Vec::new(),
                complete: true,
                segment_path: None,
                exit_code: Some(0),
            })
            .await
            .unwrap();

        let hidden = store
            .writer
            .prepare_workstream_run(PrepareWorkstreamRun {
                repo_fingerprint: "other-repo".into(),
                worktree_fingerprint: "other-worktree".into(),
                selection: WorkstreamSelection::New("hidden".into()),
                ..managed_prepare_input(ws, proj, "hidden")
            })
            .await
            .unwrap();
        store
            .writer
            .cancel_managed_run(hidden.run_id)
            .await
            .unwrap();

        let rows = store
            .reader
            .recent_workstreams(ws, proj, "repo".into(), "worktree".into(), 20)
            .await
            .unwrap();
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            ["current", "older"]
        );
        assert!(rows[0].current);
        assert!(!rows[1].current);
        assert_eq!(
            rows[0].linked_harnesses,
            [AgentKind::ClaudeCode, AgentKind::Codex]
        );
        assert_eq!(rows[1].linked_harnesses, [AgentKind::OpenCode]);
        assert!(rows[0].last_active_at >= rows[0].created_at);
        let limited = store
            .reader
            .recent_workstreams(ws, proj, "repo".into(), "worktree".into(), 1)
            .await
            .unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].workstream_id, current.workstream_id);
        assert!(limited[0].current);
        assert_eq!(
            limited[0].linked_harnesses,
            [AgentKind::ClaudeCode, AgentKind::Codex]
        );
    }

    #[tokio::test]
    async fn recent_workstreams_do_not_leak_across_workspaces() {
        // Two workspaces can hold a project of the same name over the very
        // same clone, so repo/worktree identity alone does not separate them:
        // only the scope columns do.
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (mine, mine_proj) = open_managed_scope(&store, "shared-name").await;
        let theirs = store
            .writer
            .get_or_create_workspace("other-workspace")
            .await
            .unwrap();
        let theirs_proj = store
            .writer
            .get_or_create_project(theirs, "shared-name", None)
            .await
            .unwrap();

        for (ws, proj, name) in [(mine, mine_proj, "mine"), (theirs, theirs_proj, "theirs")] {
            store
                .writer
                .prepare_workstream_run(PrepareWorkstreamRun {
                    selection: WorkstreamSelection::New(name.into()),
                    ..managed_prepare_input(ws, proj, name)
                })
                .await
                .unwrap();
        }

        async fn visible(store: &Store, ws: WorkspaceId, proj: ProjectId) -> Vec<String> {
            store
                .reader
                .recent_workstreams(ws, proj, "repo".into(), "worktree".into(), 20)
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.name)
                .collect()
        }
        assert_eq!(visible(&store, mine, mine_proj).await, ["mine"]);
        assert_eq!(visible(&store, theirs, theirs_proj).await, ["theirs"]);
        // A real workspace paired with the other one's project must not fall
        // back to either list.
        assert!(visible(&store, mine, theirs_proj).await.is_empty());
    }

    /// Create `names` as workstreams in one checkout, newest last, and hand
    /// back each one's `(workstream, run)` pair. Preparing is the only way to
    /// create a workstream, so every seeded row also owns a live run lease.
    async fn seed_workstreams(
        store: &Store,
        ws: WorkspaceId,
        proj: ProjectId,
        names: &[&str],
    ) -> Vec<(WorkstreamId, ManagedRunId)> {
        let mut ids = Vec::new();
        for name in names {
            let prepared = store
                .writer
                .prepare_workstream_run(PrepareWorkstreamRun {
                    selection: WorkstreamSelection::New((*name).into()),
                    ..managed_prepare_input(ws, proj, name)
                })
                .await
                .unwrap();
            ids.push((prepared.workstream_id, prepared.run_id));
        }
        ids
    }

    fn rename_input(
        ws: WorkspaceId,
        proj: ProjectId,
        selector: WorkstreamSelector,
        to: &str,
    ) -> RenameWorkstream {
        RenameWorkstream {
            workspace_id: ws,
            project_id: proj,
            repo_fingerprint: "repo".into(),
            worktree_fingerprint: "worktree".into(),
            selector,
            new_name: to.into(),
        }
    }

    #[tokio::test]
    async fn rename_retitles_by_name_or_id_and_keeps_the_stable_id() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, proj) = open_managed_scope(&store, "proj").await;
        let seeded = seed_workstreams(&store, ws, proj, &["alpha", "beta"]).await;
        let ids: Vec<WorkstreamId> = seeded.iter().map(|(id, _)| *id).collect();
        let run_ids: Vec<ManagedRunId> = seeded.iter().map(|(_, run)| *run).collect();

        let by_name = store
            .writer
            .rename_workstream(rename_input(
                ws,
                proj,
                WorkstreamSelector::Name("alpha".into()),
                "alpha-renamed",
            ))
            .await
            .unwrap();
        assert_eq!(by_name.from, "alpha");
        assert_eq!(by_name.to, "alpha-renamed");
        // The id is what the ledger and `workstream-search` key on, so a
        // rename that minted a new one would orphan the history.
        assert_eq!(by_name.workstream_id, ids[0]);

        let by_id = store
            .writer
            .rename_workstream(rename_input(
                ws,
                proj,
                WorkstreamSelector::Id(ids[1]),
                "beta-renamed",
            ))
            .await
            .unwrap();
        assert_eq!(by_id.from, "beta");
        assert_eq!(by_id.workstream_id, ids[1]);

        // Both new names are selectable, which is the whole point of the
        // rename. Seeding left a live lease on each workstream, so release
        // them first: `Named` selection refuses a busy workstream, and that
        // refusal would mask whether the name resolved at all.
        for run_id in &run_ids {
            assert!(store.writer.cancel_managed_run(*run_id).await.unwrap());
        }
        for name in ["alpha-renamed", "beta-renamed"] {
            store
                .writer
                .prepare_workstream_run(PrepareWorkstreamRun {
                    selection: WorkstreamSelection::Named(name.into()),
                    ..managed_prepare_input(ws, proj, name)
                })
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn rename_does_not_reorder_the_discovery_listing() {
        // Relabelling is not activity. If a rename bumped `updated_at` the
        // renamed row would jump its peers, and if it bumped `selected_at` a
        // bare `ai-memory run` would silently resume a different workstream —
        // both as a side effect of fixing a typo.
        //
        // Three rows, not two: `is_current` leads the ORDER BY, so the
        // current workstream sorts first whatever its timestamps say. Only a
        // pair of non-current rows can show an `updated_at` bump at all.
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, proj) = open_managed_scope(&store, "proj").await;
        seed_workstreams(&store, ws, proj, &["oldest", "middle", "current"]).await;

        async fn listing(store: &Store, ws: WorkspaceId, proj: ProjectId) -> Vec<(String, bool)> {
            store
                .reader
                .recent_workstreams(ws, proj, "repo".into(), "worktree".into(), 20)
                .await
                .unwrap()
                .into_iter()
                .map(|row| (row.name, row.current))
                .collect()
        }
        let before = listing(&store, ws, proj).await;
        assert_eq!(
            before,
            [
                ("current".to_owned(), true),
                ("middle".to_owned(), false),
                ("oldest".to_owned(), false),
            ]
        );

        store
            .writer
            .rename_workstream(rename_input(
                ws,
                proj,
                WorkstreamSelector::Name("oldest".into()),
                "oldest-renamed",
            ))
            .await
            .unwrap();

        // `oldest-renamed` must stay behind `middle`: it was renamed, not
        // worked on. A bumped `updated_at` would put it second.
        assert_eq!(
            listing(&store, ws, proj).await,
            [
                ("current".to_owned(), true),
                ("middle".to_owned(), false),
                ("oldest-renamed".to_owned(), false),
            ]
        );

        // Renaming the current workstream must not hand `current` to anyone
        // else either, which a bumped `selected_at` on the wrong row would do.
        store
            .writer
            .rename_workstream(rename_input(
                ws,
                proj,
                WorkstreamSelector::Name("oldest-renamed".into()),
                "oldest-again",
            ))
            .await
            .unwrap();
        let after = listing(&store, ws, proj).await;
        assert_eq!(after[0], ("current".to_owned(), true));
        assert_eq!(after[2].0, "oldest-again");
    }

    #[tokio::test]
    async fn rename_refuses_a_name_another_workstream_holds() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, proj) = open_managed_scope(&store, "proj").await;
        seed_workstreams(&store, ws, proj, &["alpha", "beta"]).await;

        let failure = store
            .writer
            .rename_workstream(rename_input(
                ws,
                proj,
                WorkstreamSelector::Name("alpha".into()),
                "beta",
            ))
            .await
            .unwrap_err();
        assert!(
            matches!(&failure, StoreError::WorkstreamNameTaken(name) if name == "beta"),
            "expected a named collision, got {failure:?}"
        );

        // Renaming onto its own current name writes nothing and is not an
        // error, so a repeated command stays safe.
        let noop = store
            .writer
            .rename_workstream(rename_input(
                ws,
                proj,
                WorkstreamSelector::Name("alpha".into()),
                "alpha",
            ))
            .await
            .unwrap();
        assert_eq!((noop.from.as_str(), noop.to.as_str()), ("alpha", "alpha"));
    }

    #[tokio::test]
    async fn rename_cannot_reach_a_workstream_outside_the_resolved_scope() {
        // `workstreams.id` is globally unique, so the id selector would be a
        // cross-scope write primitive without the checkout predicate.
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (mine, mine_proj) = open_managed_scope(&store, "shared-name").await;
        let theirs = store
            .writer
            .get_or_create_workspace("other-workspace")
            .await
            .unwrap();
        let theirs_proj = store
            .writer
            .get_or_create_project(theirs, "shared-name", None)
            .await
            .unwrap();
        let theirs_ids = seed_workstreams(&store, theirs, theirs_proj, &["theirs"]).await;
        let theirs_workstream = theirs_ids[0].0;
        seed_workstreams(&store, mine, mine_proj, &["mine"]).await;

        let by_id = store
            .writer
            .rename_workstream(rename_input(
                mine,
                mine_proj,
                WorkstreamSelector::Id(theirs_workstream),
                "stolen",
            ))
            .await
            .unwrap_err();
        assert!(
            matches!(by_id, StoreError::NotFound(_)),
            "an id from another workspace must read as absent, got {by_id:?}"
        );

        let by_name = store
            .writer
            .rename_workstream(rename_input(
                mine,
                mine_proj,
                WorkstreamSelector::Name("theirs".into()),
                "stolen",
            ))
            .await
            .unwrap_err();
        assert!(matches!(by_name, StoreError::NotFound(_)));

        // The other workspace's row is untouched by either attempt.
        let theirs_names: Vec<String> = store
            .reader
            .recent_workstreams(theirs, theirs_proj, "repo".into(), "worktree".into(), 20)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.name)
            .collect();
        assert_eq!(theirs_names, ["theirs"]);
    }

    #[tokio::test]
    async fn rename_rejects_a_name_that_run_new_would_reject() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, proj) = open_managed_scope(&store, "proj").await;
        seed_workstreams(&store, ws, proj, &["alpha"]).await;

        for bad in ["", "   ", "has/slash", "has\\backslash", "ctrl\u{1}char"] {
            let failure = store
                .writer
                .rename_workstream(rename_input(
                    ws,
                    proj,
                    WorkstreamSelector::Name("alpha".into()),
                    bad,
                ))
                .await
                .unwrap_err();
            assert!(
                matches!(failure, StoreError::InvalidState(_)),
                "name {bad:?} should be rejected, got {failure:?}"
            );
        }
    }

    #[tokio::test]
    async fn prepare_expires_a_stale_lease_and_reopens_the_workstream() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, proj) = open_managed_scope(&store, "managed-expiry").await;
        let stale = store
            .writer
            .prepare_workstream_run(managed_prepare_input(ws, proj, "test:1"))
            .await
            .unwrap();

        // Force the lease into the past; the next prepare must expire the run
        // instead of reporting the workstream busy.
        set_managed_run_lease(store.db_path(), stale.run_id, 1);
        let reopened = store
            .writer
            .prepare_workstream_run(managed_prepare_input(ws, proj, "test:2"))
            .await
            .unwrap();
        assert_ne!(reopened.run_id, stale.run_id);
        assert_eq!(reopened.workstream_id, stale.workstream_id);
        assert_eq!(
            store
                .reader
                .managed_run_status(stale.run_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "expired"
        );
        assert_eq!(
            store
                .reader
                .managed_run_status(reopened.run_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "active"
        );
    }
    #[tokio::test]
    async fn entity_stream_finds_and_weights_pages() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "entities", None)
            .await
            .unwrap();

        let with_entities = |path: &str, body: &str, entities: Vec<&str>| {
            let mut page = sample_page(ws, proj, path, body);
            page.entities = entities.into_iter().map(str::to_string).collect();
            page
        };
        // `turbopuffer` is rare (one page); `sqlite` is common (three).
        store
            .writer
            .upsert_page(with_entities(
                "rare.md",
                "no query words in this body at all",
                vec!["turbopuffer", "sqlite"],
            ))
            .await
            .unwrap();
        for path in ["common1.md", "common2.md"] {
            store
                .writer
                .upsert_page(with_entities(path, "unrelated prose", vec!["sqlite"]))
                .await
                .unwrap();
        }
        store
            .writer
            .upsert_page(with_entities(
                "delimiters.md",
                "more unrelated prose",
                vec!["writer-actor", "queue_worker", "comma, entity"],
            ))
            .await
            .unwrap();

        // A query naming the entity finds the page whose body lacks the term.
        let hits = store
            .reader
            .entity_hits_for_project(ws, proj, "how do we use Turbopuffer?", 10, None)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].hit.path.as_str(), "rare.md");
        assert_eq!(hits[0].matched, vec!["turbopuffer".to_string()]);

        let delimiter_hits = store
            .reader
            .entity_hits_for_project(ws, proj, "actor worker comma", 10, None)
            .await
            .unwrap();
        let delimiter_hit = delimiter_hits
            .iter()
            .find(|hit| hit.hit.path.as_str() == "delimiters.md")
            .expect("word prefixes after hyphen and underscore must match");
        assert_eq!(
            delimiter_hit.matched,
            vec![
                "comma, entity".to_string(),
                "queue_worker".to_string(),
                "writer-actor".to_string(),
            ],
            "explain names must preserve commas and sort deterministically",
        );

        // Inverse frequency: the page carrying the rare entity outranks
        // pages carrying only the common one.
        let hits = store
            .reader
            .entity_hits_for_project(ws, proj, "turbopuffer sqlite", 10, None)
            .await
            .unwrap();
        assert_eq!(hits.len(), 3, "{hits:?}");
        assert_eq!(hits[0].hit.path.as_str(), "rare.md");
        assert!(
            hits[0].weight > hits[1].weight,
            "rare entity must weigh more: {:?}",
            hits.iter()
                .map(|h| (h.hit.path.as_str(), h.weight))
                .collect::<Vec<_>>(),
        );

        // Prefix matching, and short tokens are ignored as noise.
        assert_eq!(
            store
                .reader
                .entity_hits_for_project(ws, proj, "turbopuff", 10, None)
                .await
                .unwrap()
                .len(),
            1,
            "prefix should match the entity",
        );
        assert!(
            store
                .reader
                .entity_hits_for_project(ws, proj, "a of", 10, None)
                .await
                .unwrap()
                .is_empty(),
            "sub-3-char tokens must not match",
        );

        // Hybrid search surfaces the entity-only hit and explains it.
        let explained = store
            .reader
            .hybrid_search_explained(
                ws,
                proj,
                "turbopuffer".into(),
                None,
                String::new(),
                String::new(),
                0,
                10,
                None,
            )
            .await
            .unwrap();
        let (hit, explain) = explained
            .iter()
            .find(|(h, _)| h.path.as_str() == "rare.md")
            .expect("entity stream must feed hybrid search");
        assert_eq!(explain.entity_rank, Some(1));
        assert!(explain.entity_weight.is_some_and(|weight| weight > 0.0));
        assert_eq!(explain.matched_entities, vec!["turbopuffer".to_string()]);
        assert!(explain.rrf.entity > 0.0);
        assert!(
            (explain.fused
                - (explain.rrf.fts + explain.rrf.entity + explain.rrf.vector + explain.rrf.graph))
                .abs()
                < f64::EPSILON,
            "fused score must include the entity stream: {explain:?}",
        );
        assert!(
            explain.fts_rank.is_none(),
            "the body has no query term, so FTS must miss it",
        );
        // `rank` is the fused score after the bounded authority multiplier,
        // so it tracks `fused` only up to that factor — which is exactly why
        // the explain reports the factor alongside it.
        let authority = explain.authority.unwrap_or(1.0);
        assert!((hit.rank + explain.fused * authority).abs() < 1e-12);

        // Rewriting the page replaces the latest version's entity set.
        store
            .writer
            .upsert_page(with_entities("rare.md", "rewritten body", vec!["lancedb"]))
            .await
            .unwrap();
        assert!(
            store
                .reader
                .entity_hits_for_project(ws, proj, "turbopuffer", 10, None)
                .await
                .unwrap()
                .is_empty(),
            "a rewrite must drop the old entity links",
        );
        assert_eq!(
            store
                .reader
                .entity_hits_for_project(ws, proj, "lancedb", 10, None)
                .await
                .unwrap()
                .len(),
            1,
        );

        let mut expired = with_entities("expired.md", "historical prose", vec!["lancedb"]);
        expired.expires_at = Some("2000-01-01T00:00:00Z".parse().unwrap());
        store.writer.upsert_page(expired).await.unwrap();
        let current_only = store
            .reader
            .entity_hits_for_project(ws, proj, "lancedb", usize::MAX, None)
            .await
            .unwrap();
        assert_eq!(current_only.len(), 1, "expired entity pages stay hidden");
        assert_eq!(
            current_only[0].weight, 1.0,
            "expired pages must not dilute inverse-frequency weighting"
        );

        // A project with no declared entities contributes nothing.
        let bare = store
            .writer
            .get_or_create_project(ws, "bare", None)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, bare, "plain.md", "sqlite and turbopuffer"))
            .await
            .unwrap();
        assert!(
            store
                .reader
                .entity_hits_for_project(ws, bare, "turbopuffer", 10, None)
                .await
                .unwrap()
                .is_empty(),
            "no entities declared → the stream stays silent",
        );

        let decoy = store
            .writer
            .get_or_create_project(ws, "decoy", None)
            .await
            .unwrap();
        let mut decoy_page = sample_page(ws, decoy, "private.md", "unrelated");
        decoy_page.entities = vec!["lancedb".into()];
        store.writer.upsert_page(decoy_page).await.unwrap();
        let scoped = store
            .reader
            .entity_hits_for_project(ws, proj, "lancedb", 10, None)
            .await
            .unwrap();
        assert!(
            scoped
                .iter()
                .all(|hit| hit.hit.path.as_str() != "private.md"),
            "entity retrieval must not leak a sibling project's pages"
        );

        store
            .writer
            .delete_page(ws, proj, PagePath::new("delimiters.md").unwrap(), None)
            .await
            .unwrap();
        let conn = Connection::open(store.db_path()).unwrap();
        let deleted_entities: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entities \
                 WHERE workspace_id = ?1 AND project_id = ?2 \
                   AND name IN ('writer-actor', 'queue_worker', 'comma, entity')",
                params![ws.as_bytes(), proj.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            deleted_entities, 0,
            "deleting every page version must remove unlinked derived entities"
        );
    }
}
