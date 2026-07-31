//! M8 forget sweep — episodic-only retention pass.
//!
//! Walks the `is_latest = 1` pages for a project, computes the retention score
//! for each via [`ai_memory_store::retention_score_with_breadth`],
//! and soft-deletes those below threshold. Semantic / procedural /
//! working tiers are skipped (M8 policy: semantic compounds, only
//! episodic decays). Pinned pages (schema flag OR `pinned: true` in
//! frontmatter) are exempt regardless of tier.
//!
//! TTL pass: pages whose frontmatter `expires_at:` is in the past are
//! hard-deleted through the wiki layer (file + rows), regardless of
//! tier or pin — an explicit expiry is a more explicit user statement
//! than a pin. `memory_lint` warns about pinned+expiring combos so the
//! contradiction is visible before the delete lands.
//!
//! Hard-delete pass cleans up rows soft-deleted more than
//! `hard_delete_after_days` ago that received zero subsequent access.
//! M7 supersession rows are safe: they have `supersedes IS NOT NULL`
//! and therefore never match the hard-delete predicate.

use std::collections::HashMap;

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
    /// Identifier of the soft-deleted page.
    pub id: PageId,
    /// Relative wiki path.
    pub path: String,
    /// Retention score at the time of the sweep.
    pub retention: f64,
    /// Days since the page's last update.
    pub age_days: f64,
    /// Total access count.
    pub access_count: u32,
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
    /// Pages that fell below the cold threshold (soft-deleted unless
    /// `dry_run`).
    pub evicted: Vec<EvictedPage>,
    /// Pages past their frontmatter `expires_at:` TTL (hard-deleted
    /// through the wiki layer unless `dry_run`).
    pub expired: Vec<ExpiredPage>,
    /// Number of older soft-deleted rows hard-deleted on this pass.
    pub hard_deleted: usize,
}

/// Errors raised by the sweep.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SweepError {
    /// Underlying store error.
    #[error(transparent)]
    Store(#[from] ai_memory_store::StoreError),
}

const US_PER_DAY: f64 = 86_400_000_000.0;

/// Run a sweep against the given workspace/project.
///
/// `wiki` routes TTL deletions through the wiki layer so the markdown
/// file is removed together with the rows (deleting only store rows
/// would let the watcher re-index the file). Callers without a wiki
/// handle (bare-store tests) pass `None`; expired pages are then reported
/// but left intact. A store-only delete would leave the authoritative file
/// behind for the watcher to re-index.
///
/// # Errors
/// Propagates any store error encountered while reading candidates or
/// writing soft-deletions. Per-page TTL delete failures (rejecting
/// admission webhook, IO error) are reported in the
/// [`SweepReport::expired`] entries instead of aborting the sweep.
pub async fn run_sweep(
    reader: &ReaderPool,
    writer: &WriterHandle,
    wiki: Option<&Wiki>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    params: &DecayParams,
    dry_run: bool,
) -> Result<SweepReport, SweepError> {
    let candidates = reader.decay_candidates(workspace_id, project_id).await?;
    let breadth = access_breadth_for_scoring(reader, workspace_id, project_id, params).await?;
    let now_us = Timestamp::now().as_microsecond();

    let mut evicted = Vec::new();
    let mut to_evict_ids: Vec<PageId> = Vec::new();
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
        );
        if score < params.cold_threshold {
            evicted.push(EvictedPage {
                id: c.id,
                path: c.path.as_str().to_string(),
                retention: score,
                age_days,
                access_count: c.access_count,
            });
            to_evict_ids.push(c.id);
        }
    }

    let mut hard_deleted = 0usize;
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
        if !to_evict_ids.is_empty() {
            writer.soft_delete_for_decay(to_evict_ids).await?;
        }
        // Hard-delete within the swept scope only. This used to be an
        // unscoped, database-wide DELETE issued on every non-dry sweep — even
        // when this project had nothing to evict — so sweeping one project
        // permanently removed other projects' evicted pages too.
        hard_deleted = writer
            .hard_delete_decayed(workspace_id, project_id, params.hard_delete_after_days)
            .await?;
    }

    Ok(SweepReport {
        dry_run,
        candidates_evaluated: candidates.len(),
        evicted,
        expired,
        hard_deleted,
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
    params: &DecayParams,
) -> ai_memory_store::StoreResult<HashMap<PageId, u32>> {
    if params.breadth_weight == 0.0 {
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
