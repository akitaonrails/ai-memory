//! `ai-memory backup --to <tarball>` — POST to the server's
//! `/admin/backup` endpoint and save the returned gzip tarball.
//!
//! The server uses SQLite's online backup API so the live DB is never
//! raced. This client streams the response to the caller-supplied path.

use anyhow::{Context, Result};
use tracing::info;

use crate::cli::BackupArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_to_file};

/// Run the `backup` subcommand.
///
/// # Errors
/// Returns an error if the POST to `/admin/backup` fails, the server
/// returns a non-2xx status, or the output file cannot be written.
pub async fn run(config: &Config, args: BackupArgs) -> Result<()> {
    let i18n = &config.i18n;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let dest = &args.to;
    if let Some(parent) = dest.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir for {}", dest.display()))?;
    }
    let size = post_to_file(&endpoint, "/admin/backup", dest)
        .await
        .context(i18n.text("cli-backup-requesting-backup-from-server"))?;

    info!(path = %dest.display(), bytes = size, "backup written");
    println!(
        "{}",
        i18n.format(
            "cli-backup-wrote-backup-to",
            &[
                ("value0", format!("{}", dest.display())),
                ("value1", human_bytes(size).to_string())
            ]
        )
    );
    Ok(())
}

/// Render a backup size with binary units and two decimal places.
pub(crate) fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    // Two decimal places: 1023.995 KiB renders as 1024.00. Carry while the
    // formatted magnitude would still be 1024.00, the same rule `compact`
    // uses at one decimal. Stopping at `value < 1024.0` leaves that sliver
    // printing a size the unit cannot hold (1 MiB − 5 bytes came out as
    // `1024.00 KiB`).
    while (value * 100.0).round() >= 102_400.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::human_bytes;

    /// A size just under a unit boundary rounds up at two decimals, so it must
    /// carry to the next unit rather than print a magnitude that unit cannot
    /// hold. `backup` reported `1024.00 KiB` for 1 MiB − 5 bytes.
    #[test]
    fn human_bytes_carries_a_value_that_would_round_up_to_the_next_unit() {
        for n in 1_048_571..=1_048_575 {
            assert_eq!(human_bytes(n), "1.00 MiB", "{n}");
        }
        assert_eq!(human_bytes(1024 * 1024 * 1024 - 6), "1.00 GiB");
        // Controls: a value that does not reach the boundary stays put, and
        // exact boundaries keep the scale they already had.
        assert_eq!(human_bytes(1_048_570), "1023.99 KiB");
        assert_eq!(human_bytes(1_047_976), "1023.41 KiB");
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.00 KiB");
        assert_eq!(human_bytes(1536), "1.50 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.00 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.00 GiB");
    }
}
