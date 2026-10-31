//! `ai-memory commit` — thin HTTP client for the manual wiki git commit.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::CommitArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Request sent to `POST /admin/commit`.
#[derive(Serialize)]
struct CommitRequest {
    message: String,
}

/// Response from `POST /admin/commit`.
#[derive(Deserialize)]
struct CommitResponse {
    committed: bool,
    #[serde(default)]
    oid: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

/// Run the `commit` subcommand.
///
/// # Errors
/// Returns an error if the server is unreachable or returns a non-2xx
/// response.
pub async fn run(config: &Config, args: CommitArgs) -> Result<()> {
    let i18n = &config.i18n;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let resp: CommitResponse = post_json(
        &endpoint,
        "/admin/commit",
        &CommitRequest {
            message: args.message,
        },
    )
    .await?;
    if resp.committed {
        println!(
            "{}",
            i18n.format(
                "cli-commit-committed",
                &[(
                    "value0",
                    resp.oid.as_deref().unwrap_or("(unknown)").to_string()
                )]
            )
        );
    } else {
        println!(
            "{}",
            i18n.format(
                "cli-commit-nothing-to-commit",
                &[(
                    "value0",
                    resp.reason
                        .as_deref()
                        .map(|r| format!(" ({r})"))
                        .unwrap_or_default()
                        .to_string()
                )]
            )
        );
    }
    Ok(())
}
