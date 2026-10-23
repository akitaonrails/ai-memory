//! M8 forget sweep — episodic-only retention pass.
//!
//! Walks the `is_latest = 1` pages for a project, computes the retention score
//! for each via [`ai_memory_store::retention_score_with_breadth`],
//! and evicts those below threshold through the wiki layer: the authoritative
//! Markdown file is removed and the exact latest row becomes a decay
//! tombstone. Semantic / procedural / working tiers are skipped (M8 policy:
//! semantic compounds, only episodic decays). Pinned pages (schema flag OR
//! `pinned: true` in frontmatter) are exempt regardless of tier.
//!
//! TTL pass: pages whose frontmatter `expires_at:` is in the past are
//! hard-deleted through the wiki layer (file + rows), regardless of
//! tier or pin — an explicit expiry is a more explicit user statement
//! than a pin. `memory_lint` warns about pinned+expiring combos so the
//! contradiction is visible before the delete lands.
//!
//! Hard-delete pass cleans up tombstones older than
//! `hard_delete_after_days` and their supersession ancestry. Ordinary
//! supersession rows are safe because only a tombstone (decay eviction, or
//! the watcher's opt-in reconcile-delete safety net, #929 — the two are
//! deliberately indistinguishable here) writes `superseded_at`. A page
//! rewritten at a tombstoned path re-links onto that chain via `supersedes`
//! and clears the old row's `superseded_at` (`ops::upsert_page_in_tx`'s
//! resurrection path), so the old version becomes an ordinary, protected
//! chain member instead of an orphan this pass would otherwise hard-delete —
//! this is what keeps a reconcile tombstone's false positive from being
//! permanently destroyed once the file reappears.
//!
//! Observation prune pass (opt-in, disabled unless
//! `ObservationRetention::days > 0`) deletes raw observations older than the
//! configured age, and only for sessions already distilled into a page that is
//! still live. It runs LAST so both of this run's page deletions are already
//! visible to its predicate: a session whose summary page was evicted or
//! hard-deleted by the passes above keeps its raw capture, because that capture
//! is now the only surviving copy.
//!
//! Event ledger pass (opt-in, disabled unless `LedgerRetention::months > 0`)
//! deletes old hook event ledgers (`log-YYYY-MM.md`, one per project per
//! month) once their month ended at least that many whole calendar months ago.
//! Ledgers are files, not pages: the pass runs through the wiki's dedicated
//! ledger delete (confined, under the mutation guard, re-checking the
//! content gate) and never touches the store. The current and the previous
//! month are never deleted whatever the setting.

use std::collections::{HashMap, HashSet};

use ai_memory_core::{
    ActorContext, PageEvidence, PageEvidenceKind, PageId, PagePath, ProjectId, Tier, WorkspaceId,
};
use ai_memory_store::{
    DecayCandidate, DecayParams, ReaderPool, WriterHandle, retention_score_with_breadth,
};
use ai_memory_wiki::{Wiki, WritePageRequest};
use jiff::Timestamp;
use serde::Serialize;
use thiserror::Error;

use crate::cold_cluster::{adaptive_eps, dbscan};
use crate::compaction::build_compacted_markdown;

/// One evicted page surfaced in the [`SweepReport`].
#[derive(Debug, Clone, Serialize)]
pub struct EvictedPage {
    /// Identifier of the page selected for eviction.
    pub id: PageId,
    /// Relative wiki path.
    pub path: String,
    /// Retention score at the time of the sweep.
    pub retention: f64,
    /// Days since the page's last update.
    pub age_days: f64,
    /// Total access count.
    pub access_count: u32,
    /// `true` when the Markdown removal and tombstone write landed. Always
    /// `false` on `dry_run`; failures are retried by the next sweep.
    pub deleted: bool,
}

/// One cold episodic page tiered DOWN (extractively compacted) instead of
/// evicted, surfaced in the [`SweepReport`] (A2, docs/design-memory-aging.md).
#[derive(Debug, Clone, Serialize)]
pub struct CompactedPage {
    /// Identifier of the pre-compaction page version selected for tier-down.
    pub id: PageId,
    /// Relative wiki path.
    pub path: String,
    /// Retention score at the time of the sweep (below the cold threshold).
    pub retention: f64,
    /// Days since the page's last update.
    pub age_days: f64,
    /// Total access count.
    pub access_count: u32,
    /// `true` when the compacting rewrite landed through the wiki layer.
    /// Always `false` on `dry_run`; a failed rewrite is retried next sweep.
    pub compacted: bool,
    /// Identifier of the new compacted latest version, once the rewrite lands.
    /// The prior full-body version stays reachable via the supersession chain
    /// and git history, so tier-down is reversible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_id: Option<PageId>,
}

/// One cold-cluster collapse surfaced in the [`SweepReport`] (A3,
/// docs/design-memory-aging.md). A cluster of near-duplicate cold episodic pages
/// is collapsed to one survivor; the other members are superseded with a merge
/// note pointing at the survivor and stay reachable (invariant #16, never a hard
/// delete).
#[derive(Debug, Clone, Serialize)]
pub struct MergedCluster {
    /// Path of the survivor (the highest-retention cluster member).
    pub survivor: String,
    /// Pre-merge identifier of the survivor version.
    pub survivor_id: PageId,
    /// Paths of the members merged away into the survivor.
    pub merged: Vec<String>,
    /// The eps (cosine distance) the adaptive k-distance heuristic chose for
    /// this run — surfaced so a conservative-vs-eager run is observable.
    pub eps: f64,
    /// `true` when the collapse landed through the wiki layer. Always `false` on
    /// `dry_run`; a failed batch is retried on the next sweep.
    pub applied: bool,
    /// Identifier of the new (merged) survivor version once the collapse lands.
    /// The pre-merge survivor and every merged-away member stay reachable via
    /// the supersession chain and git history, so the collapse is reversible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_survivor_id: Option<PageId>,
}

/// The `(provider, model, dim)` triple identifying which stored embeddings the
/// A3 cold-cluster dedup should load. Must match the triple the pages were
/// embedded under; a mismatch (or no embeddings) makes A3 a clean no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingCoord {
    /// Embedding provider name (e.g. `synthetic`, `openai`).
    pub provider: String,
    /// Embedding model id.
    pub model: String,
    /// Vector dimensionality.
    pub dim: u32,
}

/// Opt-in configuration for the A3 cold-cluster dedup pass.
///
/// Off by default (`enabled = false` and no [`EmbeddingCoord`]): the sweep never
/// clusters or merges, so behaviour is byte-identical to before A3. The pass is
/// also a clean no-op — never an error — when no embeddings exist for the given
/// triple or no embedder is configured (invariant #13, zero generative LLM: A3
/// only reads already-stored vectors).
#[derive(Debug, Clone, Default)]
pub struct ColdClusterDedup {
    /// Master switch. `false` (the default) disables clustering entirely.
    pub enabled: bool,
    /// Which stored embeddings to cluster over. `None` ⇒ no-op (nothing to
    /// load), so a store with no embedder configured never dedups.
    pub embedding: Option<EmbeddingCoord>,
    /// DBSCAN density floor. `0` falls back to the conservative default of 2 (a
    /// pair of near-duplicates is the smallest thing worth collapsing).
    pub min_pts: usize,
    /// Conservative ceiling on the adaptive eps (cosine distance). The adaptive
    /// k-distance elbow is clamped to this, so A3 errs toward NOT merging.
    /// `0.0` falls back to [`DEFAULT_DEDUP_MAX_EPS`].
    pub max_eps: f32,
}

/// Default DBSCAN density floor for A3 when unset.
pub const DEFAULT_DEDUP_MIN_PTS: usize = 2;
/// Default conservative eps ceiling (cosine distance ≈ 0.15 ⇒ cosine similarity
/// ≥ 0.85) for A3 when unset. Deliberately tight: over-eager clustering is the
/// documented A3 failure mode.
pub const DEFAULT_DEDUP_MAX_EPS: f32 = 0.15;

impl ColdClusterDedup {
    fn effective_min_pts(&self) -> usize {
        if self.min_pts == 0 {
            DEFAULT_DEDUP_MIN_PTS
        } else {
            self.min_pts
        }
    }

    fn effective_max_eps(&self) -> f32 {
        if self.max_eps > 0.0 && self.max_eps.is_finite() {
            self.max_eps
        } else {
            DEFAULT_DEDUP_MAX_EPS
        }
    }
}

/// One TTL-expired page surfaced in the [`SweepReport`].
#[derive(Debug, Clone, Serialize)]
pub struct ExpiredPage {
    /// Identifier of the expired page version.
    pub id: PageId,
    /// Relative wiki path.
    pub path: String,
    /// ISO-8601 instant the page expired at.
    pub expired_at: String,
    /// `true` when the delete landed (always `false` on `dry_run`; can
    /// be `false` on a real run if an admission webhook rejected the
    /// delete or the wiki/store errored — the page is retried on the
    /// next sweep).
    pub deleted: bool,
}

/// Outcome of one sweep run.
#[derive(Debug, Clone, Serialize)]
pub struct SweepReport {
    /// `true` if `dry_run` was set and no rows were actually mutated.
    pub dry_run: bool,
    /// Total candidates evaluated (all tiers, before filtering).
    pub candidates_evaluated: usize,
    /// Pages that fell below the cold threshold (evicted through the wiki
    /// layer unless `dry_run`).
    pub evicted: Vec<EvictedPage>,
    /// Cold episodic pages tiered DOWN (extractively compacted) instead of
    /// evicted, when `compact_cold_episodic` is enabled (A2). Empty — and the
    /// sweep behaves exactly as before — while the flag is off, which is the
    /// default. Reported in both modes so every run is observable.
    pub compacted: Vec<CompactedPage>,
    /// Near-duplicate cold-episodic clusters collapsed to one survivor (A3),
    /// when `dedup.enabled` is set. Empty — and the sweep behaves exactly as
    /// before — while the flag is off, which is the default. Reported in both
    /// modes so every run is observable.
    pub merged: Vec<MergedCluster>,
    /// Pages past their frontmatter `expires_at:` TTL (hard-deleted
    /// through the wiki layer unless `dry_run`).
    pub expired: Vec<ExpiredPage>,
    /// Number of page-version rows permanently deleted on this pass.
    pub hard_deleted: usize,
    /// Observations the prune predicate matched. Reported in both modes; `0`
    /// when observation retention is disabled, which is the default.
    pub observations_prunable: usize,
    /// Observation rows permanently deleted. Always `0` on `dry_run`, exactly
    /// like the wiki-routed passes above.
    pub observations_pruned: usize,
    /// Distinct consolidated sessions that lost rows, so the blast radius is a
    /// number in the response rather than a claim in a changelog.
    pub observation_prune_sessions: usize,
    /// Transactions the prune spent. Makes the batching bound observable.
    pub observation_prune_batches: usize,
    /// Old hook event ledgers (`log-YYYY-MM.md`) the ledger pass matched.
    /// Reported in both modes; `0` when ledger retention is disabled (the
    /// default) or the caller has no wiki handle.
    pub ledgers_prunable: usize,
    /// Bytes on disk of the ledgers counted in [`Self::ledgers_prunable`].
    pub ledger_bytes_prunable: u64,
    /// Ledger files actually deleted. Always `0` on `dry_run`.
    pub ledgers_pruned: usize,
    /// Bytes freed by the deleted ledgers. Always `0` on `dry_run`.
    pub ledger_bytes_pruned: u64,
    /// Matched ledgers the pass could not delete: an admission webhook with a
    /// `reject` policy refused the removal, or the delete itself failed. The
    /// file is preserved and retried on the next sweep — the ledger answer
    /// to a page delete that a webhook refused (kept, `deleted: false`,
    /// retried). Always `0` on `dry_run`.
    pub ledgers_undeleted: usize,
    /// Matched ledgers that still have a legacy `pages` row (indexed before
    /// #660). The pass deletes the file only and leaves the row alone;
    /// `reclaim-ledger-versions` is the tool that cleans those rows.
    pub ledger_page_rows_left: usize,
}

/// Opt-in bound on how long raw observations outlive their consolidation.
///
/// Deliberately a type of its own rather than two more
/// [`DecayParams`] fields: the retention coefficients are a public struct that
/// downstream Rust callers construct directly, and widening it would break
/// them. Same reasoning the access-breadth coefficient already carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationRetention {
    /// Age in days past which a consolidated session's observations may be
    /// pruned. `0` disables the pass entirely — the default, so an existing
    /// install behaves exactly as it did before this pass existed.
    pub days: i64,
    /// Rows deleted per transaction.
    pub batch: usize,
}

/// Disabled: nothing is ever pruned until an operator sets a positive age.
impl Default for ObservationRetention {
    fn default() -> Self {
        Self {
            days: 0,
            batch: DEFAULT_OBSERVATION_PRUNE_BATCH,
        }
    }
}

impl ObservationRetention {
    /// `true` when the pass is enabled and can delete something.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        self.days > 0 && self.batch > 0
    }
}

/// Default rows per prune transaction.
pub const DEFAULT_OBSERVATION_PRUNE_BATCH: usize = 5_000;

/// Opt-in age bound for the hook event ledgers (`log-YYYY-MM.md`).
///
/// A type of its own for the same reason as [`ObservationRetention`]: the
/// sweep entry points are public and widening an existing struct would break
/// downstream callers that build it directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LedgerRetention {
    /// A ledger is deletable once this many whole calendar months (UTC) have
    /// passed after its own month ended. `0` disables the pass entirely — the
    /// default, so an existing install keeps every ledger.
    pub months: i64,
}

impl LedgerRetention {
    /// `true` when the pass is enabled and can delete something.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        self.months > 0
    }
}

/// Errors raised by the sweep.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SweepError {
    /// Underlying store error.
    #[error(transparent)]
    Store(#[from] ai_memory_store::StoreError),
    /// The optional access-breadth coefficient was negative or non-finite.
    #[error("decay breadth_weight must be a finite number greater than or equal to zero")]
    InvalidBreadthWeight,
    /// The opt-in observation retention age was negative.
    #[error("decay observation_retention_days must be greater than or equal to zero")]
    InvalidObservationRetention,
    /// The opt-in ledger retention age was negative.
    #[error("decay ledger_retention_months must be greater than or equal to zero")]
    InvalidLedgerRetention,
}

const US_PER_DAY: f64 = 86_400_000_000.0;
const US_PER_DAY_I64: i64 = 86_400_000_000;

/// Run a sweep against the given workspace/project.
///
/// `wiki` routes decay and TTL deletions through the wiki layer so the
/// Markdown source of truth is removed with the index mutation. Callers
/// without a wiki handle (bare-store tests) pass `None`; affected pages are
/// reported but left intact. A store-only mutation would let reconciliation
/// re-index the authoritative file.
///
/// # Errors
/// Propagates any store error encountered while reading candidates or
/// writing tombstones. Per-page Wiki failures (rejecting admission
/// webhook, IO error) are reported in the corresponding entry instead of
/// aborting the sweep.
pub async fn run_sweep(
    reader: &ReaderPool,
    writer: &WriterHandle,
    wiki: Option<&Wiki>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    params: &DecayParams,
    dry_run: bool,
) -> Result<SweepReport, SweepError> {
    run_sweep_with_breadth(
        reader,
        writer,
        wiki,
        workspace_id,
        project_id,
        params,
        0.0,
        dry_run,
    )
    .await
}

/// Run a sweep with an opt-in access-breadth coefficient.
///
/// # Errors
/// Returns [`SweepError::InvalidBreadthWeight`] for negative or non-finite
/// coefficients, in addition to the errors documented by [`run_sweep`].
#[allow(clippy::too_many_arguments)]
pub async fn run_sweep_with_breadth(
    reader: &ReaderPool,
    writer: &WriterHandle,
    wiki: Option<&Wiki>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    params: &DecayParams,
    breadth_weight: f64,
    dry_run: bool,
) -> Result<SweepReport, SweepError> {
    run_sweep_with_options(
        reader,
        writer,
        wiki,
        workspace_id,
        project_id,
        params,
        breadth_weight,
        ObservationRetention::default(),
        dry_run,
    )
    .await
}

/// Run a sweep with the breadth coefficient and the opt-in observation prune.
///
/// The prune is the only pass that can delete raw capture, and it is off unless
/// `retention.days` is positive. Extractive tier-down (A2) is left OFF, so this
/// behaves exactly as it did before A2 existed — cold episodic pages are
/// evicted (tombstoned). Callers that want tier-down use
/// [`run_sweep_with_compaction`].
///
/// # Errors
/// Returns [`SweepError::InvalidObservationRetention`] for a negative age, in
/// addition to the errors documented by [`run_sweep_with_breadth`].
#[allow(clippy::too_many_arguments)]
pub async fn run_sweep_with_options(
    reader: &ReaderPool,
    writer: &WriterHandle,
    wiki: Option<&Wiki>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    params: &DecayParams,
    breadth_weight: f64,
    retention: ObservationRetention,
    dry_run: bool,
) -> Result<SweepReport, SweepError> {
    run_sweep_with_compaction(
        reader,
        writer,
        wiki,
        workspace_id,
        project_id,
        params,
        breadth_weight,
        retention,
        false,
        dry_run,
    )
    .await
}

/// Run a sweep with the breadth coefficient, the observation prune, and the
/// opt-in A2 extractive tier-down (`compact_cold_episodic`).
///
/// With `compact_cold_episodic = false` (the default everywhere) this is the
/// historical sweep for every page that carries no V65 marker: a cold episodic
/// page is evicted (tombstoned). With it `true`, a cold episodic page that has
/// NOT already been compacted is tiered DOWN instead — rewritten through the
/// wiki layer to keep
/// its L0 abstract, an L1 summary and the L2 keep-token set, dropping the prose
/// — and reported under [`SweepReport::compacted`] rather than `evicted`. The
/// original full body stays reachable via supersession + git (reversible,
/// invariant #16). An already-compacted cold page (its V65 marker set) is
/// terminal for the decay pass: it is neither re-compacted nor evicted, so the
/// durable residue survives.
///
/// # Errors
/// Same as [`run_sweep_with_options`].
#[allow(clippy::too_many_arguments)]
pub async fn run_sweep_with_compaction(
    reader: &ReaderPool,
    writer: &WriterHandle,
    wiki: Option<&Wiki>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    params: &DecayParams,
    breadth_weight: f64,
    retention: ObservationRetention,
    compact_cold_episodic: bool,
    dry_run: bool,
) -> Result<SweepReport, SweepError> {
    run_sweep_with_hygiene(
        reader,
        writer,
        wiki,
        workspace_id,
        project_id,
        params,
        breadth_weight,
        retention,
        compact_cold_episodic,
        ColdClusterDedup::default(),
        dry_run,
    )
    .await
}

/// Run a sweep with the breadth coefficient, the observation prune, the opt-in
/// A2 extractive tier-down, and the opt-in A3 cold-cluster dedup.
///
/// A3 (`dedup.enabled`) clusters the *bounded cold-episodic set this sweep
/// already materialised* by embedding (cosine DBSCAN, adaptive k-distance eps)
/// and collapses each near-duplicate cluster to one survivor — the
/// highest-retention member, its body the extractive union of the cluster's
/// keep-tokens — superseding the other members with a merge note that points at
/// the survivor. Nothing is ever hard-deleted: every merged-away member stays
/// reachable through the supersession chain and git (invariant #16), and the
/// merge provenance is recorded in `page_evidence`. With `dedup` disabled (the
/// default) no clustering happens and this is byte-identical to
/// [`run_sweep_with_compaction`]. With no embeddings for the configured triple —
/// or no embedder configured at all — A3 is a clean no-op (invariant #13: it
/// reads only already-stored vectors, never a provider).
///
/// # Errors
/// Same as [`run_sweep_with_options`].
#[allow(clippy::too_many_arguments)]
pub async fn run_sweep_with_hygiene(
    reader: &ReaderPool,
    writer: &WriterHandle,
    wiki: Option<&Wiki>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    params: &DecayParams,
    breadth_weight: f64,
    retention: ObservationRetention,
    compact_cold_episodic: bool,
    dedup: ColdClusterDedup,
    dry_run: bool,
) -> Result<SweepReport, SweepError> {
    run_sweep_with_ledgers(
        reader,
        writer,
        wiki,
        workspace_id,
        project_id,
        params,
        breadth_weight,
        retention,
        compact_cold_episodic,
        dedup,
        LedgerRetention::default(),
        dry_run,
    )
    .await
}

/// Run a sweep with everything [`run_sweep_with_hygiene`] does plus the opt-in
/// event ledger pass, at `Timestamp::now()`.
///
/// The ledger pass runs after the observation prune and, like the other
/// wiki-routed passes, needs a `wiki` handle (without one it is skipped). With
/// `ledgers.months == 0` (the default everywhere) it does nothing and this is
/// byte-identical to [`run_sweep_with_hygiene`]. A dry run lists the files and
/// bytes it would delete and deletes nothing.
///
/// # Errors
/// Returns [`SweepError::InvalidLedgerRetention`] for a negative age, in
/// addition to the errors documented by [`run_sweep_with_hygiene`].
#[allow(clippy::too_many_arguments)]
pub async fn run_sweep_with_ledgers(
    reader: &ReaderPool,
    writer: &WriterHandle,
    wiki: Option<&Wiki>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    params: &DecayParams,
    breadth_weight: f64,
    retention: ObservationRetention,
    compact_cold_episodic: bool,
    dedup: ColdClusterDedup,
    ledgers: LedgerRetention,
    dry_run: bool,
) -> Result<SweepReport, SweepError> {
    run_sweep_at(
        reader,
        writer,
        wiki,
        workspace_id,
        project_id,
        params,
        breadth_weight,
        retention,
        compact_cold_episodic,
        dedup,
        ledgers,
        Timestamp::now(),
        dry_run,
    )
    .await
}

/// The full-knob sweep run with its caller-supplied instant: the scoring,
/// TTL and observation cutoffs and the ledger pass's month arithmetic all
/// read `now`, so the passes that were already evaluated at sweep start keep
/// one consistent instant. (The aged-tombstone cutoff stays a fresh
/// `Timestamp::now()`, because it has to sit after this run's own eviction
/// stamps — see the inline note.)
///
/// Production callers go through [`run_sweep_with_ledgers`] (which passes
/// `Timestamp::now()`), so this changes no production behaviour. The seam
/// exists so the integration tests can hold the pass at a fixed UTC instant,
/// away from the real month boundary a wall-clock run could cross mid-test.
///
/// # Errors
/// Same as [`run_sweep_with_ledgers`].
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_sweep_at(
    reader: &ReaderPool,
    writer: &WriterHandle,
    wiki: Option<&Wiki>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    params: &DecayParams,
    breadth_weight: f64,
    retention: ObservationRetention,
    compact_cold_episodic: bool,
    dedup: ColdClusterDedup,
    ledgers: LedgerRetention,
    now: Timestamp,
    dry_run: bool,
) -> Result<SweepReport, SweepError> {
    if !breadth_weight.is_finite() || breadth_weight < 0.0 {
        return Err(SweepError::InvalidBreadthWeight);
    }
    if retention.days < 0 {
        return Err(SweepError::InvalidObservationRetention);
    }
    if ledgers.months < 0 {
        return Err(SweepError::InvalidLedgerRetention);
    }
    let candidates = reader.decay_candidates(workspace_id, project_id).await?;
    let breadth =
        access_breadth_for_scoring(reader, workspace_id, project_id, breadth_weight).await?;
    let now_us = now.as_microsecond();

    let mut evicted = Vec::new();
    let mut compacted: Vec<CompactedPage> = Vec::new();
    let mut expired: Vec<ExpiredPage> = Vec::new();

    // First pass: TTL and the cold set. Collecting the cold set once lets the A3
    // dedup pass cluster the *same* bounded set the evict/compact pass would
    // otherwise process (invariant #2 — one materialisation, not two chances to
    // drift), and lets the remainder loop skip whatever dedup already claimed.
    let mut cold: Vec<ColdEntry> = Vec::new();
    for c in &candidates {
        if let Some(expires_us) = c.expires_at_us
            && expires_us <= now_us
        {
            expired.push(ExpiredPage {
                id: c.id,
                path: c.path.as_str().to_string(),
                expired_at: Timestamp::from_microsecond(expires_us)
                    .map(|ts| ts.to_string())
                    .unwrap_or_default(),
                deleted: false,
            });
            continue;
        }
        if let Some(entry) = cold_entry_for(c, &breadth, breadth_weight, params, now_us) {
            cold.push(entry);
        }
    }

    // A3 cold-cluster dedup plan (read-only). Populates `merged` and the set of
    // page ids the collapse claims, so the evict/compact remainder skips them.
    // A clean no-op (empty plan) when the flag is off, no embeddings exist for
    // the configured triple, or no wiki handle is available.
    let plan =
        plan_cold_cluster_dedup(reader, wiki, workspace_id, project_id, &dedup, &cold).await?;
    let mut merged = plan.report;
    let mut dedup_requests = plan.requests;
    let dedup_survivor_req_index = plan.survivor_req_index;
    let deduped = plan.claimed;

    for entry in &cold {
        if deduped.contains(&entry.id) {
            continue;
        }
        // An already-compacted page (its V65 marker set) is terminal for the
        // decay pass on BOTH arms. The durable residue is what we chose to
        // keep, so evicting it would destroy exactly what tier-down preserved,
        // and re-compacting it is a no-op loop. Skip it entirely.
        //
        // This guard cannot sit inside the `compact_cold_episodic` arm: the
        // marker is stamped by passes with their OWN flags — the A3 dedup
        // merge-note stub and the B2 dream pass both write `compacted: true`,
        // from which `compacted_at` is derived at the single write choke point
        // — so residues exist with A2 off, which is the default everywhere.
        if entry.compacted_at_us.is_some() {
            continue;
        }
        if compact_cold_episodic {
            // A2 tier-down replaces eviction for cold episodic pages.
            compacted.push(CompactedPage {
                id: entry.id,
                path: entry.path.as_str().to_string(),
                retention: entry.retention,
                age_days: entry.age_days,
                access_count: entry.access_count,
                compacted: false,
                new_id: None,
            });
        } else {
            evicted.push(EvictedPage {
                id: entry.id,
                path: entry.path.as_str().to_string(),
                retention: entry.retention,
                age_days: entry.age_days,
                access_count: entry.access_count,
                deleted: false,
            });
        }
    }

    let mut hard_deleted = 0usize;
    let mut observations_prunable = 0usize;
    let mut observations_pruned = 0usize;
    let mut observation_prune_sessions = 0usize;
    let mut observation_prune_batches = 0usize;
    let observation_cutoff_us = now
        .as_microsecond()
        .saturating_sub(retention.days.saturating_mul(US_PER_DAY_I64));
    if retention.is_enabled() {
        observations_prunable = reader
            .prunable_observation_count(workspace_id, project_id, observation_cutoff_us)
            .await?;
    }
    if !dry_run {
        // A3 cold-cluster collapse (invariant #2: one batched write for every
        // survivor rewrite and merge-note supersession). The survivor keeps the
        // union of the cluster's keep-tokens and cites its merged-away members
        // in `page_evidence`; every member stays reachable via supersession +
        // git (invariant #16). Freshly rewritten, none of these pages is cold
        // next sweep, so the collapse does not immediately re-trigger.
        if !dedup_requests.is_empty() {
            match wiki {
                Some(wiki) => {
                    let requests = std::mem::take(&mut dedup_requests);
                    match wiki.apply_batch(requests).await {
                        Ok(new_ids) => {
                            for (cluster, &req_idx) in
                                merged.iter_mut().zip(dedup_survivor_req_index.iter())
                            {
                                cluster.applied = true;
                                cluster.new_survivor_id = new_ids.get(req_idx).copied();
                            }
                        }
                        Err(error) => tracing::warn!(
                            %error,
                            "forget sweep: cold-cluster dedup batch failed; retrying on next sweep"
                        ),
                    }
                }
                None => tracing::warn!(
                    "forget sweep: wiki unavailable; refusing store-only cold-cluster dedup"
                ),
            }
        }
        for page in &mut expired {
            let path = match ai_memory_core::PagePath::new(page.path.clone()) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let result = match wiki {
                Some(w) => match w
                    .delete_page_if_latest(workspace_id, project_id, &path, page.id, None, None)
                    .await
                {
                    Ok(true) => Ok(()),
                    Ok(false) => {
                        Err("page changed after expiry selection; refusing stale delete"
                            .to_string())
                    }
                    Err(e) => Err(e.to_string()),
                },
                None => Err("wiki unavailable; refusing store-only TTL delete".to_string()),
            };
            match result {
                Ok(()) => page.deleted = true,
                Err(error) => {
                    tracing::warn!(
                        path = %page.path,
                        error,
                        "forget sweep: TTL delete failed; retrying on next sweep"
                    );
                }
            }
        }
        // A2 compaction runs BEFORE the decay-eviction pass. When
        // `compact_cold_episodic` is on, `evicted` is empty and this pass owns
        // the cold episodic pages; when it is off, `compacted` is empty and this
        // pass is a no-op, so the historical eviction path below is unchanged.
        //
        // One batched wiki write for the whole cold set (invariant #2): the
        // rewrite supersedes each page's prior version, keeping the full body
        // reachable (invariant #16) through git + the supersession chain.
        if !compacted.is_empty() {
            match wiki {
                Some(wiki) => {
                    let mut requests: Vec<WritePageRequest> = Vec::new();
                    let mut request_index: Vec<usize> = Vec::new();
                    for (i, page) in compacted.iter().enumerate() {
                        let path = match PagePath::new(page.path.clone()) {
                            Ok(path) => path,
                            Err(_) => continue,
                        };
                        let markdown = match wiki.read_page(workspace_id, project_id, &path) {
                            Ok(md) => md,
                            Err(error) => {
                                tracing::warn!(
                                    path = %page.path,
                                    %error,
                                    "forget sweep: could not read page for compaction; retrying next sweep"
                                );
                                continue;
                            }
                        };
                        let (frontmatter, body) =
                            build_compacted_markdown(&markdown.frontmatter, &markdown.body);
                        requests.push(WritePageRequest {
                            workspace_id,
                            project_id,
                            path,
                            frontmatter,
                            body,
                            tier: Tier::Episodic,
                            pinned: false,
                            title: None,
                            admission_ctx: None,
                            author_id: None,
                            actor: ActorContext::anonymous(),
                            evidence: Vec::new(),
                        });
                        request_index.push(i);
                    }
                    if !requests.is_empty() {
                        match wiki.apply_batch(requests).await {
                            Ok(new_ids) => {
                                for (slot, new_id) in request_index.into_iter().zip(new_ids) {
                                    compacted[slot].compacted = true;
                                    compacted[slot].new_id = Some(new_id);
                                }
                            }
                            Err(error) => tracing::warn!(
                                %error,
                                "forget sweep: compaction batch failed; retrying on next sweep"
                            ),
                        }
                    }
                }
                None => {
                    tracing::warn!("forget sweep: wiki unavailable; refusing store-only compaction")
                }
            }
        }

        for page in &mut evicted {
            let path = match ai_memory_core::PagePath::new(page.path.clone()) {
                Ok(path) => path,
                Err(_) => continue,
            };
            let result = match wiki {
                Some(wiki) => wiki
                    .evict_page_if_latest(workspace_id, project_id, &path, page.id, None)
                    .await
                    .map_err(|error| error.to_string()),
                None => Err("wiki unavailable; refusing store-only decay eviction".to_string()),
            };
            match result {
                Ok(true) => page.deleted = true,
                Ok(false) => tracing::debug!(
                    path = %page.path,
                    "forget sweep: page changed after decay selection; skipping stale eviction"
                ),
                Err(error) => tracing::warn!(
                    path = %page.path,
                    error,
                    "forget sweep: decay eviction failed; retrying on next sweep"
                ),
            }
        }

        if let Some(wiki) = wiki {
            let grace_days = params.hard_delete_after_days.max(0);
            // A FRESH clock read, deliberately not the caller's `now`: the
            // evictions above stamp their tombstones' `superseded_at` as they
            // go, and the grace-0 case hard-deletes exactly those. The cutoff
            // has to sit after this run's own stamps, or a tombstone the run
            // just wrote is not "older than itself" and the grace-0 purge
            // silently skips it.
            let cutoff_us = Timestamp::now()
                .as_microsecond()
                .saturating_sub(grace_days.saturating_mul(US_PER_DAY_I64));
            let tombstones = reader
                .decay_tombstones_before(workspace_id, project_id, cutoff_us)
                .await?;
            for tombstone in tombstones {
                match wiki
                    .hard_delete_decay_tombstone(
                        workspace_id,
                        project_id,
                        &tombstone.path,
                        tombstone.id,
                        cutoff_us,
                    )
                    .await
                {
                    Ok(deleted) => hard_deleted += deleted,
                    Err(error) => tracing::warn!(
                        path = %tombstone.path,
                        %error,
                        "forget sweep: decay tombstone cleanup failed; retrying on next sweep"
                    ),
                }
            }
        }

        // LAST on purpose. Both passes above can strip a session of its
        // distillation — decay stamps `superseded_at`, hard-delete removes the
        // row and the `ON DELETE SET NULL` foreign key clears the pointer — and
        // the prune predicate reads exactly those two columns. Running here
        // means this run's own evictions already exclude their sessions,
        // instead of a batch of raw capture outliving its page by one sweep.
        //
        // Unlike the wiki-routed passes this one runs without a `wiki` handle:
        // observations have no Markdown file, so there is no source of truth
        // for reconciliation to re-index and no store-only-mutation hazard.
        if retention.is_enabled() {
            let mut touched: HashSet<ai_memory_core::SessionId> = HashSet::new();
            loop {
                let outcome = writer
                    .prune_consolidated_observations(
                        workspace_id,
                        project_id,
                        observation_cutoff_us,
                        retention.batch,
                    )
                    .await?;
                observation_prune_batches += 1;
                observations_pruned += outcome.deleted;
                touched.extend(outcome.sessions_touched.iter().copied());
                if outcome.deleted < retention.batch {
                    break;
                }
            }
            observation_prune_sessions = touched.len();
        }
    }

    // Event ledger pass, last: files, not pages, so nothing above depends on it
    // and it depends on nothing above. Both modes list; only a real run deletes.
    let ledger_outcome = match wiki {
        Some(wiki) if ledgers.is_enabled() => {
            sweep_event_ledgers(
                reader,
                wiki,
                workspace_id,
                project_id,
                ledgers,
                now,
                dry_run,
            )
            .await
        }
        _ => LedgerSweepOutcome::default(),
    };

    Ok(SweepReport {
        dry_run,
        candidates_evaluated: candidates.len(),
        evicted,
        compacted,
        merged,
        expired,
        hard_deleted,
        observations_prunable,
        observations_pruned,
        observation_prune_sessions,
        observation_prune_batches,
        ledgers_prunable: ledger_outcome.matched,
        ledger_bytes_prunable: ledger_outcome.matched_bytes,
        ledgers_pruned: ledger_outcome.deleted,
        ledger_bytes_pruned: ledger_outcome.deleted_bytes,
        ledgers_undeleted: ledger_outcome.undeleted,
        ledger_page_rows_left: ledger_outcome.page_rows_left,
    })
}

/// What the ledger pass found and did, folded into the [`SweepReport`].
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct LedgerSweepOutcome {
    pub(crate) matched: usize,
    pub(crate) matched_bytes: u64,
    pub(crate) deleted: usize,
    pub(crate) deleted_bytes: u64,
    /// Matched ledgers whose removal failed (a refusing admission webhook or
    /// an IO error). Kept and retried by the next sweep.
    pub(crate) undeleted: usize,
    pub(crate) page_rows_left: usize,
}

/// Position of a calendar month on one line: `year * 12 + (month - 1)`.
fn month_index(year: i64, month: i64) -> i64 {
    year.saturating_mul(12).saturating_add(month - 1)
}

/// Month index of a `log-YYYY-MM.md` file name, or `None` when the name is not
/// that shape or the month is not 1..=12 (a shape the content gate cannot
/// vouch for is never an age decision).
fn ledger_month_index(file_name: &str) -> Option<i64> {
    if !ai_memory_core::log_ledger::is_rotated_log_ledger_path(file_name) {
        return None;
    }
    let stem = file_name.strip_prefix("log-")?.strip_suffix(".md")?;
    let year: i64 = stem.get(..4)?.parse().ok()?;
    let month: i64 = stem.get(5..)?.parse().ok()?;
    (1..=12).contains(&month).then(|| month_index(year, month))
}

/// Whether a ledger whose month sits at `ledger_idx` may be deleted when the
/// current month sits at `now_idx`, with `months` as the retention age.
///
/// Two independent rules, both required:
///
/// - the writer guard: the current and the previous month are never
///   deletable. The hook appender may still write to them (the previous month
///   takes late-delivered events), so no setting can make them eligible;
/// - the age rule: at least `months` whole calendar months have passed since
///   the ledger's month ended (`ledger_idx + 1 + months <= now_idx`).
///
/// For a positive `months` the age rule already implies the guard; the guard
/// stays a separate line so the protection does not depend on that arithmetic
/// or on the caller's gate (it is exercised directly with `months == 0`).
fn ledger_is_expired(ledger_idx: i64, now_idx: i64, months: i64) -> bool {
    if ledger_idx >= now_idx.saturating_sub(1) {
        return false;
    }
    ledger_idx.saturating_add(months).saturating_add(1) <= now_idx
}

/// The ledger pass: list the project's event ledgers, pick the expired ones
/// and, unless `dry_run`, delete them through [`Wiki::delete_event_ledger`],
/// which consults the blocking admission chain (a `reject`-policy webhook
/// keeps the file).
///
/// Best-effort like the other wiki-routed passes: a listing or per-file
/// failure (an admission refusal, an IO error) is logged and counted in
/// [`LedgerSweepOutcome::undeleted`], the file is kept, and the pass keeps
/// going — the same answer a refused page delete gets, which the sweep
/// retries on the next run. The store is only read (to say how many matched
/// ledgers still have a legacy page row); those rows are left alone.
async fn sweep_event_ledgers(
    reader: &ReaderPool,
    wiki: &Wiki,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    ledgers: LedgerRetention,
    now: Timestamp,
    dry_run: bool,
) -> LedgerSweepOutcome {
    let mut outcome = LedgerSweepOutcome::default();
    let found = match wiki.list_event_ledgers(workspace_id, project_id) {
        Ok(found) => found,
        Err(error) => {
            tracing::warn!(
                %error,
                "forget sweep: listing event ledgers failed; retrying on next sweep"
            );
            return outcome;
        }
    };
    let now_utc = now.to_zoned(jiff::tz::TimeZone::UTC);
    let now_idx = month_index(i64::from(now_utc.year()), i64::from(now_utc.month()));
    for ledger in found {
        let Some(idx) = ledger_month_index(&ledger.file_name) else {
            continue;
        };
        if !ledger_is_expired(idx, now_idx, ledgers.months) {
            continue;
        }
        outcome.matched += 1;
        outcome.matched_bytes = outcome.matched_bytes.saturating_add(ledger.bytes);
        match reader
            .latest_page_id_by_ids(workspace_id, project_id, ledger.file_name.clone())
            .await
        {
            Ok(Some(_)) => outcome.page_rows_left += 1,
            Ok(None) => {}
            Err(error) => tracing::warn!(
                file = %ledger.file_name,
                %error,
                "forget sweep: could not check a ledger for a legacy page row"
            ),
        }
        if dry_run {
            continue;
        }
        match wiki
            .delete_event_ledger(workspace_id, project_id, &ledger.file_name)
            .await
        {
            Ok(Some(bytes)) => {
                outcome.deleted += 1;
                outcome.deleted_bytes = outcome.deleted_bytes.saturating_add(bytes);
            }
            // Gone or no longer a ledger since the listing: nothing to delete.
            Ok(None) => {}
            Err(error) => {
                // A refusing webhook (or a failed delete): the file stays,
                // the pass keeps going, the next sweep retries it — the same
                // shape as a refused page delete above.
                outcome.undeleted += 1;
                tracing::warn!(
                    file = %ledger.file_name,
                    %error,
                    "forget sweep: event ledger delete refused or failed; retrying on next sweep"
                );
            }
        }
    }
    outcome
}

/// A cold page (scored below `cold_threshold`) captured once so the A3 dedup
/// pass and the evict/compact remainder read the same materialised set.
///
/// `pub(crate)` so the B2 dream pass ([`crate::dream`]) clusters the *same*
/// bounded cold set the forget sweep does — invariant #2: one materialisation of
/// the retention scoring, not two chances to drift.
pub(crate) struct ColdEntry {
    pub(crate) id: PageId,
    pub(crate) path: PagePath,
    pub(crate) retention: f64,
    pub(crate) age_days: f64,
    pub(crate) access_count: u32,
    pub(crate) compacted_at_us: Option<i64>,
}

/// Score one decay candidate and return a [`ColdEntry`] when it is a decayable
/// episodic page below `cold_threshold`. `None` for a non-decayable tier, a
/// pinned page, or a page still above the threshold. TTL expiry is the caller's
/// concern (the sweep hard-deletes those; the dream pass skips them).
///
/// The single scoring path shared by the forget sweep's inline loop and
/// [`materialize_cold_set`], so the cold set is defined once (invariant #2).
pub(crate) fn cold_entry_for(
    c: &DecayCandidate,
    breadth: &HashMap<PageId, u32>,
    breadth_weight: f64,
    params: &DecayParams,
    now_us: i64,
) -> Option<ColdEntry> {
    if !is_decayable(c) {
        return None;
    }
    let age_days = elapsed_days(now_us, c.updated_at_us);
    let days_since_access = c.last_accessed_at_us.map(|us| elapsed_days(now_us, us));
    let score = retention_score_with_breadth(
        params,
        c.tier,
        age_days,
        c.access_count,
        days_since_access,
        c.salience,
        breadth.get(&c.id).copied().unwrap_or(0),
        breadth_weight,
    );
    if score < params.cold_threshold {
        Some(ColdEntry {
            id: c.id,
            path: c.path.clone(),
            retention: score,
            age_days,
            access_count: c.access_count,
            compacted_at_us: c.compacted_at_us,
        })
    } else {
        None
    }
}

/// Materialise the bounded cold set for a scope, reusing the sweep's exact
/// retention scoring (invariant #2). TTL-expired pages are excluded — an expired
/// page is the sweep's to hard-delete, never the dream pass's to rewrite.
///
/// # Errors
/// Returns [`SweepError::InvalidBreadthWeight`] for a negative or non-finite
/// coefficient, or a store error while reading candidates.
pub(crate) async fn materialize_cold_set(
    reader: &ReaderPool,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    params: &DecayParams,
    breadth_weight: f64,
) -> Result<Vec<ColdEntry>, SweepError> {
    if !breadth_weight.is_finite() || breadth_weight < 0.0 {
        return Err(SweepError::InvalidBreadthWeight);
    }
    let candidates = reader.decay_candidates(workspace_id, project_id).await?;
    let breadth =
        access_breadth_for_scoring(reader, workspace_id, project_id, breadth_weight).await?;
    let now_us = Timestamp::now().as_microsecond();
    let mut cold = Vec::new();
    for c in &candidates {
        if let Some(expires_us) = c.expires_at_us
            && expires_us <= now_us
        {
            continue;
        }
        if let Some(entry) = cold_entry_for(c, &breadth, breadth_weight, params, now_us) {
            cold.push(entry);
        }
    }
    Ok(cold)
}

/// The read-only output of the A3 dedup planner: what to report, which page ids
/// the collapse claims, and the batched wiki writes to apply.
struct DedupPlan {
    report: Vec<MergedCluster>,
    claimed: HashSet<PageId>,
    requests: Vec<WritePageRequest>,
    /// For each entry in `report`, the index into `requests` of that cluster's
    /// survivor write, so the applied `new_survivor_id` can be mapped back.
    survivor_req_index: Vec<usize>,
}

impl DedupPlan {
    fn empty() -> Self {
        Self {
            report: Vec::new(),
            claimed: HashSet::new(),
            requests: Vec::new(),
            survivor_req_index: Vec::new(),
        }
    }
}

/// Footer stamped on a merge-note stub so a reader sees the page was collapsed
/// and where its content now lives.
const MERGE_STUB_FOOTER: &str = "_Merged into the survivor below by A3 cold-cluster dedup. This page's full \
     pre-merge body is retained in git history and the supersession chain, and \
     can be recovered with `restore-page`._";

/// Build the A3 cold-cluster dedup plan over the already-materialised cold set.
///
/// Pure read path: it loads stored embeddings, clusters them, and prepares the
/// wiki writes, but applies nothing. A clean no-op (empty plan) when disabled,
/// when no `EmbeddingCoord`/wiki is available, or when no embeddings exist for
/// the cold set (invariant #13 — never a provider call).
async fn plan_cold_cluster_dedup(
    reader: &ReaderPool,
    wiki: Option<&Wiki>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    dedup: &ColdClusterDedup,
    cold: &[ColdEntry],
) -> Result<DedupPlan, SweepError> {
    if !dedup.enabled {
        return Ok(DedupPlan::empty());
    }
    let Some(coord) = dedup.embedding.clone() else {
        return Ok(DedupPlan::empty());
    };
    // Dedup mutates through the wiki (survivor rewrite + supersessions); without
    // a handle there is nothing to plan, exactly like the compaction pass.
    let Some(wiki) = wiki else {
        return Ok(DedupPlan::empty());
    };
    // Only pages that are cold, not already a compacted/merged residue (their
    // marker set), are eligible — a terminal residue is never re-merged.
    let mut eligible: HashMap<PageId, &ColdEntry> = HashMap::new();
    for entry in cold {
        if entry.compacted_at_us.is_none() {
            eligible.insert(entry.id, entry);
        }
    }
    if eligible.len() < dedup.effective_min_pts() {
        return Ok(DedupPlan::empty());
    }

    let embeddings = reader
        .load_embeddings(
            workspace_id,
            project_id,
            coord.provider,
            coord.model,
            coord.dim,
        )
        .await?;
    // Intersect the stored embeddings with the eligible cold set.
    let mut ids: Vec<PageId> = Vec::new();
    let mut vectors: Vec<Vec<f32>> = Vec::new();
    for e in &embeddings {
        if eligible.contains_key(&e.id) {
            ids.push(e.id);
            vectors.push(e.vector.clone());
        }
    }
    if vectors.len() < dedup.effective_min_pts() {
        return Ok(DedupPlan::empty());
    }

    let min_pts = dedup.effective_min_pts();
    let Some(eps) = adaptive_eps(&vectors, min_pts, dedup.effective_max_eps()) else {
        return Ok(DedupPlan::empty());
    };
    let clusters = dbscan(&vectors, eps, min_pts);
    if clusters.is_empty() {
        return Ok(DedupPlan::empty());
    }

    let mut plan = DedupPlan::empty();
    for cluster in clusters {
        if cluster.len() < 2 {
            continue;
        }
        // Survivor = highest-retention member (ties broken by first index, for
        // determinism). Read every member's body to mine the union of keep-tokens.
        let mut member_entries: Vec<&ColdEntry> = Vec::new();
        for &idx in &cluster {
            match eligible.get(&ids[idx]) {
                Some(entry) => member_entries.push(entry),
                None => continue,
            }
        }
        if member_entries.len() < 2 {
            continue;
        }
        let survivor_pos = member_entries
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                a.retention
                    .partial_cmp(&b.retention)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i)
            .unwrap_or(0);
        let survivor = member_entries[survivor_pos];

        // Read the survivor's frontmatter and concatenate every member's body so
        // the extractive miner keeps the UNION of the cluster's keep-tokens.
        let survivor_md = match wiki.read_page(workspace_id, project_id, &survivor.path) {
            Ok(md) => md,
            Err(error) => {
                tracing::warn!(path = %survivor.path.as_str(), %error, "forget sweep: could not read survivor for dedup; skipping cluster");
                continue;
            }
        };
        let mut union_body = survivor_md.body.clone();
        let mut loser_paths: Vec<String> = Vec::new();
        let mut loser_requests: Vec<WritePageRequest> = Vec::new();
        let mut evidence: Vec<PageEvidence> = Vec::new();
        let mut read_failed = false;
        for (pos, member) in member_entries.iter().enumerate() {
            if pos == survivor_pos {
                continue;
            }
            let md = match wiki.read_page(workspace_id, project_id, &member.path) {
                Ok(md) => md,
                Err(error) => {
                    tracing::warn!(path = %member.path.as_str(), %error, "forget sweep: could not read cluster member for dedup; skipping cluster");
                    read_failed = true;
                    break;
                }
            };
            union_body.push_str("\n\n");
            union_body.push_str(&md.body);
            loser_paths.push(member.path.as_str().to_string());
            // Merge provenance: the closed `page_evidence` vocabulary has no
            // dedicated A3 kind (adding one needs a CHECK-constraint migration,
            // which A3 deliberately avoids). `reconsolidation` is the accurate
            // existing kind — an A3 collapse rewrites the survivor from several
            // sources — and the `a3_merge:` source-id prefix keeps the merge
            // distinguishable and unique.
            evidence.push(PageEvidence {
                kind: PageEvidenceKind::Reconsolidation,
                source_id: format!("a3_merge:{}", member.id),
            });
            // Supersede the loser with a merge-note stub pointing at the
            // survivor. Marked `compacted: true` so it is terminal for the decay
            // pass and never re-clustered; its full body stays in the chain.
            let stub_body = format!(
                "This page was merged into [[{}]] by A3 cold-cluster dedup.\n\n{}\n",
                survivor.path.as_str(),
                MERGE_STUB_FOOTER
            );
            let mut stub_fm = frontmatter_object(&md.frontmatter);
            // The stub's body is now a two-line merge note, so the loser's own
            // `summary:` would describe content that moved to the survivor:
            // `page_descriptor_expr` prefers a non-blank frontmatter summary
            // over the body on every non-FTS retrieval path, and
            // `abstract_bearing_page_ids` would keep re-embedding the abstract.
            // Removing both rather than rewriting `summary` to the merge note:
            // the descriptor then falls back to the body prefix, which already
            // IS that note. The pre-merge frontmatter stays reachable through
            // the supersession chain and git.
            stub_fm.remove("summary");
            stub_fm.remove("abstract");
            stub_fm.insert("compacted".to_string(), serde_json::Value::Bool(true));
            stub_fm.insert(
                "merged_into".to_string(),
                serde_json::Value::String(survivor.path.as_str().to_string()),
            );
            loser_requests.push(WritePageRequest {
                workspace_id,
                project_id,
                path: member.path.clone(),
                frontmatter: serde_json::Value::Object(stub_fm),
                body: stub_body,
                tier: Tier::Episodic,
                pinned: false,
                title: None,
                admission_ctx: None,
                author_id: None,
                actor: ActorContext::anonymous(),
                evidence: Vec::new(),
            });
        }
        if read_failed || loser_paths.is_empty() {
            continue;
        }

        // Survivor body: extractive union of keep-tokens + a merge note. Reuses
        // A2's `build_compacted_markdown`, so the survivor keeps every member's
        // durable facts; the prose is dropped (zero-LLM, invariant #13).
        let (mut survivor_fm, mut survivor_body) =
            build_compacted_markdown(&survivor_md.frontmatter, &union_body);
        survivor_body.push_str(&format!(
            "\n\n_Merged {} near-duplicate cold page(s): {} (A3 cold-cluster dedup)._\n",
            loser_paths.len(),
            loser_paths.join(", ")
        ));
        if let serde_json::Value::Object(map) = &mut survivor_fm {
            map.insert(
                "merged_from".to_string(),
                serde_json::Value::Array(
                    loser_paths
                        .iter()
                        .map(|p| serde_json::Value::String(p.clone()))
                        .collect(),
                ),
            );
        }
        let survivor_request = WritePageRequest {
            workspace_id,
            project_id,
            path: survivor.path.clone(),
            frontmatter: survivor_fm,
            body: survivor_body,
            tier: Tier::Episodic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: ActorContext::anonymous(),
            evidence,
        };

        // Record. Survivor write goes first in this cluster's slice, so its
        // index is the current request length.
        let survivor_req_index = plan.requests.len();
        plan.requests.push(survivor_request);
        plan.requests.extend(loser_requests);
        plan.claimed.insert(survivor.id);
        for member in &member_entries {
            plan.claimed.insert(member.id);
        }
        plan.report.push(MergedCluster {
            survivor: survivor.path.as_str().to_string(),
            survivor_id: survivor.id,
            merged: loser_paths,
            eps: f64::from(eps),
            applied: false,
            new_survivor_id: None,
        });
        plan.survivor_req_index.push(survivor_req_index);
    }
    Ok(plan)
}

/// Coerce a frontmatter value into an object map, discarding a non-object shape.
pub(crate) fn frontmatter_object(
    fm: &serde_json::Value,
) -> serde_json::Map<String, serde_json::Value> {
    match fm {
        serde_json::Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    }
}

/// Distinct-actor counts keyed by page, for feeding
/// [`retention_score_with_breadth`].
///
/// One grouped query for the whole candidate set, not one per page — and none
/// at all while the breadth term is off, which is the default: the score is then
/// identical whatever the breakdown says, so a deployment that never enables it
/// never pays for reading it.
///
/// Shared with the curator rather than duplicated there: the curator's
/// `cold_episodic` verdict is a prediction of what the sweep will evict, so the
/// two must read the same input. A second lookup would be a second chance to
/// drift.
pub(crate) async fn access_breadth_for_scoring(
    reader: &ReaderPool,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    breadth_weight: f64,
) -> ai_memory_store::StoreResult<HashMap<PageId, u32>> {
    if breadth_weight == 0.0 {
        return Ok(HashMap::new());
    }
    reader
        .access_breadth_for_project(workspace_id, project_id)
        .await
}

fn elapsed_days(now_us: i64, then_us: i64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let raw = (now_us - then_us) as f64 / US_PER_DAY;
    raw.max(0.0)
}

fn is_decayable(c: &DecayCandidate) -> bool {
    if c.tier != Tier::Episodic {
        return false;
    }
    if c.pinned {
        return false;
    }
    if let Ok(fm) = serde_json::from_str::<serde_json::Value>(&c.frontmatter_json)
        && fm.get("pinned").and_then(serde_json::Value::as_bool) == Some(true)
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_pages_skip_decay() {
        let c = DecayCandidate {
            id: PageId::new(),
            path: ai_memory_core::PagePath::new("x.md").unwrap(),
            tier: Tier::Semantic,
            pinned: false,
            updated_at_us: 0,
            access_count: 0,
            last_accessed_at_us: None,
            frontmatter_json: "{}".into(),
            expires_at_us: None,
            salience: None,
            compacted_at_us: None,
        };
        assert!(!is_decayable(&c));
    }

    #[test]
    fn pinned_pages_skip_decay() {
        let c = DecayCandidate {
            id: PageId::new(),
            path: ai_memory_core::PagePath::new("x.md").unwrap(),
            tier: Tier::Episodic,
            pinned: true,
            updated_at_us: 0,
            access_count: 0,
            last_accessed_at_us: None,
            frontmatter_json: "{}".into(),
            expires_at_us: None,
            salience: None,
            compacted_at_us: None,
        };
        assert!(!is_decayable(&c));
    }

    #[test]
    fn frontmatter_pinned_overrides() {
        let c = DecayCandidate {
            id: PageId::new(),
            path: ai_memory_core::PagePath::new("x.md").unwrap(),
            tier: Tier::Episodic,
            pinned: false,
            updated_at_us: 0,
            access_count: 0,
            last_accessed_at_us: None,
            frontmatter_json: r#"{"pinned": true}"#.into(),
            expires_at_us: None,
            salience: None,
            compacted_at_us: None,
        };
        assert!(!is_decayable(&c));
    }

    #[test]
    fn fresh_episodic_page_is_decayable() {
        let c = DecayCandidate {
            id: PageId::new(),
            path: ai_memory_core::PagePath::new("x.md").unwrap(),
            tier: Tier::Episodic,
            pinned: false,
            updated_at_us: 0,
            access_count: 0,
            last_accessed_at_us: None,
            frontmatter_json: "{}".into(),
            expires_at_us: None,
            salience: None,
            compacted_at_us: None,
        };
        assert!(is_decayable(&c));
    }

    #[test]
    fn elapsed_days_clamps_future_timestamps() {
        assert_eq!(elapsed_days(1_000, 2_000), 0.0);
        assert!(elapsed_days(US_PER_DAY as i64, 0) >= 1.0);
    }

    #[test]
    fn ledger_month_index_reads_only_real_months() {
        assert_eq!(
            ledger_month_index("log-2026-07.md"),
            Some(month_index(2026, 7))
        );
        assert_eq!(
            ledger_month_index("log-2026-12.md"),
            Some(month_index(2026, 12))
        );
        for name in [
            "log.md",
            "log-2026-13.md",
            "log-2026-00.md",
            "log-2026-7.md",
            "log-2026-07-15.md",
            "notes/log-2026-07.md",
            "LOG-2026-07.md",
        ] {
            assert_eq!(
                ledger_month_index(name),
                None,
                "{name} must not date a ledger"
            );
        }
        // Consecutive months, across a year boundary, are consecutive indices.
        assert_eq!(
            month_index(2027, 1) - month_index(2026, 12),
            1,
            "december to january is one month"
        );
    }

    /// The age rule: a ledger is deletable once `months` whole calendar months
    /// passed after its own month ended, and not a month earlier.
    #[test]
    fn ledger_age_rule_counts_whole_months_after_the_month_ended() {
        let now = month_index(2026, 10);
        // N = 3: June ended Jul 1; Jul, Aug and Sep are the three whole months
        // after it, so June is deletable in October. July has only Aug and Sep
        // behind it (October is not over), so it is one month short.
        assert!(ledger_is_expired(month_index(2026, 5), now, 3));
        assert!(ledger_is_expired(month_index(2026, 6), now, 3));
        assert!(!ledger_is_expired(month_index(2026, 7), now, 3));
        assert!(!ledger_is_expired(month_index(2026, 8), now, 3));
        assert!(!ledger_is_expired(month_index(2026, 9), now, 3));
        assert!(!ledger_is_expired(month_index(2026, 10), now, 3));
        // N = 1 across a year boundary.
        let jan = month_index(2027, 1);
        assert!(ledger_is_expired(month_index(2026, 11), jan, 1));
        assert!(!ledger_is_expired(month_index(2026, 12), jan, 1));
    }

    /// The writer guard holds whatever the age: the current and the previous
    /// month are never deletable, even with `months == 0` (which the sweep never
    /// passes, but which makes the age rule alone delete last month) or a
    /// retention so large the arithmetic saturates. Mutation captured: dropping
    /// the guard line makes the `months == 0` case delete the previous month.
    #[test]
    fn ledger_guard_protects_the_current_and_previous_month_regardless_of_age() {
        let now = month_index(2026, 10);
        for months in [0, 1, 2, 12, i64::MAX] {
            assert!(
                !ledger_is_expired(now, now, months),
                "the current month must be kept at months={months}"
            );
            assert!(
                !ledger_is_expired(now - 1, now, months),
                "the previous month must be kept at months={months}"
            );
        }
        // Control: with no retention the age rule alone reaches two months back,
        // so the cases above are the guard's doing.
        assert!(ledger_is_expired(now - 2, now, 0));
        // A retention too large to ever elapse deletes nothing, without panic.
        assert!(!ledger_is_expired(now - 2, now, i64::MAX));
    }
}
