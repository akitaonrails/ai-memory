//! `ai-memory compact` — thin HTTP client for on-demand space reclamation.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::cli::CompactArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Request sent to `POST /admin/compact`.
#[derive(Serialize)]
struct CompactRequest {
    confirm: bool,
}

/// Run the `compact` subcommand.
///
/// Reclaims free pages without deleting anything. Requires `--confirm`
/// because it holds an exclusive lock for the duration, not because it is
/// destructive.
///
/// # Errors
/// Returns an error when `--confirm` is absent, the server is unreachable, or
/// the server returns a non-2xx response.
pub async fn run(config: &Config, args: CompactArgs) -> Result<()> {
    let i18n = &config.i18n;
    if !args.confirm {
        bail!(
            "{}", i18n.text("cli-compact-compact-rewrites-the-whole-database-under-an-exclusive-lock-every-write-bloc"));
    }

    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let report: serde_json::Value = post_json(
        &endpoint,
        "/admin/compact",
        &CompactRequest { confirm: true },
    )
    .await?;

    let n = |key: &str| report[key].as_u64().unwrap_or(0);
    let reclaimed = n("bytes_reclaimed");
    println!(
        "{}",
        i18n.format(
            "cli-compact-compacted-reclaimed",
            &[
                ("value0", human_bytes(n("bytes_before")).to_string()),
                ("value1", human_bytes(n("bytes_after")).to_string()),
                ("value2", human_bytes(reclaimed).to_string())
            ]
        )
    );
    if reclaimed == 0 {
        // Not a failure, and worth saying plainly: SQLite reuses free pages,
        // so a healthy store that is not carrying a large deletion has
        // genuinely nothing to give back.
        println!(
            "{}", i18n.text("cli-compact-nothing-to-reclaim-the-database-had-no-meaningful-free-page-backlog-this-is"));
    }
    Ok(())
}

/// Render a byte count for humans. Binary units, one decimal above KiB.
pub(crate) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    // Carry while the value would still *render* as 1024.0 or more at one
    // decimal. Stopping at `value < 1024.0` leaves 1023.95..1024.0 to round
    // up at format time, printing a magnitude that belongs to the next unit
    // (1 MiB − 6 bytes came out as `1024.0 KiB`).
    while (value * 10.0).round() >= 10240.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_scales_and_keeps_exact_byte_counts_exact() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    /// A size just under a unit boundary rounds up at one decimal, so it must
    /// carry to the next unit rather than print a magnitude that unit cannot
    /// hold. `status` reported `1024.0 KiB on disk` for a 1 MiB − 6 database.
    #[test]
    fn human_bytes_carries_a_value_that_would_round_up_to_the_next_unit() {
        assert_eq!(human_bytes(1024 * 1024 - 6), "1.0 MiB");
        assert_eq!(human_bytes(1024 * 1024 * 1024 - 24), "1.0 GiB");
        // Controls: a value that does not reach the boundary stays put.
        assert_eq!(human_bytes(1024 * 1024 - 600), "1023.4 KiB");
        assert_eq!(human_bytes(1023), "1023 B");
    }
}
