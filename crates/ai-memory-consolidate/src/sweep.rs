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
//! `hard_delete_after_days` and their supersession ancestry. A page recreated
//! at the same path is a separate live chain and is preserved. Ordinary
//! supersession rows are safe because only decay writes `superseded_at`.
//!
//! Observation prune pass (opt-in, disabled unless
//! `ObservationRetention::days > 0`) deletes raw observations older than the
//! configured age, and only for sessions already distilled into a page that is
//! still live. It runs LAST so both of this run's page deletions are already
//! visible to its predicate: a session whose summary page was evicted or
//! hard-deleted by the passes above keeps its raw capture, because that capture
//! is now the only surviving copy.

use std::collections::{HashMap, HashSet};

use ai_memory_core::{PageId, ProjectId, Tier, WorkspaceId};
use ai_memory_store::{
    DecayCandidate, DecayParams, ReaderPool, WriterHandle, retention_score_with_breadth,
};
use ai_memory_wiki::Wiki;
use jiff::Timestamp;
use serde::Serialize;
use thiserror::Error;

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
/// `retention.days` is positive.
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
    if !breadth_weight.is_finite() || breadth_weight < 0.0 {
        return Err(SweepError::InvalidBreadthWeight);
    }
    if retention.days < 0 {
        return Err(SweepError::InvalidObservationRetention);
    }
    let candidates = reader.decay_candidates(workspace_id, project_id).await?;
    let breadth =
        access_breadth_for_scoring(reader, workspace_id, project_id, breadth_weight).await?;
    let now_us = Timestamp::now().as_microsecond();

    let mut evicted = Vec::new();
    let mut expired: Vec<ExpiredPage> = Vec::new();

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
        if !is_decayable(c) {
            continue;
        }
        let age_days = elapsed_days(now_us, c.updated_at_us);
        let days_since_access = c.last_accessed_at_us.map(|us| elapsed_days(now_us, us));
        let score = retention_score_with_breadth(
            params,
            age_days,
            c.access_count,
            days_since_access,
            c.salience,
            breadth.get(&c.id).copied().unwrap_or(0),
            breadth_weight,
        );
        if score < params.cold_threshold {
            evicted.push(EvictedPage {
                id: c.id,
                path: c.path.as_str().to_string(),
                retention: score,
                age_days,
                access_count: c.access_count,
                deleted: false,
            });
        }
    }

    let mut hard_deleted = 0usize;
    let mut observations_prunable = 0usize;
    let mut observations_pruned = 0usize;
    let mut observation_prune_sessions = 0usize;
    let mut observation_prune_batches = 0usize;
    let observation_cutoff_us = Timestamp::now()
        .as_microsecond()
        .saturating_sub(retention.days.saturating_mul(US_PER_DAY_I64));
    if retention.is_enabled() {
        observations_prunable = reader
            .prunable_observation_count(workspace_id, project_id, observation_cutoff_us)
            .await?;
    }
    if !dry_run {
        for page in &mut expired {
            let path = match ai_memory_core::PagePath::new(page.path.clone()) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let result = match wiki {
                Some(w) => match w
                    .delete_page_if_latest(workspace_id, project_id, &path, page.id, None)
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

    Ok(SweepReport {
        dry_run,
        candidates_evaluated: candidates.len(),
        evicted,
        expired,
        hard_deleted,
        observations_prunable,
        observations_pruned,
        observation_prune_sessions,
        observation_prune_batches,
    })
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
        };
        assert!(is_decayable(&c));
    }

    #[test]
    fn elapsed_days_clamps_future_timestamps() {
        assert_eq!(elapsed_days(1_000, 2_000), 0.0);
        assert!(elapsed_days(US_PER_DAY as i64, 0) >= 1.0);
    }
}
