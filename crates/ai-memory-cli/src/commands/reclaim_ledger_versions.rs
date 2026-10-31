//! `ai-memory reclaim-ledger-versions` — thin HTTP client for dropping the
//! superseded versions of the raw hook event ledger.

use anyhow::Result;
use serde::Serialize;

use crate::cli::ReclaimLedgerVersionsArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

use super::compact::human_bytes;

/// Request sent to `POST /admin/reclaim-ledger-versions`.
#[derive(Serialize)]
struct ReclaimLedgerVersionsRequest {
    confirm: bool,
    dry_run: bool,
    drop_latest: bool,
    compact: bool,
}

/// Run the `reclaim-ledger-versions` subcommand.
///
/// A dry run by default: the report says what would go, and `--confirm` is
/// what makes it go. `--confirm` is required for the deleting run because,
/// unlike `compact`, this one removes rows and nothing here can put them back.
///
/// # Errors
/// Returns an error when the server is unreachable or returns a non-2xx
/// response.
pub async fn run(config: &Config, args: ReclaimLedgerVersionsArgs) -> Result<()> {
    let i18n = &config.i18n;
    let dry_run = !args.confirm;

    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let report: serde_json::Value = post_json(
        &endpoint,
        "/admin/reclaim-ledger-versions",
        &ReclaimLedgerVersionsRequest {
            confirm: args.confirm,
            dry_run,
            drop_latest: args.drop_latest,
            compact: args.compact,
        },
    )
    .await?;

    let n = |key: &str| report[key].as_u64().unwrap_or(0);
    let paths = n("ledger_paths");
    let rows = n("pages_deleted");
    let body_bytes = n("bytes_deleted");
    let dropped_latest = report["dropped_latest"].as_bool().unwrap_or(false);
    let compacted = report["compacted"].as_bool().unwrap_or(false);

    if paths == 0 || rows == 0 {
        println!(
            "{}", i18n.text("cli-reclaim-ledger-versions-no-superseded-ledger-versions-to-reclaim-nothing-written-by-the-pre-2-1-1-in"));
        return Ok(());
    }

    if dry_run {
        println!(
            "{}", i18n.format("cli-reclaim-ledger-versions-would-delete-rows-superseded-version-s-of-the-hook-event-ledger-across-paths", &[("value0", format!("{}", rows)), ("value1", format!("{}", paths)), ("value2", human_bytes(body_bytes).to_string())]));
        if dropped_latest {
            println!(
                "{}",
                i18n.text(
                    "cli-reclaim-ledger-versions-drop-latest-each-ledger-s-live-row-would-go-too"
                )
            );
        }
        println!(
            "{}",
            i18n.text("cli-reclaim-ledger-versions-nothing-was-changed-to-apply")
        );
        let mut flags = String::from("  ai-memory reclaim-ledger-versions --confirm");
        if args.drop_latest {
            flags.push_str(" --drop-latest");
        }
        if args.compact {
            flags.push_str(" --compact");
        }
        println!("{flags}");
        return Ok(());
    }

    println!(
        "{}", i18n.format("cli-reclaim-ledger-versions-deleted-rows-superseded-ledger-version-s-across-paths-ledger-path-s-carrying", &[("value0", format!("{}", rows)), ("value1", format!("{}", paths)), ("value2", human_bytes(body_bytes).to_string())]));
    if dropped_latest {
        println!(
            "{}",
            i18n.text(
                "cli-reclaim-ledger-versions-each-ledger-s-live-row-was-dropped-as-well-drop-latest"
            )
        );
    }
    if compacted {
        println!(
            "{}",
            i18n.format(
                "cli-reclaim-ledger-versions-reclaimed",
                &[
                    ("value0", human_bytes(n("bytes_reclaimed")).to_string()),
                    ("value1", human_bytes(n("bytes_before")).to_string()),
                    ("value2", human_bytes(n("bytes_after")).to_string())
                ]
            )
        );
    } else {
        println!(
            "{}", i18n.text("cli-reclaim-ledger-versions-the-bytes-are-free-pages-now-run-ai-memory-compact-confirm-to-return-them-to"));
    }
    Ok(())
}
