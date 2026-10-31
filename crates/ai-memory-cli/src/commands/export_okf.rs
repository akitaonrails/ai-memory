//! `ai-memory export-okf --project <name> --to <tarball>` — download one
//! project's wiki as an OKF v0.2 bundle from the server's
//! `/admin/export-okf` endpoint (docs/okf.md).
//!
//! The wiki files ARE the bundle (native conformance); the server ships
//! a validated copy with a freshly generated `index.md`. The reverse
//! direction needs no dedicated command: unpack a bundle's concept files
//! into a project's wiki directory and the watcher (or `reindex`)
//! ingests them.

use anyhow::{Context, Result};
use tracing::info;

use crate::cli::ExportOkfArgs;
use crate::commands::hook_capture::url_encode;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_to_file};

/// Run the `export-okf` subcommand.
///
/// # Errors
/// Returns an error if the POST fails, the server rejects the export
/// (e.g. a non-conformant page pre-migration), or the file cannot be
/// written.
pub async fn run(config: &Config, args: ExportOkfArgs) -> Result<()> {
    let i18n = &config.i18n;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    if let Some(parent) = args.to.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir for {}", args.to.display()))?;
    }
    let path = format!(
        "/admin/export-okf?workspace={}&project={}",
        url_encode(&args.workspace),
        url_encode(&args.project),
    );
    let size = post_to_file(&endpoint, &path, &args.to)
        .await
        .context(i18n.text("cli-export-okf-requesting-okf-bundle-from-server"))?;
    info!(
        dest = %args.to.display(),
        bytes = size,
        "OKF bundle exported"
    );
    println!(
        "{}",
        i18n.format(
            "cli-export-okf-exported-okf-bundle-to-size-bytes",
            &[
                ("value0", format!("{}", args.to.display())),
                ("value1", format!("{}", size))
            ]
        )
    );
    Ok(())
}
