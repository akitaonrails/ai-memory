//! Persisted cadence state for global maintenance jobs.

use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::StoreResult;

/// Global maintenance jobs with persisted cadence. Embedding is intentionally
/// absent: it remains opt-in and has no startup catch-up behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceJob {
    /// Retention forget sweep across every existing scope.
    ForgetSweep,
    /// Rule-based lint across every existing scope.
    RuleLint,
    /// Lint with the LLM contradiction pass across every existing scope
    /// (`[maintenance] lint_llm_interval_secs`, #2021).
    LlmLint,
}

impl MaintenanceJob {
    /// Stable database key for this job.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ForgetSweep => "forget_sweep",
            Self::RuleLint => "rule_lint",
            Self::LlmLint => "llm_lint",
        }
    }
}

/// Return the last successful completion time for a maintenance job.
pub fn last_success(conn: &Connection, job: MaintenanceJob) -> StoreResult<Option<i64>> {
    conn.query_row(
        "SELECT last_success_at FROM maintenance_scheduler_state WHERE job = ?1",
        params![job.as_str()],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// Record a job completion after its work has succeeded.
pub fn record_success(conn: &Connection, job: MaintenanceJob) -> StoreResult<()> {
    conn.execute(
        "INSERT INTO maintenance_scheduler_state (job, last_success_at) VALUES (?1, ?2) \
         ON CONFLICT(job) DO UPDATE SET last_success_at = excluded.last_success_at",
        params![job.as_str(), Timestamp::now().as_microsecond()],
    )?;
    Ok(())
}
