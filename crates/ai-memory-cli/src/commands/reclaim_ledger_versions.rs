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
            "No superseded ledger versions to reclaim. Nothing written by the \
             pre-2.1.1 indexer is left in this store."
        );
        return Ok(());
    }

    if dry_run {
        println!(
            "Would delete {rows} superseded version(s) of the hook event ledger \
             across {paths} ledger path(s), carrying {} of page body.",
            human_bytes(body_bytes),
        );
        if dropped_latest {
            println!("  --drop-latest: each ledger's live row would go too.");
        }
        println!("Nothing was changed. To apply:");
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
        "Deleted {rows} superseded ledger version(s) across {paths} ledger \
         path(s), carrying {} of page body.",
        human_bytes(body_bytes),
    );
    if dropped_latest {
        println!("  Each ledger's live row was dropped as well (--drop-latest).");
    }
    if compacted {
        println!(
            "Reclaimed {} ({} → {}).",
            human_bytes(n("bytes_reclaimed")),
            human_bytes(n("bytes_before")),
            human_bytes(n("bytes_after")),
        );
    } else {
        println!(
            "The bytes are free pages now. Run `ai-memory compact --confirm` \
             to return them to the filesystem."
        );
    }
    Ok(())
}
