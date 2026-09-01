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
    if !args.confirm {
        bail!(
            "compact rewrites the whole database under an exclusive lock: every \
             write blocks until it finishes, and it needs free disk space of \
             roughly the database's own size.\n\n\
             Check `ai-memory status` for the reclaimable figure first, then:\n\n  \
             ai-memory compact --confirm"
        );
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
        "Compacted: {} → {} ({} reclaimed).",
        human_bytes(n("bytes_before")),
        human_bytes(n("bytes_after")),
        human_bytes(reclaimed),
    );
    if reclaimed == 0 {
        // Not a failure, and worth saying plainly: SQLite reuses free pages,
        // so a healthy store that is not carrying a large deletion has
        // genuinely nothing to give back.
        println!(
            "Nothing to reclaim — the database had no meaningful free-page \
             backlog. This is the normal result for a store that has not just \
             had a large deletion."
        );
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
    while value >= 1024.0 && unit < UNITS.len() - 1 {
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
}
