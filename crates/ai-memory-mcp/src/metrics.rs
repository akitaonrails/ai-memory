//! Process-lifetime MCP counters and the `GET /admin/metrics` renderer.
//!
//! Everything exposed here is a count or a duration with a label drawn from a
//! closed, compile-time set. Nothing is keyed on request data: no query text,
//! page body, prompt, client name, scope or project name, and no token can
//! reach a metric name, a label or a value. Scope is never a label, so the
//! series set is fixed and a scrape cannot enumerate projects.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use ai_memory_core::{IngestMetrics, LatencyHistogram};

/// Prometheus text exposition content type served by `/admin/metrics`.
pub const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// MCP-side counters shared by the tool server (writer) and the admin router
/// (reader). One instance per process: [`crate::AiMemoryServer::mcp_metrics`]
/// hands out the server's own so the admin route reads what the tools record.
#[derive(Debug, Default)]
pub struct McpMetrics {
    /// Wall time of every `memory_query` call, errors included.
    query_latency: LatencyHistogram,
    /// Read-shaped tool calls (the same split as `client_activity`).
    tool_reads: AtomicU64,
    /// Write-shaped tool calls.
    tool_writes: AtomicU64,
}

impl McpMetrics {
    /// The `memory_query` latency histogram.
    #[must_use]
    pub fn query_latency(&self) -> &LatencyHistogram {
        &self.query_latency
    }

    /// Count one tool call. The client it came from is deliberately not
    /// recorded: a client name is caller-supplied text.
    pub fn record_tool_call(&self, is_write: bool) {
        let counter = if is_write {
            &self.tool_writes
        } else {
            &self.tool_reads
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// `(reads, writes)` tool calls since this process started.
    #[must_use]
    pub fn tool_calls(&self) -> (u64, u64) {
        (
            self.tool_reads.load(Ordering::Relaxed),
            self.tool_writes.load(Ordering::Relaxed),
        )
    }
}

/// Render every metric in the Prometheus text format.
#[must_use]
pub fn render_prometheus(ingest: &IngestMetrics, mcp: &McpMetrics) -> String {
    let mut out = String::with_capacity(4096);
    let snap = ingest.snapshot();

    counter_family(
        &mut out,
        "ai_memory_hook_admissions_total",
        "Hook events by admission result since the server started.",
        "result",
        &[
            ("accepted", snap.accepted),
            ("dropped_by_policy", snap.dropped_by_policy),
            ("dropped_unauthorized", snap.dropped_unauthorized),
            ("dropped_invalid", snap.dropped_invalid),
            ("shed_saturated", snap.shed_saturated),
            ("shed_rate_limited", snap.shed_rate_limited),
        ],
    );
    counter_family(
        &mut out,
        "ai_memory_hook_deliveries_total",
        "Completed hook deliveries by outcome since the server started.",
        "outcome",
        &[
            ("stored", snap.stored),
            ("replayed", snap.replayed),
            ("resumed", snap.resumed),
            ("ignored_end", snap.ignored_end),
            ("dropped_collision", snap.dropped_collision),
            ("failed", snap.failed),
        ],
    );
    counter_family(
        &mut out,
        "ai_memory_hook_file_paths_total",
        "Stored file-tool deliveries by file path outcome since the server started.",
        "outcome",
        &snap.file_paths(),
    );
    if let Some(ms) = snap.last_persisted_ms {
        let _ = writeln!(
            out,
            "# HELP ai_memory_hook_last_persisted_timestamp_seconds Unix time of the last newly persisted observation or effect."
        );
        let _ = writeln!(
            out,
            "# TYPE ai_memory_hook_last_persisted_timestamp_seconds gauge"
        );
        let _ = writeln!(
            out,
            "ai_memory_hook_last_persisted_timestamp_seconds {}",
            ms / 1000
        );
    }
    let (reads, writes) = mcp.tool_calls();
    counter_family(
        &mut out,
        "ai_memory_mcp_tool_calls_total",
        "MCP tool calls by kind since the server started.",
        "kind",
        &[("read", reads), ("write", writes)],
    );
    ingest.hook_latency().snapshot().write_prometheus(
        &mut out,
        "ai_memory_hook_request_duration_seconds",
        "Time POST /hook took to answer.",
    );
    mcp.query_latency().snapshot().write_prometheus(
        &mut out,
        "ai_memory_query_duration_seconds",
        "Wall time of memory_query calls.",
    );
    out
}

fn counter_family(
    out: &mut String,
    name: &str,
    help: &str,
    label: &str,
    values: &[(&'static str, u64)],
) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} counter");
    for (value, count) in values {
        let _ = writeln!(out, "{name}{{{label}=\"{value}\"}} {count}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn tool_calls_split_reads_from_writes() {
        let m = McpMetrics::default();
        m.record_tool_call(false);
        m.record_tool_call(false);
        m.record_tool_call(true);
        assert_eq!(m.tool_calls(), (2, 1));
    }

    #[test]
    fn rendering_is_well_formed_and_covers_every_family() {
        let ingest = IngestMetrics::default();
        ingest.record_accepted();
        ingest.record_stored();
        ingest.record_persisted(1_700_000_000_123);
        ingest.hook_latency().observe(Duration::from_millis(3));
        let mcp = McpMetrics::default();
        mcp.record_tool_call(false);
        mcp.query_latency().observe(Duration::from_millis(40));

        let text = render_prometheus(&ingest, &mcp);
        for line in [
            "ai_memory_hook_admissions_total{result=\"accepted\"} 1",
            "ai_memory_hook_admissions_total{result=\"shed_saturated\"} 0",
            "ai_memory_hook_deliveries_total{outcome=\"stored\"} 1",
            "ai_memory_hook_file_paths_total{outcome=\"indexed\"} 0",
            "ai_memory_hook_last_persisted_timestamp_seconds 1700000000",
            "ai_memory_mcp_tool_calls_total{kind=\"read\"} 1",
            "ai_memory_mcp_tool_calls_total{kind=\"write\"} 0",
            "ai_memory_hook_request_duration_seconds_bucket{le=\"0.005\"} 1",
            "ai_memory_hook_request_duration_seconds_count 1",
            "ai_memory_query_duration_seconds_bucket{le=\"0.025\"} 0",
            "ai_memory_query_duration_seconds_bucket{le=\"0.05\"} 1",
            "ai_memory_query_duration_seconds_count 1",
        ] {
            assert!(text.lines().any(|l| l == line), "missing {line}:\n{text}");
        }
        // Every sample line is `name{labels} value` or `name value` with a
        // numeric value; every family is announced by HELP and TYPE first.
        let mut announced = std::collections::HashSet::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                announced.insert(rest.split(' ').next().unwrap().to_string());
                continue;
            }
            if line.starts_with("# HELP ") {
                continue;
            }
            let (series, value) = line.rsplit_once(' ').expect("sample line");
            assert!(value.parse::<f64>().is_ok(), "{line}");
            let name = series.split('{').next().unwrap();
            let family = ["_bucket", "_sum", "_count"]
                .iter()
                .find_map(|s| name.strip_suffix(s))
                .filter(|base| announced.contains(*base))
                .unwrap_or(name);
            assert!(announced.contains(family), "unannounced series {line}");
        }
    }

    #[test]
    fn no_write_yet_means_no_last_persisted_gauge() {
        let text = render_prometheus(&IngestMetrics::default(), &McpMetrics::default());
        assert!(!text.contains("last_persisted"), "{text}");
    }
}
