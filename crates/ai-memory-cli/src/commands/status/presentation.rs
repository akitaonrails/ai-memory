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
    terminal.header("STATUS / memory & capture");
    terminal.line(format_args!("ai-memory {} (server)", report.version));
    for line in client_version_lines(&report.version, CLIENT_VERSION) {
        terminal.line(format_args!("{line}"));
    }
    terminal.section("Connection");
    terminal.line(format_args!("  server:       {}", ep.url));
    terminal.line(format_args!("  data-dir:     {}", report.data_dir));
    terminal.line(format_args!("  db:           {}", report.db_path));
    terminal.line(format_args!("  bind:         {}", report.bind));
    terminal.line(format_args!("  auth:         {}", auth_posture.line()));
    if let Some(problem) = &windows_logon {
        terminal.text(&problem.render_human());
    }
    terminal.section("Memory");
    terminal.line(format_args!(
        "  pages:        {} (all versions: {})",
        report.counts.pages_latest, report.counts.pages_all
    ));
    terminal.line(format_args!("  sessions:     {}", report.counts.sessions));
    terminal.line(format_args!(
        "  observations: {}",
        report.counts.observations
    ));
    terminal.section("Search index");
    terminal.line(format_args!(
        "  fts:          pages {}/{}; observations {}/{}",
        report.derived.pages_fts_rows,
        report.derived.pages_rows,
        report.derived.observations_fts_rows,
        report.derived.observations_rows
    ));
    terminal.line(format_args!(
        "  embeddings:   {} rows; {} latest pages missing",
        report.derived.embedding_rows, report.derived.latest_pages_missing_embeddings
    ));
    if report.derived.latest_pages_unembeddable > 0 {
        terminal.line(format_args!(
            "    unembeddable: {} (empty body; no embedder can cover these)",
            report.derived.latest_pages_unembeddable
        ));
    }
    for triple in &report.derived.embedding_triples {
        terminal.line(format_args!(
            "    {}/{} dim {}: {} rows",
            triple.provider, triple.model, triple.dim, triple.count
        ));
    }
    // Only shown when there is something to act on. A recovered count with
    // no outstanding failures is history, not a problem.
    if report.derived.embed_failures_unresolved > 0 {
        terminal.line(format_args!(
            "    embed failures: {} unresolved ({} recovered since)",
            report.derived.embed_failures_unresolved, report.derived.embed_failures_recovered
        ));
    }
    terminal.section("Storage & links");
    // The figure that makes a `compact` decision possible. Reported
    // always, so "should I VACUUM?" has an answer other than a guess; the
    // advisory line only appears once it is worth the exclusive lock.
    if report.storage.database_bytes > 0 {
        let pct = (report.storage.reclaimable_bytes as f64 / report.storage.database_bytes as f64)
            * 100.0;
        terminal.line(format_args!(
            "  storage:      {} on disk, {} reclaimable ({pct:.1}%)",
            super::super::compact::human_bytes(report.storage.database_bytes),
            super::super::compact::human_bytes(report.storage.reclaimable_bytes),
        ));
        if let Some(free) = report.storage.data_dir_free_bytes {
            terminal.line(format_args!(
                "    filesystem free: {}",
                super::super::compact::human_bytes(free)
            ));
        }
        if fix_lines::reclaim_advised(
            report.storage.reclaimable_bytes,
            report.storage.database_bytes,
        ) {
            terminal.line(format_args!(
                "    `{}` would return it (blocks writes while it runs)",
                fix_lines::COMPACT
            ));
        }
    }
    terminal.line(format_args!(
        "  links:        {} latest-page links (unresolved: {}, stale: {})",
        report.derived.links_from_latest_pages,
        report.derived.unresolved_links_from_latest_pages,
        report.derived.stale_links_from_latest_pages
    ));
    if !report.derived.typed_links_from_latest_pages.is_empty() {
        let typed: Vec<String> = report
            .derived
            .typed_links_from_latest_pages
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect();
        terminal.line(format_args!("    typed edges: {}", typed.join(", ")));
    }
    if let Some(wiki) = &report.wiki_format {
        let migrated = if wiki.okf_migrated {
            "OKF v0.2 (migrated)"
        } else {
            "OKF v0.2 (native, no migration needed)"
        };
        match (&wiki.backup_archive, wiki.backup_archive_bytes) {
            (Some(path), Some(bytes)) => terminal.line(format_args!(
                "  wiki format:  {migrated}; pre-migration backup still on disk: \
                     {path} ({})",
                super::super::compact::human_bytes(bytes)
            )),
            _ => terminal.line(format_args!("  wiki format:  {migrated}")),
        }
    }
    terminal.section("Capture & delivery");
    if let Some((queued, capacity)) = report.write_queue {
        // A queue pinned near capacity is the wedged-writer signal.
        if queued > 0 {
            terminal.line(format_args!("  write queue:  {queued}/{capacity}"));
        }
    }
    terminal.line(format_args!("  spool:"));
    terminal.line(format_args!("    pending:    {}", spool.pending));
    terminal.line(format_args!(
        "    oldest:     {}",
        spool_age_line(spool.oldest_age_ms)
    ));
    terminal.line(format_args!("    retries:    {}", spool.retries_total));
    if journal_pending > 0 || journal_error.is_some() {
        terminal.line(format_args!("  recovery:"));
        terminal.line(format_args!(
                "    journal:    {journal_pending} pending ({journal_degraded} degraded, {journal_finish_failed} finish-failed)"
            ));
        if let Some(error) = &journal_error {
            terminal.line(format_args!("    error:      {error}"));
        }
        terminal.line(format_args!("    action:     run `{}`", fix_lines::RECOVER));
    }
    if let Some(ingest) = &report.ingest {
        terminal.line(format_args!("  ingest (server, this process):"));
        terminal.line(format_args!("    accepted:   {}", ingest.accepted));
        terminal.line(format_args!(
            "    dropped:    {} (capture policy)",
            ingest.dropped_by_policy
        ));
        terminal.line(format_args!(
            "    outcomes:   {} stored, {} replayed, {} resumed, {} ignored end, {} failed",
            ingest.stored, ingest.replayed, ingest.resumed, ingest.ignored_end, ingest.failed
        ));
        terminal.line(format_args!(
            "    drops:      {} unauthorized, {} collision, {} invalid",
            ingest.dropped_unauthorized, ingest.dropped_collision, ingest.dropped_invalid
        ));
        terminal.line(format_args!(
            "    shed:       {} saturated, {} rate-limited",
            ingest.shed_saturated, ingest.shed_rate_limited
        ));
        terminal.line(format_args!(
            "    last write: {}",
            last_write_line(ingest.last_persisted_ms)
        ));
    }
    // #446 + #428 interact here: under allowlist mode an unmarked
    // repository never sends, so zero counters read exactly like a broken
    // install. Deliberately outside the ingest block above: the mode is a
    // client-side fact, and an older server returns no ingest section at
    // all — which is precisely a case where the operator needs telling.
    if capture_mode == "allowlist" {
        terminal.line(format_args!(
            "  capture mode: allowlist — repositories without a \
                 .ai-memory.toml marker send nothing"
        ));
    }
    terminal.section("Optional models");
    terminal.line(format_args!("  providers:"));
    terminal.line(format_args!(
        "    llm:       {}",
        provider_health_line(&report.providers.llm)
    ));
    terminal.line(format_args!(
        "    embedding: {}",
        provider_health_line(&report.providers.embedding)
    ));
    if report.providers.llm.status == ProviderHealthStatus::Error
        && let Some(hint) = &report.providers.llm.retry_hint
    {
        terminal.line(format_args!("    retry:     {hint}"));
    }
    // Only present when `llm_fallbacks` is configured — an older server
    // or a plain single-provider setup reports an empty list.
    if !report.providers.llm_candidates.is_empty() {
        terminal.line(format_args!("    fallback chain:"));
        for candidate in &report.providers.llm_candidates {
            terminal.line(format_args!("      {}", fallback_candidate_line(candidate)));
        }
    }
}
