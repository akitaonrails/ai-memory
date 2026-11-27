//! Process-memory state for `/admin/compact`.
//!
//! The `VACUUM` runs on the writer actor and can outlast a proxy's timeout, so
//! the route can start a compaction and answer at once, and a `GET` reports how
//! it is going. The state lives outside the writer and SQLite on purpose: the
//! `GET` has to answer while the writer is busy with the very `VACUUM` it
//! reports on. A restart forgets it.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::admin::CompactReport;

/// Shared handle to the single compaction slot. Cloning it shares the slot.
#[derive(Debug, Clone, Default)]
pub struct CompactJobs {
    inner: Arc<Mutex<Slot>>,
}

#[derive(Debug, Default)]
struct Slot {
    /// The last job this process started; `None` before the first one.
    latest: Option<Job>,
    /// Duration of the last succeeded run. Kept when a newer job starts, so
    /// the UI can show an estimate while that job is still running.
    previous_duration: Option<Duration>,
    /// Source of job ids for this process.
    issued: u64,
}

#[derive(Debug)]
struct Job {
    id: String,
    started_at: jiff::Timestamp,
    started: Instant,
    database_bytes_at_start: Option<u64>,
    reclaimable_bytes_at_start: Option<u64>,
    outcome: Outcome,
}

#[derive(Debug)]
enum Outcome {
    Running,
    Finished {
        finished_at: jiff::Timestamp,
        elapsed: Duration,
        result: Result<CompactReport, String>,
    },
}

/// Wire form of one compaction job, returned by `GET /admin/compact`, by the
/// background `POST` (202), and by the single-flight conflict (409).
#[derive(Debug, Clone, Serialize)]
pub struct CompactJobStatus {
    pub job_id: Option<String>,
    pub state: CompactJobState,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub elapsed_ms: Option<u64>,
    pub database_bytes_at_start: Option<u64>,
    pub reclaimable_bytes_at_start: Option<u64>,
    pub previous_duration_ms: Option<u64>,
    pub report: Option<CompactReport>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactJobState {
    Idle,
    Running,
    Succeeded,
    Failed,
}

impl CompactJobs {
    fn lock(&self) -> MutexGuard<'_, Slot> {
        // Nothing holding this lock panics, so a poisoned guard still holds
        // consistent state; keep serving it rather than wedging the route.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Claim the slot for a new run. The running check and the transition to
    /// `running` happen under one lock acquisition, so two simultaneous
    /// requests cannot both start a compaction.
    ///
    /// Returns the job id and its starting status, or the running job's status
    /// when one already holds the slot.
    pub(crate) fn try_start(
        &self,
        database_bytes_at_start: Option<u64>,
        reclaimable_bytes_at_start: Option<u64>,
    ) -> Result<(String, CompactJobStatus), Box<CompactJobStatus>> {
        let mut slot = self.lock();
        if slot.is_running() {
            return Err(Box::new(slot.snapshot()));
        }
        slot.issued += 1;
        let id = format!("compact-{}", slot.issued);
        slot.latest = Some(Job {
            id: id.clone(),
            started_at: jiff::Timestamp::now(),
            started: Instant::now(),
            database_bytes_at_start,
            reclaimable_bytes_at_start,
            outcome: Outcome::Running,
        });
        Ok((id, slot.snapshot()))
    }

    /// Record how job `job_id` ended. The slot is only ever claimed by one job
    /// at a time, so `job_id` is the running one; a stale id is ignored.
    pub(crate) fn finish(&self, job_id: &str, result: Result<CompactReport, String>) {
        let mut slot = self.lock();
        let Some(job) = slot.latest.as_mut() else {
            return;
        };
        if job.id != job_id || !matches!(job.outcome, Outcome::Running) {
            return;
        }
        let elapsed = job.started.elapsed();
        job.outcome = Outcome::Finished {
            finished_at: jiff::Timestamp::now(),
            elapsed,
            result: result.clone(),
        };
        if result.is_ok() {
            slot.previous_duration = Some(elapsed);
        }
    }

    /// Current status for `GET /admin/compact`.
    pub(crate) fn status(&self) -> CompactJobStatus {
        self.lock().snapshot()
    }
}

impl Slot {
    fn is_running(&self) -> bool {
        matches!(
            self.latest,
            Some(Job {
                outcome: Outcome::Running,
                ..
            })
        )
    }

    fn snapshot(&self) -> CompactJobStatus {
        match &self.latest {
            Some(job) => self.describe(job),
            None => CompactJobStatus {
                job_id: None,
                state: CompactJobState::Idle,
                started_at: None,
                finished_at: None,
                elapsed_ms: None,
                database_bytes_at_start: None,
                reclaimable_bytes_at_start: None,
                previous_duration_ms: None,
                report: None,
                error: None,
            },
        }
    }

    fn describe(&self, job: &Job) -> CompactJobStatus {
        let (state, finished_at, elapsed, report, error) = match &job.outcome {
            Outcome::Running => (
                CompactJobState::Running,
                None,
                job.started.elapsed(),
                None,
                None,
            ),
            Outcome::Finished {
                finished_at,
                elapsed,
                result: Ok(report),
            } => (
                CompactJobState::Succeeded,
                Some(*finished_at),
                *elapsed,
                Some(report.clone()),
                None,
            ),
            Outcome::Finished {
                finished_at,
                elapsed,
                result: Err(error),
            } => (
                CompactJobState::Failed,
                Some(*finished_at),
                *elapsed,
                None,
                Some(error.clone()),
            ),
        };
        CompactJobStatus {
            job_id: Some(job.id.clone()),
            state,
            started_at: Some(job.started_at.to_string()),
            finished_at: finished_at.map(|t| t.to_string()),
            elapsed_ms: Some(millis(elapsed)),
            database_bytes_at_start: job.database_bytes_at_start,
            reclaimable_bytes_at_start: job.reclaimable_bytes_at_start,
            previous_duration_ms: self.previous_duration.map(millis),
            report,
            error,
        }
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
