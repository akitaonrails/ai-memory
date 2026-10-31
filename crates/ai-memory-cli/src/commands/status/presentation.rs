//! Human report formatting; every diagnostic field and remedy is retained.

use super::*;

pub(super) struct View<'a> {
    pub report: &'a Report,
    pub ep: &'a ServerEndpoint,
    pub spool: &'a SpoolHealth,
    pub journal_pending: usize,
    pub journal_degraded: usize,
    pub journal_finish_failed: usize,
    pub journal_error: Option<&'a str>,
    pub capture_mode: &'a str,
    pub auth_posture: crate::local_auth::AuthPosture,
    pub windows_logon: Option<&'a super::super::user_service::MissingLogonBinary>,
}

pub(super) fn render(terminal: &crate::terminal::Terminal, view: View<'_>) {
    let i18n = &terminal.i18n;
    let View {
        report,
        ep,
        spool,
        journal_pending,
        journal_degraded,
        journal_finish_failed,
        journal_error,
        capture_mode,
        auth_posture,
        windows_logon,
    } = view;
    terminal.header(&i18n.text("cli-presentation-status-memory-capture"));
    terminal.line(format_args!("ai-memory {} (server)", report.version));
    for line in client_version_lines(i18n, &report.version, CLIENT_VERSION) {
        terminal.line(format_args!("{line}"));
    }
    terminal.section(&i18n.text("cli-presentation-connection"));
    terminal.line(format_args!(
        "{}",
        i18n.format("cli-presentation-server", &[("value0", ep.url.to_string())])
    ));
    terminal.line(format_args!(
        "{}",
        i18n.format(
            "cli-presentation-data-dir",
            &[("value0", report.data_dir.to_string())]
        )
    ));
    terminal.line(format_args!(
        "{}",
        i18n.format(
            "cli-presentation-db",
            &[("value0", report.db_path.to_string())]
        )
    ));
    terminal.line(format_args!(
        "{}",
        i18n.format(
            "cli-presentation-bind",
            &[("value0", report.bind.to_string())]
        )
    ));
    terminal.line(format_args!(
        "{}",
        i18n.format(
            "cli-presentation-auth-2",
            &[("value0", auth_posture.localized_line(i18n).to_string())]
        )
    ));
    if let Some(problem) = &windows_logon {
        terminal.text(&problem.render_human());
    }
    terminal.section(&i18n.text("cli-presentation-memory"));
    terminal.line(format_args!(
        "{}",
        i18n.format(
            "cli-presentation-pages-all-versions",
            &[
                ("value0", format!("{}", report.counts.pages_latest)),
                ("value1", format!("{}", report.counts.pages_all))
            ]
        )
    ));
    terminal.line(format_args!(
        "{}",
        i18n.format(
            "cli-presentation-sessions",
            &[("value0", format!("{}", report.counts.sessions))]
        )
    ));
    terminal.line(format_args!(
        "{}",
        i18n.format(
            "cli-presentation-observations",
            &[("value0", format!("{}", report.counts.observations))]
        )
    ));
    terminal.section(&i18n.text("cli-presentation-search-index"));
    terminal.line(format_args!(
        "{}",
        i18n.format(
            "cli-presentation-fts-pages-observations",
            &[
                ("value0", format!("{}", report.derived.pages_fts_rows)),
                ("value1", format!("{}", report.derived.pages_rows)),
                (
                    "value2",
                    format!("{}", report.derived.observations_fts_rows)
                ),
                ("value3", format!("{}", report.derived.observations_rows))
            ]
        )
    ));
    terminal.line(format_args!(
        "{}",
        i18n.format(
            "cli-presentation-embeddings-rows-latest-pages-missing",
            &[
                ("value0", format!("{}", report.derived.embedding_rows)),
                (
                    "value1",
                    format!("{}", report.derived.latest_pages_missing_embeddings)
                )
            ]
        )
    ));
    if report.derived.latest_pages_unembeddable > 0 {
        terminal.line(format_args!(
            "{}",
            i18n.format(
                "cli-presentation-unembeddable-empty-body-no-embedder-can-cover-these",
                &[(
                    "value0",
                    format!("{}", report.derived.latest_pages_unembeddable)
                )]
            )
        ));
    }
    for triple in &report.derived.embedding_triples {
        terminal.line(format_args!(
            "{}",
            i18n.format(
                "cli-presentation-dim-rows",
                &[
                    ("value0", triple.provider.to_string()),
                    ("value1", triple.model.to_string()),
                    ("value2", format!("{}", triple.dim)),
                    ("value3", format!("{}", triple.count))
                ]
            )
        ));
    }
    // Only shown when there is something to act on. A recovered count with
    // no outstanding failures is history, not a problem.
    if report.derived.embed_failures_unresolved > 0 {
        terminal.line(format_args!(
            "{}",
            i18n.format(
                "cli-presentation-embed-failures-unresolved-recovered-since",
                &[
                    (
                        "value0",
                        format!("{}", report.derived.embed_failures_unresolved)
                    ),
                    (
                        "value1",
                        format!("{}", report.derived.embed_failures_recovered)
                    )
                ]
            )
        ));
    }
    terminal.section(&i18n.text("cli-presentation-storage-links"));
    // The figure that makes a `compact` decision possible. Reported
    // always, so "should I VACUUM?" has an answer other than a guess; the
    // advisory line only appears once it is worth the exclusive lock.
    if report.storage.database_bytes > 0 {
        let pct = (report.storage.reclaimable_bytes as f64 / report.storage.database_bytes as f64)
            * 100.0;
        terminal.line(format_args!(
            "{}",
            i18n.format(
                "cli-presentation-storage-on-disk-reclaimable-pct-1",
                &[
                    (
                        "value0",
                        super::super::compact::human_bytes(report.storage.database_bytes)
                            .to_string()
                    ),
                    (
                        "value1",
                        super::super::compact::human_bytes(report.storage.reclaimable_bytes)
                            .to_string()
                    ),
                    ("value2", format!("{:.1}", pct))
                ]
            )
        ));
        if let Some(free) = report.storage.data_dir_free_bytes {
            terminal.line(format_args!(
                "{}",
                i18n.format(
                    "cli-presentation-filesystem-free",
                    &[(
                        "value0",
                        super::super::compact::human_bytes(free).to_string()
                    )]
                )
            ));
        }
        if fix_lines::reclaim_advised(
            report.storage.reclaimable_bytes,
            report.storage.database_bytes,
        ) {
            terminal.line(format_args!(
                "{}",
                i18n.format(
                    "cli-presentation-would-return-it-blocks-writes-while-it-runs",
                    &[("value0", fix_lines::COMPACT.to_string())]
                )
            ));
        }
    }
    terminal.line(format_args!(
        "{}",
        i18n.format(
            "cli-presentation-links-latest-page-links-unresolved-stale",
            &[
                (
                    "value0",
                    format!("{}", report.derived.links_from_latest_pages)
                ),
                (
                    "value1",
                    format!("{}", report.derived.unresolved_links_from_latest_pages)
                ),
                (
                    "value2",
                    format!("{}", report.derived.stale_links_from_latest_pages)
                )
            ]
        )
    ));
    if !report.derived.typed_links_from_latest_pages.is_empty() {
        let typed: Vec<String> = report
            .derived
            .typed_links_from_latest_pages
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect();
        terminal.line(format_args!(
            "{}",
            i18n.format(
                "cli-presentation-typed-edges",
                &[("value0", typed.join(", ").to_string())]
            )
        ));
    }
    if let Some(wiki) = &report.wiki_format {
        let migrated = if wiki.okf_migrated {
            "OKF v0.2 (migrated)"
        } else {
            "OKF v0.2 (native, no migration needed)"
        };
        match (&wiki.backup_archive, wiki.backup_archive_bytes) {
            (Some(path), Some(bytes)) => terminal.line(format_args!(
                "{}",
                i18n.format(
                    "cli-presentation-wiki-format-migrated-pre-migration-backup-still-on-disk-path",
                    &[
                        ("value0", migrated.to_string()),
                        ("value1", path.to_string()),
                        (
                            "value2",
                            super::super::compact::human_bytes(bytes).to_string()
                        )
                    ]
                )
            )),
            _ => terminal.line(format_args!(
                "{}",
                i18n.format(
                    "cli-presentation-wiki-format-migrated",
                    &[("value0", migrated.to_string())]
                )
            )),
        }
    }
    terminal.section(&i18n.text("cli-presentation-capture-delivery"));
    if let Some((queued, capacity)) = report.write_queue {
        // A queue pinned near capacity is the wedged-writer signal.
        if queued > 0 {
            terminal.line(format_args!(
                "{}",
                i18n.format(
                    "cli-presentation-write-queue-queued-capacity",
                    &[
                        ("value0", format!("{}", queued)),
                        ("value1", format!("{}", capacity))
                    ]
                )
            ));
        }
    }
    terminal.line(format_args!("{}", i18n.text("cli-presentation-spool")));
    terminal.line(format_args!(
        "{}",
        i18n.format(
            "cli-presentation-pending",
            &[("value0", format!("{}", spool.pending))]
        )
    ));
    terminal.line(format_args!(
        "{}",
        i18n.format(
            "cli-presentation-oldest",
            &[("value0", spool_age_line(spool.oldest_age_ms).to_string())]
        )
    ));
    terminal.line(format_args!(
        "{}",
        i18n.format(
            "cli-presentation-retries",
            &[("value0", format!("{}", spool.retries_total))]
        )
    ));
    if journal_pending > 0 || journal_error.is_some() {
        terminal.line(format_args!("{}", i18n.text("cli-presentation-recovery")));
        terminal.line(format_args!(
                "{}", i18n.format("cli-presentation-journal-journal-pending-pending-journal-degraded-degraded-journal-finish-fai", &[("value0", format!("{}", journal_pending)), ("value1", format!("{}", journal_degraded)), ("value2", format!("{}", journal_finish_failed))])));
        if let Some(error) = &journal_error {
            terminal.line(format_args!(
                "{}",
                i18n.format(
                    "cli-presentation-error-error",
                    &[("value0", error.to_string())]
                )
            ));
        }
        terminal.line(format_args!(
            "{}",
            i18n.format(
                "cli-presentation-action-run",
                &[("value0", fix_lines::RECOVER.to_string())]
            )
        ));
    }
    if let Some(ingest) = &report.ingest {
        terminal.line(format_args!(
            "{}",
            i18n.text("cli-presentation-ingest-server-this-process")
        ));
        terminal.line(format_args!(
            "{}",
            i18n.format(
                "cli-presentation-accepted",
                &[("value0", format!("{}", ingest.accepted))]
            )
        ));
        terminal.line(format_args!(
            "{}",
            i18n.format(
                "cli-presentation-dropped-capture-policy",
                &[("value0", format!("{}", ingest.dropped_by_policy))]
            )
        ));
        terminal.line(format_args!(
            "{}",
            i18n.format(
                "cli-presentation-outcomes-stored-replayed-resumed-ignored-end-failed",
                &[
                    ("value0", format!("{}", ingest.stored)),
                    ("value1", format!("{}", ingest.replayed)),
                    ("value2", format!("{}", ingest.resumed)),
                    ("value3", format!("{}", ingest.ignored_end)),
                    ("value4", format!("{}", ingest.failed))
                ]
            )
        ));
        terminal.line(format_args!(
            "{}",
            i18n.format(
                "cli-presentation-drops-unauthorized-collision-invalid",
                &[
                    ("value0", format!("{}", ingest.dropped_unauthorized)),
                    ("value1", format!("{}", ingest.dropped_collision)),
                    ("value2", format!("{}", ingest.dropped_invalid))
                ]
            )
        ));
        terminal.line(format_args!(
            "{}",
            i18n.format(
                "cli-presentation-shed-saturated-rate-limited",
                &[
                    ("value0", format!("{}", ingest.shed_saturated)),
                    ("value1", format!("{}", ingest.shed_rate_limited))
                ]
            )
        ));
        terminal.line(format_args!(
            "{}",
            i18n.format(
                "cli-presentation-last-write",
                &[(
                    "value0",
                    last_write_line(ingest.last_persisted_ms).to_string()
                )]
            )
        ));
    }
    // #446 + #428 interact here: under allowlist mode an unmarked
    // repository never sends, so zero counters read exactly like a broken
    // install. Deliberately outside the ingest block above: the mode is a
    // client-side fact, and an older server returns no ingest section at
    // all — which is precisely a case where the operator needs telling.
    if capture_mode == "allowlist" {
        terminal.line(format_args!(
            "{}", i18n.text("cli-presentation-capture-mode-allowlist-repositories-without-a-ai-memory-toml-marker-send-not")));
    }
    terminal.section(&i18n.text("cli-presentation-optional-models"));
    terminal.line(format_args!("{}", i18n.text("cli-presentation-providers")));
    terminal.line(format_args!(
        "{}",
        i18n.format(
            "cli-presentation-llm",
            &[(
                "value0",
                provider_health_line(i18n, &report.providers.llm).to_string()
            )]
        )
    ));
    terminal.line(format_args!(
        "{}",
        i18n.format(
            "cli-presentation-embedding",
            &[(
                "value0",
                provider_health_line(i18n, &report.providers.embedding).to_string()
            )]
        )
    ));
    if report.providers.llm.status == ProviderHealthStatus::Error
        && let Some(hint) = &report.providers.llm.retry_hint
    {
        terminal.line(format_args!(
            "{}",
            i18n.format(
                "cli-presentation-retry-hint",
                &[("value0", hint.to_string())]
            )
        ));
    }
    // Only present when `llm_fallbacks` is configured — an older server
    // or a plain single-provider setup reports an empty list.
    if !report.providers.llm_candidates.is_empty() {
        terminal.line(format_args!(
            "{}",
            i18n.text("cli-presentation-fallback-chain")
        ));
        for candidate in &report.providers.llm_candidates {
            terminal.line(format_args!("      {}", fallback_candidate_line(candidate)));
        }
    }
}
