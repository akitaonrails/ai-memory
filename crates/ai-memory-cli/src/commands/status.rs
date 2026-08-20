//! `ai-memory status` — report runtime config and persisted counts.
//!
//! Thin HTTP client. Calls `GET /admin/status` on the configured
//! server; renders the response as human text or JSON. Never opens
//! the store directly — the server is the source of truth.

use std::path::Path;
use std::time::{Duration, SystemTime};

use ai_memory_llm::{ProviderHealthSnapshot, ProviderHealthStatus, ProviderRoleHealthSnapshot};
use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::hook_spool;
use crate::cli::StatusArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json};

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
    embedding_rows: u64,
    embedding_triples: Vec<EmbeddingTriple>,
    links_from_latest_pages: u64,
    unresolved_links_from_latest_pages: u64,
    stale_links_from_latest_pages: u64,
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
/// Returns an error if the server is unreachable, returns non-2xx, or
/// the response can't be parsed.
pub async fn run(config: &Config, args: StatusArgs) -> Result<()> {
    let ep = ServerEndpoint::from_config_resolving_auth(config).await;
    let report: Report = get_json(&ep, "/admin/status", &[]).await?;

    // Local hook-spool health is read from the client-side data dir (the spool
    // is always local, unlike the server-reported paths above). Metadata only.
    let spool = SpoolHealth::gather(&config.data_dir);

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
                "providers": report.providers,
                "hook_spool": {
                    "pending": spool.pending,
                    "oldest_age_secs": spool.oldest_age.map(|age| age.as_secs()),
                },
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
        println!(
            "  links:        {} latest-page links (unresolved: {}, stale: {})",
            report.derived.links_from_latest_pages,
            report.derived.unresolved_links_from_latest_pages,
            report.derived.stale_links_from_latest_pages
        );
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
        println!("  hook spool:");
        println!("    pending:        {}", spool.pending);
        println!("    oldest pending: {}", spool.oldest_display());
    }
    Ok(())
}

/// Local hook-spool backlog for status output: how many captured events are
/// still queued on this machine and how long the oldest has waited. Only the
/// count and age are ever exposed — the spool bodies (prompts, tool payloads,
/// captured observations) are never opened, per the status privacy rule.
struct SpoolHealth {
    /// Events currently queued in the local spool.
    pending: usize,
    /// Wall-clock age of the oldest queued event, or `None` when the spool is
    /// empty. Saturates to zero if the entry's timestamp is in the future
    /// (clock skew), so status never reports a negative age.
    oldest_age: Option<Duration>,
}

impl SpoolHealth {
    /// Read the local spool metadata. Degrades to an empty backlog when the
    /// spool directory is missing/unreadable — status must stay useful (and
    /// never panic) when one component can't be read.
    fn gather(data_dir: &Path) -> Self {
        let spool = hook_spool::spool_dir(data_dir);
        let pending = hook_spool::spool_len(&spool);
        let oldest_age = hook_spool::oldest_spool_entry_time(&spool).map(|created| {
            SystemTime::now()
                .duration_since(created)
                .unwrap_or_default()
        });
        Self {
            pending,
            oldest_age,
        }
    }

    /// Human-readable oldest-pending age, or `none` when the spool is empty.
    fn oldest_display(&self) -> String {
        self.oldest_age
            .map_or_else(|| "none".to_string(), format_age)
    }
}

/// Render a backlog age as a compact `3s` / `2m` / `1h` / `4d` string, matching
/// the terse style of the surrounding status lines. Coarsens to the largest
/// whole unit; sub-second ages read as `0s`.
fn format_age(age: Duration) -> String {
    let secs = age.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 60 * 60 {
        format!("{}m", secs / 60)
    } else if secs < 24 * 60 * 60 {
        format!("{}h", secs / (60 * 60))
    } else {
        format!("{}d", secs / (24 * 60 * 60))
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
    use jiff::Timestamp;

    fn enqueue_at(data_dir: &Path, created_ms: u64) {
        let spool = hook_spool::spool_dir(data_dir);
        let mut entry =
            hook_spool::entry_for("https://x/hook?event=stop".into(), "{}".into(), None, false);
        entry.created_ms = created_ms;
        hook_spool::enqueue(&spool, &entry).unwrap();
    }

    #[test]
    fn format_age_coarsens_to_largest_whole_unit() {
        assert_eq!(format_age(Duration::from_millis(400)), "0s");
        assert_eq!(format_age(Duration::from_secs(8)), "8s");
        assert_eq!(format_age(Duration::from_secs(59)), "59s");
        assert_eq!(format_age(Duration::from_secs(60)), "1m");
        assert_eq!(format_age(Duration::from_secs(3599)), "59m");
        assert_eq!(format_age(Duration::from_secs(3600)), "1h");
        assert_eq!(format_age(Duration::from_secs(24 * 3600)), "1d");
    }

    #[test]
    fn spool_health_empty_spool_reports_zero_and_none() {
        let tmp = tempfile::tempdir().unwrap();
        let health = SpoolHealth::gather(tmp.path());
        assert_eq!(health.pending, 0);
        assert!(health.oldest_age.is_none());
        assert_eq!(health.oldest_display(), "none");
    }

    #[test]
    fn spool_health_counts_pending_and_ages_oldest() {
        let tmp = tempfile::tempdir().unwrap();
        // Two entries; the older is well in the past so its age is non-trivial
        // and its display is a real duration rather than `none`.
        let now_ms = u64::try_from(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        enqueue_at(tmp.path(), now_ms - 5_000);
        enqueue_at(tmp.path(), now_ms - 90_000); // oldest → ~90s

        let health = SpoolHealth::gather(tmp.path());
        assert_eq!(health.pending, 2);
        let age = health.oldest_age.expect("oldest age present");
        assert!(age.as_secs() >= 90, "age tracks the oldest entry: {age:?}");
        assert_eq!(health.oldest_display(), format_age(age));
    }

    #[test]
    fn spool_health_future_timestamp_saturates_to_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let now_ms = u64::try_from(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        // Clock skew: a timestamp far in the future must not yield a negative
        // age or panic.
        enqueue_at(tmp.path(), now_ms + 10 * 60 * 1000);

        let health = SpoolHealth::gather(tmp.path());
        assert_eq!(health.pending, 1);
        assert_eq!(health.oldest_age, Some(Duration::ZERO));
        assert_eq!(health.oldest_display(), "0s");
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
}
