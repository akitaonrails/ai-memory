//! External-edit reconciler: heal the SQLite index from the Outl SoT.
//!
//! The user can edit ai-memory pages inside the Outl app (TUI, desktop,
//! or by editing the projected `.md` and running `outl serve`). Those
//! edits land in the op log under another actor; this task periodically
//! scans every ai-memory-owned page and re-indexes the ones whose
//! content no longer matches the `ai-memory-sha` stamp written at
//! projection time. Pages only ai-memory touched are skipped, so the
//! index keeps serving the ORIGINAL document body for them.
//!
//! The stamp also encodes the heal contract for missing index rows:
//! a page whose stamp matches its content but has no row was dropped
//! by the decay sweep on purpose and stays untouched, while a page
//! with a missing/diverged stamp and no row (failed index write after
//! a SoT write, or a page created inside Outl under our prefix) is
//! indexed fresh from the SoT copy.
//!
//! The scan runs over a long-lived in-process `Workspace` whose reader
//! merges every `ops-*.jsonl` on boot; to observe writes from OTHER
//! actors made after boot, the actor thread's storage layer picks them
//! up on read (JSONL merge). Known limitation: an incremental
//! `ops_since` cursor API in outl-core would make this cheaper than the
//! current full-page rescan; at local scale the rescan is fine.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use ai_memory_core::{NewPage, Tier};

use super::{OutlContentBackend, sha256_hex, slug};

/// Outcome of one reconcile pass, for logs and tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcileOutcome {
    /// Pages scanned (everything under the slug prefix).
    pub scanned: usize,
    /// Pages whose content diverged and were (re-)indexed.
    pub reindexed: usize,
}

/// Run one reconcile pass now. Exposed for tests and for a manual
/// `reconcile` trigger; the background task just calls this in a loop.
pub async fn run_once(backend: &OutlContentBackend) -> Result<ReconcileOutcome, String> {
    let mut outcome = ReconcileOutcome::default();
    // Pick up ops other actors appended since our last look (the
    // materialized tree only sees them on a re-open).
    if backend.handle().refresh_external().await? {
        tracing::debug!("outl reconcile: workspace refreshed from external ops");
    }
    let all_prefix = format!("{}~", backend.slug_prefix.as_str());
    let slugs = backend.handle().list_owned(all_prefix).await?;

    for page_slug in slugs {
        let Some((ws, proj, path)) = slug::decode(backend.slug_prefix.as_str(), &page_slug) else {
            continue;
        };
        outcome.scanned += 1;

        let Some(content) = backend.handle().read_owned(page_slug.clone()).await? else {
            continue;
        };
        let current_sha = sha256_hex(&content.body);
        if content.stored_sha.as_deref() == Some(current_sha.as_str()) {
            // Untouched projection: the index either has the original
            // body already, or deliberately dropped the row (decay
            // sweep). Either way the pass leaves the page alone, so
            // decayed pages never resurrect.
            continue;
        }

        // Stamp missing or diverged: an external edit, or an index
        // write that failed right after the SoT write (persist drops
        // the stamp). Merge the Outl body onto the existing index row
        // when there is one (preserving frontmatter/tier/pinned), or
        // index the page fresh from the SoT copy when there is none.
        let existing = backend
            .reader
            .page_body_by_ids(ws, proj, path.as_str())
            .await
            .map_err(|e| e.to_string())?;
        let (frontmatter, tier, pinned) = match &existing {
            Some(row) => (
                serde_json::from_str(&row.frontmatter_json).unwrap_or(serde_json::Value::Null),
                Tier::from_str(&row.tier).unwrap_or(Tier::Semantic),
                row.pinned,
            ),
            None => {
                let tier = content
                    .tier
                    .as_deref()
                    .and_then(|t| Tier::from_str(t).ok())
                    .unwrap_or(Tier::Semantic);
                (serde_json::json!({ "tier": tier.as_str() }), tier, false)
            }
        };
        let page = NewPage {
            workspace_id: ws,
            project_id: proj,
            path,
            title: content.title,
            body: content.body,
            tier,
            frontmatter_json: frontmatter,
            pinned,
            // Wikilink extraction lives in the wiki layer; externally
            // edited pages temporarily lose graph edges until their
            // next engine write. TODO: share the extractor.
            links: Vec::new(),
            author_id: None,
        };
        backend
            .writer
            .upsert_page(page)
            .await
            .map_err(|e| e.to_string())?;
        backend
            .handle()
            .mark_reconciled(page_slug, current_sha)
            .await?;
        outcome.reindexed += 1;
    }
    Ok(outcome)
}

/// Spawn the background reconcile loop. Returns the task handle; abort
/// it on shutdown.
pub fn spawn(backend: Arc<OutlContentBackend>, interval: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            match run_once(&backend).await {
                Ok(outcome) if outcome.reindexed > 0 => {
                    tracing::info!(
                        scanned = outcome.scanned,
                        reindexed = outcome.reindexed,
                        "outl reconcile: absorbed external edits"
                    );
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "outl reconcile pass failed"),
            }
        }
    })
}
