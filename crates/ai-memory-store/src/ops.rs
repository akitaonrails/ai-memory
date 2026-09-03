//! Mutating SQL operations executed on the writer thread.
//!
//! Each operation is one transaction. Calling them from anywhere other than
//! the writer thread would violate the single-writer invariant (see
//! [`crate::writer`]).

use std::collections::BTreeSet;

use ai_memory_core::{
    AgentKind, EntityId, HandoffAcceptance, HandoffId, IdentityKey, LinkTarget, NewHandoff,
    NewObservation, NewPage, NewSession, ObservationId, ObservationKind, OwnerFilter, PageId,
    PagePath, ProjectId, SessionId, WorkspaceId,
};

/// Summary returned by [`reorg_sessions`] and exposed via
/// [`crate::writer::WriterHandle::reorg_sessions`].
#[derive(Debug, Default, Clone)]
pub struct ReorgSummary {
    /// Sessions whose `project_id` was changed.
    pub sessions_moved: usize,
    /// Observations updated to match their session's new project.
    pub observations_updated: usize,
    /// `is_latest=1` pages marked `is_latest=0` (mash-up graveyard).
    pub pages_graveyarded: usize,
}

/// Result of atomically claiming a keyed hook observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngestObservationOutcome {
    /// This delivery claimed the key and inserted the observation.
    Inserted(ObservationId),
    /// The observation exists, but downstream hook effects did not finish.
    ResumePending,
    /// The observation and downstream hook effects already completed.
    AlreadyComplete,
}

/// Unforgeable writer-issued session capability for hook follow-up mutations.
#[derive(Clone, Debug)]
pub struct AdmittedSession {
    session_id: SessionId,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    agent_kind: AgentKind,
    owner: Option<String>,
}
impl AdmittedSession {
    /// Persisted session identifier authorized by this guard.
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }
    /// Persisted workspace identifier authorized by this guard.
    pub fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }
    /// Persisted project identifier authorized by this guard.
    pub fn project_id(&self) -> ProjectId {
        self.project_id
    }
    /// Persisted agent kind authorized by this guard.
    pub fn agent_kind(&self) -> AgentKind {
        self.agent_kind
    }
    /// Persisted owner storage key, or `None` for a shared session.
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }
}

/// Result of atomic hook admission.
#[derive(Clone, Debug)]
pub enum HookSessionAdmission {
    /// An ordinary observation was admitted for this persisted session.
    Observation {
        /// Writer-issued guard for the persisted session.
        session: AdmittedSession,
        /// Result of claiming and inserting the observation.
        ingest: IngestObservationOutcome,
    },
    /// A terminal event was admitted for a session that was still open.
    EndOpen {
        /// Writer-issued guard for the persisted session.
        session: AdmittedSession,
        /// Result of claiming and inserting the observation.
        ingest: IngestObservationOutcome,
    },
    /// A terminal event was admitted after new observations followed an end.
    ReEnd {
        /// Writer-issued guard for the persisted session.
        session: AdmittedSession,
        /// Result of claiming and inserting the observation.
        ingest: IngestObservationOutcome,
    },
    /// A terminal event was already fully represented by this persisted session.
    AlreadyEnded {
        /// Writer-issued guard for the persisted session.
        session: AdmittedSession,
    },
    /// A terminal event named no persisted session and created nothing.
    InvalidMissingEnd,
    /// A terminal event named a persisted session in a different scope, so it
    /// is not that session's end. Mirrors the pre-guard
    /// `SessionEndDisposition::DropInvalid` arm.
    InvalidScopedEnd,
}
/// Result of conditionally ending a session whose persisted observations are
/// all lifecycle boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleOnlyEndOutcome {
    /// The session was still boundary-only inside the writer transaction.
    Ended {
        /// Startup handoff returned to the open pool, when one was claimed.
        reopened_handoff: Option<HandoffId>,
    },
    /// Substantive work arrived before the atomic end check.
    Substantive,
}

/// Summary returned by [`purge_project`] and exposed via
/// [`crate::writer::WriterHandle::purge_project`].
#[derive(Debug, Default, Clone)]
pub struct PurgeSummary {
    /// Human-readable `workspace/project` label. Set by the caller (writer
    /// only knows IDs); filled in by [`purge_project`] from its parameters.
    pub label: String,
    /// Distinct page paths that were present before the delete (all versions,
    /// not just `is_latest=1`). The admin handler uses this list to remove
    /// the corresponding files from the wiki directory.
    pub page_paths: Vec<String>,
    /// Number of `pages` rows deleted (all versions, not just latest).
    pub pages_deleted: u64,
    /// Number of `sessions` rows deleted.
    pub sessions_deleted: u64,
    /// Number of `observations` rows deleted.
    pub observations_deleted: u64,
    /// Number of `handoffs` rows deleted.
    pub handoffs_deleted: u64,
    /// Number of `page_embeddings` rows deleted (cascades through pages).
    pub embeddings_deleted: u64,
    /// Number of `workstreams` rows deleted. These cascade from `projects`,
    /// so a project that looks empty by page/session/observation count can
    /// still take a managed workstream — and its portable event ledger — down
    /// with it. Counted so the caller can say so out loud.
    pub workstreams_deleted: u64,
    /// Number of `managed_runs` rows deleted (cascades through workstreams).
    pub managed_runs_deleted: u64,
    /// Ids of the deleted workstreams. The admin layer uses these typed,
    /// pre-delete identifiers to remove `raw/workstreams/<id>/` after the SQL
    /// transaction commits and to report any filesystem partial failure.
    pub workstream_ids: Vec<String>,
    /// Whether the freed bytes were reclaimed (`VACUUM` ran). False for a
    /// plain logical delete.
    pub compacted: bool,
}
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};

use crate::error::{StoreError, StoreResult};

/// One embedding upsert requested by a backfill or embed command.
#[derive(Debug)]
pub struct EmbeddingWrite {
    /// Page receiving the embedding.
    pub page_id: PageId,
    /// Packed little-endian `f32` vector bytes.
    pub vector_bytes: Vec<u8>,
    /// Embedding provider name.
    pub provider: String,
    /// Embedding model name.
    pub model: String,
    /// Vector dimension.
    pub dim: u32,
}

/// Upsert a page by path, superseding any existing latest version when the
/// content (sha256 of body) has changed.
///
/// Returns the id of the page row that should now be considered current.
pub fn upsert_page(conn: &mut Connection, page: &NewPage) -> StoreResult<PageId> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let result_id = upsert_page_in_tx(&tx, page, now)?;
    tx.commit()?;
    Ok(result_id)
}

/// Resolve a workspace by name, creating it if missing. Atomic.
pub fn get_or_create_workspace(
    conn: &mut Connection,
    name: &str,
) -> StoreResult<ai_memory_core::WorkspaceId> {
    let tx = conn.transaction()?;
    let existing: Option<Vec<u8>> = tx
        .query_row(
            "SELECT id FROM workspaces WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )
        .optional()?;
    let id = if let Some(bytes) = existing {
        ai_memory_core::WorkspaceId::from_slice(&bytes)?
    } else {
        let id = ai_memory_core::WorkspaceId::new();
        tx.execute(
            "INSERT INTO workspaces (id, name, created_at) VALUES (?1, ?2, ?3)",
            params![id.as_bytes(), name, Timestamp::now().as_microsecond()],
        )?;
        id
    };
    tx.commit()?;
    Ok(id)
}

/// Names of other workspaces that already contain a project called `name`
/// (excluding `workspace_id`), ordered by workspace name. Homonymous
/// projects across workspaces are legal — rows are id-namespaced — but on
/// *creation* a non-empty result is almost always an accidental misroute,
/// so the caller can surface a warning. Runs inside the caller's `tx`.
fn project_name_in_other_workspaces(
    tx: &rusqlite::Transaction<'_>,
    workspace_id: &ai_memory_core::WorkspaceId,
    name: &str,
) -> StoreResult<Vec<String>> {
    let mut stmt = tx.prepare(
        "SELECT w.name FROM projects p \
         JOIN workspaces w ON w.id = p.workspace_id \
         WHERE p.name = ?1 AND p.workspace_id != ?2 \
         ORDER BY w.name",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![name, workspace_id.as_bytes()], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn warn_project_name_in_other_workspaces(name: &str, also_in: &[String]) {
    if !also_in.is_empty() {
        tracing::warn!(
            project = name,
            also_in = ?also_in,
            "creating a project whose name already exists in other workspace(s) — legal (id-namespaced) but often an accidental misroute"
        );
    }
}

/// Resolve a project by `(workspace_id, name)`, creating it if missing.
/// Atomic.
pub fn get_or_create_project(
    conn: &mut Connection,
    workspace_id: &ai_memory_core::WorkspaceId,
    name: &str,
    repo_path: Option<&str>,
) -> StoreResult<ai_memory_core::ProjectId> {
    let repo_path = repo_path.map(normalize_repo_path_key);
    let tx = conn.transaction()?;
    let mut also_in = Vec::new();
    let mut created = false;
    let existing: Option<Vec<u8>> = tx
        .query_row(
            "SELECT id FROM projects WHERE workspace_id = ?1 AND name = ?2",
            params![workspace_id.as_bytes(), name],
            |row| row.get(0),
        )
        .optional()?;
    let id = if let Some(bytes) = existing {
        ai_memory_core::ProjectId::from_slice(&bytes)?
    } else {
        also_in = project_name_in_other_workspaces(&tx, workspace_id, name)?;
        let id = ai_memory_core::ProjectId::new();
        tx.execute(
            "INSERT INTO projects (id, workspace_id, name, repo_path, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id.as_bytes(),
                workspace_id.as_bytes(),
                name,
                repo_path.as_deref(),
                Timestamp::now().as_microsecond()
            ],
        )?;
        created = true;
        id
    };
    tx.commit()?;
    if scheduler_state_table_exists(conn)? {
        crate::auto_improve::ensure_scheduler_state(conn, *workspace_id, id)?;
    }
    if created {
        warn_project_name_in_other_workspaces(name, &also_in);
    }
    Ok(id)
}

/// Delete "hollow" project rows: zero pages (any version), zero sessions,
/// zero observations, zero handoffs, zero managed workstreams, zero
/// auto-improve runs/proposals/rejections, and older than `min_age_days`.
/// (The per-project scheduler-state row is bookkeeping created for every
/// project and does not count as data.) These are pure bookkeeping noise left
/// behind by probes, renames, and failed first events — nothing exists to lose,
/// which is what makes this safe to run on a schedule (the operator-driven
/// `purge-project` covers everything that actually holds data). Reserved
/// projects (`scratch`, the cwd-less fallback; `_global`, the preferences
/// scope) are exempt even when empty. Returns the deleted names for logging.
///
/// # Errors
/// Propagates SQLite failures.
pub fn sweep_hollow_projects(conn: &mut Connection, min_age_days: u32) -> StoreResult<Vec<String>> {
    let cutoff =
        Timestamp::now().as_microsecond() - i64::from(min_age_days) * 24 * 60 * 60 * 1_000_000;
    let tx = conn.transaction()?;
    let names: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT name FROM projects
             WHERE name NOT IN ('scratch', ?1)
               AND created_at < ?2
               AND NOT EXISTS (SELECT 1 FROM pages        WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM sessions     WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM observations WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM handoffs     WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM workstreams  WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM auto_improve_runs      WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM auto_improve_proposals WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM auto_improve_rejections WHERE project_id = projects.id)",
        )?;
        let rows = stmt.query_map(
            params![ai_memory_core::GLOBAL_SCOPE_PROJECT, cutoff],
            |row| row.get::<_, String>(0),
        )?;
        rows.collect::<Result<_, _>>()?
    };
    if !names.is_empty() {
        tx.execute(
            "DELETE FROM projects
             WHERE name NOT IN ('scratch', ?1)
               AND created_at < ?2
               AND NOT EXISTS (SELECT 1 FROM pages        WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM sessions     WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM observations WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM handoffs     WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM workstreams  WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM auto_improve_runs      WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM auto_improve_proposals WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM auto_improve_rejections WHERE project_id = projects.id)",
            params![ai_memory_core::GLOBAL_SCOPE_PROJECT, cutoff],
        )?;
    }
    tx.commit()?;
    Ok(names)
}

/// NULL out `repo_path` values that act as longest-prefix-match catch-alls
/// (issue #103), so every project nested beneath them stops resolving to the
/// wrong row after upgrade. A one-shot startup heal; idempotent (a healed row
/// is NULL and drops out of the candidate set, so a second pass heals 0).
/// Returns the number of rows healed.
///
/// A row is healed when:
/// - `repo_path` is one of the two broad sentinels -- filesystem root (`/`)
///   or the operator's home directory (`home`, when provided). These are
///   healed even if they happen to be git work-tree roots (e.g. a dotfiles
///   repo checked out at `$HOME`): as prefix keys they swallow everything
///   beneath them.
/// - otherwise, `repo_path` EXISTS on this host but is NOT a git work-tree
///   root (e.g. a bare `~/projects` cwd the original corruption captured).
///
/// A `repo_path` that does NOT exist on this host is left untouched: under a
/// remote/multi-user daemon it may be a client path for another user, or a
/// temporarily unmounted drive, and destroying it would wipe a valid prefix
/// key. This safety rule is mandatory.
pub fn heal_catch_all_repo_paths(conn: &mut Connection, home: Option<&str>) -> StoreResult<u64> {
    let home = home.map(normalize_repo_path_key);
    let candidates: Vec<(Vec<u8>, String)> = {
        let mut stmt =
            conn.prepare("SELECT id, repo_path FROM projects WHERE repo_path IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let to_null: Vec<Vec<u8>> = candidates
        .into_iter()
        .filter(|(_, repo_path)| should_heal_repo_path(repo_path, home.as_deref()))
        .map(|(id, _)| id)
        .collect();
    let tx = conn.transaction()?;
    for id in &to_null {
        tx.execute(
            "UPDATE projects SET repo_path = NULL WHERE id = ?1",
            params![id],
        )?;
    }
    tx.commit()?;
    Ok(u64::try_from(to_null.len()).unwrap_or(0))
}

/// Decide whether a non-NULL `repo_path` is a prefix-match catch-all that
/// should be NULLed. See [`heal_catch_all_repo_paths`] for the full rule.
fn should_heal_repo_path(repo_path: &str, home: Option<&str>) -> bool {
    let repo_path_key = normalize_repo_path_key(repo_path);
    if repo_path_key == "/" || home == Some(repo_path_key.as_str()) {
        return true; // broad sentinels, healed even if they look like git roots
    }
    let p = std::path::Path::new(repo_path);
    // Non-existent paths (and stat errors) are left alone: multi-user/unmounted
    // safety. An existing path is a catch-all only when its `.git` is
    // definitively absent (a normal repo has a `.git` dir, a worktree/submodule
    // a `.git` file); a `.git` stat error preserves the row, same as the
    // path-existence check above.
    matches!(p.try_exists(), Ok(true)) && matches!(p.join(".git").try_exists(), Ok(false))
}

fn normalize_repo_path_key(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if normalized.len() > 1 {
        normalized.trim_end_matches('/').to_string()
    } else {
        normalized
    }
}

fn scheduler_state_table_exists(conn: &Connection) -> StoreResult<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'auto_improve_scheduler_state'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Insert a workspace with an **explicit id**, idempotent. Unlike
/// [`get_or_create_workspace`] (which mints a fresh id), this preserves the id
/// the caller already holds — used by `reindex`, which recovers the id from the
/// wiki directory name so the rebuilt index keys pages by the same
/// `(workspace_id, project_id)` the on-disk tree is laid out under. Re-running
/// is a no-op (`ON CONFLICT(id)`). `created_at` is the rebuild time.
pub fn ensure_workspace_with_id(
    conn: &mut Connection,
    id: ai_memory_core::WorkspaceId,
    name: &str,
) -> StoreResult<()> {
    conn.execute(
        "INSERT INTO workspaces (id, name, created_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(id) DO NOTHING",
        params![id.as_bytes(), name, Timestamp::now().as_microsecond()],
    )?;
    let existing: Option<String> = conn
        .query_row(
            "SELECT name FROM workspaces WHERE id = ?1",
            params![id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    match existing {
        Some(existing) if existing == name => Ok(()),
        Some(existing) => Err(StoreError::Duplicate(format!(
            "workspace id {id} already exists as name '{existing}', not manifest name '{name}'"
        ))),
        None => Err(StoreError::NotFound(format!(
            "workspace id {id} was not inserted"
        ))),
    }?;
    Ok(())
}

/// Insert a project with an **explicit id** under `workspace_id`, idempotent.
/// The reindex counterpart of [`ensure_workspace_with_id`].
pub fn ensure_project_with_id(
    conn: &mut Connection,
    id: ai_memory_core::ProjectId,
    workspace_id: ai_memory_core::WorkspaceId,
    name: &str,
    repo_path: Option<&str>,
) -> StoreResult<()> {
    let repo_path = repo_path.map(normalize_repo_path_key);
    let tx = conn.transaction()?;
    let inserted = tx.execute(
        "INSERT INTO projects (id, workspace_id, name, repo_path, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(id) DO NOTHING",
        params![
            id.as_bytes(),
            workspace_id.as_bytes(),
            name,
            repo_path.as_deref(),
            Timestamp::now().as_microsecond()
        ],
    )?;
    let also_in = if inserted > 0 {
        project_name_in_other_workspaces(&tx, &workspace_id, name)?
    } else {
        Vec::new()
    };
    type ProjectRow = (Vec<u8>, String, Option<String>);
    let existing: Option<ProjectRow> = tx
        .query_row(
            "SELECT workspace_id, name, repo_path FROM projects WHERE id = ?1",
            params![id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    match existing {
        Some((existing_ws, existing_name, existing_repo_path))
            if existing_ws.as_slice() == workspace_id.as_bytes()
                && existing_name == name
                && existing_repo_path.as_deref() == repo_path.as_deref() =>
        {
            Ok(())
        }
        Some((existing_ws, existing_name, existing_repo_path)) => {
            Err(StoreError::Duplicate(format!(
                "project id {id} already exists with workspace_id bytes length {}, name='{existing_name}', repo_path={existing_repo_path:?}; manifest has workspace={workspace_id}, name='{name}', repo_path={repo_path:?}",
                existing_ws.len(),
            )))
        }
        None => Err(StoreError::NotFound(format!(
            "project id {id} was not inserted"
        ))),
    }?;
    tx.commit()?;
    if inserted > 0 {
        warn_project_name_in_other_workspaces(name, &also_in);
    }
    Ok(())
}

/// Assert that `project_id` currently belongs to `workspace_id`.
///
/// Wiki writes call this before touching the filesystem so a stale hook/cache
/// carrying the old workspace for a moved project fails before it can create an
/// orphan file. The pairing INSERT triggers are still the final SQL backstop.
pub fn ensure_project_workspace(
    conn: &Connection,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
) -> StoreResult<()> {
    let found = conn
        .query_row(
            "SELECT 1 FROM projects WHERE id = ?1 AND workspace_id = ?2",
            params![project_id.as_bytes(), workspace_id.as_bytes()],
            |_| Ok(()),
        )
        .optional()?;
    if found.is_some() {
        Ok(())
    } else {
        Err(StoreError::NotFound(format!(
            "project {project_id} does not belong to workspace {workspace_id}"
        )))
    }
}

/// Upsert a batch of pages inside one transaction. Either *all* pages
/// land (each becoming the new `is_latest=true` version) or none do.
///
/// This is the M7b atomic-fan-out path: the consolidator can hand a
/// list of {sessions, concepts, decisions} pages and trust that
/// either the whole batch supersedes or the wiki is unchanged.
pub fn upsert_pages_batch(conn: &mut Connection, pages: &[NewPage]) -> StoreResult<Vec<PageId>> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let mut out = Vec::with_capacity(pages.len());
    for page in pages {
        let id = upsert_page_in_tx(&tx, page, now)?;
        out.push(id);
    }
    tx.commit()?;
    Ok(out)
}

struct ExistingPageVersion {
    id: Vec<u8>,
    body_sha256: Vec<u8>,
    frontmatter_json: String,
    title: String,
    tier: String,
    pinned: i64,
}

/// Normalise a page path into FTS-friendly search text, indexing BOTH forms
/// so either a whole-slug or a single-word query hits:
/// - segments: `/` and `.` → space, KEEPING `-`/`_` (FTS token chars) so the
///   full hyphenated slug stays one token (`foo-bar` matches a `"foo-bar"`
///   query);
/// - words: also split `-`/`_` so each word is its own token (`bar` matches).
///
/// `notes/foo-bar.md` → `notes foo-bar md notes foo bar md`.
///
/// MUST stay byte-identical to the backfill expression in migration V17 so
/// the `rebuild` and live-write paths index the same text (matching bm25
/// term frequencies, not just the same match set).
pub(crate) fn path_search_text(path: &str) -> String {
    let segments = path.replace(['/', '.'], " ");
    let words = segments.replace(['-', '_'], " ");
    format!("{segments} {words}")
}

/// Count latest page rows whose frontmatter is not yet OKF-conformant
/// (missing `type` or `generated.at`). Read-only; the migration uses it
/// to decide whether the backup gate must engage at all.
pub fn okf_nonconformant_latest_pages(conn: &Connection) -> StoreResult<u64> {
    let mut stmt = conn.prepare("SELECT path, frontmatter_json FROM pages WHERE is_latest = 1")?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let mut pending = 0u64;
    for (path, fm_str) in rows {
        let fm: serde_json::Value = serde_json::from_str(&fm_str).unwrap_or_default();
        let mut conformed = fm.clone();
        ai_memory_core::okf::conform_frontmatter(&path, &mut conformed);
        if conformed != fm || ai_memory_core::okf::generated_at(&fm).is_none() {
            pending += 1;
        }
    }
    Ok(pending)
}

/// One latest-page row rewritten in place by [`okf_migrate_latest_pages`].
#[derive(Debug, Clone)]
pub struct OkfMigratedPage {
    /// Owning workspace.
    pub workspace_id: ai_memory_core::WorkspaceId,
    /// Owning project.
    pub project_id: ai_memory_core::ProjectId,
    /// Wiki-relative page path.
    pub path: String,
    /// The `generated.at` stamped from the row's own `updated_at`.
    pub generated_at: String,
}

/// One-shot OKF migration of every latest page row (docs/okf.md): conform
/// the frontmatter IN PLACE — same id, same version row, `updated_at`
/// untouched, body untouched — stamping `generated.at` from the row's own
/// `updated_at` so nothing looks freshly written. Historical versions
/// (`is_latest = 0`) stay as they were. Idempotent: conformed rows are
/// left alone, so a re-run rewrites zero pages.
pub fn okf_migrate_latest_pages(conn: &mut Connection) -> StoreResult<Vec<OkfMigratedPage>> {
    type LatestPageRow = (Vec<u8>, Vec<u8>, Vec<u8>, String, String, i64);
    let tx = conn.transaction()?;
    let mut migrated = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT id, workspace_id, project_id, path, frontmatter_json, updated_at              FROM pages WHERE is_latest = 1",
        )?;
        let rows: Vec<LatestPageRow> = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })?
            .collect::<Result<_, _>>()?;
        drop(stmt);
        for (id, ws, proj, path, fm_str, updated_at) in rows {
            let fm: serde_json::Value = serde_json::from_str(&fm_str).unwrap_or_default();
            let mut conformed = fm.clone();
            ai_memory_core::okf::conform_frontmatter(&path, &mut conformed);
            let at = jiff::Timestamp::from_microsecond(updated_at)
                .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
                .strftime("%Y-%m-%dT%H:%M:%SZ")
                .to_string();
            if ai_memory_core::okf::generated_at(&conformed).is_none() {
                ai_memory_core::okf::stamp_generated_at(&mut conformed, &at);
            }
            if conformed == fm {
                continue;
            }
            tx.execute(
                "UPDATE pages SET frontmatter_json = ?1 WHERE id = ?2",
                params![serde_json::to_string(&conformed)?, id],
            )?;
            migrated.push(OkfMigratedPage {
                workspace_id: ai_memory_core::WorkspaceId::from_slice(&ws)?,
                project_id: ai_memory_core::ProjectId::from_slice(&proj)?,
                path,
                generated_at: ai_memory_core::okf::generated_at(&conformed)
                    .unwrap_or(&at)
                    .to_string(),
            });
        }
    }
    tx.commit()?;
    Ok(migrated)
}

/// Stamp the per-version `generated.at` (write time, UTC) onto conformed
/// frontmatter and serialize it. Runs only on the paths that create a new
/// version row — the idempotent short-circuit above never reaches it, so
/// an unchanged page keeps its original timestamp.
fn stamped_frontmatter(mut conformed: serde_json::Value, now_us: i64) -> StoreResult<String> {
    // The wiki layer stamps (or inherits) `generated.at` before the file
    // is emitted; keep that value so file and row stay byte-aligned. The
    // stamp here covers writers that reach the store without a wiki file.
    if ai_memory_core::okf::generated_at(&conformed).is_none() {
        let ts = jiff::Timestamp::from_microsecond(now_us)
            .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
            .strftime("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        ai_memory_core::okf::stamp_generated_at(&mut conformed, &ts);
    }
    Ok(serde_json::to_string(&conformed)?)
}

/// What a [`backfill_entity_index`] pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EntityBackfillSummary {
    /// Latest pages that gained at least one entity link.
    pub pages_backfilled: u64,
    /// Entity→page links inserted.
    pub links_added: u64,
}

/// One-shot, idempotent backfill of the entity index from existing
/// frontmatter (docs/temporal.md).
///
/// The entity retrieval stream and every `as_of` timeline read from
/// `entities` / `entity_page_links`, which are populated only when a page
/// is written with entities. Historically those came from an LLM
/// consolidator's `entities:` field, absent on the bulk of a real store —
/// bootstrapped pages predate it and stable pages are never
/// re-consolidated — so both features sat empty on mature deployments.
///
/// This derives each latest page's entities from its `tags`/`entities`
/// frontmatter (the exact rule the write path now uses,
/// [`ai_memory_core::frontmatter_entity_names`]) and attaches the links
/// the page version never got, with `valid_from` = the version's
/// `created_at` — the same window semantics the V56 migration used, and
/// honest because those tags have lived in the frontmatter since that
/// version was written.
///
/// Only latest pages that currently have NO entity links are considered,
/// so a store whose pages were written through the fixed path is a cheap
/// no-op and a re-run inserts nothing (the link insert is also
/// `ON CONFLICT DO NOTHING`). Superseded versions are left untouched.
pub fn backfill_entity_index(conn: &mut Connection) -> StoreResult<EntityBackfillSummary> {
    type CandidateRow = (Vec<u8>, Vec<u8>, Vec<u8>, String, i64);
    let tx = conn.transaction()?;
    let mut summary = EntityBackfillSummary::default();
    {
        let rows: Vec<CandidateRow> = {
            let mut stmt = tx.prepare(
                "SELECT p.id, p.workspace_id, p.project_id, p.frontmatter_json, p.created_at \
                 FROM pages p \
                 WHERE p.is_latest = 1 \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM entity_page_links l WHERE l.page_id = p.id \
                   )",
            )?;
            stmt.query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?
            .collect::<Result<_, _>>()?
        };
        for (page_id, ws, proj, fm_str, created_at) in rows {
            let fm: serde_json::Value = serde_json::from_str(&fm_str).unwrap_or_default();
            let names = ai_memory_core::frontmatter_entity_names(&fm);
            if names.is_empty() {
                continue;
            }
            let mut page_gained_a_link = false;
            for name in names {
                let entity_id: Vec<u8> = tx.query_row(
                    "INSERT INTO entities (id, workspace_id, project_id, name, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT (workspace_id, project_id, name) DO UPDATE SET name = excluded.name \
                     RETURNING id",
                    params![EntityId::new().as_bytes(), &ws, &proj, name, created_at],
                    |row| row.get(0),
                )?;
                let inserted = tx.execute(
                    "INSERT INTO entity_page_links (entity_id, page_id, valid_from) \
                     VALUES (?1, ?2, ?3) \
                     ON CONFLICT (entity_id, page_id) DO NOTHING",
                    params![entity_id, &page_id, created_at],
                )?;
                if inserted > 0 {
                    summary.links_added += 1;
                    page_gained_a_link = true;
                }
            }
            if page_gained_a_link {
                summary.pages_backfilled += 1;
            }
        }
    }
    tx.commit()?;
    Ok(summary)
}

pub(crate) fn upsert_page_in_tx(
    tx: &rusqlite::Transaction<'_>,
    page: &NewPage,
    now: i64,
) -> StoreResult<PageId> {
    let path_search = path_search_text(page.path.as_str());
    let body_sha256: [u8; 32] = {
        let mut hasher = Sha256::new();
        hasher.update(page.body.as_bytes());
        hasher.finalize().into()
    };
    // OKF conformance at the single write choke point (docs/okf.md):
    // fill the deterministic keys, compare idempotency modulo the
    // per-version `generated.at`, and stamp `at` only when content
    // actually changed (below).
    let mut conformed = page.frontmatter_json.clone();
    ai_memory_core::okf::conform_frontmatter(page.path.as_str(), &mut conformed);
    let tier_str = page.tier.as_str();

    let existing: Option<ExistingPageVersion> = tx
        .query_row(
            "SELECT id, body_sha256, frontmatter_json, title, tier, pinned FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![
                page.workspace_id.as_bytes(),
                page.project_id.as_bytes(),
                page.path.as_str(),
            ],
            |row| {
                Ok(ExistingPageVersion {
                    id: row.get(0)?,
                    body_sha256: row.get(1)?,
                    frontmatter_json: row.get(2)?,
                    title: row.get(3)?,
                    tier: row.get(4)?,
                    pinned: row.get(5)?,
                })
            },
        )
        .optional()?;

    if let Some(existing) = existing {
        let existing_fm: serde_json::Value =
            serde_json::from_str(&existing.frontmatter_json).unwrap_or_default();
        if existing.body_sha256 == body_sha256
            && ai_memory_core::okf::strip_generated_at(&existing_fm)
                == ai_memory_core::okf::strip_generated_at(&conformed)
            && existing.title == page.title
            && existing.tier == tier_str
            && existing.pinned == i64::from(page.pinned)
        {
            return PageId::from_slice(&existing.id).map_err(StoreError::from);
        }
        let frontmatter_str = stamped_frontmatter(conformed, now)?;
        let new_id = PageId::new();
        tx.execute(
            "UPDATE pages SET is_latest = 0 WHERE id = ?1",
            params![&existing.id],
        )?;
        // Close the superseded version's entity-link windows at the new
        // version's birth instant (docs/temporal.md).
        tx.execute(
            "UPDATE entity_page_links SET superseded_at = ?2 \
             WHERE page_id = ?1 AND superseded_at IS NULL",
            params![&existing.id, now],
        )?;
        tx.execute(
            "INSERT INTO pages \
             (id, workspace_id, project_id, path, path_search, title, tier, body, body_sha256, \
              frontmatter_json, is_latest, supersedes, pinned, author_id, \
              created_at, updated_at, expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11, ?12, ?13, ?14, ?14, ?15)",
            params![
                new_id.as_bytes(),
                page.workspace_id.as_bytes(),
                page.project_id.as_bytes(),
                page.path.as_str(),
                path_search,
                page.title,
                tier_str,
                page.body,
                body_sha256.as_slice(),
                frontmatter_str,
                &existing.id,
                i64::from(page.pinned),
                page.author_id.map(|id| id.as_bytes().to_vec()),
                now,
                page.expires_at.map(|ts| ts.as_microsecond()),
            ],
        )?;
        replace_links_in_tx(tx, &new_id, page)?;
        attach_entities_in_tx(tx, &new_id, page, now)?;
        refresh_incoming_links_for_path(tx, page, &new_id)?;
        audit(
            tx,
            "supersede_page",
            Some(page.workspace_id.as_bytes()),
            Some(page.project_id.as_bytes()),
            Some(new_id.as_bytes()),
            page.author_id
                .as_ref()
                .map(ai_memory_core::UserId::as_bytes),
            now,
        )?;
        return Ok(new_id);
    }
    let frontmatter_str = stamped_frontmatter(conformed, now)?;
    let new_id = PageId::new();
    tx.execute(
        "INSERT INTO pages \
         (id, workspace_id, project_id, path, path_search, title, tier, body, body_sha256, \
          frontmatter_json, is_latest, pinned, author_id, created_at, updated_at, expires_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11, ?12, ?13, ?13, ?14)",
        params![
            new_id.as_bytes(),
            page.workspace_id.as_bytes(),
            page.project_id.as_bytes(),
            page.path.as_str(),
            path_search,
            page.title,
            tier_str,
            page.body,
            body_sha256.as_slice(),
            frontmatter_str,
            i64::from(page.pinned),
            page.author_id.map(|id| id.as_bytes().to_vec()),
            now,
            page.expires_at.map(|ts| ts.as_microsecond()),
        ],
    )?;
    replace_links_in_tx(tx, &new_id, page)?;
    attach_entities_in_tx(tx, &new_id, page, now)?;
    refresh_incoming_links_for_path(tx, page, &new_id)?;
    audit(
        tx,
        "create_page",
        Some(page.workspace_id.as_bytes()),
        Some(page.project_id.as_bytes()),
        Some(new_id.as_bytes()),
        page.author_id
            .as_ref()
            .map(ai_memory_core::UserId::as_bytes),
        now,
    )?;
    Ok(new_id)
}

/// Attach the normalized entity set to a new page version (V38). Entity
/// rows are shared within one project; links stay attached to immutable
/// page versions and latest-page filtering controls retrieval.
fn attach_entities_in_tx(
    tx: &rusqlite::Transaction<'_>,
    page_id: &PageId,
    page: &NewPage,
    version_created_at: i64,
) -> StoreResult<()> {
    if page.entities.is_empty() {
        return Ok(());
    }
    let now = Timestamp::now().as_microsecond();
    // Defence in depth: the wiki layer normalises on the way in, but the
    // store is the last gate before the UNIQUE(name) constraint.
    for name in ai_memory_core::normalize_entities(&page.entities) {
        let entity_id: Vec<u8> = tx.query_row(
            "INSERT INTO entities (id, workspace_id, project_id, name, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (workspace_id, project_id, name) DO UPDATE SET name = excluded.name \
             RETURNING id",
            params![
                EntityId::new().as_bytes(),
                page.workspace_id.as_bytes(),
                page.project_id.as_bytes(),
                name,
                now,
            ],
            |row| row.get(0),
        )?;
        // Bi-temporal-lite (docs/temporal.md): the link's window opens
        // when its page version was created and stays open until the
        // version is superseded (closed in `upsert_page_in_tx`).
        tx.execute(
            "INSERT INTO entity_page_links (entity_id, page_id, valid_from) \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT (entity_id, page_id) DO NOTHING",
            params![entity_id, page_id.as_bytes(), version_created_at],
        )?;
    }
    Ok(())
}

fn replace_links_in_tx(
    tx: &rusqlite::Transaction<'_>,
    from_page_id: &PageId,
    page: &NewPage,
) -> StoreResult<()> {
    tx.execute(
        "DELETE FROM links WHERE from_page_id = ?1",
        params![from_page_id.as_bytes()],
    )?;

    let mut seen = BTreeSet::new();
    for link in &page.links {
        // The relation joins the dedupe key (and the table's PK): a
        // typed edge and a plain reference to the same target are two
        // different rows, mirroring `link_type` in the schema.
        let key = (
            link.workspace.clone(),
            link.project.clone(),
            link.path.as_str().to_string(),
            link.relation,
        );
        if !seen.insert(key) {
            continue;
        }
        let to_page_id = latest_page_id_for_link(tx, page, link)?;
        let to_page_blob = to_page_id.as_ref().map(|id| &id.as_bytes()[..]);
        tx.execute(
            "INSERT INTO links \
                 (from_page_id, to_page_id, to_workspace, to_project, to_path, link_type) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                from_page_id.as_bytes(),
                to_page_blob,
                link.workspace,
                link.project,
                link.path.as_str(),
                link.relation
                    .map_or("references", ai_memory_core::Relation::as_str),
            ],
        )?;
    }
    Ok(())
}

/// Resolve a link target to the latest page id it points at, or `None` if the
/// target workspace / project / page does not exist yet (an unresolved forward
/// link). A bare link resolves within the source page's own project; a
/// `[[project:path]]` / `[[workspace/project:path]]` link resolves against the
/// named project (same workspace when only the project is given).
fn latest_page_id_for_link(
    tx: &rusqlite::Transaction<'_>,
    page: &NewPage,
    link: &LinkTarget,
) -> StoreResult<Option<PageId>> {
    let (workspace_blob, project_blob): (Vec<u8>, Vec<u8>) = match &link.project {
        None => (
            page.workspace_id.as_bytes().to_vec(),
            page.project_id.as_bytes().to_vec(),
        ),
        Some(project_name) => {
            let workspace_blob: Vec<u8> = match &link.workspace {
                None => page.workspace_id.as_bytes().to_vec(),
                Some(workspace_name) => {
                    let found: Option<Vec<u8>> = tx
                        .query_row(
                            "SELECT id FROM workspaces WHERE name = ?1",
                            params![workspace_name],
                            |row| row.get(0),
                        )
                        .optional()?;
                    match found {
                        Some(id) => id,
                        None => return Ok(None),
                    }
                }
            };
            let project_blob: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT id FROM projects WHERE workspace_id = ?1 AND name = ?2",
                    params![workspace_blob, project_name],
                    |row| row.get(0),
                )
                .optional()?;
            match project_blob {
                Some(id) => (workspace_blob, id),
                None => return Ok(None),
            }
        }
    };

    let bytes: Option<Vec<u8>> = tx
        .query_row(
            "SELECT id FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![workspace_blob, project_blob, link.path.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    bytes
        .map(|bytes| PageId::from_slice(&bytes).map_err(StoreError::from))
        .transpose()
}

fn refresh_incoming_links_for_path(
    tx: &rusqlite::Transaction<'_>,
    page: &NewPage,
    latest_page_id: &PageId,
) -> StoreResult<()> {
    // (1) Bare (same-project) links: from_page lives in this page's project and
    // the target carries no scope. Repoints all matches (not only unresolved):
    // a new page version changes the latest id, so resolved links must follow.
    tx.execute(
        "UPDATE links \
         SET to_page_id = ?1 \
         WHERE to_project IS NULL AND to_path = ?2 \
           AND EXISTS ( \
               SELECT 1 FROM pages from_page \
               WHERE from_page.id = links.from_page_id \
                 AND from_page.workspace_id = ?3 \
                 AND from_page.project_id = ?4 \
           )",
        params![
            latest_page_id.as_bytes(),
            page.path.as_str(),
            page.workspace_id.as_bytes(),
            page.project_id.as_bytes(),
        ],
    )?;

    // (2) Cross-project links naming this page's project by name. `to_workspace`
    // may be explicit (cross-workspace) or NULL (same workspace as the source).
    let project_name: Option<String> = tx
        .query_row(
            "SELECT name FROM projects WHERE id = ?1",
            params![page.project_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    let workspace_name: Option<String> = tx
        .query_row(
            "SELECT name FROM workspaces WHERE id = ?1",
            params![page.workspace_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    if let (Some(project_name), Some(workspace_name)) = (project_name, workspace_name) {
        tx.execute(
            "UPDATE links \
             SET to_page_id = ?1 \
             WHERE to_project = ?2 AND to_path = ?3 \
               AND ( \
                   to_workspace = ?4 \
                   OR ( \
                       to_workspace IS NULL \
                       AND EXISTS ( \
                           SELECT 1 FROM pages from_page \
                           WHERE from_page.id = links.from_page_id \
                             AND from_page.workspace_id = ?5 \
                       ) \
                   ) \
               )",
            params![
                latest_page_id.as_bytes(),
                project_name,
                page.path.as_str(),
                workspace_name,
                page.workspace_id.as_bytes(),
            ],
        )?;
    }
    Ok(())
}

/// Begin (or re-affirm) a session row keyed on the caller-supplied id.
/// Idempotent: a second call with the same id leaves the row untouched.
pub fn begin_session(conn: &mut Connection, session: &NewSession) -> StoreResult<()> {
    begin_session_row(conn, session)
}

pub(crate) fn begin_session_in_transaction(
    tx: &Transaction<'_>,
    session: &NewSession,
) -> StoreResult<()> {
    begin_session_row(tx, session)?;
    let matches_receiver: bool = tx.query_row(
        "SELECT EXISTS( \
             SELECT 1 FROM sessions \
             WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3 \
               AND agent_kind = ?4 AND actor_user IS ?5 \
         )",
        params![
            session.id.as_bytes(),
            session.workspace_id.as_bytes(),
            session.project_id.as_bytes(),
            session.agent_kind.as_str(),
            session.actor_user.as_deref(),
        ],
        |row| row.get(0),
    )?;
    if !matches_receiver {
        return Err(StoreError::InvalidState(
            "startup receiver id belongs to a different scope, agent, or operator".into(),
        ));
    }
    Ok(())
}

/// Whether this session was purged and must not be recreated by late-arriving
/// events. Scoped: a purge in one project does not suppress ingest for a
/// session id in another.
fn session_is_purged(conn: &Connection, session: &NewSession) -> StoreResult<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS( \
             SELECT 1 FROM purged_sessions \
             WHERE session_id = ?1 AND workspace_id = ?2 AND project_id = ?3 \
         )",
        params![
            session.id.as_bytes(),
            session.workspace_id.as_bytes(),
            session.project_id.as_bytes(),
        ],
        |row| row.get::<_, i64>(0),
    )? == 1)
}

fn begin_session_row(conn: &Connection, session: &NewSession) -> StoreResult<()> {
    validate_identity_storage_key(session.actor_user.as_deref(), "session owner")?;
    // A purge must be terminal. The events that produced a session can still
    // be sitting undelivered in a client hook spool; without this check the
    // next drain recreates the session row and the observations land again,
    // silently undoing a deletion an application already promised its user
    // (#387). Every ingest path funnels through here, so this is the one
    // place the guard has to live.
    if session_is_purged(conn, session)? {
        return Err(StoreError::SessionPurged(session.id.to_string()));
    }
    let now = Timestamp::now().as_microsecond();
    let agent = session.agent_kind.as_str();
    let cwd: Option<String> = session
        .cwd
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    conn.execute(
        "INSERT INTO sessions \
         (id, workspace_id, project_id, agent_kind, cwd, started_at, actor_user) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(id) DO NOTHING",
        params![
            session.id.as_bytes(),
            session.workspace_id.as_bytes(),
            session.project_id.as_bytes(),
            agent,
            cwd,
            now,
            session.actor_user.as_deref(),
        ],
    )?;
    Ok(())
}

/// Stamp a session as ended, optionally linking the synthesised summary page,
/// and record the observation generation covered by this end.
pub fn end_session(
    conn: &mut Connection,
    session_id: &SessionId,
    summary_page_id: Option<&PageId>,
) -> StoreResult<()> {
    end_session_row(conn, session_id, summary_page_id)
}

/// Atomically end a lifecycle-only session and return its startup handoff to
/// the open pool.
///
/// Only a handoff claimed by this exact receiver session is reopened. The
/// compare-and-set protects a baton that has moved to any other state, while
/// clearing all acceptance metadata makes a later claim indistinguishable
/// from the original open row.
pub fn end_lifecycle_only_session(
    conn: &mut Connection,
    session_id: &SessionId,
) -> StoreResult<LifecycleOnlyEndOutcome> {
    let tx = conn.transaction()?;
    let outcome = end_lifecycle_only_session_in_tx(&tx, session_id)?;
    tx.commit()?;
    Ok(outcome)
}

fn end_lifecycle_only_session_in_tx(
    tx: &Transaction<'_>,
    session_id: &SessionId,
) -> StoreResult<LifecycleOnlyEndOutcome> {
    let has_substantive_observation: bool = tx.query_row(
        "SELECT EXISTS( \
             SELECT 1 FROM observations \
             WHERE session_id = ?1 AND kind NOT IN ('session-start', 'session-end') \
         )",
        params![session_id.as_bytes()],
        |row| row.get(0),
    )?;
    if has_substantive_observation {
        return Ok(LifecycleOnlyEndOutcome::Substantive);
    }
    end_session_row(tx, session_id, None)?;
    let reopened: Option<(Vec<u8>, Vec<u8>, Vec<u8>)> = tx
        .query_row(
            "UPDATE handoffs \
             SET state = 'open', accepted_by = NULL, accepted_at = NULL, \
                 accepted_by_session = NULL, accepted_by_user = NULL \
             WHERE id = ( \
                 SELECT id FROM handoffs \
                 WHERE state = 'accepted' AND accepted_by_session = ?1 \
                 ORDER BY accepted_at DESC, created_at DESC LIMIT 1 \
             ) AND state = 'accepted' AND accepted_by_session = ?1 \
             RETURNING id, workspace_id, project_id",
            params![session_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let reopened = reopened
        .map(|(id, workspace_id, project_id)| -> StoreResult<HandoffId> {
            let handoff_id = HandoffId::from_slice(&id)?;
            let workspace_id = WorkspaceId::from_slice(&workspace_id)?;
            let project_id = ProjectId::from_slice(&project_id)?;
            audit(
                tx,
                "release_lifecycle_only_handoff",
                Some(workspace_id.as_bytes()),
                Some(project_id.as_bytes()),
                None,
                None,
                Timestamp::now().as_microsecond(),
            )?;
            Ok(handoff_id)
        })
        .transpose()?;
    Ok(LifecycleOnlyEndOutcome::Ended {
        reopened_handoff: reopened,
    })
}

fn end_session_row(
    conn: &Connection,
    session_id: &SessionId,
    summary_page_id: Option<&PageId>,
) -> StoreResult<()> {
    let now = Timestamp::now().as_microsecond();
    let page_blob: Option<&[u8]> = summary_page_id.map(|p| &p.as_bytes()[..]);
    conn.execute(
        "UPDATE sessions \
         SET ended_at = ?1, summary_page_id = ?2, \
             ended_observation_count = (\
                 SELECT COUNT(*) FROM observations WHERE session_id = ?3\
             ) \
         WHERE id = ?3",
        params![now, page_blob, session_id.as_bytes()],
    )?;
    Ok(())
}

/// Append a single observation. Caller is expected to have already
/// inserted the parent session via [`begin_session`].
pub fn insert_observation(
    conn: &mut Connection,
    obs: &NewObservation,
) -> StoreResult<ObservationId> {
    insert_observation_row(conn, obs)
}

/// Ingest-keys older than this are swept opportunistically on every keyed
/// insert. Keys only need to outlive the client spool (7 days + retry
/// backoff); 30 days is a generous margin.
const INGEST_KEY_TTL_MICROS: i64 = 30 * 24 * 60 * 60 * 1_000_000;

/// Claim a project-scoped ingest key and append its observation atomically.
///
/// The key claim and the observation row commit in ONE transaction: either
/// both land or neither does, so a crash cannot claim a key without its
/// observation. A claimed-but-incomplete key tells the ingest path to resume
/// downstream wiki/handoff effects without inserting another observation. A
/// completed key means the whole event can be acknowledged and skipped.
pub fn insert_observation_keyed(
    conn: &mut Connection,
    obs: &NewObservation,
    ingest_key: &str,
) -> StoreResult<IngestObservationOutcome> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;

    // Sweep before looking up the current key so an expired key can be reused
    // even when no unrelated keyed event arrived in the meantime.
    tx.execute(
        "DELETE FROM ingest_keys WHERE seen_at < ?1",
        params![now - INGEST_KEY_TTL_MICROS],
    )?;
    let existing: Option<Option<i64>> = tx
        .query_row(
            "SELECT completed_at FROM ingest_keys \
             WHERE project_id = ?1 AND key = ?2",
            params![obs.project_id.as_bytes(), ingest_key],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(completed_at) = existing {
        tx.commit()?;
        return Ok(if completed_at.is_some() {
            IngestObservationOutcome::AlreadyComplete
        } else {
            IngestObservationOutcome::ResumePending
        });
    }

    tx.execute(
        "INSERT INTO ingest_keys (project_id, key, seen_at, completed_at) \
         VALUES (?1, ?2, ?3, NULL)",
        params![obs.project_id.as_bytes(), ingest_key, now],
    )?;
    let id = insert_observation_row(&tx, obs)?;
    tx.commit()?;
    Ok(IngestObservationOutcome::Inserted(id))
}

/// Find or create the hook session, validate its immutable tuple and owner,
/// optionally claim an ingest key, and append the observation in one writer
/// transaction. Validation always precedes key mutation.
pub fn admit_hook_session_event(
    conn: &mut Connection,
    session: &NewSession,
    obs: &NewObservation,
    owner_filter: &OwnerFilter,
    ingest_key: Option<&str>,
) -> StoreResult<HookSessionAdmission> {
    if obs.session_id != session.id
        || obs.workspace_id != session.workspace_id
        || obs.project_id != session.project_id
    {
        return Err(StoreError::InvalidState(
            "hook observation does not match its session tuple".into(),
        ));
    }
    let session_end = obs.kind == ObservationKind::SessionEnd;
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    type Row = (Vec<u8>, Vec<u8>, String, Option<String>, Option<i64>, u64);
    let existing: Option<Row> = tx.query_row(
        "SELECT workspace_id, project_id, agent_kind, actor_user, ended_at, ended_observation_count FROM sessions WHERE id = ?1",
        params![session.id.as_bytes()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
    ).optional()?;
    let (owner, ended_at, ended_count) = match existing {
        Some((ws, project, agent, owner, ended_at, ended_count)) => {
            // Corrupt owners fail closed, including for Any recovery. Owner
            // and agent identify WHO the session belongs to, so a mismatch
            // there is a genuine UUID collision and is terminal.
            if owner
                .as_deref()
                .is_some_and(|value| IdentityKey::from_storage_key(value).is_none())
                || agent != session.agent_kind.as_str()
                || !owner_filter.admits(owner.as_deref())
            {
                return Err(StoreError::SessionCollision);
            }
            // Scope is NOT identity. Under the default `follow-cwd` routing a
            // mid-session `cd` into another project legitimately resolves this
            // event to a different (workspace, project) than the session row,
            // and the observation belongs in the project it names — that is
            // exactly the record-splitting `[routing] mid_session = "sticky"`
            // exists to opt out of. Treating the difference as a collision
            // silently DROPPED those events instead of recording them.
            //
            // A terminal event is the one exception: an end naming a different
            // scope is not this session's end, so it is dropped rather than
            // ending someone else's session (the pre-guard
            // `SessionEndDisposition::DropInvalid` arm).
            let scoped_to_session = ws.as_slice() == session.workspace_id.as_bytes()
                && project.as_slice() == session.project_id.as_bytes();
            if session_end && !scoped_to_session {
                tx.commit()?;
                return Ok(HookSessionAdmission::InvalidScopedEnd);
            }
            (owner, ended_at, ended_count)
        }
        None if session_end => {
            tx.commit()?;
            return Ok(HookSessionAdmission::InvalidMissingEnd);
        }
        None => {
            validate_identity_storage_key(session.actor_user.as_deref(), "session owner")?;
            if !owner_filter.admits(session.actor_user.as_deref()) {
                return Err(StoreError::SessionCollision);
            }
            tx.execute(
                "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at, actor_user) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![session.id.as_bytes(), session.workspace_id.as_bytes(), session.project_id.as_bytes(), session.agent_kind.as_str(), session.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()), now, session.actor_user.as_deref()],
            )?;
            (session.actor_user.clone(), None, 0)
        }
    };
    let guard = AdmittedSession {
        session_id: session.id,
        workspace_id: session.workspace_id,
        project_id: session.project_id,
        agent_kind: session.agent_kind,
        owner,
    };
    if session_end && ended_at.is_some() {
        let count: u64 = tx.query_row(
            "SELECT COUNT(*) FROM observations WHERE session_id = ?1",
            params![session.id.as_bytes()],
            |r| r.get(0),
        )?;
        if count <= ended_count {
            tx.commit()?;
            return Ok(HookSessionAdmission::AlreadyEnded { session: guard });
        }
    }
    let ingest = if let Some(key) = ingest_key {
        tx.execute(
            "DELETE FROM ingest_keys WHERE seen_at < ?1",
            params![now - INGEST_KEY_TTL_MICROS],
        )?;
        let old: Option<Option<i64>> = tx
            .query_row(
                "SELECT completed_at FROM ingest_keys WHERE project_id = ?1 AND key = ?2",
                params![obs.project_id.as_bytes(), key],
                |r| r.get(0),
            )
            .optional()?;
        match old {
            Some(done) => {
                if done.is_some() {
                    IngestObservationOutcome::AlreadyComplete
                } else {
                    IngestObservationOutcome::ResumePending
                }
            }
            None => {
                tx.execute("INSERT INTO ingest_keys (project_id, key, seen_at, completed_at) VALUES (?1, ?2, ?3, NULL)", params![obs.project_id.as_bytes(), key, now])?;
                IngestObservationOutcome::Inserted(insert_observation_row(&tx, obs)?)
            }
        }
    } else {
        IngestObservationOutcome::Inserted(insert_observation_row(&tx, obs)?)
    };
    tx.commit()?;
    if !session_end {
        Ok(HookSessionAdmission::Observation {
            session: guard,
            ingest,
        })
    } else if ended_at.is_some() {
        Ok(HookSessionAdmission::ReEnd {
            session: guard,
            ingest,
        })
    } else {
        Ok(HookSessionAdmission::EndOpen {
            session: guard,
            ingest,
        })
    }
}

fn validate_admitted_session(tx: &Transaction<'_>, admitted: &AdmittedSession) -> StoreResult<()> {
    let owner: Option<String> = tx.query_row(
        "SELECT actor_user FROM sessions WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3 AND agent_kind = ?4",
        params![admitted.session_id.as_bytes(), admitted.workspace_id.as_bytes(), admitted.project_id.as_bytes(), admitted.agent_kind.as_str()],
        |r| r.get(0),
    ).optional()?.ok_or(StoreError::SessionCollision)?;
    if owner != admitted.owner
        || owner
            .as_deref()
            .is_some_and(|v| IdentityKey::from_storage_key(v).is_none())
    {
        return Err(StoreError::SessionCollision);
    }
    Ok(())
}

/// Guarded hook equivalent of [`end_session`].
pub fn end_admitted_session(
    conn: &mut Connection,
    admitted: &AdmittedSession,
    page: Option<&PageId>,
) -> StoreResult<()> {
    let tx = conn.transaction()?;
    validate_admitted_session(&tx, admitted)?;
    end_session_row(&tx, &admitted.session_id, page)?;
    tx.commit()?;
    Ok(())
}

/// Guarded hook lifecycle-only end.
pub fn end_admitted_lifecycle_only_session(
    conn: &mut Connection,
    admitted: &AdmittedSession,
) -> StoreResult<LifecycleOnlyEndOutcome> {
    let tx = conn.transaction()?;
    validate_admitted_session(&tx, admitted)?;
    let outcome = end_lifecycle_only_session_in_tx(&tx, &admitted.session_id)?;
    tx.commit()?;
    Ok(outcome)
}

/// Guarded hook end plus automatic handoff in one transaction.
pub fn end_admitted_session_with_handoff(
    conn: &mut Connection,
    admitted: &AdmittedSession,
    page: Option<&PageId>,
    handoff: &NewHandoff,
) -> StoreResult<HandoffId> {
    if handoff.from_session_id != Some(admitted.session_id)
        || handoff.workspace_id != admitted.workspace_id
        || handoff.project_id != admitted.project_id
        || handoff.owner_user != admitted.owner
    {
        return Err(StoreError::SessionCollision);
    }
    let tx = conn.transaction()?;
    validate_admitted_session(&tx, admitted)?;
    end_session_row(&tx, &admitted.session_id, page)?;
    let id = insert_handoff_row(&tx, handoff)?;
    tx.commit()?;
    Ok(id)
}

/// Mark a keyed hook event complete after its downstream effects finish.
///
/// This transition is idempotent so two resumed processors can converge on the
/// same completed key without turning a successful delivery into an error.
pub fn complete_observation_ingest(
    conn: &mut Connection,
    project_id: &ProjectId,
    ingest_key: &str,
) -> StoreResult<()> {
    if !complete_observation_ingest_if_claimed(conn, project_id, ingest_key)? {
        return Err(StoreError::InvalidState(
            "cannot complete an ingest key that was not claimed".into(),
        ));
    }
    Ok(())
}

/// Mark a keyed hook event complete when its observation claim exists.
///
/// Recovery paths use the boolean result to distinguish a pending native-hook
/// replay from an unkeyed or unrelated duplicate SessionEnd.
pub fn complete_observation_ingest_if_claimed(
    conn: &mut Connection,
    project_id: &ProjectId,
    ingest_key: &str,
) -> StoreResult<bool> {
    let completed_at = Timestamp::now().as_microsecond();
    let matched = conn.execute(
        "UPDATE ingest_keys \
         SET completed_at = COALESCE(completed_at, ?1) \
         WHERE project_id = ?2 AND key = ?3",
        params![completed_at, project_id.as_bytes(), ingest_key],
    )?;
    Ok(matched != 0)
}

/// The observation INSERT itself, shared by the plain and keyed paths
/// (`&Connection` so it also runs inside a [`rusqlite::Transaction`]).
fn insert_observation_row(conn: &Connection, obs: &NewObservation) -> StoreResult<ObservationId> {
    let id = ObservationId::new();
    let now = Timestamp::now().as_microsecond();
    let kind = observation_kind_as_str(obs.kind);
    let importance: i64 = i64::from(obs.importance.clamp(1, 10));
    let (extension, source_event) = match (&obs.extension, &obs.source_event) {
        (Some(extension), Some(source_event)) => {
            (Some(extension.as_str()), Some(source_event.as_str()))
        }
        _ => (None, None),
    };
    conn.execute(
        "INSERT INTO observations \
         (id, session_id, workspace_id, project_id, kind, extension, source_event, title, body, \
          importance, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            id.as_bytes(),
            obs.session_id.as_bytes(),
            obs.workspace_id.as_bytes(),
            obs.project_id.as_bytes(),
            kind,
            extension,
            source_event,
            obs.title,
            obs.body,
            importance,
            now,
        ],
    )?;
    Ok(id)
}

/// Store / replace one page's embedding. Bytes are the host-endian
/// `f32` packing of the unit-normalised vector. Provider/model/dim
/// are denormalised onto the row so a single SELECT can detect
/// heterogeneity (refuse-on-mismatch path).
/// Why an embed attempt produced no embedding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedOutcome {
    /// The embedding provider returned an error.
    Failed,
    /// The page could not be read from the wiki.
    Unreadable,
    /// The page body was empty after trimming, so there was nothing to embed.
    /// Not an error, but permanent until the body changes — and previously
    /// indistinguishable from an idle pass.
    SkippedEmpty,
}

impl EmbedOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Unreadable => "unreadable",
            Self::SkippedEmpty => "skipped_empty",
        }
    }
}

/// Longest `detail` we keep. A provider error can carry a whole response body;
/// the ledger exists to say what happened, not to archive it.
const EMBED_FAILURE_DETAIL_MAX: usize = 500;

/// Record that an embed attempt on `page_id` did not produce an embedding.
///
/// Failure-only by design: a success writes nothing, because the existence of
/// a `page_embeddings` row already records it. That keeps the common path free
/// of an extra write and means a global `embed --force` cannot erase the
/// history here — it never touches this table (#528).
///
/// Upserts on `page_id`, so the row is always the most recent unsuccessful
/// attempt. It is deliberately left in place after a later pass succeeds: join
/// against `page_embeddings` to tell "still unresolved" from "recovered".
///
/// # Errors
/// Propagates SQL errors.
pub fn record_embed_failure(
    conn: &Connection,
    page_id: &PageId,
    outcome: EmbedOutcome,
    detail: Option<&str>,
) -> StoreResult<()> {
    let detail = detail.map(|d| {
        let mut truncated: String = d.chars().take(EMBED_FAILURE_DETAIL_MAX).collect();
        if truncated.chars().count() < d.chars().count() {
            truncated.push('…');
        }
        truncated
    });
    conn.execute(
        "INSERT INTO page_embed_failures (page_id, at, outcome, detail) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(page_id) DO UPDATE SET \
             at = excluded.at, outcome = excluded.outcome, detail = excluded.detail",
        params![
            page_id.as_bytes(),
            Timestamp::now().as_microsecond(),
            outcome.as_str(),
            detail,
        ],
    )?;
    Ok(())
}

pub fn store_embedding(
    conn: &mut Connection,
    page_id: &PageId,
    vector_bytes: &[u8],
    provider: &str,
    model: &str,
    dim: u32,
) -> StoreResult<()> {
    let now = Timestamp::now().as_microsecond();
    conn.execute(
        "INSERT INTO page_embeddings (page_id, vector, provider, model, dim, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT(page_id) DO UPDATE SET \
             vector = excluded.vector, \
             provider = excluded.provider, \
             model = excluded.model, \
             dim = excluded.dim, \
             created_at = excluded.created_at",
        params![page_id.as_bytes(), vector_bytes, provider, model, dim, now,],
    )?;
    Ok(())
}

/// Store / replace a batch of page embeddings in one transaction.
pub fn store_embeddings(conn: &mut Connection, embeddings: &[EmbeddingWrite]) -> StoreResult<()> {
    if embeddings.is_empty() {
        return Ok(());
    }
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO page_embeddings (page_id, vector, provider, model, dim, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(page_id) DO UPDATE SET \
                 vector = excluded.vector, \
                 provider = excluded.provider, \
                 model = excluded.model, \
                 dim = excluded.dim, \
                 created_at = excluded.created_at",
        )?;
        for embedding in embeddings {
            stmt.execute(params![
                embedding.page_id.as_bytes(),
                embedding.vector_bytes.as_slice(),
                embedding.provider.as_str(),
                embedding.model.as_str(),
                embedding.dim,
                now,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Fold per-client MCP tool-call deltas into their `(client, day)`
/// buckets. UPSERT per bucket inside one transaction: the buffer layer
/// in the MCP server coalesces a minute of calls into a handful of
/// entries, so this stays a few tiny rows per flush regardless of
/// traffic.
pub(crate) fn bump_client_activity(
    conn: &mut Connection,
    entries: &[(String, i64, u32, u32)],
) -> StoreResult<()> {
    if entries.is_empty() {
        return Ok(());
    }
    for (client, _, _, _) in entries {
        if !is_normalized_client_activity_label(client) {
            return Err(StoreError::InvalidState(
                "client activity label is not normalized".into(),
            ));
        }
    }
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO client_activity (client, day, reads, writes) \
             VALUES ( \
                 CASE \
                     WHEN ?1 = ?5 \
                       OR EXISTS ( \
                           SELECT 1 FROM client_activity \
                           WHERE client = ?1 AND day = ?2 \
                       ) \
                       OR ( \
                           SELECT COUNT(*) FROM client_activity \
                           WHERE day = ?2 AND client <> ?5 \
                       ) < ?6 \
                     THEN ?1 \
                     ELSE ?5 \
                 END, \
                 ?2, ?3, ?4 \
             ) \
             ON CONFLICT(client, day) DO UPDATE SET \
                 reads = CASE \
                     WHEN reads > 9223372036854775807 - excluded.reads \
                     THEN 9223372036854775807 \
                     ELSE reads + excluded.reads \
                 END, \
                 writes = CASE \
                     WHEN writes > 9223372036854775807 - excluded.writes \
                     THEN 9223372036854775807 \
                     ELSE writes + excluded.writes \
                 END",
        )?;
        for (client, day, reads, writes) in entries {
            if *reads == 0 && *writes == 0 {
                continue;
            }
            stmt.execute(params![
                client,
                day,
                reads,
                writes,
                crate::CLIENT_ACTIVITY_OVERFLOW_CLIENT,
                crate::CLIENT_ACTIVITY_MAX_CLIENTS_PER_DAY,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn is_normalized_client_activity_label(client: &str) -> bool {
    if client.is_empty()
        || client != client.trim()
        || client.chars().count() > crate::CLIENT_ACTIVITY_MAX_NAME_CHARS
        || client.chars().any(|c| c.is_control() || is_bidi_control(c))
    {
        return false;
    }

    let mut previous_space = false;
    for c in client.chars() {
        if c.is_whitespace() {
            if c != ' ' || previous_space {
                return false;
            }
            previous_space = true;
        } else {
            previous_space = false;
        }
    }
    true
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

/// Bump `access_count` + `last_accessed_at` for the pages whose ids
/// appear in `page_ids`. Idempotent for unknown ids (no-op).
/// Used by the read path to feed the M8 reinforcement term.
///
/// Bump shared access counters and record an optional typed operator as a
/// distinct reader. Both mutations commit in the same transaction.
pub fn bump_access_for_pages_for_actor(
    conn: &mut Connection,
    page_ids: &[PageId],
    actor: Option<&IdentityKey>,
) -> StoreResult<()> {
    if page_ids.is_empty() {
        return Ok(());
    }
    if actor.is_some_and(|identity| !identity.is_valid()) {
        return Err(StoreError::InvalidState(
            "page access actor is not a normalized identity".into(),
        ));
    }
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "UPDATE pages \
             SET access_count = access_count + 1, last_accessed_at = ?1 \
             WHERE id = ?2 AND is_latest = 1",
        )?;
        for id in page_ids {
            stmt.execute(params![now, id.as_bytes()])?;
        }
        // Per-operator reinforcement, recorded in the SAME transaction so the
        // scalar and the breakdown cannot drift. Purely additive: the scalar
        // above is what the retention formula still reads by default.
        if let Some(actor) = actor {
            let actor = actor.storage_key();
            // The `WHERE EXISTS` guard is what keeps the documented idempotence
            // for unknown ids: `page_access.page_id` REFERENCES `pages(id)` and
            // the writer connection runs with `foreign_keys` ON, so a bare
            // INSERT for a page deleted between the search and this (detached,
            // post-response) bump would raise a FK violation and roll back the
            // WHOLE batch — costing every other page in the result set its
            // reinforcement. `INSERT OR IGNORE` does not help: it suppresses
            // constraint conflicts, not FK violations. The predicate mirrors the
            // UPDATE above so the scalar and the breakdown cannot drift.
            let mut per_actor = tx.prepare(
                "INSERT INTO page_access (page_id, actor) \
                 SELECT ?1, ?2 \
                 WHERE EXISTS (SELECT 1 FROM pages WHERE id = ?1 AND is_latest = 1) \
                 ON CONFLICT(page_id, actor) DO NOTHING",
            )?;
            for id in page_ids {
                per_actor.execute(params![id.as_bytes(), actor])?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// Record one explicit feedback signal against the latest version of a
/// page and update its derived salience, in a single transaction.
///
/// `page_feedback` is the append-only source of truth; `pages.salience`
/// is the derived value the decay formula reads. Returns the page id and
/// the salience after the update, or `None` when the path has no latest
/// version in that scope (idempotent no-op for a deleted page).
#[allow(clippy::too_many_arguments)]
pub fn record_page_feedback(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    path: &PagePath,
    kind: ai_memory_core::FeedbackKind,
    reason: Option<&str>,
    author_id: Option<ai_memory_core::UserId>,
    params: &crate::decay::DecayParams,
) -> StoreResult<Option<(PageId, f64)>> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let existing: Option<(Vec<u8>, Option<f64>)> = tx
        .query_row(
            "SELECT id, salience FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                path.as_str()
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((page_id_bytes, current_salience)) = existing else {
        return Ok(None);
    };
    let page_id = PageId::from_slice(&page_id_bytes)?;
    let next_salience = crate::decay::salience_after_feedback(params, current_salience, kind);

    tx.execute(
        "INSERT INTO page_feedback \
         (id, page_id, workspace_id, project_id, kind, reason, salience_after, author_id, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            ai_memory_core::PageFeedbackId::new().as_bytes(),
            page_id.as_bytes(),
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            kind.as_str(),
            reason,
            next_salience,
            author_id.map(|id| id.as_bytes().to_vec()),
            now,
        ],
    )?;
    // Salience rides on the page *version*: a later supersession starts
    // from salience_default again, which is intentional — feedback is a
    // judgement on the content that was read, not on the path forever.
    tx.execute(
        "UPDATE pages SET salience = ?1 WHERE id = ?2",
        params![next_salience, page_id.as_bytes()],
    )?;
    audit(
        &tx,
        "page_feedback",
        Some(workspace_id.as_bytes()),
        Some(project_id.as_bytes()),
        Some(page_id.as_bytes()),
        author_id.as_ref().map(ai_memory_core::UserId::as_bytes),
        now,
    )?;
    tx.commit()?;
    Ok(Some((page_id, next_salience)))
}

/// Mark the expected latest page as evicted by the forget sweep.
///
/// The full identity and latest-id check share one transaction so a stale
/// sweep candidate cannot evict a page that was rewritten after selection.
/// `superseded_at` is the decay-tombstone marker; unlike `supersedes`, it is
/// never populated by ordinary page versioning.
pub fn soft_delete_for_decay_if_latest(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    path: &PagePath,
    expected_latest_id: PageId,
) -> StoreResult<bool> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let affected = tx.execute(
        "UPDATE pages \
         SET is_latest = 0, superseded_at = ?1 \
         WHERE id = ?2 \
           AND workspace_id = ?3 \
           AND project_id = ?4 \
           AND path = ?5 \
           AND is_latest = 1",
        params![
            now,
            expected_latest_id.as_bytes(),
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            path.as_str(),
        ],
    )?;
    if affected != 0 {
        // Retirement is supersession for the entity timeline too: an
        // open window on a tombstoned page made `as_of` resurrect
        // retired knowledge forever (post-audit finding).
        tx.execute(
            "UPDATE entity_page_links SET superseded_at = ?1 \
             WHERE page_id = ?2 AND superseded_at IS NULL",
            params![now, expected_latest_id.as_bytes()],
        )?;
        audit(
            &tx,
            "soft_delete_for_decay",
            Some(workspace_id.as_bytes()),
            Some(project_id.as_bytes()),
            Some(expected_latest_id.as_bytes()),
            // Decay is a scheduled/admin system operation rather than a
            // user-attributable page edit.
            None,
            now,
        )?;
    }
    tx.commit()?;
    Ok(affected != 0)
}

/// Delete every version of a page (by path) from the index. Used when the
/// wiki file is removed (`Wiki::delete_page`): the watcher does not handle
/// file deletions, so the derived rows must be dropped explicitly or the
/// page keeps surfacing in search/recent with stale content. FK cascades
/// drop outgoing links + embeddings; the `pages_fts_ad` trigger keeps FTS in
/// sync; incoming links are set to NULL (unresolved). Idempotent.
pub fn delete_page(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    path: &PagePath,
    author_id: Option<ai_memory_core::UserId>,
) -> StoreResult<()> {
    delete_page_inner(conn, workspace_id, project_id, path, None, author_id).map(|_| ())
}

/// Delete every version of `path` only when `expected_latest_id` is still its
/// latest version. The comparison and deletion share one transaction so a
/// stale retention candidate cannot remove a page that was refreshed later.
pub fn delete_page_if_latest(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    path: &PagePath,
    expected_latest_id: PageId,
    author_id: Option<ai_memory_core::UserId>,
) -> StoreResult<bool> {
    delete_page_inner(
        conn,
        workspace_id,
        project_id,
        path,
        Some(expected_latest_id),
        author_id,
    )
}

fn delete_page_inner(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    path: &PagePath,
    expected_latest_id: Option<PageId>,
    author_id: Option<ai_memory_core::UserId>,
) -> StoreResult<bool> {
    let tx = conn.transaction()?;
    // Capture the latest page id BEFORE the delete so the audit row can point
    // at it. None when the page is absent (delete is an idempotent no-op).
    let page_id: Option<[u8; 16]> = tx
        .query_row(
            "SELECT id FROM pages WHERE workspace_id = ?1 AND project_id = ?2 \
             AND path = ?3 AND is_latest = 1 LIMIT 1",
            params![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                path.as_str()
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?
        .and_then(|v| <[u8; 16]>::try_from(v.as_slice()).ok());
    if expected_latest_id
        .as_ref()
        .is_some_and(|expected| page_id.as_ref() != Some(expected.as_bytes()))
    {
        return Ok(false);
    }
    let rows = tx.execute(
        "DELETE FROM pages WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3",
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            path.as_str()
        ],
    )?;
    if rows > 0 {
        tx.execute(
            "DELETE FROM entities \
             WHERE workspace_id = ?1 AND project_id = ?2 \
               AND NOT EXISTS ( \
                   SELECT 1 FROM entity_page_links l WHERE l.entity_id = entities.id \
               )",
            params![workspace_id.as_bytes(), project_id.as_bytes()],
        )?;
    }
    // Only audit a real deletion — a no-op delete (0 rows) writes nothing, so
    // the trail isn't polluted with idempotent misses.
    if rows > 0 {
        audit(
            &tx,
            "delete_page",
            Some(workspace_id.as_bytes()),
            Some(project_id.as_bytes()),
            page_id.as_ref(),
            author_id.as_ref().map(ai_memory_core::UserId::as_bytes),
            Timestamp::now().as_microsecond(),
        )?;
    }
    tx.commit()?;
    Ok(rows > 0)
}

/// Permanently delete one eligible decay tombstone and its ancestry chain.
///
/// The tombstone id, full path identity, cutoff, and current latest-page state
/// are all rechecked in the transaction. This lets the wiki layer remove the
/// authoritative file only when the path has no newer live page, while still
/// allowing an old chain to be cleaned after that path was deliberately
/// recreated. Ordinary supersession rows have `superseded_at IS NULL` and
/// cannot become roots of this deletion.
pub fn hard_delete_decayed_page_chain(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    path: &PagePath,
    tombstone_id: PageId,
    expected_latest_id: Option<PageId>,
    cutoff_us: i64,
) -> StoreResult<usize> {
    let tx = conn.transaction()?;
    let current_latest: Option<Vec<u8>> = tx
        .query_row(
            "SELECT id FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                path.as_str()
            ],
            |row| row.get(0),
        )
        .optional()?;
    let expected_latest = expected_latest_id.map(|id| id.as_bytes().to_vec());
    if current_latest != expected_latest {
        return Ok(0);
    }
    let n = tx.execute(
        "WITH RECURSIVE decay_chain(id) AS ( \
             SELECT id FROM pages \
             WHERE id = ?1 \
               AND workspace_id = ?2 \
               AND project_id = ?3 \
               AND path = ?4 \
               AND is_latest = 0 \
               AND superseded_at IS NOT NULL \
               AND superseded_at <= ?5 \
             UNION \
             SELECT parent.id \
             FROM decay_chain c \
             JOIN pages child ON child.id = c.id \
             JOIN pages parent ON parent.id = child.supersedes \
             WHERE parent.workspace_id = ?2 \
               AND parent.project_id = ?3 \
               AND parent.path = ?4 \
         ) \
         DELETE FROM pages WHERE id IN (SELECT id FROM decay_chain)",
        params![
            tombstone_id.as_bytes(),
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            path.as_str(),
            cutoff_us,
        ],
    )?;
    if n > 0 {
        tx.execute(
            "DELETE FROM entities \
             WHERE workspace_id = ?1 AND project_id = ?2 \
               AND NOT EXISTS ( \
                   SELECT 1 FROM entity_page_links l WHERE l.entity_id = entities.id \
               )",
            params![workspace_id.as_bytes(), project_id.as_bytes()],
        )?;
        audit(
            &tx,
            "hard_delete_decayed",
            Some(workspace_id.as_bytes()),
            Some(project_id.as_bytes()),
            Some(tombstone_id.as_bytes()),
            None,
            Timestamp::now().as_microsecond(),
        )?;
    }
    tx.commit()?;
    Ok(n)
}

/// One batch of an observation prune, as reported by
/// [`prune_consolidated_observations`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ObservationPruneOutcome {
    /// Observation rows permanently deleted by this batch.
    pub deleted: usize,
    /// Distinct sessions that lost rows in this batch. Returned rather than
    /// counted so a caller looping over batches can deduplicate a session whose
    /// rows straddle a batch boundary.
    pub sessions_touched: Vec<SessionId>,
    /// Sessions whose `ended_observation_count` watermark was lowered back onto
    /// the surviving row count.
    pub sessions_resynced: usize,
}

/// Permanently delete one bounded batch of raw observations belonging to
/// sessions that were already distilled into a live summary page.
///
/// Raw capture is the store's growth driver, but it is also the only input
/// consolidation, auto-improvement and the `raw_hits` fallback have. Deleting
/// it is safe exactly when its durable distillation still exists, so the
/// predicate joins through `sessions.summary_page_id` — the marker
/// [`end_session_row`] writes — and requires the page it points at to still be
/// live. `superseded_at IS NOT NULL` means decay already evicted that page, so
/// its session's raw capture is the last copy and must stay; a page hard-deleted
/// by the same sweep is excluded for free, because the `ON DELETE SET NULL`
/// foreign key has already cleared the pointer. A session with no `sessions`
/// row, or one that never ended, matches nothing and can never be pruned.
///
/// `ORDER BY created_at` makes the batching deterministic: a run interrupted
/// halfway leaves a clean age boundary rather than holes, so the next run
/// resumes on a store whose invariant ("everything older than X is gone for
/// consolidated sessions") is still one comparison.
///
/// The watermark resync is not optional. `session_end_disposition` compares a
/// live `COUNT(*)` against the persisted `sessions.ended_observation_count`;
/// leaving the watermark above reality would make a resumed session's genuinely
/// new work read as `AlreadyEnded` and never be re-consolidated. The
/// `ended_observation_count > (…)` guard keeps the repair idempotent and
/// one-directional — it can only lower a watermark that is now above reality.
/// Bounding it to the ids the delete returned also repairs sessions whose row
/// lives in a different scope than the observations being pruned.
pub fn prune_consolidated_observations(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    cutoff_us: i64,
    batch: usize,
) -> StoreResult<ObservationPruneOutcome> {
    if batch == 0 {
        return Ok(ObservationPruneOutcome::default());
    }
    let tx = conn.transaction()?;
    let mut deleted = 0usize;
    let mut sessions: BTreeSet<Vec<u8>> = BTreeSet::new();
    {
        let mut stmt = tx.prepare(
            "DELETE FROM observations \
             WHERE id IN ( \
                 SELECT o.id FROM observations o \
                 WHERE o.workspace_id = ?1 \
                   AND o.project_id = ?2 \
                   AND o.created_at < ?3 \
                   AND EXISTS ( \
                       SELECT 1 FROM sessions s \
                       JOIN pages p ON p.id = s.summary_page_id \
                       WHERE s.id = o.session_id \
                         AND p.superseded_at IS NULL \
                   ) \
                 ORDER BY o.created_at \
                 LIMIT ?4 \
             ) \
             RETURNING session_id",
        )?;
        let rows = stmt.query_map(
            params![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                cutoff_us,
                u64::try_from(batch).unwrap_or(u64::MAX),
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )?;
        for row in rows {
            sessions.insert(row?);
            deleted += 1;
        }
    }
    let mut sessions_touched = Vec::with_capacity(sessions.len());
    let mut sessions_resynced = 0usize;
    if deleted > 0 {
        for session_id in &sessions {
            sessions_touched.push(SessionId::from_slice(session_id)?);
            sessions_resynced += tx.execute(
                "UPDATE sessions \
                 SET ended_observation_count = ( \
                         SELECT COUNT(*) FROM observations o WHERE o.session_id = sessions.id \
                     ) \
                 WHERE id = ?1 \
                   AND ended_observation_count > ( \
                         SELECT COUNT(*) FROM observations o WHERE o.session_id = sessions.id \
                     )",
                params![session_id],
            )?;
        }
        audit_with_detail(
            &tx,
            "prune_observations",
            Some(workspace_id.as_bytes()),
            Some(project_id.as_bytes()),
            None,
            None,
            Timestamp::now().as_microsecond(),
            &format!("{{\"deleted\":{deleted}}}"),
        )?;
    }
    tx.commit()?;
    Ok(ObservationPruneOutcome {
        deleted,
        sessions_touched,
        sessions_resynced,
    })
}

/// Insert a new handoff in state=open.
pub fn insert_handoff(conn: &mut Connection, h: &NewHandoff) -> StoreResult<HandoffId> {
    let tx = conn.transaction()?;
    let id = insert_handoff_row(&tx, h)?;
    tx.commit()?;
    Ok(id)
}

/// Atomically stamp a session ended and insert its automatic handoff.
///
/// A failed handoff insert rolls the end stamp back, so a keyed retry can run
/// the complete SessionEnd path instead of observing a partially ended session.
pub fn end_session_with_handoff(
    conn: &mut Connection,
    session_id: &SessionId,
    summary_page_id: Option<&PageId>,
    handoff: &NewHandoff,
) -> StoreResult<HandoffId> {
    if handoff.from_session_id.as_ref() != Some(session_id) {
        return Err(StoreError::InvalidState(
            "automatic handoff source does not match the ended session".into(),
        ));
    }
    let tx = conn.transaction()?;
    let session_scope: Option<(Vec<u8>, Vec<u8>, Option<String>)> = tx
        .query_row(
            "SELECT workspace_id, project_id, actor_user FROM sessions WHERE id = ?1",
            params![session_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((workspace_id, project_id, actor_user)) = session_scope else {
        return Err(StoreError::InvalidState(
            "cannot end a missing session with an automatic handoff".into(),
        ));
    };
    if workspace_id.as_slice() != handoff.workspace_id.as_bytes()
        || project_id.as_slice() != handoff.project_id.as_bytes()
    {
        return Err(StoreError::InvalidState(
            "automatic handoff scope does not match the ended session".into(),
        ));
    }
    if actor_user != handoff.owner_user {
        return Err(StoreError::InvalidState(
            "automatic handoff owner does not match the ended session".into(),
        ));
    }
    end_session_row(&tx, session_id, summary_page_id)?;
    let id = insert_handoff_row(&tx, handoff)?;
    tx.commit()?;
    Ok(id)
}

/// Store-boundary bound for a single handoff prose field. Generous
/// (16 KiB, matching the observation body) — a backstop against
/// unbounded content, not a functional cap; callers cap tighter.
const HANDOFF_FIELD_MAX_BYTES: usize = 16 * 1024;
/// Store-boundary bound on the number of items in a handoff list field.
const HANDOFF_LIST_MAX_ITEMS: usize = 256;

fn bound_handoff_field(value: &str) -> String {
    ai_memory_core::truncate_utf8_bytes(value, HANDOFF_FIELD_MAX_BYTES)
}

fn bound_handoff_list(items: &[String]) -> Vec<String> {
    items
        .iter()
        .take(HANDOFF_LIST_MAX_ITEMS)
        .map(|s| bound_handoff_field(s))
        .collect()
}

fn insert_handoff_row(conn: &Transaction<'_>, h: &NewHandoff) -> StoreResult<HandoffId> {
    validate_identity_storage_key(h.owner_user.as_deref(), "handoff owner")?;
    let id = HandoffId::new();
    let now = Timestamp::now().as_microsecond();
    // Store-boundary bound (defense in depth): the MCP/hook callers already
    // scrub and cap handoff prose, but the store is the last gate before
    // durable persistence — mirror the observation body's bound so a caller
    // that ever forgets cannot write unbounded content to the DB. Generous
    // enough never to fire under the callers' tighter caps.
    let summary = bound_handoff_field(&h.summary);
    let open_q = serde_json::to_string(&bound_handoff_list(&h.open_questions))?;
    let next_s = serde_json::to_string(&bound_handoff_list(&h.next_steps))?;
    let files = serde_json::to_string(&bound_handoff_list(&h.files_touched))?;
    let from_session: Option<&[u8]> = h.from_session_id.as_ref().map(|s| &s.as_bytes()[..]);
    // Normalize the stored cwd: strip trailing path separators (keep a bare root
    // as "/"). The hook extractor preserves whatever the agent payload sent,
    // so this single write point guarantees a consistent stored form for both
    // manual and auto (SessionEnd) handoffs, keeping the next session's
    // path-boundary match robust to trailing slash/backslash drift.
    let cwd: Option<String> = h.cwd.as_ref().map(|p| {
        let s = p.to_string_lossy();
        let trimmed = s.trim_end_matches(['/', '\\']);
        if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        }
    });
    let from_agent = h.from_agent.as_str();
    let to_agent = h.to_agent.map(AgentKind::as_str);
    // A newer automatic handoff from the exact same cwd is the only one that
    // can ever win there, even before a SessionStart occurs. Bound abandoned
    // same-directory sessions without touching deliberate manual handoffs or
    // independent parent/sibling cwd scopes — and without crossing an operator
    // boundary: the same directory inside a shared container is the norm, so
    // owner equality is the only thing keeping one operator's SessionEnd from
    // retiring another's pending baton.
    if from_session.is_some() {
        let expired = conn.execute(
            "UPDATE handoffs SET state = 'expired' \
             WHERE workspace_id = ?1 AND project_id = ?2 \
               AND state = 'open' AND from_session_id IS NOT NULL \
               AND (cwd = ?3 OR (cwd IS NULL AND ?3 IS NULL)) \
               AND owner_user IS ?4",
            params![
                h.workspace_id.as_bytes(),
                h.project_id.as_bytes(),
                cwd,
                h.owner_user.as_deref()
            ],
        )?;
        if expired > 0 {
            audit(
                conn,
                "expire_superseded_handoffs",
                Some(h.workspace_id.as_bytes()),
                Some(h.project_id.as_bytes()),
                None,
                None,
                now,
            )?;
        }
    }
    // Insert + audit atomically. Handoffs are keyed by agent/session, not a DB
    // user, so the audit author is NULL — the row records the lifecycle event
    // (op + workspace/project + time), not an operator identity.
    conn.execute(
        "INSERT INTO handoffs \
         (id, workspace_id, project_id, from_session_id, from_agent, to_agent, cwd, summary, \
          open_questions, next_steps, files_touched, state, created_at, owner_user) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'open', ?12, ?13)",
        params![
            id.as_bytes(),
            h.workspace_id.as_bytes(),
            h.project_id.as_bytes(),
            from_session,
            from_agent,
            to_agent,
            cwd,
            summary,
            open_q,
            next_s,
            files,
            now,
            // NULL owner = shared with the whole project: what a caller with no
            // actor writes, and what every pre-V39 row already looks like.
            h.owner_user.as_deref(),
        ],
    )?;
    audit(
        conn,
        "insert_handoff",
        Some(h.workspace_id.as_bytes()),
        Some(h.project_id.as_bytes()),
        None,
        None,
        now,
    )?;
    Ok(id)
}

/// Mark a handoff accepted by `accepting_agent` / `accepting_session`.
///
/// Returns whether this call is the one that actually claimed it: `false` means
/// the row was already accepted/expired, or its owner does not admit this
/// caller. Callers must not hand the body to the agent on `false`, or a lost
/// race delivers the same baton twice.
///
/// `receiving_cwd` is where the *claiming* session is starting, not where the
/// claimed handoff came from; it bounds the post-claim sweep of stale automatic
/// handoffs.
pub fn accept_handoff(conn: &mut Connection, acceptance: &HandoffAcceptance) -> StoreResult<bool> {
    let tx = conn.transaction()?;
    let claimed = accept_handoff_in_transaction(&tx, acceptance)?;
    tx.commit()?;
    Ok(claimed)
}

pub(crate) fn accept_handoff_in_transaction(
    tx: &Transaction<'_>,
    acceptance: &HandoffAcceptance,
) -> StoreResult<bool> {
    let HandoffAcceptance {
        handoff_id,
        workspace_id,
        project_id,
        accepting_agent,
        accepting_session,
        accepting_user,
        owner_filter,
        receiving_cwd,
    } = acceptance;
    let accepting_user = accepting_user.as_deref();
    validate_identity_storage_key(accepting_user, "accepting handoff operator")?;
    match (owner_filter, accepting_user) {
        (OwnerFilter::User(expected), Some(actual)) if expected != actual => {
            return Err(StoreError::InvalidState(
                "accepting handoff operator does not match its owner filter".into(),
            ));
        }
        (OwnerFilter::User(_), None) => {
            return Err(StoreError::InvalidState(
                "an owner-scoped handoff claim requires an accepting operator".into(),
            ));
        }
        (OwnerFilter::Unattributed, Some(_)) => {
            return Err(StoreError::InvalidState(
                "an unattributed handoff claim cannot record a named operator".into(),
            ));
        }
        _ => {}
    }
    let now = Timestamp::now().as_microsecond();
    let agent = accepting_agent.as_str();
    let session: Option<&[u8]> = accepting_session.as_ref().map(|s| &s.as_bytes()[..]);
    if let Some(accepting_session) = accepting_session {
        let receiver: Option<(bool, bool)> = tx
            .query_row(
                "SELECT workspace_id = ?2 AND project_id = ?3 AND agent_kind = ?4, \
                        ended_at IS NULL \
                 FROM sessions WHERE id = ?1",
                params![
                    accepting_session.as_bytes(),
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    accepting_agent.as_str(),
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((matching_scope_and_agent, open)) = receiver else {
            return Err(StoreError::InvalidState(
                "handoff receiver session does not exist".into(),
            ));
        };
        if !matching_scope_and_agent {
            return Err(StoreError::InvalidState(
                "handoff receiver session does not match the accepting scope and agent".into(),
            ));
        }
        if !open {
            return Err(StoreError::InvalidState(
                "an ended session cannot accept a handoff".into(),
            ));
        }
        let already_claimed: bool = tx.query_row(
            "SELECT EXISTS( \
                 SELECT 1 FROM handoffs \
                 WHERE state = 'accepted' AND accepted_by_session = ?1 \
             )",
            params![accepting_session.as_bytes()],
            |row| row.get(0),
        )?;
        if already_claimed {
            // SessionStart can be retried. One receiver must never consume a
            // second baton, or an empty end could return only one and strand
            // the first accepted row.
            return Ok(false);
        }
    }
    let metadata = tx
        .query_row(
            "SELECT from_session_id IS NOT NULL, cwd, created_at, owner_user \
             FROM handoffs \
             WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3 AND state = 'open'",
            params![
                handoff_id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
            |row| {
                Ok((
                    row.get::<_, bool>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((automatic, cwd, created_at, owner_user)) = metadata else {
        return Ok(false);
    };
    // The ownership check rides along in the UPDATE's WHERE rather than being a
    // separate read: the claim stays a single atomic compare-and-set (only one
    // racing session can flip 'open' -> 'accepted'), and a caller who is not
    // allowed to take this baton simply changes 0 rows.
    let owner_clause = match owner_filter {
        OwnerFilter::Any => "",
        OwnerFilter::User(_) => " AND (owner_user IS NULL OR owner_user = ?8)",
        OwnerFilter::Unattributed => " AND owner_user IS NULL",
    };
    let sql = format!(
        "UPDATE handoffs SET state = 'accepted', accepted_by = ?1, accepted_at = ?2, \
         accepted_by_session = ?3, accepted_by_user = ?4 \
         WHERE id = ?5 AND workspace_id = ?6 AND project_id = ?7 \
           AND state = 'open'{owner_clause}"
    );
    let changed = match owner_filter {
        OwnerFilter::User(user) => tx.execute(
            &sql,
            params![
                agent,
                now,
                session,
                accepting_user,
                handoff_id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                user
            ],
        )?,
        _ => tx.execute(
            &sql,
            params![
                agent,
                now,
                session,
                accepting_user,
                handoff_id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
        )?,
    };
    // Only a real state transition ('open' -> 'accepted') is audited.
    if changed > 0 {
        audit(
            tx,
            "accept_handoff",
            Some(workspace_id.as_bytes()),
            Some(project_id.as_bytes()),
            None,
            None,
            now,
        )?;
        if automatic {
            // The sweep inherits the claimed handoff's owner: retiring stale
            // batons must never reach across an operator boundary, or one
            // person's session start silently destroys another's pending
            // handoff — a worse failure than the misdelivery ownership exists
            // to prevent, because nothing is delivered at all.
            let expired = expire_superseded_auto_handoffs(
                tx,
                handoff_id,
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                cwd.as_deref(),
                created_at,
                receiving_cwd.as_deref(),
                owner_user.as_deref(),
            )?;
            if expired > 0 {
                audit(
                    tx,
                    "expire_superseded_handoffs",
                    Some(workspace_id.as_bytes()),
                    Some(project_id.as_bytes()),
                    None,
                    None,
                    now,
                )?;
            }
        }
    }
    Ok(changed > 0)
}

fn validate_identity_storage_key(value: Option<&str>, label: &str) -> StoreResult<()> {
    if value.is_some_and(|value| IdentityKey::from_storage_key(value).is_none()) {
        return Err(StoreError::InvalidState(format!(
            "{label} is not a qualified identity storage key"
        )));
    }
    Ok(())
}

/// Expire older automatic handoffs that were eligible for the same receiving
/// cwd as the accepted handoff. Manual and sibling-directory handoffs are
/// deliberately excluded, and so is every handoff belonging to a different
/// operator: a NULL owner is visible to *everyone*, so expiring one on Alice's
/// behalf would take it away from Bob too. Equality on `owner_user` keeps the
/// unattributed single-operator case (every row NULL) behaving exactly as it
/// does without ownership.
#[allow(clippy::too_many_arguments)]
fn expire_superseded_auto_handoffs(
    tx: &Transaction<'_>,
    accepted_id: &HandoffId,
    workspace_id: &[u8],
    project_id: &[u8],
    accepted_cwd: Option<&str>,
    accepted_created_at: i64,
    receiving_cwd: Option<&str>,
    accepted_owner: Option<&str>,
) -> StoreResult<usize> {
    let receiving_cwd = receiving_cwd.or(accepted_cwd);
    let accepted_key = crate::reader::handoff_selection_key(
        false,
        accepted_created_at,
        accepted_cwd,
        *accepted_id,
    );
    let mut stmt = tx.prepare(
        "SELECT id, cwd, created_at FROM handoffs \
         WHERE workspace_id = ?1 AND project_id = ?2 \
           AND state = 'open' AND from_session_id IS NOT NULL \
           AND owner_user IS ?3",
    )?;
    let rows = stmt.query_map(params![workspace_id, project_id, accepted_owner], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut ids = Vec::new();
    for row in rows {
        let (id_bytes, cwd, created_at) = row?;
        let id = HandoffId::from_slice(&id_bytes)?;
        if crate::reader::auto_handoff_matches_cwd(cwd.as_deref(), receiving_cwd)
            && crate::reader::handoff_selection_key(false, created_at, cwd.as_deref(), id)
                < accepted_key
        {
            ids.push(id_bytes);
        }
    }
    drop(stmt);

    let mut expired = 0;
    for chunk in ids.chunks(500) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        expired += tx.execute(
            &format!(
                "UPDATE handoffs SET state = 'expired' \
                 WHERE state = 'open' AND id IN ({placeholders})"
            ),
            rusqlite::params_from_iter(chunk.iter().map(Vec::as_slice)),
        )?;
    }
    Ok(expired)
}

/// Expire every open handoff in one scope, optionally only those older than
/// `older_than_us`. Returns how many rows changed.
///
/// # Why this ignores the automatic expiry's exemptions
///
/// [`expire_superseded_auto_handoffs`] deliberately spares two kinds of open
/// handoff: manual ones (`from_session_id IS NULL`, written by a person on
/// purpose) and ones whose `cwd` does not match the accepting session's. Those
/// exemptions are correct for an automatic sweep triggered by an unrelated
/// accept.
///
/// They also mean the leftover backlog consists, by construction, of exactly
/// the handoffs the sweep will never touch. An operator-driven bulk expiry
/// that honoured the same exemptions would therefore expire nothing at all —
/// it would be a no-op for the only situation it exists to resolve. So this
/// applies to every open handoff in scope, which is what an operator asking to
/// clear a backlog is asking for.
///
/// Owner scoping still applies, and lives inside the `UPDATE` rather than a
/// preceding read, so expiring another user's baton is a zero-row no-op rather
/// than a read-then-write race — the same shape [`cancel_handoff`] uses.
///
/// This is a state change, not a delete: the summary, provenance and
/// timestamps survive. An expired handoff simply stops being consumable.
///
/// # Errors
/// Propagates SQL errors.
pub fn expire_open_handoffs(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    owner_filter: &OwnerFilter,
    older_than_us: Option<i64>,
    author_id: Option<ai_memory_core::UserId>,
) -> StoreResult<u64> {
    let tx = conn.transaction()?;
    let owner_clause = match owner_filter {
        OwnerFilter::Any => "",
        OwnerFilter::User(_) => " AND (owner_user IS NULL OR owner_user = :owner)",
        OwnerFilter::Unattributed => " AND owner_user IS NULL",
    };
    let age_clause = if older_than_us.is_some() {
        " AND created_at <= :cutoff"
    } else {
        ""
    };
    let sql = format!(
        "UPDATE handoffs SET state = 'expired' \
         WHERE workspace_id = :ws AND project_id = :proj \
           AND state = 'open'{owner_clause}{age_clause}"
    );

    let ws_bytes: Vec<u8> = workspace_id.as_bytes().to_vec();
    let proj_bytes: Vec<u8> = project_id.as_bytes().to_vec();
    let mut params: Vec<(&str, &dyn rusqlite::ToSql)> = vec![
        (":ws", &ws_bytes as &dyn rusqlite::ToSql),
        (":proj", &proj_bytes as &dyn rusqlite::ToSql),
    ];
    let owner_value;
    if let OwnerFilter::User(user) = owner_filter {
        owner_value = user.clone();
        params.push((":owner", &owner_value as &dyn rusqlite::ToSql));
    }
    let cutoff_value;
    if let Some(cutoff) = older_than_us {
        cutoff_value = cutoff;
        params.push((":cutoff", &cutoff_value as &dyn rusqlite::ToSql));
    }

    let changed = tx.execute(&sql, params.as_slice())? as u64;

    if changed > 0 {
        audit_with_detail(
            &tx,
            "expire_open_handoffs",
            Some(workspace_id.as_bytes()),
            Some(project_id.as_bytes()),
            None,
            author_id.as_ref().map(ai_memory_core::UserId::as_bytes),
            Timestamp::now().as_microsecond(),
            &format!("{{\"expired\":{changed}}}"),
        )?;
    }
    tx.commit()?;
    Ok(changed)
}

/// Mark an open handoff expired so it will no longer be consumed.
pub fn cancel_handoff(
    conn: &mut Connection,
    handoff_id: &HandoffId,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    owner_filter: &OwnerFilter,
) -> StoreResult<bool> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    // Same reasoning as the accept path: the owner predicate lives in the
    // UPDATE so cancelling somebody else's baton is a 0-row no-op instead of a
    // read-then-write race.
    let owner_clause = match owner_filter {
        OwnerFilter::Any => "",
        OwnerFilter::User(_) => " AND (owner_user IS NULL OR owner_user = ?4)",
        OwnerFilter::Unattributed => " AND owner_user IS NULL",
    };
    let sql = format!(
        "UPDATE handoffs SET state = 'expired' \
         WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3 \
           AND state = 'open'{owner_clause}"
    );
    let changed = match owner_filter {
        OwnerFilter::User(user) => tx.execute(
            &sql,
            params![
                handoff_id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                user,
            ],
        )?,
        _ => tx.execute(
            &sql,
            params![
                handoff_id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
        )?,
    };
    if changed > 0 {
        audit(
            &tx,
            "cancel_handoff",
            Some(workspace_id.as_bytes()),
            Some(project_id.as_bytes()),
            None,
            None,
            now,
        )?;
    }
    tx.commit()?;
    Ok(changed > 0)
}

fn observation_kind_as_str(kind: ObservationKind) -> &'static str {
    kind.as_str()
}

fn audit(
    tx: &rusqlite::Transaction<'_>,
    op: &str,
    workspace_id: Option<&[u8; 16]>,
    project_id: Option<&[u8; 16]>,
    page_id: Option<&[u8; 16]>,
    author_id: Option<&[u8; 16]>,
    at: i64,
) -> StoreResult<()> {
    audit_with_detail(
        tx,
        op,
        workspace_id,
        project_id,
        page_id,
        author_id,
        at,
        "{}",
    )
}

/// `audit`, but with a caller-supplied JSON `detail` payload. Exists so a
/// destructive batch can record what it did (e.g. the observation prune's
/// deleted-row count) instead of leaving a gap only inferable from timestamps.
#[allow(clippy::too_many_arguments)]
fn audit_with_detail(
    tx: &rusqlite::Transaction<'_>,
    op: &str,
    workspace_id: Option<&[u8; 16]>,
    project_id: Option<&[u8; 16]>,
    page_id: Option<&[u8; 16]>,
    author_id: Option<&[u8; 16]>,
    at: i64,
    detail: &str,
) -> StoreResult<()> {
    tx.execute(
        "INSERT INTO audit_log (at, op, workspace_id, project_id, page_id, author_id, detail) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            at,
            op,
            workspace_id.map(|b| &b[..]),
            project_id.map(|b| &b[..]),
            page_id.map(|b| &b[..]),
            author_id.map(|b| &b[..]),
            detail,
        ],
    )?;
    Ok(())
}

/// Retro-fit sessions + observations to per-cwd projects and graveyard
/// any `is_latest=1` pages (which are mash-ups across the old single-project
/// bucket). Executes atomically in one transaction.
///
/// `plan` contains `(session_id, new_project_id)` pairs. Sessions not in
/// the plan are left untouched. Pages are graveyarded unconditionally so a
/// fresh consolidation can regenerate clean per-project pages.
pub fn reorg_sessions(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    plan: &[(SessionId, ProjectId)],
) -> StoreResult<ReorgSummary> {
    if plan.is_empty() {
        return Ok(ReorgSummary::default());
    }
    let tx = conn.transaction()?;
    let mut sessions_moved = 0usize;
    let mut observations_updated = 0usize;
    for (session_id, new_project_id) in plan {
        let rows = tx.execute(
            "UPDATE sessions
             SET project_id = ?1
             WHERE id = ?2 AND workspace_id = ?3 AND project_id != ?1",
            params![
                new_project_id.as_bytes(),
                session_id.as_bytes(),
                workspace_id.as_bytes()
            ],
        )?;
        sessions_moved += rows;
        // Update observations whose session_id matches, keeping project_id
        // in sync with the session row we just moved.
        let obs_rows = tx.execute(
            "UPDATE observations SET project_id = ?1 WHERE session_id = ?2 AND workspace_id = ?3",
            params![
                new_project_id.as_bytes(),
                session_id.as_bytes(),
                workspace_id.as_bytes()
            ],
        )?;
        observations_updated += obs_rows;
    }
    // Graveyard only this workspace's latest pages; sibling workspaces may
    // have already-consolidated pages that must remain current.
    tx.execute(
        "UPDATE entity_page_links SET superseded_at = ?2 \
         WHERE superseded_at IS NULL AND page_id IN ( \
             SELECT id FROM pages WHERE workspace_id = ?1 AND is_latest = 1)",
        params![workspace_id.as_bytes(), Timestamp::now().as_microsecond()],
    )?;
    let pages_graveyarded: usize = tx.execute(
        "UPDATE pages SET is_latest = 0 WHERE workspace_id = ?1 AND is_latest = 1",
        params![workspace_id.as_bytes()],
    )?;
    tx.commit()?;
    Ok(ReorgSummary {
        sessions_moved,
        observations_updated,
        pages_graveyarded,
    })
}

/// Rename a project within its workspace.
///
/// Only the `name` column is updated — all pages, sessions, observations,
/// and handoffs remain associated with the same `project_id`. No files
/// move on disk (the wiki is flat: every page from every project lives
/// under `wiki/`; only the `project_id` foreign key distinguishes them).
///
/// # Errors
/// - [`StoreError::InvalidProjectName`] when `new_name` is empty,
///   contains a `/` character, or is all whitespace.
/// - [`StoreError::ProjectNameTaken`] when a project with `new_name`
///   already exists in the same workspace.
/// - [`StoreError::Sqlite`] on any other SQL failure.
pub fn rename_project(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    new_name: &str,
    author_id: Option<ai_memory_core::UserId>,
) -> StoreResult<()> {
    let trimmed = new_name.trim();
    if trimmed.is_empty() {
        return Err(StoreError::InvalidProjectName(
            "project name must not be empty or all whitespace".into(),
        ));
    }
    if trimmed.contains('/') {
        return Err(StoreError::InvalidProjectName(
            "project name must not contain '/' (it appears in URL paths)".into(),
        ));
    }

    // Wrap the UPDATE + audit row in one transaction so the trail can never
    // diverge from the rename it records (on any error the tx drops without
    // commit, rolling both back).
    let tx = conn.transaction()?;
    let rows = tx.execute(
        "UPDATE projects SET name = ?1 WHERE id = ?2 AND workspace_id = ?3",
        params![trimmed, project_id.as_bytes(), workspace_id.as_bytes()],
    );

    match rows {
        // Zero rows affected means the project row vanished between the
        // admin handler's `lookup_ws_proj_no_create` and this UPDATE —
        // the classic shape is a concurrent `purge-project` racing the
        // rename. Without this check, the rename would happily return
        // `Ok(())` and the admin handler would respond `200 OK` for an
        // operation that touched nothing, contradicting the purge's
        // (also `200 OK`) destruction of the same row.
        Ok(0) => Err(StoreError::NotFound(format!(
            "project id {project_id} no longer exists in workspace {workspace_id} \
             (race with concurrent purge or delete)",
        ))),
        Ok(_) => {
            audit(
                &tx,
                "rename_project",
                Some(workspace_id.as_bytes()),
                Some(project_id.as_bytes()),
                None,
                author_id.as_ref().map(ai_memory_core::UserId::as_bytes),
                Timestamp::now().as_microsecond(),
            )?;
            tx.commit()?;
            Ok(())
        }
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                || err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Err(StoreError::ProjectNameTaken(trimmed.to_string()))
        }
        Err(e) => Err(StoreError::Sqlite(e)),
    }
}

/// Record a successfully-applied wiki-structure migration.
///
/// Uses `INSERT OR IGNORE` so re-running the same name is a no-op
/// (idempotent by design — the runner already skips known names, but
/// this guards against any concurrent writes).
pub fn insert_wiki_migration(
    conn: &mut Connection,
    name: &str,
    applied_at: i64,
) -> StoreResult<()> {
    conn.execute(
        "INSERT OR IGNORE INTO wiki_migrations (name, applied_at) VALUES (?1, ?2)",
        params![name, applied_at],
    )?;
    Ok(())
}

/// Whether a destructive scope operation should reclaim the freed bytes
/// afterwards. Shared by [`purge_session`], [`purge_project`] and
/// [`delete_workspace`] so the family makes one promise, not three.
///
/// A purge is a logical delete: the rows are gone and nothing can reach them
/// through the API or through search, but their bytes remain in free pages of
/// the database file until the file is rewritten — exactly as for any other
/// SQLite delete.
///
/// [`Compaction::Reclaim`] additionally rebuilds the affected FTS5 indexes
/// (the tokens survive an ordinary delete inside the index segments) and runs
/// `VACUUM`, after the deletion has committed.
///
/// # Cost and limits
///
/// `VACUUM` rewrites the entire database and needs free disk space of roughly
/// the database's own size. On a multi-gigabyte store this is minutes, not
/// milliseconds, so it is opt-in rather than the default.
///
/// It also does **not** make the content forensically unrecoverable. The wiki
/// git repository keeps page text in its objects and commit messages, and any
/// backup taken before the purge still contains it. Reclaiming bytes from the
/// live SQLite file is worth doing on its own terms; it is not a guarantee of
/// erasure everywhere, and must not be described as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compaction {
    /// Leave the freed pages alone. The default, and what #387 promises.
    Skip,
    /// Rebuild the affected FTS indexes and `VACUUM` after committing.
    Reclaim,
}

/// Every FTS5 index in the schema, which is what [`reclaim_freed_pages`]
/// rebuilds.
///
/// Named here rather than spelled inside the SQL so the "every" is checkable:
/// `reclaim_rebuilds_every_fts_index_in_the_schema` compares this list against
/// `sqlite_master` and fails when a new index is added without a decision
/// being made about it. A list inside a string literal cannot be compared to
/// anything, and the failure mode of getting it wrong is silent — text stays
/// searchable after an operator asked for it to be reclaimed.
const ALL_FTS_INDEXES: &[&str] = &["observations_fts", "pages_fts", "workstream_events_fts"];

/// Rebuild every FTS5 index, then `VACUUM`. Runs after the caller's
/// transaction has committed: `VACUUM` cannot run inside one, and a rebuild is
/// a whole-index operation we do not want holding the write lock for the
/// duration of a delete.
///
/// All three indexes are rebuilt regardless of which scope was deleted. They
/// are `content=`-backed external-content tables, so an ordinary delete leaves
/// the tokens behind inside the index segments and only a `rebuild` clears
/// them — and a project or workspace delete cascades `workstream_events` just
/// as it does `pages` and `observations`. Rebuilding only the indexes a given
/// caller "should" have touched is exactly the reasoning that leaves text
/// searchable after a purge, and the cost is dominated by the `VACUUM` that
/// follows in every case. A `rebuild` is idempotent, so the extra work is a
/// no-op for a scope that deleted nothing from that table.
pub(crate) fn reclaim_freed_pages(conn: &Connection) -> StoreResult<()> {
    let mut batch = String::new();
    for index in ALL_FTS_INDEXES {
        // Compile-time constants, never caller input; FTS5 has no
        // bind-parameter form for the rebuild statement.
        batch.push_str(&format!("INSERT INTO {index}({index}) VALUES('rebuild'); "));
    }
    batch.push_str("VACUUM;");
    conn.execute_batch(&batch)?;
    Ok(())
}

/// What one [`compact`] run reclaimed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CompactSummary {
    /// Database size in bytes before the rebuild + `VACUUM`.
    pub bytes_before: u64,
    /// Database size in bytes afterwards.
    pub bytes_after: u64,
}

impl CompactSummary {
    /// Bytes returned to the filesystem. Saturating: a `VACUUM` can leave the
    /// file marginally larger when defragmentation costs more than the free
    /// pages it released, and that is a zero, not a negative.
    #[must_use]
    pub fn bytes_reclaimed(&self) -> u64 {
        self.bytes_before.saturating_sub(self.bytes_after)
    }
}

/// Reclaim free pages on demand, deleting nothing.
///
/// Same operation the destructive commands offer as `--compact`, reachable
/// without first deleting something. Until this existed, an operator whose
/// database had accumulated free pages — a large forget-sweep, a retention
/// pass, months of superseded page versions — had no way to return the space
/// except by purging data they wanted to keep.
///
/// # Cost
///
/// `VACUUM` takes an exclusive lock and rewrites the whole file, so every
/// write blocks for the duration and it needs free disk space of roughly the
/// database's own size. Read [`crate::reader::StorageStatus::reclaimable_bytes`]
/// first and run this when it is worth the stall — not on a blind schedule.
///
/// # Errors
/// Propagates the SQL error from the rebuild or the `VACUUM`.
pub fn compact(conn: &mut Connection) -> StoreResult<CompactSummary> {
    let bytes_before = database_bytes(conn)?;
    reclaim_freed_pages(conn)?;
    let bytes_after = database_bytes(conn)?;
    Ok(CompactSummary {
        bytes_before,
        bytes_after,
    })
}

/// `page_count * page_size` — the database's own view of its size, which is
/// what `VACUUM` acts on. Deliberately not the filesystem size: a `-wal`
/// sidecar holds committed pages not yet checkpointed into the main file, so
/// `metadata().len()` would move for reasons compaction has nothing to do with.
pub(crate) fn database_bytes(conn: &Connection) -> StoreResult<u64> {
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    Ok((page_count.max(0) as u64).saturating_mul(page_size.max(0) as u64))
}

/// What [`purge_session`] removed. All counts are rows actually deleted.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PurgeSessionSummary {
    /// `observations` rows removed.
    pub observations_deleted: u64,
    /// `handoffs` rows removed — only those this session *authored*.
    pub handoffs_deleted: u64,
    /// `pages` rows removed, counting every version in the supersession chain.
    pub pages_deleted: u64,
    /// `auto_improve_runs` rows removed.
    pub auto_improve_runs_deleted: u64,
    /// On-disk wiki paths whose rows are gone, for the caller to unlink.
    pub removed_paths: Vec<String>,
    /// Whether the freed bytes were reclaimed (`VACUUM` ran).
    pub compacted: bool,
}

/// Delete one session and everything addressable from it, within one scope.
///
/// # Scope containment
///
/// This is a destructive operation driven by a caller-supplied id, so the
/// scope is enforced structurally rather than trusted:
///
/// - the session must exist **and** belong to `(workspace_id, project_id)`;
///   a session id from another scope is [`StoreError::NotFound`] and nothing
///   is deleted;
/// - every statement is additionally filtered on `workspace_id` and
///   `project_id` wherever the table carries them, so even a mismatched id
///   cannot reach a row in another workspace or project;
/// - derived pages are deleted by **id**, from a set collected and
///   scope-checked first — never by path. Two projects may hold the same
///   `sessions/<uuid>.md` path, and a hand-written page can occupy the path a
///   session later claims; deleting by path would take those with it.
///
/// # Ordering
///
/// `auto_improve_runs.session_id`, `handoffs.from_session_id`,
/// `handoffs.accepted_by_session` and `pages.supersedes` are all
/// `ON DELETE SET NULL`. Cutting the session first nulls the pointers needed
/// to find what it produced, so every target is collected before anything is
/// deleted.
///
/// # What is deliberately *not* deleted
///
/// Handoffs this session **accepted** but did not author. Their summary text
/// belongs to the authoring session; accepting one does not transfer
/// ownership, and removing it would destroy another session's record. Those
/// rows keep their content and lose only the `accepted_by_session` pointer,
/// via the existing `ON DELETE SET NULL`.
pub fn purge_session(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    session_id: SessionId,
    author_id: Option<ai_memory_core::UserId>,
    compaction: Compaction,
) -> StoreResult<PurgeSessionSummary> {
    let wid = workspace_id.as_bytes();
    let pid = project_id.as_bytes();
    let sid = session_id.as_bytes();
    let tx = conn.transaction()?;

    // The session must live in this scope. A mismatch is NotFound, not a
    // silent no-op: the caller asked to delete something and must learn that
    // it did not happen.
    let in_scope: i64 = tx.query_row(
        "SELECT COUNT(*) FROM sessions \
         WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3",
        rusqlite::params![&sid[..], &wid[..], &pid[..]],
        |row| row.get(0),
    )?;
    if in_scope == 0 {
        return Err(StoreError::NotFound(format!(
            "session {session_id} not found in this workspace/project"
        )));
    }

    // ---- collect, before anything is cut ----

    // The derived page and every version of it. Walk from the recorded
    // summary page across the supersession chain, staying inside the scope.
    let page_ids: Vec<Vec<u8>> = {
        let mut stmt = tx.prepare(
            "WITH RECURSIVE chain(id) AS ( \
                 SELECT summary_page_id FROM sessions \
                  WHERE id = ?1 AND summary_page_id IS NOT NULL \
                 UNION \
                 SELECT p.id FROM pages p JOIN chain c ON p.supersedes = c.id \
             ) \
             SELECT p.id FROM pages p JOIN chain c ON p.id = c.id \
              WHERE p.workspace_id = ?2 AND p.project_id = ?3",
        )?;
        stmt.query_map(rusqlite::params![&sid[..], &wid[..], &pid[..]], |row| {
            row.get::<_, Vec<u8>>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let removed_paths: Vec<String> = if page_ids.is_empty() {
        Vec::new()
    } else {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT path FROM pages \
              WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3",
        )?;
        let mut paths = Vec::new();
        for id in &page_ids {
            let found: Option<String> = stmt
                .query_row(rusqlite::params![&id[..], &wid[..], &pid[..]], |row| {
                    row.get(0)
                })
                .optional()?;
            if let Some(path) = found
                && !paths.contains(&path)
            {
                paths.push(path);
            }
        }
        paths
    };

    let run_ids: Vec<Vec<u8>> = {
        let mut stmt = tx.prepare(
            "SELECT id FROM auto_improve_runs \
              WHERE session_id = ?1 AND workspace_id = ?2 AND project_id = ?3",
        )?;
        stmt.query_map(rusqlite::params![&sid[..], &wid[..], &pid[..]], |row| {
            row.get::<_, Vec<u8>>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };

    // ---- delete, innermost first ----

    for run in &run_ids {
        tx.execute(
            "DELETE FROM auto_improve_rejections \
              WHERE source_run_id = ?1 AND workspace_id = ?2 AND project_id = ?3",
            rusqlite::params![&run[..], &wid[..], &pid[..]],
        )?;
    }
    let auto_improve_runs_deleted = tx.execute(
        "DELETE FROM auto_improve_runs \
          WHERE session_id = ?1 AND workspace_id = ?2 AND project_id = ?3",
        rusqlite::params![&sid[..], &wid[..], &pid[..]],
    )? as u64;

    // Authored only. See the note above about accepted handoffs.
    let handoffs_deleted = tx.execute(
        "DELETE FROM handoffs \
          WHERE from_session_id = ?1 AND workspace_id = ?2 AND project_id = ?3",
        rusqlite::params![&sid[..], &wid[..], &pid[..]],
    )? as u64;

    // Pages by id, scoped. Cascades `page_embeddings` and fires `pages_fts_ad`.
    let mut pages_deleted = 0u64;
    for id in &page_ids {
        pages_deleted += tx.execute(
            "DELETE FROM pages WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3",
            rusqlite::params![&id[..], &wid[..], &pid[..]],
        )? as u64;
    }

    // Deleted explicitly rather than left to the cascade so the row count is
    // known and can be reported. Measured: this does *not* change what the
    // FTS index retains — with an external-content table the cascade already
    // makes the text unsearchable, and the tokens stay in the FTS5 segments
    // either way until a `rebuild`. See the scope note on this function.
    let observations_deleted = tx.execute(
        "DELETE FROM observations \
          WHERE session_id = ?1 AND workspace_id = ?2 AND project_id = ?3",
        rusqlite::params![&sid[..], &wid[..], &pid[..]],
    )? as u64;

    // Tombstone before the row goes, in the same transaction: a purge that
    // committed the deletion but not the tombstone would be undone by the
    // next spool drain (#387).
    tx.execute(
        "INSERT INTO purged_sessions (session_id, workspace_id, project_id, purged_at) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(session_id) DO UPDATE SET purged_at = excluded.purged_at",
        rusqlite::params![
            &sid[..],
            &wid[..],
            &pid[..],
            Timestamp::now().as_microsecond()
        ],
    )?;

    // Finally the session itself; cascades scheduler claims and
    // consolidation jobs, which hold no content of their own.
    tx.execute(
        "DELETE FROM sessions WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3",
        rusqlite::params![&sid[..], &wid[..], &pid[..]],
    )?;

    audit(
        &tx,
        "purge_session",
        Some(wid),
        Some(pid),
        None,
        author_id.as_ref().map(ai_memory_core::UserId::as_bytes),
        Timestamp::now().as_microsecond(),
    )?;

    tx.commit()?;

    if compaction == Compaction::Reclaim {
        reclaim_freed_pages(conn)?;
    }

    Ok(PurgeSessionSummary {
        observations_deleted,
        handoffs_deleted,
        pages_deleted,
        auto_improve_runs_deleted,
        removed_paths,
        compacted: compaction == Compaction::Reclaim,
    })
}

/// Delete a project's rows inside one transaction.
///
/// This is a *logical* delete unless `compaction` is [`Compaction::Reclaim`].
/// The rows are gone and nothing reaches them through the API or search, but
/// their bytes remain in free pages of the database file until it is
/// rewritten — as with any SQLite delete. Neither mode is forensic erasure:
/// the wiki git history and any earlier backup still hold the content.
///
/// Execution order:
/// 1. Count rows in each dependent table (pages/all versions, sessions,
///    observations, handoffs, embeddings) before the delete so we can
///    report how many rows were removed.
/// 2. Collect all distinct page paths stored under the project — these are
///    the on-disk files the caller must clean up after this function returns.
/// 3. DELETE FROM projects WHERE id = ? — the ON DELETE CASCADE clauses in
///    V01 + V02 propagate the delete to pages, sessions, observations,
///    handoffs, and page_embeddings automatically.
/// 4. Commit and return the [`PurgeSummary`].
///
/// The `workspace_project_label` string is passed in by the caller (the
/// admin handler has the human-readable names; the writer only has IDs) and
/// forwarded verbatim into [`PurgeSummary::label`] for logging.
///
/// `force` overrides the live-managed-run guard (step 0): `workstreams`
/// cascades out of `projects`, so purging a scope whose lease is still live
/// would delete the lease row out from under a running agent.
///
/// # Errors
/// Returns [`StoreError::ManagedRunActive`] when a managed run's lease is
/// still live and `force` is false, or [`StoreError`] if any SQL statement
/// fails. The transaction is rolled back automatically on error.
pub fn purge_project(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    workspace_project_label: &str,
    author_id: Option<ai_memory_core::UserId>,
    force: bool,
    compaction: Compaction,
) -> StoreResult<PurgeSummary> {
    let tx = conn.transaction()?;

    let count = |sql: &str, id: &[u8]| -> StoreResult<u64> {
        let n: Option<i64> = tx
            .query_row(sql, rusqlite::params![id], |row| row.get(0))
            .optional()?;
        Ok(u64::try_from(n.unwrap_or(0)).unwrap_or(0))
    };

    let pid = project_id.as_bytes();

    // Managed workstreams cascade out of `projects`, and `managed_runs`
    // cascades out of them. A live run's lease row would go with them, which
    // leaves the running agent heartbeating a run id that no longer exists —
    // `409 managed run lease is not active`, every 30s, with its transcript
    // unable to reach any ledger. Refuse unless the operator insists.
    //
    // `state = 'active'` alone is NOT liveness: a crashed wrapper leaves that
    // row untouched, and the only sweep that flips it to `'expired'` runs
    // inside `workstream::prepare_run`. Without the lease-expiry predicate a
    // single crashed agent would block every future purge of the project
    // until someone launched another managed run. The lease is short (90s,
    // extended by heartbeat), so `lease_expires_at > now` is the real signal.
    let now = Timestamp::now().as_microsecond();
    let active_runs: Vec<(String, String)> = {
        let mut stmt = tx.prepare(
            "SELECT w.name, mr.agent_kind FROM managed_runs mr \
             JOIN workstreams w ON w.id = mr.workstream_id \
             WHERE w.project_id = ?1 AND mr.state = 'active' \
               AND mr.lease_expires_at > ?2",
        )?;
        stmt.query_map(rusqlite::params![&pid[..], now], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    if !active_runs.is_empty() && !force {
        let mut names: Vec<String> = active_runs
            .iter()
            .map(|(workstream, agent)| format!("'{workstream}' ({agent})"))
            .collect();
        names.sort();
        names.dedup();
        return Err(StoreError::ManagedRunActive {
            count: active_runs.len() as u64,
            workstreams: names.join(", "),
        });
    }
    let pages_deleted = count("SELECT COUNT(*) FROM pages WHERE project_id = ?1", &pid[..])?;
    let sessions_deleted = count(
        "SELECT COUNT(*) FROM sessions WHERE project_id = ?1",
        &pid[..],
    )?;
    let observations_deleted = count(
        "SELECT COUNT(*) FROM observations WHERE project_id = ?1",
        &pid[..],
    )?;
    let handoffs_deleted = count(
        "SELECT COUNT(*) FROM handoffs WHERE project_id = ?1",
        &pid[..],
    )?;
    // page_embeddings cascade through pages; count pages that have them.
    let embeddings_deleted = count(
        "SELECT COUNT(*) FROM page_embeddings \
         WHERE page_id IN (SELECT id FROM pages WHERE project_id = ?1)",
        &pid[..],
    )?;

    let workstreams_deleted = count(
        "SELECT COUNT(*) FROM workstreams WHERE project_id = ?1",
        &pid[..],
    )?;
    let managed_runs_deleted = count(
        "SELECT COUNT(*) FROM managed_runs WHERE workstream_id IN \
         (SELECT id FROM workstreams WHERE project_id = ?1)",
        &pid[..],
    )?;

    // Collect all distinct on-disk paths for the caller to clean up.
    // We use DISTINCT because multiple versions of the same logical page
    // share a path; the file only exists once. The statement must be
    // dropped before we call tx.commit() to release the borrow on `tx`.
    let page_paths: Vec<String> = {
        let mut path_stmt = tx.prepare("SELECT DISTINCT path FROM pages WHERE project_id = ?1")?;
        path_stmt
            .query_map(rusqlite::params![&pid[..]], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?
    };

    // Same idea for the workstream segment directories: the rows go with the
    // cascade but `raw/workstreams/<id>/` needs post-commit filesystem
    // cleanup. Decode through the typed id so the reported string matches the
    // directory name `write_segment` builds from `WorkstreamId::to_string`; a
    // malformed blob is a corrupt row, so surface it rather than silently
    // shortening the cleanup list.
    let workstream_ids: Vec<String> = {
        let mut stmt = tx.prepare("SELECT id FROM workstreams WHERE project_id = ?1")?;
        let rows = stmt
            .query_map(rusqlite::params![&pid[..]], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|raw| ai_memory_core::WorkstreamId::from_slice(&raw).map(|id| id.to_string()))
            .collect::<Result<Vec<_>, _>>()?
    };

    // Cascade handles pages / sessions / observations / handoffs /
    // page_embeddings. The workspace row is intentionally left intact —
    // other projects may still live there.
    tx.execute(
        "DELETE FROM projects WHERE id = ?1 AND workspace_id = ?2",
        rusqlite::params![&pid[..], workspace_id.as_bytes()],
    )?;

    // Attributed audit trail for the destructive purge. `page_id` is None
    // (the whole project is gone); the operator identity comes from the
    // authenticated request (NULL when single-user / unauthenticated).
    audit(
        &tx,
        "purge_project",
        Some(workspace_id.as_bytes()),
        Some(project_id.as_bytes()),
        None,
        author_id.as_ref().map(ai_memory_core::UserId::as_bytes),
        Timestamp::now().as_microsecond(),
    )?;

    tx.commit()?;

    if compaction == Compaction::Reclaim {
        reclaim_freed_pages(conn)?;
    }

    Ok(PurgeSummary {
        label: workspace_project_label.to_string(),
        page_paths,
        pages_deleted,
        sessions_deleted,
        observations_deleted,
        handoffs_deleted,
        embeddings_deleted,
        workstreams_deleted,
        managed_runs_deleted,
        workstream_ids,
        compacted: compaction == Compaction::Reclaim,
    })
}

/// Summary returned by [`delete_workspace`].
#[derive(Debug, Default, Clone)]
pub struct DeleteWorkspaceSummary {
    /// Projects removed (0 when the workspace was already empty).
    pub projects_deleted: u64,
    /// `pages` rows removed via cascade (all versions).
    pub pages_deleted: u64,
    /// Managed `workstreams` rows removed via cascade.
    pub workstreams_deleted: u64,
    /// `managed_runs` rows removed via cascade.
    pub managed_runs_deleted: u64,
    /// Pre-delete identifiers for post-commit raw-segment cleanup.
    pub workstream_ids: Vec<String>,
    /// Whether the freed bytes were reclaimed (`VACUUM` ran). False for a
    /// plain logical delete.
    pub compacted: bool,
}

/// Delete a workspace row. Refuses a workspace that still holds projects
/// unless `force` is set (the guard exists so a stray typo can't wipe a live
/// workspace). The `workspace_id` FKs are `ON DELETE CASCADE`, so a single
/// `DELETE FROM workspaces` also removes its projects / pages / sessions /
/// observations / handoffs / managed workstreams. The caller removes the
/// on-disk workspace and raw workstream directories afterwards.
///
/// # Errors
/// [`StoreError::WorkspaceNotEmpty`] when it still holds projects and `force`
/// is false; [`StoreError::NotFound`] when the workspace does not exist.
pub fn delete_workspace(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    force: bool,
    compaction: Compaction,
) -> StoreResult<DeleteWorkspaceSummary> {
    let tx = conn.transaction()?;
    let wid = workspace_id.as_bytes();
    let count = |sql: &str| -> StoreResult<u64> {
        let n: Option<i64> = tx
            .query_row(sql, rusqlite::params![&wid[..]], |row| row.get(0))
            .optional()?;
        Ok(u64::try_from(n.unwrap_or(0)).unwrap_or(0))
    };

    let projects_deleted = count("SELECT COUNT(*) FROM projects WHERE workspace_id = ?1")?;
    if projects_deleted > 0 && !force {
        return Err(StoreError::WorkspaceNotEmpty(projects_deleted));
    }
    let pages_deleted = count("SELECT COUNT(*) FROM pages WHERE workspace_id = ?1")?;
    let workstreams_deleted = count("SELECT COUNT(*) FROM workstreams WHERE workspace_id = ?1")?;
    let managed_runs_deleted = count(
        "SELECT COUNT(*) FROM managed_runs mr \
         JOIN workstreams w ON w.id = mr.workstream_id \
         WHERE w.workspace_id = ?1",
    )?;
    let workstream_ids: Vec<String> = {
        let mut stmt = tx.prepare("SELECT id FROM workstreams WHERE workspace_id = ?1")?;
        let rows = stmt
            .query_map(rusqlite::params![&wid[..]], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|raw| ai_memory_core::WorkstreamId::from_slice(&raw).map(|id| id.to_string()))
            .collect::<Result<Vec<_>, _>>()?
    };

    let removed = tx.execute(
        "DELETE FROM workspaces WHERE id = ?1",
        rusqlite::params![&wid[..]],
    )?;
    if removed == 0 {
        return Err(StoreError::NotFound("workspace".into()));
    }
    tx.commit()?;

    if compaction == Compaction::Reclaim {
        reclaim_freed_pages(conn)?;
    }

    Ok(DeleteWorkspaceSummary {
        projects_deleted,
        pages_deleted,
        workstreams_deleted,
        managed_runs_deleted,
        workstream_ids,
        compacted: compaction == Compaction::Reclaim,
    })
}

/// Rename a workspace: a `workspaces.name` UPDATE only — the on-disk dir is
/// keyed by UUID, so nothing moves. Mirrors [`rename_project`].
///
/// # Errors
/// [`StoreError::InvalidWorkspaceName`] on an empty / `/`-containing name;
/// [`StoreError::WorkspaceNameTaken`] on a `UNIQUE(name)` collision;
/// [`StoreError::NotFound`] when the workspace vanished (race with delete).
pub fn rename_workspace(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    new_name: &str,
) -> StoreResult<()> {
    let trimmed = new_name.trim();
    if trimmed.is_empty() {
        return Err(StoreError::InvalidWorkspaceName(
            "workspace name must not be empty or all whitespace".into(),
        ));
    }
    if trimmed.contains('/') {
        return Err(StoreError::InvalidWorkspaceName(
            "workspace name must not contain '/'".into(),
        ));
    }
    let rows = conn.execute(
        "UPDATE workspaces SET name = ?1 WHERE id = ?2",
        params![trimmed, workspace_id.as_bytes()],
    );
    match rows {
        Ok(0) => Err(StoreError::NotFound(format!(
            "workspace id {workspace_id} no longer exists (race with delete)"
        ))),
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                || err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Err(StoreError::WorkspaceNameTaken(trimmed.to_string()))
        }
        Err(e) => Err(StoreError::Sqlite(e)),
    }
}

/// Summary returned by [`move_project_workspace`] and exposed via
/// [`crate::writer::WriterHandle::move_project_workspace`].
#[derive(Debug, Default, Clone)]
pub struct MoveSummary {
    /// `pages` rows re-stamped (all versions, not just latest).
    pub pages_moved: u64,
    /// `sessions` rows re-stamped.
    pub sessions_moved: u64,
    /// `observations` rows re-stamped.
    pub observations_moved: u64,
    /// `handoffs` rows re-stamped.
    pub handoffs_moved: u64,
    /// `audit_log` rows re-stamped.
    pub audit_log_moved: u64,
    /// `auto_improve_runs` rows re-stamped.
    pub auto_improve_runs_moved: u64,
    /// `auto_improve_proposals` rows re-stamped.
    pub auto_improve_proposals_moved: u64,
    /// `auto_improve_rejections` rows re-stamped.
    pub auto_improve_rejections_moved: u64,
    /// `auto_improve_scheduler_state` rows re-stamped.
    pub auto_improve_scheduler_state_moved: u64,
    /// `auto_improve_scheduler_claims` rows re-stamped.
    pub auto_improve_scheduler_claims_moved: u64,
    /// Durable SessionEnd consolidation jobs re-stamped.
    pub session_consolidation_jobs_moved: u64,
    /// Managed workstreams re-stamped. Native sessions, runs, and events stay
    /// attached through `workstream_id` and need no direct update.
    pub workstreams_moved: u64,
}

/// Re-stamp a project's `workspace_id` across every domain table in ONE
/// transaction, keeping the same `project_id`. This is a lossless "true move":
/// pages, sessions, observations, handoffs and supersession history all stay
/// attached to the project — unlike a copy+purge, which drops everything but
/// the latest pages.
///
/// `page_embeddings` and `links` are keyed by `page_id` (not `workspace_id`),
/// so they follow automatically with no re-stamp.
///
/// The destination workspace row MUST already exist (FK on
/// `projects.workspace_id`); the caller get-or-creates it first. A same-named
/// project already living in the destination workspace makes the `projects`
/// UPDATE violate `UNIQUE (workspace_id, name)` and the whole transaction
/// rolls back — the caller must detect that merge case and route it through
/// copy+purge instead.
pub fn move_project_workspace(
    conn: &mut Connection,
    project_id: &ProjectId,
    from_workspace: &WorkspaceId,
    to_workspace: &WorkspaceId,
) -> StoreResult<MoveSummary> {
    let tx = conn.transaction()?;

    let pid = project_id.as_bytes();
    let from = from_workspace.as_bytes();
    let to = to_workspace.as_bytes();

    // Re-stamp child tables first (they carry the denormalized workspace_id),
    // then the project row last. Order is irrelevant inside the transaction,
    // but doing projects last keeps the UNIQUE(workspace_id, name) failure —
    // the merge-collision signal — as the final, cheapest check.
    let pages_moved = tx.execute(
        "UPDATE pages SET workspace_id = ?1 WHERE project_id = ?2",
        params![&to[..], &pid[..]],
    )? as u64;
    let sessions_moved = tx.execute(
        "UPDATE sessions SET workspace_id = ?1 WHERE project_id = ?2",
        params![&to[..], &pid[..]],
    )? as u64;
    let observations_moved = tx.execute(
        "UPDATE observations SET workspace_id = ?1 WHERE project_id = ?2",
        params![&to[..], &pid[..]],
    )? as u64;
    let handoffs_moved = tx.execute(
        "UPDATE handoffs SET workspace_id = ?1 WHERE project_id = ?2",
        params![&to[..], &pid[..]],
    )? as u64;
    let audit_log_moved = tx.execute(
        "UPDATE audit_log SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;
    let auto_improve_runs_moved = tx.execute(
        "UPDATE auto_improve_runs SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;
    let auto_improve_proposals_moved = tx.execute(
        "UPDATE auto_improve_proposals SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;
    let auto_improve_rejections_moved = tx.execute(
        "UPDATE auto_improve_rejections SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;
    let auto_improve_scheduler_state_moved = tx.execute(
        "UPDATE auto_improve_scheduler_state SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;
    let auto_improve_scheduler_claims_moved = tx.execute(
        "UPDATE auto_improve_scheduler_claims SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;
    let session_consolidation_jobs_moved = tx.execute(
        "UPDATE session_consolidation_jobs SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;
    let workstreams_moved = tx.execute(
        "UPDATE workstreams SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;

    let projects_updated = tx.execute(
        "UPDATE projects SET workspace_id = ?1 WHERE id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )?;
    if projects_updated != 1 {
        return Err(StoreError::NotFound(format!(
            "project {project_id} not found in source workspace {from_workspace}"
        )));
    }

    tx.commit()?;
    Ok(MoveSummary {
        pages_moved,
        sessions_moved,
        observations_moved,
        handoffs_moved,
        audit_log_moved,
        auto_improve_runs_moved,
        auto_improve_proposals_moved,
        auto_improve_rejections_moved,
        auto_improve_scheduler_state_moved,
        auto_improve_scheduler_claims_moved,
        session_consolidation_jobs_moved,
        workstreams_moved,
    })
}

/// How [`move_session`] treats the session's consolidated wiki page
/// (`sessions/<session_id>.md`) when the session changes scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PagesMode {
    /// Re-stamp every version of the page into the destination scope so the
    /// curated page and its supersession history follow the session. Fails
    /// with [`StoreError::PagePathTaken`] when the destination already holds
    /// a latest page at that path.
    #[default]
    Move,
    /// Leave the rows in the source scope but clear `is_latest` (and the
    /// session's `summary_page_id` when it pointed at them), so the next
    /// consolidation of the session writes a fresh page in the destination.
    Regenerate,
}

/// One scope a move drained observations out of, and how many.
///
/// A move matches dependent rows by session id alone, so it gathers rows from
/// EVERY scope they landed in — not just the one the caller named as the
/// source. That is deliberate (it is what repairs a session scattered by
/// pre-sticky mid-session routing), but it is not cleanly reversible: moving
/// back sends every row to one scope, and the original per-row attribution is
/// gone. So the caller is told which scopes it is about to drain, by name,
/// before it commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveSessionSourceScope {
    /// Workspace the rows are currently stamped with.
    pub workspace_id: WorkspaceId,
    /// Project the rows are currently stamped with.
    pub project_id: ProjectId,
    /// Workspace name, resolved for the operator-facing report.
    pub workspace_name: String,
    /// Project name, resolved for the operator-facing report.
    pub project_name: String,
    /// Observations of this session currently in that scope.
    pub observations: u64,
}

/// Summary returned by [`move_session`] and exposed via
/// [`crate::writer::WriterHandle::move_session`]. Populated identically for a
/// dry run (`commit = false`), whose transaction is rolled back after counting.
#[derive(Debug, Clone)]
pub struct MoveSessionSummary {
    /// Whether the `sessions` row changed scope. `false` when the row already
    /// sat in the target scope: the call is then a "re-home" that only
    /// re-stamps the session's rows still lying in other scopes, and every
    /// count says how many of those there were (all zero when none).
    pub session_moved: bool,
    /// `observations` rows re-stamped: every row of the session that was
    /// outside the target scope, including any that landed outside the
    /// session row's own scope. Rows already in the target are not counted.
    pub observations: u64,
    /// `handoffs` rows re-stamped (`from_session_id` = the session).
    pub handoffs: u64,
    /// `session_consolidation_jobs` rows re-stamped.
    pub consolidation_jobs: u64,
    /// `auto_improve_runs` rows re-stamped.
    pub auto_improve_runs: u64,
    /// `auto_improve_scheduler_claims` rows re-stamped.
    pub auto_improve_claims: u64,
    /// `pages` rows (all versions) re-stamped under [`PagesMode::Move`].
    pub page_versions_moved: u64,
    /// `pages` rows whose `is_latest` was cleared under
    /// [`PagesMode::Regenerate`].
    pub pages_regenerated: u64,
    /// The session page path when the source scope held at least one version
    /// of it, so the caller can move or retire the on-disk file too.
    pub page_path: Option<String>,
    /// `pages` versions of `sessions/<id>.md` sitting in the target scope
    /// once the call is done (moved there now or already there before), so
    /// a caller can tell "nothing to move" from "no page anywhere".
    pub page_versions_in_target: u64,
    /// Scope the session was read from.
    pub from_workspace: WorkspaceId,
    /// Scope the session was read from.
    pub from_project: ProjectId,
    /// Scope the session now belongs to.
    pub to_workspace: WorkspaceId,
    /// Scope the session now belongs to.
    pub to_project: ProjectId,
    /// The session's recorded working directory, left untouched by the move
    /// (historical truth; the caller may warn when it no longer matches the
    /// destination project).
    pub cwd: Option<String>,
    /// Scopes holding observations of this session that are NOT the target,
    /// with their counts, newest-largest first. More than one entry means the
    /// move will drain a project the caller did not name; an empty list means
    /// everything already sits in the target.
    pub source_scopes: Vec<MoveSessionSourceScope>,
}

/// Re-stamp one session and every row that hangs off it (`observations`,
/// `handoffs` it produced, its consolidation jobs, auto-improve runs and
/// scheduler claim) into `(target_workspace, target_project)` in ONE
/// transaction, plus its `sessions/<session_id>.md` page per `pages`.
///
/// Dependent rows are matched by session id alone, so a hybrid session whose
/// events were scattered over other scopes (mid-session routing before the
/// sticky mode) is gathered whole; only rows not already in the target are
/// touched and counted.
///
/// A target equal to the session row's own scope is a **re-home**: the row
/// stays put (`session_moved = false`) and the same sweep re-stamps every
/// dependent row still lying in any other scope; a session with nothing
/// outside the target reports zero counts (not an error). Its page rows are
/// then handled wherever they sit outside the target: under
/// [`PagesMode::Move`] they are re-stamped (refused with
/// [`StoreError::PagePathTaken`] when the target already holds a latest page
/// or more than one outside scope does), under [`PagesMode::Regenerate`]
/// their latest rows are retired. In a real move the page rows handled are
/// those of the source scope, as before.
///
/// `commit = false` performs the same work and rolls the transaction back
/// before returning, so the summary is an exact dry run. `author_id` is the
/// operator recorded on the `audit_log` row, as in [`rename_project`].
///
/// The `workspace_id`/`project_id` pairing triggers only guard INSERT, so the
/// target pair is validated against `projects` here before any UPDATE runs.
///
/// # Errors
/// - [`StoreError::NotFound`] when the session or the target project (in the
///   target workspace) does not exist.
/// - [`StoreError::PagePathTaken`] under [`PagesMode::Move`] when the target
///   scope already has a latest page at the session page path.
/// - [`StoreError::Sqlite`] on any other SQL failure.
pub fn move_session(
    conn: &mut Connection,
    session_id: SessionId,
    target_workspace: WorkspaceId,
    target_project: ProjectId,
    pages: PagesMode,
    author_id: Option<ai_memory_core::UserId>,
    commit: bool,
) -> StoreResult<MoveSessionSummary> {
    let tx = conn.transaction()?;

    let sid = session_id.as_bytes();
    let row = tx
        .query_row(
            "SELECT workspace_id, project_id, cwd FROM sessions WHERE id = ?1",
            params![&sid[..]],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((from_ws, from_proj, cwd)) = row else {
        return Err(StoreError::NotFound(format!(
            "session {session_id} not found"
        )));
    };
    let from_workspace = WorkspaceId::from_slice(&from_ws)?;
    let from_project = ProjectId::from_slice(&from_proj)?;

    let mut summary = MoveSessionSummary {
        session_moved: false,
        observations: 0,
        handoffs: 0,
        consolidation_jobs: 0,
        auto_improve_runs: 0,
        auto_improve_claims: 0,
        page_versions_moved: 0,
        pages_regenerated: 0,
        page_path: None,
        page_versions_in_target: 0,
        from_workspace,
        from_project,
        to_workspace: target_workspace,
        to_project: target_project,
        cwd,
        source_scopes: Vec::new(),
    };
    let rehome = from_workspace == target_workspace && from_project == target_project;

    let to_ws = target_workspace.as_bytes();
    let to_proj = target_project.as_bytes();
    let target_exists: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM projects WHERE id = ?1 AND workspace_id = ?2",
            params![&to_proj[..], &to_ws[..]],
            |row| row.get(0),
        )
        .optional()?;
    if target_exists.is_none() {
        return Err(StoreError::NotFound(format!(
            "project {target_project} not found in workspace {target_workspace}"
        )));
    }

    if !rehome {
        let sessions_updated = tx.execute(
            "UPDATE sessions SET workspace_id = ?1, project_id = ?2 WHERE id = ?3",
            params![&to_ws[..], &to_proj[..], &sid[..]],
        )?;
        summary.session_moved = sessions_updated == 1;
    }
    // Which scopes this move is about to drain, resolved to names BEFORE the
    // re-stamp moves the rows. A move gathers by session id alone, so it can
    // empty a project the caller never named; the operator sees that in the
    // dry run rather than discovering it afterwards, when the original
    // attribution is no longer recoverable.
    {
        let mut stmt = tx.prepare(
            "SELECT o.workspace_id, o.project_id, w.name, p.name, COUNT(*) \
             FROM observations o \
             JOIN workspaces w ON w.id = o.workspace_id \
             JOIN projects p ON p.id = o.project_id \
             WHERE o.session_id = ?1 AND NOT (o.workspace_id = ?2 AND o.project_id = ?3) \
             GROUP BY o.workspace_id, o.project_id \
             ORDER BY COUNT(*) DESC",
        )?;
        let rows = stmt.query_map(params![&sid[..], &to_ws[..], &to_proj[..]], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        for row in rows {
            let (ws_bytes, proj_bytes, workspace_name, project_name, count) = row?;
            summary.source_scopes.push(MoveSessionSourceScope {
                workspace_id: WorkspaceId::from_slice(&ws_bytes)?,
                project_id: ProjectId::from_slice(&proj_bytes)?,
                workspace_name,
                project_name,
                observations: u64::try_from(count).unwrap_or(0),
            });
        }
    }

    // Every dependent row of the session not already in the target, whatever
    // scope it lies in. Fires `observations_fts_au` per observation row
    // (delete + reinsert of the same text); the FTS index stays consistent
    // inside this transaction.
    let restamp = |table: &str, session_col: &str| -> StoreResult<u64> {
        Ok(tx.execute(
            &format!(
                "UPDATE {table} SET workspace_id = ?1, project_id = ?2 \
                 WHERE {session_col} = ?3 \
                   AND NOT (workspace_id = ?1 AND project_id = ?2)"
            ),
            params![&to_ws[..], &to_proj[..], &sid[..]],
        )? as u64)
    };
    summary.observations = restamp("observations", "session_id")?;
    summary.handoffs = restamp("handoffs", "from_session_id")?;
    summary.consolidation_jobs = restamp("session_consolidation_jobs", "session_id")?;
    summary.auto_improve_runs = restamp("auto_improve_runs", "session_id")?;
    summary.auto_improve_claims = restamp("auto_improve_scheduler_claims", "session_id")?;

    // Page rows to handle: the source scope's in a real move; in a re-home,
    // any scope other than the target (the source IS the target).
    let page_path = format!("sessions/{session_id}.md");
    let page_scope_sql = if rehome {
        "NOT (workspace_id = ?1 AND project_id = ?2)"
    } else {
        "workspace_id = ?1 AND project_id = ?2"
    };
    let page_scope_params = if rehome {
        (&to_ws[..], &to_proj[..])
    } else {
        (&from_ws[..], &from_proj[..])
    };
    let (candidate_versions, candidate_latest): (i64, i64) = tx.query_row(
        &format!(
            "SELECT COUNT(*), COALESCE(SUM(is_latest), 0) FROM pages \
             WHERE {page_scope_sql} AND path = ?3"
        ),
        params![page_scope_params.0, page_scope_params.1, page_path.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if candidate_versions > 0 {
        match pages {
            PagesMode::Move => {
                let taken: Option<i64> = tx
                    .query_row(
                        "SELECT 1 FROM pages \
                         WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 \
                           AND is_latest = 1",
                        params![&to_ws[..], &to_proj[..], page_path.as_str()],
                        |row| row.get(0),
                    )
                    .optional()?;
                // Two latest rows cannot share the path in the target
                // (`idx_pages_latest_path`): a re-home that finds latest
                // rows in more than one outside scope is a collision too.
                if taken.is_some() || candidate_latest > 1 {
                    return Err(StoreError::PagePathTaken { path: page_path });
                }
                summary.page_versions_moved = tx.execute(
                    &format!(
                        "UPDATE pages SET workspace_id = ?4, project_id = ?5 \
                         WHERE {page_scope_sql} AND path = ?3"
                    ),
                    params![
                        page_scope_params.0,
                        page_scope_params.1,
                        page_path.as_str(),
                        &to_ws[..],
                        &to_proj[..],
                    ],
                )? as u64;
            }
            PagesMode::Regenerate => {
                // Close the retiring page's entity windows first (the
                // predicate needs is_latest = 1, flipped just below).
                tx.execute(
                    &format!(
                        "UPDATE entity_page_links SET superseded_at = ?4 \
                         WHERE superseded_at IS NULL AND page_id IN ( \
                             SELECT id FROM pages \
                             WHERE {page_scope_sql} AND path = ?3 AND is_latest = 1)"
                    ),
                    params![
                        page_scope_params.0,
                        page_scope_params.1,
                        page_path.as_str(),
                        Timestamp::now().as_microsecond(),
                    ],
                )?;
                summary.pages_regenerated = tx.execute(
                    &format!(
                        "UPDATE pages SET is_latest = 0 \
                         WHERE {page_scope_sql} AND path = ?3 AND is_latest = 1"
                    ),
                    params![page_scope_params.0, page_scope_params.1, page_path.as_str()],
                )? as u64;
                // The session's summary pointer targeted the page just
                // retired; the next consolidation sets it again.
                tx.execute(
                    &format!(
                        "UPDATE sessions SET summary_page_id = NULL \
                         WHERE id = ?4 AND summary_page_id IN ( \
                             SELECT id FROM pages WHERE {page_scope_sql} AND path = ?3)"
                    ),
                    params![
                        page_scope_params.0,
                        page_scope_params.1,
                        page_path.as_str(),
                        &sid[..],
                    ],
                )?;
            }
        }
        summary.page_path = Some(page_path.clone());
    }
    summary.page_versions_in_target = tx.query_row(
        "SELECT COUNT(*) FROM pages WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3",
        params![&to_ws[..], &to_proj[..], page_path.as_str()],
        |row| row.get::<_, i64>(0),
    )? as u64;

    audit(
        &tx,
        "move_session",
        Some(to_ws),
        Some(to_proj),
        None,
        author_id.as_ref().map(ai_memory_core::UserId::as_bytes),
        Timestamp::now().as_microsecond(),
    )?;

    if commit {
        tx.commit()?;
    } else {
        tx.rollback()?;
    }
    Ok(summary)
}

/// Remove embedding rows in a workspace/project scope whose `(provider, model, dim)`
/// does not match the configured triple, plus rows tied to superseded pages.
pub fn delete_stale_page_embeddings(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    project_id: Option<&ProjectId>,
    provider: &str,
    model: &str,
    dim: u32,
) -> StoreResult<u64> {
    let tx = conn.transaction()?;
    let (n, orphans) = if let Some(project_id) = project_id {
        let n = tx.execute(
            "DELETE FROM page_embeddings \
             WHERE page_id IN (\
                SELECT id FROM pages \
                WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1\
             ) \
               AND NOT (provider = ?3 AND model = ?4 AND dim = CAST(?5 AS INTEGER))",
            params![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                provider,
                model,
                dim
            ],
        )?;
        let orphans = tx.execute(
            "DELETE FROM page_embeddings \
             WHERE page_id IN (\
                SELECT id FROM pages \
                WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 0\
             )",
            params![workspace_id.as_bytes(), project_id.as_bytes()],
        )?;
        (n, orphans)
    } else {
        let n = tx.execute(
            "DELETE FROM page_embeddings \
             WHERE page_id IN (\
                SELECT id FROM pages \
                WHERE workspace_id = ?1 AND is_latest = 1\
             ) \
               AND NOT (provider = ?2 AND model = ?3 AND dim = CAST(?4 AS INTEGER))",
            params![workspace_id.as_bytes(), provider, model, dim],
        )?;
        let orphans = tx.execute(
            "DELETE FROM page_embeddings \
             WHERE page_id IN (\
                SELECT id FROM pages \
                WHERE workspace_id = ?1 AND is_latest = 0\
             )",
            params![workspace_id.as_bytes()],
        )?;
        (n, orphans)
    };
    tx.commit()?;
    Ok(u64::try_from(n.saturating_add(orphans)).unwrap_or(0))
}

#[cfg(test)]
pub(crate) mod tests {
    //! Focused unit tests for the load-bearing mutating SQL paths.
    //!
    //! `Store::open` exercises these incidentally through
    //! integration tests, but specific edges — supersession on body
    //! change, no-op on identical body, handoff state transitions,
    //! end_session summary linkage, embedding PK-replacement —
    //! deserve direct coverage so a regression surfaces with a
    //! one-line diff instead of a cascading e2e failure.
    use super::*;
    use ai_memory_core::{
        FeedbackKind, LinkTarget, NewHandoff, NewPage, NewSession, PagePath, ProjectId, Tier,
        UserId, WorkspaceId,
    };
    use rusqlite::Connection;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    struct CapturedLogWriter(Arc<Mutex<Vec<u8>>>);

    static WARNING_CAPTURE_LOCK: Mutex<()> = Mutex::new(());

    impl Write for CapturedLogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = CapturedLogWriter;

        fn make_writer(&'a self) -> Self::Writer {
            CapturedLogWriter(Arc::clone(&self.0))
        }
    }

    /// A subscriber that exists only to be counted (#558).
    ///
    /// `tracing` resolves a callsite's interest **once**, on first execution,
    /// and caches the answer process-globally. While only one `Dispatch` is
    /// alive — which `WARNING_CAPTURE_LOCK` guarantees, since it serialises the
    /// captures — `tracing-core` takes a fast path that resolves that answer
    /// from the *registering thread's own* default subscriber
    /// (`callsite.rs`: `if has_just_one { … dispatcher::get_default(f) }`).
    /// A thread without one resolves to `Interest::never()`, and every later
    /// capture of that callsite goes blind.
    ///
    /// Keeping a second `Dispatch` alive for the life of the test binary
    /// defeats that: with more than one registered, resolution consults every
    /// live dispatcher instead of only the current thread's, so the capture's
    /// own subscriber counts no matter which thread registers first.
    ///
    /// **Being counted is the whole contribution.** It is never installed as
    /// anyone's default and never receives an event. Verified by control: see
    /// `a_subscriberless_thread_must_not_disable_a_callsite_the_capture_needs`,
    /// which fails when this pin is removed and passes with it, whatever this
    /// subscriber answers.
    ///
    /// The two ways it could stop being invisible are not symmetric, and the
    /// difference is easy to get backwards:
    ///
    /// - **Interest is not a maximum.** `Interest::and` keeps the value when
    ///   two dispatchers agree and collapses to `sometimes()` when they differ.
    ///   This pin answers `never` (its `enabled` is false, and the default
    ///   `register_callsite` follows that), so pairing it with a capture that
    ///   answers `always` yields `sometimes` — a per-event `enabled()` call,
    ///   answered by whichever subscriber is actually in scope. That is why an
    ///   inert pin cannot silence a callsite someone else wants.
    /// - **The level hint *is* a maximum**, and a `None` hint is read as
    ///   `TRACE`. Hence the explicit `Some(OFF)` below.
    struct RegistryPin;

    impl tracing::Subscriber for RegistryPin {
        /// Explicit, and load-bearing — see
        /// `the_registry_pin_stays_invisible_to_the_global_max_level`.
        fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
            Some(tracing::level_filters::LevelFilter::OFF)
        }

        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            false
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// Holds the second dispatcher alive. Constructing a `Dispatch` is what
    /// registers it, so simply keeping this initialised is the mechanism.
    static REGISTRY_PIN: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();

    fn capture_warnings(run: impl FnOnce()) -> String {
        REGISTRY_PIN.get_or_init(|| tracing::Dispatch::new(RegistryPin));
        let _guard = WARNING_CAPTURE_LOCK.lock().unwrap();
        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_writer(logs.clone())
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        String::from_utf8(logs.0.lock().unwrap().clone()).unwrap()
    }

    /// Guards the `max_level_hint` override on [`RegistryPin`] against being
    /// removed as apparent dead code.
    ///
    /// The pin has to be *registered* to do its job, and every registered
    /// dispatcher feeds `tracing`'s global max level — which `rebuild_interest`
    /// computes as the maximum across live dispatchers, reading a `None` hint
    /// as `TRACE`. Dropping the override therefore raises this binary's global
    /// level from `OFF` to `TRACE` for good after the first capture. Measured
    /// both ways: with `Some(OFF)` the level follows exactly the `OFF -> WARN`
    /// path it follows with no pin at all; without the override it goes to
    /// `TRACE` and stays there.
    ///
    /// Nothing observable breaks when that happens, which is precisely why it
    /// wants a guard rather than a comment — every `trace!` and `debug!`
    /// callsite in the crate would quietly start being evaluated instead of
    /// short-circuiting.
    #[test]
    fn the_registry_pin_stays_invisible_to_the_global_max_level() {
        use tracing::Subscriber as _;

        assert_eq!(
            RegistryPin.max_level_hint(),
            Some(tracing::level_filters::LevelFilter::OFF),
            "a None hint is read as TRACE, which would raise the whole binary's \
             global max level as a side effect of pinning the dispatcher"
        );
    }

    /// A `warn!` callsite reachable from nowhere but the control test below.
    ///
    /// The defect being reproduced is in `tracing`'s **one-shot** callsite
    /// registration, so the callsite has to still be unregistered when the
    /// control runs. Any second caller would register it first and make the
    /// control vacuous.
    fn control_only_warn() {
        tracing::warn!("control-callsite-canary");
    }

    /// #558: a thread with no subscriber must not be able to disable a
    /// callsite that a concurrent capture depends on.
    ///
    /// The flake is `Interest::never()` cached **globally** for a callsite,
    /// resolved from whichever thread happens to register it first:
    ///
    /// - `tracing`'s event macro gates on the global max level *before*
    ///   consulting the callsite's interest. With no other subscriber in this
    ///   binary that level starts at `OFF`, so a subscriberless thread normally
    ///   short-circuits and never registers anything.
    /// - While a capture holds a live `Dispatch`, the level is `WARN`, and now
    ///   a subscriberless thread *does* reach the registration.
    /// - With a single live dispatcher, registration resolves interest from the
    ///   registering thread's own default — `NoSubscriber` — writing
    ///   `Interest::never()` into a process-global cache.
    ///
    /// The capturing thread then skips its `warn!` entirely and the capture
    /// comes back **empty**, which is the reported symptom. Under `--workspace`
    /// the foreign thread is real: the homonym `warn!` in this module is also
    /// driven by the `ai-memory-writer` thread and by other libtest threads,
    /// none of which hold a subscriber.
    ///
    /// Ordering, not luck, reproduces it: register from the foreign thread
    /// first, then ask the capture to observe the same callsite.
    #[test]
    fn a_subscriberless_thread_must_not_disable_a_callsite_the_capture_needs() {
        let logs = capture_warnings(|| {
            std::thread::spawn(control_only_warn)
                .join()
                .expect("control thread panicked");
            control_only_warn();
        });

        assert!(
            logs.contains("control-callsite-canary"),
            "a subscriberless thread registered this callsite as Interest::never() \
             and the capture went blind: {logs:?}"
        );
    }

    fn handoff_acceptance(
        handoff_id: HandoffId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> HandoffAcceptance {
        HandoffAcceptance {
            handoff_id,
            workspace_id,
            project_id,
            accepting_agent: AgentKind::Codex,
            accepting_session: None,
            accepting_user: None,
            owner_filter: OwnerFilter::Any,
            receiving_cwd: None,
        }
    }

    /// Open a fresh DB with migrations applied + a default workspace
    /// and "scratch" project pre-created. Tuple-return keeps the
    /// tempdir alive for the duration of the test.
    // Issue #156 regression, class-wide: every AgentKind must survive
    // begin_session — i.e. the persisted sessions.agent_kind CHECK
    // constraint must enumerate every variant. Zero shipped with the enum
    // variant but without the CHECK migration (V26) and only a live test
    // caught the constraint failure; this pins the whole class so the next
    // agent addition fails here, before any deploy.
    #[test]
    fn begin_session_accepts_every_agent_kind() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        for kind in ai_memory_core::AgentKind::ALL {
            let session = NewSession {
                id: ai_memory_core::SessionId::new(),
                workspace_id: ws,
                project_id: proj,
                agent_kind: kind,
                cwd: None,
                actor_user: None,
            };
            begin_session(&mut conn, &session).unwrap_or_else(|e| {
                panic!(
                    "sessions.agent_kind CHECK rejects '{}' — add it to the \
                     constraint with a new migration: {e}",
                    kind.as_str()
                )
            });
        }
    }

    #[test]
    fn get_or_create_project_flags_homonym_across_workspaces() {
        let (_tmp, mut conn, _ws, _proj) = fresh_db();
        let ws_a = get_or_create_workspace(&mut conn, "alpha").unwrap();
        let ws_b = get_or_create_workspace(&mut conn, "beta").unwrap();
        let a_shared = get_or_create_project(&mut conn, &ws_a, "shared", None).unwrap();

        // "shared" already lives in `alpha`, so from `beta`'s side it is a
        // cross-workspace homonym; the owning workspace and unique names
        // flag nothing.
        {
            let tx = conn.transaction().unwrap();
            assert_eq!(
                project_name_in_other_workspaces(&tx, &ws_b, "shared").unwrap(),
                vec!["alpha".to_string()]
            );
            assert!(
                project_name_in_other_workspaces(&tx, &ws_a, "shared")
                    .unwrap()
                    .is_empty(),
                "the owning workspace must be excluded"
            );
            assert!(
                project_name_in_other_workspaces(&tx, &ws_b, "unique-name")
                    .unwrap()
                    .is_empty()
            );
        }

        // Creation is NOT blocked — the homonym is id-namespaced.
        let b_shared = get_or_create_project(&mut conn, &ws_b, "shared", None).unwrap();
        assert_ne!(
            a_shared, b_shared,
            "homonymous projects must get distinct ids"
        );
    }

    #[test]
    fn get_or_create_project_warns_when_creating_homonym_across_workspaces() {
        let (_tmp, mut conn, _ws, _proj) = fresh_db();
        let ws_a = get_or_create_workspace(&mut conn, "alpha").unwrap();
        let ws_b = get_or_create_workspace(&mut conn, "beta").unwrap();
        get_or_create_project(&mut conn, &ws_a, "shared", None).unwrap();

        let logs = capture_warnings(|| {
            get_or_create_project(&mut conn, &ws_b, "shared", None).unwrap();
        });
        assert!(
            logs.contains("already exists in other workspace")
                && logs.contains("shared")
                && logs.contains("alpha"),
            "homonym creation warning should name the project and other workspace: {logs}"
        );

        let logs = capture_warnings(|| {
            get_or_create_project(&mut conn, &ws_b, "shared", None).unwrap();
        });
        assert!(
            !logs.contains("already exists in other workspace"),
            "idempotent lookup must not warn: {logs}"
        );
    }

    #[test]
    fn ensure_project_with_id_warns_when_creating_homonym_across_workspaces() {
        let (_tmp, mut conn, _ws, _proj) = fresh_db();
        let ws_a = get_or_create_workspace(&mut conn, "alpha").unwrap();
        let ws_b = get_or_create_workspace(&mut conn, "beta").unwrap();
        get_or_create_project(&mut conn, &ws_a, "shared", None).unwrap();
        let id = ProjectId::new();

        let logs = capture_warnings(|| {
            ensure_project_with_id(&mut conn, id, ws_b, "shared", None).unwrap();
        });
        assert!(
            logs.contains("already exists in other workspace")
                && logs.contains("shared")
                && logs.contains("alpha"),
            "manifest project creation warning should name the project and other workspace: {logs}"
        );

        let logs = capture_warnings(|| {
            ensure_project_with_id(&mut conn, id, ws_b, "shared", None).unwrap();
        });
        assert!(
            !logs.contains("already exists in other workspace"),
            "idempotent manifest import must not warn: {logs}"
        );
    }

    #[test]
    fn ensure_project_with_id_does_not_warn_when_validation_fails() {
        let (_tmp, mut conn, _ws, _proj) = fresh_db();
        let ws_a = get_or_create_workspace(&mut conn, "alpha").unwrap();
        let ws_b = get_or_create_workspace(&mut conn, "beta").unwrap();
        get_or_create_project(&mut conn, &ws_a, "shared", None).unwrap();
        let id = ProjectId::new();
        ensure_project_with_id(&mut conn, id, ws_b, "other", None).unwrap();

        let logs = capture_warnings(|| {
            let err = ensure_project_with_id(&mut conn, id, ws_b, "shared", None)
                .expect_err("same id with different name must fail validation");
            assert!(
                matches!(err, StoreError::Duplicate(_)),
                "unexpected error: {err}"
            );
        });
        assert!(
            !logs.contains("already exists in other workspace"),
            "failed validation must not emit a creation warning: {logs}"
        );
    }

    #[test]
    fn delete_workspace_refuses_non_empty_then_cascades_with_force() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body")).unwrap();
        let prepared = open_managed_run(&mut conn, &ws, &proj);

        // Non-empty (holds the "scratch" project + a page) → refused w/o force.
        let err = delete_workspace(&mut conn, &ws, false, Compaction::Skip).unwrap_err();
        assert!(
            matches!(err, StoreError::WorkspaceNotEmpty(n) if n >= 1),
            "expected WorkspaceNotEmpty, got {err:?}"
        );
        let ws_still: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM workspaces WHERE id = ?1",
                rusqlite::params![ws.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ws_still, 1, "refused delete must not touch the row");

        // Force cascades: project + page gone, workspace row gone.
        let summary = delete_workspace(&mut conn, &ws, true, Compaction::Skip).unwrap();
        assert!(
            summary.projects_deleted >= 1 && summary.pages_deleted >= 1,
            "{summary:?}"
        );
        assert_eq!(summary.workstreams_deleted, 1);
        assert_eq!(summary.managed_runs_deleted, 1);
        assert_eq!(
            summary.workstream_ids,
            vec![prepared.workstream_id.to_string()]
        );
        let proj_left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM projects WHERE workspace_id = ?1",
                rusqlite::params![ws.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(proj_left, 0, "projects must cascade on workspace delete");

        // Deleting again → NotFound.
        assert!(matches!(
            delete_workspace(&mut conn, &ws, true, Compaction::Skip).unwrap_err(),
            StoreError::NotFound(_)
        ));
    }

    #[test]
    fn delete_workspace_empty_succeeds_without_force() {
        let (_tmp, mut conn, _ws, _proj) = fresh_db();
        let empty = get_or_create_workspace(&mut conn, "orphan-ws").unwrap();
        let summary = delete_workspace(&mut conn, &empty, false, Compaction::Skip).unwrap();
        assert_eq!(summary.projects_deleted, 0);
        assert_eq!(summary.pages_deleted, 0);
        assert_eq!(summary.workstreams_deleted, 0);
        assert_eq!(summary.managed_runs_deleted, 0);
        assert!(summary.workstream_ids.is_empty());
    }

    #[test]
    fn rename_workspace_updates_name_and_rejects_collision() {
        let (_tmp, mut conn, ws, _proj) = fresh_db();
        get_or_create_workspace(&mut conn, "taken").unwrap();

        rename_workspace(&mut conn, &ws, "renamed").unwrap();
        let name: String = conn
            .query_row(
                "SELECT name FROM workspaces WHERE id = ?1",
                rusqlite::params![ws.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(name, "renamed", "name column updated in place");

        assert!(matches!(
            rename_workspace(&mut conn, &ws, "taken").unwrap_err(),
            StoreError::WorkspaceNameTaken(_)
        ));
        assert!(matches!(
            rename_workspace(&mut conn, &ws, "   ").unwrap_err(),
            StoreError::InvalidWorkspaceName(_)
        ));
    }

    fn fresh_db() -> (
        TempDir,
        Connection,
        ai_memory_core::WorkspaceId,
        ai_memory_core::ProjectId,
    ) {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let mut conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrations::run(&mut conn).unwrap();
        let ws = get_or_create_workspace(&mut conn, "default").unwrap();
        let proj = get_or_create_project(&mut conn, &ws, "scratch", None).unwrap();
        (tmp, conn, ws, proj)
    }

    fn hook_session(
        id: SessionId,
        ws: WorkspaceId,
        proj: ProjectId,
        owner: Option<&str>,
    ) -> NewSession {
        NewSession {
            id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: owner.map(str::to_owned),
        }
    }

    fn hook_observation(session: &NewSession) -> NewObservation {
        NewObservation {
            session_id: session.id,
            workspace_id: session.workspace_id,
            project_id: session.project_id,
            kind: ObservationKind::UserPrompt,
            extension: None,
            source_event: None,
            title: "hook".into(),
            body: "observation".into(),
            importance: 5,
        }
    }

    /// Seed a session with one observation and a derived page, return the ids.
    fn seed_session(
        conn: &mut Connection,
        ws: WorkspaceId,
        proj: ProjectId,
        marker: &str,
    ) -> (SessionId, PageId) {
        let sid = SessionId::new();
        let session = hook_session(sid, ws, proj, None);
        begin_session(conn, &session).unwrap();
        let mut obs = hook_observation(&session);
        obs.body = format!("obs-{marker}");
        insert_observation(conn, &obs).unwrap();
        let page_id = upsert_page(
            conn,
            &page(
                ws,
                proj,
                &format!("sessions/{marker}.md"),
                &format!("page-{marker}"),
            ),
        )
        .unwrap();
        end_session(conn, &sid, Some(&page_id)).unwrap();
        (sid, page_id)
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// Insert one managed workstream event carrying a distinctive marker.
    /// `workstream_events` cascades out of `workstreams`, which cascades out
    /// of `projects` — so a project purge removes these rows without ever
    /// running a `DELETE` statement against the table itself.
    fn seed_workstream_event(
        conn: &Connection,
        workstream_id: &ai_memory_core::WorkstreamId,
        marker: &str,
    ) {
        conn.execute(
            "INSERT INTO workstream_events (workstream_id, sequence, event_id, agent_kind, \
             native_session_id, kind, role, content, created_at) \
             VALUES (?1, 1, ?2, 'claude-code', 'native-1', 'message', 'user', ?3, ?4)",
            rusqlite::params![
                workstream_id.as_bytes(),
                format!("evt-{marker}"),
                format!("evt-body-{marker}"),
                Timestamp::now().as_microsecond(),
            ],
        )
        .unwrap();
    }

    /// The default for a project purge is the same logical delete `#387`
    /// documented for a session purge. Pinned so the CLI help, the admin route
    /// docs and `docs/lifecycle-ops.md` cannot drift away from the behaviour:
    /// if this starts failing, byte-level removal became the default and all
    /// three need updating together.
    #[test]
    fn purge_project_is_a_logical_delete_by_default() {
        let (tmp, mut conn, ws, proj) = fresh_db();
        let db_path = tmp.path().join("test.sqlite");
        seed_session(&mut conn, ws, proj, "canaryproj");

        let summary = purge_project(
            &mut conn,
            &ws,
            &proj,
            "default/scratch",
            None,
            false,
            Compaction::Skip,
        )
        .unwrap();
        assert!(!summary.compacted, "the default does not VACUUM");

        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
        let bytes = std::fs::read(&db_path).unwrap();
        assert!(
            bytes.windows(14).any(|w| w == b"obs-canaryproj"),
            "documented boundary: purge-project is a logical delete. If this now \
             fails, byte-level removal became the default and the #540 note, the \
             CLI help and docs/lifecycle-ops.md all need updating to match."
        );
    }

    /// The opt-in half. Paired with `purge_project_is_a_logical_delete_by_default`
    /// so the two together pin the difference between what the command promises
    /// and what it offers.
    #[test]
    fn purge_project_with_reclaim_removes_the_bytes_from_the_file() {
        let (tmp, mut conn, ws, proj) = fresh_db();
        let db_path = tmp.path().join("test.sqlite");
        seed_session(&mut conn, ws, proj, "canaryproj");
        // A second project in the same workspace must survive untouched.
        let other = get_or_create_project(&mut conn, &ws, "keeper", None).unwrap();
        seed_session(&mut conn, ws, other, "survivor");

        let summary = purge_project(
            &mut conn,
            &ws,
            &proj,
            "default/scratch",
            None,
            false,
            Compaction::Reclaim,
        )
        .unwrap();
        assert!(summary.compacted, "the summary reports that VACUUM ran");

        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
        let bytes = std::fs::read(&db_path).unwrap();
        assert!(
            !bytes.windows(14).any(|w| w == b"obs-canaryproj"),
            "Reclaim must remove the purged project's text from the database file"
        );
        assert!(
            bytes.windows(12).any(|w| w == b"obs-survivor"),
            "the sibling project's text is untouched by the compaction"
        );
    }

    /// The reason `reclaim_freed_pages` rebuilds all three FTS indexes rather
    /// than the two a session purge needs.
    ///
    /// `workstream_events` is an external-content FTS5 table whose rows leave
    /// via the `projects` → `workstreams` → `workstream_events` cascade. A
    /// cascade does not fire the `AFTER DELETE` trigger that would remove the
    /// row from the index (`recursive_triggers` is off), so both the index
    /// entry and its tokens survive the delete. Rebuilding only `pages_fts`
    /// and `observations_fts` — the set a session purge needs — would leave a
    /// managed agent's transcript text in the file after an operator asked for
    /// it to be reclaimed.
    #[test]
    fn reclaim_clears_workstream_event_text_that_left_via_cascade() {
        let (tmp, mut conn, ws, proj) = fresh_db();
        let db_path = tmp.path().join("test.sqlite");
        let prepared = open_managed_run(&mut conn, &ws, &proj);
        seed_workstream_event(&conn, &prepared.workstream_id, "canaryevt");

        // Present before the purge, both in the file and in the index.
        let indexed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM workstream_events_fts \
                 WHERE workstream_events_fts MATCH '\"canaryevt\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(indexed, 1, "the event is searchable before the purge");

        purge_project(
            &mut conn,
            &ws,
            &proj,
            "default/scratch",
            None,
            true,
            Compaction::Reclaim,
        )
        .unwrap();

        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
        let bytes = std::fs::read(&db_path).unwrap();
        // Assert on the *token*, not the whole hyphenated body. The body only
        // ever existed contiguously in the content table, which `VACUUM` clears
        // on its own; what survives an ordinary delete is the tokenized form
        // inside the FTS5 segments, and only a `rebuild` removes that. Asserting
        // on the full string would pass with or without the rebuild and prove
        // nothing.
        assert!(
            !bytes.windows(9).any(|w| w == b"canaryevt"),
            "Reclaim must clear workstream event tokens; if this fails, the \
             workstream_events_fts rebuild was dropped from reclaim_freed_pages \
             and a managed transcript's text survives a compacted purge"
        );
    }

    /// `delete_workspace` takes the same path and makes the same promise.
    #[test]
    fn delete_workspace_reclaim_removes_the_bytes_and_reports_it() {
        let (tmp, mut conn, ws, proj) = fresh_db();
        let db_path = tmp.path().join("test.sqlite");
        seed_session(&mut conn, ws, proj, "canaryws");

        let skipped = {
            let other = get_or_create_workspace(&mut conn, "doomed").unwrap();
            delete_workspace(&mut conn, &other, false, Compaction::Skip).unwrap()
        };
        assert!(!skipped.compacted, "the default does not VACUUM");

        let summary = delete_workspace(&mut conn, &ws, true, Compaction::Reclaim).unwrap();
        assert!(summary.compacted, "the summary reports that VACUUM ran");

        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
        let bytes = std::fs::read(&db_path).unwrap();
        assert!(
            !bytes.windows(12).any(|w| w == b"obs-canaryws"),
            "Reclaim must remove the deleted workspace's text from the file"
        );
    }

    /// The premise of #549: after a large delete the space is *not* returned
    /// to the filesystem — SQLite keeps the pages on its freelist for reuse.
    /// `compact` is what gives them back, without deleting anything itself.
    ///
    /// Deliberately paired assertions rather than one: the first pins that
    /// there is a backlog to reclaim (otherwise the second could pass on an
    /// empty database and prove nothing), the second that compaction reclaims
    /// it.
    #[test]
    fn compact_returns_free_pages_that_a_delete_left_behind() {
        let (_tmp, mut conn, ws, proj) = fresh_db();

        // Enough rows that the freelist is unambiguous rather than noise.
        for i in 0..400 {
            upsert_page(
                &mut conn,
                &page(ws, proj, &format!("notes/{i}.md"), &"padding ".repeat(256)),
            )
            .unwrap();
        }
        conn.execute("DELETE FROM pages", []).unwrap();

        let before = database_bytes(&conn).unwrap();
        let freelist: i64 = conn
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .unwrap();
        assert!(
            freelist > 0,
            "the delete must leave a freelist, or this test proves nothing"
        );

        let summary = compact(&mut conn).unwrap();

        assert_eq!(summary.bytes_before, before);
        assert!(
            summary.bytes_after < summary.bytes_before,
            "compaction must shrink the file: {} -> {}",
            summary.bytes_before,
            summary.bytes_after
        );
        assert_eq!(
            summary.bytes_reclaimed(),
            summary.bytes_before - summary.bytes_after
        );
        let after_freelist: i64 = conn
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            after_freelist, 0,
            "the freelist is gone, not merely smaller"
        );
    }

    /// Compaction deletes nothing. Worth a test of its own: the operation is
    /// reachable from an admin route and shares its implementation with the
    /// destructive commands' `--compact`, so "it only reclaims" is a property
    /// someone could break without noticing.
    #[test]
    fn compact_preserves_every_row_and_keeps_the_index_searchable() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        seed_session(&mut conn, ws, proj, "survivor");
        let pages_before = count(&conn, "SELECT COUNT(*) FROM pages");
        let obs_before = count(&conn, "SELECT COUNT(*) FROM observations");

        compact(&mut conn).unwrap();

        assert_eq!(count(&conn, "SELECT COUNT(*) FROM pages"), pages_before);
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM observations"),
            obs_before
        );
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM observations_fts \
                 WHERE observations_fts MATCH '\"obs-survivor\"'"
            ),
            1,
            "the rebuilt index still finds the surviving session"
        );
    }

    /// A `VACUUM` can leave the file marginally larger when defragmentation
    /// costs more than the pages it released. That is zero reclaimed, not a
    /// wrapped-around u64.
    #[test]
    fn bytes_reclaimed_saturates_instead_of_underflowing() {
        let grew = CompactSummary {
            bytes_before: 1_000,
            bytes_after: 1_200,
        };
        assert_eq!(grew.bytes_reclaimed(), 0);
    }

    /// #387, the reporter's reproduction: after purging one session by UUID,
    /// its rows are gone from every table that held them.
    #[test]
    fn purge_session_removes_the_session_and_everything_derived_from_it() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let (sid, _page) = seed_session(&mut conn, ws, proj, "target");

        let summary = purge_session(&mut conn, ws, proj, sid, None, Compaction::Skip).unwrap();

        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM sessions"),
            0,
            "sessions=0"
        );
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM observations"),
            0,
            "observations=0"
        );
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM pages"), 0, "pages=0");
        assert_eq!(summary.observations_deleted, 1);
        assert_eq!(summary.pages_deleted, 1);
        assert_eq!(
            summary.removed_paths,
            vec!["sessions/target.md".to_string()]
        );
    }

    /// The blast radius must stop at the session. A sibling session in the
    /// same project keeps every row it owns.
    #[test]
    fn purge_session_leaves_a_sibling_session_in_the_same_project_intact() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let (target, _) = seed_session(&mut conn, ws, proj, "target");
        let (keep, keep_page) = seed_session(&mut conn, ws, proj, "keep");

        purge_session(&mut conn, ws, proj, target, None, Compaction::Skip).unwrap();

        assert_eq!(count(&conn, "SELECT COUNT(*) FROM sessions"), 1);
        let survivor: Vec<u8> = conn
            .query_row("SELECT id FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(survivor, keep.as_bytes().to_vec(), "the sibling survived");
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM observations"), 1);
        let body: String = conn
            .query_row("SELECT body FROM observations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(body, "obs-keep");
        let page_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages WHERE id = ?1",
                [keep_page.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(page_rows, 1, "the sibling's page survived");
    }

    /// A session id is not authority over another scope. Naming the wrong
    /// project must fail closed and delete nothing.
    #[test]
    fn purge_session_refuses_a_session_from_another_project_and_deletes_nothing() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let other = get_or_create_project(&mut conn, &ws, "other", None).unwrap();
        let (sid, _) = seed_session(&mut conn, ws, proj, "target");

        let err = purge_session(&mut conn, ws, other, sid, None, Compaction::Skip)
            .expect_err("a session outside the named project must not be purgeable");
        assert!(matches!(err, StoreError::NotFound(_)), "got {err:?}");

        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM sessions"),
            1,
            "nothing deleted"
        );
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM observations"), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM pages"), 1);
    }

    /// Same containment across workspaces.
    #[test]
    fn purge_session_refuses_a_session_from_another_workspace() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let ws2 = get_or_create_workspace(&mut conn, "second").unwrap();
        let proj2 = get_or_create_project(&mut conn, &ws2, "scratch", None).unwrap();
        let (sid, _) = seed_session(&mut conn, ws, proj, "target");

        let err = purge_session(&mut conn, ws2, proj2, sid, None, Compaction::Skip)
            .expect_err("cross-workspace purge must be refused");
        assert!(matches!(err, StoreError::NotFound(_)));
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM sessions"), 1);
    }

    /// Two projects can hold the same `sessions/<uuid>.md` path. Deleting by
    /// path would take the other one; deleting by id must not.
    #[test]
    fn purge_session_does_not_delete_an_identically_pathed_page_in_another_project() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let other = get_or_create_project(&mut conn, &ws, "other", None).unwrap();
        let (sid, _) = seed_session(&mut conn, ws, proj, "shared");
        let twin = upsert_page(
            &mut conn,
            &page(ws, other, "sessions/shared.md", "page-elsewhere"),
        )
        .unwrap();

        purge_session(&mut conn, ws, proj, sid, None, Compaction::Skip).unwrap();

        let survived: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages WHERE id = ?1",
                [twin.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            survived, 1,
            "a same-path page in another project must survive a purge by id"
        );
    }

    /// Insert one open handoff. `manual` writes `from_session_id = NULL`,
    /// which is what makes a handoff exempt from the automatic sweep.
    fn seed_handoff(
        conn: &Connection,
        ws: WorkspaceId,
        proj: ProjectId,
        cwd: Option<&str>,
        manual: bool,
        owner: Option<&str>,
        created_at: i64,
    ) -> Vec<u8> {
        let id = uuid::Uuid::now_v7().as_bytes().to_vec();
        let from_session: Option<Vec<u8>> = if manual {
            None
        } else {
            Some(SessionId::new().as_bytes().to_vec())
        };
        // An auto handoff needs a real session row for the FK.
        if let Some(sid) = &from_session {
            conn.execute(
                "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, started_at, \
                 actor_user) VALUES (?1, ?2, ?3, 'codex', 1, ?4)",
                rusqlite::params![&sid[..], ws.as_bytes(), proj.as_bytes(), owner],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO handoffs (id, workspace_id, project_id, from_session_id, from_agent, \
             summary, state, created_at, cwd, owner_user) \
             VALUES (?1, ?2, ?3, ?4, 'codex', 'baton', 'open', ?5, ?6, ?7)",
            rusqlite::params![
                &id[..],
                ws.as_bytes(),
                proj.as_bytes(),
                from_session,
                created_at,
                cwd,
                owner
            ],
        )
        .unwrap();
        id
    }

    fn open_handoffs(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM handoffs WHERE state = 'open'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// #513: the leftover backlog is made of exactly the handoffs the
    /// automatic sweep exempts — manual ones and ones from a non-matching
    /// cwd. An operator-driven bulk expiry has to clear those, or it clears
    /// nothing at all.
    #[test]
    fn expire_open_handoffs_clears_the_handoffs_the_auto_sweep_exempts() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        seed_handoff(&conn, ws, proj, Some("/work/a"), true, None, 100);
        seed_handoff(&conn, ws, proj, Some("/elsewhere"), false, None, 200);
        seed_handoff(&conn, ws, proj, Some("/work/a"), false, None, 300);
        assert_eq!(open_handoffs(&conn), 3, "precondition");

        let expired =
            expire_open_handoffs(&mut conn, &ws, &proj, &OwnerFilter::Any, None, None).unwrap();

        assert_eq!(expired, 3, "every open handoff in scope is cleared");
        assert_eq!(open_handoffs(&conn), 0);
    }

    /// It is a state change, not a delete: the summary and provenance survive
    /// so an operator can still see what was cleared.
    #[test]
    fn expire_open_handoffs_preserves_the_rows_it_expires() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        seed_handoff(&conn, ws, proj, Some("/work/a"), true, None, 100);

        expire_open_handoffs(&mut conn, &ws, &proj, &OwnerFilter::Any, None, None).unwrap();

        let (state, summary): (String, String) = conn
            .query_row("SELECT state, summary FROM handoffs", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(state, "expired");
        assert_eq!(summary, "baton", "content is not destroyed");
    }

    /// `--older-than` lets an operator clear a backlog without touching a
    /// handoff created moments ago.
    #[test]
    fn expire_open_handoffs_respects_the_age_cutoff() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        seed_handoff(&conn, ws, proj, None, true, None, 100);
        seed_handoff(&conn, ws, proj, None, true, None, 5_000);

        let expired =
            expire_open_handoffs(&mut conn, &ws, &proj, &OwnerFilter::Any, Some(1_000), None)
                .unwrap();

        assert_eq!(expired, 1, "only the older one");
        assert_eq!(open_handoffs(&conn), 1);
    }

    /// Scope containment: another project's backlog is untouched.
    #[test]
    fn expire_open_handoffs_does_not_cross_project_boundaries() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let other = get_or_create_project(&mut conn, &ws, "other", None).unwrap();
        seed_handoff(&conn, ws, proj, None, true, None, 100);
        seed_handoff(&conn, ws, other, None, true, None, 100);

        let expired =
            expire_open_handoffs(&mut conn, &ws, &proj, &OwnerFilter::Any, None, None).unwrap();

        assert_eq!(expired, 1);
        assert_eq!(
            open_handoffs(&conn),
            1,
            "the other project's handoff survives"
        );
    }

    /// Owner scoping: expiring as one user must not clear another's baton.
    #[test]
    fn expire_open_handoffs_leaves_another_owners_baton_alone() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        seed_handoff(&conn, ws, proj, None, true, Some("user:alice"), 100);
        seed_handoff(&conn, ws, proj, None, true, Some("user:bob"), 100);
        seed_handoff(&conn, ws, proj, None, true, None, 100);

        let expired = expire_open_handoffs(
            &mut conn,
            &ws,
            &proj,
            &OwnerFilter::User("user:alice".into()),
            None,
            None,
        )
        .unwrap();

        assert_eq!(expired, 2, "alice's own plus the shared one");
        let remaining: String = conn
            .query_row(
                "SELECT owner_user FROM handoffs WHERE state = 'open'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, "user:bob");
    }

    /// Already-accepted handoffs are history, not backlog.
    #[test]
    fn expire_open_handoffs_does_not_touch_accepted_ones() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        conn.execute(
            "INSERT INTO handoffs (id, workspace_id, project_id, from_agent, summary, state, \
             created_at) VALUES (?1, ?2, ?3, 'codex', 'done', 'accepted', 1)",
            rusqlite::params![
                &uuid::Uuid::now_v7().as_bytes()[..],
                ws.as_bytes(),
                proj.as_bytes()
            ],
        )
        .unwrap();

        let expired =
            expire_open_handoffs(&mut conn, &ws, &proj, &OwnerFilter::Any, None, None).unwrap();
        assert_eq!(expired, 0);
        let state: String = conn
            .query_row("SELECT state FROM handoffs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "accepted", "an accepted handoff is not backlog");
    }

    fn embed_failure_rows(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM page_embed_failures", [], |r| r.get(0))
            .unwrap()
    }

    /// #528: a failed embed left nothing durable — the inline path warned and
    /// returned success, and the backfill's warnings died with the container.
    #[test]
    fn a_failed_embed_is_recorded_with_its_reason_and_detail() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let page_id = upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body")).unwrap();

        record_embed_failure(
            &conn,
            &page_id,
            EmbedOutcome::Failed,
            Some("400 empty Part"),
        )
        .unwrap();

        let (outcome, detail): (String, Option<String>) = conn
            .query_row(
                "SELECT outcome, detail FROM page_embed_failures WHERE page_id = ?1",
                [page_id.as_bytes()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(outcome, "failed");
        assert_eq!(detail.as_deref(), Some("400 empty Part"));
    }

    /// A success must write nothing here — the `page_embeddings` row already
    /// records it, and the reporter's first constraint was no extra write on
    /// the hot path.
    #[test]
    fn a_successful_embed_writes_no_failure_row() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let page_id = upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body")).unwrap();

        store_embedding(&mut conn, &page_id, &[0_u8; 8], "p", "m", 2).unwrap();

        assert_eq!(embed_failure_rows(&conn), 0);
    }

    /// The second constraint: a global re-embed must not erase the history.
    #[test]
    fn a_later_successful_embed_preserves_the_failure_history() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let page_id = upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body")).unwrap();
        record_embed_failure(&conn, &page_id, EmbedOutcome::Failed, Some("boom")).unwrap();

        store_embedding(&mut conn, &page_id, &[0_u8; 8], "p", "m", 2).unwrap();

        assert_eq!(
            embed_failure_rows(&conn),
            1,
            "the record of what went wrong survives the repair"
        );
    }

    /// Re-attempting overwrites rather than accumulating: one row per page.
    #[test]
    fn repeated_failures_keep_only_the_latest_attempt() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let page_id = upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body")).unwrap();

        record_embed_failure(&conn, &page_id, EmbedOutcome::Failed, Some("first")).unwrap();
        record_embed_failure(&conn, &page_id, EmbedOutcome::SkippedEmpty, None).unwrap();

        assert_eq!(embed_failure_rows(&conn), 1);
        let outcome: String = conn
            .query_row("SELECT outcome FROM page_embed_failures", [], |r| r.get(0))
            .unwrap();
        assert_eq!(outcome, "skipped_empty", "latest attempt wins");
    }

    /// A provider error can carry a whole response body; the ledger says what
    /// happened, it does not archive it.
    #[test]
    fn failure_detail_is_truncated() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let page_id = upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body")).unwrap();
        let huge = "x".repeat(5_000);

        record_embed_failure(&conn, &page_id, EmbedOutcome::Failed, Some(&huge)).unwrap();

        let detail: String = conn
            .query_row("SELECT detail FROM page_embed_failures", [], |r| r.get(0))
            .unwrap();
        assert!(detail.chars().count() <= EMBED_FAILURE_DETAIL_MAX + 1);
        assert!(detail.ends_with('…'), "truncation is visible");
    }

    /// The ledger cannot outgrow the corpus: rows go with their page.
    #[test]
    fn a_failure_row_is_removed_with_its_page() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let page_id = upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body")).unwrap();
        record_embed_failure(&conn, &page_id, EmbedOutcome::Failed, None).unwrap();

        delete_page(
            &mut conn,
            ws,
            proj,
            &PagePath::new("notes/a.md").unwrap(),
            None,
        )
        .unwrap();

        assert_eq!(embed_failure_rows(&conn), 0);
    }

    /// A failure row is the *last unsuccessful attempt*, not proof the page is
    /// still broken. `status` must separate the two, or a page that failed
    /// once and recovered reads as an outstanding problem forever.
    #[test]
    fn status_separates_unresolved_embed_failures_from_recovered_ones() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let broken = upsert_page(&mut conn, &page(ws, proj, "notes/broken.md", "b")).unwrap();
        let recovered = upsert_page(&mut conn, &page(ws, proj, "notes/ok.md", "b")).unwrap();

        record_embed_failure(&conn, &broken, EmbedOutcome::Failed, Some("boom")).unwrap();
        record_embed_failure(&conn, &recovered, EmbedOutcome::SkippedEmpty, None).unwrap();
        store_embedding(&mut conn, &recovered, &[0_u8; 8], "p", "m", 2).unwrap();

        let unresolved: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM page_embed_failures f \
                 JOIN pages pg ON pg.id = f.page_id AND pg.is_latest = 1 \
                 LEFT JOIN page_embeddings pe ON pe.page_id = f.page_id \
                 WHERE pe.page_id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let recovered_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM page_embed_failures f \
                 JOIN pages pg ON pg.id = f.page_id AND pg.is_latest = 1 \
                 JOIN page_embeddings pe ON pe.page_id = f.page_id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unresolved, 1, "only the page still lacking an embedding");
        assert_eq!(recovered_count, 1, "the repaired page is history");
    }

    /// Accepting a handoff does not make its text yours. Purging the
    /// accepting session must not destroy the authoring session's record.
    #[test]
    fn purge_session_deletes_authored_handoffs_but_not_accepted_ones() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let (author, _) = seed_session(&mut conn, ws, proj, "author");
        let (accepter, _) = seed_session(&mut conn, ws, proj, "accepter");
        conn.execute(
            "INSERT INTO handoffs (id, workspace_id, project_id, from_session_id, from_agent, \
             summary, state, created_at, accepted_by_session) \
             VALUES (?1, ?2, ?3, ?4, 'codex', 'authored-by-author', 'accepted', 1, ?5)",
            rusqlite::params![
                &uuid::Uuid::now_v7().as_bytes()[..],
                ws.as_bytes(),
                proj.as_bytes(),
                author.as_bytes(),
                accepter.as_bytes()
            ],
        )
        .unwrap();

        let summary = purge_session(&mut conn, ws, proj, accepter, None, Compaction::Skip).unwrap();
        assert_eq!(summary.handoffs_deleted, 0, "accepting is not authorship");
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM handoffs"),
            1,
            "the authoring session's handoff survives"
        );

        let summary = purge_session(&mut conn, ws, proj, author, None, Compaction::Skip).unwrap();
        assert_eq!(
            summary.handoffs_deleted, 1,
            "the author's handoff is removed"
        );
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM handoffs"), 0);
    }

    /// Purging a session that is already gone is NotFound, not a partial
    /// delete of whatever happens to match.
    #[test]
    fn purge_session_is_not_found_when_run_twice() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let (sid, _) = seed_session(&mut conn, ws, proj, "target");
        let (keep, _) = seed_session(&mut conn, ws, proj, "keep");

        purge_session(&mut conn, ws, proj, sid, None, Compaction::Skip).unwrap();
        let err = purge_session(&mut conn, ws, proj, sid, None, Compaction::Skip)
            .expect_err("already gone");
        assert!(matches!(err, StoreError::NotFound(_)));
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM sessions"),
            1,
            "the second call left the surviving session alone"
        );
        let _ = keep;
    }

    /// Pins the advertised boundary. A purged session is unreachable through
    /// the API and through search, but its bytes are still in the database
    /// file until a `rebuild` + `VACUUM` — exactly as they are for any other
    /// SQLite delete. #387 promises the former and explicitly not the latter,
    /// and this test fails if someone changes that without revisiting the
    /// claim we make to operators.
    #[test]
    fn purge_session_is_unsearchable_but_does_not_claim_byte_level_removal() {
        let (tmp, mut conn, ws, proj) = fresh_db();
        let db_path = tmp.path().join("test.sqlite");
        let (sid, _) = seed_session(&mut conn, ws, proj, "target");
        let (_keep, _) = seed_session(&mut conn, ws, proj, "keep");

        purge_session(&mut conn, ws, proj, sid, None, Compaction::Skip).unwrap();

        let gone: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM observations_fts WHERE observations_fts MATCH '\"obs-target\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(gone, 0, "the purged session is not searchable");
        let kept: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM observations_fts WHERE observations_fts MATCH '\"obs-keep\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1, "the sibling session is still searchable");

        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
        let bytes = std::fs::read(&db_path).unwrap();
        let still_on_disk = bytes.windows(10).any(|w| w == b"obs-target");
        assert!(
            still_on_disk,
            "documented boundary: purge is a logical delete. If this now fails, \
             byte-level removal was added and the #387 scope note, the CLI help \
             and the operator-facing docs all need updating to match."
        );
    }

    /// `reclaim_freed_pages` promises to rebuild *every* FTS5 index, which is
    /// what makes it safe for all three scopes to share one definition of
    /// "reclaimed". #548 established that a project or workspace delete
    /// cascades `workstream_events` out without firing the trigger that would
    /// drop its rows from `workstream_events_fts`, so an index left out of the
    /// batch keeps live entries for rows that no longer exist.
    ///
    /// The promise is checked against the schema rather than trusted. A new
    /// FTS index added later fails here, which routes whoever adds it into the
    /// decision — the alternative is that `--compact` silently skips it and
    /// text stays searchable after an operator asked for it to be reclaimed,
    /// with the response still reporting `compacted: true`.
    #[test]
    fn reclaim_rebuilds_every_fts_index_in_the_schema() {
        let (_tmp, conn, _ws, _proj) = fresh_db();
        let mut in_schema: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master \
                 WHERE type = 'table' AND sql LIKE '%USING fts5%'",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        in_schema.sort();

        let mut declared: Vec<String> = ALL_FTS_INDEXES.iter().map(|s| (*s).to_owned()).collect();
        declared.sort();

        assert_eq!(
            in_schema, declared,
            "ALL_FTS_INDEXES is out of date. `reclaim_freed_pages` rebuilds only \
             what is listed there, so an FTS index missing from it survives a \
             --compact with entries for deleted rows still in it. Add the new \
             index, or exclude it deliberately here and say why."
        );
    }

    /// The opt-in half: `Compaction::Reclaim` must actually remove the bytes
    /// the default leaves behind. Paired with
    /// `purge_session_is_unsearchable_but_does_not_claim_byte_level_removal`,
    /// which asserts the opposite for the default, so the two together pin
    /// the difference between what we promise and what we offer.
    #[test]
    fn purge_session_with_reclaim_removes_the_bytes_from_the_file() {
        let (tmp, mut conn, ws, proj) = fresh_db();
        let db_path = tmp.path().join("test.sqlite");
        let (sid, _) = seed_session(&mut conn, ws, proj, "target");
        let (_keep, _) = seed_session(&mut conn, ws, proj, "keep");

        let summary = purge_session(&mut conn, ws, proj, sid, None, Compaction::Reclaim).unwrap();
        assert!(summary.compacted, "the summary reports that VACUUM ran");

        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
        let bytes = std::fs::read(&db_path).unwrap();
        assert!(
            !bytes.windows(10).any(|w| w == b"obs-target"),
            "Reclaim must remove the purged text from the database file"
        );
        assert!(
            bytes.windows(8).any(|w| w == b"obs-keep"),
            "the surviving session's text is untouched by the compaction"
        );
    }

    /// A purge must be terminal. The events that produced a session can sit
    /// undelivered in a client hook spool for days (#493 measured spools with
    /// thousands), so without a tombstone the next drain recreates the session
    /// row and the observations land again — silently undoing a deletion an
    /// application already reported to its user.
    #[test]
    fn a_purged_session_is_not_recreated_by_late_arriving_events() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let (sid, _) = seed_session(&mut conn, ws, proj, "target");
        purge_session(&mut conn, ws, proj, sid, None, Compaction::Skip).unwrap();

        // Exactly what a spool drain delivers after the purge.
        let session = hook_session(sid, ws, proj, None);
        let err =
            begin_session(&mut conn, &session).expect_err("a purged session must not be recreated");
        assert!(matches!(err, StoreError::SessionPurged(_)), "got {err:?}");

        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM sessions"),
            0,
            "still gone"
        );
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM observations"), 0);
    }

    /// The tombstone is scoped: purging a session id in one project must not
    /// block ingest for the same id in another, or one purge could deny
    /// capture across the whole store.
    #[test]
    fn a_tombstone_does_not_block_the_same_session_id_in_another_project() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let other = get_or_create_project(&mut conn, &ws, "other", None).unwrap();
        let (sid, _) = seed_session(&mut conn, ws, proj, "target");
        purge_session(&mut conn, ws, proj, sid, None, Compaction::Skip).unwrap();

        let elsewhere = hook_session(sid, ws, other, None);
        begin_session(&mut conn, &elsewhere)
            .expect("a purge in one project must not suppress ingest in another");
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM sessions"), 1);
    }

    fn session_end_observation(session: &NewSession) -> NewObservation {
        let mut observation = hook_observation(session);
        observation.kind = ObservationKind::SessionEnd;
        observation
    }

    fn seed_ingest_key(conn: &Connection, project_id: ProjectId, key: &str, completed: bool) {
        conn.execute(
            "INSERT INTO ingest_keys (project_id, key, seen_at, completed_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                project_id.as_bytes(),
                key,
                Timestamp::now().as_microsecond(),
                completed.then_some(Timestamp::now().as_microsecond()),
            ],
        )
        .unwrap();
    }

    fn admitted_session(admission: HookSessionAdmission) -> AdmittedSession {
        match admission {
            HookSessionAdmission::Observation { session, .. }
            | HookSessionAdmission::EndOpen { session, .. }
            | HookSessionAdmission::ReEnd { session, .. }
            | HookSessionAdmission::AlreadyEnded { session } => session,
            HookSessionAdmission::InvalidMissingEnd | HookSessionAdmission::InvalidScopedEnd => {
                panic!("expected admitted session")
            }
        }
    }

    fn page(
        ws: ai_memory_core::WorkspaceId,
        proj: ai_memory_core::ProjectId,
        path: &str,
        body: &str,
    ) -> NewPage {
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

    /// A page written before entity extraction — tags in frontmatter but
    /// no entity links — is backfilled from those tags, with the link's
    /// window opening at the page version's creation. A page written with
    /// entities is left alone, and a re-run is a no-op.
    #[test]
    fn backfill_entity_index_populates_from_tags_idempotently() {
        let (_tmp, mut conn, ws, proj) = fresh_db();

        // Pre-fix page: tags present, but the write set no entities, so it
        // has no entity links — exactly what a mature store looks like.
        let mut stale = page(ws, proj, "concepts/writer.md", "the writer actor");
        stale.frontmatter_json = serde_json::json!({"tags": ["SQLite", "Concurrency"]});
        stale.entities = Vec::new();
        let stale_id = upsert_page(&mut conn, &stale).unwrap();
        let stale_created: i64 = conn
            .query_row(
                "SELECT created_at FROM pages WHERE id = ?1",
                params![stale_id.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();

        // Already-indexed page: written with entities through the normal
        // path. The backfill must not touch it.
        let mut indexed = page(ws, proj, "concepts/search.md", "fts and vectors");
        indexed.entities = vec!["fts5".into()];
        upsert_page(&mut conn, &indexed).unwrap();
        let links_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM entity_page_links", [], |r| r.get(0))
            .unwrap();

        let summary = backfill_entity_index(&mut conn).unwrap();
        assert_eq!(summary.pages_backfilled, 1, "only the tag-only page");
        assert_eq!(summary.links_added, 2, "sqlite + concurrency");

        // Entities exist, normalized, and the links carry the page
        // version's own creation instant as the window open (docs/temporal).
        let names: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT e.name FROM entities e \
                     JOIN entity_page_links l ON l.entity_id = e.id \
                     WHERE l.page_id = ?1 ORDER BY e.name",
                )
                .unwrap();
            let rows = stmt
                .query_map(params![stale_id.as_bytes()], |r| r.get(0))
                .unwrap();
            rows.collect::<Result<_, _>>().unwrap()
        };
        assert_eq!(names, vec!["concurrency".to_string(), "sqlite".to_string()]);
        let valid_from: i64 = conn
            .query_row(
                "SELECT DISTINCT valid_from FROM entity_page_links WHERE page_id = ?1",
                params![stale_id.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            valid_from, stale_created,
            "window opens at the version's created_at"
        );

        // Idempotent: a second pass finds no candidates and inserts nothing.
        let again = backfill_entity_index(&mut conn).unwrap();
        assert_eq!(again, EntityBackfillSummary::default(), "re-run is a no-op");
        let links_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM entity_page_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            links_after,
            links_before + 2,
            "the pre-existing entity page kept its single link; only the stale page gained two"
        );
    }

    #[test]
    fn page_feedback_is_scoped_rebuildable_and_audited() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let other = get_or_create_project(&mut conn, &ws, "other", None).unwrap();
        let path = PagePath::new("notes/shared.md").unwrap();
        let target_id = upsert_page(&mut conn, &page(ws, proj, path.as_str(), "target")).unwrap();
        upsert_page(&mut conn, &page(ws, other, path.as_str(), "other")).unwrap();

        let author = UserId::new();
        let now = Timestamp::now().as_microsecond();
        conn.execute(
            "INSERT INTO users (id, username, created_at) \
             VALUES (?1, 'feedback-author', ?2)",
            params![author.as_bytes(), now],
        )
        .unwrap();
        let params = crate::decay::DecayParams::default();

        let first = record_page_feedback(
            &mut conn,
            ws,
            proj,
            &path,
            FeedbackKind::Helpful,
            Some("useful"),
            Some(author),
            &params,
        )
        .unwrap()
        .unwrap();
        assert_eq!(first.0, target_id);
        assert!((first.1 - 1.25).abs() < f64::EPSILON);

        let second = record_page_feedback(
            &mut conn,
            ws,
            proj,
            &path,
            FeedbackKind::NotHelpful,
            None,
            Some(author),
            &params,
        )
        .unwrap()
        .unwrap();
        assert_eq!(second.0, target_id);
        assert!((second.1 - 1.0).abs() < f64::EPSILON);

        let target_salience: Option<f64> = conn
            .query_row(
                "SELECT salience FROM pages WHERE id = ?1",
                params![target_id.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(target_salience, Some(1.0));
        let other_salience: Option<f64> = conn
            .query_row(
                "SELECT salience FROM pages \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
                params![ws.as_bytes(), other.as_bytes(), path.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            other_salience, None,
            "same path in another scope must not move"
        );

        let events: Vec<(String, Option<String>, f64, Vec<u8>)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT kind, reason, salience_after, author_id FROM page_feedback \
                     WHERE page_id = ?1 ORDER BY rowid ASC",
                )
                .unwrap();
            stmt.query_map(params![target_id.as_bytes()], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
        };
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, "helpful");
        assert_eq!(events[0].1.as_deref(), Some("useful"));
        assert_eq!(events[0].2, 1.25);
        assert_eq!(events[1].0, "not_helpful");
        assert_eq!(events[1].2, 1.0);
        assert!(events.iter().all(|event| event.3 == author.as_bytes()[..]));

        let audit_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audit_log \
                 WHERE op = 'page_feedback' AND page_id = ?1 AND author_id = ?2",
                params![target_id.as_bytes(), author.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(audit_rows, 2);
    }

    #[test]
    fn page_feedback_noop_and_failure_leave_no_partial_state() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let params = crate::decay::DecayParams::default();
        let missing = PagePath::new("notes/missing.md").unwrap();
        assert!(
            record_page_feedback(
                &mut conn,
                ws,
                proj,
                &missing,
                FeedbackKind::Helpful,
                None,
                None,
                &params,
            )
            .unwrap()
            .is_none()
        );

        let path = PagePath::new("notes/rollback.md").unwrap();
        let page_id = upsert_page(&mut conn, &page(ws, proj, path.as_str(), "body")).unwrap();
        let err = record_page_feedback(
            &mut conn,
            ws,
            proj,
            &path,
            FeedbackKind::Wrong,
            Some("must roll back"),
            Some(UserId::new()),
            &params,
        )
        .expect_err("unknown author must violate the feedback FK");
        assert!(
            matches!(err, StoreError::Sqlite(_)),
            "unexpected error: {err}"
        );

        let (feedback_rows, audit_rows, salience): (i64, i64, Option<f64>) = conn
            .query_row(
                "SELECT \
                    (SELECT COUNT(*) FROM page_feedback), \
                    (SELECT COUNT(*) FROM audit_log WHERE op = 'page_feedback'), \
                    (SELECT salience FROM pages WHERE id = ?1)",
                params![page_id.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(feedback_rows, 0);
        assert_eq!(audit_rows, 0);
        assert_eq!(salience, None);
    }

    #[test]
    fn rewriting_a_flagged_page_retires_the_flag_but_keeps_its_event() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let path = PagePath::new("notes/versioned.md").unwrap();
        let old_id = upsert_page(&mut conn, &page(ws, proj, path.as_str(), "old")).unwrap();
        let params = crate::decay::DecayParams::default();
        record_page_feedback(
            &mut conn,
            ws,
            proj,
            &path,
            FeedbackKind::Stale,
            Some("outdated"),
            None,
            &params,
        )
        .unwrap()
        .unwrap();

        let new_id = upsert_page(&mut conn, &page(ws, proj, path.as_str(), "new")).unwrap();
        assert_ne!(old_id, new_id);
        let new_salience: Option<f64> = conn
            .query_row(
                "SELECT salience FROM pages WHERE id = ?1",
                params![new_id.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(new_salience, None);

        let (events, open_flags): (i64, i64) = conn
            .query_row(
                "SELECT \
                    (SELECT COUNT(*) FROM page_feedback WHERE page_id = ?1), \
                    (SELECT COUNT(*) FROM page_feedback f \
                     JOIN pages pg ON pg.id = f.page_id AND pg.is_latest = 1 \
                     WHERE f.page_id = ?1)",
                params![old_id.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(events, 1, "supersession keeps the append-only event");
        assert_eq!(open_flags, 0, "superseded feedback must not remain open");
    }

    /// Trickier path: upserting a page with a CHANGED body must
    /// produce a NEW row and mark the previous row `is_latest = 0`.
    /// This is the M7 supersession chain — the entire wiki versioning
    /// guarantee rides on it.
    /// V16: every page write lands an `audit_log` row whose
    /// `author_id` mirrors the NewPage's. Anonymous writes leave it
    /// NULL (the entire audit-log-by-author query pattern relies on
    /// the partial index covering only the non-NULL minority).
    #[test]
    fn audit_log_records_author_for_attributed_create_page() {
        use ai_memory_core::UserId;

        let (_tmp, mut conn, ws, proj) = fresh_db();

        // Seed a synthetic user row so the FK on author_id resolves.
        let user_id = UserId::new();
        let now = jiff::Timestamp::now().as_microsecond();
        conn.execute(
            "INSERT INTO users (id, username, name, email, created_at) \
             VALUES (?1, 'alice', NULL, NULL, ?2)",
            params![user_id.as_bytes(), now],
        )
        .unwrap();

        let mut np = page(ws, proj, "notes/by-alice.md", "alice body");
        np.author_id = Some(user_id);
        let page_id = upsert_page(&mut conn, &np).unwrap();

        let author_bytes: Vec<u8> = conn
            .query_row(
                "SELECT author_id FROM audit_log \
                 WHERE op = 'create_page' AND page_id = ?1",
                params![page_id.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        let recorded = UserId::from_slice(&author_bytes).unwrap();
        assert_eq!(
            recorded, user_id,
            "create_page audit row must carry the writer's user_id"
        );
    }

    /// Backward-compat gate (and the headline of the "no behaviour
    /// change for legacy installs" promise): anonymous writes leave
    /// audit_log.author_id NULL — the partial index stays empty for
    /// pre-multi-user history.
    #[test]
    fn audit_log_records_null_author_for_anonymous_create_page() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let np = page(ws, proj, "notes/anon.md", "anon body");
        assert!(np.author_id.is_none());
        let page_id = upsert_page(&mut conn, &np).unwrap();

        let author: Option<Vec<u8>> = conn
            .query_row(
                "SELECT author_id FROM audit_log \
                 WHERE op = 'create_page' AND page_id = ?1",
                params![page_id.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            author.is_none(),
            "anonymous writes must record audit_log.author_id = NULL"
        );
    }

    /// Supersession rows carry the SUPERSEDING author, not the
    /// original. Two consecutive attributed writes (alice then bob)
    /// yield a create_page row tagged alice and a supersede_page row
    /// tagged bob — point-in-time truth, not "latest author".
    #[test]
    fn audit_log_supersede_records_new_authors_identity() {
        use ai_memory_core::UserId;

        let (_tmp, mut conn, ws, proj) = fresh_db();
        let now = jiff::Timestamp::now().as_microsecond();
        let alice = UserId::new();
        let bob = UserId::new();
        conn.execute(
            "INSERT INTO users (id, username, name, email, created_at) \
             VALUES (?1, 'alice', NULL, NULL, ?2), \
                    (?3, 'bob',   NULL, NULL, ?2)",
            params![alice.as_bytes(), now, bob.as_bytes()],
        )
        .unwrap();

        let mut np1 = page(ws, proj, "notes/shared.md", "v1");
        np1.author_id = Some(alice);
        upsert_page(&mut conn, &np1).unwrap();

        let mut np2 = page(ws, proj, "notes/shared.md", "v2 — different body");
        np2.author_id = Some(bob);
        let v2_id = upsert_page(&mut conn, &np2).unwrap();

        let bob_bytes: Vec<u8> = conn
            .query_row(
                "SELECT author_id FROM audit_log \
                 WHERE op = 'supersede_page' AND page_id = ?1",
                params![v2_id.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            UserId::from_slice(&bob_bytes).unwrap(),
            bob,
            "supersede_page audit row must carry the SUPERSEDING author"
        );
    }

    #[test]
    fn upsert_page_supersedes_on_body_change() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let id1 = upsert_page(&mut conn, &page(ws, proj, "notes/foo.md", "v1 body")).unwrap();
        let id2 = upsert_page(&mut conn, &page(ws, proj, "notes/foo.md", "v2 body")).unwrap();

        assert_ne!(id1, id2, "supersession must produce a new row id");

        let latest_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages WHERE path = ?1 AND is_latest = 1",
                params!["notes/foo.md"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(latest_count, 1, "exactly one latest version expected");

        let total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages WHERE path = ?1",
                params!["notes/foo.md"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, 2, "old version must remain on disk for history");

        // The newest row should point at the older as its predecessor
        // (supersedes column), so chains are reconstructible.
        let supersedes: Option<Vec<u8>> = conn
            .query_row(
                "SELECT supersedes FROM pages WHERE id = ?1",
                params![&id2.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert!(supersedes.is_some(), "new row must link to its predecessor");
    }

    /// Idempotency: re-upserting the same body should NOT create a
    /// second row. The watcher's reconciliation calls upsert_page on
    /// every file on every tick — without this, a quiet repo would
    /// accumulate spurious history every 30 seconds.
    #[test]
    fn upsert_page_is_noop_when_body_unchanged() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let p = page(ws, proj, "notes/foo.md", "same body");
        let id1 = upsert_page(&mut conn, &p).unwrap();
        let id2 = upsert_page(&mut conn, &p).unwrap();

        assert_eq!(id1, id2, "identical body should not supersede");
        conn.execute(
            "UPDATE pages SET updated_at = 123 WHERE id = ?1",
            params![id1.as_bytes()],
        )
        .unwrap();
        let id3 = upsert_page(&mut conn, &p).unwrap();
        assert_eq!(id1, id3, "identical body should keep the same page id");
        let updated_at: i64 = conn
            .query_row(
                "SELECT updated_at FROM pages WHERE id = ?1",
                params![id1.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            updated_at, 123,
            "unchanged content should not dirty the row"
        );
        let total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages WHERE path = ?1",
                params!["notes/foo.md"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, 1, "no duplicate row for unchanged content");
    }

    /// OKF conformance happens at this choke point for every writer
    /// (docs/okf.md): a page stored with bare frontmatter comes back
    /// with the required `type`, a `generated {by, at}` stanza, and
    /// provenance derived from the session stamp. Breaking the
    /// conform call fails this test.
    #[test]
    fn stored_pages_are_okf_conformant() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let mut p = page(ws, proj, "gotchas/build.md", "watch out");
        p.frontmatter_json = serde_json::json!({
            "title": "Build gotcha",
            "session_id": "0192aaaa-0000-7000-8000-000000000000",
            "agent": "claude-code",
        });
        let id = upsert_page(&mut conn, &p).unwrap();
        let fm: String = conn
            .query_row(
                "SELECT frontmatter_json FROM pages WHERE id = ?1",
                params![id.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        let fm: serde_json::Value = serde_json::from_str(&fm).unwrap();
        assert_eq!(fm["type"], "Gotcha");
        assert!(
            fm["generated"]["by"]
                .as_str()
                .unwrap()
                .starts_with("process:ai-memory/")
        );
        assert!(
            fm["generated"]["at"].as_str().unwrap().ends_with('Z'),
            "generated.at stamped on the new version"
        );
        assert_eq!(
            fm["sources"][0]["resource"],
            "ai-memory://session/0192aaaa-0000-7000-8000-000000000000"
        );
        // producer keys survive as extensions
        assert_eq!(fm["agent"], "claude-code");
    }

    /// The critical interaction: conformance adds `generated.at`, which
    /// differs per write — an unchanged page must STILL be a noop, or
    /// every rewrite would supersede itself over a timestamp.
    #[test]
    fn conformance_does_not_break_idempotent_rewrites() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let mut p = page(ws, proj, "notes/idem.md", "stable body");
        p.frontmatter_json = serde_json::json!({"title": "Idem"});
        let id1 = upsert_page(&mut conn, &p).unwrap();
        let id2 = upsert_page(&mut conn, &p).unwrap();
        assert_eq!(
            id1, id2,
            "conformed rewrite of identical content superseded"
        );
        let versions: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages WHERE path = ?1",
                params!["notes/idem.md"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(versions, 1);
    }

    /// A producer that already writes a `type` keeps it verbatim — the
    /// choke point repairs, it never overrules.
    #[test]
    fn an_explicit_producer_type_survives_the_choke_point() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let mut p = page(ws, proj, "notes/custom.md", "body");
        p.frontmatter_json = serde_json::json!({"type": "Custom Kind"});
        let id = upsert_page(&mut conn, &p).unwrap();
        let fm: String = conn
            .query_row(
                "SELECT frontmatter_json FROM pages WHERE id = ?1",
                params![id.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        let fm: serde_json::Value = serde_json::from_str(&fm).unwrap();
        assert_eq!(fm["type"], "Custom Kind");
    }

    #[test]
    fn upsert_page_supersedes_on_frontmatter_change() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let mut p1 = page(ws, proj, "_slots/project_context.md", "same body");
        p1.frontmatter_json = serde_json::json!({
            "title": "Project context",
            "slot_kind": "state",
        });
        let id1 = upsert_page(&mut conn, &p1).unwrap();

        let mut p2 = p1.clone();
        p2.frontmatter_json = serde_json::json!({
            "title": "Project context",
            "slot_kind": "invariant",
        });
        let id2 = upsert_page(&mut conn, &p2).unwrap();

        assert_ne!(id1, id2, "frontmatter-only changes must supersede");
        let latest_frontmatter: String = conn
            .query_row(
                "SELECT frontmatter_json FROM pages WHERE id = ?1 AND is_latest = 1",
                params![id2.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            latest_frontmatter.contains("invariant"),
            "latest row should store the updated slot_kind"
        );
    }

    #[test]
    fn upsert_page_persists_and_resolves_links() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let mut source = page(ws, proj, "concepts/source.md", "see target");
        source.links = vec![PagePath::new("decisions/target.md").unwrap().into()];
        let source_id = upsert_page(&mut conn, &source).unwrap();

        let unresolved: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM links \
                 WHERE from_page_id = ?1 AND to_path = ?2 AND to_page_id IS NULL",
                params![source_id.as_bytes(), "decisions/target.md"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unresolved, 1, "forward link should persist unresolved");

        let target_id = upsert_page(
            &mut conn,
            &page(ws, proj, "decisions/target.md", "target body"),
        )
        .unwrap();

        let resolved: Option<Vec<u8>> = conn
            .query_row(
                "SELECT to_page_id FROM links WHERE from_page_id = ?1 AND to_path = ?2",
                params![source_id.as_bytes(), "decisions/target.md"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(resolved.as_deref(), Some(&target_id.as_bytes()[..]));
    }

    /// A `[[infra:runbooks/02.md]]` link from one project resolves to a page
    /// in a sibling project once that page exists — the cross-project edge.
    #[test]
    fn upsert_page_resolves_cross_project_link() {
        let (_tmp, mut conn, ws, scratch) = fresh_db();
        let infra = get_or_create_project(&mut conn, &ws, "infra", None).unwrap();

        let mut source = page(ws, scratch, "concepts/dep.md", "depends on infra runbook");
        source.links = vec![LinkTarget {
            workspace: None,
            project: Some("infra".into()),
            path: PagePath::new("runbooks/02.md").unwrap(),
            relation: None,
        }];
        let source_id = upsert_page(&mut conn, &source).unwrap();

        // Persisted with the scope, unresolved until the target project's page exists.
        let (to_project, resolved): (Option<String>, Option<Vec<u8>>) = conn
            .query_row(
                "SELECT to_project, to_page_id FROM links \
                 WHERE from_page_id = ?1 AND to_path = ?2",
                params![source_id.as_bytes(), "runbooks/02.md"],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(to_project.as_deref(), Some("infra"));
        assert!(
            resolved.is_none(),
            "cross-project link is unresolved before the target exists"
        );

        // Create the target in `infra` → the forward link repoints across projects.
        let target_id =
            upsert_page(&mut conn, &page(ws, infra, "runbooks/02.md", "the runbook")).unwrap();
        let resolved: Option<Vec<u8>> = conn
            .query_row(
                "SELECT to_page_id FROM links WHERE from_page_id = ?1 AND to_path = ?2",
                params![source_id.as_bytes(), "runbooks/02.md"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            resolved.as_deref(),
            Some(&target_id.as_bytes()[..]),
            "link must resolve across projects once the target lands"
        );
    }

    /// Store-boundary defense in depth: an unbounded handoff field or list
    /// (a caller that skipped its own cap) is bounded before it reaches the
    /// DB, so pathological content cannot grow the store without limit.
    #[test]
    fn insert_handoff_bounds_oversized_fields_at_the_store() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let huge = "x".repeat(HANDOFF_FIELD_MAX_BYTES * 3);
        let many: Vec<String> = (0..HANDOFF_LIST_MAX_ITEMS * 2)
            .map(|i| format!("step {i}"))
            .collect();
        let new = NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: huge.clone(),
            open_questions: vec![huge.clone()],
            next_steps: many,
            files_touched: vec![],
            owner_user: None,
        };
        let id = insert_handoff(&mut conn, &new).unwrap();

        let (summary, open_q, next_s): (String, String, String) = conn
            .query_row(
                "SELECT summary, open_questions, next_steps FROM handoffs WHERE id = ?1",
                params![&id.as_bytes()[..]],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert!(
            summary.len() <= HANDOFF_FIELD_MAX_BYTES,
            "summary must be bounded: {} bytes",
            summary.len()
        );
        let open: Vec<String> = serde_json::from_str(&open_q).unwrap();
        assert!(
            open[0].len() <= HANDOFF_FIELD_MAX_BYTES,
            "list item bounded"
        );
        let steps: Vec<String> = serde_json::from_str(&next_s).unwrap();
        assert!(
            steps.len() <= HANDOFF_LIST_MAX_ITEMS,
            "list length bounded: {}",
            steps.len()
        );
    }

    /// Handoff state machine: insert → Open; accept_handoff → Accepted
    /// with accepted_by stamped. Calling accept again must be safe
    /// (idempotent at the DB level) because hooks fire-and-forget.
    #[test]
    fn accept_handoff_transitions_open_to_accepted() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let new = NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "test summary".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        };
        let id = insert_handoff(&mut conn, &new).unwrap();

        // Pre-state: Open, accepted_by NULL.
        let (state, accepted_by): (String, Option<String>) = conn
            .query_row(
                "SELECT state, accepted_by FROM handoffs WHERE id = ?1",
                params![&id.as_bytes()[..]],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "open");
        assert!(accepted_by.is_none());

        accept_handoff(&mut conn, &handoff_acceptance(id, ws, proj)).unwrap();
        let (state, accepted_by): (String, Option<String>) = conn
            .query_row(
                "SELECT state, accepted_by FROM handoffs WHERE id = ?1",
                params![&id.as_bytes()[..]],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "accepted");
        assert_eq!(accepted_by.as_deref(), Some("codex"));

        // Idempotency: accepting an already-accepted handoff must
        // either succeed silently or fail clearly, never corrupt
        // the row. (Current impl is a no-op UPDATE with a state
        // guard.)
        let second = accept_handoff(&mut conn, &handoff_acceptance(id, ws, proj));
        assert!(second.is_ok(), "double-accept must not error");
    }

    #[test]
    fn lifecycle_only_receiver_releases_its_handoff_and_cannot_reclaim_after_end() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let receiver = SessionId::new();
        begin_session(
            &mut conn,
            &NewSession {
                id: receiver,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Codex,
                cwd: Some("/repo".into()),
                actor_user: None,
            },
        )
        .unwrap();
        let new_handoff = || NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: Some("/repo".into()),
            summary: "real pending work".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        };
        let handoff_id = insert_handoff(&mut conn, &new_handoff()).unwrap();
        let mut claim = handoff_acceptance(handoff_id, ws, proj);
        claim.accepting_session = Some(receiver);
        assert!(accept_handoff(&mut conn, &claim).unwrap());
        let second_handoff = insert_handoff(&mut conn, &new_handoff()).unwrap();
        let mut duplicate_start = handoff_acceptance(second_handoff, ws, proj);
        duplicate_start.accepting_session = Some(receiver);
        assert!(
            !accept_handoff(&mut conn, &duplicate_start).unwrap(),
            "a repeated SessionStart must not consume a second handoff"
        );

        assert_eq!(
            end_lifecycle_only_session(&mut conn, &receiver).unwrap(),
            LifecycleOnlyEndOutcome::Ended {
                reopened_handoff: Some(handoff_id)
            }
        );
        let (state, accepted_by, accepted_at, accepted_session): (
            String,
            Option<String>,
            Option<i64>,
            Option<Vec<u8>>,
        ) = conn
            .query_row(
                "SELECT state, accepted_by, accepted_at, accepted_by_session \
                 FROM handoffs WHERE id = ?1",
                params![handoff_id.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(state, "open");
        assert!(accepted_by.is_none());
        assert!(accepted_at.is_none());
        assert!(accepted_session.is_none());
        let (ended_at, summary_page): (Option<i64>, Option<Vec<u8>>) = conn
            .query_row(
                "SELECT ended_at, summary_page_id FROM sessions WHERE id = ?1",
                params![receiver.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(ended_at.is_some());
        assert!(summary_page.is_none());
        assert_eq!(audit_row_for(&conn, "release_lifecycle_only_handoff").0, 1);
        let second_state: String = conn
            .query_row(
                "SELECT state FROM handoffs WHERE id = ?1",
                params![second_handoff.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(second_state, "open");

        let next_handoff = insert_handoff(&mut conn, &new_handoff()).unwrap();
        let mut stale_claim = handoff_acceptance(next_handoff, ws, proj);
        stale_claim.accepting_session = Some(receiver);
        let error = accept_handoff(&mut conn, &stale_claim)
            .expect_err("an out-of-order startup fetch must not bind to an ended session");
        assert!(matches!(error, StoreError::InvalidState(_)));
        let state: String = conn
            .query_row(
                "SELECT state FROM handoffs WHERE id = ?1",
                params![next_handoff.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "open");
    }

    #[test]
    fn lifecycle_only_end_compare_and_set_yields_to_substantive_observation() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let receiver = SessionId::new();
        begin_session(
            &mut conn,
            &NewSession {
                id: receiver,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Codex,
                cwd: Some("/repo".into()),
                actor_user: None,
            },
        )
        .unwrap();
        let handoff_id = insert_handoff(
            &mut conn,
            &NewHandoff {
                workspace_id: ws,
                project_id: proj,
                from_session_id: None,
                from_agent: AgentKind::ClaudeCode,
                to_agent: None,
                cwd: None,
                summary: "continue".into(),
                open_questions: vec![],
                next_steps: vec![],
                files_touched: vec![],
                owner_user: None,
            },
        )
        .unwrap();
        let mut claim = handoff_acceptance(handoff_id, ws, proj);
        claim.accepting_session = Some(receiver);
        assert!(accept_handoff(&mut conn, &claim).unwrap());
        insert_observation(
            &mut conn,
            &NewObservation {
                session_id: receiver,
                workspace_id: ws,
                project_id: proj,
                kind: ObservationKind::PreToolUse,
                extension: None,
                source_event: None,
                title: "Read".into(),
                body: "README.md".into(),
                importance: 5,
            },
        )
        .unwrap();

        assert_eq!(
            end_lifecycle_only_session(&mut conn, &receiver).unwrap(),
            LifecycleOnlyEndOutcome::Substantive
        );
        let ended_at: Option<i64> = conn
            .query_row(
                "SELECT ended_at FROM sessions WHERE id = ?1",
                params![receiver.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(ended_at.is_none());
        let state: String = conn
            .query_row(
                "SELECT state FROM handoffs WHERE id = ?1",
                params![handoff_id.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "accepted");
    }

    #[test]
    fn failed_auto_handoff_insert_rolls_back_same_cwd_expiration() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let first_session = SessionId::new();
        begin_session(
            &mut conn,
            &NewSession {
                id: first_session,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: Some("/repo".into()),
                actor_user: None,
            },
        )
        .unwrap();
        let handoff = |session_id| NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: Some(session_id),
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: Some("/repo".into()),
            summary: "continue".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        };
        let first = insert_handoff(&mut conn, &handoff(first_session)).unwrap();

        let error = insert_handoff(&mut conn, &handoff(SessionId::new()))
            .expect_err("a missing source session must violate the foreign key");
        assert!(matches!(error, StoreError::Sqlite(_)));
        let state: String = conn
            .query_row(
                "SELECT state FROM handoffs WHERE id = ?1",
                params![first.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            state, "open",
            "the failed transaction must restore the prior handoff"
        );
        assert_eq!(
            audit_row_for(&conn, "expire_superseded_handoffs").0,
            0,
            "the rolled-back expiration must leave no audit row"
        );
    }

    /// The handoff lifecycle (insert / accept / cancel) writes audit rows,
    /// scoped to the handoff's workspace/project, with a NULL author (handoffs
    /// are agent/session-keyed, not owned by a DB user).
    #[test]
    fn audit_log_records_handoff_lifecycle_with_null_author() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let new = || NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "s".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        };
        let id = insert_handoff(&mut conn, &new()).unwrap();
        accept_handoff(&mut conn, &handoff_acceptance(id, ws, proj)).unwrap();
        let id2 = insert_handoff(&mut conn, &new()).unwrap();
        assert!(cancel_handoff(&mut conn, &id2, &ws, &proj, &OwnerFilter::Any).unwrap());

        for op in ["insert_handoff", "accept_handoff", "cancel_handoff"] {
            let (count, author) = audit_row_for(&conn, op);
            assert!(count >= 1, "{op} must be audited");
            assert!(author.is_none(), "{op} audit author must be NULL");
        }
        assert_eq!(
            audit_row_for(&conn, "insert_handoff").0,
            2,
            "two inserts → two rows"
        );
        // Idempotent misses stay out of the trail: a double-accept and a
        // cancel of an already-accepted handoff change no row, audit nothing.
        accept_handoff(&mut conn, &handoff_acceptance(id, ws, proj)).unwrap();
        assert!(!cancel_handoff(&mut conn, &id, &ws, &proj, &OwnerFilter::Any).unwrap());
        assert_eq!(
            audit_row_for(&conn, "accept_handoff").0,
            1,
            "a no-op double-accept must not write a second audit row"
        );
        assert_eq!(
            audit_row_for(&conn, "cancel_handoff").0,
            1,
            "cancelling a non-open handoff must not write an audit row"
        );
        let ws_bytes: Option<Vec<u8>> = conn
            .query_row(
                "SELECT workspace_id FROM audit_log WHERE op = 'accept_handoff'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            ws_bytes.as_deref(),
            Some(&ws.as_bytes()[..]),
            "handoff audit is scoped to its workspace"
        );
    }

    /// The stored cwd is normalized (trailing path separator stripped) at insert time
    /// so trailing-slash drift between agent payloads cannot break the next
    /// session's path-boundary match. Covers both manual and auto handoffs,
    /// since both go through `insert_handoff`.
    #[test]
    fn insert_handoff_strips_trailing_separator_from_cwd() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let new = NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: Some(std::path::PathBuf::from("/home/u/repo/")),
            summary: "trailing slash".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        };
        let id = insert_handoff(&mut conn, &new).unwrap();
        let cwd: Option<String> = conn
            .query_row(
                "SELECT cwd FROM handoffs WHERE id = ?1",
                params![&id.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cwd.as_deref(), Some("/home/u/repo"));

        let windows = NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: Some(std::path::PathBuf::from(r"C:\repo\")),
            summary: "trailing backslash".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        };
        let id = insert_handoff(&mut conn, &windows).unwrap();
        let cwd: Option<String> = conn
            .query_row(
                "SELECT cwd FROM handoffs WHERE id = ?1",
                params![&id.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cwd.as_deref(), Some(r"C:\repo"));
    }

    #[test]
    fn cancel_handoff_transitions_open_to_expired() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let new = NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "accidental handoff".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        };
        let id = insert_handoff(&mut conn, &new).unwrap();

        assert!(cancel_handoff(&mut conn, &id, &ws, &proj, &OwnerFilter::Any).unwrap());
        let state: String = conn
            .query_row(
                "SELECT state FROM handoffs WHERE id = ?1",
                params![&id.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "expired");

        assert!(
            !cancel_handoff(&mut conn, &id, &ws, &proj, &OwnerFilter::Any).unwrap(),
            "double-cancel should be a no-op"
        );
    }

    /// Supported hook agents persist concrete agent_kind values. V01's CHECK
    /// omitted agents added after launch; regression for hook-router WARNs.
    #[test]
    fn begin_session_accepts_all_supported_agent_kinds() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        for agent_kind in AgentKind::ALL {
            let sid = SessionId::new();
            begin_session(
                &mut conn,
                &NewSession {
                    id: sid,
                    workspace_id: ws,
                    project_id: proj,
                    agent_kind,
                    cwd: Some(std::path::PathBuf::from(r"C:\GIT\ai-memory")),
                    actor_user: None,
                },
            )
            .unwrap();

            let stored: String = conn
                .query_row(
                    "SELECT agent_kind FROM sessions WHERE id = ?1",
                    params![&sid.as_bytes()[..]],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(stored, agent_kind.as_str());
        }
    }

    /// end_session links the synthesised summary page so callers can
    /// jump straight from session row to summary.
    #[test]
    fn end_session_links_summary_page_when_provided() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let sid = SessionId::new();
        begin_session(
            &mut conn,
            &NewSession {
                id: sid,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
                actor_user: None,
            },
        )
        .unwrap();
        let page_id = upsert_page(
            &mut conn,
            &page(ws, proj, "sessions/abc.md", "summary body"),
        )
        .unwrap();
        end_session(&mut conn, &sid, Some(&page_id)).unwrap();

        let summary: Option<Vec<u8>> = conn
            .query_row(
                "SELECT summary_page_id FROM sessions WHERE id = ?1",
                params![&sid.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            summary.is_some(),
            "summary_page_id must persist when supplied"
        );
        let bytes = summary.unwrap();
        assert_eq!(bytes.len(), 16);
        assert_eq!(&bytes[..], &page_id.as_bytes()[..]);
    }

    /// end_session without a summary leaves the column NULL — the
    /// session ended but no page was synthesised (e.g. zero
    /// observations recorded). This must not be confused with the
    /// summary-linked case.
    #[test]
    fn end_session_without_summary_page_id_leaves_null() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let sid = SessionId::new();
        begin_session(
            &mut conn,
            &NewSession {
                id: sid,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
                actor_user: None,
            },
        )
        .unwrap();
        end_session(&mut conn, &sid, None).unwrap();
        let summary: Option<Vec<u8>> = conn
            .query_row(
                "SELECT summary_page_id FROM sessions WHERE id = ?1",
                params![&sid.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert!(summary.is_none());
    }

    /// Embeddings are keyed by page_id (PK). Re-storing for the same
    /// page must REPLACE, not duplicate — otherwise `ai-memory embed
    /// --reembed` would multiply rows on each run.
    #[test]
    fn store_embedding_replaces_existing_row() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let pid = upsert_page(&mut conn, &page(ws, proj, "notes/x.md", "body")).unwrap();
        store_embedding(
            &mut conn,
            &pid,
            &vec![0u8; 1536 * 4],
            "test",
            "model-a",
            1536,
        )
        .unwrap();
        store_embedding(
            &mut conn,
            &pid,
            &vec![1u8; 1536 * 4],
            "test",
            "model-b",
            1536,
        )
        .unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM page_embeddings WHERE page_id = ?1",
                params![&pid.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "embedding row must be replaced, not duplicated");

        let model: String = conn
            .query_row(
                "SELECT model FROM page_embeddings WHERE page_id = ?1",
                params![&pid.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(model, "model-b", "latest model metadata wins");
    }

    #[test]
    fn store_embeddings_batches_rows_in_one_call() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let p1 = upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body a")).unwrap();
        let p2 = upsert_page(&mut conn, &page(ws, proj, "notes/b.md", "body b")).unwrap();

        store_embeddings(
            &mut conn,
            &[
                EmbeddingWrite {
                    page_id: p1,
                    vector_bytes: vec![0u8; 4],
                    provider: "test".into(),
                    model: "model".into(),
                    dim: 1,
                },
                EmbeddingWrite {
                    page_id: p2,
                    vector_bytes: vec![1u8; 4],
                    provider: "test".into(),
                    model: "model".into(),
                    dim: 1,
                },
            ],
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM page_embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn delete_stale_page_embeddings_removes_mismatched_rows() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let other = get_or_create_project(&mut conn, &ws, "other", None).unwrap();
        let p1 = upsert_page(&mut conn, &page(ws, proj, "a.md", "body a")).unwrap();
        let p2 = upsert_page(&mut conn, &page(ws, proj, "b.md", "body b")).unwrap();
        let p3 = upsert_page(&mut conn, &page(ws, other, "c.md", "body c")).unwrap();
        let old = upsert_page(&mut conn, &page(ws, proj, "old.md", "old body")).unwrap();
        let _new = upsert_page(&mut conn, &page(ws, proj, "old.md", "new body")).unwrap();
        store_embedding(
            &mut conn,
            &p1,
            &[0u8; 4],
            "google",
            "models/gemini-embedding-001",
            768,
        )
        .unwrap();
        store_embedding(
            &mut conn,
            &p3,
            &[2u8; 4],
            "google",
            "models/gemini-embedding-001",
            768,
        )
        .unwrap();
        store_embedding(
            &mut conn,
            &p2,
            &[1u8; 4],
            "openai",
            "openai/text-embedding-3-small",
            1536,
        )
        .unwrap();
        store_embedding(
            &mut conn,
            &old,
            &[3u8; 4],
            "openai",
            "openai/text-embedding-3-small",
            1536,
        )
        .unwrap();
        let n = super::delete_stale_page_embeddings(
            &mut conn,
            &ws,
            Some(&proj),
            "openai",
            "openai/text-embedding-3-small",
            1536,
        )
        .unwrap();
        assert_eq!(n, 2);
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM page_embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 2);
        let model: String = conn
            .query_row(
                "SELECT model FROM page_embeddings WHERE page_id = ?1",
                params![&p2.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(model, "openai/text-embedding-3-small");
        let other_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM page_embeddings WHERE page_id = ?1",
                params![&p3.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            other_rows, 1,
            "explicit project purge must not touch siblings"
        );
    }

    #[test]
    fn path_search_text_indexes_slug_and_words() {
        // Both forms: hyphenated slug kept whole, plus split into words.
        assert_eq!(
            path_search_text("notes/foo-bar.md"),
            "notes foo-bar md notes foo bar md"
        );
        assert_eq!(path_search_text("a/b_c.md"), "a b_c md a b c md");
    }

    /// A page is findable by its PATH slug even when the slug appears in
    /// neither the title nor the body — the V17 `path_search` FTS column.
    #[test]
    fn fts_matches_page_by_path_slug_not_in_body() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        // Title + body deliberately avoid the slug words.
        let mut p = page(
            ws,
            proj,
            "notes/followup-bulk-rename-runbook-titles.md",
            "totally unrelated prose about elephants",
        );
        p.title = "Elephants".into();
        upsert_page(&mut conn, &p).unwrap();

        // The slug, as a quoted single token (how prepare_fts5_query renders a
        // hyphenated term), matches via the path_search column.
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages_fts \
                 WHERE pages_fts MATCH ?1",
                params!["\"followup-bulk-rename-runbook-titles\""],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "slug in path must be searchable");

        // A distinct path segment is independently searchable too.
        let seg: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages_fts WHERE pages_fts MATCH 'runbook'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(seg, 1, "path segment token must match");

        // Body words still match (regression: body stays indexed at col 1).
        let body_hit: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages_fts WHERE pages_fts MATCH 'elephants'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(body_hit, 1, "body must remain searchable");
    }

    #[test]
    fn pages_fts_path_migration_preserves_accent_folding() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let mut p = page(ws, proj, "notes/descricao.md", "descrição do projeto");
        p.title = "Descrição".into();
        upsert_page(&mut conn, &p).unwrap();

        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages_fts WHERE pages_fts MATCH 'descricao'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "page FTS should remain accent-insensitive");
    }

    #[test]
    fn pages_fts_update_trigger_ignores_access_counter_updates() {
        let (_tmp, conn, _ws, _proj) = fresh_db();
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'pages_fts_au'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            sql.contains("AFTER UPDATE OF title, body, path_search ON pages"),
            "pages_fts_au must not fire on access_count/last_accessed_at updates: {sql}"
        );
    }

    /// True move: re-stamping a project's workspace_id keeps the same
    /// project_id and carries pages, sessions and observations along —
    /// the whole point of the lossless move. The summary counts must
    /// match what actually moved.
    #[test]
    fn move_project_workspace_restamps_all_domain_rows() {
        use ai_memory_core::ObservationKind;

        let (_tmp, mut conn, src_ws, proj) = fresh_db();
        let dst_ws = get_or_create_workspace(&mut conn, "djalmajr").unwrap();

        // Seed a page, a session and an observation under the source ws.
        let page_id = upsert_page(&mut conn, &page(src_ws, proj, "notes/a.md", "body")).unwrap();
        let sid = SessionId::new();
        begin_session(
            &mut conn,
            &NewSession {
                id: sid,
                workspace_id: src_ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
                actor_user: None,
            },
        )
        .unwrap();
        insert_observation(
            &mut conn,
            &NewObservation {
                session_id: sid,
                workspace_id: src_ws,
                project_id: proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "t".into(),
                body: "b".into(),
                importance: 5,
            },
        )
        .unwrap();
        end_session(&mut conn, &sid, None).unwrap();
        crate::session_consolidation::enqueue(&mut conn, src_ws, proj, sid).unwrap();
        conn.execute(
            "INSERT INTO auto_improve_rejections \
             (id, workspace_id, project_id, reason, normalized_fingerprint, summary, created_at) \
             VALUES (?1, ?2, ?3, 'rejected', 'fingerprint', 'summary', 1)",
            params![
                uuid::Uuid::new_v4().as_bytes(),
                src_ws.as_bytes(),
                proj.as_bytes(),
            ],
        )
        .unwrap();
        let managed = crate::workstream::prepare_run(
            &mut conn,
            &crate::PrepareWorkstreamRun {
                workspace_id: src_ws,
                project_id: proj,
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                cwd: "/repo".into(),
                agent: AgentKind::Codex,
                automatic_harness: false,
                available_agents: vec![AgentKind::Codex],
                selection: crate::WorkstreamSelection::Current,
                lease_owner: "test".into(),
            },
        )
        .unwrap();

        let summary = move_project_workspace(&mut conn, &proj, &src_ws, &dst_ws).unwrap();
        assert_eq!(summary.pages_moved, 1);
        assert_eq!(summary.sessions_moved, 1);
        assert_eq!(summary.observations_moved, 1);
        assert_eq!(summary.auto_improve_rejections_moved, 1);
        assert_eq!(summary.session_consolidation_jobs_moved, 1);
        assert_eq!(summary.workstreams_moved, 1);

        // The project_id is unchanged; every row now points at dst_ws.
        // `projects` keys the project by `id`; child tables by `project_id`.
        let count_in = |table: &str, ws: &ai_memory_core::WorkspaceId| -> i64 {
            let id_col = if table == "projects" {
                "id"
            } else {
                "project_id"
            };
            conn.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE {id_col} = ?1 AND workspace_id = ?2"),
                params![&proj.as_bytes()[..], ws.as_bytes()],
                |r| r.get(0),
            )
            .unwrap()
        };
        for table in [
            "projects",
            "pages",
            "sessions",
            "observations",
            "auto_improve_rejections",
            "session_consolidation_jobs",
            "workstreams",
        ] {
            assert_eq!(count_in(table, &dst_ws), 1, "{table} must move to dst ws");
            assert_eq!(count_in(table, &src_ws), 0, "{table} must leave src ws");
        }
        let managed_run_still_attached: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM managed_runs mr \
                 JOIN workstreams w ON w.id = mr.workstream_id \
                 WHERE mr.id = ?1 AND w.workspace_id = ?2 AND w.project_id = ?3",
                params![
                    managed.run_id.as_bytes(),
                    dst_ws.as_bytes(),
                    proj.as_bytes(),
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(managed_run_still_attached, 1);
        // The page keeps its id (embeddings/links follow via page_id).
        let still_there: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages WHERE id = ?1 AND workspace_id = ?2",
                params![&page_id.as_bytes()[..], dst_ws.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still_there, 1);
    }

    /// A same-named project already in the destination workspace makes the
    /// projects UPDATE collide with UNIQUE(workspace_id, name); the whole
    /// transaction must roll back, leaving the source intact. The admin
    /// layer detects this merge case up front and routes it to copy+purge.
    #[test]
    fn move_project_workspace_rolls_back_on_name_collision() {
        let (_tmp, mut conn, src_ws, proj) = fresh_db();
        let dst_ws = get_or_create_workspace(&mut conn, "djalmajr").unwrap();
        // Destination already holds a project named "scratch".
        get_or_create_project(&mut conn, &dst_ws, "scratch", None).unwrap();
        upsert_page(&mut conn, &page(src_ws, proj, "notes/a.md", "body")).unwrap();

        let err = move_project_workspace(&mut conn, &proj, &src_ws, &dst_ws);
        assert!(err.is_err(), "name collision must fail the move");

        // Source rows untouched after rollback.
        let src_pages: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages WHERE project_id = ?1 AND workspace_id = ?2",
                params![&proj.as_bytes()[..], src_ws.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(src_pages, 1, "rollback must preserve source pages");
    }

    /// Seed one session in `(ws, proj)` with everything `move_session`
    /// re-stamps: two observations, one handoff it produced, one completed
    /// consolidation job, one auto-improve run, one scheduler claim, and two
    /// versions of its `sessions/<id>.md` page. Returns the session id.
    fn seed_movable_session(conn: &mut Connection, ws: WorkspaceId, proj: ProjectId) -> SessionId {
        use ai_memory_core::ObservationKind;

        let sid = SessionId::new();
        begin_session(
            conn,
            &NewSession {
                id: sid,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: Some("/repo/src".into()),
                actor_user: None,
            },
        )
        .unwrap();
        for n in 0..2 {
            insert_observation(
                conn,
                &NewObservation {
                    session_id: sid,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: format!("movable-obs-{n}"),
                    body: "zebra token body".into(),
                    importance: 5,
                },
            )
            .unwrap();
        }
        end_session(conn, &sid, None).unwrap();
        insert_handoff(
            conn,
            &NewHandoff {
                workspace_id: ws,
                project_id: proj,
                from_session_id: Some(sid),
                from_agent: AgentKind::ClaudeCode,
                to_agent: None,
                cwd: None,
                summary: "handoff from the movable session".into(),
                open_questions: vec![],
                next_steps: vec![],
                files_touched: vec![],
                owner_user: None,
            },
        )
        .unwrap();
        assert!(crate::session_consolidation::enqueue(conn, ws, proj, sid).unwrap());
        conn.execute(
            "UPDATE session_consolidation_jobs SET state = 'completed', completed_at = 1 \
             WHERE session_id = ?1",
            params![sid.as_bytes()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_improve_runs \
             (id, workspace_id, project_id, session_id, proposal_actor_json, created_at) \
             VALUES (?1, ?2, ?3, ?4, '{}', 1)",
            params![
                uuid::Uuid::new_v4().as_bytes(),
                ws.as_bytes(),
                proj.as_bytes(),
                sid.as_bytes()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_improve_scheduler_claims \
             (workspace_id, project_id, session_id, claimed_at) VALUES (?1, ?2, ?3, 1)",
            params![ws.as_bytes(), proj.as_bytes(), sid.as_bytes()],
        )
        .unwrap();
        let path = format!("sessions/{sid}.md");
        upsert_page(conn, &page(ws, proj, &path, "first version")).unwrap();
        upsert_page(conn, &page(ws, proj, &path, "second version")).unwrap();
        sid
    }

    /// Rows of `table` keyed to `sid` (via `session_col`) that sit in the
    /// given scope.
    fn session_rows_in_scope(
        conn: &Connection,
        table: &str,
        session_col: &str,
        sid: SessionId,
        ws: WorkspaceId,
        proj: ProjectId,
    ) -> i64 {
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM {table} \
                 WHERE {session_col} = ?1 AND workspace_id = ?2 AND project_id = ?3"
            ),
            params![sid.as_bytes(), ws.as_bytes(), proj.as_bytes()],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// `(all versions, latest versions)` of the session page in a scope.
    fn session_page_versions(
        conn: &Connection,
        sid: SessionId,
        ws: WorkspaceId,
        proj: ProjectId,
    ) -> (i64, i64) {
        let path = format!("sessions/{sid}.md");
        conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(is_latest), 0) FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3",
            params![ws.as_bytes(), proj.as_bytes(), path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    /// Observations of `sid` reachable through FTS in a scope, i.e. what a
    /// scoped search would return after the move.
    fn fts_hits_in_scope(
        conn: &Connection,
        sid: SessionId,
        ws: WorkspaceId,
        proj: ProjectId,
    ) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM observations_fts \
             JOIN observations ON observations.rowid = observations_fts.rowid \
             WHERE observations_fts MATCH 'zebra' \
               AND observations.session_id = ?1 \
               AND observations.workspace_id = ?2 AND observations.project_id = ?3",
            params![sid.as_bytes(), ws.as_bytes(), proj.as_bytes()],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// The `(table, session column)` pairs `move_session` re-stamps besides
    /// `sessions` and `pages`.
    const MOVE_SESSION_CHILD_TABLES: [(&str, &str); 5] = [
        ("observations", "session_id"),
        ("handoffs", "from_session_id"),
        ("session_consolidation_jobs", "session_id"),
        ("auto_improve_runs", "session_id"),
        ("auto_improve_scheduler_claims", "session_id"),
    ];

    fn assert_session_bundle_in_scope(
        conn: &Connection,
        sid: SessionId,
        here: (WorkspaceId, ProjectId),
        gone: (WorkspaceId, ProjectId),
    ) {
        assert_eq!(
            session_rows_in_scope(conn, "sessions", "id", sid, here.0, here.1),
            1
        );
        assert_eq!(
            session_rows_in_scope(conn, "sessions", "id", sid, gone.0, gone.1),
            0
        );
        for (table, col) in MOVE_SESSION_CHILD_TABLES {
            let expected = if table == "observations" { 2 } else { 1 };
            assert_eq!(
                session_rows_in_scope(conn, table, col, sid, here.0, here.1),
                expected,
                "{table} rows must sit in the expected scope"
            );
            assert_eq!(
                session_rows_in_scope(conn, table, col, sid, gone.0, gone.1),
                0,
                "{table} rows must not sit in the other scope"
            );
        }
    }

    #[test]
    fn move_session_dry_run_reports_counts_and_changes_nothing() {
        let (_tmp, mut conn, src_ws, src_proj) = fresh_db();
        let dst_ws = get_or_create_workspace(&mut conn, "other").unwrap();
        let dst_proj = get_or_create_project(&mut conn, &dst_ws, "target", None).unwrap();
        let sid = seed_movable_session(&mut conn, src_ws, src_proj);

        let summary = move_session(
            &mut conn,
            sid,
            dst_ws,
            dst_proj,
            PagesMode::Move,
            None,
            false,
        )
        .unwrap();
        assert!(summary.session_moved);
        assert_eq!(summary.observations, 2);
        assert_eq!(summary.handoffs, 1);
        assert_eq!(summary.consolidation_jobs, 1);
        assert_eq!(summary.auto_improve_runs, 1);
        assert_eq!(summary.auto_improve_claims, 1);
        assert_eq!(summary.page_versions_moved, 2);
        assert_eq!(summary.pages_regenerated, 0);
        assert_eq!(
            summary.page_path.as_deref(),
            Some(format!("sessions/{sid}.md").as_str())
        );
        assert_eq!(summary.from_workspace, src_ws);
        assert_eq!(summary.from_project, src_proj);
        assert_eq!(summary.to_workspace, dst_ws);
        assert_eq!(summary.to_project, dst_proj);
        assert_eq!(summary.cwd.as_deref(), Some("/repo/src"));

        // Rolled back: everything still lives in the source scope.
        assert_session_bundle_in_scope(&conn, sid, (src_ws, src_proj), (dst_ws, dst_proj));
        assert_eq!(session_page_versions(&conn, sid, src_ws, src_proj), (2, 1));
        assert_eq!(session_page_versions(&conn, sid, dst_ws, dst_proj), (0, 0));
        assert_eq!(fts_hits_in_scope(&conn, sid, src_ws, src_proj), 2);
        let audited: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audit_log WHERE op = 'move_session'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(audited, 0, "dry run must not leave an audit row");
    }

    #[test]
    fn move_session_restamps_dependent_rows_and_page_versions() {
        let (_tmp, mut conn, src_ws, src_proj) = fresh_db();
        let dst_ws = get_or_create_workspace(&mut conn, "other").unwrap();
        let dst_proj = get_or_create_project(&mut conn, &dst_ws, "target", None).unwrap();
        let sid = seed_movable_session(&mut conn, src_ws, src_proj);
        point_summary_at_latest(&mut conn, sid, src_ws, src_proj);
        let pointer = summary_page_id(&conn, sid);

        let summary = move_session(
            &mut conn,
            sid,
            dst_ws,
            dst_proj,
            PagesMode::Move,
            None,
            true,
        )
        .unwrap();
        assert!(summary.session_moved);
        assert_eq!(summary.observations, 2);
        assert_eq!(summary.page_versions_moved, 2);
        assert_eq!(summary.pages_regenerated, 0);
        assert_eq!(summary.page_versions_in_target, 2);
        // The page kept its id, so the pointer is still right.
        assert_eq!(summary_page_id(&conn, sid), pointer);

        assert_session_bundle_in_scope(&conn, sid, (dst_ws, dst_proj), (src_ws, src_proj));
        // Both versions moved and the supersession chain kept exactly one latest.
        assert_eq!(session_page_versions(&conn, sid, dst_ws, dst_proj), (2, 1));
        assert_eq!(session_page_versions(&conn, sid, src_ws, src_proj), (0, 0));
        // The FTS index followed the observations into the new scope.
        assert_eq!(fts_hits_in_scope(&conn, sid, dst_ws, dst_proj), 2);
        assert_eq!(fts_hits_in_scope(&conn, sid, src_ws, src_proj), 0);
        // cwd is historical truth and stays put.
        let cwd: Option<String> = conn
            .query_row(
                "SELECT cwd FROM sessions WHERE id = ?1",
                params![sid.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cwd.as_deref(), Some("/repo/src"));
        let audited: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audit_log \
                 WHERE op = 'move_session' AND workspace_id = ?1 AND project_id = ?2",
                params![dst_ws.as_bytes(), dst_proj.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(audited, 1);
    }

    #[test]
    fn move_session_rejects_page_path_taken_in_target_and_rolls_back() {
        let (_tmp, mut conn, src_ws, src_proj) = fresh_db();
        let dst_ws = get_or_create_workspace(&mut conn, "other").unwrap();
        let dst_proj = get_or_create_project(&mut conn, &dst_ws, "target", None).unwrap();
        let sid = seed_movable_session(&mut conn, src_ws, src_proj);
        let path = format!("sessions/{sid}.md");
        upsert_page(&mut conn, &page(dst_ws, dst_proj, &path, "already here")).unwrap();

        let err = move_session(
            &mut conn,
            sid,
            dst_ws,
            dst_proj,
            PagesMode::Move,
            None,
            true,
        )
        .expect_err("latest page at the same path in the target must block the move");
        assert!(
            matches!(&err, StoreError::PagePathTaken { path: p } if *p == path),
            "unexpected error: {err:?}"
        );
        // Nothing moved: the session rows were re-stamped inside the same
        // transaction and must have rolled back with the page check.
        assert_session_bundle_in_scope(&conn, sid, (src_ws, src_proj), (dst_ws, dst_proj));
        assert_eq!(session_page_versions(&conn, sid, src_ws, src_proj), (2, 1));
        assert_eq!(session_page_versions(&conn, sid, dst_ws, dst_proj), (1, 1));
    }

    #[test]
    fn move_session_regenerate_retires_source_page_and_moves_rows() {
        let (_tmp, mut conn, src_ws, src_proj) = fresh_db();
        let dst_ws = get_or_create_workspace(&mut conn, "other").unwrap();
        let dst_proj = get_or_create_project(&mut conn, &dst_ws, "target", None).unwrap();
        let sid = seed_movable_session(&mut conn, src_ws, src_proj);
        // A latest page at the path in the target is fine under Regenerate.
        let path = format!("sessions/{sid}.md");
        upsert_page(&mut conn, &page(dst_ws, dst_proj, &path, "already here")).unwrap();
        // The session points at its (source) latest page, as consolidation
        // leaves it.
        point_summary_at_latest(&mut conn, sid, src_ws, src_proj);

        let summary = move_session(
            &mut conn,
            sid,
            dst_ws,
            dst_proj,
            PagesMode::Regenerate,
            None,
            true,
        )
        .unwrap();
        assert!(summary.session_moved);
        assert_eq!(summary.page_versions_moved, 0);
        assert_eq!(summary.pages_regenerated, 1);
        assert_eq!(summary.page_path.as_deref(), Some(path.as_str()));

        assert_session_bundle_in_scope(&conn, sid, (dst_ws, dst_proj), (src_ws, src_proj));
        // Source versions stay where they are, none of them latest anymore,
        // and the session no longer points at the retired page.
        assert_eq!(session_page_versions(&conn, sid, src_ws, src_proj), (2, 0));
        assert_eq!(session_page_versions(&conn, sid, dst_ws, dst_proj), (1, 1));
        assert_eq!(summary_page_id(&conn, sid), None);
    }

    /// Point `sessions.summary_page_id` at the session's latest page in a
    /// scope, the way session consolidation does.
    fn point_summary_at_latest(
        conn: &mut Connection,
        sid: SessionId,
        ws: WorkspaceId,
        proj: ProjectId,
    ) {
        let path = format!("sessions/{sid}.md");
        conn.execute(
            "UPDATE sessions SET summary_page_id = ( \
                 SELECT id FROM pages \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1) \
             WHERE id = ?4",
            params![ws.as_bytes(), proj.as_bytes(), path, sid.as_bytes()],
        )
        .unwrap();
        assert!(summary_page_id(conn, sid).is_some());
    }

    fn summary_page_id(conn: &Connection, sid: SessionId) -> Option<Vec<u8>> {
        conn.query_row(
            "SELECT summary_page_id FROM sessions WHERE id = ?1",
            params![sid.as_bytes()],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn move_session_without_page_reports_no_page_path() {
        let (_tmp, mut conn, src_ws, src_proj) = fresh_db();
        let dst_ws = get_or_create_workspace(&mut conn, "other").unwrap();
        let dst_proj = get_or_create_project(&mut conn, &dst_ws, "target", None).unwrap();
        let sid = seed_movable_session(&mut conn, src_ws, src_proj);
        conn.execute(
            "DELETE FROM pages WHERE workspace_id = ?1 AND project_id = ?2",
            params![src_ws.as_bytes(), src_proj.as_bytes()],
        )
        .unwrap();

        let summary = move_session(
            &mut conn,
            sid,
            dst_ws,
            dst_proj,
            PagesMode::Move,
            None,
            true,
        )
        .unwrap();
        assert!(summary.session_moved);
        assert_eq!(summary.page_versions_moved, 0);
        assert_eq!(summary.pages_regenerated, 0);
        assert_eq!(summary.page_path, None);
        assert_eq!(summary.page_versions_in_target, 0);
    }

    #[test]
    fn move_session_unknown_session_is_not_found() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let err = move_session(
            &mut conn,
            SessionId::new(),
            ws,
            proj,
            PagesMode::Move,
            None,
            true,
        )
        .expect_err("unknown session must not succeed");
        assert!(
            matches!(err, StoreError::NotFound(_)),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn move_session_unknown_target_project_is_not_found() {
        let (_tmp, mut conn, src_ws, src_proj) = fresh_db();
        let dst_ws = get_or_create_workspace(&mut conn, "other").unwrap();
        let sid = seed_movable_session(&mut conn, src_ws, src_proj);

        // A project id that exists, but not in the requested workspace: the
        // pairing triggers only guard INSERT, so the op must refuse it itself.
        let err = move_session(
            &mut conn,
            sid,
            dst_ws,
            src_proj,
            PagesMode::Move,
            None,
            true,
        )
        .expect_err("target project outside the target workspace must be refused");
        assert!(
            matches!(err, StoreError::NotFound(_)),
            "unexpected error: {err:?}"
        );
        assert_session_bundle_in_scope(&conn, sid, (src_ws, src_proj), (dst_ws, src_proj));
    }

    /// Same scope, nothing stray: a re-home with every count at zero, not
    /// an error (the audit row is still written).
    #[test]
    fn move_session_same_scope_with_nothing_stray_reports_zero_counts() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let sid = seed_movable_session(&mut conn, ws, proj);

        let summary = move_session(&mut conn, sid, ws, proj, PagesMode::Move, None, true).unwrap();
        assert!(!summary.session_moved);
        assert_eq!(summary.observations, 0);
        assert_eq!(summary.handoffs, 0);
        assert_eq!(summary.consolidation_jobs, 0);
        assert_eq!(summary.auto_improve_runs, 0);
        assert_eq!(summary.auto_improve_claims, 0);
        assert_eq!(summary.page_versions_moved, 0);
        assert_eq!(summary.pages_regenerated, 0);
        assert_eq!(summary.page_path, None);
        assert_eq!(summary.from_workspace, ws);
        assert_eq!(summary.to_project, proj);
        assert_eq!(summary.cwd.as_deref(), Some("/repo/src"));
        assert_session_bundle_in_scope(&conn, sid, (ws, proj), (WorkspaceId::new(), proj));
        assert_eq!(session_page_versions(&conn, sid, ws, proj), (2, 1));
        let audited: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audit_log WHERE op = 'move_session'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(audited, 1);
    }

    /// Insert `n` observations of `sid` stamped into `(ws, proj)` directly,
    /// the way mid-session routing before the sticky mode scattered events
    /// into phantom projects.
    fn scatter_observations(
        conn: &mut Connection,
        sid: SessionId,
        ws: WorkspaceId,
        proj: ProjectId,
        n: usize,
    ) {
        use ai_memory_core::ObservationKind;
        for i in 0..n {
            insert_observation(
                conn,
                &NewObservation {
                    session_id: sid,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::PostToolUse,
                    extension: None,
                    source_event: None,
                    title: format!("stray-obs-{i}"),
                    body: "zebra token body".into(),
                    importance: 5,
                },
            )
            .unwrap();
        }
    }

    /// A move gathers dependent rows by session id alone, so it drains every
    /// scope they landed in — including projects the caller never named. That
    /// is deliberate, but it is not cleanly reversible, so the dry run has to
    /// say which scopes it is about to empty before anything is committed.
    #[test]
    fn move_session_dry_run_names_every_scope_it_will_drain() {
        let (_tmp, mut conn, ws, target) = fresh_db();
        let unnamed_a = get_or_create_project(&mut conn, &ws, "scratchpad", None).unwrap();
        let unnamed_b = get_or_create_project(&mut conn, &ws, "tmp", None).unwrap();
        let sid = seed_movable_session(&mut conn, ws, target);
        scatter_observations(&mut conn, sid, ws, unnamed_a, 3);
        scatter_observations(&mut conn, sid, ws, unnamed_b, 1);

        // Dry run: nothing is written, but the report must already name both
        // scopes it would empty.
        let summary =
            move_session(&mut conn, sid, ws, target, PagesMode::Move, None, false).unwrap();

        let named: Vec<(String, u64)> = summary
            .source_scopes
            .iter()
            .map(|s| (s.project_name.clone(), s.observations))
            .collect();
        assert_eq!(
            named,
            vec![("scratchpad".to_string(), 3), ("tmp".to_string(), 1)],
            "both drained scopes must be named, largest first"
        );
        assert!(
            summary.source_scopes.iter().all(|s| s.workspace_id == ws),
            "each entry carries its resolved scope ids"
        );
        // The dry run really was a dry run.
        assert_eq!(
            session_rows_in_scope(&conn, "observations", "session_id", sid, ws, unnamed_a),
            3
        );

        // The destination itself is never listed as a scope to drain.
        let confirmed =
            move_session(&mut conn, sid, ws, target, PagesMode::Move, None, true).unwrap();
        assert_eq!(confirmed.observations, 4);
        let after = move_session(&mut conn, sid, ws, target, PagesMode::Move, None, false).unwrap();
        assert!(
            after.source_scopes.is_empty(),
            "with everything in the destination there is nothing left to drain"
        );
    }

    /// The phantom-bucket case: the session row already sits in the target,
    /// some of its observations landed in another scope. Moving the session
    /// to its own scope re-homes those rows and reports the real counts.
    #[test]
    fn move_session_rehomes_stray_rows_when_session_row_already_in_target() {
        let (_tmp, mut conn, ws, target) = fresh_db();
        let stray = get_or_create_project(&mut conn, &ws, "tmp", None).unwrap();
        // Session (row, 2 observations, handoff, job, run, claim, page) in T.
        let sid = seed_movable_session(&mut conn, ws, target);
        // 3 observations of the same session stamped into S.
        scatter_observations(&mut conn, sid, ws, stray, 3);
        assert_eq!(
            session_rows_in_scope(&conn, "observations", "session_id", sid, ws, stray),
            3
        );

        // Dry run: same counts, nothing changes.
        let summary =
            move_session(&mut conn, sid, ws, target, PagesMode::Move, None, false).unwrap();
        assert!(!summary.session_moved);
        assert_eq!(summary.observations, 3);
        assert_eq!(
            session_rows_in_scope(&conn, "observations", "session_id", sid, ws, stray),
            3
        );

        let summary =
            move_session(&mut conn, sid, ws, target, PagesMode::Move, None, true).unwrap();
        assert!(!summary.session_moved, "the row was already in the target");
        assert_eq!(summary.observations, 3, "only the stray rows are counted");
        assert_eq!(summary.handoffs, 0);
        assert_eq!(summary.consolidation_jobs, 0);
        assert_eq!(summary.auto_improve_runs, 0);
        assert_eq!(summary.auto_improve_claims, 0);
        assert_eq!(summary.page_versions_moved, 0);
        assert_eq!(summary.pages_regenerated, 0);
        assert_eq!(
            summary.page_path, None,
            "the page already sits in the target"
        );
        assert_eq!(summary.page_versions_in_target, 2);
        assert_eq!(summary.from_project, target);
        assert_eq!(summary.to_project, target);

        assert_eq!(
            session_rows_in_scope(&conn, "observations", "session_id", sid, ws, target),
            5
        );
        assert_eq!(
            session_rows_in_scope(&conn, "observations", "session_id", sid, ws, stray),
            0
        );
        assert_eq!(fts_hits_in_scope(&conn, sid, ws, target), 5);
        assert_eq!(fts_hits_in_scope(&conn, sid, ws, stray), 0);
        assert_eq!(
            session_rows_in_scope(&conn, "sessions", "id", sid, ws, target),
            1
        );
        assert_eq!(session_page_versions(&conn, sid, ws, target), (2, 1));
    }

    /// A re-home also gathers the session page when its rows lie outside the
    /// target: moved under `Move` (collision rule against the target),
    /// retired under `Regenerate`.
    #[test]
    fn move_session_rehome_handles_page_rows_outside_target() {
        let (_tmp, mut conn, ws, target) = fresh_db();
        let stray = get_or_create_project(&mut conn, &ws, "tmp", None).unwrap();
        let sid = seed_movable_session(&mut conn, ws, target);
        let path = format!("sessions/{sid}.md");
        // Park the page in the stray scope: nothing at the path in T.
        conn.execute(
            "UPDATE pages SET workspace_id = ?1, project_id = ?2 WHERE path = ?3",
            params![ws.as_bytes(), stray.as_bytes(), path],
        )
        .unwrap();
        assert_eq!(session_page_versions(&conn, sid, ws, target), (0, 0));
        assert_eq!(session_page_versions(&conn, sid, ws, stray), (2, 1));

        let summary =
            move_session(&mut conn, sid, ws, target, PagesMode::Move, None, true).unwrap();
        assert!(!summary.session_moved);
        assert_eq!(summary.observations, 0);
        assert_eq!(summary.page_versions_moved, 2);
        assert_eq!(summary.page_path.as_deref(), Some(path.as_str()));
        assert_eq!(session_page_versions(&conn, sid, ws, target), (2, 1));
        assert_eq!(session_page_versions(&conn, sid, ws, stray), (0, 0));

        // Stray latest AND a latest in the target: Move collides, Regenerate
        // retires the stray one and leaves the target's page alone.
        upsert_page(&mut conn, &page(ws, stray, &path, "stray again")).unwrap();
        let err = move_session(&mut conn, sid, ws, target, PagesMode::Move, None, true)
            .expect_err("a latest page at the path in the target must block the page move");
        assert!(
            matches!(&err, StoreError::PagePathTaken { path: p } if *p == path),
            "unexpected error: {err:?}"
        );
        assert_eq!(session_page_versions(&conn, sid, ws, stray), (1, 1));
        let summary = move_session(
            &mut conn,
            sid,
            ws,
            target,
            PagesMode::Regenerate,
            None,
            true,
        )
        .unwrap();
        assert_eq!(summary.pages_regenerated, 1);
        assert_eq!(summary.page_versions_moved, 0);
        assert_eq!(session_page_versions(&conn, sid, ws, stray), (1, 0));
        assert_eq!(session_page_versions(&conn, sid, ws, target), (2, 1));
    }

    #[test]
    fn ensure_project_workspace_rejects_stale_pair_before_disk_write() {
        let (_tmp, conn, ws, proj) = fresh_db();
        let other_ws = WorkspaceId::new();

        ensure_project_workspace(&conn, &ws, &proj).unwrap();
        assert!(
            matches!(
                ensure_project_workspace(&conn, &other_ws, &proj),
                Err(StoreError::NotFound(_))
            ),
            "a stale workspace/project pair must fail before wiki writes touch disk"
        );
    }

    #[test]
    fn ensure_workspace_with_id_rejects_id_name_mismatch() {
        let (_tmp, mut conn, _ws, _proj) = fresh_db();
        let id = WorkspaceId::new();

        ensure_workspace_with_id(&mut conn, id, "from-manifest").unwrap();
        let err = ensure_workspace_with_id(&mut conn, id, "other-name").unwrap_err();

        assert!(
            matches!(err, StoreError::Duplicate(_)),
            "same workspace id with different name must fail loudly; got {err:?}"
        );
    }

    #[test]
    fn ensure_project_with_id_rejects_existing_id_mismatch() {
        let (_tmp, mut conn, ws, _proj) = fresh_db();
        let id = ProjectId::new();

        ensure_project_with_id(&mut conn, id, ws, "from-manifest", Some("/repo/a")).unwrap();
        let err =
            ensure_project_with_id(&mut conn, id, ws, "renamed", Some("/repo/a")).unwrap_err();

        assert!(
            matches!(err, StoreError::Duplicate(_)),
            "same project id with different manifest data must fail loudly; got {err:?}"
        );
    }

    /// V19 data-repair migration: observations whose `project_id`
    /// disagrees with their session's `project_id` are re-attributed
    /// to the session's project. Handoffs that carry a session id are
    /// repaired the same way. Project rows that become truly empty
    /// after repair are deleted. The migration is idempotent: re-run
    /// on a repaired DB updates / deletes nothing.
    #[test]
    fn v19_repairs_orphan_observation_attribution_and_purges_empty_projects() {
        use ai_memory_core::{NewObservation, ObservationKind, SessionId};

        // Apply migrations through V18 (not V19) so we can seed the
        // orphaned-attribution state V19 is designed to repair. If we
        // ran the full chain via `fresh_db`, V19 would already be in
        // the refinery history and re-invoking `migrations::run` below
        // would be a no-op.
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let mut conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrations::run_to(&mut conn, 18).unwrap();

        // Seed the bug shape with V18-and-earlier semantics: parent
        // project `manga-plus` and fragment project `reader` co-exist
        // in the same workspace; a session lives under `manga-plus`
        // and an observation was misattributed to `reader`.
        let ws = get_or_create_workspace(&mut conn, "default").unwrap();
        let parent = get_or_create_project(
            &mut conn,
            &ws,
            "manga-plus",
            Some("/mnt/data/Projects/manga-plus"),
        )
        .unwrap();
        let fragment = get_or_create_project(
            &mut conn,
            &ws,
            "reader",
            Some("/mnt/data/Projects/manga-plus/reader"),
        )
        .unwrap();

        let sid = SessionId::new();
        // Era-appropriate raw insert: this fixture stops at an older
        // schema on purpose (see `seed_historical_session`).
        seed_historical_session(
            &conn,
            &sid,
            &ws,
            &parent,
            ai_memory_core::AgentKind::ClaudeCode,
            Some("/mnt/data/Projects/manga-plus"),
        );

        // Three misattributed observations on the fragment.
        for i in 0..3 {
            insert_observation(
                &mut conn,
                &NewObservation {
                    session_id: sid,
                    workspace_id: ws,
                    project_id: fragment,
                    kind: ObservationKind::PreToolUse,
                    extension: None,
                    source_event: None,
                    title: format!("call {i}"),
                    body: "body".into(),
                    importance: 5,
                },
            )
            .unwrap();
        }

        // Run the repair migration (V19). Target V19 explicitly rather than
        // the open-ended `run`: this test seeds rows first (leaving cached
        // statements on `sessions`), so letting a later table-rebuild
        // migration (V20+) run here would trip SQLITE_LOCKED on its
        // `DROP TABLE sessions`. Production runs migrations before any query,
        // so the rebuild is unaffected there.
        crate::migrations::run_to(&mut conn, 19).unwrap();

        // All observations now point at the parent.
        let cnt_parent: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM observations WHERE project_id = ?1",
                params![&parent.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt_parent, 3, "observations re-attributed to parent");

        // The fragment row is gone — it's truly empty post-repair.
        let frag_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM projects WHERE id = ?1",
                params![&fragment.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(frag_rows, 0, "fragment project row deleted");

        // Parent survives; it owns its rows.
        let parent_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM projects WHERE id = ?1",
                params![&parent.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(parent_rows, 1);
    }

    // Hollow-project sweep: deletes only rows with zero data of any kind
    // and past the age cutoff; reserved names survive even when hollow.
    #[test]
    fn sweep_hollow_projects_deletes_only_old_dataless_rows() {
        use ai_memory_core::{AgentKind, NewSession, SessionId};
        let (_tmp, mut conn, ws, _scratch) = fresh_db();

        // Hollow + old: delete. (Backdate created_at 8 days.)
        let hollow = get_or_create_project(&mut conn, &ws, "zt", None).unwrap();
        // Hollow + fresh: keep (inside the grace window).
        let fresh = get_or_create_project(&mut conn, &ws, "new-probe", None).unwrap();
        // Old but has a session: keep (not hollow).
        let with_data = get_or_create_project(&mut conn, &ws, "one-off", None).unwrap();
        // Old but has a managed workstream: keep (not hollow).
        let with_workstream = get_or_create_project(&mut conn, &ws, "managed-only", None).unwrap();
        // Reserved + hollow + old: keep.
        let global =
            get_or_create_project(&mut conn, &ws, ai_memory_core::GLOBAL_SCOPE_PROJECT, None)
                .unwrap();

        let eight_days_us: i64 = 8 * 24 * 60 * 60 * 1_000_000;
        for id in [&hollow, &with_data, &with_workstream, &global] {
            conn.execute(
                "UPDATE projects SET created_at = created_at - ?1 WHERE id = ?2",
                params![eight_days_us, &id.as_bytes()[..]],
            )
            .unwrap();
        }
        begin_session(
            &mut conn,
            &NewSession {
                id: SessionId::new(),
                workspace_id: ws,
                project_id: with_data,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
                actor_user: None,
            },
        )
        .unwrap();
        let managed = open_managed_run(&mut conn, &ws, &with_workstream);

        let deleted = sweep_hollow_projects(&mut conn, 7).unwrap();
        assert_eq!(deleted, vec!["zt".to_string()]);

        let exists = |id: &ai_memory_core::ProjectId| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM projects WHERE id = ?1",
                params![&id.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(exists(&hollow), 0, "old hollow row deleted");
        assert_eq!(exists(&fresh), 1, "fresh hollow row kept (grace window)");
        assert_eq!(exists(&with_data), 1, "data-bearing row kept");
        assert_eq!(
            exists(&with_workstream),
            1,
            "managed-workstream-bearing row kept"
        );
        assert_eq!(exists(&global), 1, "reserved _global kept even when hollow");
        let workstream_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM workstreams WHERE id = ?1",
                params![&managed.workstream_id.as_bytes()[..]],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(workstream_rows, 1, "managed workstream kept");
        let run_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM managed_runs WHERE id = ?1",
                params![&managed.run_id.as_bytes()[..]],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(run_rows, 1, "managed run kept");

        // Idempotent: a second pass deletes nothing.
        assert!(sweep_hollow_projects(&mut conn, 7).unwrap().is_empty());
    }

    /// Insert a `sessions` row using only the columns that have existed since
    /// V01, for fixtures that deliberately stop at an older schema version.
    /// The production `begin_session` writes whatever columns the CURRENT
    /// schema has — including V40's `actor_user`, which these fixtures'
    /// `sessions` tables do not have yet.
    fn seed_historical_session(
        conn: &Connection,
        id: &ai_memory_core::SessionId,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        agent_kind: ai_memory_core::AgentKind,
        cwd: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO sessions \
             (id, workspace_id, project_id, agent_kind, cwd, started_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                agent_kind.as_str(),
                cwd,
                Timestamp::now().as_microsecond(),
            ],
        )
        .unwrap();
    }

    /// V27 re-runs the V19 repair for the fragments that accumulated
    /// after V19: non-git parents have no repo_path, so the v0.12.2
    /// prefix guard couldn't anchor subdirectory cwds and per-event
    /// basename derivation kept minting fragment projects. Idempotent:
    /// a second pass repairs nothing.
    #[test]
    fn v27_reattributes_nongit_fragments_and_preserves_reserved_projects() {
        use ai_memory_core::{NewObservation, ObservationKind, SessionId};

        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let mut conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrations::run_to(&mut conn, 26).unwrap();

        // Non-git parent (repo_path NULL — the production shape) plus a
        // basename fragment holding misattributed observations, and the
        // reserved projects that must survive even when empty.
        let ws = get_or_create_workspace(&mut conn, "default").unwrap();
        let parent = get_or_create_project(&mut conn, &ws, "tiktok_analysis", None).unwrap();
        let fragment = get_or_create_project(&mut conn, &ws, "sources", None).unwrap();
        let scratch = get_or_create_project(&mut conn, &ws, "scratch", None).unwrap();
        let global = get_or_create_project(&mut conn, &ws, "_global", None).unwrap();

        let sid = SessionId::new();
        // Seeded with era-appropriate SQL rather than `begin_session`: this
        // fixture stops at an older schema on purpose, and the production
        // helper writes whatever columns the CURRENT schema has.
        seed_historical_session(
            &conn,
            &sid,
            &ws,
            &parent,
            ai_memory_core::AgentKind::ClaudeCode,
            Some("/home/user/tiktok_analysis"),
        );
        for i in 0..4 {
            insert_observation(
                &mut conn,
                &NewObservation {
                    session_id: sid,
                    workspace_id: ws,
                    project_id: fragment,
                    kind: ObservationKind::PostToolUse,
                    extension: None,
                    source_event: None,
                    title: format!("call {i}"),
                    body: "body".into(),
                    importance: 5,
                },
            )
            .unwrap();
        }

        crate::migrations::run_to(&mut conn, 27).unwrap();

        let count = |sql: &str, id: &ai_memory_core::ProjectId| -> i64 {
            conn.query_row(sql, params![&id.as_bytes()[..]], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(
            count(
                "SELECT COUNT(*) FROM observations WHERE project_id = ?1",
                &parent
            ),
            4,
            "observations re-attributed to the non-git parent"
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM projects WHERE id = ?1", &fragment),
            0,
            "emptied fragment row deleted"
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM projects WHERE id = ?1", &scratch),
            1,
            "scratch is reserved and survives empty"
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM projects WHERE id = ?1", &global),
            1,
            "_global is reserved and survives empty"
        );

        // Idempotency: replay V27's statements directly (refinery won't
        // re-run an applied version); zero rows change.
        let sql = include_str!("../migrations/V27__repair_nongit_fragment_attribution.sql");
        conn.execute_batch(sql).unwrap();
        assert_eq!(
            count(
                "SELECT COUNT(*) FROM observations WHERE project_id = ?1",
                &parent
            ),
            4
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM projects WHERE id = ?1", &scratch),
            1
        );
    }

    #[test]
    fn v20_adds_grok_and_preserves_sessions_invariants_on_upgraded_db() {
        use ai_memory_core::{NewObservation, ObservationKind, SessionId};

        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let ws;
        let proj;
        let existing_sid = SessionId::new();

        {
            let mut conn = Connection::open(&db_path).unwrap();
            conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
            crate::migrations::run_to(&mut conn, 19).unwrap();
            conn.pragma_update(None, "foreign_keys", "ON").unwrap();
            ws = get_or_create_workspace(&mut conn, "default").unwrap();
            proj = get_or_create_project(&mut conn, &ws, "scratch", None).unwrap();
            // Era-appropriate raw insert: this fixture stops at an older
            // schema on purpose (see `seed_historical_session`).
            seed_historical_session(
                &conn,
                &existing_sid,
                &ws,
                &proj,
                ai_memory_core::AgentKind::ClaudeCode,
                None,
            );
            insert_observation(
                &mut conn,
                &NewObservation {
                    session_id: existing_sid,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "before v20".into(),
                    body: "existing observation survives table rebuild".into(),
                    importance: 5,
                },
            )
            .unwrap();
        }

        let mut conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        crate::migrations::run_to(&mut conn, 20).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        // Era-appropriate raw insert (see `seed_historical_session`):
        // this fixture asserts the migration's widened CHECK accepts the
        // newly added agent kind on an upgraded database.
        seed_historical_session(
            &conn,
            &SessionId::new(),
            &ws,
            &proj,
            ai_memory_core::AgentKind::Grok,
            None,
        );

        let obs_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM observations WHERE session_id = ?1",
                params![existing_sid.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(obs_count, 1, "V20 must preserve existing observations");

        let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' \
                   AND name IN ('idx_sessions_recent', 'idx_sessions_project', 'idx_sessions_started_at')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 3, "V20 must recreate sessions indexes");

        let trigger_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'trigger' AND name = 'sessions_ws_proj_pairing_ai'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            trigger_count, 1,
            "V20 must recreate the V18 pairing trigger"
        );

        let other_ws = get_or_create_workspace(&mut conn, "other").unwrap();
        let other_proj =
            get_or_create_project(&mut conn, &other_ws, "other-project", None).unwrap();
        // Raw insert on purpose: the fixture sits on an older schema, and
        // what is under test is the pairing TRIGGER rejecting a session
        // whose workspace does not own its project.
        let err = conn
            .execute(
                "INSERT INTO sessions \
                 (id, workspace_id, project_id, agent_kind, cwd, started_at) \
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
                params![
                    SessionId::new().as_bytes(),
                    ws.as_bytes(),
                    other_proj.as_bytes(),
                    ai_memory_core::AgentKind::Grok.as_str(),
                    Timestamp::now().as_microsecond(),
                ],
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("sessions.workspace_id does not match"),
            "pairing trigger must reject split-brain sessions after V20: {err}"
        );

        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk_violations, 0, "V20 must leave foreign keys clean");
    }

    #[test]
    fn v25_adds_pi_and_preserves_sessions_invariants_on_upgraded_db() {
        use ai_memory_core::SessionId;

        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let ws;
        let proj;
        let existing_sid = SessionId::new();

        {
            let mut conn = Connection::open(&db_path).unwrap();
            conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
            crate::migrations::run_to(&mut conn, 24).unwrap();
            conn.pragma_update(None, "foreign_keys", "ON").unwrap();
            ws = get_or_create_workspace(&mut conn, "default").unwrap();
            proj = get_or_create_project(&mut conn, &ws, "scratch", None).unwrap();
            // Era-appropriate raw insert: this fixture stops at an older
            // schema on purpose (see `seed_historical_session`).
            seed_historical_session(
                &conn,
                &existing_sid,
                &ws,
                &proj,
                ai_memory_core::AgentKind::ClaudeCode,
                None,
            );
        }

        let mut conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        crate::migrations::run_to(&mut conn, 25).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        // Era-appropriate raw insert (see `seed_historical_session`):
        // this fixture asserts the migration's widened CHECK accepts the
        // newly added agent kind on an upgraded database.
        seed_historical_session(
            &conn,
            &SessionId::new(),
            &ws,
            &proj,
            ai_memory_core::AgentKind::Pi,
            None,
        );

        let session_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(session_count, 2, "V25 must preserve existing sessions");

        let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' \
                   AND name IN ('idx_sessions_recent', 'idx_sessions_project', 'idx_sessions_started_at', 'idx_sessions_scope_ended')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 4, "V25 must recreate sessions indexes");

        let trigger_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'trigger' AND name = 'sessions_ws_proj_pairing_ai'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            trigger_count, 1,
            "V25 must recreate the V18 pairing trigger"
        );

        let scheduler_trigger_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'trigger' AND name = 'auto_improve_scheduler_claims_session_pairing_ai'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            scheduler_trigger_count, 1,
            "V25 must recreate the V22 scheduler/session pairing trigger"
        );

        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk_violations, 0, "V25 must leave foreign keys clean");
    }

    #[test]
    fn v28_adds_devin_and_preserves_sessions_invariants_on_upgraded_db() {
        use ai_memory_core::SessionId;

        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let ws;
        let proj;
        let existing_sid = SessionId::new();

        {
            let mut conn = Connection::open(&db_path).unwrap();
            conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
            crate::migrations::run_to(&mut conn, 25).unwrap();
            conn.pragma_update(None, "foreign_keys", "ON").unwrap();
            ws = get_or_create_workspace(&mut conn, "default").unwrap();
            proj = get_or_create_project(&mut conn, &ws, "scratch", None).unwrap();
            // Era-appropriate raw insert: this fixture stops at an older
            // schema on purpose (see `seed_historical_session`).
            seed_historical_session(
                &conn,
                &existing_sid,
                &ws,
                &proj,
                ai_memory_core::AgentKind::ClaudeCode,
                None,
            );
        }

        // Run through V26 (Zero) and V27 (unrelated data repair) too, not
        // just straight to V28: this proves the whole upgrade chain composes
        // on a pre-V26 database, not just V28 applied in isolation.
        let mut conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        crate::migrations::run_to(&mut conn, 28).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        // Era-appropriate raw insert (see `seed_historical_session`):
        // this fixture asserts the migration's widened CHECK accepts the
        // newly added agent kind on an upgraded database.
        seed_historical_session(
            &conn,
            &SessionId::new(),
            &ws,
            &proj,
            ai_memory_core::AgentKind::Devin,
            None,
        );

        let session_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(session_count, 2, "V28 must preserve existing sessions");

        let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' \
                   AND name IN ('idx_sessions_recent', 'idx_sessions_project', 'idx_sessions_started_at', 'idx_sessions_scope_ended')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 4, "V28 must recreate sessions indexes");

        let trigger_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'trigger' AND name = 'sessions_ws_proj_pairing_ai'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            trigger_count, 1,
            "V28 must recreate the V18 pairing trigger"
        );

        let scheduler_trigger_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'trigger' AND name = 'auto_improve_scheduler_claims_session_pairing_ai'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            scheduler_trigger_count, 1,
            "V28 must recreate the V22 scheduler/session pairing trigger"
        );

        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk_violations, 0, "V28 must leave foreign keys clean");

        let other_ws = get_or_create_workspace(&mut conn, "other").unwrap();
        let other_proj =
            get_or_create_project(&mut conn, &other_ws, "other-project", None).unwrap();
        // Raw insert on purpose: the fixture sits on an older schema, and
        // what is under test is the pairing TRIGGER rejecting a session
        // whose workspace does not own its project.
        let err = conn
            .execute(
                "INSERT INTO sessions \
                 (id, workspace_id, project_id, agent_kind, cwd, started_at) \
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
                params![
                    SessionId::new().as_bytes(),
                    ws.as_bytes(),
                    other_proj.as_bytes(),
                    ai_memory_core::AgentKind::Devin.as_str(),
                    Timestamp::now().as_microsecond(),
                ],
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("sessions.workspace_id does not match"),
            "pairing trigger must reject split-brain sessions after V28: {err}"
        );

        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            fk_violations, 0,
            "V28 must leave foreign keys clean after pairing test"
        );
    }

    /// V19 is idempotent: re-running on a repaired DB is a no-op.
    /// Also asserts the initial run on a clean DB (no orphans, no
    /// empty fragments) is a no-op.
    #[test]
    fn v19_is_idempotent() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        // fresh_db already ran the full chain (including V19). Seed a
        // few valid rows to ensure they survive a re-run.
        upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body")).unwrap();
        let before: (i64, i64, i64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM projects), \
                        (SELECT COUNT(*) FROM observations), \
                        (SELECT COUNT(*) FROM pages)",
                params![],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        crate::migrations::run(&mut conn).unwrap();
        let after: (i64, i64, i64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM projects), \
                        (SELECT COUNT(*) FROM observations), \
                        (SELECT COUNT(*) FROM pages)",
                params![],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            before, after,
            "V19 must be a no-op on already-repaired data"
        );
    }

    /// `scratch` keeps its standalone handoffs even when it would
    /// otherwise look empty. CLAUDE.md invariant #15a names it as the
    /// defensive default for hook events that arrive without a usable
    /// cwd; the V19 DELETE explicitly carves it out.
    #[test]
    fn v19_preserves_scratch_with_standalone_handoffs() {
        use ai_memory_core::{AgentKind, NewHandoff};

        let (_tmp, mut conn, ws, _proj) = fresh_db();
        // Add a standalone handoff to scratch (no from_session_id).
        let scratch = get_or_create_project(&mut conn, &ws, "scratch", None).unwrap();
        insert_handoff(
            &mut conn,
            &NewHandoff {
                workspace_id: ws,
                project_id: scratch,
                from_session_id: None,
                from_agent: AgentKind::ClaudeCode,
                to_agent: None,
                cwd: None,
                summary: "standalone".into(),
                open_questions: vec![],
                next_steps: vec![],
                files_touched: vec![],
                owner_user: None,
            },
        )
        .unwrap();

        crate::migrations::run(&mut conn).unwrap();

        let scratch_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM projects WHERE name = 'scratch'",
                params![],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            scratch_rows, 1,
            "scratch must survive even if it looks empty"
        );
        let scratch_handoffs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM handoffs WHERE project_id = ?1",
                params![&scratch.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(scratch_handoffs, 1);
    }

    /// Seed a minimal `pages` row against a pre-V36 schema. Migration-era
    /// tests run old schemas, where current `upsert_page` — which writes
    /// the V36 `expires_at` column — cannot be used to seed fixtures.
    pub(crate) fn insert_page_pre_v36(conn: &Connection, page: &NewPage) -> PageId {
        let id = PageId::new();
        let now = Timestamp::now().as_microsecond();
        let body_sha256: [u8; 32] = {
            let mut hasher = Sha256::new();
            hasher.update(page.body.as_bytes());
            hasher.finalize().into()
        };
        conn.execute(
            "INSERT INTO pages \
             (id, workspace_id, project_id, path, path_search, title, tier, body, \
              body_sha256, frontmatter_json, is_latest, pinned, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11, ?12, ?12)",
            params![
                id.as_bytes(),
                page.workspace_id.as_bytes(),
                page.project_id.as_bytes(),
                page.path.as_str(),
                path_search_text(page.path.as_str()),
                page.title,
                page.tier.as_str(),
                page.body,
                body_sha256.as_slice(),
                serde_json::to_string(&page.frontmatter_json).unwrap(),
                i64::from(page.pinned),
                now,
            ],
        )
        .unwrap();
        id
    }

    #[test]
    fn v18_migration_refuses_existing_split_brain_rows() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let mut conn = Connection::open(&db_path).unwrap();
        crate::migrations::run_to(&mut conn, 17).unwrap();

        let src_ws = get_or_create_workspace(&mut conn, "src").unwrap();
        let stale_ws = get_or_create_workspace(&mut conn, "stale").unwrap();
        let proj = get_or_create_project(&mut conn, &src_ws, "scratch", None).unwrap();
        let mut bad_page = page(src_ws, proj, "notes/split.md", "body");
        bad_page.workspace_id = stale_ws;
        insert_page_pre_v36(&conn, &bad_page);

        let err = crate::migrations::run_to(&mut conn, 18).unwrap_err();
        assert!(
            err.to_string().contains("CHECK constraint failed"),
            "V18 must abort instead of preserving split-brain rows: {err}"
        );
    }

    /// V18 integrity triggers: an INSERT whose `workspace_id` disagrees with
    /// the project's actual workspace ABORTs (the split-brain a stale hook
    /// cache would otherwise create), while the consistent pair inserts fine.
    #[test]
    fn insert_with_mismatched_workspace_is_rejected() {
        use ai_memory_core::ObservationKind;

        let (_tmp, mut conn, ws, proj) = fresh_db();
        let other_ws = get_or_create_workspace(&mut conn, "other").unwrap();

        // A page under the WRONG workspace (project lives in `ws`) is refused.
        let mut bad_page = page(ws, proj, "notes/a.md", "body");
        bad_page.workspace_id = other_ws;
        assert!(
            upsert_page(&mut conn, &bad_page).is_err(),
            "page insert with mismatched workspace must abort"
        );

        // The consistent pair inserts fine.
        upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body")).unwrap();

        // The session insert is guarded too: a mismatched pair aborts.
        let bad_sid = SessionId::new();
        assert!(
            begin_session(
                &mut conn,
                &NewSession {
                    id: bad_sid,
                    workspace_id: other_ws,
                    project_id: proj,
                    agent_kind: AgentKind::ClaudeCode,
                    cwd: None,
                    actor_user: None,
                },
            )
            .is_err(),
            "session insert with mismatched workspace must abort"
        );

        let sid = SessionId::new();
        begin_session(
            &mut conn,
            &NewSession {
                id: sid,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
                actor_user: None,
            },
        )
        .unwrap();

        // The split-brain case the maintainer flagged: a hook writes an
        // observation with a stale workspace id for a moved project.
        let mismatched_obs = NewObservation {
            session_id: sid,
            workspace_id: other_ws,
            project_id: proj,
            kind: ObservationKind::UserPrompt,
            extension: None,
            source_event: None,
            title: "t".into(),
            body: "b".into(),
            importance: 5,
        };
        assert!(
            insert_observation(&mut conn, &mismatched_obs).is_err(),
            "observation insert with mismatched workspace must abort"
        );

        // Same observation under the correct workspace is accepted.
        let good_obs = NewObservation {
            workspace_id: ws,
            ..mismatched_obs
        };
        insert_observation(&mut conn, &good_obs).unwrap();

        // The handoff insert is the fourth INSERT trigger; audit flagged
        // that the original V18 test omitted it, so the only coverage
        // for the handoffs trigger was the temp-table CHECK on migration.
        // Assert the BEFORE INSERT trigger fires on a stale pair, and
        // a corrected pair lands cleanly.
        let mismatched_handoff = NewHandoff {
            workspace_id: other_ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "stale".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        };
        assert!(
            insert_handoff(&mut conn, &mismatched_handoff).is_err(),
            "handoff insert with mismatched workspace must abort"
        );
        let good_handoff = NewHandoff {
            workspace_id: ws,
            ..mismatched_handoff
        };
        insert_handoff(&mut conn, &good_handoff).unwrap();
    }

    /// Regression for the rename-vs-purge race that live exploration
    /// caught: a `rename_project` for a row that was deleted between
    /// the admin handler's `lookup_ws_proj_no_create` and the
    /// `UPDATE projects` used to silently return `Ok(())` — the admin
    /// endpoint then responded `200 OK` for an operation that touched
    /// zero rows, contradicting the concurrent purge's (also `200 OK`)
    /// destruction of the same project. After the fix, the writer
    /// returns `StoreError::NotFound`, which the admin handler maps to
    /// `404 Not Found`. Pins both the writer-side semantic and a
    /// concrete recipe for the failure shape so a future refactor
    /// can't quietly downgrade the error back to a silent Ok.
    #[test]
    fn rename_project_after_purge_returns_not_found() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        // Simulate the post-purge state: project row gone.
        // `purge_project` drives the cascading deletes we want here.
        let _ = purge_project(
            &mut conn,
            &ws,
            &proj,
            "default/scratch",
            None,
            false,
            Compaction::Skip,
        )
        .expect("purge of fresh project should succeed");
        // Now try to rename the project that no longer exists. The
        // pre-fix code returned `Ok(())` because `UPDATE` affected
        // zero rows. The fix returns `NotFound` so admin handlers
        // can respond 404 honestly.
        let err = rename_project(&mut conn, &ws, &proj, "renamed", None)
            .expect_err("rename of purged project must error");
        match err {
            StoreError::NotFound(_) => {}
            other => panic!("expected StoreError::NotFound, got {other:?}"),
        }
    }

    /// Belt-and-suspenders for the common path: rename of an existing
    /// project still succeeds. Without this, a future "always return
    /// NotFound" regression would also pass the test above by accident.
    #[test]
    fn rename_project_of_live_project_succeeds() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        rename_project(&mut conn, &ws, &proj, "renamed-live", None)
            .expect("rename of live project must succeed");
    }

    fn seed_user(conn: &Connection, username: &str) -> ai_memory_core::UserId {
        crate::users::insert_human_user(
            conn,
            &ai_memory_core::NewUser {
                username: username.to_string(),
                name: None,
                email: None,
            },
            ai_memory_core::UserRole::User,
            None,
            false,
        )
        .expect("seed user")
    }

    fn audit_row_for(conn: &Connection, op: &str) -> (i64, Option<Vec<u8>>) {
        conn.query_row(
            "SELECT COUNT(*), MAX(author_id) FROM audit_log WHERE op = ?1",
            [op],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    /// Attribution guard: a `purge_project` leaves exactly one `audit_log`
    /// row tagged with the op + the authenticated operator — the
    /// point-in-time answer to "who wiped this project?" (V16's motivating
    /// case). Without the audit call the count would be 0.
    #[test]
    fn audit_log_records_purge_project_with_author() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let author = seed_user(&conn, "alice");

        purge_project(
            &mut conn,
            &ws,
            &proj,
            "default/scratch",
            Some(author),
            false,
            Compaction::Skip,
        )
        .expect("purge should succeed");

        let (count, op_author) = audit_row_for(&conn, "purge_project");
        assert_eq!(count, 1, "exactly one purge_project audit row");
        assert_eq!(
            op_author.as_deref(),
            Some(&author.as_bytes()[..]),
            "audit row must carry the purging operator"
        );
    }

    /// Open one managed run on `proj` so the purge guard has something to
    /// refuse. Mirrors what `ai-memory run` does on launch.
    fn open_managed_run(
        conn: &mut Connection,
        ws: &ai_memory_core::WorkspaceId,
        proj: &ai_memory_core::ProjectId,
    ) -> crate::workstream::PreparedWorkstreamRun {
        crate::workstream::prepare_run(
            conn,
            &crate::workstream::PrepareWorkstreamRun {
                workspace_id: *ws,
                project_id: *proj,
                repo_fingerprint: "repo-fp".to_string(),
                worktree_fingerprint: "worktree-fp".to_string(),
                cwd: "/tmp/checkout".to_string(),
                agent: ai_memory_core::AgentKind::ClaudeCode,
                automatic_harness: false,
                available_agents: Vec::new(),
                selection: crate::workstream::WorkstreamSelection::Current,
                lease_owner: "test".to_string(),
            },
        )
        .expect("opening a managed run should succeed")
    }

    /// `workstreams` cascades out of `projects` and `managed_runs` cascades
    /// out of `workstreams`, so purging a project silently tore the lease row
    /// out from under a live agent: its heartbeat then failed with
    /// `409 managed run lease is not active` every 30s and its transcript
    /// never reached the ledger. The purge must refuse instead.
    #[test]
    fn purge_project_refuses_while_a_managed_run_is_active() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        open_managed_run(&mut conn, &ws, &proj);

        let err = purge_project(
            &mut conn,
            &ws,
            &proj,
            "default/scratch",
            None,
            false,
            Compaction::Skip,
        )
        .expect_err("an active managed run must block the purge");

        match err {
            StoreError::ManagedRunActive { count, workstreams } => {
                assert_eq!(count, 1, "one active run");
                assert!(
                    workstreams.contains("default"),
                    "the error names the workstream so the operator can find the session: {workstreams}"
                );
            }
            other => panic!("expected ManagedRunActive, got {other:?}"),
        }

        let survived: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM projects WHERE id = ?1",
                rusqlite::params![&proj.as_bytes()[..]],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(survived, 1, "a refused purge must not delete anything");
    }

    /// `force` is the deliberate override, and the summary must account for
    /// what the cascade took with it — the counters the pre-fix report showed
    /// (`0 pages, 0 sessions, …`) made a scope carrying a live workstream look
    /// safe to delete.
    #[test]
    fn forced_purge_reports_the_workstreams_it_cascaded() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let prepared = open_managed_run(&mut conn, &ws, &proj);

        let summary = purge_project(
            &mut conn,
            &ws,
            &proj,
            "default/scratch",
            None,
            true,
            Compaction::Skip,
        )
        .expect("force purges regardless of the live lease");

        assert_eq!(summary.workstreams_deleted, 1);
        assert_eq!(summary.managed_runs_deleted, 1);
        assert_eq!(
            summary.workstream_ids,
            vec![prepared.workstream_id.to_string()],
            "the raw/workstreams/<id>/ dir is reported for post-commit cleanup"
        );
    }

    /// A crashed wrapper leaves `state = 'active'` behind forever — the only
    /// sweep that flips it to `'expired'` lives in `prepare_run`. Guarding on
    /// the state alone would let one dead session block every future purge of
    /// the project, so the guard must read the lease expiry too.
    #[test]
    fn purge_project_ignores_a_managed_run_whose_lease_expired() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        open_managed_run(&mut conn, &ws, &proj);
        // Simulate the crash: the row still says 'active', but nothing has
        // heartbeated it, so its lease lapsed.
        conn.execute(
            "UPDATE managed_runs SET lease_expires_at = ?1 WHERE state = 'active'",
            rusqlite::params![Timestamp::now().as_microsecond() - 1],
        )
        .unwrap();

        let summary = purge_project(
            &mut conn,
            &ws,
            &proj,
            "default/scratch",
            None,
            false,
            Compaction::Skip,
        )
        .expect("a lapsed lease is not a running agent");

        assert_eq!(summary.workstreams_deleted, 1);
        assert_eq!(summary.managed_runs_deleted, 1);
    }

    /// A `rename_project` writes an attributed audit row, committed
    /// atomically with the rename.
    #[test]
    fn audit_log_records_rename_project_with_author() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let author = seed_user(&conn, "bob");

        rename_project(&mut conn, &ws, &proj, "renamed", Some(author))
            .expect("rename should succeed");

        let (count, op_author) = audit_row_for(&conn, "rename_project");
        assert_eq!(count, 1, "exactly one rename_project audit row");
        assert_eq!(op_author.as_deref(), Some(&author.as_bytes()[..]));
    }

    /// A failed rename (name collision) writes NO audit row — the tx that
    /// wraps the UPDATE + audit rolls back as a unit.
    #[test]
    fn audit_log_omits_rename_project_on_collision() {
        let (_tmp, mut conn, ws, _proj) = fresh_db();
        let author = seed_user(&conn, "carol");
        // Second project whose name we'll collide with.
        let other = get_or_create_project(&mut conn, &ws, "taken", None).unwrap();

        let err = rename_project(&mut conn, &ws, &other, "scratch", Some(author))
            .expect_err("rename onto an existing name must fail");
        assert!(matches!(err, StoreError::ProjectNameTaken(_)));

        let (count, _) = audit_row_for(&conn, "rename_project");
        assert_eq!(count, 0, "a rolled-back rename must leave no audit row");
    }

    /// Deleting an existing page writes ONE attributed `delete_page` audit row
    /// pointing at the deleted page id — the "who deleted the gotcha page?"
    /// case V16 names.
    #[test]
    fn audit_log_records_delete_page_with_author_and_page_id() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let author = seed_user(&conn, "dave");
        let pid = upsert_page(&mut conn, &page(ws, proj, "notes/doomed.md", "body")).unwrap();

        delete_page(
            &mut conn,
            ws,
            proj,
            &PagePath::new("notes/doomed.md").unwrap(),
            Some(author),
        )
        .unwrap();

        let (count, op_author) = audit_row_for(&conn, "delete_page");
        assert_eq!(count, 1, "exactly one delete_page audit row");
        assert_eq!(op_author.as_deref(), Some(&author.as_bytes()[..]));
        let page_id: Option<Vec<u8>> = conn
            .query_row(
                "SELECT page_id FROM audit_log WHERE op = 'delete_page'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            page_id.as_deref(),
            Some(&pid.as_bytes()[..]),
            "audit row must point at the deleted page id"
        );
    }

    /// A delete that matches no row (idempotent no-op) writes NO audit row.
    #[test]
    fn audit_log_omits_delete_page_noop() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        delete_page(
            &mut conn,
            ws,
            proj,
            &PagePath::new("notes/ghost.md").unwrap(),
            None,
        )
        .unwrap();
        let (count, _) = audit_row_for(&conn, "delete_page");
        assert_eq!(count, 0, "a no-op delete must leave no audit row");
    }

    /// Run the reader's exact FTS5 `MATCH` against the real, populated
    /// `pages_fts` index — the path the web search / MCP query take.
    /// Returns the matched paths (and surfaces any FTS5 syntax error as
    /// an `Err`, the way the bug originally manifested).
    fn fts_match_paths(conn: &Connection, raw: &str) -> rusqlite::Result<Vec<String>> {
        let fts_query = crate::fts_query::prepare_fts5_query(raw);
        let mut stmt = conn.prepare(
            "SELECT pages.path \
             FROM pages_fts \
             JOIN pages ON pages.rowid = pages_fts.rowid \
             WHERE pages_fts MATCH ?1 AND pages.is_latest = 1 \
             ORDER BY pages_fts.rank",
        )?;
        let rows = stmt.query_map(params![fts_query], |r| r.get::<_, String>(0))?;
        rows.collect()
    }

    /// End-to-end regression for the dotted-filename search bug (PR #81).
    /// Searching `current.md` used to reach FTS5 **bare** and SQLite
    /// errored with `fts5: syntax error near "."`, so the web UI showed
    /// "No results" and the MCP surfaced the raw error. The string-level
    /// `fts_query` unit tests only proved the *output* was quoted — they
    /// never exercised real FTS5. This drives the actual indexed
    /// `pages_fts` (via `upsert_page` → `path_search` triggers) to prove
    /// the prepared query (a) does not error and (b) matches the page at
    /// `reference/architecture-current.md`. This is the scenario that
    /// would have caught the bug *before* it shipped.
    #[test]
    fn dotted_filename_search_matches_indexed_path() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        upsert_page(
            &mut conn,
            &page(ws, proj, "reference/architecture-current.md", "body text"),
        )
        .unwrap();

        // The prepared query must not error AND must find the page.
        let hits = fts_match_paths(&conn, "current.md")
            .expect("dotted-filename search must not raise an FTS5 syntax error");
        assert!(
            hits.iter()
                .any(|p| p == "reference/architecture-current.md"),
            "search for `current.md` should match the indexed path; got {hits:?}"
        );

        // Guard the sanitizer is load-bearing: the same token reaching
        // FTS5 bare (the pre-fix behaviour) is a hard syntax error.
        let bare = conn
            .prepare("SELECT rowid FROM pages_fts WHERE pages_fts MATCH ?1")
            .unwrap()
            .query_map(params!["current.md"], |r| r.get::<_, i64>(0))
            .and_then(Iterator::collect::<rusqlite::Result<Vec<i64>>>);
        assert!(
            bare.is_err(),
            "raw `current.md` should error in FTS5 — if this passes, the \
             quoting sanitizer is no longer load-bearing and the test above \
             proves nothing"
        );
    }

    /// Regression for the live-found hyphen bug: searching `ui-refresh`
    /// returned nothing in prod even though
    /// `follow-ups/ui-refresh-scroll-restoration.md` exists. The first fix
    /// quoted it as `"ui-refresh"`, which **does not error but also does not
    /// match** the indexed `ui refresh` — only `"ui refresh"` (sub-token
    /// phrase) does. The string-level test can't see this; this drives real
    /// FTS5 against the real `path_search` index. It would have caught the
    /// bug the dotted-only fix left behind.
    #[test]
    fn hyphenated_filename_search_matches_indexed_path() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        upsert_page(
            &mut conn,
            &page(
                ws,
                proj,
                "follow-ups/ui-refresh-scroll-restoration.md",
                "body text",
            ),
        )
        .unwrap();

        let hits = fts_match_paths(&conn, "ui-refresh")
            .expect("hyphenated search must not raise an FTS5 syntax error");
        assert!(
            hits.iter()
                .any(|p| p == "follow-ups/ui-refresh-scroll-restoration.md"),
            "search for `ui-refresh` should match the indexed path; got {hits:?}"
        );

        // Pin the exact FTS5 quirk the fix works around: the keeps-the-hyphen
        // phrase matches nothing, the spaces phrase matches. If this ever
        // flips, the sub-token quoting is no longer load-bearing.
        let count = |q: &str| -> i64 {
            conn.query_row(
                "SELECT count(*) FROM pages_fts WHERE pages_fts MATCH ?1",
                params![q],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            count("\"ui-refresh\""),
            0,
            "kept-hyphen phrase must not match"
        );
        assert_eq!(count("\"ui refresh\""), 1, "sub-token phrase must match");
    }

    /// Issue #103 legacy heal: the two broad sentinels ($HOME and `/`) are
    /// always NULLed. Here the inputs are non-existent fake paths, so the
    /// nested row is preserved by the multi-user/unmounted safety rule (a
    /// `repo_path` absent on this host is left alone), not merely because it
    /// is non-sentinel -- the filesystem broadening is covered separately by
    /// `heal_catch_all_repo_paths_uses_filesystem_to_judge_catch_alls`.
    #[test]
    fn heal_catch_all_repo_paths_nulls_home_and_root_only() {
        let (_tmp, mut conn, ws, _proj) = fresh_db();

        let read_repo_path = |conn: &Connection, id: &ProjectId| -> Option<String> {
            conn.query_row(
                "SELECT repo_path FROM projects WHERE id = ?1",
                params![id.as_bytes()],
                |row| row.get(0),
            )
            .unwrap()
        };

        let home = get_or_create_project(&mut conn, &ws, "home", Some("/home/tester")).unwrap();
        let root = get_or_create_project(&mut conn, &ws, "root", Some("/")).unwrap();
        let nested =
            get_or_create_project(&mut conn, &ws, "app", Some("/home/tester/projects/app"))
                .unwrap();
        let none = get_or_create_project(&mut conn, &ws, "none", None).unwrap();

        let healed = heal_catch_all_repo_paths(&mut conn, Some("/home/tester")).unwrap();
        assert_eq!(healed, 2, "only the $HOME and `/` rows should be healed");

        assert_eq!(
            read_repo_path(&conn, &home),
            None,
            "$HOME row must be NULLed"
        );
        assert_eq!(read_repo_path(&conn, &root), None, "`/` row must be NULLed");
        assert_eq!(
            read_repo_path(&conn, &nested),
            Some("/home/tester/projects/app".to_string()),
            "nested fake path does not exist on disk, so the safety rule preserves it"
        );
        assert_eq!(read_repo_path(&conn, &none), None, "NULL row stays NULL");

        // Idempotent: a second pass over a healed DB changes nothing.
        assert_eq!(
            heal_catch_all_repo_paths(&mut conn, Some("/home/tester")).unwrap(),
            0,
            "re-running the heal must be a no-op"
        );
    }

    /// Filesystem broadening of the #103 heal: a real cwd captured as a
    /// catch-all (a non-git ancestor like `~/projects`) is NULLed, a real git
    /// work-tree root is preserved, and a path absent on this host is left
    /// alone (multi-user/unmounted safety).
    #[test]
    fn heal_catch_all_repo_paths_uses_filesystem_to_judge_catch_alls() {
        let (_tmp, mut conn, ws, _proj) = fresh_db();
        let fixtures = TempDir::new().unwrap();

        let read_repo_path = |conn: &Connection, id: &ProjectId| -> Option<String> {
            conn.query_row(
                "SELECT repo_path FROM projects WHERE id = ?1",
                params![id.as_bytes()],
                |row| row.get(0),
            )
            .unwrap()
        };

        // Real non-git ancestor (the legacy `~/projects`-style catch-all).
        let non_git = fixtures.path().join("projects");
        std::fs::create_dir_all(&non_git).unwrap();
        let non_git_path = non_git.to_str().unwrap().to_string();
        let non_git_proj =
            get_or_create_project(&mut conn, &ws, "non_git", Some(&non_git_path)).unwrap();

        // Real git work-tree root (legitimate prefix key).
        let git_root = fixtures.path().join("repo");
        std::fs::create_dir_all(git_root.join(".git")).unwrap();
        let git_path = git_root.to_str().unwrap().to_string();
        let git_proj = get_or_create_project(&mut conn, &ws, "git_root", Some(&git_path)).unwrap();

        // Path absent on this host (do NOT create it).
        let gone = fixtures.path().join("gone");
        let gone_path = gone.to_str().unwrap().to_string();
        let gone_proj = get_or_create_project(&mut conn, &ws, "gone", Some(&gone_path)).unwrap();

        let healed = heal_catch_all_repo_paths(&mut conn, None).unwrap();
        assert_eq!(
            healed, 1,
            "only the real non-git catch-all should be healed"
        );

        assert_eq!(
            read_repo_path(&conn, &non_git_proj),
            None,
            "real non-git ancestor must be healed"
        );
        assert_eq!(
            read_repo_path(&conn, &git_proj),
            Some(normalize_repo_path_key(&git_path)),
            "real git work-tree root must be preserved"
        );
        assert_eq!(
            read_repo_path(&conn, &gone_proj),
            Some(normalize_repo_path_key(&gone_path)),
            "path absent on this host must be preserved (multi-user/unmounted safety)"
        );
    }

    #[test]
    fn hook_admission_rejects_cross_owner_fresh_key_without_claiming_it() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let alice = "user:alice";
        let bob = "user:bob";
        let session = hook_session(SessionId::new(), ws, proj, Some(alice));
        begin_session(&mut conn, &session).unwrap();
        let bob_event = hook_observation(&session);

        assert!(matches!(
            admit_hook_session_event(
                &mut conn,
                &session,
                &bob_event,
                &OwnerFilter::User(bob.into()),
                Some("fresh-key"),
            ),
            Err(StoreError::SessionCollision)
        ));
        let observations: u64 = conn
            .query_row("SELECT COUNT(*) FROM observations", [], |row| row.get(0))
            .unwrap();
        let keys: u64 = conn
            .query_row("SELECT COUNT(*) FROM ingest_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!((observations, keys), (0, 0));

        assert!(matches!(
            admit_hook_session_event(
                &mut conn,
                &session,
                &bob_event,
                &OwnerFilter::User(alice.into()),
                Some("fresh-key"),
            )
            .unwrap(),
            HookSessionAdmission::Observation {
                ingest: IngestObservationOutcome::Inserted(_),
                ..
            }
        ));
    }

    // A mid-session `cd` into another project resolves the event to a
    // different (workspace, project) than the session row. Under the default
    // `follow-cwd` routing that is legitimate — the observation belongs in the
    // project it names — so scope difference must NOT be treated as a UUID
    // collision. Guarding it as one silently dropped every cross-project
    // mid-session event (#394 / #396 composition).
    #[test]
    fn cross_project_mid_session_observation_is_admitted_not_dropped() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let session = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
        begin_session(&mut conn, &session).unwrap();

        // Same session and operator, event resolved into a sibling project.
        let visited = get_or_create_project(&mut conn, &ws, "visited", None).unwrap();
        let mut moved = session.clone();
        moved.project_id = visited;
        let observation = hook_observation(&moved);

        assert!(matches!(
            admit_hook_session_event(
                &mut conn,
                &moved,
                &observation,
                &OwnerFilter::User("user:alice".into()),
                None,
            )
            .unwrap(),
            HookSessionAdmission::Observation {
                ingest: IngestObservationOutcome::Inserted(_),
                ..
            }
        ));

        // It lands in the project it named, and the session row is untouched.
        let landed: Vec<u8> = conn
            .query_row("SELECT project_id FROM observations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(landed.as_slice(), visited.as_bytes());
        let session_project: Vec<u8> = conn
            .query_row(
                "SELECT project_id FROM sessions WHERE id = ?1",
                params![session.id.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(session_project.as_slice(), proj.as_bytes());
    }

    // Identity is still identity: a different operator or a different agent
    // reusing the UUID stays terminal, in the same scope-moved shape as above.
    #[test]
    fn cross_project_events_still_reject_foreign_owner_and_agent() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let session = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
        begin_session(&mut conn, &session).unwrap();
        let visited = get_or_create_project(&mut conn, &ws, "visited", None).unwrap();

        let mut moved = session.clone();
        moved.project_id = visited;
        let observation = hook_observation(&moved);
        assert!(matches!(
            admit_hook_session_event(
                &mut conn,
                &moved,
                &observation,
                &OwnerFilter::User("user:bob".into()),
                None,
            ),
            Err(StoreError::SessionCollision)
        ));

        let mut other_agent = moved.clone();
        other_agent.agent_kind = AgentKind::ClaudeCode;
        let observation = hook_observation(&other_agent);
        assert!(matches!(
            admit_hook_session_event(
                &mut conn,
                &other_agent,
                &observation,
                &OwnerFilter::User("user:alice".into()),
                None,
            ),
            Err(StoreError::SessionCollision)
        ));

        let observations: u64 = conn
            .query_row("SELECT COUNT(*) FROM observations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(observations, 0, "neither rejection may leave a row");
    }

    // A terminal event naming a different scope is not this session's end, so
    // it is dropped rather than ending someone else's session — preserving the
    // pre-guard `SessionEndDisposition::DropInvalid` arm.
    #[test]
    fn cross_project_session_end_is_dropped_without_ending_the_session() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let session = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
        begin_session(&mut conn, &session).unwrap();
        let visited = get_or_create_project(&mut conn, &ws, "visited", None).unwrap();

        let mut moved = session.clone();
        moved.project_id = visited;
        let end = session_end_observation(&moved);
        assert!(matches!(
            admit_hook_session_event(
                &mut conn,
                &moved,
                &end,
                &OwnerFilter::User("user:alice".into()),
                None,
            )
            .unwrap(),
            HookSessionAdmission::InvalidScopedEnd
        ));

        let ended: Option<i64> = conn
            .query_row(
                "SELECT ended_at FROM sessions WHERE id = ?1",
                params![session.id.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ended, None, "a foreign-scope end must not end the session");
        let observations: u64 = conn
            .query_row("SELECT COUNT(*) FROM observations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(observations, 0);
    }

    #[test]
    fn concurrent_alice_and_bob_events_only_admit_alice_observation() {
        let (tmp, mut conn, ws, proj) = fresh_db();
        let session = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
        begin_session(&mut conn, &session).unwrap();
        drop(conn);
        let path = tmp.path().join("test.sqlite");
        let gate = Arc::new(std::sync::Barrier::new(2));
        let run = |owner: &'static str, key: &'static str, gate: Arc<std::sync::Barrier>| {
            let path = path.clone();
            let session = session.clone();
            std::thread::spawn(move || {
                let mut conn = Connection::open(path).unwrap();
                conn.pragma_update(None, "busy_timeout", 5_000).unwrap();
                gate.wait();
                admit_hook_session_event(
                    &mut conn,
                    &session,
                    &hook_observation(&session),
                    &OwnerFilter::User(owner.into()),
                    Some(key),
                )
            })
        };
        let alice = run("user:alice", "alice-key", Arc::clone(&gate));
        let bob = run("user:bob", "bob-key", gate);
        assert!(matches!(
            alice.join().unwrap(),
            Ok(HookSessionAdmission::Observation { .. })
        ));
        assert!(matches!(
            bob.join().unwrap(),
            Err(StoreError::SessionCollision)
        ));

        let conn = Connection::open(path).unwrap();
        let observations: u64 = conn
            .query_row("SELECT COUNT(*) FROM observations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(observations, 1);
    }

    #[test]
    fn hook_admission_rejects_tuple_mismatches_before_mutation() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let other = get_or_create_project(&mut conn, &ws, "other", None).unwrap();
        let session = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
        let mut observation = hook_observation(&session);
        observation.project_id = other;

        assert!(matches!(
            admit_hook_session_event(
                &mut conn,
                &session,
                &observation,
                &OwnerFilter::User("user:alice".into()),
                Some("tuple-key"),
            ),
            Err(StoreError::InvalidState(_))
        ));
        let counts: (u64, u64, u64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM sessions), (SELECT COUNT(*) FROM observations), (SELECT COUNT(*) FROM ingest_keys)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(counts, (0, 0, 0));

        let alice_session = hook_session(session.id, ws, proj, Some("user:alice"));
        begin_session(&mut conn, &alice_session).unwrap();
        let mut agent_mismatch = alice_session.clone();
        agent_mismatch.agent_kind = AgentKind::ClaudeCode;
        let observation = hook_observation(&alice_session);
        assert!(matches!(
            admit_hook_session_event(
                &mut conn,
                &agent_mismatch,
                &observation,
                &OwnerFilter::User("user:alice".into()),
                Some("agent-key"),
            ),
            Err(StoreError::SessionCollision)
        ));
        let counts: (u64, u64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM observations), (SELECT COUNT(*) FROM ingest_keys)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(counts, (0, 0));
    }

    #[test]
    fn hook_admission_rejects_unadmitted_new_owner_and_accepts_shared_session() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let owned = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
        assert!(matches!(
            admit_hook_session_event(
                &mut conn,
                &owned,
                &hook_observation(&owned),
                &OwnerFilter::User("user:bob".into()),
                Some("denied-owner"),
            ),
            Err(StoreError::SessionCollision)
        ));
        let counts: (u64, u64, u64) = conn
            .query_row("SELECT (SELECT COUNT(*) FROM sessions), (SELECT COUNT(*) FROM observations), (SELECT COUNT(*) FROM ingest_keys)", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap();
        assert_eq!(counts, (0, 0, 0));

        let shared = hook_session(SessionId::new(), ws, proj, None);
        let admission = admit_hook_session_event(
            &mut conn,
            &shared,
            &hook_observation(&shared),
            &OwnerFilter::User("user:alice".into()),
            None,
        )
        .unwrap();
        assert!(admitted_session(admission).owner().is_none());
        let owner: Option<String> = conn
            .query_row(
                "SELECT actor_user FROM sessions WHERE id = ?1",
                params![shared.id.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(owner.is_none());

        assert!(matches!(
            admit_hook_session_event(
                &mut conn,
                &shared,
                &hook_observation(&shared),
                &OwnerFilter::User("user:bob".into()),
                Some("shared-bob"),
            )
            .unwrap(),
            HookSessionAdmission::Observation { .. }
        ));
        let owner: Option<String> = conn
            .query_row(
                "SELECT actor_user FROM sessions WHERE id = ?1",
                params![shared.id.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(owner.is_none());
    }

    #[test]
    fn open_session_end_admissions_preserve_all_ingest_outcomes() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        for (key, completed, expected_complete) in [
            ("open-inserted", None, false),
            ("open-pending", Some(false), false),
            ("open-complete", Some(true), true),
        ] {
            let session = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
            begin_session(&mut conn, &session).unwrap();
            if let Some(completed) = completed {
                seed_ingest_key(&conn, proj, key, completed);
            }
            let admission = admit_hook_session_event(
                &mut conn,
                &session,
                &session_end_observation(&session),
                &OwnerFilter::User("user:alice".into()),
                Some(key),
            )
            .unwrap();
            match admission {
                HookSessionAdmission::EndOpen { ingest, .. } if key == "open-inserted" => {
                    assert!(matches!(ingest, IngestObservationOutcome::Inserted(_)));
                }
                HookSessionAdmission::EndOpen { ingest, .. } if key == "open-pending" => {
                    assert_eq!(ingest, IngestObservationOutcome::ResumePending);
                }
                HookSessionAdmission::EndOpen { ingest, .. } if expected_complete => {
                    assert_eq!(ingest, IngestObservationOutcome::AlreadyComplete);
                }
                other => panic!("unexpected open SessionEnd admission: {other:?}"),
            }
        }
    }

    #[test]
    fn reend_session_end_admissions_preserve_all_ingest_outcomes() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        for (key, completed, expected_complete) in [
            ("reend-inserted", None, false),
            ("reend-pending", Some(false), false),
            ("reend-complete", Some(true), true),
        ] {
            let session = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
            begin_session(&mut conn, &session).unwrap();
            end_session(&mut conn, &session.id, None).unwrap();
            insert_observation(&mut conn, &hook_observation(&session)).unwrap();
            if let Some(completed) = completed {
                seed_ingest_key(&conn, proj, key, completed);
            }
            let admission = admit_hook_session_event(
                &mut conn,
                &session,
                &session_end_observation(&session),
                &OwnerFilter::User("user:alice".into()),
                Some(key),
            )
            .unwrap();
            match admission {
                HookSessionAdmission::ReEnd { ingest, .. } if key == "reend-inserted" => {
                    assert!(matches!(ingest, IngestObservationOutcome::Inserted(_)));
                }
                HookSessionAdmission::ReEnd { ingest, .. } if key == "reend-pending" => {
                    assert_eq!(ingest, IngestObservationOutcome::ResumePending);
                }
                HookSessionAdmission::ReEnd { ingest, .. } if expected_complete => {
                    assert_eq!(ingest, IngestObservationOutcome::AlreadyComplete);
                }
                other => panic!("unexpected re-end SessionEnd admission: {other:?}"),
            }
        }
    }

    #[test]
    fn missing_session_end_creates_no_session_key_or_observation() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let session = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
        assert!(matches!(
            admit_hook_session_event(
                &mut conn,
                &session,
                &session_end_observation(&session),
                &OwnerFilter::User("user:alice".into()),
                Some("missing-end"),
            )
            .unwrap(),
            HookSessionAdmission::InvalidMissingEnd
        ));
        let counts: (u64, u64, u64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM sessions), (SELECT COUNT(*) FROM observations), (SELECT COUNT(*) FROM ingest_keys)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(counts, (0, 0, 0));
    }

    #[test]
    fn ordinary_observation_already_complete_remains_observation_admission() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let session = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
        begin_session(&mut conn, &session).unwrap();
        seed_ingest_key(&conn, proj, "ordinary-complete", true);
        assert!(matches!(
            admit_hook_session_event(
                &mut conn,
                &session,
                &hook_observation(&session),
                &OwnerFilter::User("user:alice".into()),
                Some("ordinary-complete"),
            )
            .unwrap(),
            HookSessionAdmission::Observation {
                ingest: IngestObservationOutcome::AlreadyComplete,
                ..
            }
        ));
    }

    #[test]
    fn admitted_guard_revalidates_and_lifecycle_preserves_handoff_release_audit() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let session = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
        let guard = admitted_session(
            admit_hook_session_event(
                &mut conn,
                &session,
                &hook_observation(&session),
                &OwnerFilter::User("user:alice".into()),
                None,
            )
            .unwrap(),
        );
        conn.execute(
            "DELETE FROM sessions WHERE id = ?1",
            params![session.id.as_bytes()],
        )
        .unwrap();
        assert!(matches!(
            end_admitted_session(&mut conn, &guard, None),
            Err(StoreError::SessionCollision)
        ));
        begin_session(&mut conn, &session).unwrap();
        conn.execute(
            "UPDATE sessions SET actor_user = 'user:bob' WHERE id = ?1",
            params![session.id.as_bytes()],
        )
        .unwrap();
        assert!(matches!(
            end_admitted_lifecycle_only_session(&mut conn, &guard),
            Err(StoreError::SessionCollision)
        ));
        let handoff = NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: Some(session.id),
            from_agent: AgentKind::Codex,
            to_agent: None,
            cwd: None,
            summary: "x".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: Some("user:alice".into()),
        };
        assert!(matches!(
            end_admitted_session_with_handoff(&mut conn, &guard, None, &handoff),
            Err(StoreError::SessionCollision)
        ));

        let receiver = hook_session(SessionId::new(), ws, proj, None);
        let mut lifecycle_observation = hook_observation(&receiver);
        lifecycle_observation.kind = ObservationKind::SessionStart;
        let receiver_guard = admitted_session(
            admit_hook_session_event(
                &mut conn,
                &receiver,
                &lifecycle_observation,
                &OwnerFilter::User("user:alice".into()),
                None,
            )
            .unwrap(),
        );
        let handoff = NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "x".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        };
        let handoff_id = insert_handoff(&mut conn, &handoff).unwrap();
        let mut acceptance = handoff_acceptance(handoff_id, ws, proj);
        acceptance.accepting_session = Some(receiver.id);
        assert!(accept_handoff(&mut conn, &acceptance).unwrap());
        assert_eq!(
            end_admitted_lifecycle_only_session(&mut conn, &receiver_guard).unwrap(),
            LifecycleOnlyEndOutcome::Ended {
                reopened_handoff: Some(handoff_id)
            }
        );
        assert_eq!(audit_row_for(&conn, "release_lifecycle_only_handoff").0, 1);
    }
}
