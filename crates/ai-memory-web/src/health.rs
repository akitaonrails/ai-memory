//! The root-only health page's data model and its checks.
//!
//! Everything here is a count, a timestamp, a configured provider/model
//! label or a fixed fix command. Nothing reads page bodies, prompts,
//! observations, scope names, provider error text or credentials, so the page
//! cannot leak them however the counters move.
//!
//! The hook counters are the process [`IngestMetrics`] that
//! `GET /admin/metrics` and `ai-memory status` read; the fix lines come from
//! [`ai_memory_core::fix_lines`], the same constants `ai-memory status`
//! prints.

use std::sync::Arc;

use ai_memory_core::{IngestMetrics, IngestMetricsSnapshot, fix_lines};
use ai_memory_llm::{ProviderHealth, ProviderHealthSnapshot, ProviderHealthStatus};
use ai_memory_store::{DerivedIndexStatus, SessionConsolidationQueue, StorageStatus};

/// Process-side state the server holds outside SQLite, read once per page
/// load. `serve` implements it over the writer queue, the local hook spool
/// and the recovery journal in its data directory.
pub trait HealthProbe: Send + Sync {
    /// Write-queue depth, local spool and recovery journal, as far as this
    /// process can see them.
    fn process_state(&self) -> ProcessState;
}

/// Process-side state for one page load. `None` means the server cannot
/// see that part (a hook client on another machine spools there, not here).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessState {
    /// `(queued, capacity)` of the single writer's channel.
    pub write_queue: Option<(usize, usize)>,
    /// The hook spool under this server's data directory.
    pub spool: Option<SpoolState>,
    /// The recovery journal under this server's data directory.
    pub journal: Option<JournalState>,
}

/// Local hook spool counts. Content-free, like the spool's own health read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpoolState {
    /// Events queued for delivery.
    pub pending: usize,
    /// Age of the oldest queued event, in milliseconds.
    pub oldest_age_ms: Option<u64>,
    /// Failed delivery attempts summed over the queued events.
    pub retries_total: u64,
    /// Most events the spool holds before it drops newly captured events.
    pub capacity: usize,
}

/// Recovery journal counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JournalState {
    /// Journaled runs waiting for `ai-memory recover`.
    pub pending: usize,
    /// Of those, runs that degraded mid-flight.
    pub degraded: usize,
    /// Of those, runs whose finish step failed.
    pub finish_failed: usize,
    /// The journal exists but could not be read. Its error text is not kept:
    /// it can carry a path.
    pub unreadable: bool,
}

/// What `serve` hands the web state for the health page.
#[derive(Clone)]
pub struct HealthSources {
    /// The hook ingestion counters the `/hook` router records into.
    pub ingest: Arc<IngestMetrics>,
    /// Passive provider health the LLM and embedder wrappers record into.
    pub providers: ProviderHealth,
    /// Writer queue, spool and journal.
    pub probe: Arc<dyn HealthProbe>,
}

/// The configured embedder's identity, as the vector stream matches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedderIdentity {
    /// Provider label.
    pub provider: String,
    /// Model identity stored next to each vector.
    pub model: String,
    /// Vector dimensionality.
    pub dim: u32,
}

/// Every input of one health page load.
#[derive(Debug, Clone)]
pub struct HealthSnapshot {
    /// Hook counters since the server started.
    pub ingest: IngestMetricsSnapshot,
    /// LLM and embedding provider health.
    pub providers: ProviderHealthSnapshot,
    /// Writer queue, spool, journal.
    pub process: ProcessState,
    /// Derived-index readiness.
    pub derived: DerivedIndexStatus,
    /// Database file and reclaimable space.
    pub storage: StorageStatus,
    /// SessionEnd consolidation queue.
    pub consolidation: SessionConsolidationQueue,
    /// The configured embedder; `None` on a zero-LLM server.
    pub embedder: Option<EmbedderIdentity>,
}

/// How bad one check is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    /// Worth a look; nothing is lost yet.
    Warn,
    /// Something is failing or being lost.
    Error,
}

impl CheckStatus {
    /// Display label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// One failed check, with the command that fixes it when one exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthCheck {
    /// Stable identifier, for tests and anchors.
    pub id: &'static str,
    /// Severity.
    pub status: CheckStatus,
    /// What is wrong, with the counts that show it.
    pub detail: String,
    /// The exact command to run, when there is one.
    pub fix: Option<String>,
}

/// How old the oldest spooled event may get before the spool counts as stuck.
/// A healthy session drains at its next boundary (session start, stop,
/// pre-compact, session end) or at the threshold catch-up, well inside this.
/// An unreachable endpoint never charges an attempt, so for it age is the
/// only signal.
const SPOOL_STUCK_AFTER_MS: u64 = 15 * 60 * 1000;

fn spool_is_stuck(spool: &SpoolState) -> bool {
    spool.pending > 0
        && (spool.retries_total > 0
            || spool
                .oldest_age_ms
                .is_none_or(|age| age >= SPOOL_STUCK_AFTER_MS))
}

/// The checks that fail for `s`, most severe first. An empty list is a
/// healthy server.
#[must_use]
pub fn checks(s: &HealthSnapshot) -> Vec<HealthCheck> {
    localized_checks(&ai_memory_core::i18n::Localizer::default(), s)
}

/// Health checks rendered in the requesting operator's interface language.
#[must_use]
pub fn localized_checks(
    i18n: &ai_memory_core::i18n::Localizer,
    s: &HealthSnapshot,
) -> Vec<HealthCheck> {
    let mut out = Vec::new();
    let mut push = |id, status, detail: String, fix: Option<&str>| {
        out.push(HealthCheck {
            id,
            status,
            detail,
            fix: fix.map(str::to_owned),
        });
    };

    let shed = s.ingest.shed_saturated + s.ingest.shed_rate_limited;
    if shed > 0 {
        push(
            "hook_shed",
            CheckStatus::Warn,
            i18n.format("web-health-detail-shed-hook-event-s-were-answered-429-since-the-server-started-saturated-rate", &[("value0", format!("{}", shed)), ("value1", format!("{}", s.ingest.shed_saturated)), ("value2", format!("{}", s.ingest.shed_rate_limited))]),
            None,
        );
    }
    if s.ingest.failed > 0 {
        push(
            "hook_failed",
            CheckStatus::Error,
            i18n.format(
                "web-health-detail-hook-delivery-ies-failed-to-store-since-the-server-started",
                &[("value0", format!("{}", s.ingest.failed))],
            ),
            None,
        );
    }
    if let Some((queued, capacity)) = s.process.write_queue
        && capacity > 0
        && queued.saturating_mul(4) >= capacity.saturating_mul(3)
    {
        push(
            "write_queue",
            CheckStatus::Warn,
            i18n.format(
                "web-health-detail-the-write-queue-holds-queued-of-capacity-commands",
                &[
                    ("value0", format!("{}", queued)),
                    ("value1", format!("{}", capacity)),
                ],
            ),
            None,
        );
    }
    if let Some(spool) = s.process.spool {
        if spool.capacity > 0 && spool.pending >= spool.capacity {
            push(
                "spool_evicting",
                CheckStatus::Error,
                i18n.format("web-health-detail-the-hook-spool-is-full-events-newly-captured-events-are-dropped-until-space", &[("value0", format!("{}", spool.pending))]),
                Some(fix_lines::RECOVER),
            );
        } else if spool_is_stuck(&spool) {
            push(
                "spool_pending",
                CheckStatus::Warn,
                i18n.format("web-health-detail-hook-event-s-wait-in-the-local-spool-and-delivery-looks-stuck-oldest-failed", &[("value0", format!("{}", spool.pending)), ("value1", spool.oldest_age_ms.map_or_else(
                        || i18n.text("web-health-detail-age-unknown"),
                        |ms| i18n.format("web-health-detail-min", &[("value0", format!("{}", ms / 60_000))])
                    ).to_string()), ("value2", format!("{}", spool.retries_total))]),
                Some(fix_lines::RECOVER),
            );
        }
    }
    if let Some(journal) = s.process.journal {
        if journal.unreadable {
            push(
                "journal_unreadable",
                CheckStatus::Error,
                i18n.text("web-health-detail-the-recovery-journal-could-not-be-read"),
                Some(fix_lines::RECOVER),
            );
        } else if journal.pending > 0 {
            push(
                "journal_pending",
                CheckStatus::Warn,
                i18n.format(
                    "web-health-detail-run-s-wait-in-the-recovery-journal-degraded-finish-failed",
                    &[
                        ("value0", format!("{}", journal.pending)),
                        ("value1", format!("{}", journal.degraded)),
                        ("value2", format!("{}", journal.finish_failed)),
                    ],
                ),
                Some(fix_lines::RECOVER),
            );
        }
    }
    if s.providers.llm.status == ProviderHealthStatus::Error {
        push(
            "llm_provider",
            CheckStatus::Error,
            i18n.format(
                "web-health-detail-the-llm-provider-s-last-call-failed",
                &[(
                    "value0",
                    s.providers
                        .llm
                        .last_error_status
                        .map(|status| {
                            i18n.format(
                                "web-health-detail-http-status",
                                &[("value0", format!("{}", status))],
                            )
                        })
                        .unwrap_or_default()
                        .to_string(),
                )],
            ),
            s.providers.llm.retry_hint.as_deref(),
        );
    }
    if s.providers.embedding.status == ProviderHealthStatus::Error {
        push(
            "embedding_provider",
            CheckStatus::Error,
            i18n.format("web-health-detail-the-embedding-provider-s-last-call-failed-searches-run-without-the-vector-st", &[("value0", s.providers
                    .embedding
                    .last_error_status
                    .map(|status| i18n.format("web-health-detail-http-status", &[("value0", format!("{}", status))]))
                    .unwrap_or_default().to_string())]),
            None,
        );
    }
    if let Some(embedder) = &s.embedder {
        let stale: u64 = s
            .derived
            .embedding_triples
            .iter()
            .filter(|t| {
                t.provider != embedder.provider
                    || t.model != embedder.model
                    || t.dim != embedder.dim
            })
            .map(|t| t.count)
            .sum();
        if stale > 0 {
            push(
                "stale_vectors",
                CheckStatus::Warn,
                i18n.format("web-health-detail-stale-stored-vector-s-were-made-under-another-provider-model-or-dimension-an", &[("value0", format!("{}", stale))]),
                Some(fix_lines::EMBED_FORCE),
            );
        }
        let missing = s.derived.latest_pages_missing_embeddings;
        let failed = s.derived.embed_failures_unresolved;
        if missing > 0 || failed > 0 {
            push(
                "missing_vectors",
                CheckStatus::Warn,
                i18n.format("web-health-detail-missing-latest-page-s-have-no-vector-failed-after-a-failed-or-skipped-embed", &[("value0", format!("{}", missing)), ("value1", format!("{}", failed))]),
                Some(fix_lines::EMBED),
            );
        }
    }
    if fix_lines::reclaim_advised(s.storage.reclaimable_bytes, s.storage.database_bytes) {
        push(
            "storage_reclaim",
            CheckStatus::Warn,
            i18n.format("web-health-detail-of-the-byte-database-is-reclaimable-compacting-blocks-writes-while-it-runs", &[("value0", format!("{}", s.storage.reclaimable_bytes)), ("value1", format!("{}", s.storage.database_bytes))]),
            Some(fix_lines::COMPACT),
        );
    }
    if s.consolidation.failed > 0 {
        push(
            "consolidation_failed",
            CheckStatus::Warn,
            i18n.format(
                "web-health-detail-session-consolidation-job-s-gave-up-after-their-retries",
                &[("value0", format!("{}", s.consolidation.failed))],
            ),
            None,
        );
    }
    out.sort_by_key(|check| match check.status {
        CheckStatus::Error => 0,
        CheckStatus::Warn => 1,
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_store::EmbeddingTripleCount;

    /// A healthy server: every input present, nothing failing.
    fn healthy() -> HealthSnapshot {
        HealthSnapshot {
            ingest: IngestMetrics::default().snapshot(),
            providers: ProviderHealthSnapshot::default(),
            process: ProcessState {
                write_queue: Some((0, 1024)),
                spool: Some(SpoolState {
                    capacity: 500,
                    ..SpoolState::default()
                }),
                journal: Some(JournalState::default()),
            },
            derived: DerivedIndexStatus::default(),
            storage: StorageStatus::default(),
            consolidation: SessionConsolidationQueue::default(),
            embedder: Some(EmbedderIdentity {
                provider: "synthetic".into(),
                model: "bag-of-words-v1".into(),
                dim: 16,
            }),
        }
    }

    fn only(snapshot: &HealthSnapshot) -> HealthCheck {
        let found = checks(snapshot);
        assert_eq!(found.len(), 1, "{found:?}");
        found.into_iter().next().unwrap()
    }

    #[test]
    fn a_healthy_server_fails_no_check() {
        assert_eq!(checks(&healthy()), Vec::new());
    }

    #[test]
    fn shed_hooks_warn_without_a_fix() {
        let ingest = IngestMetrics::default();
        ingest.record_shed_rate_limited();
        let snapshot = HealthSnapshot {
            ingest: ingest.snapshot(),
            ..healthy()
        };
        let check = only(&snapshot);
        assert_eq!(
            (check.id, check.status, check.fix),
            ("hook_shed", CheckStatus::Warn, None)
        );
    }

    #[test]
    fn failed_deliveries_are_an_error() {
        let ingest = IngestMetrics::default();
        ingest.record_failed();
        let snapshot = HealthSnapshot {
            ingest: ingest.snapshot(),
            ..healthy()
        };
        let check = only(&snapshot);
        assert_eq!(
            (check.id, check.status),
            ("hook_failed", CheckStatus::Error)
        );
    }

    #[test]
    fn a_three_quarters_full_write_queue_warns() {
        let mut snapshot = healthy();
        snapshot.process.write_queue = Some((767, 1024));
        assert!(checks(&snapshot).is_empty(), "below three quarters");
        snapshot.process.write_queue = Some((768, 1024));
        assert_eq!(only(&snapshot).id, "write_queue");
    }

    #[test]
    fn a_young_spool_awaiting_its_boundary_drain_is_healthy() {
        let mut snapshot = healthy();
        snapshot.process.spool = Some(SpoolState {
            pending: 6,
            oldest_age_ms: Some(SPOOL_STUCK_AFTER_MS - 1),
            retries_total: 0,
            capacity: 500,
        });
        assert!(checks(&snapshot).is_empty());

        snapshot.process.spool = Some(SpoolState {
            pending: 6,
            oldest_age_ms: Some(1_000),
            retries_total: 1,
            capacity: 500,
        });
        assert_eq!(only(&snapshot).id, "spool_pending");

        snapshot.process.spool = Some(SpoolState {
            pending: 6,
            oldest_age_ms: None,
            retries_total: 0,
            capacity: 500,
        });
        assert_eq!(only(&snapshot).id, "spool_pending");
    }

    #[test]
    fn spool_and_journal_point_at_recover() {
        let mut snapshot = healthy();
        snapshot.process.spool = Some(SpoolState {
            pending: 3,
            oldest_age_ms: Some(SPOOL_STUCK_AFTER_MS),
            capacity: 500,
            ..SpoolState::default()
        });
        let check = only(&snapshot);
        assert_eq!(check.id, "spool_pending");
        assert_eq!(check.fix.as_deref(), Some(fix_lines::RECOVER));

        snapshot.process.spool = Some(SpoolState {
            pending: 500,
            capacity: 500,
            ..SpoolState::default()
        });
        let check = only(&snapshot);
        assert_eq!(
            (check.id, check.status),
            ("spool_evicting", CheckStatus::Error)
        );
        assert_eq!(check.fix.as_deref(), Some(fix_lines::RECOVER));

        let mut snapshot = healthy();
        snapshot.process.journal = Some(JournalState {
            pending: 2,
            ..JournalState::default()
        });
        let check = only(&snapshot);
        assert_eq!(check.id, "journal_pending");
        assert_eq!(check.fix.as_deref(), Some(fix_lines::RECOVER));

        snapshot.process.journal = Some(JournalState {
            unreadable: true,
            ..JournalState::default()
        });
        assert_eq!(only(&snapshot).id, "journal_unreadable");
    }

    #[test]
    fn an_unseen_spool_or_journal_is_not_a_failure() {
        let mut snapshot = healthy();
        snapshot.process = ProcessState::default();
        assert!(checks(&snapshot).is_empty());
    }

    #[test]
    fn provider_errors_carry_the_llm_retry_hint_only() {
        let mut snapshot = healthy();
        snapshot.providers.llm.status = ProviderHealthStatus::Error;
        snapshot.providers.llm.last_error_status = Some(429);
        snapshot.providers.llm.retry_hint = Some("ai-memory llm-test --prompt ping".into());
        snapshot.providers.llm.last_error_message = Some("sk-secret-body".into());
        let check = only(&snapshot);
        assert_eq!(check.id, "llm_provider");
        assert_eq!(
            check.fix.as_deref(),
            Some("ai-memory llm-test --prompt ping")
        );
        assert!(check.detail.contains("HTTP 429"), "{}", check.detail);
        assert!(!check.detail.contains("sk-secret-body"), "{}", check.detail);

        let mut snapshot = healthy();
        snapshot.providers.embedding.status = ProviderHealthStatus::Error;
        snapshot.providers.embedding.last_error_message = Some("sk-secret-body".into());
        let check = only(&snapshot);
        assert_eq!((check.id, check.fix.clone()), ("embedding_provider", None));
        assert!(!check.detail.contains("sk-secret-body"), "{}", check.detail);
    }

    fn triple(provider: &str, model: &str, dim: u32, count: u64) -> EmbeddingTripleCount {
        EmbeddingTripleCount {
            provider: provider.into(),
            model: model.into(),
            dim,
            count,
        }
    }

    #[test]
    fn vectors_under_another_identity_are_stale() {
        let mut snapshot = healthy();
        snapshot.derived.embedding_triples = vec![
            triple("synthetic", "bag-of-words-v1", 16, 10),
            triple("synthetic", "bag-of-words-v1", 8, 3),
            triple("openai", "bag-of-words-v1", 16, 2),
        ];
        let check = only(&snapshot);
        assert_eq!(check.id, "stale_vectors");
        assert!(check.detail.starts_with("5 stored"), "{}", check.detail);
        assert_eq!(check.fix.as_deref(), Some(fix_lines::EMBED_FORCE));
    }

    #[test]
    fn missing_vectors_point_at_embed() {
        let mut snapshot = healthy();
        snapshot.derived.latest_pages_missing_embeddings = 4;
        snapshot.derived.embed_failures_unresolved = 1;
        let check = only(&snapshot);
        assert_eq!(check.id, "missing_vectors");
        assert_eq!(check.fix.as_deref(), Some(fix_lines::EMBED));
    }

    #[test]
    fn vector_checks_need_a_configured_embedder() {
        let mut snapshot = healthy();
        snapshot.embedder = None;
        snapshot.derived.latest_pages_missing_embeddings = 4;
        snapshot.derived.embedding_triples = vec![triple("x", "y", 3, 1)];
        assert!(checks(&snapshot).is_empty());
    }

    #[test]
    fn reclaimable_storage_points_at_compact() {
        let mut snapshot = healthy();
        snapshot.storage.database_bytes = 1024 * 1024 * 1024;
        snapshot.storage.reclaimable_bytes = 300 * 1024 * 1024;
        let check = only(&snapshot);
        assert_eq!(check.id, "storage_reclaim");
        assert_eq!(check.fix.as_deref(), Some(fix_lines::COMPACT));
    }

    #[test]
    fn failed_consolidation_jobs_warn() {
        let mut snapshot = healthy();
        snapshot.consolidation.failed = 2;
        let check = only(&snapshot);
        assert_eq!((check.id, check.fix), ("consolidation_failed", None));
    }

    #[test]
    fn errors_sort_before_warnings() {
        let ingest = IngestMetrics::default();
        ingest.record_shed_saturated();
        ingest.record_failed();
        let snapshot = HealthSnapshot {
            ingest: ingest.snapshot(),
            ..healthy()
        };
        let ids: Vec<_> = checks(&snapshot).into_iter().map(|c| c.id).collect();
        assert_eq!(ids, ["hook_failed", "hook_shed"]);
    }
}
