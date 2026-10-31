//! `GET /health` — root-only server health panel.
//!
//! Reads hook counters, the writer queue, the local spool and recovery
//! journal, the consolidation queue, embedder and provider state, and runs
//! the server-side checks in [`crate::health::checks`] with their fix lines.
//! It only reads: there is no button on the page and no write path behind
//! it. Root-only on a multi-user server through the same `Capability::Admin`
//! gate as `/pending` and every `/admin/*` route, because the counters
//! describe the whole server, not one repository.

use std::sync::Arc;

use ai_memory_core::{AuthLevel, IngestMetrics};
use ai_memory_llm::{ProviderHealthSnapshot, ProviderRoleHealthSnapshot};
use askama::Template;
use axum::Extension;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};

use crate::health::{EmbedderIdentity, HealthSnapshot, ProcessState, localized_checks};
use crate::state::WebState;
use crate::templates::{HealthCheckRow, HealthRow, HealthSection, HealthView};

/// Handler for `GET /health`.
pub(crate) async fn handler(
    axum::Extension(i18n): axum::Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
) -> Response {
    if let Err(response) = super::require_admin(&i18n, &state, level).await {
        return response;
    }
    let internal = |error: ai_memory_store::StoreError| {
        tracing::error!(%error, "web health read failed");
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    };
    // The configured embedder's stored-vector triple, read once so the
    // missing-vector count and the "embedder" row below both match the
    // identity rows are stored under (`model_identity`, not the wire
    // model name). With no embedder the historical "no vector at all"
    // count is kept. The count takes a clone: the row keeps the original.
    let configured_embedding = state.recall.embedder.as_deref().map(|embedder| {
        (
            embedder.provider().to_owned(),
            embedder.model_identity(),
            embedder.dim(),
        )
    });
    let derived = match match configured_embedding.clone() {
        Some(identity) => {
            state
                .reader
                .derived_index_status_for_embedding(identity)
                .await
        }
        None => state.reader.derived_index_status().await,
    } {
        Ok(derived) => derived,
        Err(error) => return internal(error),
    };
    // Pragma reads; a failure must not take the whole page down.
    let storage = state.reader.storage_status().await.unwrap_or_default();
    let consolidation = match state.reader.session_consolidation_queue().await {
        Ok(queue) => queue,
        Err(error) => return internal(error),
    };
    let (ingest, providers, process) = match &state.health {
        Some(sources) => {
            // The spool and journal reads touch the filesystem; keep them off
            // the async workers.
            let probe = Arc::clone(&sources.probe);
            let process = tokio::task::spawn_blocking(move || probe.process_state())
                .await
                .unwrap_or_default();
            (
                sources.ingest.snapshot(),
                sources.providers.snapshot(),
                process,
            )
        }
        None => (
            IngestMetrics::default().snapshot(),
            ProviderHealthSnapshot::default(),
            ProcessState::default(),
        ),
    };
    let snapshot = HealthSnapshot {
        ingest,
        providers,
        process,
        derived,
        storage,
        consolidation,
        embedder: configured_embedding.map(|(provider, model, dim)| EmbedderIdentity {
            provider,
            model,
            dim,
        }),
    };
    let view = HealthView {
        i18n: i18n.clone(),
        wired: state.health.is_some(),
        checks: localized_checks(&i18n, &snapshot)
            .into_iter()
            .map(|check| HealthCheckRow {
                id: check.id,
                status: check.status.label(),
                detail: check.detail,
                fix: check.fix,
            })
            .collect(),
        sections: sections(&i18n, &snapshot, state.recall.rerank.is_some()),
    };
    match view.render() {
        Ok(body) => {
            let mut response = Html(body).into_response();
            // Server-wide operator state: never cache it in a shared proxy.
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, no-store"),
            );
            response
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn row(
    i18n: &ai_memory_core::i18n::Localizer,
    label: &'static str,
    value: impl Into<String>,
) -> HealthRow {
    HealthRow {
        label: i18n.text(label),
        value: value.into(),
    }
}

fn micros(i18n: &ai_memory_core::i18n::Localizer, us: Option<i64>) -> String {
    us.and_then(|us| jiff::Timestamp::from_microsecond(us).ok())
        .map_or_else(|| i18n.text("web-health-detail-never"), |ts| ts.to_string())
}

fn age(i18n: &ai_memory_core::i18n::Localizer, ms: Option<u64>) -> String {
    let Some(ms) = ms else {
        return i18n.text("web-not-available");
    };
    let secs = ms / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// `status provider/model[ dim N] · last ok … · last error … (HTTP n)`.
/// The provider's error message is left out: it can quote a response body.
fn provider_line(
    i18n: &ai_memory_core::i18n::Localizer,
    role: &ProviderRoleHealthSnapshot,
) -> String {
    let mut out = i18n.status(role.status.as_str());
    if let Some(provider) = &role.provider {
        out.push_str(&format!(
            " · {provider}/{}",
            role.model.as_deref().unwrap_or("?")
        ));
    }
    if let Some(dim) = role.dim {
        out.push_str(&i18n.format(
            "web-health-detail-dim-dim",
            &[("value0", format!("{}", dim))],
        ));
    }
    if let Some(at) = role.last_success_at {
        out.push_str(&i18n.format(
            "web-health-detail-last-ok-at",
            &[("value0", format!("{}", at))],
        ));
    }
    if let Some(at) = role.last_error_at {
        out.push_str(&i18n.format(
            "web-health-detail-last-error-at",
            &[("value0", format!("{}", at))],
        ));
        if let Some(status) = role.last_error_status {
            out.push_str(&i18n.format(
                "web-health-detail-http-status",
                &[("value0", format!("{}", status))],
            ));
        }
    }
    out
}

fn sections(
    i18n: &ai_memory_core::i18n::Localizer,
    s: &HealthSnapshot,
    rerank: bool,
) -> Vec<HealthSection> {
    let ingest = &s.ingest;
    let hooks = HealthSection {
        title: i18n.text("web-health-label-hooks-since-the-server-started"),
        rows: vec![
            row(
                i18n,
                "web-health-label-accepted",
                ingest.accepted.to_string(),
            ),
            row(
                i18n,
                "web-health-label-shed-429",
                i18n.format(
                    "web-health-detail-saturated-rate-limited",
                    &[
                        (
                            "value0",
                            format!("{}", ingest.shed_saturated + ingest.shed_rate_limited),
                        ),
                        ("value1", format!("{}", ingest.shed_saturated)),
                        ("value2", format!("{}", ingest.shed_rate_limited)),
                    ],
                ),
            ),
            row(
                i18n,
                "web-health-label-outcomes",
                i18n.format(
                    "web-health-detail-stored-replayed-resumed-ignored-end-failed",
                    &[
                        ("value0", format!("{}", ingest.stored)),
                        ("value1", format!("{}", ingest.replayed)),
                        ("value2", format!("{}", ingest.resumed)),
                        ("value3", format!("{}", ingest.ignored_end)),
                        ("value4", format!("{}", ingest.failed)),
                    ],
                ),
            ),
            row(
                i18n,
                "web-health-label-dropped",
                i18n.format(
                    "web-health-detail-capture-policy-unauthorized-collision-invalid",
                    &[
                        ("value0", format!("{}", ingest.dropped_by_policy)),
                        ("value1", format!("{}", ingest.dropped_unauthorized)),
                        ("value2", format!("{}", ingest.dropped_collision)),
                        ("value3", format!("{}", ingest.dropped_invalid)),
                    ],
                ),
            ),
            row(
                i18n,
                "web-health-label-last-write",
                micros(
                    i18n,
                    ingest
                        .last_persisted_ms
                        .and_then(|ms| i64::try_from(ms).ok())
                        .map(|ms| ms.saturating_mul(1000)),
                ),
            ),
        ],
    };
    let process = &s.process;
    let local = HealthSection {
        title: i18n.text("web-health-label-writer-spool-and-recovery"),
        rows: vec![
            row(
                i18n,
                "web-health-label-write-queue",
                process.write_queue.map_or_else(
                    || i18n.text("web-health-detail-unknown"),
                    |(queued, capacity)| format!("{queued}/{capacity}"),
                ),
            ),
            row(
                i18n,
                "web-health-label-hook-spool",
                process.spool.map_or_else(
                    || i18n.text("web-health-detail-not-visible-to-this-server"),
                    |spool| {
                        i18n.format(
                            "web-health-detail-pending-of-oldest-retries",
                            &[
                                ("value0", format!("{}", spool.pending)),
                                ("value1", format!("{}", spool.capacity)),
                                ("value2", age(i18n, spool.oldest_age_ms).to_string()),
                                ("value3", format!("{}", spool.retries_total)),
                            ],
                        )
                    },
                ),
            ),
            row(
                i18n,
                "web-health-label-recovery-journal",
                process.journal.map_or_else(
                    || i18n.text("web-health-detail-not-visible-to-this-server"),
                    |journal| {
                        if journal.unreadable {
                            i18n.text("web-health-detail-unreadable")
                        } else {
                            i18n.format(
                                "web-health-detail-pending-degraded-finish-failed",
                                &[
                                    ("value0", format!("{}", journal.pending)),
                                    ("value1", format!("{}", journal.degraded)),
                                    ("value2", format!("{}", journal.finish_failed)),
                                ],
                            )
                        }
                    },
                ),
            ),
        ],
    };
    let queue = &s.consolidation;
    let consolidation = HealthSection {
        title: i18n.text("web-health-label-consolidation"),
        rows: vec![
            row(
                i18n,
                "web-health-label-last-completed",
                micros(i18n, queue.last_completed_at_us),
            ),
            row(
                i18n,
                "web-health-label-queue",
                i18n.format(
                    "web-health-detail-pending-running-completed-failed",
                    &[
                        ("value0", format!("{}", queue.pending)),
                        ("value1", format!("{}", queue.running)),
                        ("value2", format!("{}", queue.completed)),
                        ("value3", format!("{}", queue.failed)),
                    ],
                ),
            ),
            row(
                i18n,
                "web-health-label-oldest-pending-since",
                micros(i18n, queue.oldest_pending_requested_at_us),
            ),
        ],
    };
    let mut models = vec![
        row(
            i18n,
            "web-health-label-embedder",
            s.embedder.as_ref().map_or_else(
                || i18n.text("web-health-detail-none-fts5-entity-graph-only"),
                |e| {
                    i18n.format(
                        "web-health-detail-dim",
                        &[
                            ("value0", e.provider.to_string()),
                            ("value1", e.model.to_string()),
                            ("value2", format!("{}", e.dim)),
                        ],
                    )
                },
            ),
        ),
        row(
            i18n,
            "web-health-label-embedding-provider",
            provider_line(i18n, &s.providers.embedding),
        ),
        row(
            i18n,
            "web-health-label-llm-provider",
            provider_line(i18n, &s.providers.llm),
        ),
        row(
            i18n,
            "web-health-label-reranker",
            if rerank { "on" } else { "off" },
        ),
        row(
            i18n,
            "web-health-label-vectors",
            i18n.format(
                "web-health-detail-rows-latest-pages-missing-unembeddable",
                &[
                    ("value0", format!("{}", s.derived.embedding_rows)),
                    (
                        "value1",
                        format!("{}", s.derived.latest_pages_missing_embeddings),
                    ),
                    ("value2", format!("{}", s.derived.latest_pages_unembeddable)),
                ],
            ),
        ),
    ];
    for triple in &s.derived.embedding_triples {
        models.push(row(
            i18n,
            "web-health-label-stored-triple",
            i18n.format(
                "web-health-detail-dim-rows",
                &[
                    ("value0", triple.provider.to_string()),
                    ("value1", triple.model.to_string()),
                    ("value2", format!("{}", triple.dim)),
                    ("value3", format!("{}", triple.count)),
                ],
            ),
        ));
    }
    let models = HealthSection {
        title: i18n.text("web-health-label-embedder-and-models"),
        rows: models,
    };
    let index = HealthSection {
        title: i18n.text("web-health-label-index-and-storage"),
        rows: vec![
            row(
                i18n,
                "web-health-label-fts",
                i18n.format(
                    "web-health-detail-pages-observations",
                    &[
                        ("value0", format!("{}", s.derived.pages_fts_rows)),
                        ("value1", format!("{}", s.derived.pages_rows)),
                        ("value2", format!("{}", s.derived.observations_fts_rows)),
                        ("value3", format!("{}", s.derived.observations_rows)),
                    ],
                ),
            ),
            row(
                i18n,
                "web-health-label-database",
                i18n.format(
                    "web-health-detail-bytes-reclaimable",
                    &[
                        ("value0", format!("{}", s.storage.database_bytes)),
                        ("value1", format!("{}", s.storage.reclaimable_bytes)),
                    ],
                ),
            ),
        ],
    };
    vec![hooks, local, consolidation, models, index]
}
