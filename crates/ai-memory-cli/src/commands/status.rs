//! `ai-memory status` — report runtime config and persisted counts.
//!
//! Thin HTTP client. Calls `GET /admin/status` on the configured
//! server; renders the response as human text or JSON. Never opens
//! the store directly — the server is the source of truth.

use ai_memory_llm::{ProviderHealthSnapshot, ProviderHealthStatus, ProviderRoleHealthSnapshot};
use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::hook_spool::{SpoolHealth, spool_dir, spool_health};
use crate::cli::StatusArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json};

/// Server-side hook-ingestion counters. `Option` at the call site so a
/// newer CLI pointed at an older server renders the rest of `status`
/// instead of failing to deserialise the whole report.
#[derive(Debug, Deserialize, Serialize)]
struct IngestReport {
    accepted: u64,
    dropped_by_policy: u64,
    shed_saturated: u64,
    shed_rate_limited: u64,
    last_persisted_ms: Option<u64>,
}

/// Server-shaped response. Mirrors `ai_memory_mcp::admin::StatusReport`.
#[derive(Debug, Deserialize, Serialize)]
struct Report {
    /// Server binary version.
    version: String,
    /// Server-side data directory path.
    data_dir: String,
    /// Server bind address.
    bind: String,
    /// Server-side SQLite path.
    db_path: String,
    /// Lifetime counts.
    counts: Counts,
    /// Derived-index diagnostics.
    #[serde(default)]
    derived: Derived,
    /// Physical storage figures (absent from pre-#549 servers).
    #[serde(default)]
    storage: Storage,
    /// Hook-ingestion counters from the server process.
    #[serde(default)]
    ingest: Option<IngestReport>,
    /// Passive process-scoped provider health.
    #[serde(default)]
    providers: ProviderHealthSnapshot,
}

#[derive(Debug, Deserialize, Serialize)]
struct Counts {
    pages_latest: u64,
    pages_all: u64,
    sessions: u64,
    observations: u64,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Derived {
    pages_rows: u64,
    pages_fts_rows: u64,
    observations_rows: u64,
    observations_fts_rows: u64,
    latest_pages_missing_embeddings: u64,
    #[serde(default)]
    embed_failures_unresolved: u64,
    #[serde(default)]
    embed_failures_recovered: u64,
    embedding_rows: u64,
    embedding_triples: Vec<EmbeddingTriple>,
    links_from_latest_pages: u64,
    unresolved_links_from_latest_pages: u64,
    stale_links_from_latest_pages: u64,
}

/// Suggest compaction only above this share of the file. Below it the
/// exclusive lock costs more than the space is worth, and SQLite will reuse
/// those pages on its own as the store grows.
const RECLAIM_ADVICE_PCT: f64 = 20.0;

/// …and only when the absolute figure is worth a stall. 20% of a 4 MiB
/// database is not a reason to block every write.
const RECLAIM_ADVICE_MIN_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Default, Deserialize, Serialize)]
struct Storage {
    page_size: u64,
    page_count: u64,
    freelist_count: u64,
    database_bytes: u64,
    reclaimable_bytes: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct EmbeddingTriple {
    provider: String,
    model: String,
    dim: u32,
    count: u64,
}

/// Run the `status` subcommand.
///
/// # Errors
/// Print local spool state when the server could not be reached.
///
/// Goes to stderr so it cannot corrupt `--json` consumers, which expect a
/// single object on stdout and get a non-zero exit here anyway. The
/// unreachable-server error still propagates unchanged; this only adds the
/// local half of the picture that the caller can actually act on.
/// Render the last-persisted stamp as an age, or `-` when this server
/// process has not written an event yet.
///
/// An age rather than a wall-clock time: the useful question is "how long
/// since anything landed", and the server may be in another timezone.
fn last_write_line(unix_ms: Option<u64>) -> String {
    let Some(ms) = unix_ms else {
        return "-".to_string();
    };
    let now = jiff::Timestamp::now().as_millisecond();
    let now_ms = u64::try_from(now).unwrap_or(0);
    spool_age_line(Some(now_ms.saturating_sub(ms)))
}

fn report_offline_spool(spool: &SpoolHealth, json: bool) {
    if json {
        if let Ok(rendered) = serde_json::to_string(spool) {
            eprintln!("local spool (server unreachable): {rendered}");
        }
        return;
    }
    eprintln!("local spool (server unreachable):");
    eprintln!("  pending:    {}", spool.pending);
    eprintln!("  oldest:     {}", spool_age_line(spool.oldest_age_ms));
    eprintln!("  retries:    {}", spool.retries_total);
    if spool.pending > 0 {
        eprintln!(
            "  {} event(s) are queued locally and will be delivered once the \
             server is reachable again.",
            spool.pending
        );
    }
}

/// Which capture mode the hook would enforce for this install (#446).
///
/// Reads the same file the hook reads rather than keeping a second source of
/// truth that could drift from the one actually gating events. Anything
/// unreadable or unrecognised reports the historical default, matching the
/// hook's own fallback.
fn resolve_capture_mode(data_dir: &std::path::Path) -> &'static str {
    match std::fs::read_to_string(data_dir.join(crate::commands::hook::CAPTURE_MODE_FILE)) {
        Ok(text) if text.trim().eq_ignore_ascii_case("allowlist") => "allowlist",
        _ => "denylist",
    }
}

/// Returns an error if the server is unreachable, returns non-2xx, or
/// the response can't be parsed.
pub async fn run(config: &Config, args: StatusArgs) -> Result<()> {
    let ep = ServerEndpoint::from_config_resolving_auth(config).await;
    let capture_mode = resolve_capture_mode(&config.data_dir);

    // Read the spool BEFORE contacting the server, and surface it even when
    // that call fails.
    //
    // The spool exists to buffer hook events while the server is
    // unreachable, so "how much is queued locally?" is most worth answering
    // precisely when the server is down. Computing it after `get_json` meant
    // the one question this section exists for could not be asked in the one
    // situation that prompts it.
    let spool = spool_health(&spool_dir(&config.data_dir));

    let report: Report = match get_json::<Report>(&ep, "/admin/status", &[]).await {
        Ok(report) => report,
        Err(err) => {
            report_offline_spool(&spool, args.json);
            return Err(err);
        }
    };

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "version": report.version,
                "data_dir": report.data_dir,
                "bind": report.bind,
                "db_path": report.db_path,
                "counts": {
                    "pages_latest": report.counts.pages_latest,
                    "pages_all": report.counts.pages_all,
                    "sessions": report.counts.sessions,
                    "observations": report.counts.observations,
                },
                "derived": report.derived,
                "storage": report.storage,
                "providers": report.providers,
                "spool": spool,
                "capture_mode": capture_mode,
                "ingest": report.ingest,
                "client": { "server_url": ep.url, "auth": ep.auth_token.is_some() },
            }))?
        );
    } else {
        println!("ai-memory {} (server)", report.version);
        println!("  server:       {}", ep.url);
        println!("  data-dir:     {}", report.data_dir);
        println!("  db:           {}", report.db_path);
        println!("  bind:         {}", report.bind);
        println!(
            "  pages:        {} (all versions: {})",
            report.counts.pages_latest, report.counts.pages_all
        );
        println!("  sessions:     {}", report.counts.sessions);
        println!("  observations: {}", report.counts.observations);
        println!(
            "  fts:          pages {}/{}; observations {}/{}",
            report.derived.pages_fts_rows,
            report.derived.pages_rows,
            report.derived.observations_fts_rows,
            report.derived.observations_rows
        );
        println!(
            "  embeddings:   {} rows; {} latest pages missing",
            report.derived.embedding_rows, report.derived.latest_pages_missing_embeddings
        );
        // Only shown when there is something to act on. A recovered count with
        // no outstanding failures is history, not a problem.
        if report.derived.embed_failures_unresolved > 0 {
            println!(
                "    embed failures: {} unresolved ({} recovered since)",
                report.derived.embed_failures_unresolved, report.derived.embed_failures_recovered
            );
        }
        // The figure that makes a `compact` decision possible. Reported
        // always, so "should I VACUUM?" has an answer other than a guess; the
        // advisory line only appears once it is worth the exclusive lock.
        if report.storage.database_bytes > 0 {
            let pct = (report.storage.reclaimable_bytes as f64
                / report.storage.database_bytes as f64)
                * 100.0;
            println!(
                "  storage:      {} on disk, {} reclaimable ({pct:.1}%)",
                super::compact::human_bytes(report.storage.database_bytes),
                super::compact::human_bytes(report.storage.reclaimable_bytes),
            );
            if pct >= RECLAIM_ADVICE_PCT
                && report.storage.reclaimable_bytes >= RECLAIM_ADVICE_MIN_BYTES
            {
                println!(
                    "    `ai-memory compact --confirm` would return it \
                     (blocks writes while it runs)"
                );
            }
        }
        println!(
            "  links:        {} latest-page links (unresolved: {}, stale: {})",
            report.derived.links_from_latest_pages,
            report.derived.unresolved_links_from_latest_pages,
            report.derived.stale_links_from_latest_pages
        );
        println!("  spool:");
        println!("    pending:    {}", spool.pending);
        println!("    oldest:     {}", spool_age_line(spool.oldest_age_ms));
        println!("    retries:    {}", spool.retries_total);
        if let Some(ingest) = &report.ingest {
            println!("  ingest (server, this process):");
            println!("    accepted:   {}", ingest.accepted);
            println!(
                "    dropped:    {} (capture policy)",
                ingest.dropped_by_policy
            );
            println!(
                "    shed:       {} saturated, {} rate-limited",
                ingest.shed_saturated, ingest.shed_rate_limited
            );
            println!(
                "    last write: {}",
                last_write_line(ingest.last_persisted_ms)
            );
        }
        // #446 + #428 interact here: under allowlist mode an unmarked
        // repository never sends, so zero counters read exactly like a broken
        // install. Deliberately outside the ingest block above: the mode is a
        // client-side fact, and an older server returns no ingest section at
        // all — which is precisely a case where the operator needs telling.
        if capture_mode == "allowlist" {
            println!(
                "  capture mode: allowlist — repositories without a \
                 .ai-memory.toml marker send nothing"
            );
        }
        println!("  providers:");
        println!(
            "    llm:       {}",
            provider_health_line(&report.providers.llm)
        );
        println!(
            "    embedding: {}",
            provider_health_line(&report.providers.embedding)
        );
        if report.providers.llm.status == ProviderHealthStatus::Error
            && let Some(hint) = &report.providers.llm.retry_hint
        {
            println!("    retry:     {hint}");
        }
    }
    Ok(())
}

/// Render a spool age (ms) as a compact human duration, or `-` when the spool
/// holds no events. Allowed to saturate: an operator reading a stuck-spool
/// diagnosis wants the magnitude, not sub-second precision.
fn spool_age_line(age_ms: Option<u64>) -> String {
    let Some(ms) = age_ms else {
        return "-".to_string();
    };
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d {}h", secs / 86_400, (secs % 86_400) / 3600)
    }
}

fn provider_health_line(role: &ProviderRoleHealthSnapshot) -> String {
    match role.status {
        ProviderHealthStatus::Disabled => "disabled".to_string(),
        ProviderHealthStatus::Unknown => {
            format!("{} unknown (no calls yet)", provider_label(role))
        }
        ProviderHealthStatus::Ok => {
            let when = role
                .last_call_at
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "unknown time".to_string());
            format!("{} ok (last call: {when})", provider_label(role))
        }
        ProviderHealthStatus::Error => {
            let detail = error_detail(role);
            format!("{} error ({detail})", provider_label(role))
        }
    }
}

fn provider_label(role: &ProviderRoleHealthSnapshot) -> String {
    match (&role.provider, &role.model, role.dim) {
        (Some(provider), Some(model), Some(dim)) => format!("{provider}/{model} ({dim}d)"),
        (Some(provider), Some(model), None) => format!("{provider}/{model}"),
        (Some(provider), None, _) => provider.clone(),
        _ => "provider".to_string(),
    }
}

fn error_detail(role: &ProviderRoleHealthSnapshot) -> String {
    let mut parts = Vec::new();
    if let Some(status) = role.last_error_status {
        parts.push(format!("status {status}"));
    }
    if let Some(message) = &role.last_error_message
        && !message.is_empty()
    {
        parts.push(message.clone());
    }
    if let Some(when) = &role.last_error_at {
        parts.push(format!("last error: {when}"));
    }
    if parts.is_empty() {
        "last call failed".to_string()
    } else {
        parts.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #428's counters and #446's gate compose into a trap: under allowlist
    /// mode an unmarked repository never sends, so `accepted: 0` with an empty
    /// spool looks exactly like a broken install. `status` must be able to say
    /// which of the two it is. Resolved from the same file the hook enforces
    /// from, so the two cannot drift.
    #[test]
    fn capture_mode_defaults_to_denylist_when_unset() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(resolve_capture_mode(tmp.path()), "denylist");
    }

    #[test]
    fn capture_mode_reports_allowlist_when_the_hook_would_enforce_it() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(crate::commands::hook::CAPTURE_MODE_FILE),
            "allowlist\n",
        )
        .unwrap();
        assert_eq!(resolve_capture_mode(tmp.path()), "allowlist");
        // The status reader and the hook enforcer must agree.
        assert_eq!(
            crate::commands::hook::CAPTURE_MODE_FILE,
            "capture-mode",
            "status reads the file the hook enforces from"
        );
    }
    use jiff::Timestamp;

    /// The offline path must be readable and, critically, must not write the
    /// spool object to stdout in `--json` mode: consumers there expect one
    /// object and this call exits non-zero anyway.
    #[test]
    fn offline_spool_report_is_content_free_and_serialises() {
        let spool = SpoolHealth {
            pending: 2,
            oldest_age_ms: Some(900_000),
            retries_total: 4,
        };
        let rendered = serde_json::to_string(&spool).expect("SpoolHealth serialises");
        assert_eq!(
            rendered,
            r#"{"pending":2,"oldest_age_ms":900000,"retries_total":4}"#
        );
        // The shape is the contract: counts and ages only. If a field
        // carrying captured text is ever added, this fails loudly.
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, vec!["pending", "oldest_age_ms", "retries_total"]);

        // Both render paths must tolerate an empty spool.
        report_offline_spool(&SpoolHealth::default(), false);
        report_offline_spool(&SpoolHealth::default(), true);
        report_offline_spool(&spool, false);
        report_offline_spool(&spool, true);
    }

    #[test]
    fn provider_health_line_renders_unknown_and_disabled() {
        assert_eq!(
            provider_health_line(&ProviderRoleHealthSnapshot::default()),
            "disabled"
        );

        let role = ProviderRoleHealthSnapshot {
            status: ProviderHealthStatus::Unknown,
            provider: Some("openai".to_string()),
            model: Some("gpt-5.5".to_string()),
            ..ProviderRoleHealthSnapshot::default()
        };
        assert_eq!(
            provider_health_line(&role),
            "openai/gpt-5.5 unknown (no calls yet)"
        );
    }

    #[test]
    fn provider_health_line_renders_error_details() {
        let when = "2026-05-28T12:00:00Z".parse::<Timestamp>().unwrap();
        let role = ProviderRoleHealthSnapshot {
            status: ProviderHealthStatus::Error,
            provider: Some("anthropic-oauth".to_string()),
            model: Some("claude-sonnet-4-6".to_string()),
            last_error_at: Some(when),
            last_error_status: Some(401),
            last_error_message: Some("bad token".to_string()),
            ..ProviderRoleHealthSnapshot::default()
        };

        assert!(provider_health_line(&role).contains("anthropic-oauth/claude-sonnet-4-6 error"));
        assert!(provider_health_line(&role).contains("status 401"));
        assert!(provider_health_line(&role).contains("bad token"));
    }

    #[test]
    fn spool_age_line_renders_durations_and_empty() {
        assert_eq!(spool_age_line(None), "-");
        assert_eq!(spool_age_line(Some(0)), "0s");
        assert_eq!(spool_age_line(Some(59_999)), "59s");
        assert_eq!(spool_age_line(Some(60_000)), "1m 0s");
        assert_eq!(spool_age_line(Some(3_661_000)), "1h 1m");
        assert_eq!(spool_age_line(Some(172_800_000)), "2d 0h");
    }
}
