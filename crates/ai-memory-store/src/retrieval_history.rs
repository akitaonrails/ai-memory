//! Opt-in per-session retrieval history (#2038, Workstream 2).
//!
//! When `[retrieval] record_history` is on, each single-project `memory_query`
//! call records what it returned — the scrubbed query, resolved scope, active
//! streams, effective filter settings, duration, result count, any soft notice,
//! and every returned page's version reference, position, calibrated relevance
//! score and ranking explanation. A read-only `/web` dashboard renders this so
//! an operator can inspect retrieval accuracy and debug noisy results.
//!
//! Recording is best-effort and additive: a query never fails because its
//! history could not be written, and an install that never opts in writes
//! nothing. Rows are keyed by the `(workspace_id, project_id, session_id)`
//! 3-tuple like every other session-scoped row (invariant #4), so the
//! dashboard's reads reuse the project's authorization and the session-owner
//! filter. Evidence is stored, never recomputed: a later rerun cannot reproduce
//! a past query's ranking, so the original scores and explanations are kept, and
//! only a version *reference* (path + version id) per hit, never the page body.

use ai_memory_core::{ProjectId, SessionId, WorkspaceId};
use jiff::Timestamp;
use rusqlite::{Connection, params};
use uuid::Uuid;

use crate::error::StoreResult;

/// Upper bound on the scrubbed query text stored per retrieval record. The
/// caller scrubs and truncates to this before recording; the dashboard escapes
/// it on render.
pub const RETRIEVAL_HISTORY_QUERY_MAX_CHARS: usize = 4096;

/// Built-in retention cap: the newest-N retrieval records kept per
/// `(workspace, project, session)` when recording is on and the config names no
/// explicit `[retrieval] history_retention_per_session`.
pub const DEFAULT_RETRIEVAL_HISTORY_RETENTION: u32 = 500;

/// One returned hit recorded alongside a retrieval call, in returned order.
#[derive(Debug, Clone)]
pub struct NewRetrievalHistoryHit {
    /// Project-relative page path (a version reference, not the body).
    pub page_path: String,
    /// The page version id the hit resolved to, when known.
    pub page_version: Option<String>,
    /// Calibrated reranker relevance in `0.0..=1.0`; `None` when the hit was not
    /// scored (no reranker, the fused tail, or a pinned page).
    pub relevance: Option<f32>,
    /// Compact JSON ranking explanation (`score_details`) when the call ran with
    /// `explain`; `None` otherwise.
    pub explanation: Option<String>,
}

/// One single-project `memory_query` call to record. The caller has already
/// scrubbed and length-bounded `query`; [`record_retrieval_history`] only
/// persists what it is given.
#[derive(Debug, Clone)]
pub struct NewRetrievalHistory {
    /// Caller's workspace (the project the session queried).
    pub workspace_id: WorkspaceId,
    /// Caller's project.
    pub project_id: ProjectId,
    /// Caller's session, the `SessionId` the hook ingress derives for the
    /// harness-native id (bounded, and what `purge_session` matches on).
    pub session_id: SessionId,
    /// Scrubbed, length-bounded query expression.
    pub query: String,
    /// Resolved scope shape. v1 records the single-project path only.
    pub scope_kind: String,
    /// Comma-separated active retrieval streams (e.g. `fts,entity,vector`).
    pub source: String,
    /// Effective result limit after #2038 precedence resolution.
    pub result_limit: Option<u32>,
    /// Effective calibrated relevance floor after #2038 precedence.
    pub min_relevance: Option<f32>,
    /// Compact JSON of the active pre-ranking filters; `None` when none.
    pub filters: Option<String>,
    /// Wall time the retrieval took, in microseconds.
    pub duration_us: i64,
    /// Number of memory hits returned to the caller (0 is valid).
    pub result_count: u32,
    /// A soft notice attached to the call (not a hard failure); `None` when
    /// clean.
    pub notice: Option<String>,
    /// Returned hits in order.
    pub hits: Vec<NewRetrievalHistoryHit>,
}

/// One recorded hit, read back for the dashboard.
#[derive(Debug, Clone)]
pub struct RetrievalHistoryHitRow {
    /// 0-based rank in the returned order.
    pub position: u32,
    /// Project-relative page path.
    pub page_path: String,
    /// The page version id, when it was known at record time.
    pub page_version: Option<String>,
    /// Calibrated reranker relevance, when the hit was scored.
    pub relevance: Option<f32>,
    /// Ranking explanation JSON, when the call ran with `explain`.
    pub explanation: Option<String>,
}

/// One recorded retrieval call with its hits, read back for the dashboard.
#[derive(Debug, Clone)]
pub struct RetrievalHistoryRow {
    /// Record id (UUIDv7, hyphenated) — stable and URL-safe.
    pub id: String,
    /// When the call was recorded.
    pub created_at: Timestamp,
    /// Scrubbed query expression.
    pub query: String,
    /// Resolved scope shape.
    pub scope_kind: String,
    /// Active retrieval streams.
    pub source: String,
    /// Effective result limit.
    pub result_limit: Option<u32>,
    /// Effective relevance floor.
    pub min_relevance: Option<f32>,
    /// Active pre-ranking filters JSON.
    pub filters: Option<String>,
    /// Wall time in microseconds.
    pub duration_us: i64,
    /// Hits returned.
    pub result_count: u32,
    /// Soft notice, when any.
    pub notice: Option<String>,
    /// Returned hits in order.
    pub hits: Vec<RetrievalHistoryHitRow>,
}

/// One page of a session's retrieval history, newest first.
#[derive(Debug, Clone)]
pub struct RetrievalHistoryPage {
    /// The records on this page.
    pub records: Vec<RetrievalHistoryRow>,
    /// Total records for the session (for the scrubber).
    pub total: i64,
}

/// Persist one retrieval call and its hits, then prune the session's history to
/// the newest `retention_cap` records (pruned hit rows cascade). All in one
/// transaction so a reader never sees a record without its hits.
///
/// A `retention_cap` of 0 is floored to 1 — recording is opt-in, and a zero cap
/// that deleted the row it just wrote would make the feature silently inert; the
/// config validation already floors the cap, this is defence in depth.
pub(crate) fn record_retrieval_history(
    conn: &mut Connection,
    record: &NewRetrievalHistory,
    retention_cap: u32,
) -> StoreResult<()> {
    let id = Uuid::now_v7();
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO retrieval_history \
          (id, workspace_id, project_id, session_id, created_at, query, scope_kind, source, \
           result_limit, min_relevance, filters, duration_us, result_count, notice) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            id.as_bytes(),
            record.workspace_id.as_bytes(),
            record.project_id.as_bytes(),
            record.session_id.as_bytes(),
            now,
            record.query,
            record.scope_kind,
            record.source,
            record.result_limit.map(i64::from),
            record.min_relevance.map(f64::from),
            record.filters,
            record.duration_us,
            i64::from(record.result_count),
            record.notice,
        ],
    )?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO retrieval_history_hits \
              (history_id, position, page_path, page_version, relevance, explanation) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for (position, hit) in record.hits.iter().enumerate() {
            stmt.execute(params![
                id.as_bytes(),
                i64::try_from(position).unwrap_or(i64::MAX),
                hit.page_path,
                hit.page_version,
                hit.relevance.map(f64::from),
                hit.explanation,
            ])?;
        }
    }
    let cap = i64::from(retention_cap.max(1));
    tx.execute(
        "DELETE FROM retrieval_history \
          WHERE workspace_id = ?1 AND project_id = ?2 AND session_id = ?3 \
            AND id NOT IN ( \
              SELECT id FROM retrieval_history \
               WHERE workspace_id = ?1 AND project_id = ?2 AND session_id = ?3 \
               ORDER BY created_at DESC, id DESC \
               LIMIT ?4)",
        params![
            record.workspace_id.as_bytes(),
            record.project_id.as_bytes(),
            record.session_id.as_bytes(),
            cap,
        ],
    )?;
    tx.commit()?;
    Ok(())
}

/// One page of a session's retrieval history, newest first, strictly scoped to
/// `(workspace_id, project_id, session_id)` so a foreign workspace/project can
/// never read another scope's rows through a guessed session id. The owner
/// filter is enforced upstream by the session-visibility gate the dashboard
/// shares with the session timeline, exactly as the observations reader relies
/// on it.
pub(crate) fn retrieval_history_page(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    session_id: SessionId,
    offset: i64,
    limit: i64,
) -> StoreResult<RetrievalHistoryPage> {
    let ws = workspace_id.as_bytes();
    let proj = project_id.as_bytes();
    let sid = session_id.as_bytes();
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM retrieval_history \
          WHERE workspace_id = ?1 AND project_id = ?2 AND session_id = ?3",
        params![ws, proj, sid],
        |row| row.get(0),
    )?;
    let mut stmt = conn.prepare(
        "SELECT id, created_at, query, scope_kind, source, result_limit, min_relevance, \
                filters, duration_us, result_count, notice \
           FROM retrieval_history \
          WHERE workspace_id = ?1 AND project_id = ?2 AND session_id = ?3 \
          ORDER BY created_at DESC, id DESC \
          LIMIT ?4 OFFSET ?5",
    )?;
    let mut records: Vec<RetrievalHistoryRow> = stmt
        .query_map(params![ws, proj, sid, limit, offset], |row| {
            let id: Vec<u8> = row.get(0)?;
            Ok(RetrievalHistoryRow {
                id: uuid_string(&id),
                created_at: micros_to_ts(row.get(1)?),
                query: row.get(2)?,
                scope_kind: row.get(3)?,
                source: row.get(4)?,
                result_limit: row
                    .get::<_, Option<i64>>(5)?
                    .and_then(|v| u32::try_from(v).ok()),
                min_relevance: row.get::<_, Option<f64>>(6)?.map(|v| v as f32),
                filters: row.get(7)?,
                duration_us: row.get(8)?,
                result_count: u32::try_from(row.get::<_, i64>(9)?).unwrap_or(0),
                notice: row.get(10)?,
                hits: Vec::new(),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // Attach each record's hits. The page size is bounded, so a cached
    // per-record query stays cheap and avoids a large `IN (...)` of blob ids.
    let mut hits_stmt = conn.prepare(
        "SELECT position, page_path, page_version, relevance, explanation \
           FROM retrieval_history_hits \
          WHERE history_id = ?1 \
          ORDER BY position ASC",
    )?;
    for record in &mut records {
        let Ok(id) = Uuid::parse_str(&record.id) else {
            continue;
        };
        record.hits = hits_stmt
            .query_map(params![id.as_bytes()], |row| {
                Ok(RetrievalHistoryHitRow {
                    position: u32::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
                    page_path: row.get(1)?,
                    page_version: row.get(2)?,
                    relevance: row.get::<_, Option<f64>>(3)?.map(|v| v as f32),
                    explanation: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
    }
    Ok(RetrievalHistoryPage { records, total })
}

/// Hyphenated UUID for a 16-byte id blob, falling back to lowercase hex for a
/// malformed one (never reached through the write path, which stores a UUIDv7).
fn uuid_string(bytes: &[u8]) -> String {
    match <[u8; 16]>::try_from(bytes) {
        Ok(array) => Uuid::from_bytes(array).to_string(),
        Err(_) => bytes.iter().map(|b| format!("{b:02x}")).collect(),
    }
}

/// Microseconds since the epoch to a timestamp, clamped on an out-of-range row.
fn micros_to_ts(us: i64) -> Timestamp {
    Timestamp::from_microsecond(us).unwrap_or(Timestamp::UNIX_EPOCH)
}
