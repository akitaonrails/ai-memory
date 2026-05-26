//! Shared data-dir wipe primitive used by `reset` and `uninstall --purge-data`.
//! Mute by design: returns the affected paths; callers own logging/printing and
//! the live-process guard (invariant #9). The remove+recreate is not atomic —
//! pre-existing, matches `reset`/`restore`.

use std::path::{Path, PathBuf};

use anyhow::Context;

/// The subdirectories wiped by `reset` / `uninstall --purge-data`.
/// `logs/` and `models/` are intentionally excluded and never wiped. This is
/// the reset/uninstall set only — `init` and `restore` declare their own
/// (different) sets by design; do not converge them here.
pub(crate) const WIPE_SUBDIRS: &[&str] = &["wiki", "db", "raw"];

/// Paths that WOULD be purged (existing wipe subdirs), for dry-run preview.
pub(crate) fn purge_preview(data_dir: &Path) -> Vec<PathBuf> {
    WIPE_SUBDIRS
        .iter()
        .map(|s| data_dir.join(s))
        .filter(|p| p.exists())
        .collect()
}

/// Wipe each existing wipe-subdir (remove + recreate empty). Returns the paths
/// actually purged (the subset that existed). Missing subdirs are skipped, not
/// errors. Carries per-path context on failure.
pub(crate) fn purge_data_dirs(data_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut purged = Vec::new();
    for sub in WIPE_SUBDIRS {
        let path = data_dir.join(sub);
        if !path.exists() {
            continue;
        }
        std::fs::remove_dir_all(&path).with_context(|| format!("removing {}", path.display()))?;
        std::fs::create_dir_all(&path).with_context(|| format!("recreating {}", path.display()))?;
        purged.push(path);
    }
    Ok(purged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn seed(dir: &Path) {
        for sub in ["wiki", "db", "raw"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
            fs::write(dir.join(sub).join("f.txt"), b"x").unwrap();
        }
        fs::create_dir_all(dir.join("logs")).unwrap();
        fs::write(dir.join("logs").join("app.log"), b"log").unwrap();
    }

    #[test]
    fn purge_data_dirs_wipes_set_keeps_logs() {
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path());
        let purged = purge_data_dirs(tmp.path()).unwrap();
        assert_eq!(purged.len(), 3);
        for sub in ["wiki", "db", "raw"] {
            assert!(tmp.path().join(sub).is_dir());
            assert!(!tmp.path().join(sub).join("f.txt").exists());
        }
        assert!(tmp.path().join("logs/app.log").exists());
    }

    #[test]
    fn purge_data_dirs_skips_absent() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("wiki")).unwrap();
        let purged = purge_data_dirs(tmp.path()).unwrap();
        assert_eq!(purged, vec![tmp.path().join("wiki")]);
    }

    #[test]
    fn purge_missing_data_dir_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope");
        assert!(purge_preview(&missing).is_empty());
        assert!(purge_data_dirs(&missing).unwrap().is_empty());
        assert!(!missing.exists());
    }

    #[test]
    fn purge_preview_lists_only_existing() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("db")).unwrap();
        assert_eq!(purge_preview(tmp.path()), vec![tmp.path().join("db")]);
    }
}
