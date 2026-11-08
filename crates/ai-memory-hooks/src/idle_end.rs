//! Server-side idle session auto-end.
//!
//! A harness that crashes, gets killed, or times out mid-task never delivers
//! its `SessionEnd`: the session row stays open forever, its observations are
//! never consolidated, and no one decides whether its summary page was worth
//! keeping. When `[sessions] idle_end_after_hours` is set, the `ai-memory
//! serve` maintenance scheduler runs [`run_idle_end_tick`] every
//! [`IDLE_END_INTERVAL_SECS`]: it ends the open sessions whose last activity
//! is older than the cutoff with the effects a real `SessionEnd` would have
//! had — summary page, consolidation job, watermark — and re-opens a handoff
//! that an ended boundary-only session had claimed for its owner.
//!
//! The baton is the hook path's decision: the tick never creates a handoff.
//! What the server itself checks are the idle threshold — the session's
//! last activity is older than `[sessions] idle_end_after_hours` — and the
//! active-run exclusion: a session bound to a `state = 'active'` managed run
//! is left to that run's own lifecycle.
//!
//! The tick never owns a session a live hook owns. Selection is a single
//! read that also captures each session page's current version; a real
//! `SessionEnd` landing between that read and the tick's write wins at every
//! seam:
//! - the substantive summary page is written conditionally on the version
//!   seen at selection ([`ai_memory_wiki::ConditionalWrite`]) — a page the
//!   hook delivered after the selection is a precondition mismatch, so the
//!   session is skipped entirely;
//! - the end stamp re-verifies, inside its CAS, that the session is still
//!   open in its scope, that the session's observation count (every row of
//!   the session, whatever project each landed in) still equals the count
//!   read in the digest's snapshot, and that the latest page version is still
//!   the one the tick wrote or kept;
//! - the lifecycle-only end applies the same scope/open guards before any
//!   mutation and re-checks substantiveness atomically;
//! - sessions bound to an active managed run are excluded before any write,
//!   and the caller over-fetches the selection (`IDLE_END_BATCH` plus the
//!   number of active-run sessions) so the exclusion cannot starve the
//!   batch behind persistently excluded sessions;
//! - attribution follows the session's stored owner, parsed with the same
//!   fail-closed semantics as the `SessionEnd` path.

use std::collections::HashSet;

use ai_memory_core::{PageId, SessionId};
use ai_memory_store::{
    IdleEndAction, IdleEndCandidate, IdleEndResult, PagePrecondition, ReaderPool, WriterHandle,
};
use ai_memory_wiki::{ConditionalWrite, Wiki, WritePageRequest};
use tracing::{debug, info, warn};

use crate::router::stored_session_identity;
use crate::synth::{SessionDigest, synthesize_session_page};

/// Bounded per-tick selection: one tick ends at most this many sessions; the
/// next picks up the rest. Keeps a backlog of long-idle sessions from
/// turning one tick into an unbounded burst of page writes and
/// consolidation jobs.
pub const IDLE_END_BATCH: usize = 25;

/// Fixed tick interval. The scan is stateless and idempotent (the
/// `ended_at IS NULL` CAS plus the conditional page write make a double
/// end impossible), so a fixed interval needs no persisted cadence state.
pub const IDLE_END_INTERVAL_SECS: u64 = 15 * 60;

/// What one idle auto-end tick did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IdleEndTickOutcome {
    /// Substantive sessions ended (summary page + consolidation job).
    pub ended: usize,
    /// Boundary-only sessions ended (no page, no job; a handoff they had
    /// claimed is re-opened for its owner).
    pub lifecycle_only: usize,
    /// Sessions skipped: an active managed run owns them, their page write
    /// lost to a real `SessionEnd`, their end CAS lost, or their stored
    /// owner is corrupt.
    pub skipped: usize,
    /// Summary pages the tick wrote.
    pub pages_written: usize,
}

/// Everything one tick needs. `sessions` is the selection the serve loop
/// made (oldest last-activity first); the serve loop over-fetches it —
/// [`IDLE_END_BATCH`] plus the number of active-run sessions — so the
/// exclusion of active-run sessions cannot starve the batch. The page
/// version each entry carries is the conditional write's precondition.
pub struct IdleEndTickInput<'a> {
    /// Writer handle for the batched end command.
    pub writer: &'a WriterHandle,
    /// Reader pool for the per-session reads.
    pub reader: &'a ReaderPool,
    /// Wiki for the conditional page write and the auto-commit.
    pub wiki: &'a Wiki,
    /// Selected idle sessions.
    pub sessions: Vec<ai_memory_store::IdleOpenSession>,
    /// The tick's clock (Unix microseconds): the `ended_at` of every end it
    /// stamps, so the auto-improvement discovery (ordered by `ended_at`)
    /// finds auto-ended sessions in the order they were ended.
    pub now_us: i64,
    /// Server `consolidate_on_session_end` flag.
    pub consolidate_on_session_end: bool,
    /// Whether a consolidator is configured; the enqueue only happens when
    /// this and `consolidate_on_session_end` are both true, mirroring the
    /// hook path.
    pub has_consolidator: bool,
}

/// What the summary-page step decided for one substantive session.
enum PageDecision {
    /// The tick wrote the rule-based summary; the id is current.
    Written(PageId),
    /// The agent's own page is kept as-is (no write); the id is current.
    KeptAgentPage(PageId),
    /// A real `SessionEnd` (or another writer) landed at the path after the
    /// selection: the session is skipped entirely.
    Superseded,
}

/// Run one idle auto-end tick over a pre-selected batch of idle sessions.
///
/// See the module docs for the race guarantees. Selection happens in the
/// serve loop (one reader call); this function only classifies, writes the
/// summary pages conditionally, and stamps the ends in one writer command.
///
/// # Errors
/// Propagates store or wiki errors (a failed read/write leaves the affected
/// sessions open for the next tick); per-session races are reported in
/// [`IdleEndTickOutcome`], not as errors.
pub async fn run_idle_end_tick(input: &IdleEndTickInput<'_>) -> anyhow::Result<IdleEndTickOutcome> {
    let mut outcome = IdleEndTickOutcome::default();
    if input.sessions.is_empty() {
        return Ok(outcome);
    }
    // A live managed run owns its native session's lifecycle (its end comes
    // from the run's own completion), so its sessions are never idle-ended
    // even when the session row looks quiet. The join is done in Rust —
    // `managed_runs.native_session_id` holds the harness's native id string
    // while `sessions.id` is the derived UUID — via the one shared rule,
    // `SessionId::from_native`.
    let active_runs: HashSet<SessionId> = input
        .reader
        .active_managed_run_native_session_ids()
        .await?
        .iter()
        .map(|raw| SessionId::from_native(raw))
        .collect();
    let enqueue_consolidation = input.consolidate_on_session_end && input.has_consolidator;

    // Exclude active-run sessions BEFORE consuming the batch bound: the
    // caller over-fetched the selection past the excluded ones, so a
    // backlog of persistently excluded sessions cannot starve the eligible
    // ones behind them.
    let eligible: Vec<&ai_memory_store::IdleOpenSession> = input
        .sessions
        .iter()
        .filter(|session| {
            if active_runs.contains(&session.session_id) {
                outcome.skipped += 1;
                debug!(
                    session = %session.session_id,
                    "idle end: session is bound to an active managed run; leaving it to the run's own lifecycle"
                );
                false
            } else {
                true
            }
        })
        .take(IDLE_END_BATCH)
        .collect();

    let mut candidates = Vec::new();
    for session in eligible {
        let substantive = input
            .reader
            .session_has_substantive_observation(
                session.workspace_id,
                session.project_id,
                session.session_id,
            )
            .await?;
        if !substantive {
            candidates.push(IdleEndCandidate {
                session_id: session.session_id,
                workspace_id: session.workspace_id,
                project_id: session.project_id,
                agent_kind: session.agent_kind,
                action: IdleEndAction::LifecycleOnly {
                    now_us: input.now_us,
                    last_activity_us: session.last_activity_us,
                },
            });
            continue;
        }
        let (page, observation_count) = match summary_page_decision(input, session).await {
            Ok((PageDecision::Written(page_id), count)) => {
                outcome.pages_written += 1;
                (page_id, count)
            }
            Ok((PageDecision::KeptAgentPage(page_id), count)) => (page_id, count),
            Ok((PageDecision::Superseded, _)) => {
                outcome.skipped += 1;
                continue;
            }
            Err(error) => {
                warn!(
                    session = %session.session_id,
                    error = %error,
                    "idle end: summary page failed; session stays open for the next tick"
                );
                outcome.skipped += 1;
                continue;
            }
        };
        candidates.push(IdleEndCandidate {
            session_id: session.session_id,
            workspace_id: session.workspace_id,
            project_id: session.project_id,
            agent_kind: session.agent_kind,
            action: IdleEndAction::Substantive {
                expected_page_id: page,
                observation_count,
                now_us: input.now_us,
                last_activity_us: session.last_activity_us,
            },
        });
    }
    if candidates.is_empty() {
        return Ok(outcome);
    }
    // One writer command for the whole batch: each candidate is stamped by
    // its own guarded write (a real SessionEnd winning the race reports
    // Lost, never an error), and each substantive end enqueues its
    // consolidation job inside the same command.
    let session_ids: Vec<SessionId> = candidates.iter().map(|c| c.session_id).collect();
    let results = input
        .writer
        .end_idle_sessions(candidates, enqueue_consolidation)
        .await?;
    for (session_id, result) in session_ids.into_iter().zip(results) {
        match result {
            IdleEndResult::EndedSubstantive => outcome.ended += 1,
            IdleEndResult::EndedLifecycleOnly => outcome.lifecycle_only += 1,
            IdleEndResult::Lost => {
                outcome.skipped += 1;
                debug!(
                    session = %session_id,
                    "idle end: a real SessionEnd won the race; the hook's end owns the session"
                );
            }
        }
    }
    // Auto-commit the wiki tree so the session page changes land in git in
    // one snapshot, the same way the SessionEnd path does.
    if outcome.pages_written > 0 {
        match input.wiki.commit_all(&format!(
            "idle session end: {} session(s)",
            outcome.pages_written
        )) {
            Ok(Some(oid)) => debug!(commit = %oid, "idle session end auto-commit"),
            Ok(None) => debug!("wiki clean after idle session end"),
            Err(e) => warn!(error = %e, "idle session end auto-commit failed"),
        }
    }
    if outcome.ended + outcome.lifecycle_only > 0 {
        info!(
            ended = outcome.ended,
            lifecycle_only = outcome.lifecycle_only,
            skipped = outcome.skipped,
            pages_written = outcome.pages_written,
            "idle session auto-end tick"
        );
    }
    Ok(outcome)
}

/// Write — or keep — one substantive session's summary page exactly the way
/// the `SessionEnd` path does, but conditional on the page version the
/// selection saw, so a real `SessionEnd` that delivered its page after the
/// selection is never superseded by the rule-based summary.
async fn summary_page_decision(
    input: &IdleEndTickInput<'_>,
    session: &ai_memory_store::IdleOpenSession,
) -> anyhow::Result<(PageDecision, i64)> {
    // The agent's own page wins over the rule-based summary, the same rule
    // the SessionEnd path applies at its end: when the agent wrote
    // `sessions/<id>.md` itself, the automatic writer keeps it. Its current
    // version is read now (keeping means not writing, so a late write
    // cannot be superseded); a missing row falls through to the
    // rule-based summary, as the SessionEnd path does.
    let agent_page_current = if input.wiki.session_page_written_by_agent(
        session.workspace_id,
        session.project_id,
        session.session_id,
    )? {
        input
            .reader
            .latest_page_id_by_ids(
                session.workspace_id,
                session.project_id,
                session_page_path(session.session_id),
            )
            .await?
    } else {
        None
    };
    if let Some(page_id) = agent_page_current {
        debug!(
            session = %session.session_id,
            "idle end: keeping the agent's own session page over the rule-based summary"
        );
        // The kept page is not tied to a digest generation, but the stamp's
        // count guard still needs a count read at decision time: a non-end
        // observation landing after this read loses the CAS (the session
        // retries on the next tick) instead of silently widening the
        // watermark.
        let observation_count = input
            .reader
            .session_observation_count(session.workspace_id, session.project_id, session.session_id)
            .await?;
        return Ok((PageDecision::KeptAgentPage(page_id), observation_count));
    }
    // Read after the decision above, so anything captured in between is in
    // it — the same order the SessionEnd path uses. The count the digest is
    // built from is read in the SAME snapshot, so the stamp's count guard
    // (the window between this read and the stamp) is checked against
    // exactly what the page represents.
    let (digest, observation_count) = SessionDigest::load_with_count(
        input.reader,
        session.workspace_id,
        session.project_id,
        session.session_id,
    )
    .await?;
    let page = synthesize_session_page(
        session.workspace_id,
        session.project_id,
        session.session_id,
        session.agent_kind,
        &digest,
    );
    // Attribute the write to the operator who OWNED the session, read back
    // from the session row — the server process is the writer, not the
    // author.
    let actor = stored_session_identity(session.actor_user.clone())?.1;
    // The precondition is the version the tick SAW WHEN IT READ THE
    // SESSION (the selection), captured before any write in this tick: a
    // real SessionEnd that lands the page after the selection changes the
    // latest version and this write must back off.
    let precondition = match session.seen_page_id {
        Some(seen) => PagePrecondition::Latest(seen),
        None => PagePrecondition::Absent,
    };
    match input
        .wiki
        .write_page_if(
            WritePageRequest {
                workspace_id: page.workspace_id,
                project_id: page.project_id,
                path: page.path.clone(),
                frontmatter: page.frontmatter_json.clone(),
                body: page.body.clone(),
                tier: page.tier,
                pinned: page.pinned,
                title: None,
                admission_ctx: None,
                author_id: None,
                actor,
                evidence: Vec::new(),
            },
            precondition,
        )
        .await
    {
        Ok(ConditionalWrite::Written(page_id)) => {
            Ok((PageDecision::Written(page_id), observation_count))
        }
        Ok(ConditionalWrite::Mismatch { current }) => {
            debug!(
                session = %session.session_id,
                current = ?current,
                "idle end: the session page moved after the selection; skipping the session"
            );
            Ok((PageDecision::Superseded, observation_count))
        }
        Err(error) => Err(error.into()),
    }
}

/// The session page path, the same `sessions/<id>.md` rule the rest of the
/// automatic writers use.
fn session_page_path(session_id: SessionId) -> String {
    format!("sessions/{session_id}.md")
}
