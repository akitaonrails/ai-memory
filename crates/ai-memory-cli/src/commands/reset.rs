//! `ai-memory reset --confirm` — wipe wiki/, db/, raw/ contents.
//!
//! Refuses to run while another `ai-memory` process is alive (lesson from
//! basic-memory #765, where a zombie process holding the old SQLite
//! inode caused phantom search results after a reset).

use anyhow::{Result, bail};

use crate::cli::ResetArgs;
use crate::config::Config;
use crate::process_guard::{busy_message, sibling_processes};

const SUBDIRS: &[&str] = &["wiki", "db", "raw"];

/// Run the `reset` subcommand.
///
/// # Errors
/// Returns an error if another `ai-memory` process is running, if
/// `--confirm` was not provided, or if a directory cannot be removed.
pub fn run(config: &Config, args: ResetArgs) -> Result<()> {
    let siblings = sibling_processes();
    if !siblings.is_empty() {
        bail!(busy_message("reset", &siblings));
    }

    if !args.confirm {
        for sub in SUBDIRS {
            let path = config.data_dir.join(sub);
            if path.exists() {
                println!("would remove {}", path.display());
            }
        }
        println!("(dry-run; pass --confirm to wipe)");
        return Ok(());
    }

    for sub in SUBDIRS {
        let path = config.data_dir.join(sub);
        if !path.exists() {
            continue;
        }
        std::fs::remove_dir_all(&path)?;
        std::fs::create_dir_all(&path)?;
        tracing::info!(path = %path.display(), "reset");
    }
    tracing::info!("reset complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn seed(dir: &Path) {
        for sub in ["wiki", "db", "raw"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
            fs::write(dir.join(sub).join("f.txt"), b"x").unwrap();
        }
        fs::create_dir_all(dir.join("logs")).unwrap();
        fs::write(dir.join("logs").join("app.log"), b"log").unwrap();
    }

    fn config_for(dir: &Path) -> Config {
        Config {
            data_dir: dir.to_path_buf(),
            ..Config::default()
        }
    }

    #[test]
    fn reset_dry_run_leaves_files() {
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path());
        run(&config_for(tmp.path()), ResetArgs { confirm: false }).unwrap();
        assert!(tmp.path().join("wiki/f.txt").exists());
        assert!(tmp.path().join("db/f.txt").exists());
        assert!(tmp.path().join("raw/f.txt").exists());
    }

    #[test]
    fn reset_apply_wipes_data_keeps_logs() {
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path());
        run(&config_for(tmp.path()), ResetArgs { confirm: true }).unwrap();
        for sub in ["wiki", "db", "raw"] {
            assert!(tmp.path().join(sub).is_dir(), "{sub} dir should remain");
            assert!(
                !tmp.path().join(sub).join("f.txt").exists(),
                "{sub} emptied"
            );
        }
        assert!(tmp.path().join("logs/app.log").exists(), "logs preserved");
    }

    #[test]
    fn reset_apply_skips_absent_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("wiki")).unwrap();
        fs::write(tmp.path().join("wiki/f.txt"), b"x").unwrap();
        // db/ and raw/ intentionally absent.
        run(&config_for(tmp.path()), ResetArgs { confirm: true }).unwrap();
        assert!(!tmp.path().join("wiki/f.txt").exists());
        assert!(tmp.path().join("wiki").is_dir());
    }
}
