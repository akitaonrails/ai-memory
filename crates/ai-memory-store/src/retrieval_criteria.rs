//! Per-project retrieval criteria (#2038).
//!
//! A project can carry retrieval defaults forwarded from the nearest
//! `.ai-memory.toml` `[retrieval]` section and persisted on its `projects`
//! row, so `memory_query` need not repeat the argument on every call. A field
//! left `None` inherits: the project defers to the global `[retrieval]`
//! default, then to the built-in. The MCP query path resolves the effective
//! value per field under `call argument → project → global → built-in`.
//!
//! This mirrors the `[profile]` per-project flags ([`crate::profile`]): the
//! value is forwarded on the hook query string and written to a `projects`
//! column, read back here, and the row is shared across every session and
//! operator in the project (invariant #16 — a per-project setting, never a
//! per-author read filter).

use ai_memory_core::{ProjectId, WorkspaceId};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::StoreResult;

/// Upper bound on a persisted or per-call result limit, matching the clamp the
/// MCP `memory_query` path already applies to an explicit `limit`.
pub const MAX_RESULT_LIMIT: u32 = 100;

/// A project's persisted retrieval criteria. `None` inherits the global
/// default; see the module docs for the precedence chain.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RetrievalCriteria {
    /// Default number of hits `memory_query` returns for this project when a
    /// call gives no explicit `limit`. Stored `1..=MAX_RESULT_LIMIT`; `None`
    /// inherits the global `[retrieval] default_result_limit`.
    pub default_result_limit: Option<u32>,
    /// Lowest calibrated reranker relevance a `memory_query` hit may have for
    /// this project when a call gives no explicit `min_relevance`; hits below
    /// it are excluded before the count limit applies. Stored `0.0..=1.0`;
    /// `None` inherits the global `[retrieval] min_relevance`, then no floor.
    /// The threshold keys off the reranker's calibrated score only, so a query
    /// with no reranker configured rejects a `min_relevance` rather than
    /// applying it to the uncalibrated RRF rank.
    pub min_relevance: Option<f32>,
}

impl RetrievalCriteria {
    /// Whether every field is unset, so nothing needs to be persisted. A
    /// marker that declares no retrieval criteria leaves the row untouched.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.default_result_limit.is_none() && self.min_relevance.is_none()
    }
}

/// Persist `criteria` on `project_id`. The session-start path calls this only
/// when a forwarded value differs from the stored one; the NULL-safe `IS NOT`
/// guard keeps a racing duplicate write a no-op. A `None` field clears the
/// column back to "inherit".
pub(crate) fn set_project_retrieval_criteria(
    conn: &Connection,
    project_id: ProjectId,
    criteria: RetrievalCriteria,
) -> StoreResult<()> {
    conn.execute(
        "UPDATE projects SET default_result_limit = ?2, min_relevance = ?3 \
         WHERE id = ?1 \
           AND (default_result_limit IS NOT ?2 OR min_relevance IS NOT ?3)",
        params![
            project_id.as_bytes(),
            criteria.default_result_limit.map(i64::from),
            criteria.min_relevance.map(f64::from),
        ],
    )?;
    Ok(())
}

/// The stored criteria of a project; all-inherit for an unknown project. A
/// stored value outside its valid range (not reachable through the forwarding
/// path or the `CHECK` constraint) is read as "inherit" rather than trusted.
pub(crate) fn project_retrieval_criteria(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> StoreResult<RetrievalCriteria> {
    let stored: Option<(Option<i64>, Option<f64>)> = conn
        .query_row(
            "SELECT default_result_limit, min_relevance FROM projects \
             WHERE id = ?1 AND workspace_id = ?2",
            params![project_id.as_bytes(), workspace_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (default_result_limit, min_relevance) = match stored {
        Some((limit, relevance)) => (
            limit.and_then(sanitize_stored_limit),
            relevance.and_then(sanitize_stored_min_relevance),
        ),
        None => (None, None),
    };
    Ok(RetrievalCriteria {
        default_result_limit,
        min_relevance,
    })
}

/// A stored limit is trusted only inside `1..=MAX_RESULT_LIMIT`; anything else
/// (a hand-edited DB row that slipped past the `CHECK`) inherits.
fn sanitize_stored_limit(raw: i64) -> Option<u32> {
    u32::try_from(raw)
        .ok()
        .filter(|&v| (1..=MAX_RESULT_LIMIT).contains(&v))
}

/// A stored threshold is trusted only inside `0.0..=1.0` and finite; anything
/// else (a hand-edited DB row past the `CHECK`, or a stray NaN) inherits.
fn sanitize_stored_min_relevance(raw: f64) -> Option<f32> {
    (raw.is_finite() && (0.0..=1.0).contains(&raw)).then_some(raw as f32)
}

#[cfg(test)]
mod tests {
    use super::{MAX_RESULT_LIMIT, sanitize_stored_limit, sanitize_stored_min_relevance};

    #[test]
    fn sanitize_rejects_out_of_range_and_keeps_edges() {
        assert_eq!(sanitize_stored_limit(1), Some(1));
        assert_eq!(
            sanitize_stored_limit(i64::from(MAX_RESULT_LIMIT)),
            Some(MAX_RESULT_LIMIT)
        );
        // Values the `CHECK` constraint would reject, read defensively as
        // "inherit" rather than trusted: zero, over the cap, and negatives.
        assert_eq!(sanitize_stored_limit(0), None);
        assert_eq!(sanitize_stored_limit(i64::from(MAX_RESULT_LIMIT) + 1), None);
        assert_eq!(sanitize_stored_limit(-5), None);
    }

    #[test]
    fn sanitize_min_relevance_rejects_out_of_range_and_keeps_edges() {
        assert_eq!(sanitize_stored_min_relevance(0.0), Some(0.0));
        assert_eq!(sanitize_stored_min_relevance(1.0), Some(1.0));
        assert_eq!(sanitize_stored_min_relevance(0.5), Some(0.5));
        // Past the `CHECK` or otherwise nonsensical: read as "inherit".
        assert_eq!(sanitize_stored_min_relevance(-0.1), None);
        assert_eq!(sanitize_stored_min_relevance(1.1), None);
        assert_eq!(sanitize_stored_min_relevance(f64::NAN), None);
        assert_eq!(sanitize_stored_min_relevance(f64::INFINITY), None);
    }
}
