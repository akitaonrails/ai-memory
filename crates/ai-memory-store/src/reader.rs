//! Read-only connection pool and query helpers.
//!
//! WAL mode lets us have unlimited concurrent readers alongside the single
//! writer, so the pool is mostly about bounding file-descriptor usage and
//! avoiding `Connection::open` overhead on hot paths. Pool eviction is a
//! soft cap: a connection that comes back when the pool is already full
//! is simply dropped.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ai_memory_core::{
    AgentKind, AutoImproveProposalId, AutoImproveRunId, Handoff, HandoffId, HandoffState,
    ManagedRunId, Observation, ObservationId, ObservationKind, OwnerFilter, PageId, PagePath,
    ProjectId, SessionId, User, UserId, WorkspaceId, WorkstreamEvent, WorkstreamId,
};
use jiff::Timestamp;
use parking_lot::Mutex;
use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, params_from_iter};
use serde::Serialize;
use uuid::Uuid;
// `ai_memory_core::Tier` is referenced via fully-qualified path inside the
// DecayCandidate struct definition above to avoid a top-level import
// for a single use-site.

use crate::auto_improve::{
    AutoImproveProposalDetail, AutoImproveProposalEvent, AutoImproveProposalStatus,
    AutoImproveProposalSummary, AutoImproveRejectionSummary, AutoImproveTelemetryAggregate,
    AutoImproveTelemetryCount, bytes32, opt_bytes32, summary_from_row, to_sql_err,
};
use crate::error::{StoreError, StoreResult};
use crate::fts_query::prepare_fts5_query;
use crate::maintenance::MaintenanceJob;
use crate::users::TOKEN_HASH_LEN;
use crate::workstream::{ManagedRunContext, StoredManagedRunStatus};

/// TTL guard for retrieval surfaces (search / recent / embedding hits /
/// graph neighbours / briefing lists): appended to a WHERE clause that
/// already constrains `is_latest = 1`. `table` is the pages alias in
/// that query; `now_param` is the positional placeholder bound to the
/// current wall-clock in microseconds. One definition so the
/// NULL-handling never drifts between queries. Exact-path/id reads and
/// the decay-candidate walk deliberately do NOT use this — reads
/// annotate expiry instead of hiding it, and the sweep must see
/// expired rows to delete them.
fn not_expired(table: &str, now_param: &str) -> String {
    format!(" AND ({table}.expires_at IS NULL OR {table}.expires_at > {now_param})")
}

/// Current wall-clock in microseconds, for binding against
/// [`not_expired`] fragments.
fn now_us() -> i64 {
    Timestamp::now().as_microsecond()
}

fn page_kind_expr(path_column: &str, frontmatter_column: &str) -> String {
    format!(
        "COALESCE( \
            json_extract({frontmatter_column}, '$.kind'), \
            CASE \
                WHEN {path_column} LIKE '\\_rules/%' ESCAPE '\\' THEN 'rule' \
                WHEN {path_column} LIKE '\\_slots/%' ESCAPE '\\' THEN 'slot' \
                WHEN {path_column} LIKE 'sessions/%' THEN 'session' \
                WHEN {path_column} LIKE 'decisions/%' THEN 'decision' \
                WHEN {path_column} LIKE 'gotchas/%' THEN 'gotcha' \
                WHEN {path_column} LIKE 'concepts/%' THEN 'concept' \
                WHEN {path_column} LIKE 'procedures/%' THEN 'procedure' \
                WHEN {path_column} LIKE 'notes/%' THEN 'note' \
                ELSE 'fact' \
            END \
        )"
    )
}

const AUTHORITY_CANDIDATE_MULTIPLIER: usize = 4;
const AUTHORITY_MIN_CANDIDATES: usize = 20;
const AUTHORITY_MAX_EXTRA_CANDIDATES: usize = 300;

#[derive(Debug, Clone, Copy)]
struct PageAuthority {
    factor: f64,
}

impl PageAuthority {
    fn from_stored(
        path: &str,
        kind: &str,
        tier: &str,
        pinned: bool,
        frontmatter_json: &str,
    ) -> Self {
        // Relevance remains primary: even the strongest maintained page gets
        // at most a 1.5x boost, while explicit low-authority metadata bottoms
        // out at 0.55x rather than excluding the page.
        let mut factor = 1.0_f64;

        factor += match kind {
            "rule" | "decision" => 0.15,
            "procedure" | "gotcha" => 0.12,
            "concept" | "slot" => 0.07,
            "session" => -0.15,
            _ => 0.0,
        };
        factor += match tier {
            "semantic" | "procedural" => 0.10,
            "episodic" => -0.08,
            "working" => -0.03,
            _ => 0.0,
        };
        if pinned {
            factor += 0.08;
        }

        if path.starts_with("_lint/") {
            factor -= 0.20;
        } else if path.starts_with("investigations/") {
            factor -= 0.05;
        }

        let tags = authority_tags(frontmatter_json);
        if tags
            .iter()
            .any(|tag| tag == "canonical" || tag == "source-of-truth")
        {
            factor += 0.20;
        }
        if tags.iter().any(|tag| tag == "active") {
            factor += 0.05;
        }

        // Negative tags are explicit curator instructions. They cap the
        // result's authority instead of merely cancelling a canonical path
        // boost, but they never remove the page from search.
        if tags
            .iter()
            .any(|tag| tag == "do-not-answer-from" || tag == "test-fixture")
        {
            factor = factor.min(0.55);
        } else if tags.iter().any(|tag| tag == "superseded") {
            factor = factor.min(0.65);
        } else if tags.iter().any(|tag| tag == "historical") {
            factor = factor.min(0.80);
        }

        Self {
            factor: factor.clamp(0.55, 1.50),
        }
    }

    fn adjust_rank(&self, rank: f64) -> f64 {
        if rank <= 0.0 {
            rank * self.factor
        } else {
            rank / self.factor
        }
    }
}

fn authority_tags(frontmatter_json: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(frontmatter_json)
        .ok()
        .and_then(|frontmatter| frontmatter.get("tags").cloned())
        .and_then(|tags| tags.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|tag| tag.as_str().map(normalize_authority_tag))
        .collect()
}

fn normalize_authority_tag(tag: &str) -> String {
    tag.trim().to_ascii_lowercase().replace('_', "-")
}

fn authority_candidate_limit(limit: usize) -> usize {
    if limit == 0 {
        return 0;
    }
    limit
        .saturating_mul(AUTHORITY_CANDIDATE_MULTIPLIER)
        .max(AUTHORITY_MIN_CANDIDATES)
        .min(limit.saturating_add(AUTHORITY_MAX_EXTRA_CANDIDATES))
}

fn rerank_page_hits(candidates: Vec<(PageHit, PageAuthority)>, limit: usize) -> Vec<PageHit> {
    let mut hits: Vec<PageHit> = candidates
        .into_iter()
        .map(|(mut hit, authority)| {
            hit.rank = authority.adjust_rank(hit.rank);
            hit
        })
        .collect();
    hits.sort_by(|a, b| {
        a.rank
            .partial_cmp(&b.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(limit);
    hits
}

fn rerank_page_hits_with_meta(
    candidates: Vec<(PageHitWithMeta, PageAuthority)>,
    limit: usize,
) -> Vec<PageHitWithMeta> {
    let mut hits: Vec<PageHitWithMeta> = candidates
        .into_iter()
        .map(|(mut hit, authority)| {
            hit.rank = authority.adjust_rank(hit.rank);
            hit
        })
        .collect();
    hits.sort_by(|a, b| {
        a.rank
            .partial_cmp(&b.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(limit);
    hits
}

/// One page flagged by `stale`/`wrong` feedback on its current version,
/// aggregated per (page, kind) for the lint pass.
#[derive(Debug, Clone, Serialize)]
pub struct FeedbackFinding {
    /// Wiki path of the flagged page.
    pub path: String,
    /// `stale` or `wrong`.
    pub kind: String,
    /// How many signals of this kind the current version has.
    pub signal_count: u32,
    /// ISO-8601 timestamp of the most recent signal.
    pub latest_at: String,
    /// Most recent caller-supplied reason for this kind, if any.
    pub reason: Option<String>,
}

/// A page matched by the entity stream, with the inverse-frequency
/// weight that ranked it and the entity names that matched.
#[derive(Debug, Clone)]
pub(crate) struct EntityHit {
    /// The matched page.
    pub(crate) hit: PageHit,
    /// Sum of `1 / pages_carrying_entity` over the matched entities.
    pub(crate) weight: f64,
    /// Entity names that matched the query.
    pub(crate) matched: Vec<String>,
}

/// Escape a literal for use inside a SQL `LIKE` pattern with
/// `ESCAPE '\'`. Entity tokens legitimately contain `_`, which `LIKE`
/// would otherwise treat as "any single character" — so `foo_bar` would
/// match `fooxbar`.
fn like_escape(literal: &str) -> String {
    literal
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Split a query into entity-match tokens: lowercase, alphanumeric-ish
/// runs, 3+ characters, de-duplicated, bounded. Short tokens are
/// dropped because a 1-2 character prefix match is noise, not a signal.
///
/// A hyphenated/underscored run yields BOTH the whole token and its
/// parts — the same both-forms trick [`crate::ops::path_search_text`]
/// uses on paths, and for the same reason: an entity may be written
/// `ai-memory`, `ai_memory`, or `ai memory`, and the query may pick
/// any of them.
fn entity_query_tokens(query: &str) -> Vec<String> {
    /// Beyond this the query is prose, not a set of nouns; the FTS
    /// stream already handles prose better than prefix matching does.
    const MAX_TOKENS: usize = 12;
    const MIN_TOKEN_CHARS: usize = 3;
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    let mut push = |raw: &str| -> bool {
        let char_count = raw.chars().count();
        if !(MIN_TOKEN_CHARS..=ai_memory_core::MAX_ENTITY_LEN).contains(&char_count) {
            return true;
        }
        let token = raw.to_lowercase();
        if seen.insert(token.clone()) {
            out.push(token);
        }
        out.len() < MAX_TOKENS
    };
    'outer: for raw in query.split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_')) {
        let token = raw.trim_matches(['-', '_']);
        if token.is_empty() {
            continue;
        }
        let compound = token.contains(['-', '_']);
        if !push(token) {
            break;
        }
        if compound {
            for part in token.split(['-', '_']) {
                if !push(part) {
                    break 'outer;
                }
            }
        }
    }
    out
}

/// A graph-expansion neighbour with provenance (which seed and which
/// link direction produced it). Internal to hybrid search + explain.
struct GraphNeighbor {
    hit: PageHit,
    /// Index into the seed list handed to the expansion.
    seed_ord: usize,
    /// `true` when the neighbour links TO the seed (backlink).
    incoming: bool,
}

/// Which seed page pulled a hit in via graph expansion, and the link
/// direction. Part of [`SearchExplain`].
#[derive(Debug, Clone, Serialize)]
pub struct GraphVia {
    /// Path of the FTS/entity/vector seed page the link was followed from.
    pub seed_path: String,
    /// `outgoing` = seed links to the hit; `incoming` = hit links to
    /// the seed (backlink).
    pub direction: &'static str,
}

/// Per-stream RRF contributions (`1/(k+rank)`, k=60) for one hit.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RrfContributions {
    /// FTS5 stream contribution (0.0 when the hit missed that stream).
    pub fts: f64,
    /// Vector stream contribution.
    pub vector: f64,
    /// Graph-neighbour stream contribution.
    pub graph: f64,
    /// Entity-match stream contribution.
    pub entity: f64,
}

/// Score transparency for one `memory_query` hit: where the hit ranked
/// in each retrieval stream, the raw per-stream scores, and how the
/// RRF fusion combined them. All ranks are 1-based; a `None` rank
/// means the hit did not surface in that stream.
///
/// [`Default`] is "surfaced in no stream": every rank `None`, zero
/// contributions. Streams fill in their own fields as they contribute.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SearchExplain {
    /// 1-based rank in the FTS5 stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fts_rank: Option<usize>,
    /// Raw FTS5 (bm25-based) score — lower is better.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fts_score: Option<f64>,
    /// 1-based rank in the vector stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_rank: Option<usize>,
    /// Cosine similarity against the query embedding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cosine: Option<f32>,
    /// 1-based rank in the graph-neighbour stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_rank: Option<usize>,
    /// Which seed page and link direction pulled the hit in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_via: Option<GraphVia>,
    /// 1-based rank in the entity-match stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_rank: Option<usize>,
    /// Raw inverse-frequency weight used to order the entity stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_weight: Option<f64>,
    /// Entity names on this page that matched the query.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matched_entities: Vec<String>,
    /// Per-stream RRF contributions.
    pub rrf: RrfContributions,
    /// Total fused score (sum of the RRF contributions) — higher is
    /// better. The returned `rank` is its negation *after* the
    /// authority multiplier below, so `fused` alone does not reproduce
    /// the ordering.
    pub fused: f64,
    /// Bounded page-authority multiplier applied to the fused rank
    /// after RRF (canonical kind/tier/`pinned`/frontmatter tags).
    /// `None` when the page had no authority row to adjust with; `1.0`
    /// means it was considered and left alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authority: Option<f64>,
    /// Relevance in `[0, 1]` assigned by the optional post-RRF
    /// reranker. `None` when no reranker is configured, when it
    /// degraded, or when the hit fell outside the bounded judged prefix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerank_score: Option<f32>,
}

/// One hit returned by [`ReaderPool::search_pages`].
#[derive(Debug, Clone, Serialize)]
pub struct PageHit {
    /// Stable identifier for this page version.
    pub id: PageId,
    /// Relative path within the wiki tree.
    pub path: PagePath,
    /// Page title.
    pub title: String,
    /// FTS5 snippet of the body around the matched terms (HTML-marked).
    pub snippet: String,
    /// Relevance rank after the bounded authority adjustment (lower is better).
    pub rank: f64,
}

/// Completed session selected for scheduled auto-improvement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoImproveCandidateSession {
    /// Session id.
    pub session_id: SessionId,
    /// Session `ended_at` timestamp in Unix microseconds.
    pub ended_at: i64,
}

/// Open session selected for manual finalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSession {
    /// Session id.
    pub session_id: SessionId,
    /// Captured session cwd, if available.
    pub cwd: Option<String>,
}

/// How a `SessionEnd` event should treat its target session — see
/// [`ReaderPool::session_end_disposition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEndDisposition {
    /// The session is open in the resolved scope/agent: run the normal end
    /// path.
    Open,
    /// The session is missing or does not match the resolved scope/agent. Drop
    /// the event without attempting recovery against an unrelated session.
    DropInvalid,
    /// The matching session already ended with no observations beyond the
    /// persisted end generation. Converge any interrupted downstream effects,
    /// then drop the duplicate end.
    AlreadyEnded,
    /// Already ended, but the observation count exceeds the persisted end
    /// generation: the agent resumed the session under the same id — run the
    /// full end path again so the resumed work reaches the compiled page
    /// (issue #152).
    ReEndWithNewWork,
}

/// The latest version of a page's stored content, used as a DB-backed
/// fallback when the on-disk markdown read fails (index/disk skew). The
/// markdown file is the source of truth, but the store keeps a faithful copy
/// written in the same transaction, so serving it is safe.
#[derive(Debug, Clone)]
pub struct StoredPageBody {
    /// Page title as stored in the DB.
    pub title: String,
    /// Page body (markdown without frontmatter).
    pub body: String,
    /// Raw `frontmatter_json` TEXT column (parse at the call site).
    pub frontmatter_json: String,
    /// Memory tier stored on the latest page row.
    pub tier: String,
    /// Whether the latest page row is pinned.
    pub pinned: bool,
}

/// Search hit with workspace/project names, used by the web UI to avoid
/// per-hit metadata lookups after a global search.
#[derive(Debug, Clone, Serialize)]
pub struct PageHitWithMeta {
    /// Name of the workspace containing the page.
    pub workspace_name: String,
    /// Name of the project containing the page.
    pub project_name: String,
    /// Relative path within the wiki tree.
    pub path: PagePath,
    /// Page title.
    pub title: String,
    /// FTS5 snippet of the body around the matched terms (HTML-marked).
    pub snippet: String,
    /// Relevance rank after the bounded authority adjustment (lower is better).
    pub rank: f64,
}

/// One raw observation fallback hit returned when compiled wiki pages miss.
#[derive(Debug, Clone, Serialize)]
pub struct ObservationHit {
    /// Stable observation identifier.
    pub id: ObservationId,
    /// Owning session identifier.
    pub session_id: SessionId,
    /// Observation kind as stored on the lifecycle row.
    pub kind: String,
    /// Observation title.
    pub title: String,
    /// FTS5 snippet of the raw observation body around the matched terms.
    pub snippet: String,
    /// FTS5 rank score (lower is better).
    pub rank: f64,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
}

/// Aggregate counts surfaced by [`ReaderPool::status_counts`] and consumed
/// by `ai-memory status`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StatusCounts {
    /// Pages with `is_latest = 1`.
    pub pages_latest: u64,
    /// All page versions including superseded ones.
    pub pages_all: u64,
    /// Total sessions ever recorded.
    pub sessions: u64,
    /// Total observations across all sessions.
    pub observations: u64,
}

/// One likely cross-project contamination finding from
/// [`ReaderPool::audit_contamination`]. Advisory only — it flags STRUCTURAL
/// mislandings (an entity whose identity disagrees with the bucket it landed
/// in). Purely semantic contamination (a page whose topic belongs elsewhere
/// with no cwd/session anomaly) is not detectable structurally.
#[derive(Debug, Clone, Serialize)]
pub struct ContaminationFinding {
    /// Heuristic that fired: `session_wrong_bucket` (CHECK A).
    pub check: &'static str,
    /// Confidence — `high` for the structural check.
    pub confidence: &'static str,
    /// Entity kind: `session`.
    pub entity_kind: &'static str,
    /// Entity id (lowercase hex of the 16-byte UUID).
    pub entity_id: String,
    /// Workspace name the entity actually landed in.
    pub landed_workspace: String,
    /// Project name the entity actually landed in.
    pub landed_project: String,
    /// Project the evidence says it belongs to (the session's cwd
    /// prefix-resolution result for CHECK A).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_project: Option<String>,
    /// Originating session cwd — the prefix-resolution evidence (CHECK A).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// Per-check counts for an [`ReaderPool::audit_contamination`] run.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ContaminationSummary {
    /// Sessions whose cwd prefix-resolves to a different project (CHECK A).
    pub sessions_misbucketed: usize,
}

/// Result of [`ReaderPool::audit_contamination`] — advisory, never mutates.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ContaminationReport {
    /// Per-check counts.
    pub summary: ContaminationSummary,
    /// Individual findings.
    pub findings: Vec<ContaminationFinding>,
}

/// Counts that must all be zero before `ai-memory reindex` rebuilds the
/// derived SQLite store from wiki files.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReindexTargetStatus {
    /// Workspace rows already present in SQLite.
    pub workspaces: u64,
    /// Project rows already present in SQLite.
    pub projects: u64,
    /// Page rows, including superseded versions.
    pub pages: u64,
    /// Link rows derived from latest page bodies.
    pub links: u64,
    /// Stored embedding rows derived from latest pages.
    pub page_embeddings: u64,
    /// Session rows. These are DB-only episodic state and are not rebuilt.
    pub sessions: u64,
    /// Observation rows. These are DB-only episodic state and are not rebuilt.
    pub observations: u64,
    /// Handoff rows. These are DB-only episodic state and are not rebuilt.
    pub handoffs: u64,
    /// User rows and token hashes. These are DB-only state and are not rebuilt.
    pub users: u64,
    /// Audit rows. These are DB-only state and are not rebuilt.
    pub audit_log: u64,
}

impl ReindexTargetStatus {
    /// True when the store has no user data or derived rows.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.workspaces == 0
            && self.projects == 0
            && self.pages == 0
            && self.links == 0
            && self.page_embeddings == 0
            && self.sessions == 0
            && self.observations == 0
            && self.handoffs == 0
            && self.users == 0
            && self.audit_log == 0
    }

    /// Render a compact list of non-zero counters for operator errors.
    #[must_use]
    pub fn nonzero_summary(&self) -> String {
        let mut parts = Vec::new();
        macro_rules! push_nonzero {
            ($field:ident) => {
                if self.$field != 0 {
                    parts.push(format!("{}={}", stringify!($field), self.$field));
                }
            };
        }
        push_nonzero!(workspaces);
        push_nonzero!(projects);
        push_nonzero!(pages);
        push_nonzero!(links);
        push_nonzero!(page_embeddings);
        push_nonzero!(sessions);
        push_nonzero!(observations);
        push_nonzero!(handoffs);
        push_nonzero!(users);
        push_nonzero!(audit_log);
        parts.join(", ")
    }
}

/// Derived-index health counters surfaced by admin status.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DerivedIndexStatus {
    /// All page rows in the source table. `pages_fts_rows` should match this.
    pub pages_rows: u64,
    /// Rows currently present in the page FTS5 index.
    pub pages_fts_rows: u64,
    /// All observation rows. `observations_fts_rows` should match this.
    pub observations_rows: u64,
    /// Rows currently present in the observation FTS5 index.
    pub observations_fts_rows: u64,
    /// Latest pages without any embedding row.
    pub latest_pages_missing_embeddings: u64,
    /// Stored embedding rows, regardless of provider/model/dim.
    pub embedding_rows: u64,
    /// Stored embedding triples and row counts.
    pub embedding_triples: Vec<EmbeddingTripleCount>,
    /// Outgoing links whose source page is latest.
    pub links_from_latest_pages: u64,
    /// Latest-page outgoing links whose target path has not resolved yet.
    pub unresolved_links_from_latest_pages: u64,
    /// Latest-page outgoing links pointing at a non-latest target row.
    pub stale_links_from_latest_pages: u64,
}

/// Count of embedding rows sharing one `(provider, model, dim)` triple.
#[derive(Debug, Clone, Serialize)]
pub struct EmbeddingTripleCount {
    /// Embedding provider name.
    pub provider: String,
    /// Embedding model name.
    pub model: String,
    /// Vector dimension.
    pub dim: u32,
    /// Rows using this triple.
    pub count: u64,
}

/// Rolling activity counters over a fixed time window. Surfaced by
/// [`ReaderPool::briefing`] so the caller (or an LLM-driven `memory_explore`)
/// can calibrate verbosity against how busy the project's been.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ActivityWindow {
    /// Window size in days (e.g. 7 or 30).
    pub days: u32,
    /// Sessions whose `created_at` falls in the window.
    pub sessions: u64,
    /// Observations whose `created_at` falls in the window.
    pub observations: u64,
    /// Pages whose `updated_at` falls in the window — counts only
    /// `is_latest = 1`. Supersession of an old version into a new one
    /// counts as one update (the new row).
    pub pages_updated: u64,
}

/// Snapshot used by `memory_briefing` and the LLM-driven
/// `memory_explore`. Pure SQL aggregation; no LLM, no schema reads
/// outside the existing `pages` / `sessions` / `observations` /
/// `handoffs` tables.
#[derive(Debug, Clone, Default, Serialize)]
pub struct BriefingSnapshot {
    /// Lifetime totals — same shape `memory_status` returns today.
    pub counts: StatusCounts,
    /// Activity over the last 7 days.
    pub activity_7d: ActivityWindow,
    /// Activity over the last 30 days.
    pub activity_30d: ActivityWindow,
    /// Timestamp of the most recent observation (ISO-8601), or `null`
    /// if no observations exist. The `now - last_observation_at` gap
    /// is the signal `memory_explore` uses to scale its verbosity.
    pub last_observation_at: Option<String>,
    /// Number of open (un-accepted) handoffs.
    pub pending_handoff_count: u64,
    /// All pages currently under `_rules/` — small, surfaced verbatim
    /// because they're the highest-signal type of memory.
    pub rules: Vec<BriefingPage>,
    /// Small pinned pages under `_slots/` for active project context,
    /// preferences, current focus, and pending items.
    pub slots: Vec<BriefingPage>,
    /// Top-N most-recently-updated `is_latest = 1` pages.
    pub recent_pages: Vec<BriefingPage>,
    /// Distinct other projects whose pages link INTO this project (who
    /// depends on us). Project-scoped briefings only; `0` for
    /// workspace/global snapshots.
    pub cross_project_dependents: u64,
    /// Distinct other projects this project's pages link OUT to (what we
    /// depend on). Project-scoped briefings only; `0` otherwise.
    pub cross_project_dependencies: u64,
}

/// Trimmed page view for the briefing — path, title, kind, updated_at
/// timestamp. Body and snippets are intentionally omitted (the caller
/// can follow up with `memory_query` if they need detail).
#[derive(Debug, Clone, Serialize)]
pub struct BriefingPage {
    /// Relative wiki path.
    pub path: String,
    /// Page title (first H1 / frontmatter title).
    pub title: String,
    /// Semantic classification, derived from explicit frontmatter or the
    /// page's canonical path family.
    pub kind: String,
    /// ISO-8601 timestamp of the last update.
    pub updated_at: String,
}

/// One core page of the session-start project brief — body included,
/// because the brief is injected as agent context and the whole point is
/// sparing the agent a re-exploration round-trip. Returned by
/// [`ReaderPool::session_brief_pages`].
#[derive(Debug, Clone, Serialize)]
pub struct BriefPageBody {
    /// Relative wiki path.
    pub path: String,
    /// Page title (first H1 / frontmatter title).
    pub title: String,
    /// Full markdown body of the latest version. The hooks-side renderer
    /// applies the char budget; the store returns it whole.
    pub body: String,
    /// Whether the page is pinned (decay-immune, operator-curated).
    pub pinned: bool,
    /// ISO-8601 timestamp of the last update.
    pub updated_at: String,
}

/// One row per (workspace, project) with aggregate stats.
/// Returned by [`ReaderPool::list_projects_with_stats`].
#[derive(Debug, Clone, Serialize)]
pub struct ProjectSummary {
    /// Name of the workspace.
    pub workspace_name: String,
    /// Name of the project within the workspace.
    pub project_name: String,
    /// Number of `is_latest = 1` pages.
    pub page_count: u64,
    /// ISO-8601 timestamp of the newest `updated_at`, or `None` when
    /// the project has no pages yet.
    pub last_updated: Option<String>,
}

/// One workspace scope with the id + name needed to write its
/// self-describing `_meta.md` manifest. Returned by
/// [`ReaderPool::list_all_workspace_scopes`].
#[derive(Debug, Clone)]
pub struct WorkspaceScopeRow {
    /// Workspace id — matches the level-1 wiki directory name.
    pub workspace_id: WorkspaceId,
    /// Human-readable workspace name.
    pub workspace_name: String,
}

/// One `(workspace, project)` scope with the ids + repo_path needed to write
/// its self-describing `_meta.md` manifest. Returned by
/// [`ReaderPool::list_all_scopes`]; consumed by `Wiki::backfill_scope_manifests`.
#[derive(Debug, Clone)]
pub struct ScopeRow {
    /// Workspace id — matches the level-1 wiki directory name.
    pub workspace_id: WorkspaceId,
    /// Human-readable workspace name.
    pub workspace_name: String,
    /// Project id — matches the level-2 wiki directory name.
    pub project_id: ProjectId,
    /// Human-readable project name.
    pub project_name: String,
    /// Filesystem path the project's cwd-based routing resolves to, if any.
    pub repo_path: Option<String>,
}

/// One row per workspace with aggregate stats.
/// Returned by [`ReaderPool::list_workspaces_with_stats`].
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceSummary {
    /// Name of the workspace.
    pub workspace_name: String,
    /// Number of projects in this workspace.
    pub project_count: u64,
    /// Number of `is_latest = 1` pages across the workspace.
    pub page_count: u64,
    /// ISO-8601 timestamp of the newest `updated_at`, or `None` when
    /// the workspace has no pages yet.
    pub last_updated: Option<String>,
}

/// Page summary for tree-view rendering (no body).
/// Returned by [`ReaderPool::list_pages`].
#[derive(Debug, Clone, Serialize)]
pub struct PageSummary {
    /// Relative path within the wiki tree.
    pub path: String,
    /// Page title.
    pub title: String,
    /// Semantic kind: `fact` | `rule` | `decision` | `gotcha` | …
    pub kind: String,
    /// Memory tier: `working` | `episodic` | `semantic` | `procedural`.
    pub tier: String,
    /// ISO-8601 timestamp of last update.
    pub updated_at: String,
}

/// Page author surfaced alongside read responses (P1.7).
/// JOINed from the `users` table when `pages.author_id IS NOT NULL`;
/// `None` for anonymous + root writes (where attribution lives only in
/// the on-disk frontmatter `last_modified_by` block from P1.6).
///
/// Repeated here rather than reused from `ai_memory_core::User` because
/// the response shape intentionally omits internal fields (id,
/// created_at, last_seen_at, token_expired_at) — only the human-facing
/// identity is part of the API contract.
#[derive(Debug, Clone, Serialize)]
pub struct PageAuthor {
    /// Stable username (the attribution key recorded on writes).
    pub username: String,
    /// Optional display name (`Alice Smith`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional email surfaced alongside the username in UIs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// Full page metadata for the page-view template.
/// Returned by [`ReaderPool::page_meta`].
#[derive(Debug, Clone, Serialize)]
pub struct PageMeta {
    /// Name of the workspace.
    pub workspace_name: String,
    /// Name of the project.
    pub project_name: String,
    /// UUID of the workspace — used to construct the per-project wiki path.
    pub workspace_id: WorkspaceId,
    /// UUID of the project — used to construct the per-project wiki path.
    pub project_id: ProjectId,
    /// Relative wiki path.
    pub path: String,
    /// Page title.
    pub title: String,
    /// Semantic kind.
    pub kind: String,
    /// Memory tier.
    pub tier: String,
    /// Whether the page is pinned (decay-immune).
    pub pinned: bool,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
    /// ISO-8601 last-update timestamp.
    pub updated_at: String,
    /// Path of the page this one supersedes, if any.
    pub supersedes: Option<String>,
    /// Multi-user attribution (P1.7). `None` for pre-multi-user pages
    /// and for root / anonymous writes (where `pages.author_id IS
    /// NULL`); `Some` when JOIN resolves a `users` row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<PageAuthor>,
    /// ISO-8601 TTL instant from the frontmatter `expires_at:` key.
    /// Exact-path/id reads return expired pages (an explicit read is
    /// not search); callers annotate expiry via [`PageMeta::expired`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// `true` when [`PageMeta::expires_at`] is in the past — the page
    /// is hidden from retrieval and awaiting the next forget sweep.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub expired: bool,
}

/// One resolved cross-project edge (a link whose endpoints live in
/// different projects). The `/api/v1/graph` endpoint returns these; the UI
/// builds nodes from the endpoints and can aggregate to a project graph.
#[derive(Debug, Clone, Serialize)]
pub struct CrossProjectEdge {
    /// Source page workspace.
    pub from_workspace: String,
    /// Source page project.
    pub from_project: String,
    /// Source page path.
    pub from_path: String,
    /// Target page workspace.
    pub to_workspace: String,
    /// Target page project.
    pub to_project: String,
    /// Target page path.
    pub to_path: String,
}

/// An unresolved cross-project link — a declared dependency on another
/// project's page that does not resolve. Surfaced by `memory_lint`.
#[derive(Debug, Clone, Serialize)]
pub struct DanglingCrossLink {
    /// Path of the page that authored the link (in the queried project).
    pub from_path: String,
    /// Target workspace name (`None` = the source page's own workspace).
    pub workspace: Option<String>,
    /// Target project name.
    pub project: String,
    /// Target page path within that project.
    pub path: String,
    /// Whether the named target project exists at all. `false` →
    /// likely a typo / wrong name; `true` → the page is missing or was
    /// renamed/deleted in an existing project (a broken dependency).
    pub project_exists: bool,
}

/// A page related to another through the link graph — used by the
/// page-view "references / referenced by" panel. Body is omitted; just
/// enough to render a clickable row.
#[derive(Debug, Clone, Serialize)]
pub struct RelatedPage {
    /// Relative wiki path of the related page.
    pub path: String,
    /// Title of the related page.
    pub title: String,
    /// Semantic kind of the related page.
    pub kind: String,
    /// Workspace the related page lives in. Lets a backlink from another
    /// project be labelled / navigated (the cross-project dependency signal).
    pub workspace: String,
    /// Project the related page lives in.
    pub project: String,
}

/// Resolved outgoing links and incoming back-links for one page.
/// Returned by [`ReaderPool::page_links`].
#[derive(Debug, Clone, Default, Serialize)]
pub struct PageLinks {
    /// Latest pages this page references (resolved outgoing links).
    pub links: Vec<RelatedPage>,
    /// Latest pages that reference this page (incoming back-links).
    pub backlinks: Vec<RelatedPage>,
}

/// One page flagged by a workspace health check, with enough identity to
/// render a clickable drill-down row across projects.
#[derive(Debug, Clone, Serialize)]
pub struct HealthPage {
    /// Workspace name.
    pub workspace: String,
    /// Project name within the workspace.
    pub project: String,
    /// Relative wiki path.
    pub path: String,
    /// Page title.
    pub title: String,
    /// Semantic kind.
    pub kind: String,
}

/// Drill-down lists backing the workspace "memory health" counters.
/// Each list is capped; the headline counts stay authoritative.
/// Returned by [`ReaderPool::health_detail_for_workspace`].
#[derive(Debug, Clone, Default, Serialize)]
pub struct HealthDetail {
    /// Episodic latest pages untouched for over 30 days.
    pub stale: Vec<HealthPage>,
    /// Latest pages sharing a title with at least one other page.
    pub duplicates: Vec<HealthPage>,
    /// Latest pages with no incoming or outgoing links.
    pub orphans: Vec<HealthPage>,
}

/// Cheap, cloneable read-only connection pool handle.
#[derive(Clone)]
pub struct ReaderPool {
    inner: Arc<Inner>,
}

struct Inner {
    db_path: PathBuf,
    pool: Mutex<Vec<Connection>>,
    soft_cap: usize,
}

impl ReaderPool {
    /// Initialise the pool. Connections are opened lazily on first use.
    ///
    /// # Errors
    /// Currently infallible, but reserved so we can pre-open connections
    /// in a later milestone.
    pub fn new(db_path: &Path, soft_cap: usize) -> StoreResult<Self> {
        Ok(Self {
            inner: Arc::new(Inner {
                db_path: db_path.to_path_buf(),
                pool: Mutex::new(Vec::with_capacity(soft_cap.max(1))),
                soft_cap: soft_cap.max(1),
            }),
        })
    }

    /// Run a synchronous closure against a pooled read-only connection.
    ///
    /// The closure runs on the tokio blocking pool so it never starves the
    /// async runtime. If the pool is empty we open a fresh connection;
    /// on return we keep it only when the pool is below its soft cap.
    ///
    /// # Errors
    /// Returns [`StoreError::PoolPanic`] if the blocking task panics; any
    /// error returned by the closure is propagated unchanged.
    pub async fn with_conn<F, T>(&self, f: F) -> StoreResult<T>
    where
        F: FnOnce(&Connection) -> StoreResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let conn = checkout(&inner)?;
            let result = f(&conn);
            checkin(&inner, conn);
            result
        })
        .await
        .map_err(|e| StoreError::PoolPanic(e.to_string()))?
    }

    /// Return the current state of one `ai-memory run` invocation.
    pub async fn managed_run_status(
        &self,
        run_id: ManagedRunId,
    ) -> StoreResult<Option<StoredManagedRunStatus>> {
        self.with_conn(move |conn| crate::workstream::run_status(conn, run_id))
            .await
    }

    /// Return the unseen portable range assigned to a managed SessionStart.
    pub async fn managed_run_context(
        &self,
        run_id: ManagedRunId,
        max_events: usize,
    ) -> StoreResult<Option<ManagedRunContext>> {
        self.with_conn(move |conn| crate::workstream::run_context(conn, run_id, max_events))
            .await
    }

    /// Search visible events in one managed workstream, or return its newest
    /// events when `query` is empty.
    pub async fn search_workstream_events(
        &self,
        workstream_id: WorkstreamId,
        query: String,
        limit: usize,
    ) -> StoreResult<Vec<WorkstreamEvent>> {
        self.with_conn(move |conn| {
            crate::workstream::search_events(conn, workstream_id, &query, limit)
        })
        .await
    }

    /// Run a full-text search against the FTS5 index, apply the bounded page
    /// authority adjustment, and return the top `is_latest = 1` matches.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn search_pages(&self, query: String, limit: usize) -> StoreResult<Vec<PageHit>> {
        let fts_query = normalize_fts_query(&query);
        if fts_query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("pages.path", "pages.frontmatter_json");
            let sql = format!(
                "SELECT pages.id, pages.path, pages.title, \
                        snippet(pages_fts, 1, '<mark>', '</mark>', '…', 24) AS snip, \
                        pages_fts.rank, pages.tier, pages.pinned, \
                        pages.frontmatter_json, {kind_expr} AS kind \
                 FROM pages_fts \
                 JOIN pages ON pages.rowid = pages_fts.rowid \
                 WHERE pages_fts MATCH ?1 AND pages.is_latest = 1{not_expired} \
                 ORDER BY pages_fts.rank \
                 LIMIT ?2",
                not_expired = not_expired("pages", "?3"),
            );
            let mut stmt = conn.prepare(&sql)?;
            #[allow(clippy::cast_possible_wrap)]
            let rows = stmt.query_map(
                params![fts_query, authority_candidate_limit(limit) as i64, now_us()],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    let path: String = row.get(1)?;
                    let title: String = row.get(2)?;
                    let snippet: String = row.get(3)?;
                    let rank: f64 = row.get(4)?;
                    let tier: String = row.get(5)?;
                    let pinned = row.get::<_, i64>(6)? != 0;
                    let frontmatter_json: String = row.get(7)?;
                    let kind: String = row.get(8)?;
                    Ok((
                        id_bytes,
                        path,
                        title,
                        snippet,
                        rank,
                        tier,
                        pinned,
                        frontmatter_json,
                        kind,
                    ))
                },
            )?;

            let mut candidates = Vec::new();
            for row in rows {
                let (id_bytes, path, title, snippet, rank, tier, pinned, frontmatter_json, kind) =
                    row?;
                let authority =
                    PageAuthority::from_stored(&path, &kind, &tier, pinned, &frontmatter_json);
                candidates.push((
                    PageHit {
                        id: PageId::from_slice(&id_bytes)?,
                        path: PagePath::new(path)?,
                        title,
                        snippet,
                        rank,
                    },
                    authority,
                ));
            }
            Ok(rerank_page_hits(candidates, limit))
        })
        .await
    }

    /// Run a global authority-adjusted full-text search and include
    /// workspace/project names in each row. This keeps the web search route
    /// to one SQLite query instead of one search query plus a metadata lookup
    /// per hit.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn search_pages_with_meta(
        &self,
        query: String,
        limit: usize,
        expiry_cutoff_us: Option<i64>,
    ) -> StoreResult<Vec<PageHitWithMeta>> {
        let fts_query = normalize_fts_query(&query);
        if fts_query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let cutoff = expiry_cutoff_us.unwrap_or_else(now_us);
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("pages.path", "pages.frontmatter_json");
            let sql = format!(
                "SELECT workspaces.name, projects.name, pages.path, pages.title, \
                        snippet(pages_fts, 1, '<mark>', '</mark>', '…', 24) AS snip, \
                        pages_fts.rank, pages.tier, pages.pinned, \
                        pages.frontmatter_json, {kind_expr} AS kind \
                 FROM pages_fts \
                 JOIN pages ON pages.rowid = pages_fts.rowid \
                 JOIN projects ON projects.id = pages.project_id \
                 JOIN workspaces ON workspaces.id = pages.workspace_id \
                 WHERE pages_fts MATCH ?1 AND pages.is_latest = 1{not_expired} \
                 ORDER BY pages_fts.rank \
                 LIMIT ?2",
                not_expired = not_expired("pages", "?3"),
            );
            let mut stmt = conn.prepare(&sql)?;
            #[allow(clippy::cast_possible_wrap)]
            let rows = stmt.query_map(
                params![fts_query, authority_candidate_limit(limit) as i64, cutoff],
                |row| {
                    let workspace_name: String = row.get(0)?;
                    let project_name: String = row.get(1)?;
                    let path: String = row.get(2)?;
                    let title: String = row.get(3)?;
                    let snippet: String = row.get(4)?;
                    let rank: f64 = row.get(5)?;
                    let tier: String = row.get(6)?;
                    let pinned = row.get::<_, i64>(7)? != 0;
                    let frontmatter_json: String = row.get(8)?;
                    let kind: String = row.get(9)?;
                    Ok((
                        workspace_name,
                        project_name,
                        path,
                        title,
                        snippet,
                        rank,
                        tier,
                        pinned,
                        frontmatter_json,
                        kind,
                    ))
                },
            )?;

            let mut candidates = Vec::new();
            for row in rows {
                let (
                    workspace_name,
                    project_name,
                    path,
                    title,
                    snippet,
                    rank,
                    tier,
                    pinned,
                    frontmatter_json,
                    kind,
                ) = row?;
                let authority =
                    PageAuthority::from_stored(&path, &kind, &tier, pinned, &frontmatter_json);
                candidates.push((
                    PageHitWithMeta {
                        workspace_name,
                        project_name,
                        path: PagePath::new(path)?,
                        title,
                        snippet,
                        rank,
                    },
                    authority,
                ));
            }
            Ok(rerank_page_hits_with_meta(candidates, limit))
        })
        .await
    }

    /// Run an authority-adjusted full-text search scoped to one project.
    ///
    /// `expiry_cutoff_us`: `None` hides pages whose TTL has passed as of
    /// now (the default retrieval behaviour); `Some(cutoff)` compares
    /// against that instant instead — pass `i64::MIN` to include
    /// expired pages.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn search_pages_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query: String,
        limit: usize,
        expiry_cutoff_us: Option<i64>,
    ) -> StoreResult<Vec<PageHit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let candidates = self
            .search_page_candidates_for_project(
                workspace_id,
                project_id,
                query,
                authority_candidate_limit(limit),
                expiry_cutoff_us,
            )
            .await?;
        Ok(rerank_page_hits(candidates, limit))
    }

    async fn search_page_candidates_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query: String,
        candidate_limit: usize,
        expiry_cutoff_us: Option<i64>,
    ) -> StoreResult<Vec<(PageHit, PageAuthority)>> {
        let fts_query = normalize_fts_query(&query);
        if fts_query.is_empty() || candidate_limit == 0 {
            return Ok(Vec::new());
        }
        let cutoff = expiry_cutoff_us.unwrap_or_else(now_us);
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("pages.path", "pages.frontmatter_json");
            let sql = format!(
                "SELECT pages.id, pages.path, pages.title, \
                        snippet(pages_fts, 1, '<mark>', '</mark>', '…', 24) AS snip, \
                        pages_fts.rank, pages.tier, pages.pinned, \
                        pages.frontmatter_json, {kind_expr} AS kind \
                 FROM pages_fts \
                 JOIN pages ON pages.rowid = pages_fts.rowid \
                 WHERE pages_fts MATCH ?1 \
                   AND pages.workspace_id = ?2 \
                   AND pages.project_id = ?3 \
                   AND pages.is_latest = 1{not_expired} \
                 ORDER BY pages_fts.rank \
                 LIMIT ?4",
                not_expired = not_expired("pages", "?5"),
            );
            let mut stmt = conn.prepare(&sql)?;
            #[allow(clippy::cast_possible_wrap)]
            let rows = stmt.query_map(
                params![
                    fts_query,
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    candidate_limit as i64,
                    cutoff
                ],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    let path: String = row.get(1)?;
                    let title: String = row.get(2)?;
                    let snippet: String = row.get(3)?;
                    let rank: f64 = row.get(4)?;
                    let tier: String = row.get(5)?;
                    let pinned = row.get::<_, i64>(6)? != 0;
                    let frontmatter_json: String = row.get(7)?;
                    let kind: String = row.get(8)?;
                    Ok((
                        id_bytes,
                        path,
                        title,
                        snippet,
                        rank,
                        tier,
                        pinned,
                        frontmatter_json,
                        kind,
                    ))
                },
            )?;

            let mut candidates = Vec::new();
            for row in rows {
                let (id_bytes, path, title, snippet, rank, tier, pinned, frontmatter_json, kind) =
                    row?;
                let authority =
                    PageAuthority::from_stored(&path, &kind, &tier, pinned, &frontmatter_json);
                candidates.push((
                    PageHit {
                        id: PageId::from_slice(&id_bytes)?,
                        path: PagePath::new(path)?,
                        title,
                        snippet,
                        rank,
                    },
                    authority,
                ));
            }
            Ok(candidates)
        })
        .await
    }

    /// Run a full-text search against raw observations scoped to one project.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn search_observations_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query: String,
        limit: usize,
    ) -> StoreResult<Vec<ObservationHit>> {
        let fts_query = normalize_fts_query(&query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT observations.id, observations.session_id, observations.kind, \
                        observations.title, \
                        snippet(observations_fts, 1, '<mark>', '</mark>', '…', 24) AS snip, \
                        observations_fts.rank, observations.created_at \
                 FROM observations_fts \
                 JOIN observations ON observations.rowid = observations_fts.rowid \
                 WHERE observations_fts MATCH ?1 \
                   AND observations.workspace_id = ?2 \
                   AND observations.project_id = ?3 \
                 ORDER BY observations_fts.rank \
                 LIMIT ?4",
            )?;
            #[allow(clippy::cast_possible_wrap)]
            let rows = stmt.query_map(
                params![
                    fts_query,
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    limit as i64,
                ],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    let session_bytes: Vec<u8> = row.get(1)?;
                    let kind: String = row.get(2)?;
                    let title: String = row.get(3)?;
                    let snippet: String = row.get(4)?;
                    let rank: f64 = row.get(5)?;
                    let created_us: i64 = row.get(6)?;
                    Ok((
                        id_bytes,
                        session_bytes,
                        kind,
                        title,
                        snippet,
                        rank,
                        created_us,
                    ))
                },
            )?;

            let mut hits = Vec::new();
            for row in rows {
                let (id_bytes, session_bytes, kind, title, snippet, rank, created_us) = row?;
                let created_at = jiff::Timestamp::from_microsecond(created_us)
                    .map(|ts| ts.to_string())
                    .unwrap_or_default();
                hits.push(ObservationHit {
                    id: ObservationId::from_slice(&id_bytes)?,
                    session_id: SessionId::from_slice(&session_bytes)?,
                    kind,
                    title,
                    snippet,
                    rank,
                    created_at,
                });
            }
            Ok(hits)
        })
        .await
    }

    /// Return the N most-recently-updated `is_latest = 1` pages.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn recent_pages(&self, limit: usize) -> StoreResult<Vec<PageHit>> {
        self.with_conn(move |conn| {
            let sql = format!(
                "SELECT id, path, title, \
                        substr(body, 1, 240) AS snip, \
                        CAST(updated_at AS REAL) AS rank \
                 FROM pages \
                 WHERE is_latest = 1{not_expired} \
                 ORDER BY updated_at DESC \
                 LIMIT ?1",
                not_expired = not_expired("pages", "?2"),
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            #[allow(clippy::cast_possible_wrap)]
            let rows = stmt.query_map(params![limit as i64, now_us()], |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let path: String = row.get(1)?;
                let title: String = row.get(2)?;
                let snippet: String = row.get(3)?;
                let rank: f64 = row.get(4)?;
                Ok((id_bytes, path, title, snippet, rank))
            })?;
            let mut hits = Vec::new();
            for row in rows {
                let (id_bytes, path, title, snippet, rank) = row?;
                hits.push(PageHit {
                    id: PageId::from_slice(&id_bytes)?,
                    path: PagePath::new(path)?,
                    title,
                    snippet,
                    rank,
                });
            }
            Ok(hits)
        })
        .await
    }

    /// Return the N most-recently-updated pages scoped to one project.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn recent_pages_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        limit: usize,
    ) -> StoreResult<Vec<PageHit>> {
        self.with_conn(move |conn| {
            let sql = format!(
                "SELECT id, path, title, \
                        substr(body, 1, 240) AS snip, \
                        CAST(updated_at AS REAL) AS rank \
                 FROM pages \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1{not_expired} \
                 ORDER BY updated_at DESC \
                 LIMIT ?3",
                not_expired = not_expired("pages", "?4"),
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            #[allow(clippy::cast_possible_wrap)]
            let rows = stmt.query_map(
                params![
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    limit as i64,
                    now_us()
                ],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    let path: String = row.get(1)?;
                    let title: String = row.get(2)?;
                    let snippet: String = row.get(3)?;
                    let rank: f64 = row.get(4)?;
                    Ok((id_bytes, path, title, snippet, rank))
                },
            )?;
            let mut hits = Vec::new();
            for row in rows {
                let (id_bytes, path, title, snippet, rank) = row?;
                hits.push(PageHit {
                    id: PageId::from_slice(&id_bytes)?,
                    path: PagePath::new(path)?,
                    title,
                    snippet,
                    rank,
                });
            }
            Ok(hits)
        })
        .await
    }

    /// The `limit` most-recently-updated `is_latest` pages across EVERY
    /// project, each annotated with its workspace + project name. The
    /// cross-project analog of [`Self::recent_pages_for_project`]; used when a
    /// read's scope broadens to global.
    ///
    /// Recency has no FTS relevance score, so the reused
    /// [`PageHitWithMeta::rank`] field carries `updated_at` (µs, cast to
    /// REAL) — larger still means "ranks first", callers must not read it
    /// as an FTS rank.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn recent_pages_global(&self, limit: usize) -> StoreResult<Vec<PageHitWithMeta>> {
        self.with_conn(move |conn| {
            let sql = format!(
                "SELECT workspaces.name, projects.name, pages.path, pages.title, \
                        substr(pages.body, 1, 240) AS snip, \
                        CAST(pages.updated_at AS REAL) AS rank \
                 FROM pages \
                 JOIN projects ON projects.id = pages.project_id \
                 JOIN workspaces ON workspaces.id = pages.workspace_id \
                 WHERE pages.is_latest = 1{not_expired} \
                 ORDER BY pages.updated_at DESC \
                 LIMIT ?1",
                not_expired = not_expired("pages", "?2"),
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            #[allow(clippy::cast_possible_wrap)]
            let rows = stmt.query_map(params![limit as i64, now_us()], |row| {
                let workspace_name: String = row.get(0)?;
                let project_name: String = row.get(1)?;
                let path: String = row.get(2)?;
                let title: String = row.get(3)?;
                let snippet: String = row.get(4)?;
                let rank: f64 = row.get(5)?;
                Ok((workspace_name, project_name, path, title, snippet, rank))
            })?;
            let mut hits = Vec::new();
            for row in rows {
                let (workspace_name, project_name, path, title, snippet, rank) = row?;
                hits.push(PageHitWithMeta {
                    workspace_name,
                    project_name,
                    path: PagePath::new(path)?,
                    title,
                    snippet,
                    rank,
                });
            }
            Ok(hits)
        })
        .await
    }

    /// Return all observations for the given session, ordered by
    /// `created_at` ascending.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn observations_for_session(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Vec<Observation>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT id, session_id, workspace_id, project_id, kind, extension, source_event, \
                        title, body, importance, created_at \
                 FROM observations \
                 WHERE session_id = ?1 \
                 ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map(params![session_id.as_bytes()], row_to_observation)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r??);
            }
            Ok(out)
        })
        .await
    }

    /// Return the latest completed session for a project.
    ///
    /// Used by read-only review tools that need a natural default when the user
    /// asks what the project just learned without naming a session id.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn latest_completed_session_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<Option<SessionId>> {
        self.with_conn(move |conn| {
            let row_opt: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT id FROM sessions \
                     WHERE workspace_id = ?1 AND project_id = ?2 AND ended_at IS NOT NULL \
                     ORDER BY ended_at DESC, started_at DESC LIMIT 1",
                    params![workspace_id.as_bytes(), project_id.as_bytes()],
                    |row| row.get(0),
                )
                .optional()?;
            row_opt
                .map(|bytes| SessionId::from_slice(&bytes).map_err(StoreError::from))
                .transpose()
        })
        .await
    }

    /// Return open sessions matching one scoped project and agent.
    ///
    /// Results are newest-first so callers can default to finalizing only the
    /// latest open session while offering an explicit all-sessions mode.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn open_sessions_for_scope_agent(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        agent_kind: AgentKind,
        owner_filter: OwnerFilter,
        limit: Option<usize>,
    ) -> StoreResult<Vec<OpenSession>> {
        let agent = agent_kind.as_str().to_string();
        self.with_conn(move |conn| {
            let limit_clause = limit.map_or(String::new(), |n| format!(" LIMIT {}", n.max(1)));
            // Without the owner predicate this returns whichever session in the
            // scope started last — frequently a teammate's live one — and the
            // callers act on it destructively.
            let owner_clause = match &owner_filter {
                OwnerFilter::Any => "",
                OwnerFilter::User(_) => " AND (actor_user IS NULL OR actor_user = ?4)",
                OwnerFilter::Unattributed => " AND actor_user IS NULL",
            };
            let sql = format!(
                "SELECT id, cwd FROM sessions \
                 WHERE workspace_id = ?1 AND project_id = ?2 \
                   AND agent_kind = ?3 AND ended_at IS NULL{owner_clause} \
                 ORDER BY started_at DESC, id DESC{limit_clause}"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let row_map = |row: &rusqlite::Row<'_>| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let cwd: Option<String> = row.get(1)?;
                Ok((id_bytes, cwd))
            };
            let rows = match &owner_filter {
                OwnerFilter::User(user) => stmt.query_map(
                    params![workspace_id.as_bytes(), project_id.as_bytes(), agent, user],
                    row_map,
                )?,
                _ => stmt.query_map(
                    params![workspace_id.as_bytes(), project_id.as_bytes(), agent],
                    row_map,
                )?,
            };
            let mut out = Vec::new();
            for row in rows {
                let (id_bytes, cwd) = row?;
                out.push(OpenSession {
                    session_id: SessionId::from_slice(&id_bytes)?,
                    cwd,
                });
            }
            Ok(out)
        })
        .await
    }

    /// The `(workspace_id, project_id, cwd)` a session was created under,
    /// or `None` when no such session exists. The hook router uses this for
    /// session-sticky attribution: mid-session events inherit the session's
    /// scope instead of re-deriving a project from the event's cwd, so a
    /// `cd subdir/` inside a non-git project (whose parent has no
    /// `repo_path` for the prefix match to key on) can no longer scatter
    /// observations into basename-fragment projects. The session's own cwd
    /// is returned so the router can bound stickiness to the session's
    /// directory subtree.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn find_session_scope(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Option<(WorkspaceId, ProjectId, Option<String>)>> {
        self.with_conn(move |conn| {
            let row = conn
                .query_row(
                    "SELECT workspace_id, project_id, cwd FROM sessions WHERE id = ?1",
                    params![session_id.as_bytes()],
                    |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, Vec<u8>>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .optional()?;
            match row {
                Some((ws, proj, cwd)) => Ok(Some((
                    WorkspaceId::from_slice(&ws)?,
                    ProjectId::from_slice(&proj)?,
                    cwd,
                ))),
                None => Ok(None),
            }
        })
        .await
    }

    /// How a `SessionEnd` should treat its target session (issue #152).
    ///
    /// The old boolean ("is the session open?") conflated two very different
    /// ended states: a *duplicate/stale* end (the observation generation did
    /// not advance — drop it, the reason the guard exists) and a *re-end* of a
    /// resumed session (the agent reused the id and kept working after the
    /// first end — the end path must run again or the resumed work never
    /// reaches the compiled session page). A persisted count is deliberately
    /// used instead of comparing wall clocks, which does not converge after
    /// clock skew (issue #261).
    pub async fn session_end_disposition(
        &self,
        session_id: SessionId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        agent_kind: AgentKind,
    ) -> StoreResult<SessionEndDisposition> {
        let agent = agent_kind.as_str().to_string();
        self.with_conn(move |conn| {
            let end_state: Option<(Option<i64>, u64)> = conn
                .query_row(
                    "SELECT ended_at, ended_observation_count FROM sessions \
                     WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3 \
                       AND agent_kind = ?4",
                    params![
                        session_id.as_bytes(),
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                        agent,
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((ended_at, ended_observation_count)) = end_state else {
                // Missing, cross-scope, or cross-agent: never end it.
                return Ok(SessionEndDisposition::DropInvalid);
            };
            if ended_at.is_none() {
                return Ok(SessionEndDisposition::Open);
            }
            let current_observation_count: u64 = conn.query_row(
                "SELECT COUNT(*) FROM observations WHERE session_id = ?1",
                params![session_id.as_bytes()],
                |row| row.get(0),
            )?;
            if current_observation_count > ended_observation_count {
                Ok(SessionEndDisposition::ReEndWithNewWork)
            } else {
                Ok(SessionEndDisposition::AlreadyEnded)
            }
        })
        .await
    }

    /// Return completed sessions eligible for scheduled auto-improvement.
    ///
    /// The scheduler uses a persisted first-run watermark so an upgrade with an
    /// LLM provider configured does not chew through historical backlog by
    /// default. The scheduler inserts a per-session claim before the LLM call,
    /// so failed scheduled reviews do not retry forever or starve newer
    /// sessions. Any existing auto-improvement run row for a session also counts
    /// as a scheduler review for candidate selection; manual reruns remain
    /// available through CLI/admin/MCP.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn auto_improve_candidate_sessions(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        min_session_age_secs: u64,
        limit: usize,
    ) -> StoreResult<Vec<AutoImproveCandidateSession>> {
        let cutoff = Timestamp::now().as_microsecond().saturating_sub(
            (min_session_age_secs.min(i64::MAX as u64 / 1_000_000) as i64) * 1_000_000,
        );
        self.with_conn(move |conn| {
            let watermark: Option<i64> = conn
                .query_row(
                    "SELECT watermark_ended_at FROM auto_improve_scheduler_state \
                     WHERE workspace_id = ?1 AND project_id = ?2",
                    params![workspace_id.as_bytes(), project_id.as_bytes()],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(watermark) = watermark else {
                return Ok(Vec::new());
            };

            let mut stmt = conn.prepare_cached(
                "SELECT s.id, s.ended_at FROM sessions s \
                 WHERE s.workspace_id = ?1 \
                   AND s.project_id = ?2 \
                   AND s.ended_at IS NOT NULL \
                   AND s.ended_at > ?3 \
                   AND s.ended_at <= ?4 \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM auto_improve_scheduler_claims c \
                       WHERE c.workspace_id = s.workspace_id \
                         AND c.project_id = s.project_id \
                         AND c.session_id = s.id \
                   ) \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM auto_improve_runs r \
                       WHERE r.workspace_id = s.workspace_id \
                         AND r.project_id = s.project_id \
                         AND r.session_id = s.id \
                   ) \
                 ORDER BY s.ended_at ASC, s.started_at ASC \
                 LIMIT ?5",
            )?;
            let rows = stmt.query_map(
                params![
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    watermark,
                    cutoff,
                    limit.min(i64::MAX as usize) as i64,
                ],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    Ok((id_bytes, row.get::<_, i64>(1)?))
                },
            )?;
            let mut out = Vec::new();
            for row in rows {
                let (id_bytes, ended_at) = row?;
                out.push(AutoImproveCandidateSession {
                    session_id: SessionId::from_slice(&id_bytes)?,
                    ended_at,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Look up the `(workspace_id, project_id)` a session belongs to.
    /// Returns `None` when no such session row exists.
    ///
    /// Used by the consolidator + lint pass to write pages into the
    /// SESSION'S project, not the server's startup defaults — every
    /// session row carries the project_id the hook router resolved
    /// from its per-cwd basename heuristic, which is the correct
    /// target for any wiki page derived from that session.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn session_project_ids(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Option<(WorkspaceId, ProjectId)>> {
        self.with_conn(move |conn| {
            let mut stmt =
                conn.prepare("SELECT workspace_id, project_id FROM sessions WHERE id = ?1")?;
            let mut rows = stmt.query(params![session_id.as_bytes()])?;
            let Some(row) = rows.next()? else {
                return Ok(None);
            };
            let ws_bytes: Vec<u8> = row.get(0)?;
            let proj_bytes: Vec<u8> = row.get(1)?;
            let ws = WorkspaceId::from_slice(&ws_bytes)
                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, 0))?;
            let proj = ProjectId::from_slice(&proj_bytes)
                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, 0))?;
            Ok(Some((ws, proj)))
        })
        .await
    }

    /// The operator a session belongs to, as recorded at session start.
    ///
    /// The identity carrying a SessionEnd is not necessarily the identity that
    /// owned the session: a spool drain, an operator finalizing a stuck
    /// session, or a shared static hook token can all deliver it. Attribution
    /// for the session's page and the baton it leaves must follow the session,
    /// not the request.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn session_actor_user(&self, session_id: SessionId) -> StoreResult<Option<String>> {
        self.with_conn(move |conn| {
            let owner: Option<Option<String>> = conn
                .query_row(
                    "SELECT actor_user FROM sessions WHERE id = ?1",
                    params![session_id.as_bytes()],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(owner.flatten())
        })
        .await
    }

    /// Resolve the `(workspace, project)` where a session's observations
    /// actually landed, independent of the (possibly stale) `sessions` row.
    ///
    /// The `sessions` row is frozen at the first session-start (`begin_session`
    /// uses `ON CONFLICT(id) DO NOTHING`), so a "hybrid" session that installed
    /// its scope marker mid-flight stays anchored to the pre-marker scope. The
    /// observation log, in contrast, carries the correct per-cwd scope for each
    /// captured event. Returns the scope holding the most observations for the
    /// session (ties broken by the most recent observation), or `None` when the
    /// session has no observations.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn session_scope_from_observations(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Option<(WorkspaceId, ProjectId)>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT workspace_id, project_id \
                 FROM observations \
                 WHERE session_id = ?1 \
                 GROUP BY workspace_id, project_id \
                 ORDER BY COUNT(*) DESC, MAX(created_at) DESC \
                 LIMIT 1",
            )?;
            let mut rows = stmt.query(params![session_id.as_bytes()])?;
            let Some(row) = rows.next()? else {
                return Ok(None);
            };
            let ws_bytes: Vec<u8> = row.get(0)?;
            let proj_bytes: Vec<u8> = row.get(1)?;
            let ws = WorkspaceId::from_slice(&ws_bytes).map_err(|e| {
                StoreError::Memory(ai_memory_core::MemoryError::MalformedRecord(format!(
                    "bad observation workspace_id: {e}"
                )))
            })?;
            let proj = ProjectId::from_slice(&proj_bytes).map_err(|e| {
                StoreError::Memory(ai_memory_core::MemoryError::MalformedRecord(format!(
                    "bad observation project_id: {e}"
                )))
            })?;
            Ok(Some((ws, proj)))
        })
        .await
    }

    /// Load every `is_latest=1` page's embedding for the project, but
    /// only when the stored `(provider, model, dim)` matches the
    /// caller's expectation. Mismatched rows are skipped (the
    /// refuse-on-mismatch check is `embedding_meta_for_mismatch`).
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn load_embeddings(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        provider: String,
        model: String,
        dim: u32,
    ) -> StoreResult<Vec<StoredEmbedding>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT page_embeddings.page_id, page_embeddings.vector, pages.path \
                 FROM page_embeddings \
                 JOIN pages ON pages.id = page_embeddings.page_id \
                 WHERE pages.workspace_id = ?1 \
                   AND pages.project_id = ?2 \
                   AND pages.is_latest = 1 \
                   AND page_embeddings.provider = ?3 \
                   AND page_embeddings.model = ?4 \
                   AND page_embeddings.dim = ?5",
            )?;
            let rows = stmt.query_map(
                params![
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    provider,
                    model,
                    dim,
                ],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    let vec_bytes: Vec<u8> = row.get(1)?;
                    let path: String = row.get(2)?;
                    Ok((id_bytes, vec_bytes, path))
                },
            )?;
            let mut out = Vec::new();
            for r in rows {
                let (id_bytes, vec_bytes, path) = r?;
                let id = PageId::from_slice(&id_bytes)?;
                let path = PagePath::new(path)?;
                let vector = bytes_to_f32_vec(&vec_bytes, dim)?;
                out.push(StoredEmbedding { id, path, vector });
            }
            Ok(out)
        })
        .await
    }

    /// Return page ids that already have a matching embedding row.
    ///
    /// This is cheaper than [`ReaderPool::load_embeddings`] for backfill paths
    /// that only need to skip already-embedded pages.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn embedded_page_ids(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        provider: String,
        model: String,
        dim: u32,
    ) -> StoreResult<Vec<PageId>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT page_embeddings.page_id \
                 FROM page_embeddings \
                 JOIN pages ON pages.id = page_embeddings.page_id \
                 WHERE pages.workspace_id = ?1 \
                   AND pages.project_id = ?2 \
                   AND pages.is_latest = 1 \
                   AND page_embeddings.provider = ?3 \
                   AND page_embeddings.model = ?4 \
                   AND page_embeddings.dim = ?5",
            )?;
            let rows = stmt.query_map(
                params![
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    provider,
                    model,
                    dim,
                ],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    Ok(id_bytes)
                },
            )?;
            let mut out = Vec::new();
            for row in rows {
                out.push(PageId::from_slice(&row?)?);
            }
            Ok(out)
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn top_embedding_hits_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query_vec: Vec<f32>,
        provider: String,
        model: String,
        dim: u32,
        limit: usize,
        expiry_cutoff_us: i64,
    ) -> StoreResult<Vec<(PageId, PagePath, f32)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        self.with_conn(move |conn| {
            let sql = format!(
                "SELECT page_embeddings.page_id, page_embeddings.vector, pages.path \
                 FROM page_embeddings \
                 JOIN pages ON pages.id = page_embeddings.page_id \
                 WHERE pages.workspace_id = ?1 \
                   AND pages.project_id = ?2 \
                   AND pages.is_latest = 1{not_expired} \
                   AND page_embeddings.provider = ?3 \
                   AND page_embeddings.model = ?4 \
                   AND page_embeddings.dim = ?5",
                not_expired = not_expired("pages", "?6"),
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(
                params![
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    provider,
                    model,
                    dim,
                    expiry_cutoff_us,
                ],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    let vec_bytes: Vec<u8> = row.get(1)?;
                    let path: String = row.get(2)?;
                    Ok((id_bytes, vec_bytes, path))
                },
            )?;

            let mut out = Vec::new();
            for row in rows {
                let (id_bytes, vec_bytes, path) = row?;
                out.push((
                    PageId::from_slice(&id_bytes)?,
                    PagePath::new(path)?,
                    dot_embedding_bytes(&query_vec, &vec_bytes, dim)?,
                ));
            }
            if out.len() > limit {
                out.select_nth_unstable_by(limit, score_desc);
                out.truncate(limit);
            }
            out.sort_by(score_desc);
            Ok(out)
        })
        .await
    }

    /// Return any `(provider, model, dim)` triples currently stored
    /// that *don't* match the caller's expectation. An empty vec
    /// means "all clean". Used at startup for the refuse-on-mismatch
    /// (agentmemory #469 lesson).
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn embedding_meta_for_mismatch(
        &self,
        provider: String,
        model: String,
        dim: u32,
    ) -> StoreResult<Vec<(String, String, u32, u64)>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT pe.provider, pe.model, pe.dim, COUNT(*) \
                 FROM page_embeddings pe \
                 JOIN pages pg ON pg.id = pe.page_id AND pg.is_latest = 1 \
                 WHERE NOT (pe.provider = ?1 AND pe.model = ?2 AND pe.dim = ?3) \
                 GROUP BY pe.provider, pe.model, pe.dim",
            )?;
            let rows = stmt.query_map(params![provider, model, dim], |row| {
                let provider: String = row.get(0)?;
                let model: String = row.get(1)?;
                let dim: i64 = row.get(2)?;
                let count: i64 = row.get(3)?;
                Ok((
                    provider,
                    model,
                    u32::try_from(dim).unwrap_or(0),
                    u64::try_from(count).unwrap_or(0),
                ))
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
    }

    /// Return decay-evaluation candidates for the M8 forget sweep.
    ///
    /// Walks `pages` rows with `is_latest = 1` and returns the columns
    /// the forget sweep needs to compute the retention formula. The
    /// sweep itself filters by tier (only `episodic`) + pinned flag,
    /// so this method does not pre-filter -- it just hands the data
    /// over.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn decay_candidates(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<Vec<DecayCandidate>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, path, tier, pinned, updated_at, access_count, last_accessed_at, \
                        frontmatter_json, expires_at, salience \
                 FROM pages \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1",
            )?;
            let rows = stmt.query_map(
                params![workspace_id.as_bytes(), project_id.as_bytes()],
                row_to_decay_candidate,
            )?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r??);
            }
            Ok(out)
        })
        .await
    }

    /// Return the number of DISTINCT operators that reinforced each
    /// `is_latest = 1` page of a project, for the sweep's breadth term.
    ///
    /// Scoped and grouped exactly like [`decay_candidates`](Self::decay_candidates)
    /// so the sweep pays for one query per run, never one per candidate. Pages
    /// with no per-operator rows (everything read anonymously, and everything
    /// that predates `page_access`) are simply absent from the map, which the
    /// caller reads as breadth 0 — scored identically to breadth 1 by
    /// [`retention_score_with_breadth`](crate::retention_score_with_breadth).
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn access_breadth_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<std::collections::HashMap<PageId, u32>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT pa.page_id, COUNT(DISTINCT pa.actor) \
                 FROM page_access pa \
                 JOIN pages p ON p.id = pa.page_id \
                 WHERE p.workspace_id = ?1 AND p.project_id = ?2 AND p.is_latest = 1 \
                 GROUP BY pa.page_id",
            )?;
            let rows = stmt.query_map(
                params![workspace_id.as_bytes(), project_id.as_bytes()],
                |row| {
                    let id: Vec<u8> = row.get(0)?;
                    let actors: i64 = row.get(1)?;
                    Ok((id, u32::try_from(actors).unwrap_or(u32::MAX)))
                },
            )?;
            let mut out = std::collections::HashMap::new();
            for r in rows {
                let (id, actors) = r?;
                out.insert(PageId::from_slice(&id)?, actors);
            }
            Ok(out)
        })
        .await
    }

    /// Rank pages by how many of the query's tokens match their indexed
    /// entities, weighting each match by inverse entity frequency — a
    /// query token matching an entity that appears on 2 pages is a much
    /// stronger signal than one matching an entity on 200.
    ///
    /// Matching is lexical (exact name, name prefix, or a compound-word
    /// prefix) so this stays a plain SQL stream with no LLM call. It returns
    /// no candidates when the project has no entities indexed, as in the
    /// common zero-LLM case without hand-edited entity frontmatter.
    ///
    /// `expiry_cutoff_us`: see [`Self::search_pages_for_project`].
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub(crate) async fn entity_hits_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query: &str,
        limit: usize,
        expiry_cutoff_us: Option<i64>,
    ) -> StoreResult<Vec<EntityHit>> {
        let tokens = entity_query_tokens(query);
        if tokens.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let cutoff = expiry_cutoff_us.unwrap_or_else(now_us);
        self.with_conn(move |conn| {
            let mut placeholders = String::with_capacity(tokens.len() * 96);
            let mut sql_params: Vec<Value> = Vec::with_capacity(tokens.len() * 5 + 7);
            for (idx, token) in tokens.iter().enumerate() {
                if idx > 0 {
                    placeholders.push_str(" OR ");
                }
                // Match exact names, name prefixes, and word prefixes after
                // any accepted entity separator. The latter lets `actor`
                // find `writer actor`, `writer-actor`, and `writer_actor`.
                //
                // `?` is positional-by-order, same as the graph query.
                placeholders.push_str(
                    "(e.name = ? \
                      OR e.name LIKE ? || '%' ESCAPE '\\' \
                      OR e.name LIKE '% ' || ? || '%' ESCAPE '\\' \
                      OR e.name LIKE '%-' || ? || '%' ESCAPE '\\' \
                      OR e.name LIKE '%\\_' || ? || '%' ESCAPE '\\')",
                );
                sql_params.push(Value::Text(token.clone()));
                let escaped = like_escape(token);
                for _ in 0..4 {
                    sql_params.push(Value::Text(escaped.clone()));
                }
            }
            sql_params.push(Value::Blob(workspace_id.as_bytes().to_vec()));
            sql_params.push(Value::Blob(project_id.as_bytes().to_vec()));
            sql_params.push(Value::Integer(cutoff));
            sql_params.push(Value::Blob(workspace_id.as_bytes().to_vec()));
            sql_params.push(Value::Blob(project_id.as_bytes().to_vec()));
            sql_params.push(Value::Integer(cutoff));
            sql_params.push(Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));

            let mut sql = String::with_capacity(placeholders.len() + 1_200);
            write!(
                &mut sql,
                // `freq` is how many current pages in this project carry
                // the entity; 1/freq is the inverse-frequency weight.
                "WITH matched AS ( \
                   SELECT e.id AS entity_id, e.name AS name \
                   FROM entities e \
                   WHERE ({placeholders}) \
                     AND e.workspace_id = ? AND e.project_id = ? \
                 ), \
                 freq AS ( \
                   SELECT m.entity_id, m.name, COUNT(*) AS pages \
                   FROM matched m \
                   JOIN entity_page_links l ON l.entity_id = m.entity_id \
                   JOIN pages p ON p.id = l.page_id AND p.is_latest = 1 \
                   WHERE 1 = 1{freq_not_expired} \
                   GROUP BY m.entity_id, m.name \
                 ) \
                 SELECT pg.id, pg.path, pg.title, substr(pg.body, 1, 240) AS snippet, \
                        SUM(1.0 / f.pages) AS weight, \
                        COUNT(*) AS matches, \
                        json_group_array(f.name) AS names \
                 FROM freq f \
                 JOIN entity_page_links l ON l.entity_id = f.entity_id \
                 JOIN pages pg ON pg.id = l.page_id \
                 WHERE pg.workspace_id = ? AND pg.project_id = ? AND pg.is_latest = 1{not_expired} \
                 GROUP BY pg.id, pg.path, pg.title \
                 ORDER BY weight DESC, matches DESC, pg.path ASC \
                 LIMIT ?",
                not_expired = not_expired("pg", "?"),
                freq_not_expired = not_expired("p", "?"),
            )
            .expect("writing SQL into String cannot fail");

            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(sql_params.iter()), |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let path: String = row.get(1)?;
                let title: String = row.get(2)?;
                let snippet: String = row.get(3)?;
                let weight: f64 = row.get(4)?;
                let names_json: String = row.get(6)?;
                Ok((id_bytes, path, title, snippet, weight, names_json))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id_bytes, path, title, snippet, weight, names_json) = row?;
                let mut matched: Vec<String> = serde_json::from_str(&names_json)?;
                matched.sort_unstable();
                matched.dedup();
                out.push(EntityHit {
                    hit: PageHit {
                        id: PageId::from_slice(&id_bytes)?,
                        path: PagePath::new(path)?,
                        title,
                        snippet,
                        rank: 0.0,
                    },
                    weight,
                    matched,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Return `stale` / `wrong` feedback still attached to a *current*
    /// page version in one project, grouped by (page, kind) — the input
    /// for the lint pass's `feedback_flagged` findings. Rewriting a
    /// flagged page supersedes the version the feedback points at, so
    /// the finding drops out here without any explicit dismissal.
    /// Most-recent signal first.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn open_feedback_findings(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<Vec<FeedbackFinding>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT pg.path, f.kind, COUNT(*) AS signal_count, MAX(f.created_at) AS latest, \
                        (SELECT reason FROM page_feedback r \
                          WHERE r.page_id = f.page_id AND r.kind = f.kind \
                            AND r.reason IS NOT NULL \
                          ORDER BY r.created_at DESC, r.id DESC LIMIT 1) AS reason \
                 FROM page_feedback f \
                 JOIN pages pg ON pg.id = f.page_id AND pg.is_latest = 1 \
                 WHERE f.workspace_id = ?1 AND f.project_id = ?2 \
                   AND f.kind IN ('stale', 'wrong') \
                 GROUP BY f.page_id, pg.path, f.kind \
                 ORDER BY latest DESC, pg.path ASC, f.kind ASC",
            )?;
            let rows = stmt.query_map(
                params![workspace_id.as_bytes(), project_id.as_bytes()],
                |row| {
                    let path: String = row.get(0)?;
                    let kind: String = row.get(1)?;
                    let signal_count: i64 = row.get(2)?;
                    let latest_us: i64 = row.get(3)?;
                    let reason: Option<String> = row.get(4)?;
                    Ok((path, kind, signal_count, latest_us, reason))
                },
            )?;
            let mut out = Vec::new();
            for row in rows {
                let (path, kind, signal_count, latest_us, reason) = row?;
                out.push(FeedbackFinding {
                    path,
                    kind,
                    signal_count: u32::try_from(signal_count.max(0)).unwrap_or(u32::MAX),
                    latest_at: jiff::Timestamp::from_microsecond(latest_us)
                        .map(|ts| ts.to_string())
                        .unwrap_or_default(),
                    reason,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Return pages linked to or from the seed pages, scoped to latest pages
    /// in the same project.
    ///
    /// `expiry_cutoff_us`: see [`Self::search_pages_for_project`].
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn graph_neighbors_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        seed_ids: Vec<PageId>,
        limit: usize,
        expiry_cutoff_us: Option<i64>,
    ) -> StoreResult<Vec<PageHit>> {
        Ok(self
            .graph_neighbors_for_project_explained(
                workspace_id,
                project_id,
                seed_ids,
                limit,
                expiry_cutoff_us,
            )
            .await?
            .into_iter()
            .map(|n| n.hit)
            .collect())
    }

    /// [`Self::graph_neighbors_for_project`] plus provenance: which seed
    /// produced each neighbour (`seed_ord` indexes into the caller's
    /// `seed_ids`) and in which direction the link points. Feeds the
    /// `memory_query` explain surface.
    async fn graph_neighbors_for_project_explained(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        seed_ids: Vec<PageId>,
        limit: usize,
        expiry_cutoff_us: Option<i64>,
    ) -> StoreResult<Vec<GraphNeighbor>> {
        if seed_ids.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let cutoff = expiry_cutoff_us.unwrap_or_else(now_us);
        self.with_conn(move |conn| {
            let mut seen = std::collections::HashSet::new();
            let mut out = Vec::new();

            let mut values_clause = String::with_capacity(seed_ids.len() * 8);
            let mut sql_params = Vec::with_capacity(seed_ids.len() * 2 + 6);
            for (idx, seed_id) in seed_ids.iter().enumerate() {
                if idx > 0 {
                    values_clause.push_str(", ");
                }
                values_clause.push_str("(?, ?)");
                sql_params.push(Value::Blob(seed_id.as_bytes().to_vec()));
                sql_params.push(Value::Integer(idx as i64));
            }
            let now = cutoff;
            sql_params.push(Value::Blob(workspace_id.as_bytes().to_vec()));
            sql_params.push(Value::Blob(project_id.as_bytes().to_vec()));
            sql_params.push(Value::Integer(now));
            sql_params.push(Value::Blob(workspace_id.as_bytes().to_vec()));
            sql_params.push(Value::Blob(project_id.as_bytes().to_vec()));
            sql_params.push(Value::Integer(now));

            let out_not_expired = not_expired("tp", "?");
            let in_not_expired = not_expired("fp", "?");
            let mut sql = String::with_capacity(values_clause.len() + 1_500);
            write!(
                &mut sql,
                "WITH seeds(seed_id, seed_ord) AS (VALUES {values_clause}), \
                 neighbors AS ( \
                   SELECT tp.id AS id, tp.path AS path, tp.title AS title, \
                          substr(tp.body, 1, 240) AS snippet, \
                          seeds.seed_ord * 2 AS stream_ord, tp.updated_at AS updated_at \
                   FROM seeds \
                   JOIN links l ON l.from_page_id = seeds.seed_id \
                   JOIN pages tp ON tp.id = l.to_page_id \
                   WHERE tp.workspace_id = ? AND tp.project_id = ? AND tp.is_latest = 1{out_not_expired} \
                   UNION ALL \
                   SELECT fp.id AS id, fp.path AS path, fp.title AS title, \
                          substr(fp.body, 1, 240) AS snippet, \
                          seeds.seed_ord * 2 + 1 AS stream_ord, fp.updated_at AS updated_at \
                   FROM seeds \
                   JOIN links l ON l.to_page_id = seeds.seed_id \
                   JOIN pages fp ON fp.id = l.from_page_id \
                   WHERE fp.workspace_id = ? AND fp.project_id = ? AND fp.is_latest = 1{in_not_expired} \
                 ) \
                 SELECT id, path, title, snippet, stream_ord \
                 FROM neighbors \
                 WHERE NOT EXISTS (SELECT 1 FROM seeds s WHERE s.seed_id = neighbors.id) \
                 ORDER BY stream_ord ASC, updated_at DESC, path ASC"
            )
            .expect("writing SQL into String cannot fail");

            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(sql_params.iter()), |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let path: String = row.get(1)?;
                let title: String = row.get(2)?;
                let snippet: String = row.get(3)?;
                let stream_ord: i64 = row.get(4)?;
                Ok((id_bytes, path, title, snippet, stream_ord))
            })?;

            for row in rows {
                let (id_bytes, path, title, snippet, stream_ord) = row?;
                let id = PageId::from_slice(&id_bytes)?;
                if !seen.insert(id) {
                    continue;
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let seed_ord = (stream_ord / 2).max(0) as usize;
                out.push(GraphNeighbor {
                    hit: PageHit {
                        id,
                        path: PagePath::new(path)?,
                        title,
                        snippet,
                        rank: 0.0,
                    },
                    seed_ord,
                    incoming: stream_ord % 2 == 1,
                });
                if out.len() >= limit {
                    break;
                }
            }
            Ok(out)
        })
        .await
    }

    async fn page_authorities_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        page_ids: Vec<PageId>,
    ) -> StoreResult<std::collections::HashMap<PageId, PageAuthority>> {
        if page_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        self.with_conn(move |conn| {
            let mut values_clause = String::with_capacity(page_ids.len() * 5);
            let mut sql_params = Vec::with_capacity(page_ids.len() + 2);
            for (idx, page_id) in page_ids.iter().enumerate() {
                if idx > 0 {
                    values_clause.push_str(", ");
                }
                values_clause.push_str("(?)");
                sql_params.push(Value::Blob(page_id.as_bytes().to_vec()));
            }
            sql_params.push(Value::Blob(workspace_id.as_bytes().to_vec()));
            sql_params.push(Value::Blob(project_id.as_bytes().to_vec()));

            let kind_expr = page_kind_expr("pages.path", "pages.frontmatter_json");
            let sql = format!(
                "WITH requested(id) AS (VALUES {values_clause}) \
                 SELECT pages.id, pages.path, pages.tier, pages.pinned, \
                        pages.frontmatter_json, {kind_expr} AS kind \
                 FROM requested \
                 JOIN pages ON pages.id = requested.id \
                 WHERE pages.workspace_id = ? \
                   AND pages.project_id = ? \
                   AND pages.is_latest = 1"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(sql_params.iter()), |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let path: String = row.get(1)?;
                let tier: String = row.get(2)?;
                let pinned = row.get::<_, i64>(3)? != 0;
                let frontmatter_json: String = row.get(4)?;
                let kind: String = row.get(5)?;
                Ok((id_bytes, path, tier, pinned, frontmatter_json, kind))
            })?;

            let mut authorities = std::collections::HashMap::new();
            for row in rows {
                let (id_bytes, path, tier, pinned, frontmatter_json, kind) = row?;
                authorities.insert(
                    PageId::from_slice(&id_bytes)?,
                    PageAuthority::from_stored(&path, &kind, &tier, pinned, &frontmatter_json),
                );
            }
            Ok(authorities)
        })
        .await
    }

    /// Hybrid search: RRF-fuse FTS5 results with cosine-similarity
    /// over the stored embeddings of the matching `(provider, model,
    /// dim)`, entity matches, and link-neighbour expansion — four RRF
    /// streams. Applies the bounded page-authority adjustment after
    /// fusion and returns the top-`limit` pages by adjusted fused score.
    ///
    /// Each stream degrades independently to contributing nothing: no
    /// `query_vec` skips the vector stream, an empty `entities` table
    /// skips the entity stream, and graph expansion still runs from
    /// whatever seeds the other streams produced.
    ///
    /// `expiry_cutoff_us`: see [`Self::search_pages_for_project`]. The
    /// cutoff resolves once here so all streams agree on it.
    ///
    /// k=60 is the canonical RRF constant.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    #[allow(clippy::too_many_arguments)]
    pub async fn hybrid_search(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query: String,
        query_vec: Option<Vec<f32>>,
        provider: String,
        model: String,
        dim: u32,
        limit: usize,
        expiry_cutoff_us: Option<i64>,
    ) -> StoreResult<Vec<PageHit>> {
        Ok(self
            .hybrid_search_inner(
                workspace_id,
                project_id,
                query,
                query_vec,
                provider,
                model,
                dim,
                limit,
                expiry_cutoff_us,
                false,
            )
            .await?
            .into_iter()
            .map(|(hit, _)| hit)
            .collect())
    }

    /// [`Self::hybrid_search`] plus a [`SearchExplain`] per hit — the
    /// per-stream ranks, raw scores, and RRF contributions the fusion
    /// otherwise discards. Callers that don't need the bounded provenance
    /// bookkeeping use `hybrid_search`.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    #[allow(clippy::too_many_arguments)]
    pub async fn hybrid_search_explained(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query: String,
        query_vec: Option<Vec<f32>>,
        provider: String,
        model: String,
        dim: u32,
        limit: usize,
        expiry_cutoff_us: Option<i64>,
    ) -> StoreResult<Vec<(PageHit, SearchExplain)>> {
        Ok(self
            .hybrid_search_inner(
                workspace_id,
                project_id,
                query,
                query_vec,
                provider,
                model,
                dim,
                limit,
                expiry_cutoff_us,
                true,
            )
            .await?
            .into_iter()
            .map(|(hit, explain)| (hit, explain.unwrap_or_default()))
            .collect())
    }

    #[allow(clippy::too_many_arguments)]
    async fn hybrid_search_inner(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query: String,
        query_vec: Option<Vec<f32>>,
        provider: String,
        model: String,
        dim: u32,
        limit: usize,
        expiry_cutoff_us: Option<i64>,
        explain: bool,
    ) -> StoreResult<Vec<(PageHit, Option<SearchExplain>)>> {
        let cutoff = expiry_cutoff_us.unwrap_or_else(now_us);
        // Authority is applied after RRF, so every stream needs the same
        // bounded candidate window used by FTS-only search. A `limit * 2`
        // window was too narrow at small limits for a canonical page to enter
        // the fused pool and receive its bounded promotion.
        let candidate_limit = authority_candidate_limit(limit);
        // Tokenize the borrowed query before the FTS candidate call consumes
        // it. This avoids cloning a caller-controlled query on the hot path.
        let entity_hits = self
            .entity_hits_for_project(
                workspace_id,
                project_id,
                &query,
                candidate_limit,
                Some(cutoff),
            )
            .await?;
        let fts_candidates = self
            .search_page_candidates_for_project(
                workspace_id,
                project_id,
                query,
                candidate_limit,
                Some(cutoff),
            )
            .await?;
        let mut authorities: std::collections::HashMap<PageId, PageAuthority> = fts_candidates
            .iter()
            .map(|(hit, authority)| (hit.id, *authority))
            .collect();
        let fts_hits: Vec<PageHit> = fts_candidates
            .into_iter()
            .map(|(hit, _authority)| hit)
            .collect();
        let mut vec_hits: Vec<(PageId, PagePath, f32)> = Vec::new();
        if let Some(qv) = query_vec {
            vec_hits = self
                .top_embedding_hits_for_project(
                    workspace_id,
                    project_id,
                    qv,
                    provider,
                    model,
                    dim,
                    candidate_limit,
                    cutoff,
                )
                .await?;
        }

        let mut seed_seen = std::collections::HashSet::new();
        let mut seed_ids = Vec::new();
        let mut seed_paths = explain.then(Vec::new);
        for hit in &fts_hits {
            if seed_seen.insert(hit.id) {
                seed_ids.push(hit.id);
                if let Some(paths) = &mut seed_paths {
                    paths.push(hit.path.as_str().to_string());
                }
            }
        }
        for (id, path, _) in &vec_hits {
            if seed_seen.insert(*id) {
                seed_ids.push(*id);
                if let Some(paths) = &mut seed_paths {
                    paths.push(path.as_str().to_string());
                }
            }
        }
        for e in &entity_hits {
            if seed_seen.insert(e.hit.id) {
                seed_ids.push(e.hit.id);
                if let Some(paths) = &mut seed_paths {
                    paths.push(e.hit.path.as_str().to_string());
                }
            }
        }
        let graph_hits = self
            .graph_neighbors_for_project_explained(
                workspace_id,
                project_id,
                seed_ids,
                candidate_limit,
                Some(cutoff),
            )
            .await?;

        // RRF fuse: score(d) = Σ 1/(k + rank_i(d)) over rankers.
        let k = 60.0_f64;
        struct Fused {
            path: PagePath,
            title: String,
            snippet: String,
            score: f64,
            explain: Option<SearchExplain>,
        }
        let mut fused: std::collections::HashMap<PageId, Fused> = std::collections::HashMap::new();

        for (rank, h) in fts_hits.iter().enumerate() {
            let contrib = 1.0 / (k + (rank + 1) as f64);
            let entry = fused.entry(h.id).or_insert_with(|| Fused {
                path: h.path.clone(),
                title: h.title.clone(),
                snippet: h.snippet.clone(),
                score: 0.0,
                explain: explain.then(SearchExplain::default),
            });
            entry.score += contrib;
            if let Some(details) = &mut entry.explain {
                details.fts_rank = Some(rank + 1);
                details.fts_score = Some(h.rank);
                details.rrf.fts = contrib;
                details.fused += contrib;
            }
        }
        for (rank, (id, path, cosine)) in vec_hits.iter().enumerate() {
            let contrib = 1.0 / (k + (rank + 1) as f64);
            let entry = fused.entry(*id).or_insert_with(|| Fused {
                path: path.clone(),
                title: String::new(),
                snippet: String::new(),
                score: 0.0,
                explain: explain.then(SearchExplain::default),
            });
            entry.score += contrib;
            if let Some(details) = &mut entry.explain {
                details.vector_rank = Some(rank + 1);
                details.cosine = Some(*cosine);
                details.rrf.vector = contrib;
                details.fused += contrib;
            }
        }
        for (rank, e) in entity_hits.iter().enumerate() {
            let contrib = 1.0 / (k + (rank + 1) as f64);
            let entry = fused.entry(e.hit.id).or_insert_with(|| Fused {
                path: e.hit.path.clone(),
                title: e.hit.title.clone(),
                snippet: e.hit.snippet.clone(),
                score: 0.0,
                explain: explain.then(SearchExplain::default),
            });
            entry.score += contrib;
            if let Some(details) = &mut entry.explain {
                details.entity_rank = Some(rank + 1);
                details.entity_weight = Some(e.weight);
                details.matched_entities = e.matched.clone();
                details.rrf.entity = contrib;
                details.fused += contrib;
            }
        }
        for (rank, n) in graph_hits.iter().enumerate() {
            let contrib = 1.0 / (k + (rank + 1) as f64);
            let entry = fused.entry(n.hit.id).or_insert_with(|| Fused {
                path: n.hit.path.clone(),
                title: n.hit.title.clone(),
                snippet: n.hit.snippet.clone(),
                score: 0.0,
                explain: explain.then(SearchExplain::default),
            });
            entry.score += contrib;
            if let Some(details) = &mut entry.explain {
                details.graph_rank = Some(rank + 1);
                details.graph_via = Some(GraphVia {
                    seed_path: seed_paths
                        .as_ref()
                        .and_then(|paths| paths.get(n.seed_ord))
                        .cloned()
                        .unwrap_or_default(),
                    direction: if n.incoming { "incoming" } else { "outgoing" },
                });
                details.rrf.graph = contrib;
                details.fused += contrib;
            }
        }

        let mut out: Vec<(PageHit, Option<SearchExplain>)> = fused
            .into_iter()
            .map(|(id, entry)| {
                (
                    PageHit {
                        id,
                        path: entry.path,
                        title: entry.title,
                        snippet: entry.snippet,
                        rank: -entry.score, // lower = better (matches FTS5 convention)
                    },
                    entry.explain,
                )
            })
            .collect();
        let missing_authorities: Vec<PageId> = out
            .iter()
            .filter_map(|(hit, _)| (!authorities.contains_key(&hit.id)).then_some(hit.id))
            .collect();
        authorities.extend(
            self.page_authorities_for_project(workspace_id, project_id, missing_authorities)
                .await?,
        );
        for (hit, explain) in &mut out {
            if let Some(authority) = authorities.get(&hit.id) {
                hit.rank = authority.adjust_rank(hit.rank);
                // `fused` stays the raw RRF sum; without the multiplier
                // beside it the explain could not account for the rank it
                // returns, which is the whole point of the surface.
                if let Some(details) = explain {
                    details.authority = Some(authority.factor);
                }
            }
        }
        out.sort_by(|a, b| {
            a.0.rank
                .partial_cmp(&b.0.rank)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Deterministic tiebreak: equal fused scores (common when
                // two streams each contribute a rank-1 hit) previously fell
                // back to HashMap iteration order.
                .then_with(|| a.0.path.as_str().cmp(b.0.path.as_str()))
        });
        out.truncate(limit);
        Ok(out)
    }

    /// Return the open handoff the next session should pick up.
    ///
    /// A manual handoff (`memory_handoff_begin`, which always sets
    /// `from_session_id = None`) is project-wide and always a candidate,
    /// whatever cwd it carries. An auto SessionEnd handoff
    /// (`from_session_id = Some`) is a candidate only when its cwd is an
    /// ancestor-or-equal of `cwd_filter` (path-boundary, the prior art's
    /// check rather than exact equality: a handoff left in `/repo` reaches a
    /// session in `/repo/api`, but never `/repo-other`). Among candidates a
    /// manual handoff wins over an auto one, then the most recent, with cwd
    /// specificity only breaking timestamp ties — so an explicit "where we left off" baton
    /// deterministically beats the heuristic SessionEnd handoff, regardless of
    /// whether the model passed a cwd. With `cwd_filter == None` every open
    /// handoff is a candidate (project-wide read, e.g. the web overview). The
    /// path-boundary is computed in Rust rather than SQL `LIKE` so `%`/`_` in a
    /// path can never act as a wildcard.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn latest_open_handoff(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        cwd_filter: Option<String>,
        owner_filter: OwnerFilter,
    ) -> StoreResult<Option<Handoff>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, workspace_id, project_id, from_session_id, from_agent, to_agent, \
                        cwd, summary, open_questions, next_steps, files_touched, state, \
                        created_at, accepted_by, accepted_at, accepted_by_session, \
                        owner_user, accepted_by_user \
                 FROM handoffs \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND state = 'open' \
                 ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map(
                params![workspace_id.as_bytes(), project_id.as_bytes()],
                row_to_handoff,
            )?;
            let mut selected: Option<Handoff> = None;
            for r in rows {
                let handoff = r??;
                if is_handoff_candidate(&handoff, cwd_filter.as_deref(), &owner_filter)
                    && selected
                        .as_ref()
                        .is_none_or(|current| prefer_handoff(&handoff, current).is_gt())
                {
                    selected = Some(handoff);
                }
            }
            Ok(selected)
        })
        .await
    }

    /// List a project's handoffs, newest first.
    ///
    /// The system had no handoff listing at all: every reader fetched "the
    /// single open one" and consumed it. That made a mis-delivered or
    /// prematurely consumed baton unrecoverable and invisible — there was no
    /// way to ask what happened to it. `state = None` returns every state so an
    /// operator can find an already-accepted handoff and read it back.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn list_handoffs(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        state: Option<HandoffState>,
        owner_filter: OwnerFilter,
        limit: usize,
    ) -> StoreResult<Vec<Handoff>> {
        let state = state.map(|s| s.as_str().to_string());
        self.with_conn(move |conn| {
            let state_clause = if state.is_some() {
                " AND state = ?3"
            } else {
                ""
            };
            // Push the owner predicate and the limit into SQL rather than
            // filtering a fully materialised result set in Rust: without them
            // this reads (and sorts) every handoff the project has ever
            // accumulated, and the caller's limit provides no protection
            // because it is applied after the rows are already loaded.
            // The owner name takes the first index the state filter left free, so
            // the two optional predicates cannot claim the same slot.
            let owner_index = if state.is_some() { 4 } else { 3 };
            let (owner_clause, owner_param) = handoff_owner_sql(&owner_filter, owner_index);
            // Bounded even when the caller asks for a large page.
            let sql_limit = limit.clamp(1, 500);
            let sql = format!(
                "SELECT id, workspace_id, project_id, from_session_id, from_agent, to_agent, \
                        cwd, summary, open_questions, next_steps, files_touched, state, \
                        created_at, accepted_by, accepted_at, accepted_by_session, \
                        owner_user, accepted_by_user \
                 FROM handoffs \
                 WHERE workspace_id = ?1 AND project_id = ?2{state_clause}{owner_clause} \
                 ORDER BY created_at DESC \
                 LIMIT {sql_limit}"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut binds: Vec<&dyn rusqlite::ToSql> =
                vec![workspace_id.as_bytes(), project_id.as_bytes()];
            if let Some(state) = state.as_ref() {
                binds.push(state);
            }
            if let Some(owner) = owner_param.as_ref() {
                binds.push(owner);
            }
            let rows = stmt.query_map(binds.as_slice(), row_to_handoff)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row??);
            }
            Ok(out)
        })
        .await
    }

    /// Look up a handoff by id.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn handoff_by_id(&self, handoff_id: HandoffId) -> StoreResult<Option<Handoff>> {
        self.with_conn(move |conn| {
            let row = conn
                .query_row(
                    "SELECT id, workspace_id, project_id, from_session_id, from_agent, to_agent, \
                            cwd, summary, open_questions, next_steps, files_touched, state, \
                            created_at, accepted_by, accepted_at, accepted_by_session, \
                            owner_user, accepted_by_user \
                     FROM handoffs WHERE id = ?1",
                    params![handoff_id.as_bytes()],
                    row_to_handoff,
                )
                .optional()?;
            row.transpose()
        })
        .await
    }

    /// Snapshot the database to `dest_path` using SQLite's online backup
    /// API. The source DB stays writable for the duration of the copy.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn snapshot_to(&self, dest_path: PathBuf) -> StoreResult<()> {
        self.with_conn(move |conn| {
            conn.backup(rusqlite::DatabaseName::Main, &dest_path, None)
                .map_err(StoreError::from)
        })
        .await
    }

    /// Assemble a [`BriefingSnapshot`] — pure SQL aggregation across
    /// the `pages` / `sessions` / `observations` / `handoffs` tables.
    /// No LLM, no schema reads outside what's already there.
    ///
    /// `recent_pages_limit` caps the `recent_pages` array; pass a
    /// small number (5-20) — this is meant to be skimmed, not paged
    /// through.
    ///
    /// `slot_visibility` decides which `_slots/…` pages the snapshot lists;
    /// the default rule lists every one of them, as it always did.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    #[allow(clippy::too_many_lines)]
    pub async fn briefing(
        &self,
        recent_pages_limit: usize,
        owner_filter: OwnerFilter,
        slot_visibility: &ai_memory_core::SlotVisibility,
    ) -> StoreResult<BriefingSnapshot> {
        let recent_limit = recent_pages_limit.clamp(1, 100) as i64;
        // The slot GLOB is the only OPTIONAL bind in these statements, so it
        // goes LAST in every one of them: a fixed parameter placed after it
        // would shift by one whenever the visibility rule needs no pattern.
        let (slot_sql, slot_glob) = slot_visibility_sql(slot_visibility, 2);
        let (recent_slot_sql, recent_glob) = slot_exclusion_sql(slot_visibility, 3);
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("path", "frontmatter_json");
            let now_us = jiff::Timestamp::now().as_microsecond();
            let day_us: i64 = 86_400 * 1_000_000;
            let cutoff_7d = now_us - 7 * day_us;
            let cutoff_30d = now_us - 30 * day_us;

            let counts = StatusCounts {
                pages_latest: count(conn, "SELECT COUNT(*) FROM pages WHERE is_latest = 1")?,
                pages_all: count(conn, "SELECT COUNT(*) FROM pages")?,
                sessions: count(conn, "SELECT COUNT(*) FROM sessions")?,
                observations: count(conn, "SELECT COUNT(*) FROM observations")?,
            };

            let activity_7d = window_activity(conn, 7, cutoff_7d)?;
            let activity_30d = window_activity(conn, 30, cutoff_30d)?;

            let last_observation_at: Option<i64> = conn
                .query_row("SELECT MAX(created_at) FROM observations", [], |row| {
                    row.get::<_, Option<i64>>(0)
                })
                .optional()?
                .flatten();
            let last_observation_at = last_observation_at
                .and_then(|us| jiff::Timestamp::from_microsecond(us).ok())
                .map(|ts| ts.to_string());

            let (owner_clause, owner_param) = handoff_owner_sql(&owner_filter, 1);
            let mut owner_binds: Vec<&dyn rusqlite::ToSql> = Vec::new();
            if let Some(owner) = owner_param.as_ref() {
                owner_binds.push(owner);
            }
            let pending_handoff_count: u64 = count_bound(
                conn,
                &format!("SELECT COUNT(*) FROM handoffs WHERE state = 'open'{owner_clause}"),
                &owner_binds,
            )?;

            // Rules: any `is_latest = 1` page under `_rules/`.
            // Routed there automatically by the consolidator when
            // `kind = "rule"` — see consolidator.rs::slugify_for_rule.
            let mut rules_stmt = conn.prepare_cached(&format!(
                "SELECT path, title, {kind_expr} AS kind, \
                        updated_at \
                 FROM pages \
                  WHERE is_latest = 1 AND path GLOB '_rules/*'{not_expired} \
                  ORDER BY updated_at DESC",
                not_expired = not_expired("pages", "?1"),
            ))?;
            let rules: Vec<BriefingPage> = rules_stmt
                .query_map(params![now_us], briefing_page_from_row)?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            let mut slots_stmt = conn.prepare_cached(&format!(
                "SELECT path, title, {kind_expr} AS kind, \
                        updated_at \
                 FROM pages \
                  WHERE is_latest = 1 AND ({slot_sql}){not_expired} \
                  ORDER BY path ASC",
                not_expired = not_expired("pages", "?1"),
            ))?;
            // A slot must clear BOTH gates: unexpired, and visible to this
            // operator. The two rules are independent — an expired slot of
            // your own is still gone, a live slot of somebody else's is still
            // not yours.
            let slot_rows = match slot_glob.as_deref() {
                Some(glob) => {
                    slots_stmt.query_map(params![now_us, glob], briefing_page_from_row)?
                }
                None => slots_stmt.query_map(params![now_us], briefing_page_from_row)?,
            };
            let slots: Vec<BriefingPage> = slot_rows
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            let mut recent_stmt = conn.prepare_cached(&format!(
                "SELECT path, title, {kind_expr} AS kind, \
                        updated_at \
                 FROM pages \
                 WHERE is_latest = 1 AND ({recent_slot_sql}){not_expired} \
                 ORDER BY updated_at DESC \
                 LIMIT ?1",
                not_expired = not_expired("pages", "?2"),
            ))?;
            let recent_rows = match recent_glob.as_deref() {
                Some(glob) => recent_stmt
                    .query_map(params![recent_limit, now_us, glob], briefing_page_from_row)?,
                None => {
                    recent_stmt.query_map(params![recent_limit, now_us], briefing_page_from_row)?
                }
            };
            let recent_pages: Vec<BriefingPage> = recent_rows
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            Ok(BriefingSnapshot {
                counts,
                activity_7d,
                activity_30d,
                last_observation_at,
                pending_handoff_count,
                rules,
                slots,
                recent_pages,
                cross_project_dependents: 0,
                cross_project_dependencies: 0,
            })
        })
        .await
    }

    /// Assemble a project-scoped [`BriefingSnapshot`].
    ///
    /// `slot_visibility` decides which `_slots/…` pages the snapshot lists.
    /// Callers that feed a per-operator surface — a session brief, an LLM
    /// prompt — must pass the rule for that operator; the default lists every
    /// slot, as it always did.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    #[allow(clippy::too_many_lines)]
    pub async fn briefing_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        recent_pages_limit: usize,
        owner_filter: OwnerFilter,
        slot_visibility: &ai_memory_core::SlotVisibility,
    ) -> StoreResult<BriefingSnapshot> {
        let recent_limit = recent_pages_limit.clamp(1, 100) as i64;
        // Glob last, after the expiry cutoff: see `briefing`.
        let (slot_sql, slot_glob) = slot_visibility_sql(slot_visibility, 4);
        let (recent_slot_sql, recent_glob) = slot_exclusion_sql(slot_visibility, 5);
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("path", "frontmatter_json");
            let now_us = jiff::Timestamp::now().as_microsecond();
            let day_us: i64 = 86_400 * 1_000_000;
            let cutoff_7d = now_us - 7 * day_us;
            let cutoff_30d = now_us - 30 * day_us;

            let counts = StatusCounts {
                pages_latest: count_project(
                    conn,
                    "SELECT COUNT(*) FROM pages WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1",
                    workspace_id,
                    project_id,
                )?,
                pages_all: count_project(
                    conn,
                    "SELECT COUNT(*) FROM pages WHERE workspace_id = ?1 AND project_id = ?2",
                    workspace_id,
                    project_id,
                )?,
                sessions: count_project(
                    conn,
                    "SELECT COUNT(*) FROM sessions WHERE workspace_id = ?1 AND project_id = ?2",
                    workspace_id,
                    project_id,
                )?,
                observations: count_project(
                    conn,
                    "SELECT COUNT(*) FROM observations WHERE workspace_id = ?1 AND project_id = ?2",
                    workspace_id,
                    project_id,
                )?,
            };

            let activity_7d = window_activity_project(conn, 7, cutoff_7d, workspace_id, project_id)?;
            let activity_30d = window_activity_project(conn, 30, cutoff_30d, workspace_id, project_id)?;

            let last_observation_at: Option<i64> = conn
                .query_row(
                    "SELECT MAX(created_at) FROM observations WHERE workspace_id = ?1 AND project_id = ?2",
                    params![workspace_id.as_bytes(), project_id.as_bytes()],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .optional()?
                .flatten();
            let last_observation_at = last_observation_at
                .and_then(|us| jiff::Timestamp::from_microsecond(us).ok())
                .map(|ts| ts.to_string());

            let (owner_clause, owner_param) = handoff_owner_sql(&owner_filter, 3);
            let mut owner_binds: Vec<&dyn rusqlite::ToSql> =
                vec![workspace_id.as_bytes(), project_id.as_bytes()];
            if let Some(owner) = owner_param.as_ref() {
                owner_binds.push(owner);
            }
            let pending_handoff_count = count_bound(
                conn,
                &format!(
                    "SELECT COUNT(*) FROM handoffs \
                     WHERE workspace_id = ?1 AND project_id = ?2 AND state = 'open'{owner_clause}"
                ),
                &owner_binds,
            )?;

            let mut rules_stmt = conn.prepare_cached(&format!(
                "SELECT path, title, {kind_expr} AS kind, \
                        updated_at \
                 FROM pages \
                  WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1 AND path GLOB '_rules/*'{not_expired} \
                  ORDER BY updated_at DESC",
                not_expired = not_expired("pages", "?3"),
            ))?;
            let rules: Vec<BriefingPage> = rules_stmt
                .query_map(params![workspace_id.as_bytes(), project_id.as_bytes(), now_us], briefing_page_from_row)?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            let mut slots_stmt = conn.prepare_cached(&format!(
                "SELECT path, title, {kind_expr} AS kind, \
                        updated_at \
                 FROM pages \
                  WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1 AND ({slot_sql}){not_expired} \
                  ORDER BY path ASC",
                not_expired = not_expired("pages", "?3"),
            ))?;
            // Unexpired AND visible to this operator — independent gates.
            let slot_rows = match slot_glob.as_deref() {
                Some(glob) => slots_stmt.query_map(
                    params![
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                        now_us,
                        glob
                    ],
                    briefing_page_from_row,
                )?,
                None => slots_stmt.query_map(
                    params![workspace_id.as_bytes(), project_id.as_bytes(), now_us],
                    briefing_page_from_row,
                )?,
            };
            let slots: Vec<BriefingPage> = slot_rows
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            let mut recent_stmt = conn.prepare_cached(&format!(
                "SELECT path, title, {kind_expr} AS kind, \
                        updated_at \
                 FROM pages \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1 \
                   AND ({recent_slot_sql}){not_expired} \
                 ORDER BY updated_at DESC \
                 LIMIT ?3",
                not_expired = not_expired("pages", "?4"),
            ))?;
            let recent_rows = match recent_glob.as_deref() {
                Some(glob) => recent_stmt.query_map(
                    params![
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                        recent_limit,
                        now_us,
                        glob
                    ],
                    briefing_page_from_row,
                )?,
                None => recent_stmt.query_map(
                    params![
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                        recent_limit,
                        now_us
                    ],
                    briefing_page_from_row,
                )?,
            };
            let recent_pages: Vec<BriefingPage> = recent_rows
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            let (cross_project_dependents, cross_project_dependencies) =
                cross_project_degree(conn, workspace_id, project_id)?;
            Ok(BriefingSnapshot {
                counts,
                activity_7d,
                activity_30d,
                last_observation_at,
                pending_handoff_count,
                rules,
                slots,
                recent_pages,
                cross_project_dependents,
                cross_project_dependencies,
            })
        })
        .await
    }

    /// Fetch the pages that make up a session-start project brief: the
    /// "core" pages WITH bodies (pinned, plus everything under `_rules/`
    /// and `_slots/` — the operator-curated, highest-signal memory), and
    /// the most-recently-updated page titles WITHOUT bodies (pointers the
    /// agent can follow up on via `memory_query`).
    ///
    /// Core pages are ordered pinned-first then by path, so a char-budget
    /// cut in the renderer drops the least-curated content last. Both
    /// limits are clamped defensively; the renderer applies the actual
    /// byte budget.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn session_brief_pages(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        core_pages_limit: usize,
        recent_pages_limit: usize,
        slots: ai_memory_core::SlotVisibility,
    ) -> StoreResult<(Vec<BriefPageBody>, Vec<BriefingPage>)> {
        let core_limit = core_pages_limit.clamp(1, 100) as i64;
        let recent_limit = recent_pages_limit.clamp(1, 100) as i64;
        // Slots namespaced under `_slots/<user>/` belong to that operator once
        // `[slots] per_user` is on; an unprefixed slot is shared and stays
        // visible to everyone. With the feature off nothing is filtered at all
        // — see `slot_visibility_sql`.
        //
        // The `pinned = 1` arm has to exclude slots explicitly: every write
        // path force-pins slot pages, so filtering only the `_slots/*` arm
        // would change nothing — the personal slots would come straight back
        // through `pinned`. The slot arm then puts back exactly what the
        // visibility rule admits, which for the default rule is every slot.
        //
        // Both statements bind the expiry cutoff at ?4 and put the optional
        // glob last, at ?5: the glob disappears from the SQL entirely when the
        // visibility rule needs no pattern, so a fixed parameter after it would
        // silently shift by one.
        let (slot_sql, mine) = slot_visibility_sql(&slots, 5);
        // The `recent` pointer list below is a separate statement with its own
        // parameter numbering, so its glob is ?5 there too.
        let (recent_slot_sql, recent_glob) = slot_exclusion_sql(&slots, 5);
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("path", "frontmatter_json");
            // The expiry guard sits OUTSIDE the arm group, so it also overrules
            // the `pinned = 1` arm: a pin means "don't decay this", not "keep
            // this past the date its author set".
            let core_sql = format!(
                "SELECT path, title, body, pinned, updated_at \
                     FROM pages \
                     WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1{not_expired} \
                       AND ( \
                             path GLOB '_rules/*' \
                          OR (pinned = 1 AND path NOT GLOB '_slots/*') \
                          OR ({slot_sql}) \
                       ) \
                     ORDER BY pinned DESC, path ASC \
                     LIMIT ?3",
                not_expired = not_expired("pages", "?4"),
            );
            let mut core_stmt = conn.prepare_cached(&core_sql)?;
            // The `?5` placeholder only exists when there is a viewer to match,
            // so the bindings branch with it.
            let core_row = |row: &rusqlite::Row<'_>| {
                let updated_us: i64 = row.get(4)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)? != 0,
                    updated_us,
                ))
            };
            let core_rows = match mine.as_deref() {
                Some(mine) => core_stmt.query_map(
                    params![
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                        core_limit,
                        now_us(),
                        mine
                    ],
                    core_row,
                )?,
                None => core_stmt.query_map(
                    params![
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                        core_limit,
                        now_us()
                    ],
                    core_row,
                )?,
            };
            let core: Vec<BriefPageBody> = core_rows
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(|(path, title, body, pinned, updated_us)| {
                    jiff::Timestamp::from_microsecond(updated_us)
                        .map(|ts| BriefPageBody {
                            path,
                            title,
                            body,
                            pinned,
                            updated_at: ts.to_string(),
                        })
                        .map_err(|e| {
                            StoreError::Memory(ai_memory_core::MemoryError::MalformedRecord(
                                format!("bad updated_at: {e}"),
                            ))
                        })
                })
                .collect::<StoreResult<Vec<_>>>()?;

            let mut recent_stmt = conn.prepare_cached(&format!(
                "SELECT path, title, {kind_expr} AS kind, \
                        updated_at \
                 FROM pages \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1 \
                   AND ({recent_slot_sql}){not_expired} \
                 ORDER BY updated_at DESC \
                 LIMIT ?3",
                not_expired = not_expired("pages", "?4"),
            ))?;
            let recent_rows = match recent_glob.as_deref() {
                Some(glob) => recent_stmt.query_map(
                    params![
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                        recent_limit,
                        now_us(),
                        glob
                    ],
                    briefing_page_from_row,
                )?,
                None => recent_stmt.query_map(
                    params![
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                        recent_limit,
                        now_us()
                    ],
                    briefing_page_from_row,
                )?,
            };
            let recent: Vec<BriefingPage> = recent_rows
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            Ok((core, recent))
        })
        .await
    }

    /// Return the latest open handoff for the workspace, aggregating
    /// across all of its projects (no project filter).
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn latest_open_handoff_for_workspace(
        &self,
        workspace_id: WorkspaceId,
        owner_filter: OwnerFilter,
    ) -> StoreResult<Option<Handoff>> {
        self.with_conn(move |conn| {
            // The owner predicate is pushed into SQL (rather than filtered after
            // the fact like the project-scoped lookup) because this query keeps
            // its `LIMIT 1`: filtering afterwards would return nothing whenever
            // the newest open handoff in the workspace happened to belong to
            // somebody else.
            let owner_clause = match &owner_filter {
                OwnerFilter::Any => "",
                OwnerFilter::User(_) => " AND (owner_user IS NULL OR owner_user = ?2)",
                OwnerFilter::Unattributed => " AND owner_user IS NULL",
            };
            let sql = format!(
                "SELECT id, workspace_id, project_id, from_session_id, from_agent, to_agent, \
                        cwd, summary, open_questions, next_steps, files_touched, state, \
                        created_at, accepted_by, accepted_at, accepted_by_session, \
                        owner_user, accepted_by_user \
                 FROM handoffs \
                 WHERE workspace_id = ?1 AND state = 'open'{owner_clause} \
                 ORDER BY created_at DESC LIMIT 1"
            );
            let row_opt = match &owner_filter {
                OwnerFilter::User(user) => conn
                    .query_row(&sql, params![workspace_id.as_bytes(), user], row_to_handoff)
                    .optional()?,
                _ => conn
                    .query_row(&sql, params![workspace_id.as_bytes()], row_to_handoff)
                    .optional()?,
            };
            row_opt.transpose()
        })
        .await
    }

    /// Look up a workspace name by id.
    ///
    /// Returns `None` when no matching workspace exists. Used by the
    /// admission webhook chain to populate `AdmissionContext.workspace`
    /// so external webhooks can address the page by human name without
    /// re-implementing UUID→name lookup against the engine's store.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn workspace_name_by_id(
        &self,
        workspace_id: WorkspaceId,
    ) -> StoreResult<Option<String>> {
        self.with_conn(move |conn| {
            let row_opt = conn
                .query_row(
                    "SELECT name FROM workspaces WHERE id = ?1",
                    params![workspace_id.as_bytes()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            Ok(row_opt)
        })
        .await
    }

    /// Look up a project name by id within a workspace.
    ///
    /// Returns `None` when no matching project exists.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn project_name_by_id(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<Option<String>> {
        self.with_conn(move |conn| {
            let row_opt = conn
                .query_row(
                    "SELECT name FROM projects WHERE id = ?1 AND workspace_id = ?2",
                    params![project_id.as_bytes(), workspace_id.as_bytes()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            Ok(row_opt)
        })
        .await
    }

    /// Build a [`BriefingSnapshot`] aggregated across all projects in a
    /// workspace.
    ///
    /// Mirrors [`ReaderPool::briefing_for_project`] but scopes every query
    /// to the workspace only (no `project_id` filter), including the same
    /// `slot_visibility` rule.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn briefing_for_workspace(
        &self,
        workspace_id: WorkspaceId,
        recent_pages_limit: usize,
        owner_filter: OwnerFilter,
        slot_visibility: &ai_memory_core::SlotVisibility,
    ) -> StoreResult<BriefingSnapshot> {
        let recent_limit = recent_pages_limit.clamp(1, 100) as i64;
        // Glob last, after the expiry cutoff: see `briefing`.
        let (slot_sql, slot_glob) = slot_visibility_sql(slot_visibility, 3);
        let (recent_slot_sql, recent_glob) = slot_exclusion_sql(slot_visibility, 4);
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("path", "frontmatter_json");
            let now_us = jiff::Timestamp::now().as_microsecond();
            let day_us: i64 = 86_400 * 1_000_000;
            let cutoff_7d = now_us - 7 * day_us;
            let cutoff_30d = now_us - 30 * day_us;

            let counts = StatusCounts {
                pages_latest: count_workspace(
                    conn,
                    "SELECT COUNT(*) FROM pages WHERE workspace_id = ?1 AND is_latest = 1",
                    workspace_id,
                )?,
                pages_all: count_workspace(
                    conn,
                    "SELECT COUNT(*) FROM pages WHERE workspace_id = ?1",
                    workspace_id,
                )?,
                sessions: count_workspace(
                    conn,
                    "SELECT COUNT(*) FROM sessions WHERE workspace_id = ?1",
                    workspace_id,
                )?,
                observations: count_workspace(
                    conn,
                    "SELECT COUNT(*) FROM observations WHERE workspace_id = ?1",
                    workspace_id,
                )?,
            };

            let activity_7d = window_activity_workspace(conn, 7, cutoff_7d, workspace_id)?;
            let activity_30d = window_activity_workspace(conn, 30, cutoff_30d, workspace_id)?;

            let last_observation_at: Option<i64> = conn
                .query_row(
                    "SELECT MAX(created_at) FROM observations WHERE workspace_id = ?1",
                    params![workspace_id.as_bytes()],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .optional()?
                .flatten();
            let last_observation_at = last_observation_at
                .and_then(|us| jiff::Timestamp::from_microsecond(us).ok())
                .map(|ts| ts.to_string());

            let (owner_clause, owner_param) = handoff_owner_sql(&owner_filter, 2);
            let mut owner_binds: Vec<&dyn rusqlite::ToSql> = vec![workspace_id.as_bytes()];
            if let Some(owner) = owner_param.as_ref() {
                owner_binds.push(owner);
            }
            let pending_handoff_count = count_bound(
                conn,
                &format!(
                    "SELECT COUNT(*) FROM handoffs \
                     WHERE workspace_id = ?1 AND state = 'open'{owner_clause}"
                ),
                &owner_binds,
            )?;

            let mut rules_stmt = conn.prepare_cached(&format!(
                "SELECT path, title, {kind_expr} AS kind, \
                        updated_at \
                 FROM pages \
                  WHERE workspace_id = ?1 AND is_latest = 1 AND path GLOB '_rules/*'{not_expired} \
                  ORDER BY updated_at DESC",
                not_expired = not_expired("pages", "?2"),
            ))?;
            let rules: Vec<BriefingPage> = rules_stmt
                .query_map(
                    params![workspace_id.as_bytes(), now_us],
                    briefing_page_from_row,
                )?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            let mut slots_stmt = conn.prepare_cached(&format!(
                "SELECT path, title, {kind_expr} AS kind, \
                        updated_at \
                 FROM pages \
                  WHERE workspace_id = ?1 AND is_latest = 1 AND ({slot_sql}){not_expired} \
                  ORDER BY path ASC",
                not_expired = not_expired("pages", "?2"),
            ))?;
            // Unexpired AND visible to this operator — independent gates.
            let slot_rows = match slot_glob.as_deref() {
                Some(glob) => slots_stmt.query_map(
                    params![workspace_id.as_bytes(), now_us, glob],
                    briefing_page_from_row,
                )?,
                None => slots_stmt.query_map(
                    params![workspace_id.as_bytes(), now_us],
                    briefing_page_from_row,
                )?,
            };
            let slots: Vec<BriefingPage> = slot_rows
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            let mut recent_stmt = conn.prepare_cached(&format!(
                "SELECT path, title, {kind_expr} AS kind, \
                        updated_at \
                 FROM pages \
                 WHERE workspace_id = ?1 AND is_latest = 1 AND ({recent_slot_sql}){not_expired} \
                 ORDER BY updated_at DESC \
                 LIMIT ?2",
                not_expired = not_expired("pages", "?3"),
            ))?;
            let recent_rows = match recent_glob.as_deref() {
                Some(glob) => recent_stmt.query_map(
                    params![workspace_id.as_bytes(), recent_limit, now_us, glob],
                    briefing_page_from_row,
                )?,
                None => recent_stmt.query_map(
                    params![workspace_id.as_bytes(), recent_limit, now_us],
                    briefing_page_from_row,
                )?,
            };
            let recent_pages: Vec<BriefingPage> = recent_rows
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            Ok(BriefingSnapshot {
                counts,
                activity_7d,
                activity_30d,
                last_observation_at,
                pending_handoff_count,
                rules,
                slots,
                recent_pages,
                cross_project_dependents: 0,
                cross_project_dependencies: 0,
            })
        })
        .await
    }

    /// Compute basic memory-health counters for a workspace.
    ///
    /// Returns `(stale, duplicates, orphans)`:
    /// - `stale`: latest episodic pages not updated within `STALE_DAYS` (30).
    /// - `duplicates`: extra latest pages that share a title.
    /// - `orphans`: latest pages with no inbound or outbound links.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn memory_health_for_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> StoreResult<(u64, u64, u64)> {
        self.memory_health_scoped(workspace_id, None).await
    }

    /// Per-project variant of [`ReaderPool::memory_health_for_workspace`]:
    /// the same stale / duplicate / orphan counters, confined to one project.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn memory_health_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<(u64, u64, u64)> {
        self.memory_health_scoped(workspace_id, Some(project_id))
            .await
    }

    async fn memory_health_scoped(
        &self,
        workspace_id: WorkspaceId,
        project_id: Option<ProjectId>,
    ) -> StoreResult<(u64, u64, u64)> {
        self.with_conn(move |conn| {
            let now_us = jiff::Timestamp::now().as_microsecond();
            let cutoff_30d = now_us - 30 * 86_400 * 1_000_000;
            let proj = project_id.map(|p| Value::Blob(p.as_bytes().to_vec()));
            let ws = || Value::Blob(workspace_id.as_bytes().to_vec());
            // Optional project filter, anonymous-`?` style. The orphan query
            // aliases pages as `p`, so it needs its own qualified clause.
            let clause = if proj.is_some() {
                " AND project_id = ?"
            } else {
                ""
            };
            let clause_p = if proj.is_some() {
                " AND p.project_id = ?"
            } else {
                ""
            };

            let stale: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM pages \
                         WHERE workspace_id = ? AND is_latest = 1 \
                           AND tier = 'episodic' AND updated_at < ?{clause}"
                    ),
                    params_from_iter(
                        [ws(), Value::Integer(cutoff_30d)]
                            .into_iter()
                            .chain(proj.clone()),
                    ),
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(0);

            let duplicates: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COALESCE(SUM(c - 1), 0) FROM ( \
                            SELECT COUNT(*) c FROM pages \
                            WHERE workspace_id = ? AND is_latest = 1{clause} \
                            GROUP BY title HAVING c > 1 \
                         )"
                    ),
                    params_from_iter(std::iter::once(ws()).chain(proj.clone())),
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(0);

            let orphans: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM pages p \
                         WHERE p.workspace_id = ? AND p.is_latest = 1{clause_p} \
                           AND NOT EXISTS (SELECT 1 FROM links l WHERE l.from_page_id = p.id) \
                           AND NOT EXISTS (SELECT 1 FROM links l WHERE l.to_page_id = p.id)"
                    ),
                    params_from_iter(std::iter::once(ws()).chain(proj.clone())),
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(0);

            Ok((
                u64::try_from(stale).unwrap_or(0),
                u64::try_from(duplicates).unwrap_or(0),
                u64::try_from(orphans).unwrap_or(0),
            ))
        })
        .await
    }

    /// Drill-down lists behind [`ReaderPool::memory_health_for_workspace`]'s
    /// counters: the actual stale / duplicate / orphan pages, each capped at
    /// `limit`. Definitions mirror the counters exactly so the lists explain
    /// the headline numbers.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn health_detail_for_workspace(
        &self,
        workspace_id: WorkspaceId,
        limit: usize,
    ) -> StoreResult<HealthDetail> {
        self.health_detail_scoped(workspace_id, None, limit).await
    }

    /// Per-project variant of [`ReaderPool::health_detail_for_workspace`].
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn health_detail_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        limit: usize,
    ) -> StoreResult<HealthDetail> {
        self.health_detail_scoped(workspace_id, Some(project_id), limit)
            .await
    }

    async fn health_detail_scoped(
        &self,
        workspace_id: WorkspaceId,
        project_id: Option<ProjectId>,
        limit: usize,
    ) -> StoreResult<HealthDetail> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.with_conn(move |conn| {
            let now_us = jiff::Timestamp::now().as_microsecond();
            let cutoff_30d = now_us - 30 * 86_400 * 1_000_000;
            let proj = project_id.map(|p| Value::Blob(p.as_bytes().to_vec()));
            let ws = || Value::Blob(workspace_id.as_bytes().to_vec());
            let clause = if proj.is_some() {
                " AND pg.project_id = ?"
            } else {
                ""
            };
            let inner_clause = if proj.is_some() {
                " AND project_id = ?"
            } else {
                ""
            };

            // Shared SELECT prefix: identity + path-inferred `kind`.
            let kind_expr = page_kind_expr("pg.path", "pg.frontmatter_json");
            let select = format!(
                "SELECT w.name, p.name, pg.path, pg.title, {kind_expr} \
                   FROM pages pg \
                   JOIN projects p ON p.id = pg.project_id \
                   JOIN workspaces w ON w.id = pg.workspace_id "
            );

            // Params follow placeholder order: ws, cutoff, [proj], limit.
            let stale_sql = format!(
                "{select} WHERE pg.workspace_id = ? AND pg.is_latest = 1 \
                   AND pg.tier = 'episodic' AND pg.updated_at < ?{clause} \
                 ORDER BY pg.updated_at ASC LIMIT ?"
            );
            let mut stale_stmt = conn.prepare(&stale_sql)?;
            let stale = stale_stmt
                .query_map(
                    params_from_iter(
                        [ws(), Value::Integer(cutoff_30d)]
                            .into_iter()
                            .chain(proj.clone())
                            .chain(std::iter::once(Value::Integer(limit))),
                    ),
                    health_page_from_row,
                )?
                .collect::<Result<Vec<_>, _>>()?;

            // Params: ws, [proj], ws(inner), [proj(inner)], limit.
            let dup_sql = format!(
                "{select} WHERE pg.workspace_id = ? AND pg.is_latest = 1{clause} \
                   AND pg.title IN ( \
                       SELECT title FROM pages \
                       WHERE workspace_id = ? AND is_latest = 1{inner_clause} \
                       GROUP BY title HAVING COUNT(*) > 1 \
                   ) \
                 ORDER BY pg.title, p.name LIMIT ?"
            );
            let mut dup_stmt = conn.prepare(&dup_sql)?;
            let duplicates = dup_stmt
                .query_map(
                    params_from_iter(
                        std::iter::once(ws())
                            .chain(proj.clone())
                            .chain(std::iter::once(ws()))
                            .chain(proj.clone())
                            .chain(std::iter::once(Value::Integer(limit))),
                    ),
                    health_page_from_row,
                )?
                .collect::<Result<Vec<_>, _>>()?;

            // Params: ws, [proj], limit.
            let orphan_sql = format!(
                "{select} WHERE pg.workspace_id = ? AND pg.is_latest = 1{clause} \
                   AND NOT EXISTS (SELECT 1 FROM links l WHERE l.from_page_id = pg.id) \
                   AND NOT EXISTS (SELECT 1 FROM links l WHERE l.to_page_id = pg.id) \
                 ORDER BY pg.updated_at DESC LIMIT ?"
            );
            let mut orphan_stmt = conn.prepare(&orphan_sql)?;
            let orphans = orphan_stmt
                .query_map(
                    params_from_iter(
                        std::iter::once(ws())
                            .chain(proj.clone())
                            .chain(std::iter::once(Value::Integer(limit))),
                    ),
                    health_page_from_row,
                )?
                .collect::<Result<Vec<_>, _>>()?;

            Ok(HealthDetail {
                stale,
                duplicates,
                orphans,
            })
        })
        .await
    }

    /// Look up a page's workspace and project names by page id.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn page_meta_by_id(&self, page_id: PageId) -> StoreResult<Option<PageMeta>> {
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("pg.path", "pg.frontmatter_json");
            let sql = format!(
                "SELECT w.name, p.name, w.id, p.id, pg.path, pg.title, \
                        {kind_expr}, \
                        pg.tier, pg.pinned, pg.created_at, pg.updated_at, \
                        sp.path AS supersedes_path, \
                        au.username, au.name, au.email, pg.expires_at \
                 FROM pages pg \
                 JOIN projects p ON p.id = pg.project_id \
                 JOIN workspaces w ON w.id = pg.workspace_id \
                 LEFT JOIN pages sp ON sp.id = pg.supersedes \
                 LEFT JOIN users au ON au.id = pg.author_id \
                 WHERE pg.id = ?1 AND pg.is_latest = 1"
            );
            let row_opt = conn
                .query_row(&sql, params![page_id.as_bytes()], page_meta_from_row)
                .optional()?;
            row_opt.transpose()
        })
        .await
    }

    /// Look up a page's workspace and project names by its path (across all
    /// workspaces and projects). Returns the first `is_latest = 1` match.
    ///
    /// Used by the web search handler to resolve workspace/project for a hit
    /// without a per-hit SQL join.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn page_meta_by_path(&self, path: &str) -> StoreResult<Option<PageMeta>> {
        let path = path.to_owned();
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("pg.path", "pg.frontmatter_json");
            let sql = format!(
                "SELECT w.name, p.name, w.id, p.id, pg.path, pg.title, \
                        {kind_expr}, \
                        pg.tier, pg.pinned, pg.created_at, pg.updated_at, \
                        sp.path AS supersedes_path, \
                        au.username, au.name, au.email, pg.expires_at \
                 FROM pages pg \
                 JOIN projects p ON p.id = pg.project_id \
                 JOIN workspaces w ON w.id = pg.workspace_id \
                 LEFT JOIN pages sp ON sp.id = pg.supersedes \
                 LEFT JOIN users au ON au.id = pg.author_id \
                 WHERE pg.path = ?1 AND pg.is_latest = 1 \
                 LIMIT 1"
            );
            let row_opt = conn
                .query_row(&sql, params![path], page_meta_from_row)
                .optional()?;
            row_opt.transpose()
        })
        .await
    }

    /// Resolve the outgoing links and incoming back-links for the latest
    /// version of a page identified by `(workspace_id, project_id, path)`.
    ///
    /// Both ends are constrained to `is_latest = 1`, so superseded versions
    /// never leak into the link panel. Returns empty lists when the page is
    /// missing or has no links.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn page_links(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: String,
    ) -> StoreResult<PageLinks> {
        self.with_conn(move |conn| {
            let id_opt: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT id FROM pages \
                     WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 \
                       AND is_latest = 1",
                    params![workspace_id.as_bytes(), project_id.as_bytes(), path],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(id_bytes) = id_opt else {
                return Ok(PageLinks::default());
            };

            // Outgoing: latest pages this page links to. Incoming: latest
            // pages that link here. Both reuse the path-inference `kind`
            // fallback so untagged pages still classify.
            let kind_expr = page_kind_expr("pg.path", "pg.frontmatter_json");
            let outgoing = format!(
                "SELECT DISTINCT pg.path, pg.title, {kind_expr}, \
                            ws.name, pr.name \
                     FROM links l \
                     JOIN pages pg ON pg.id = l.to_page_id \
                     JOIN projects pr ON pr.id = pg.project_id \
                     JOIN workspaces ws ON ws.id = pg.workspace_id \
                     WHERE l.from_page_id = ?1 AND pg.is_latest = 1 \
                     ORDER BY ws.name, pr.name, pg.path"
            );
            let incoming = format!(
                "SELECT DISTINCT pg.path, pg.title, {kind_expr}, \
                            ws.name, pr.name \
                     FROM links l \
                     JOIN pages pg ON pg.id = l.from_page_id \
                     JOIN projects pr ON pr.id = pg.project_id \
                     JOIN workspaces ws ON ws.id = pg.workspace_id \
                     WHERE l.to_page_id = ?1 AND pg.is_latest = 1 \
                     ORDER BY ws.name, pr.name, pg.path"
            );

            let collect = |sql: &str| -> StoreResult<Vec<RelatedPage>> {
                let mut stmt = conn.prepare(sql)?;
                let rows = stmt.query_map(params![id_bytes], |row| {
                    Ok(RelatedPage {
                        path: row.get(0)?,
                        title: row.get(1)?,
                        kind: row.get(2)?,
                        workspace: row.get(3)?,
                        project: row.get(4)?,
                    })
                })?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r?);
                }
                Ok(out)
            };

            Ok(PageLinks {
                links: collect(&outgoing)?,
                backlinks: collect(&incoming)?,
            })
        })
        .await
    }

    /// List unresolved cross-project links authored by pages in this project
    /// — declared dependencies on another project's page that don't resolve.
    /// Each row says whether the named target project exists, so the lint can
    /// tell a typo'd project name from a missing / renamed target page.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn dangling_cross_project_links(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<Vec<DanglingCrossLink>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT fp.path, l.to_workspace, l.to_project, l.to_path, \
                        EXISTS ( \
                            SELECT 1 FROM projects pr \
                            JOIN workspaces ws ON ws.id = pr.workspace_id \
                            WHERE pr.name = l.to_project \
                              AND ws.name = COALESCE( \
                                  l.to_workspace, \
                                  (SELECT name FROM workspaces WHERE id = ?1) \
                              ) \
                        ) AS project_exists \
                 FROM links l \
                 JOIN pages fp ON fp.id = l.from_page_id \
                     AND fp.workspace_id = ?1 AND fp.project_id = ?2 AND fp.is_latest = 1 \
                 WHERE l.to_page_id IS NULL AND l.to_project IS NOT NULL \
                 ORDER BY fp.path, l.to_project, l.to_path",
            )?;
            let rows = stmt.query_map(
                params![workspace_id.as_bytes(), project_id.as_bytes()],
                |row| {
                    let exists: i64 = row.get(4)?;
                    Ok(DanglingCrossLink {
                        from_path: row.get(0)?,
                        workspace: row.get(1)?,
                        project: row.get(2)?,
                        path: row.get(3)?,
                        project_exists: exists != 0,
                    })
                },
            )?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
    }

    /// Resolved cross-project edges (links whose endpoints are in different
    /// projects). When `scope` is `Some((ws, proj))`, only edges that touch
    /// that project (as source or target) are returned; `None` returns the
    /// whole cross-project graph. Powers `/api/v1/graph`.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn cross_project_edges(
        &self,
        scope: Option<(WorkspaceId, ProjectId)>,
    ) -> StoreResult<Vec<CrossProjectEdge>> {
        self.with_conn(move |conn| {
            let base = "SELECT fw.name, fpr.name, fp.path, tw.name, tpr.name, tp.path \
                 FROM links l \
                 JOIN pages fp ON fp.id = l.from_page_id AND fp.is_latest = 1 \
                 JOIN pages tp ON tp.id = l.to_page_id AND tp.is_latest = 1 \
                 JOIN projects fpr ON fpr.id = fp.project_id \
                 JOIN workspaces fw ON fw.id = fp.workspace_id \
                 JOIN projects tpr ON tpr.id = tp.project_id \
                 JOIN workspaces tw ON tw.id = tp.workspace_id \
                 WHERE fp.project_id != tp.project_id";
            let map_row = |row: &rusqlite::Row<'_>| {
                Ok(CrossProjectEdge {
                    from_workspace: row.get(0)?,
                    from_project: row.get(1)?,
                    from_path: row.get(2)?,
                    to_workspace: row.get(3)?,
                    to_project: row.get(4)?,
                    to_path: row.get(5)?,
                })
            };
            let mut out = Vec::new();
            if let Some((_ws, proj)) = scope {
                let sql =
                    format!("{base} AND (fp.project_id = ?1 OR tp.project_id = ?1) ORDER BY fw.name, fpr.name, fp.path");
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params![proj.as_bytes()], map_row)?;
                for r in rows {
                    out.push(r?);
                }
            } else {
                let sql = format!("{base} ORDER BY fw.name, fpr.name, fp.path");
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map([], map_row)?;
                for r in rows {
                    out.push(r?);
                }
            }
            Ok(out)
        })
        .await
    }

    /// Return one row per (workspace, project) with page-count and
    /// last-updated aggregates. Used by the web UI project-list view.
    ///
    /// Only `is_latest = 1` pages are counted.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn list_projects_with_stats(&self) -> StoreResult<Vec<ProjectSummary>> {
        self.list_projects_with_stats_filtered(None).await
    }

    /// Return one row per project within one workspace.
    ///
    /// Only `is_latest = 1` pages are counted.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn list_projects_with_stats_for_workspace(
        &self,
        workspace: String,
    ) -> StoreResult<Vec<ProjectSummary>> {
        self.list_projects_with_stats_filtered(Some(workspace))
            .await
    }

    async fn list_projects_with_stats_filtered(
        &self,
        workspace: Option<String>,
    ) -> StoreResult<Vec<ProjectSummary>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT w.name AS workspace_name, \
                        p.name AS project_name, \
                        COUNT(pg.id) AS page_count, \
                        MAX(pg.updated_at) AS last_updated_us \
                 FROM workspaces w \
                 JOIN projects p ON p.workspace_id = w.id \
                 LEFT JOIN pages pg ON pg.project_id = p.id AND pg.is_latest = 1 \
                 WHERE (?1 IS NULL OR w.name = ?1) \
                 GROUP BY w.id, p.id \
                 ORDER BY last_updated_us DESC NULLS LAST",
            )?;
            let rows = stmt.query_map(params![workspace], |row| {
                let workspace_name: String = row.get(0)?;
                let project_name: String = row.get(1)?;
                let page_count: i64 = row.get(2)?;
                let last_updated_us: Option<i64> = row.get(3)?;
                Ok((workspace_name, project_name, page_count, last_updated_us))
            })?;
            let mut out = Vec::new();
            for r in rows {
                let (workspace_name, project_name, page_count, last_updated_us) = r?;
                let last_updated = last_updated_us
                    .and_then(|us| jiff::Timestamp::from_microsecond(us).ok())
                    .map(|ts| ts.to_string());
                out.push(ProjectSummary {
                    workspace_name,
                    project_name,
                    page_count: u64::try_from(page_count).unwrap_or(0),
                    last_updated,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Return every `(workspace, project)` scope with its ids, names and
    /// `repo_path` — the data needed to write each scope's self-describing
    /// `_meta.md` manifest. Unlike [`list_projects_with_stats`], this carries
    /// the surrogate ids (not just names), so a rebuild can key the wiki tree.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn list_all_scopes(&self) -> StoreResult<Vec<ScopeRow>> {
        // (ws_id, ws_name, proj_id, proj_name, repo_path) as raw SQL columns.
        type RawScope = (Vec<u8>, String, Vec<u8>, String, Option<String>);
        let raw: Vec<RawScope> = self
            .with_conn(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT w.id, w.name, p.id, p.name, p.repo_path \
                     FROM projects p JOIN workspaces w ON w.id = p.workspace_id",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                })?;
                let out: rusqlite::Result<Vec<_>> = rows.collect();
                Ok(out?)
            })
            .await?;
        raw.into_iter()
            .map(|(wi, wn, pi, pn, rp)| {
                Ok(ScopeRow {
                    workspace_id: WorkspaceId::from_slice(&wi)?,
                    workspace_name: wn,
                    project_id: ProjectId::from_slice(&pi)?,
                    project_name: pn,
                    repo_path: rp,
                })
            })
            .collect()
    }

    /// Return every workspace with its id and name so manifest backfill can
    /// describe even empty workspaces that have no project rows yet.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn list_all_workspace_scopes(&self) -> StoreResult<Vec<WorkspaceScopeRow>> {
        let raw: Vec<(Vec<u8>, String)> = self
            .with_conn(|conn| {
                let mut stmt = conn.prepare("SELECT id, name FROM workspaces")?;
                let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
                let out: rusqlite::Result<Vec<_>> = rows.collect();
                Ok(out?)
            })
            .await?;
        raw.into_iter()
            .map(|(wi, wn)| {
                Ok(WorkspaceScopeRow {
                    workspace_id: WorkspaceId::from_slice(&wi)?,
                    workspace_name: wn,
                })
            })
            .collect()
    }

    /// Return counts that must be zero before `ai-memory reindex` runs.
    ///
    /// `reindex` is a rebuild-from-files operation, not an in-place repair of a
    /// dirty DB. If rows already exist, stale derived rows or DB-only episodic
    /// state could survive and contradict the "rebuilt from wiki" contract.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn reindex_target_status(&self) -> StoreResult<ReindexTargetStatus> {
        self.with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT \
                    (SELECT COUNT(*) FROM workspaces), \
                    (SELECT COUNT(*) FROM projects), \
                    (SELECT COUNT(*) FROM pages), \
                    (SELECT COUNT(*) FROM links), \
                    (SELECT COUNT(*) FROM page_embeddings), \
                    (SELECT COUNT(*) FROM sessions), \
                    (SELECT COUNT(*) FROM observations), \
                    (SELECT COUNT(*) FROM handoffs), \
                    (SELECT COUNT(*) FROM users), \
                    (SELECT COUNT(*) FROM audit_log)",
                [],
                |row| {
                    Ok(ReindexTargetStatus {
                        workspaces: row.get(0)?,
                        projects: row.get(1)?,
                        pages: row.get(2)?,
                        links: row.get(3)?,
                        page_embeddings: row.get(4)?,
                        sessions: row.get(5)?,
                        observations: row.get(6)?,
                        handoffs: row.get(7)?,
                        users: row.get(8)?,
                        audit_log: row.get(9)?,
                    })
                },
            )?)
        })
        .await
    }

    /// Return one row per workspace with project/page-count and
    /// last-updated aggregates. Used by custom frontends that need a
    /// workspace chooser before narrowing into projects.
    ///
    /// Only `is_latest = 1` pages are counted.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn list_workspaces_with_stats(&self) -> StoreResult<Vec<WorkspaceSummary>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT w.name AS workspace_name, \
                        COUNT(DISTINCT p.id) AS project_count, \
                        COUNT(pg.id) AS page_count, \
                        MAX(pg.updated_at) AS last_updated_us \
                 FROM workspaces w \
                 LEFT JOIN projects p ON p.workspace_id = w.id \
                 LEFT JOIN pages pg ON pg.project_id = p.id AND pg.is_latest = 1 \
                 GROUP BY w.id \
                 ORDER BY w.name ASC",
            )?;
            let rows = stmt.query_map([], |row| {
                let workspace_name: String = row.get(0)?;
                let project_count: i64 = row.get(1)?;
                let page_count: i64 = row.get(2)?;
                let last_updated_us: Option<i64> = row.get(3)?;
                Ok((workspace_name, project_count, page_count, last_updated_us))
            })?;
            let mut out = Vec::new();
            for r in rows {
                let (workspace_name, project_count, page_count, last_updated_us) = r?;
                let last_updated = last_updated_us
                    .and_then(|us| jiff::Timestamp::from_microsecond(us).ok())
                    .map(|ts| ts.to_string());
                out.push(WorkspaceSummary {
                    workspace_name,
                    project_count: u64::try_from(project_count).unwrap_or(0),
                    page_count: u64::try_from(page_count).unwrap_or(0),
                    last_updated,
                });
            }
            Ok(out)
        })
        .await
    }

    /// All `is_latest = 1` pages under a given (workspace, project),
    /// ordered by path ascending. Used by the web UI tree view.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn list_pages(
        &self,
        workspace: &str,
        project: &str,
    ) -> StoreResult<Vec<PageSummary>> {
        let workspace = workspace.to_owned();
        let project = project.to_owned();
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("pg.path", "pg.frontmatter_json");
            let mut stmt = conn.prepare(&format!(
                "SELECT pg.path, pg.title, {kind_expr} AS kind, \
                        pg.tier, pg.updated_at \
                 FROM pages pg \
                 JOIN projects p ON p.id = pg.project_id \
                 JOIN workspaces w ON w.id = pg.workspace_id \
                 WHERE w.name = ?1 AND p.name = ?2 AND pg.is_latest = 1 \
                 ORDER BY pg.path ASC"
            ))?;
            let rows = stmt.query_map(params![workspace, project], |row| {
                let path: String = row.get(0)?;
                let title: String = row.get(1)?;
                let kind: String = row.get(2)?;
                let tier: String = row.get(3)?;
                let updated_us: i64 = row.get(4)?;
                Ok((path, title, kind, tier, updated_us))
            })?;
            let mut out = Vec::new();
            for r in rows {
                let (path, title, kind, tier, updated_us) = r?;
                let updated_at = jiff::Timestamp::from_microsecond(updated_us)
                    .map(|ts| ts.to_string())
                    .unwrap_or_default();
                out.push(PageSummary {
                    path,
                    title,
                    kind,
                    tier,
                    updated_at,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Full page metadata for the page-view template (body comes from
    /// `Wiki::read_page`). Returns `None` when no `is_latest = 1` row
    /// matches the given path.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn page_meta(
        &self,
        workspace: &str,
        project: &str,
        page_path: &str,
    ) -> StoreResult<Option<PageMeta>> {
        let workspace = workspace.to_owned();
        let project = project.to_owned();
        let page_path = page_path.to_owned();
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("pg.path", "pg.frontmatter_json");
            let sql = format!(
                "SELECT w.name, p.name, w.id, p.id, pg.path, pg.title, \
                        {kind_expr}, \
                        pg.tier, pg.pinned, pg.created_at, pg.updated_at, \
                        sp.path AS supersedes_path, \
                        au.username, au.name, au.email, pg.expires_at \
                 FROM pages pg \
                 JOIN projects p ON p.id = pg.project_id \
                 JOIN workspaces w ON w.id = pg.workspace_id \
                 LEFT JOIN pages sp ON sp.id = pg.supersedes \
                 LEFT JOIN users au ON au.id = pg.author_id \
                 WHERE w.name = ?1 AND p.name = ?2 AND pg.path = ?3 AND pg.is_latest = 1"
            );
            let row_opt = conn
                .query_row(
                    &sql,
                    params![workspace, project, page_path],
                    page_meta_from_row,
                )
                .optional()?;
            row_opt.transpose()
        })
        .await
    }

    /// Fetch the latest version's stored body/title/frontmatter for a page by
    /// `(workspace_id, project_id, path)`. Used as a DB-backed fallback for
    /// `memory_read_page` when the on-disk markdown read fails (index/disk
    /// skew — see `gotchas/read-page-by-query-misses`). Returns `None` when no
    /// `is_latest` row exists.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn page_body_by_ids(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &str,
    ) -> StoreResult<Option<StoredPageBody>> {
        let path = path.to_owned();
        self.with_conn(move |conn| {
            let row = conn
                .query_row(
                    "SELECT title, body, frontmatter_json, tier, pinned FROM pages \
                     WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 \
                       AND is_latest = 1",
                    params![workspace_id.as_bytes(), project_id.as_bytes(), path],
                    |row| {
                        Ok(StoredPageBody {
                            title: row.get(0)?,
                            body: row.get(1)?,
                            frontmatter_json: row.get(2)?,
                            tier: row.get(3)?,
                            pinned: row.get::<_, i64>(4)? != 0,
                        })
                    },
                )
                .optional()?;
            Ok(row)
        })
        .await
    }

    /// Return the latest page version id for one fully scoped path.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn latest_page_id_by_ids(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: String,
    ) -> StoreResult<Option<PageId>> {
        self.with_conn(move |conn| {
            let id: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT id FROM pages \
                     WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 \
                       AND is_latest = 1",
                    params![workspace_id.as_bytes(), project_id.as_bytes(), path],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(id.map(|bytes| PageId::from_slice(&bytes)).transpose()?)
        })
        .await
    }

    /// Return whether the latest version of one fully scoped page is expired.
    /// Returns `None` when the path has no latest indexed row.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn page_expired_by_ids(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &str,
    ) -> StoreResult<Option<bool>> {
        let path = path.to_owned();
        self.with_conn(move |conn| {
            let expires_at: Option<Option<i64>> = conn
                .query_row(
                    "SELECT expires_at FROM pages \
                     WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 \
                       AND is_latest = 1",
                    params![workspace_id.as_bytes(), project_id.as_bytes(), path],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(expires_at.map(|expires_at| expires_at.is_some_and(|value| value <= now_us())))
        })
        .await
    }

    /// List auto-improvement proposals for one scope, optionally filtered by status.
    pub async fn list_auto_improve_proposals(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        status: Option<AutoImproveProposalStatus>,
        limit: usize,
    ) -> StoreResult<Vec<AutoImproveProposalSummary>> {
        self.with_conn(move |conn| {
            let limit = i64::try_from(limit).unwrap_or(i64::MAX);
            let sql = if status.is_some() {
                "SELECT id, run_id, workspace_id, project_id, status, operation, target_path, \
                        kind, title, confidence, staged_at, decided_at \
                 FROM auto_improve_proposals \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND status = ?3 \
                 ORDER BY staged_at DESC LIMIT ?4"
            } else {
                "SELECT id, run_id, workspace_id, project_id, status, operation, target_path, \
                        kind, title, confidence, staged_at, decided_at \
                 FROM auto_improve_proposals \
                 WHERE workspace_id = ?1 AND project_id = ?2 \
                 ORDER BY staged_at DESC LIMIT ?3"
            };
            let mut stmt = conn.prepare(sql)?;
            let mut out = Vec::new();
            if let Some(status) = status {
                let rows = stmt.query_map(
                    params![
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                        status.as_str(),
                        limit
                    ],
                    summary_from_row,
                )?;
                for row in rows {
                    out.push(row?);
                }
            } else {
                let rows = stmt.query_map(
                    params![workspace_id.as_bytes(), project_id.as_bytes(), limit],
                    summary_from_row,
                )?;
                for row in rows {
                    out.push(row?);
                }
            }
            Ok(out)
        })
        .await
    }

    /// Read one proposal by id, failing closed when the scope does not match.
    pub async fn auto_improve_proposal_detail(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        proposal_id: AutoImproveProposalId,
    ) -> StoreResult<Option<AutoImproveProposalDetail>> {
        self.with_conn(move |conn| {
            let row = conn
                .query_row(
                    "SELECT id, run_id, workspace_id, project_id, status, operation, target_path, \
                            kind, title, confidence, staged_at, decided_at, rationale, \
                            evidence_json, body_markdown, body_sha256, artifact_path, \
                            artifact_sha256, target_latest_page_id_at_stage, \
                            target_body_sha256_at_stage, target_updated_at_at_stage, \
                            decision_reason, decided_by_author_id, decided_by_actor_json, \
                            applied_page_id, checkpoint, edit_mode, patch_json, \
                            expected_base_body_sha256, materialized_base_body_sha256, \
                            staged_by_actor_user \
                     FROM auto_improve_proposals \
                     WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3",
                    params![
                        proposal_id.as_bytes(),
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                    ],
                    |row| {
                        let summary = summary_from_row(row)?;
                        let evidence_raw: String = row.get(13)?;
                        let body_hash = bytes32(row.get(15)?).map_err(to_sql_err)?;
                        let artifact_hash = opt_bytes32(row.get(17)?).map_err(to_sql_err)?;
                        let staged_page_id = row
                            .get::<_, Option<Vec<u8>>>(18)?
                            .map(|b| PageId::from_slice(&b))
                            .transpose()
                            .map_err(to_sql_err)?;
                        let staged_body_hash = opt_bytes32(row.get(19)?).map_err(to_sql_err)?;
                        let decided_author = row
                            .get::<_, Option<Vec<u8>>>(22)?
                            .map(|b| UserId::from_slice(&b))
                            .transpose()
                            .map_err(to_sql_err)?;
                        let decided_actor_raw: Option<String> = row.get(23)?;
                        let applied_page_id = row
                            .get::<_, Option<Vec<u8>>>(24)?
                            .map(|b| PageId::from_slice(&b))
                            .transpose()
                            .map_err(to_sql_err)?;
                        let patch_raw: Option<String> = row.get(27)?;
                        Ok(AutoImproveProposalDetail {
                            summary,
                            rationale: row.get(12)?,
                            evidence_json: serde_json::from_str(&evidence_raw)
                                .map_err(to_sql_err)?,
                            body_markdown: row.get(14)?,
                            body_sha256: body_hash,
                            artifact_path: row.get(16)?,
                            artifact_sha256: artifact_hash,
                            target_latest_page_id_at_stage: staged_page_id,
                            target_body_sha256_at_stage: staged_body_hash,
                            target_updated_at_at_stage: row.get(20)?,
                            decision_reason: row.get(21)?,
                            decided_by_author_id: decided_author,
                            decided_by_actor_json: decided_actor_raw
                                .map(|raw| serde_json::from_str(&raw))
                                .transpose()
                                .map_err(to_sql_err)?,
                            applied_page_id,
                            checkpoint: row.get(25)?,
                            edit_mode: row.get(26)?,
                            patch_json: patch_raw
                                .map(|raw| serde_json::from_str(&raw))
                                .transpose()
                                .map_err(to_sql_err)?,
                            expected_base_body_sha256: opt_bytes32(row.get(28)?)
                                .map_err(to_sql_err)?,
                            materialized_base_body_sha256: opt_bytes32(row.get(29)?)
                                .map_err(to_sql_err)?,
                            staged_by_actor_user: row.get(30)?,
                            events: Vec::new(),
                        })
                    },
                )
                .optional()?;
            let Some(mut detail) = row else {
                return Ok(None);
            };
            let mut stmt = conn.prepare(
                "SELECT id, proposal_id, event, actor_json, author_id, detail_json, at \
                 FROM auto_improve_proposal_events \
                 WHERE proposal_id = ?1 ORDER BY at ASC, id ASC",
            )?;
            let rows = stmt.query_map(params![proposal_id.as_bytes()], |row| {
                let proposal_id = AutoImproveProposalId::from_slice(&row.get::<_, Vec<u8>>(1)?)
                    .map_err(to_sql_err)?;
                let actor_raw: String = row.get(3)?;
                let detail_raw: String = row.get(5)?;
                let author_id = row
                    .get::<_, Option<Vec<u8>>>(4)?
                    .map(|b| UserId::from_slice(&b))
                    .transpose()
                    .map_err(to_sql_err)?;
                Ok(AutoImproveProposalEvent {
                    id: row.get(0)?,
                    proposal_id,
                    event: row.get(2)?,
                    actor_json: serde_json::from_str(&actor_raw).map_err(to_sql_err)?,
                    author_id,
                    detail_json: serde_json::from_str(&detail_raw).map_err(to_sql_err)?,
                    at: row.get(6)?,
                })
            })?;
            for row in rows {
                detail.events.push(row?);
            }
            Ok(Some(detail))
        })
        .await
    }

    /// Read recent rejection-buffer entries for one workspace/project scope.
    pub async fn recent_auto_improve_rejections(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        limit: usize,
        since_created_at: Option<i64>,
    ) -> StoreResult<Vec<AutoImproveRejectionSummary>> {
        self.with_conn(move |conn| {
            let limit = i64::try_from(limit).unwrap_or(i64::MAX);
            let since = since_created_at.unwrap_or(0);
            let mut stmt = conn.prepare(
                "SELECT id, workspace_id, project_id, target_path, kind, operation, edit_mode, \
                        reason, normalized_fingerprint, summary, evidence_json, source_run_id, \
                        source_proposal_id, created_at \
                 FROM auto_improve_rejections \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND created_at >= ?3 \
                 ORDER BY created_at DESC LIMIT ?4",
            )?;
            let rows = stmt.query_map(
                params![workspace_id.as_bytes(), project_id.as_bytes(), since, limit],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    let id = Uuid::from_slice(&id_bytes).map_err(to_sql_err)?.to_string();
                    let workspace_id =
                        WorkspaceId::from_slice(&row.get::<_, Vec<u8>>(1)?).map_err(to_sql_err)?;
                    let project_id =
                        ProjectId::from_slice(&row.get::<_, Vec<u8>>(2)?).map_err(to_sql_err)?;
                    let evidence_raw: String = row.get(10)?;
                    let source_run_id = row
                        .get::<_, Option<Vec<u8>>>(11)?
                        .map(|b| AutoImproveRunId::from_slice(&b))
                        .transpose()
                        .map_err(to_sql_err)?;
                    let source_proposal_id = row
                        .get::<_, Option<Vec<u8>>>(12)?
                        .map(|b| AutoImproveProposalId::from_slice(&b))
                        .transpose()
                        .map_err(to_sql_err)?;
                    Ok(AutoImproveRejectionSummary {
                        id,
                        workspace_id,
                        project_id,
                        target_path: row.get(3)?,
                        kind: row.get(4)?,
                        operation: row.get(5)?,
                        edit_mode: row.get(6)?,
                        reason: row.get(7)?,
                        normalized_fingerprint: row.get(8)?,
                        summary: row.get(9)?,
                        evidence_json: serde_json::from_str(&evidence_raw).map_err(to_sql_err)?,
                        source_run_id,
                        source_proposal_id,
                        created_at: row.get(13)?,
                    })
                },
            )?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    /// Aggregate auto-improvement telemetry for one workspace/project scope.
    ///
    /// `since_created_at` filters by run/proposal/rejection creation time in Unix
    /// microseconds. Maintenance/report proposals are excluded from learning
    /// metrics and counted separately.
    pub async fn auto_improve_telemetry_aggregate(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        since_created_at: i64,
        top_limit: usize,
    ) -> StoreResult<AutoImproveTelemetryAggregate> {
        self.with_conn(move |conn| {
            let top_limit = i64::try_from(top_limit).unwrap_or(i64::MAX);
            let run_count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM auto_improve_runs
                 WHERE workspace_id = ?1 AND project_id = ?2 AND created_at >= ?3",
                params![
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    since_created_at
                ],
                |row| row.get(0),
            )?;
            let runs_with_learning_proposals: i64 = conn.query_row(
                "SELECT COUNT(DISTINCT run_id) FROM auto_improve_proposals
                 WHERE workspace_id = ?1 AND project_id = ?2 AND staged_at >= ?3
                   AND kind NOT IN ('curator_report', 'auto_improve_report')",
                params![
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    since_created_at
                ],
                |row| row.get(0),
            )?;
            Ok(AutoImproveTelemetryAggregate {
                run_count: usize::try_from(run_count).unwrap_or(usize::MAX),
                runs_with_learning_proposals: usize::try_from(runs_with_learning_proposals)
                    .unwrap_or(usize::MAX),
                proposals_by_status: auto_improve_group_counts(
                    conn,
                    "status",
                    workspace_id,
                    project_id,
                    since_created_at,
                    None,
                )?,
                proposals_by_operation: auto_improve_group_counts(
                    conn,
                    "operation",
                    workspace_id,
                    project_id,
                    since_created_at,
                    None,
                )?,
                proposals_by_edit_mode: auto_improve_group_counts(
                    conn,
                    "edit_mode",
                    workspace_id,
                    project_id,
                    since_created_at,
                    None,
                )?,
                proposals_by_kind: auto_improve_group_counts(
                    conn,
                    "kind",
                    workspace_id,
                    project_id,
                    since_created_at,
                    None,
                )?,
                maintenance_proposals_by_kind: auto_improve_group_counts(
                    conn,
                    "kind",
                    workspace_id,
                    project_id,
                    since_created_at,
                    Some("kind IN ('curator_report', 'auto_improve_report')"),
                )?,
                top_targets: auto_improve_group_counts(
                    conn,
                    "target_path",
                    workspace_id,
                    project_id,
                    since_created_at,
                    Some("kind NOT IN ('curator_report', 'auto_improve_report')"),
                )?
                .into_iter()
                .take(usize::try_from(top_limit).unwrap_or(usize::MAX))
                .collect(),
                rejections_by_reason: auto_improve_rejection_group_counts(
                    conn,
                    "r.reason",
                    workspace_id,
                    project_id,
                    since_created_at,
                    top_limit,
                )?,
                repeated_rejection_fingerprints: auto_improve_repeated_rejection_fingerprints(
                    conn,
                    workspace_id,
                    project_id,
                    since_created_at,
                    top_limit,
                )?,
                rejected_targets: auto_improve_rejection_group_counts(
                    conn,
                    "COALESCE(r.target_path, '(none)')",
                    workspace_id,
                    project_id,
                    since_created_at,
                    top_limit,
                )?,
            })
        })
        .await
    }

    /// Look up a workspace id by name without creating it.
    ///
    /// Returns `None` when no workspace with the given name exists.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn find_workspace(&self, name: String) -> StoreResult<Option<WorkspaceId>> {
        self.with_conn(move |conn| {
            let row_opt = conn
                .query_row(
                    "SELECT id FROM workspaces WHERE name = ?1",
                    params![name],
                    |row| {
                        let bytes: Vec<u8> = row.get(0)?;
                        Ok(bytes)
                    },
                )
                .optional()?;
            row_opt
                .map(|bytes| WorkspaceId::from_slice(&bytes).map_err(StoreError::from))
                .transpose()
        })
        .await
    }

    /// Look up a project id by `(workspace_id, name)` without creating it.
    ///
    /// Returns `None` when no project with the given name exists in the workspace.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn find_project(
        &self,
        workspace_id: WorkspaceId,
        name: String,
    ) -> StoreResult<Option<ProjectId>> {
        self.with_conn(move |conn| {
            let row_opt = conn
                .query_row(
                    "SELECT id FROM projects WHERE workspace_id = ?1 AND name = ?2",
                    params![workspace_id.as_bytes(), name],
                    |row| {
                        let bytes: Vec<u8> = row.get(0)?;
                        Ok(bytes)
                    },
                )
                .optional()?;
            row_opt
                .map(|bytes| ProjectId::from_slice(&bytes).map_err(StoreError::from))
                .transpose()
        })
        .await
    }

    /// Find the existing project whose `repo_path` is the longest
    /// prefix of `cwd`, if any. Used by the hook router before
    /// auto-creating a new project from `basename(cwd)` so an event
    /// whose cwd is inside an existing project's tree resolves to
    /// that parent instead of materialising a fragment project for
    /// the subdirectory name.
    ///
    /// `ORDER BY length(repo_path) DESC` picks the most-specific
    /// match: if the operator declared a sub-project via
    /// `.ai-memory.toml` (which writes its own row with a longer
    /// `repo_path`), the sub-project wins over its outer parent.
    ///
    /// **Boundary safety.** Every layer of this query has explicit
    /// guards so a path-shaped value can't accidentally widen the
    /// match set:
    ///
    /// - `workspace_id = ?1` — never matches a project in another
    ///   workspace.
    /// - `repo_path IS NOT NULL` plus Rust-side normalization rejects stored
    ///   values that would match too broadly (`NULL`, `''`, `/`) or fail the
    ///   `<repo_path>/%` boundary (trailing slash/backslash).
    /// - Stored `repo_path` wildcards (`%`, `_`) are compared as literal path
    ///   bytes in Rust, not as SQL `LIKE` wildcards.
    /// - A normalized stored `repo_path` equal to the operator's home
    ///   directory (`home`) is never matched.
    ///   Such a row would be a prefix catch-all for every project
    ///   beneath `$HOME`; the caller passes the server's own `$HOME`
    ///   (filesystem root `/` is already excluded by the
    ///   `length(repo_path) > 1` guard). A `None` `home` is a no-op.
    /// - Input canonicalisation — trailing slashes stripped; cwds
    ///   containing dot-segments (`/foo/../bar`, `/./x`) are rejected
    ///   outright so a traversal-style path can't match a parent it
    ///   doesn't logically belong to.
    ///
    /// Returns `None` (caller falls through to create-by-basename)
    /// for every defensive case so a bad input degrades the same way
    /// as "no match" rather than picking the wrong project.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn find_project_by_cwd_prefix(
        &self,
        workspace_id: WorkspaceId,
        cwd: String,
        home: Option<&str>,
    ) -> StoreResult<Option<(ProjectId, String)>> {
        let home = home.map(|h| normalize_cwd(h).into_owned());
        let cwd_norm = normalize_cwd(&cwd).into_owned();
        if !is_safe_cwd_for_prefix_match(&cwd_norm) {
            return Ok(None);
        }
        self.with_conn(move |conn| {
            // Compare in Rust instead of SQL LIKE so stored `%`/`_` bytes stay
            // literal and legacy Windows rows with backslashes keep matching a
            // slash-normalized hook cwd. New writes normalize `repo_path`, but
            // this compatibility read path protects existing databases.
            let mut stmt = conn.prepare(
                "SELECT id, name, repo_path FROM projects \
                 WHERE workspace_id = ?1 AND repo_path IS NOT NULL",
            )?;
            let rows = stmt.query_map(params![workspace_id.as_bytes()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            let mut matches = Vec::new();
            for row in rows {
                let (bytes, name, repo_path) = row?;
                if has_trailing_path_separator(&repo_path) {
                    continue;
                }
                let repo_norm = normalize_cwd(&repo_path).into_owned();
                if repo_norm.len() <= 1
                    || !is_safe_cwd_for_prefix_match(&repo_norm)
                    || home.as_deref() == Some(repo_norm.as_str())
                {
                    continue;
                }
                if cwd_within(&repo_norm, &cwd_norm) {
                    matches.push((bytes, name, repo_norm.len()));
                }
            }
            matches.sort_by_key(|entry| std::cmp::Reverse(entry.2));
            matches
                .into_iter()
                .next()
                .map(|(bytes, name, _)| {
                    ProjectId::from_slice(&bytes)
                        .map(|id| (id, name))
                        .map_err(StoreError::from)
                })
                .transpose()
        })
        .await
    }

    /// Structural cross-project contamination audit (cheap, SQL-only, no LLM).
    ///
    /// One HIGH-precision heuristic:
    /// - **CHECK A** (`session_wrong_bucket`): a session whose `cwd`
    ///   longest-prefix-resolves to a *different* project than the one it landed
    ///   in — the direct signature of the auto-scope bleed bug. Resolved with the
    ///   same prefix and cwd-safety rules as [`Self::find_project_by_cwd_prefix`],
    ///   so the audit never claims a session a live resolve would not.
    ///
    /// Note: an earlier "observation drifted from its session's project" check
    /// was removed. With per-event cwd resolution, an observation's `project_id`
    /// is set from the cwd *of that event* — so an agent that legitimately
    /// `cd`s across repos in one session produces observations whose project
    /// differs from the session's home project. That is correct attribution,
    /// not contamination, so the check flagged normal cross-repo work as a false
    /// positive (and observations carry no cwd of their own to disambiguate).
    /// CHECK A — anchored on the session's own cwd — remains the precise signal.
    ///
    /// Detects only contamination with a STRUCTURAL trace; purely semantic
    /// mislandings (topic-level, no cwd/session anomaly) are not detectable.
    /// `scope` restricts findings to the given landed `(workspace, project)`.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn audit_contamination(
        &self,
        scope: Option<(WorkspaceId, ProjectId)>,
        home: Option<&str>,
    ) -> StoreResult<ContaminationReport> {
        let scoped = scope.is_some();
        let home = home.map(|h| normalize_cwd(h).into_owned());
        let scope_params: Vec<Value> = match &scope {
            Some((ws, proj)) => vec![
                Value::Blob(ws.as_bytes().to_vec()),
                Value::Blob(proj.as_bytes().to_vec()),
            ],
            None => Vec::new(),
        };

        // One connection: the candidate session list + the valid repo_path
        // prefixes used to resolve CHECK A. Loading prefixes once avoids a
        // reader query per historical session while preserving the same path
        // boundary and safety rules as the runtime resolver.
        type Candidate = (String, WorkspaceId, ProjectId, String, String, String);
        type Prefix = (WorkspaceId, ProjectId, String, String);
        let (mut findings, candidates, prefixes): (
            Vec<ContaminationFinding>,
            Vec<Candidate>,
            Vec<Prefix>,
        ) = self
            .with_conn(move |conn| {
                // CHECK A findings are built after this closure (they need the
                // repo_path prefixes resolved outside the DB connection); this
                // closure only gathers the raw inputs.
                let findings: Vec<ContaminationFinding> = Vec::new();

                // Candidate sessions (cwd present) — resolved outside the conn.
                let mut s_sql = String::from(
                    "SELECT lower(hex(s.id)), s.workspace_id, s.project_id, wl.name, pl.name, s.cwd \
                     FROM sessions s \
                     JOIN workspaces wl ON wl.id = s.workspace_id \
                     JOIN projects pl ON pl.id = s.project_id \
                     WHERE s.cwd IS NOT NULL",
                );
                if scoped {
                    s_sql.push_str(" AND s.workspace_id = ? AND s.project_id = ?");
                }
                let mut candidates: Vec<Candidate> = Vec::new();
                {
                    let mut stmt = conn.prepare(&s_sql)?;
                    let rows = stmt.query_map(params_from_iter(scope_params.iter()), |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Vec<u8>>(1)?,
                            row.get::<_, Vec<u8>>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                        ))
                    })?;
                    for r in rows {
                        let (id, ws_b, proj_b, lws, lproj, cwd) = r?;
                        candidates.push((
                            id,
                            WorkspaceId::from_slice(&ws_b)?,
                            ProjectId::from_slice(&proj_b)?,
                            lws,
                            lproj,
                            cwd,
                        ));
                    }
                }

                let mut p_sql = String::from(
                    "SELECT workspace_id, id, name, repo_path \
                     FROM projects \
                     WHERE repo_path IS NOT NULL \
                       AND length(repo_path) > 1 \
                       AND repo_path NOT LIKE '%/'",
                );
                if scoped {
                    p_sql.push_str(" AND workspace_id = ?");
                }
                p_sql.push_str(" ORDER BY length(repo_path) DESC");
                let mut prefixes: Vec<Prefix> = Vec::new();
                {
                    let mut stmt = conn.prepare(&p_sql)?;
                    if scoped {
                        let rows = stmt.query_map(params_from_iter(scope_params.iter().take(1)), |row| {
                            Ok((
                                row.get::<_, Vec<u8>>(0)?,
                                row.get::<_, Vec<u8>>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                            ))
                        })?;
                        for r in rows {
                            let (ws_b, proj_b, name, repo_path) = r?;
                            if has_trailing_path_separator(&repo_path) {
                                continue;
                            }
                            let repo_path = normalize_cwd(&repo_path).into_owned();
                            if repo_path.len() <= 1
                                || !is_safe_cwd_for_prefix_match(&repo_path)
                                || home.as_deref() == Some(repo_path.as_str())
                            {
                                continue;
                            }
                            prefixes.push((
                                WorkspaceId::from_slice(&ws_b)?,
                                ProjectId::from_slice(&proj_b)?,
                                name,
                                repo_path,
                            ));
                        }
                    } else {
                        let rows = stmt.query_map([], |row| {
                            Ok((
                                row.get::<_, Vec<u8>>(0)?,
                                row.get::<_, Vec<u8>>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                            ))
                        })?;
                        for r in rows {
                            let (ws_b, proj_b, name, repo_path) = r?;
                            if has_trailing_path_separator(&repo_path) {
                                continue;
                            }
                            let repo_path = normalize_cwd(&repo_path).into_owned();
                            if repo_path.len() <= 1
                                || !is_safe_cwd_for_prefix_match(&repo_path)
                                || home.as_deref() == Some(repo_path.as_str())
                            {
                                continue;
                            }
                            prefixes.push((
                                WorkspaceId::from_slice(&ws_b)?,
                                ProjectId::from_slice(&proj_b)?,
                                name,
                                repo_path,
                            ));
                        }
                    }
                }

                Ok((findings, candidates, prefixes))
            })
            .await?;

        // CHECK A: resolve each session's cwd against preloaded valid prefixes
        // and flag a bucket mismatch.
        for (id, ws, landed_proj, landed_ws_name, landed_proj_name, cwd) in candidates {
            let cwd_norm = normalize_cwd(&cwd).into_owned();
            if !is_safe_cwd_for_prefix_match(&cwd_norm) {
                continue;
            }
            let resolved = prefixes.iter().find(|(prefix_ws, _, _, repo_path)| {
                *prefix_ws == ws && cwd_within(repo_path, &cwd_norm)
            });
            if let Some((_, resolved_proj, resolved_name, _)) = resolved
                && *resolved_proj != landed_proj
            {
                findings.push(ContaminationFinding {
                    check: "session_wrong_bucket",
                    confidence: "high",
                    entity_kind: "session",
                    entity_id: id,
                    landed_workspace: landed_ws_name,
                    landed_project: landed_proj_name,
                    expected_project: Some(resolved_name.clone()),
                    cwd: Some(cwd),
                });
            }
        }

        let summary = ContaminationSummary {
            sessions_misbucketed: findings
                .iter()
                .filter(|f| f.check == "session_wrong_bucket")
                .count(),
        };
        Ok(ContaminationReport { summary, findings })
    }

    /// Return aggregate counts for the `status` view.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn status_counts(&self) -> StoreResult<StatusCounts> {
        self.with_conn(|conn| {
            let pages_latest: u64 = count(conn, "SELECT COUNT(*) FROM pages WHERE is_latest = 1")?;
            let pages_all: u64 = count(conn, "SELECT COUNT(*) FROM pages")?;
            let sessions: u64 = count(conn, "SELECT COUNT(*) FROM sessions")?;
            let observations: u64 = count(conn, "SELECT COUNT(*) FROM observations")?;
            Ok(StatusCounts {
                pages_latest,
                pages_all,
                sessions,
                observations,
            })
        })
        .await
    }

    /// Return health counters for derived indexes and link/embedding state.
    ///
    /// These checks are intentionally read-only and derived-index-safe: they
    /// report drift but do not repair it. Rebuild/backfill paths stay behind
    /// explicit admin operations.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn derived_index_status(&self) -> StoreResult<DerivedIndexStatus> {
        self.with_conn(|conn| {
            let mut triples_stmt = conn.prepare(
                "SELECT provider, model, dim, COUNT(*) \
                 FROM page_embeddings \
                 GROUP BY provider, model, dim \
                 ORDER BY COUNT(*) DESC, provider, model, dim",
            )?;
            let embedding_triples = triples_stmt
                .query_map([], |row| {
                    let provider: String = row.get(0)?;
                    let model: String = row.get(1)?;
                    let dim: i64 = row.get(2)?;
                    let count: i64 = row.get(3)?;
                    Ok(EmbeddingTripleCount {
                        provider,
                        model,
                        dim: u32::try_from(dim.max(0)).unwrap_or(0),
                        count: u64::try_from(count.max(0)).unwrap_or(0),
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;

            Ok(DerivedIndexStatus {
                pages_rows: count(conn, "SELECT COUNT(*) FROM pages")?,
                pages_fts_rows: count(conn, "SELECT COUNT(*) FROM pages_fts")?,
                observations_rows: count(conn, "SELECT COUNT(*) FROM observations")?,
                observations_fts_rows: count(conn, "SELECT COUNT(*) FROM observations_fts")?,
                latest_pages_missing_embeddings: count(
                    conn,
                    "SELECT COUNT(*) \
                     FROM pages pg \
                     LEFT JOIN page_embeddings pe ON pe.page_id = pg.id \
                     WHERE pg.is_latest = 1 AND pe.page_id IS NULL",
                )?,
                embedding_rows: count(conn, "SELECT COUNT(*) FROM page_embeddings")?,
                embedding_triples,
                links_from_latest_pages: count(
                    conn,
                    "SELECT COUNT(*) \
                     FROM links l \
                     JOIN pages fp ON fp.id = l.from_page_id \
                     WHERE fp.is_latest = 1",
                )?,
                unresolved_links_from_latest_pages: count(
                    conn,
                    "SELECT COUNT(*) \
                     FROM links l \
                     JOIN pages fp ON fp.id = l.from_page_id \
                     WHERE fp.is_latest = 1 AND l.to_page_id IS NULL",
                )?,
                stale_links_from_latest_pages: count(
                    conn,
                    "SELECT COUNT(*) \
                     FROM links l \
                     JOIN pages fp ON fp.id = l.from_page_id \
                     LEFT JOIN pages tp ON tp.id = l.to_page_id \
                     WHERE fp.is_latest = 1 \
                       AND l.to_page_id IS NOT NULL \
                       AND COALESCE(tp.is_latest, 0) != 1",
                )?,
            })
        })
        .await
    }

    /// Return aggregate counts scoped to one project.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn status_counts_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<StatusCounts> {
        self.with_conn(move |conn| {
            let pages_latest = count_project(
                conn,
                "SELECT COUNT(*) FROM pages WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1",
                workspace_id,
                project_id,
            )?;
            let pages_all = count_project(
                conn,
                "SELECT COUNT(*) FROM pages WHERE workspace_id = ?1 AND project_id = ?2",
                workspace_id,
                project_id,
            )?;
            let sessions = count_project(
                conn,
                "SELECT COUNT(*) FROM sessions WHERE workspace_id = ?1 AND project_id = ?2",
                workspace_id,
                project_id,
            )?;
            let observations = count_project(
                conn,
                "SELECT COUNT(*) FROM observations WHERE workspace_id = ?1 AND project_id = ?2",
                workspace_id,
                project_id,
            )?;
            Ok(StatusCounts {
                pages_latest,
                pages_all,
                sessions,
                observations,
            })
        })
        .await
    }

    /// Return all migration names recorded in the `wiki_migrations` table.
    ///
    /// Used by the wiki migration runner to determine which migrations have
    /// already been applied to this data directory.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn wiki_migration_names(&self) -> StoreResult<Vec<String>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT name FROM wiki_migrations ORDER BY name")?;
            let names = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(names)
        })
        .await
    }

    // ── user lookups ────────────────────────────────────────────────

    /// Hot path for the auth middleware: hash the incoming bearer token,
    /// look up the matching row, and return the user iff their token is
    /// active (`token_expired_at IS NULL`).
    ///
    /// # Errors
    /// Propagates any SQL or pool error. Returns `Ok(None)` when no row
    /// matches the hash (either no such user, or the token was expired).
    pub async fn find_active_user_by_token_hash(
        &self,
        token_hash: [u8; TOKEN_HASH_LEN],
    ) -> StoreResult<Option<User>> {
        self.with_conn(move |conn| crate::users::find_active_user_by_token_hash(conn, &token_hash))
            .await
    }

    /// Look up a user by exact-match username. Used by admin endpoints
    /// that accept username on the wire (`expire`, `revive`,
    /// `rotate-token`).
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn find_user_by_username(&self, username: String) -> StoreResult<Option<User>> {
        self.with_conn(move |conn| crate::users::find_user_by_username(conn, &username))
            .await
    }

    /// Look up a user by id. **Returns even users whose token is expired**
    /// — this is the attribution-display path (a page authored by alice
    /// must still render "alice" after her token has been expired).
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn find_user_by_id(&self, id: UserId) -> StoreResult<Option<User>> {
        self.with_conn(move |conn| crate::users::find_user_by_id(conn, id))
            .await
    }

    /// All registered users, ordered by `created_at` ascending. Includes
    /// users whose token is expired (the CLI surfaces the active/expired
    /// flag from `token_expired_at`).
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn list_users(&self) -> StoreResult<Vec<User>> {
        self.with_conn(crate::users::list_users).await
    }

    /// Return whether any user row exists, including expired users. This cheap
    /// predicate is used at the admin authorization boundary to switch from
    /// bootstrap compatibility mode to root-only multi-user administration.
    ///
    /// # Errors
    /// Propagates any SQL or pool error so callers can fail closed.
    pub async fn users_exist(&self) -> StoreResult<bool> {
        self.with_conn(crate::users::users_exist).await
    }

    /// Does this deployment tell its operators apart?
    ///
    /// Two independent routes produce distinct operators: rows in `users`, and
    /// usernames asserted by a trusted authenticating proxy. Only the first is
    /// visible from the store, so the caller passes the second in — a
    /// proxy-only deployment reports `users_exist() == false` forever.
    ///
    /// One notion, several gates: the admin authorization boundaries and the
    /// per-author bucketing of pending auto-improve proposals all key on this,
    /// so a single-operator server keeps the exact behaviour it had before
    /// either route existed.
    ///
    /// # Errors
    /// Propagates any SQL or pool error so callers can fail closed.
    pub async fn distinguishes_operators(&self, trusted_proxy_identity: bool) -> StoreResult<bool> {
        // Short-circuits the DB round-trip when the static config bit already
        // settles it. The users table is otherwise consulted per call rather
        // than cached, so committing a first user tightens access without a
        // restart.
        if trusted_proxy_identity {
            return Ok(true);
        }
        self.users_exist().await
    }

    /// Return the last successful global maintenance completion for `job`.
    pub async fn maintenance_job_last_success(
        &self,
        job: MaintenanceJob,
    ) -> StoreResult<Option<i64>> {
        self.with_conn(move |conn| crate::maintenance::last_success(conn, job))
            .await
    }
}

/// Map a `(workspace, project, path, title, kind)` row to a [`HealthPage`].
fn health_page_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HealthPage> {
    Ok(HealthPage {
        workspace: row.get(0)?,
        project: row.get(1)?,
        path: row.get(2)?,
        title: row.get(3)?,
        kind: row.get(4)?,
    })
}

fn page_meta_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoreResult<PageMeta>> {
    let workspace_name: String = row.get(0)?;
    let project_name: String = row.get(1)?;
    let ws_id_bytes: Vec<u8> = row.get(2)?;
    let proj_id_bytes: Vec<u8> = row.get(3)?;
    let path: String = row.get(4)?;
    let title: String = row.get(5)?;
    let kind: String = row.get(6)?;
    let tier: String = row.get(7)?;
    let pinned: i64 = row.get(8)?;
    let created_us: i64 = row.get(9)?;
    let updated_us: i64 = row.get(10)?;
    let supersedes: Option<String> = row.get(11)?;
    // P1.7: author columns LEFT JOIN'd from `users`. NULL when the
    // page was written anonymously / by root, or when the user row
    // has been hard-deleted (FK `ON DELETE SET NULL`).
    let author_username: Option<String> = row.get(12)?;
    let author_name: Option<String> = row.get(13)?;
    let author_email: Option<String> = row.get(14)?;
    let expires_us: Option<i64> = row.get(15)?;
    let author = author_username.map(|username| PageAuthor {
        username,
        name: author_name,
        email: author_email,
    });

    let workspace_id = WorkspaceId::from_slice(&ws_id_bytes)
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(2, 0))?;
    let project_id = ProjectId::from_slice(&proj_id_bytes)
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(3, 0))?;

    let created_at = jiff::Timestamp::from_microsecond(created_us)
        .map(|ts| ts.to_string())
        .unwrap_or_default();
    let updated_at = jiff::Timestamp::from_microsecond(updated_us)
        .map(|ts| ts.to_string())
        .unwrap_or_default();
    let expired = expires_us.is_some_and(|us| us <= now_us());
    let expires_at = expires_us
        .and_then(|us| jiff::Timestamp::from_microsecond(us).ok())
        .map(|ts| ts.to_string());

    Ok(Ok(PageMeta {
        workspace_name,
        project_name,
        workspace_id,
        project_id,
        path,
        title,
        kind,
        tier,
        pinned: pinned != 0,
        created_at,
        updated_at,
        supersedes,
        author,
        expires_at,
        expired,
    }))
}

fn row_to_observation(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoreResult<Observation>> {
    let id_bytes: Vec<u8> = row.get(0)?;
    let session_bytes: Vec<u8> = row.get(1)?;
    let workspace_bytes: Vec<u8> = row.get(2)?;
    let project_bytes: Vec<u8> = row.get(3)?;
    let kind_str: String = row.get(4)?;
    let extension: Option<String> = row.get(5)?;
    let source_event: Option<String> = row.get(6)?;
    let title: String = row.get(7)?;
    let body: String = row.get(8)?;
    let importance: i64 = row.get(9)?;
    let created_us: i64 = row.get(10)?;
    Ok(materialise_observation(
        id_bytes,
        session_bytes,
        workspace_bytes,
        project_bytes,
        kind_str,
        extension,
        source_event,
        title,
        body,
        importance,
        created_us,
    ))
}

#[allow(clippy::too_many_arguments)]
fn materialise_observation(
    id_bytes: Vec<u8>,
    session_bytes: Vec<u8>,
    workspace_bytes: Vec<u8>,
    project_bytes: Vec<u8>,
    kind_str: String,
    extension: Option<String>,
    source_event: Option<String>,
    title: String,
    body: String,
    importance: i64,
    created_us: i64,
) -> StoreResult<Observation> {
    Ok(Observation {
        id: ObservationId::from_slice(&id_bytes)?,
        session_id: SessionId::from_slice(&session_bytes)?,
        workspace_id: WorkspaceId::from_slice(&workspace_bytes)?,
        project_id: ProjectId::from_slice(&project_bytes)?,
        kind: kind_str
            .parse::<ObservationKind>()
            .map_err(StoreError::from)?,
        extension,
        source_event,
        title,
        body,
        importance: u8::try_from(importance.clamp(1, 10)).unwrap_or(1),
        created_at: jiff::Timestamp::from_microsecond(created_us).map_err(|e| {
            StoreError::Memory(ai_memory_core::MemoryError::MalformedRecord(format!(
                "bad timestamp: {e}"
            )))
        })?,
    })
}

/// One stored embedding row, materialised for the vector path.
#[derive(Debug, Clone)]
pub struct StoredEmbedding {
    /// Page identifier (always the `is_latest=1` row's id).
    pub id: PageId,
    /// Relative wiki path.
    pub path: PagePath,
    /// Unit-normalised vector.
    pub vector: Vec<f32>,
}

fn score_desc(a: &(PageId, PagePath, f32), b: &(PageId, PagePath, f32)) -> std::cmp::Ordering {
    b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal)
}

fn dot_embedding_bytes(query: &[f32], bytes: &[u8], dim: u32) -> StoreResult<f32> {
    let dim = dim as usize;
    if query.len() != dim {
        return Err(StoreError::Memory(
            ai_memory_core::MemoryError::MalformedRecord(format!(
                "query vector dim {} != expected {}",
                query.len(),
                dim
            )),
        ));
    }
    let expected = dim * 4;
    if bytes.len() != expected {
        return Err(StoreError::Memory(
            ai_memory_core::MemoryError::MalformedRecord(format!(
                "embedding bytes {} != expected {}",
                bytes.len(),
                expected
            )),
        ));
    }
    Ok(query
        .iter()
        .zip(bytes.chunks_exact(4))
        .map(|(q, chunk)| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) * q)
        .sum())
}

fn bytes_to_f32_vec(bytes: &[u8], dim: u32) -> StoreResult<Vec<f32>> {
    let expected = (dim as usize) * 4;
    if bytes.len() != expected {
        return Err(StoreError::Memory(
            ai_memory_core::MemoryError::MalformedRecord(format!(
                "embedding bytes {} != expected {}",
                bytes.len(),
                expected
            )),
        ));
    }
    let mut out = Vec::with_capacity(dim as usize);
    for chunk in bytes.chunks_exact(4) {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(out)
}

/// Pack a `&[f32]` into little-endian bytes for storage. Inverse of
/// [`bytes_to_f32_vec`].
#[must_use]
pub fn f32_vec_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// One row's worth of input for the M8 retention formula.
#[derive(Debug, Clone, Serialize)]
pub struct DecayCandidate {
    /// Stable identifier.
    pub id: PageId,
    /// Relative wiki path.
    pub path: PagePath,
    /// Tier (the sweep only considers `episodic`).
    pub tier: ai_memory_core::Tier,
    /// Pinned flag — true means "never decay".
    pub pinned: bool,
    /// `updated_at` in microseconds since epoch.
    pub updated_at_us: i64,
    /// Total query/access hits.
    pub access_count: u32,
    /// `last_accessed_at` in microseconds since epoch, or `None` if never accessed.
    pub last_accessed_at_us: Option<i64>,
    /// Frontmatter JSON; the sweep peeks at it for an explicit
    /// `pinned: true` (which overrides the schema flag).
    pub frontmatter_json: String,
    /// TTL instant in microseconds since epoch. The sweep hard-deletes
    /// pages past this regardless of tier or pin.
    pub expires_at_us: Option<i64>,
    /// Per-page salience once explicit feedback has moved it (V37);
    /// `None` means "use `DecayParams::salience_default`".
    pub salience: Option<f64>,
}

fn row_to_decay_candidate(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<StoreResult<DecayCandidate>> {
    let id_bytes: Vec<u8> = row.get(0)?;
    let path: String = row.get(1)?;
    let tier_str: String = row.get(2)?;
    let pinned: i64 = row.get(3)?;
    let updated_at_us: i64 = row.get(4)?;
    let access_count: i64 = row.get(5)?;
    let last_accessed_at_us: Option<i64> = row.get(6)?;
    let frontmatter_json: String = row.get(7)?;
    let expires_at_us: Option<i64> = row.get(8)?;
    let salience: Option<f64> = row.get(9)?;
    Ok(materialise_decay_candidate(
        id_bytes,
        path,
        tier_str,
        pinned,
        updated_at_us,
        access_count,
        last_accessed_at_us,
        frontmatter_json,
        expires_at_us,
        salience,
    ))
}

#[allow(clippy::too_many_arguments)]
fn materialise_decay_candidate(
    id_bytes: Vec<u8>,
    path: String,
    tier_str: String,
    pinned: i64,
    updated_at_us: i64,
    access_count: i64,
    last_accessed_at_us: Option<i64>,
    frontmatter_json: String,
    expires_at_us: Option<i64>,
    salience: Option<f64>,
) -> StoreResult<DecayCandidate> {
    Ok(DecayCandidate {
        id: PageId::from_slice(&id_bytes)?,
        path: PagePath::new(path)?,
        tier: tier_str
            .parse::<ai_memory_core::Tier>()
            .map_err(StoreError::from)?,
        pinned: pinned != 0,
        updated_at_us,
        access_count: u32::try_from(access_count.max(0)).unwrap_or(u32::MAX),
        last_accessed_at_us,
        frontmatter_json,
        expires_at_us,
        salience,
    })
}

fn auto_improve_group_counts(
    conn: &Connection,
    column: &str,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    since_created_at: i64,
    extra_where: Option<&str>,
) -> rusqlite::Result<Vec<AutoImproveTelemetryCount>> {
    let learning_filter = "kind NOT IN ('curator_report', 'auto_improve_report')";
    let filter = extra_where.unwrap_or(learning_filter);
    let sql = format!(
        "SELECT COALESCE({column}, '(none)') AS key, COUNT(*) AS count
         FROM auto_improve_proposals
         WHERE workspace_id = ?1 AND project_id = ?2 AND staged_at >= ?3 AND {filter}
         GROUP BY key ORDER BY count DESC, key ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            since_created_at
        ],
        telemetry_count_from_row,
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn auto_improve_rejection_group_counts(
    conn: &Connection,
    expr: &str,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    since_created_at: i64,
    limit: i64,
) -> rusqlite::Result<Vec<AutoImproveTelemetryCount>> {
    let sql = format!(
        "SELECT {expr} AS key, COUNT(*) AS count
         FROM auto_improve_rejections r
         LEFT JOIN auto_improve_proposals p ON p.id = r.source_proposal_id
         WHERE r.workspace_id = ?1 AND r.project_id = ?2 AND r.created_at >= ?3
           AND (r.source_proposal_id IS NULL OR COALESCE(p.kind, '') NOT IN ('curator_report', 'auto_improve_report'))
         GROUP BY key ORDER BY count DESC, key ASC LIMIT ?4"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            since_created_at,
            limit
        ],
        telemetry_count_from_row,
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn auto_improve_repeated_rejection_fingerprints(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    since_created_at: i64,
    limit: i64,
) -> rusqlite::Result<Vec<AutoImproveTelemetryCount>> {
    let mut stmt = conn.prepare(
        "SELECT r.normalized_fingerprint AS key, COUNT(*) AS count
         FROM auto_improve_rejections r
         LEFT JOIN auto_improve_proposals p ON p.id = r.source_proposal_id
         WHERE r.workspace_id = ?1 AND r.project_id = ?2 AND r.created_at >= ?3
           AND (r.source_proposal_id IS NULL OR COALESCE(p.kind, '') NOT IN ('curator_report', 'auto_improve_report'))
         GROUP BY normalized_fingerprint HAVING count > 1
         ORDER BY count DESC, key ASC LIMIT ?4",
    )?;
    let rows = stmt.query_map(
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            since_created_at,
            limit
        ],
        telemetry_count_from_row,
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn telemetry_count_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<AutoImproveTelemetryCount> {
    let count: i64 = row.get(1)?;
    Ok(AutoImproveTelemetryCount {
        key: row.get(0)?,
        count: usize::try_from(count).unwrap_or(usize::MAX),
    })
}

/// Normalize a cwd for comparison: treat Windows backslashes as path
/// separators and trim trailing separators while keeping a bare root as `/`.
/// Pure and string-only; it does not touch the filesystem.
pub(crate) fn normalize_cwd(p: &str) -> std::borrow::Cow<'_, str> {
    let replaced = if p.contains('\\') {
        std::borrow::Cow::Owned(p.replace('\\', "/"))
    } else {
        std::borrow::Cow::Borrowed(p)
    };
    let trimmed = replaced.trim_end_matches('/');
    if trimmed.is_empty() {
        std::borrow::Cow::Borrowed("/")
    } else if trimmed.len() == replaced.len() {
        replaced
    } else {
        std::borrow::Cow::Owned(trimmed.to_string())
    }
}

/// True when `descendant` is the same directory as `ancestor` or nested
/// below it, with component-boundary awareness: `/repo` contains
/// `/repo/api` but neither `/repo-other` nor `/repository`. Inputs are
/// normalized first. Pure; no SQL `LIKE`, so `%`/`_` in a path cannot act
/// as wildcards.
pub(crate) fn cwd_within(ancestor: &str, descendant: &str) -> bool {
    let a = normalize_cwd(ancestor);
    let d = normalize_cwd(descendant);
    if a == d {
        return true;
    }
    if a == "/" {
        // Root is an ancestor of every other absolute path.
        return d.starts_with('/') && d.len() > 1;
    }
    // Byte-prefix plus a `/` boundary right after it. `starts_with` is
    // byte-exact and `/` is ASCII, so this is UTF-8 safe.
    d.starts_with(a.as_ref()) && d.as_bytes().get(a.len()) == Some(&b'/')
}

/// SQL fragment restricting a handoff query to what `filter` can actually see,
/// plus the owner name it expects bound at `?{param_index}` — the first free
/// positional parameter of the statement it is spliced into.
///
/// The count and the fetch must agree: a briefing that advertises a pending
/// baton the same caller cannot retrieve is worse than no count at all, because
/// the agent keeps asking for something that will always come back empty.
///
/// The name is BOUND, never interpolated. It is not a validated identifier: on a
/// server behind a trusted proxy it is whatever `X-Memory-Actor-Sub` carried —
/// an OIDC subject the engine never parses — so nothing upstream constrains its
/// characters. Same reason [`slot_visibility_sql`] binds its GLOB.
fn handoff_owner_sql(filter: &OwnerFilter, param_index: usize) -> (String, Option<String>) {
    match filter {
        OwnerFilter::Any => (String::new(), None),
        OwnerFilter::Unattributed => (" AND owner_user IS NULL".to_string(), None),
        OwnerFilter::User(user) => (
            format!(" AND (owner_user IS NULL OR owner_user = ?{param_index})"),
            Some(user.clone()),
        ),
    }
}

/// SQL predicate selecting the slot pages `visibility` admits over a `path`
/// column, plus the `GLOB` pattern it expects bound at `?{param_index}` — the
/// first free positional parameter of the statement it is spliced into.
///
/// One rule, one implementation: the session brief and every briefing snapshot
/// go through this, so a viewer predicate cannot be right on one read path and
/// missing on the next. Note what [`SlotVisibility::All`] produces — the bare
/// pre-feature pattern, with no nesting exclusion — because with `[slots]
/// per_user` off a page like `_slots/backend/context.md` is an ordinary shared
/// slot that a brief must keep showing.
fn slot_visibility_sql(
    visibility: &ai_memory_core::SlotVisibility,
    param_index: usize,
) -> (String, Option<String>) {
    if !visibility.hides_other_namespaces() {
        return ("path GLOB '_slots/*'".to_string(), None);
    }
    match visibility.own_namespace() {
        Some(user) => (
            format!(
                "path GLOB '_slots/*' AND (path NOT GLOB '_slots/*/*' OR path GLOB ?{param_index})"
            ),
            Some(format!("{}{user}/*", ai_memory_core::SLOT_PREFIX)),
        ),
        // Nobody to attribute the request to: the shared slots, and nothing
        // that belongs to a named operator.
        None => (
            "path GLOB '_slots/*' AND path NOT GLOB '_slots/*/*'".to_string(),
            None,
        ),
    }
}

/// The same visibility rule as [`slot_visibility_sql`], phrased for a query
/// that selects pages GENERALLY rather than slots specifically.
///
/// The briefing queries return two lists: a `slots` array, filtered by
/// [`slot_visibility_sql`], and a `recent_pages` pointer list of every recently
/// touched page. Filtering only the first leaks the second: a personal slot is
/// still a page, so another operator's `_slots/<user>/…` path and title arrive
/// in the pointer list and get rendered into the session brief. Bodies stay
/// withheld, but a path and a title are already somebody's working context.
///
/// So this admits everything that is not a slot at all, plus the slots the
/// viewer may see — i.e. it excludes exactly the namespaced slots belonging to
/// somebody else. With the feature off it admits everything, so the pointer
/// list is byte-identical to what it was before slots could be namespaced.
fn slot_exclusion_sql(
    visibility: &ai_memory_core::SlotVisibility,
    param_index: usize,
) -> (String, Option<String>) {
    if !visibility.hides_other_namespaces() {
        return ("1".to_string(), None);
    }
    match visibility.own_namespace() {
        Some(user) => (
            format!("(path NOT GLOB '_slots/*/*' OR path GLOB ?{param_index})"),
            Some(format!("{}{user}/*", ai_memory_core::SLOT_PREFIX)),
        ),
        // Nobody to attribute the request to: no namespaced slot is theirs.
        None => ("path NOT GLOB '_slots/*/*'".to_string(), None),
    }
}

fn is_handoff_candidate(h: &Handoff, cwd_filter: Option<&str>, owner_filter: &OwnerFilter) -> bool {
    // Ownership is checked FIRST, before the manual short-circuit below.
    // Order matters: a manual handoff is project-wide by cwd, so checking the
    // owner afterwards would let one operator's `memory_handoff_begin` be
    // claimed by the next session to start, whoever it belongs to — the exact
    // cross-operator mixing this filter exists to stop. Handoffs with no owner
    // (every pre-V39 row, and anything written without an actor) stay visible
    // to everyone, which preserves single-operator behaviour untouched.
    if !owner_filter.admits(h.owner_user.as_deref()) {
        return false;
    }
    // Manual handoffs (memory_handoff_begin always sets from_session_id = None)
    // are project-wide and always candidates, whatever cwd they carry. Only auto
    // SessionEnd handoffs are cwd-path-boundary scoped. This makes "a manual
    // handoff always beats the auto one" deterministic on from_session_id,
    // instead of relying on a manual handoff happening to have a NULL cwd.
    if h.from_session_id.is_none() {
        return true;
    }
    auto_handoff_matches_cwd(h.cwd.as_deref(), cwd_filter)
}

/// Whether an automatic handoff is eligible for a session starting in
/// `cwd_filter`. Shared with the acceptance path so selection and supersession
/// apply exactly the same path-boundary rule.
pub(crate) fn auto_handoff_matches_cwd(
    handoff_cwd: Option<&str>,
    cwd_filter: Option<&str>,
) -> bool {
    match (cwd_filter, handoff_cwd) {
        // Project-wide read: every open handoff is a candidate.
        (None, _) => true,
        // No stored cwd: treat as project-wide.
        (_, None) => true,
        // cwd-bearing auto handoffs match by path-boundary.
        (Some(session_cwd), Some(handoff_cwd)) => cwd_within(handoff_cwd, session_cwd),
    }
}

pub(crate) fn handoff_selection_key(
    manual: bool,
    created_at_micros: i64,
    cwd: Option<&str>,
    id: HandoffId,
) -> (bool, i64, usize, [u8; 16]) {
    // Cwd specificity discriminates only equal-time AUTO handoffs. For manual
    // handoffs it must be neutral so the newest manual wins, not whichever
    // happens to carry the longest cwd. Normalize before counting so legacy
    // `/repo/` rows do not outrank equivalent `/repo` rows.
    let auto_specificity = if manual {
        0
    } else {
        cwd.map_or(0, |cwd| normalize_cwd(cwd).len())
    };
    (
        manual,            // manual beats auto
        created_at_micros, // newest
        auto_specificity,  // most specific equal-time auto cwd
        *id.as_bytes(),    // deterministic final tie-break
    )
}

fn prefer_handoff(a: &Handoff, b: &Handoff) -> std::cmp::Ordering {
    handoff_selection_key(
        a.from_session_id.is_none(),
        a.created_at.as_microsecond(),
        a.cwd.as_deref(),
        a.id,
    )
    .cmp(&handoff_selection_key(
        b.from_session_id.is_none(),
        b.created_at.as_microsecond(),
        b.cwd.as_deref(),
        b.id,
    ))
}

/// Pick the handoff to deliver from a project's open handoffs.
///
/// See [`ReaderPool::latest_open_handoff`] for the full contract: manual handoffs
/// are project-wide, auto handoffs are filtered by cwd path-boundary, and a
/// manual handoff always beats an auto one, then newest, with cwd specificity
/// only breaking timestamp ties.
#[cfg(test)]
fn select_open_handoff(candidates: Vec<Handoff>, cwd_filter: Option<&str>) -> Option<Handoff> {
    candidates
        .into_iter()
        .filter(|h| is_handoff_candidate(h, cwd_filter, &OwnerFilter::Any))
        .max_by(prefer_handoff)
}

/// Build a [`Handoff`] from a row selected with [`HANDOFF_COLUMNS`].
///
/// Reading the row here (instead of destructuring it into a long positional
/// argument list) keeps the column order defined in exactly one place next to
/// the `SELECT` that produces it.
fn row_to_handoff(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoreResult<Handoff>> {
    let id_bytes: Vec<u8> = row.get(0)?;
    let ws_bytes: Vec<u8> = row.get(1)?;
    let pj_bytes: Vec<u8> = row.get(2)?;
    let from_session_bytes: Option<Vec<u8>> = row.get(3)?;
    let from_agent: String = row.get(4)?;
    let to_agent: Option<String> = row.get(5)?;
    let cwd: Option<String> = row.get(6)?;
    let summary: String = row.get(7)?;
    let open_q_json: String = row.get(8)?;
    let next_s_json: String = row.get(9)?;
    let files_json: String = row.get(10)?;
    let state: String = row.get(11)?;
    let created_us: i64 = row.get(12)?;
    let accepted_by: Option<String> = row.get(13)?;
    let accepted_at_us: Option<i64> = row.get(14)?;
    let accepted_by_session_bytes: Option<Vec<u8>> = row.get(15)?;
    let owner_user: Option<String> = row.get(16)?;
    let accepted_by_user: Option<String> = row.get(17)?;

    Ok((|| {
        let open_questions: Vec<String> = serde_json::from_str(&open_q_json)?;
        let next_steps: Vec<String> = serde_json::from_str(&next_s_json)?;
        let files_touched: Vec<String> = serde_json::from_str(&files_json)?;
        let from_session = from_session_bytes
            .as_deref()
            .map(SessionId::from_slice)
            .transpose()?;
        let accepted_session = accepted_by_session_bytes
            .as_deref()
            .map(SessionId::from_slice)
            .transpose()?;
        Ok(Handoff {
            id: HandoffId::from_slice(&id_bytes)?,
            workspace_id: WorkspaceId::from_slice(&ws_bytes)?,
            project_id: ProjectId::from_slice(&pj_bytes)?,
            from_session_id: from_session,
            from_agent: parse_agent(&from_agent),
            to_agent: to_agent.as_deref().map(parse_agent),
            cwd,
            summary,
            open_questions,
            next_steps,
            files_touched,
            state: state.parse::<HandoffState>().map_err(StoreError::from)?,
            created_at: jiff::Timestamp::from_microsecond(created_us).map_err(|e| {
                StoreError::Memory(ai_memory_core::MemoryError::MalformedRecord(format!(
                    "bad created_at: {e}"
                )))
            })?,
            accepted_by: accepted_by.as_deref().map(parse_agent),
            accepted_at: accepted_at_us
                .map(jiff::Timestamp::from_microsecond)
                .transpose()
                .map_err(|e| {
                    StoreError::Memory(ai_memory_core::MemoryError::MalformedRecord(format!(
                        "bad accepted_at: {e}"
                    )))
                })?,
            accepted_by_session: accepted_session,
            owner_user,
            accepted_by_user,
        })
    })())
}

fn parse_agent(s: &str) -> AgentKind {
    AgentKind::from_wire(s)
}

fn count(conn: &Connection, sql: &str) -> StoreResult<u64> {
    count_bound(conn, sql, &[])
}

/// `COUNT(*)` with an explicit parameter list.
///
/// The scope-less [`count`] and the scoped `count_project` / `count_workspace`
/// helpers each bind a fixed shape; a predicate spliced in by the caller (the
/// handoff owner filter) adds one more parameter to whichever shape it lands in,
/// so it needs a helper that takes the whole list.
fn count_bound(conn: &Connection, sql: &str, params: &[&dyn rusqlite::ToSql]) -> StoreResult<u64> {
    let n: Option<i64> = conn.query_row(sql, params, |row| row.get(0)).optional()?;
    Ok(u64::try_from(n.unwrap_or(0)).unwrap_or(0))
}

fn count_project(
    conn: &Connection,
    sql: &str,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> StoreResult<u64> {
    let n: Option<i64> = conn
        .query_row(
            sql,
            params![workspace_id.as_bytes(), project_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(u64::try_from(n.unwrap_or(0)).unwrap_or(0))
}

/// Cross-project link degree for a project: `(dependents, dependencies)`.
/// `dependents` = distinct other projects whose pages link into this one;
/// `dependencies` = distinct other projects this one links out to. Counts
/// resolved links only (`to_page_id` is set); project ids are globally
/// unique, so a bare `!= project_id` excludes self across all workspaces.
fn cross_project_degree(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> StoreResult<(u64, u64)> {
    let dependents: Option<i64> = conn
        .query_row(
            "SELECT COUNT(DISTINCT fp.project_id) \
             FROM links l \
             JOIN pages tp ON tp.id = l.to_page_id \
                 AND tp.workspace_id = ?1 AND tp.project_id = ?2 AND tp.is_latest = 1 \
             JOIN pages fp ON fp.id = l.from_page_id AND fp.is_latest = 1 \
             WHERE fp.project_id != ?2",
            params![workspace_id.as_bytes(), project_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    let dependencies: Option<i64> = conn
        .query_row(
            "SELECT COUNT(DISTINCT tp.project_id) \
             FROM links l \
             JOIN pages fp ON fp.id = l.from_page_id \
                 AND fp.workspace_id = ?1 AND fp.project_id = ?2 AND fp.is_latest = 1 \
             JOIN pages tp ON tp.id = l.to_page_id AND tp.is_latest = 1 \
             WHERE tp.project_id != ?2",
            params![workspace_id.as_bytes(), project_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    Ok((
        u64::try_from(dependents.unwrap_or(0)).unwrap_or(0),
        u64::try_from(dependencies.unwrap_or(0)).unwrap_or(0),
    ))
}

fn count_workspace(conn: &Connection, sql: &str, workspace_id: WorkspaceId) -> StoreResult<u64> {
    let n: Option<i64> = conn
        .query_row(sql, params![workspace_id.as_bytes()], |row| row.get(0))
        .optional()?;
    Ok(u64::try_from(n.unwrap_or(0)).unwrap_or(0))
}

fn normalize_fts_query(query: &str) -> String {
    // Delegates to prepare_fts5_query: neutralises `word:` column syntax and
    // quotes tokens so `-` / `*` are not FTS5 operators.
    prepare_fts5_query(query)
}

/// Count rows in a time-bounded window. Used by [`ReaderPool::briefing`]
/// to compute "last 7 days" / "last 30 days" activity slices.
fn window_activity(conn: &Connection, days: u32, cutoff_us: i64) -> StoreResult<ActivityWindow> {
    let count_since = |sql: &str| -> StoreResult<u64> {
        let n: Option<i64> = conn
            .query_row(sql, params![cutoff_us], |row| row.get(0))
            .optional()?;
        Ok(u64::try_from(n.unwrap_or(0)).unwrap_or(0))
    };
    Ok(ActivityWindow {
        days,
        // `sessions` schema uses `started_at`, not `created_at` — easy
        // to forget because the other tables all use `created_at`.
        sessions: count_since("SELECT COUNT(*) FROM sessions WHERE started_at > ?1")?,
        observations: count_since("SELECT COUNT(*) FROM observations WHERE created_at > ?1")?,
        pages_updated: count_since(
            "SELECT COUNT(*) FROM pages WHERE is_latest = 1 AND updated_at > ?1",
        )?,
    })
}

fn window_activity_project(
    conn: &Connection,
    days: u32,
    cutoff_us: i64,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> StoreResult<ActivityWindow> {
    let count_since = |sql: &str| -> StoreResult<u64> {
        let n: Option<i64> = conn
            .query_row(
                sql,
                params![workspace_id.as_bytes(), project_id.as_bytes(), cutoff_us],
                |row| row.get(0),
            )
            .optional()?;
        Ok(u64::try_from(n.unwrap_or(0)).unwrap_or(0))
    };
    Ok(ActivityWindow {
        days,
        sessions: count_since(
            "SELECT COUNT(*) FROM sessions WHERE workspace_id = ?1 AND project_id = ?2 AND started_at > ?3",
        )?,
        observations: count_since(
            "SELECT COUNT(*) FROM observations WHERE workspace_id = ?1 AND project_id = ?2 AND created_at > ?3",
        )?,
        pages_updated: count_since(
            "SELECT COUNT(*) FROM pages WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1 AND updated_at > ?3",
        )?,
    })
}

fn window_activity_workspace(
    conn: &Connection,
    days: u32,
    cutoff_us: i64,
    workspace_id: WorkspaceId,
) -> StoreResult<ActivityWindow> {
    let count_since = |sql: &str| -> StoreResult<u64> {
        let n: Option<i64> = conn
            .query_row(sql, params![workspace_id.as_bytes(), cutoff_us], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(u64::try_from(n.unwrap_or(0)).unwrap_or(0))
    };
    Ok(ActivityWindow {
        days,
        sessions: count_since(
            "SELECT COUNT(*) FROM sessions WHERE workspace_id = ?1 AND started_at > ?2",
        )?,
        observations: count_since(
            "SELECT COUNT(*) FROM observations WHERE workspace_id = ?1 AND created_at > ?2",
        )?,
        pages_updated: count_since(
            "SELECT COUNT(*) FROM pages WHERE workspace_id = ?1 AND is_latest = 1 AND updated_at > ?2",
        )?,
    })
}

/// Materialise one row from the briefing's recent-pages / rules queries
/// into a [`BriefingPage`]. The row shape is `(path, title, kind,
/// updated_at_us)` — all queries above MUST select those columns in
/// that order.
fn briefing_page_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoreResult<BriefingPage>> {
    let path: String = row.get(0)?;
    let title: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let updated_us: i64 = row.get(3)?;
    Ok(jiff::Timestamp::from_microsecond(updated_us)
        .map(|ts| BriefingPage {
            path,
            title,
            kind,
            updated_at: ts.to_string(),
        })
        .map_err(|e| {
            StoreError::Memory(ai_memory_core::MemoryError::MalformedRecord(format!(
                "bad updated_at: {e}"
            )))
        }))
}

/// Reject cwds that can't safely participate in a `repo_path` prefix
/// match. Trailing slash is already trimmed by the caller; this catches:
/// empty / single-slash / dot-segments (a `/foo/../bar` resolved-by-LIKE
/// could match a stored `/foo` parent the cwd doesn't logically belong
/// to, since LIKE doesn't normalise paths). Treats any failure as "no
/// match" so the caller falls through to the safe create-by-basename path.
fn is_safe_cwd_for_prefix_match(cwd: &str) -> bool {
    if cwd.is_empty() || cwd == "/" {
        return false;
    }
    for segment in cwd.split('/') {
        if segment == "." || segment == ".." {
            return false;
        }
    }
    true
}

fn has_trailing_path_separator(path: &str) -> bool {
    path.len() > 1 && path.ends_with(['/', '\\'])
}

fn checkout(inner: &Inner) -> StoreResult<Connection> {
    if let Some(conn) = inner.pool.lock().pop() {
        return Ok(conn);
    }
    open_read_only(&inner.db_path)
}

fn checkin(inner: &Inner, conn: Connection) {
    let mut pool = inner.pool.lock();
    if pool.len() < inner.soft_cap {
        pool.push(conn);
    }
}

fn open_read_only(path: &Path) -> StoreResult<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags)?;
    conn.pragma_update(None, "busy_timeout", 5_000)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::{entity_query_tokens, like_escape};
    use crate::Store;

    use ai_memory_core::{
        AgentKind, Handoff, HandoffId, HandoffState, NewHandoff, NewSession, ProjectId, SessionId,
        WorkspaceId,
    };

    #[test]
    fn authority_candidate_window_is_bounded_and_saturating() {
        use super::authority_candidate_limit;

        assert_eq!(authority_candidate_limit(0), 0);
        assert_eq!(authority_candidate_limit(1), 20);
        assert_eq!(authority_candidate_limit(10), 40);
        assert_eq!(authority_candidate_limit(1_000), 1_300);
        assert_eq!(authority_candidate_limit(usize::MAX), usize::MAX);
    }

    /// Build an open handoff for the pure-selection tests. `manual` toggles
    /// the manual (`from_session_id == None`) vs auto distinction; `t` is the
    /// creation time in microseconds; `summary` doubles as a label to assert
    /// which handoff won.
    fn handoff(summary: &str, cwd: Option<&str>, manual: bool, t: i64) -> Handoff {
        Handoff {
            id: HandoffId::new(),
            workspace_id: WorkspaceId::new(),
            project_id: ProjectId::new(),
            from_session_id: if manual { None } else { Some(SessionId::new()) },
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: cwd.map(str::to_string),
            summary: summary.to_string(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            state: HandoffState::Open,
            created_at: jiff::Timestamp::from_microsecond(t).unwrap(),
            accepted_by: None,
            accepted_at: None,
            accepted_by_session: None,
            owner_user: None,
            accepted_by_user: None,
        }
    }

    fn pick(candidates: Vec<Handoff>, cwd: Option<&str>) -> String {
        super::select_open_handoff(candidates, cwd).map_or_else(|| "—".to_string(), |h| h.summary)
    }

    #[test]
    fn cwd_within_respects_component_boundaries() {
        use super::cwd_within;
        assert!(cwd_within("/repo", "/repo"));
        assert!(cwd_within("/repo", "/repo/api"));
        assert!(cwd_within("/repo", "/repo/api/v2"));
        assert!(cwd_within("/repo/", "/repo/api")); // trailing slash normalized
        assert!(cwd_within("/", "/anything/here"));
        assert!(!cwd_within("/repo", "/repo-other")); // sibling, not descendant
        assert!(!cwd_within("/repo", "/repository")); // longer name, not a child
        assert!(!cwd_within("/repo/api", "/repo")); // parent is not within child
    }

    #[test]
    fn cwd_within_handles_windows_backslash_boundaries() {
        use super::cwd_within;
        assert!(cwd_within(r"C:\repo", r"C:\repo\api"));
        assert!(cwd_within(r"C:\repo\", r"C:\repo\api"));
        assert!(!cwd_within(r"C:\repo", r"C:\repo-other"));
    }

    // The realistic scenario matrix (see the cwd the SessionEnd hook injects):
    // a manual handoff is cwd=NULL, an auto handoff carries the session cwd.

    #[test]
    fn handoff_manual_beats_auto_same_dir() {
        // The reported bug: a detailed manual handoff must win over the vague
        // SessionEnd auto handoff even though the auto one is newer.
        let c = vec![
            handoff("MAN-detail", None, true, 1),
            handoff("auto-vague", Some("/repo"), false, 2),
        ];
        assert_eq!(pick(c, Some("/repo")), "MAN-detail");
    }

    #[test]
    fn handoff_auto_reaches_subdir() {
        let c = vec![handoff("auto", Some("/repo"), false, 1)];
        assert_eq!(pick(c, Some("/repo/api")), "auto");
    }

    #[test]
    fn handoff_manual_survives_from_subdir() {
        let c = vec![
            handoff("MAN-detail", None, true, 1),
            handoff("auto-vague", Some("/repo"), false, 2),
        ];
        assert_eq!(pick(c, Some("/repo/api")), "MAN-detail");
    }

    #[test]
    fn handoff_tolerates_trailing_slash_in_session_cwd() {
        let c = vec![handoff("auto", Some("/repo"), false, 1)];
        assert_eq!(pick(c, Some("/repo/")), "auto");
    }

    #[test]
    fn handoff_lone_manual_is_delivered() {
        let c = vec![handoff("MAN", None, true, 1)];
        assert_eq!(pick(c, Some("/repo")), "MAN");
    }

    #[test]
    fn handoff_newest_manual_wins_even_if_older_manual_has_cwd() {
        // An older manual that happens to carry a cwd must NOT outrank the
        // newest manual just because its cwd string is longer. The cwd
        // specificity tiebreak applies only to auto handoffs; among manuals the
        // newest wins. (Regression seen in real data: a cwd-bearing manual was
        // delivered ahead of the newer cwd-less one.)
        let c = vec![
            handoff(
                "old-with-cwd",
                Some("/var/home/luka/Trabalho/waba/bsp-core"),
                true,
                1,
            ),
            handoff("newest-null", None, true, 2),
        ];
        assert_eq!(
            pick(c, Some("/var/home/luka/Trabalho/waba/bsp-core")),
            "newest-null"
        );
    }

    #[test]
    fn handoff_manual_with_nonmatching_cwd_still_beats_auto() {
        // Even if the model passed a cwd on the manual handoff that does not
        // match the next session's dir, the manual handoff is project-wide and
        // still wins over the auto one. Deterministic on from_session_id, not
        // on the manual handoff happening to have a NULL cwd.
        let c = vec![
            handoff("MAN", Some("/some/other/place"), true, 1),
            handoff("auto", Some("/repo"), false, 2),
        ];
        assert_eq!(pick(c, Some("/repo")), "MAN");
    }

    #[test]
    fn handoff_sibling_subdirs_do_not_leak() {
        // A /mono/web session must not receive the /mono/api handoff, even
        // though that sibling handoff is newer.
        let c = vec![
            handoff("web", Some("/mono/web"), false, 1),
            handoff("api", Some("/mono/api"), false, 2),
        ];
        assert_eq!(pick(c, Some("/mono/web")), "web");
    }

    #[test]
    fn handoff_dangerous_prefix_never_matches() {
        let c = vec![handoff("repo", Some("/repo"), false, 1)];
        assert_eq!(pick(c, Some("/repo-other")), "—");
    }

    #[test]
    fn handoff_newer_parent_beats_stale_specific_ancestor() {
        let c = vec![
            handoff("a", Some("/a"), false, 2),
            handoff("ab", Some("/a/b"), false, 1),
        ];
        assert_eq!(pick(c, Some("/a/b/c")), "a");
    }

    #[test]
    fn handoff_legacy_trailing_slash_does_not_beat_newer_equivalent_auto() {
        let c = vec![
            handoff("old-slash", Some("/repo/"), false, 1),
            handoff("new-no-slash", Some("/repo"), false, 2),
        ];
        assert_eq!(pick(c, Some("/repo/api")), "new-no-slash");
    }

    #[test]
    fn handoff_wildcard_characters_are_literal_path_components() {
        let c = vec![
            handoff("percent", Some("/repo/%"), false, 2),
            handoff("underscore", Some("/repo/_x"), false, 3),
            handoff("repo", Some("/repo"), false, 1),
        ];
        assert_eq!(pick(c.clone(), Some("/repo/abc")), "repo");
        assert_eq!(pick(c, Some("/repo/ax")), "repo");
    }

    #[test]
    fn handoff_parent_session_does_not_inherit_deep_handoff() {
        // A handoff left deep in /repo/api is not delivered to a /repo session.
        let c = vec![handoff("deep", Some("/repo/api"), false, 1)];
        assert_eq!(pick(c, Some("/repo")), "—");
    }

    #[test]
    fn handoff_none_filter_is_project_wide() {
        // The web overview passes no cwd: every open handoff is a candidate,
        // manual still preferred.
        let c = vec![
            handoff("auto-deep", Some("/repo/api"), false, 1),
            handoff("MAN", None, true, 2),
        ];
        assert_eq!(pick(c, None), "MAN");
    }

    #[tokio::test]
    async fn latest_open_handoff_preserves_workspace_project_isolation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws_a = store.writer.get_or_create_workspace("a").await.unwrap();
        let ws_b = store.writer.get_or_create_workspace("b").await.unwrap();
        let proj_a = store
            .writer
            .get_or_create_project(ws_a, "same-name", None)
            .await
            .unwrap();
        let proj_b = store
            .writer
            .get_or_create_project(ws_b, "same-name", None)
            .await
            .unwrap();

        store
            .writer
            .insert_handoff(NewHandoff {
                workspace_id: ws_b,
                project_id: proj_b,
                from_session_id: None,
                from_agent: AgentKind::ClaudeCode,
                to_agent: None,
                cwd: None,
                summary: "wrong workspace".into(),
                open_questions: vec![],
                next_steps: vec![],
                files_touched: vec![],
                owner_user: None,
            })
            .await
            .unwrap();
        store
            .writer
            .insert_handoff(NewHandoff {
                workspace_id: ws_a,
                project_id: proj_a,
                from_session_id: None,
                from_agent: AgentKind::ClaudeCode,
                to_agent: None,
                cwd: None,
                summary: "right project".into(),
                open_questions: vec![],
                next_steps: vec![],
                files_touched: vec![],
                owner_user: None,
            })
            .await
            .unwrap();

        let handoff = store
            .reader
            .latest_open_handoff(
                ws_a,
                proj_a,
                Some("/repo".into()),
                ai_memory_core::OwnerFilter::Any,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(handoff.summary, "right project");
    }

    #[tokio::test]
    async fn accepting_newest_auto_expires_only_older_matching_autos() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "repo", Some("/repo".into()))
            .await
            .unwrap();

        async fn insert_auto(
            store: &Store,
            workspace_id: WorkspaceId,
            project_id: ProjectId,
            cwd: &str,
            summary: &str,
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
                    summary: summary.into(),
                    open_questions: vec![],
                    next_steps: vec![],
                    files_touched: vec![],
                    owner_user: None,
                })
                .await
                .unwrap()
        }

        let superseded_same_cwd =
            insert_auto(&store, ws, proj, "/repo/api", "older same cwd").await;
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let stale_specific = insert_auto(&store, ws, proj, "/repo/api", "stale specific").await;
        assert_eq!(
            store
                .reader
                .handoff_by_id(superseded_same_cwd)
                .await
                .unwrap()
                .unwrap()
                .state,
            HandoffState::Expired,
            "a newer auto from the exact cwd must bound pre-accept accumulation"
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let newest_parent = insert_auto(&store, ws, proj, "/repo", "newest parent").await;
        let sibling = insert_auto(&store, ws, proj, "/repo/web", "sibling").await;

        let selected = store
            .reader
            .latest_open_handoff(
                ws,
                proj,
                Some("/repo/api/src".into()),
                ai_memory_core::OwnerFilter::Any,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(selected.id, newest_parent, "newest eligible auto must win");

        // A manual handoff appearing between selection and acceptance must not
        // be swept by automatic-handoff cleanup.
        let manual = store
            .writer
            .insert_handoff(NewHandoff {
                workspace_id: ws,
                project_id: proj,
                from_session_id: None,
                from_agent: AgentKind::Codex,
                to_agent: None,
                cwd: None,
                summary: "manual".into(),
                open_questions: vec![],
                next_steps: vec![],
                files_touched: vec![],
                owner_user: None,
            })
            .await
            .unwrap();
        store
            .writer
            .accept_handoff(
                selected.id,
                AgentKind::Codex,
                None,
                None,
                ai_memory_core::OwnerFilter::Any,
                Some("/repo/api/src".into()),
            )
            .await
            .unwrap();

        assert_eq!(
            store
                .reader
                .handoff_by_id(stale_specific)
                .await
                .unwrap()
                .unwrap()
                .state,
            HandoffState::Expired
        );
        assert_eq!(
            store
                .reader
                .handoff_by_id(newest_parent)
                .await
                .unwrap()
                .unwrap()
                .state,
            HandoffState::Accepted
        );
        for untouched in [sibling, manual] {
            assert_eq!(
                store
                    .reader
                    .handoff_by_id(untouched)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                HandoffState::Open
            );
        }
    }

    /// A stored `repo_path` equal to the operator's `$HOME` must never be
    /// prefix-matched: such a row would be a catch-all parent for every
    /// project beneath the home directory. A normal nested repo under
    /// `$HOME` must still match.
    #[tokio::test]
    async fn prefix_match_skips_home_dir_but_keeps_nested_repo() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        // A "$HOME catch-all" project whose repo_path is the home dir.
        store
            .writer
            .get_or_create_project(ws, "home", Some(String::from("/home/tester")))
            .await
            .unwrap();
        // A real nested repo beneath the home dir.
        let app_id = store
            .writer
            .get_or_create_project(ws, "app", Some(String::from("/home/tester/projects/app")))
            .await
            .unwrap();

        // A cwd somewhere under $HOME but not under any real repo must NOT
        // resolve to the $HOME catch-all row.
        let matched = store
            .reader
            .find_project_by_cwd_prefix(
                ws,
                String::from("/home/tester/random/dir"),
                Some("/home/tester"),
            )
            .await
            .unwrap();
        assert_eq!(
            matched, None,
            "a stored repo_path equal to $HOME must never be prefix-matched"
        );

        // A cwd inside a genuine nested repo still resolves to that repo.
        let matched = store
            .reader
            .find_project_by_cwd_prefix(
                ws,
                String::from("/home/tester/projects/app/src"),
                Some("/home/tester"),
            )
            .await
            .unwrap();
        assert_eq!(
            matched.map(|(id, _)| id),
            Some(app_id),
            "a genuine nested repo under $HOME must still prefix-match"
        );
    }

    #[tokio::test]
    async fn prefix_match_treats_percent_and_underscore_as_literal_path_bytes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let literal_id = store
            .writer
            .get_or_create_project(
                ws,
                "literal",
                Some(String::from("/tmp/ai_memory_%literal/repo_under")),
            )
            .await
            .unwrap();

        let matched = store
            .reader
            .find_project_by_cwd_prefix(
                ws,
                String::from("/tmp/ai_memory_%literal/repo_under/src"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(matched.map(|(id, _)| id), Some(literal_id));

        let widened = store
            .reader
            .find_project_by_cwd_prefix(
                ws,
                String::from("/tmp/aiXmemory_Aliteral/repoXunder/src"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(widened, None, "wildcards in repo_path must stay literal");
    }

    #[tokio::test]
    async fn prefix_match_handles_legacy_windows_backslash_repo_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let project_id = ProjectId::new();
        let conn =
            rusqlite::Connection::open(tmp.path().join("db").join(crate::DB_FILENAME)).unwrap();
        conn.execute(
            "INSERT INTO projects (id, workspace_id, name, repo_path, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                project_id.as_bytes(),
                ws.as_bytes(),
                "app",
                r"C:\Users\tester\app",
                jiff::Timestamp::now().as_microsecond()
            ],
        )
        .unwrap();

        let matched = store
            .reader
            .find_project_by_cwd_prefix(
                ws,
                String::from("C:/Users/tester/app/crates/core"),
                Some(r"C:\Users\tester"),
            )
            .await
            .unwrap();

        assert_eq!(matched.map(|(id, _)| id), Some(project_id));
    }

    #[tokio::test]
    async fn prefix_match_ignores_legacy_trailing_separator_repo_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let project_id = ProjectId::new();
        let conn =
            rusqlite::Connection::open(tmp.path().join("db").join(crate::DB_FILENAME)).unwrap();
        conn.execute(
            "INSERT INTO projects (id, workspace_id, name, repo_path, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                project_id.as_bytes(),
                ws.as_bytes(),
                "legacy-trailing",
                "/repo/foo/",
                jiff::Timestamp::now().as_microsecond()
            ],
        )
        .unwrap();

        let matched = store
            .reader
            .find_project_by_cwd_prefix(ws, String::from("/repo/foo/bar"), None)
            .await
            .unwrap();

        assert_eq!(matched, None, "legacy trailing separator must not match");
    }

    #[test]
    fn entity_query_tokens_lowercases_filters_short_and_bounds() {
        // A compound token yields the whole form AND its parts, so a
        // query can spell an entity `writer-actor` or `writer actor`.
        assert_eq!(
            entity_query_tokens("How does the Writer-Actor use SQLite?"),
            vec![
                "how".to_string(),
                "does".to_string(),
                "the".to_string(),
                "writer-actor".to_string(),
                "writer".to_string(),
                "actor".to_string(),
                "use".to_string(),
                "sqlite".to_string(),
            ],
        );
        assert!(
            entity_query_tokens("a of an X").is_empty(),
            "sub-3-char tokens are noise for prefix matching",
        );
        assert_eq!(
            entity_query_tokens("dup dup dup"),
            vec!["dup".to_string()],
            "tokens de-duplicate",
        );
        assert!(entity_query_tokens(&"word ".repeat(50)).len() <= 12);
        assert!(
            entity_query_tokens(&"x".repeat(1_000_000)).is_empty(),
            "an oversized untrusted token must be rejected before lowercase allocation"
        );
        assert_eq!(
            entity_query_tokens(&format!("{} usable", "x".repeat(1_000_000))),
            vec!["usable".to_string()],
            "a rejected compound must not hide later bounded tokens"
        );
    }

    #[test]
    fn like_escape_neutralises_wildcards() {
        // `_` is LIKE's single-char wildcard and appears in real entity
        // names (`writer_actor`), so it must not match `writerxactor`.
        assert_eq!(like_escape("writer_actor"), "writer\\_actor");
        assert_eq!(like_escape("100%"), "100\\%");
        assert_eq!(like_escape("back\\slash"), "back\\\\slash");
        assert_eq!(like_escape("plain-name"), "plain-name");
    }
}
