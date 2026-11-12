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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetrievalCriteria {
    /// Default number of hits `memory_query` returns for this project when a
    /// call gives no explicit `limit`. Stored `1..=MAX_RESULT_LIMIT`; `None`
    /// inherits the global `[retrieval] default_result_limit`.
    pub default_result_limit: Option<u32>,
}

impl RetrievalCriteria {
    /// Whether every field is unset, so nothing needs to be persisted. A
    /// marker that declares no retrieval criteria leaves the row untouched.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.default_result_limit.is_none()
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
        "UPDATE projects SET default_result_limit = ?2 \
         WHERE id = ?1 AND default_result_limit IS NOT ?2",
        params![
            project_id.as_bytes(),
            criteria.default_result_limit.map(i64::from),
        ],
    )?;
    Ok(())
}

/// The stored criteria of a project; all-inherit for an unknown project. A
/// stored value outside `1..=MAX_RESULT_LIMIT` (not reachable through the
/// forwarding path or the `CHECK` constraint) is read as "inherit" rather than
/// trusted.
pub(crate) fn project_retrieval_criteria(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> StoreResult<RetrievalCriteria> {
    let stored: Option<Option<i64>> = conn
        .query_row(
            "SELECT default_result_limit FROM projects \
             WHERE id = ?1 AND workspace_id = ?2",
            params![project_id.as_bytes(), workspace_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    let default_result_limit = stored.flatten().and_then(sanitize_stored_limit);
    Ok(RetrievalCriteria {
        default_result_limit,
    })
}

/// A stored limit is trusted only inside `1..=MAX_RESULT_LIMIT`; anything else
/// (a hand-edited DB row that slipped past the `CHECK`) inherits.
fn sanitize_stored_limit(raw: i64) -> Option<u32> {
    u32::try_from(raw)
        .ok()
        .filter(|&v| (1..=MAX_RESULT_LIMIT).contains(&v))
}

#[cfg(test)]
mod tests {
    use super::{MAX_RESULT_LIMIT, sanitize_stored_limit};

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
}
