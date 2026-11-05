//! The jail checklist answers remembered per checkout, so the next
//! `ai-memory run` offers them again and Enter keeps them.
//!
//! They live in ai-memory's own data directory, never in the repository: a
//! project `.ai-jail` may only close what ai-jail opens, and a repository must
//! never be able to grant itself a credential or a mount. Keys are the
//! worktree fingerprint, so each checkout keeps its own answers.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ai_memory_workstream::JailChecklistItem;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use super::jail_folders::ExtraFolder;

const FILE: &str = "jail-choices.json";
/// Checkouts kept; the oldest answers go first.
const MAX_CHECKOUTS: usize = 200;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    checkouts: BTreeMap<String, Remembered>,
}

/// One checkout's last accepted checklist.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Remembered {
    /// Toggle stems the user left on.
    #[serde(default)]
    pub(crate) enabled: Vec<String>,
    /// Toggle stems the user left off.
    #[serde(default)]
    pub(crate) disabled: Vec<String>,
    /// Extra folders mounted into the jail.
    #[serde(default)]
    pub(crate) folders: Vec<ExtraFolder>,
    /// When the answers were saved (RFC 3339).
    #[serde(default)]
    pub(crate) saved_at: String,
}

/// What changed between the remembered answers and this launch's rows.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Applied {
    /// Rows the answers never covered (a new ai-jail, a new credential);
    /// they keep their smart default.
    pub(crate) new: Vec<&'static str>,
    /// Toggles that were on but are no longer offered here.
    pub(crate) gone: Vec<String>,
}

fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

fn read_store(data_dir: &Path) -> Store {
    std::fs::read_to_string(path(data_dir))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// The answers last saved for this checkout. A missing or unreadable file
/// means nothing remembered: the checklist falls back to its smart defaults.
pub(crate) fn load(data_dir: &Path, checkout: &str) -> Option<Remembered> {
    read_store(data_dir).checkouts.remove(checkout)
}

/// Save this checkout's answers, keeping at most [`MAX_CHECKOUTS`].
pub(crate) fn save(data_dir: &Path, checkout: &str, remembered: Remembered) -> Result<()> {
    let mut store = read_store(data_dir);
    store.checkouts.insert(checkout.to_owned(), remembered);
    while store.checkouts.len() > MAX_CHECKOUTS {
        let Some(oldest) = store
            .checkouts
            .iter()
            .min_by(|a, b| a.1.saved_at.cmp(&b.1.saved_at))
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        store.checkouts.remove(&oldest);
    }
    let body = serde_json::to_string_pretty(&store).context("encoding the jail choices")?;
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    ai_memory_wiki::write_atomic(&path(data_dir), body.as_bytes())
        .with_context(|| format!("writing {}", path(data_dir).display()))?;
    Ok(())
}

/// Pre-check `items` from `remembered`. A row it named takes the saved state;
/// a row it never saw keeps its smart default and is reported as new.
pub(crate) fn apply(items: &mut [JailChecklistItem], remembered: &Remembered) -> Applied {
    let mut applied = Applied::default();
    for item in items.iter_mut() {
        let stem = item.toggle.stem;
        if remembered.enabled.iter().any(|saved| saved == stem) {
            item.checked = true;
        } else if remembered.disabled.iter().any(|saved| saved == stem) {
            item.checked = false;
        } else {
            applied.new.push(stem);
        }
    }
    applied.gone = remembered
        .enabled
        .iter()
        .filter(|saved| !items.iter().any(|item| item.toggle.stem == saved.as_str()))
        .cloned()
        .collect();
    applied
}

/// The answers to remember for these accepted rows and folders.
pub(crate) fn remember(
    items: &[JailChecklistItem],
    folders: Vec<ExtraFolder>,
    saved_at: String,
) -> Remembered {
    let (on, off): (Vec<_>, Vec<_>) = items.iter().partition(|item| item.checked);
    let stems = |rows: Vec<&JailChecklistItem>| {
        rows.into_iter()
            .map(|item| item.toggle.stem.to_owned())
            .collect()
    };
    Remembered {
        enabled: stems(on),
        disabled: stems(off),
        folders,
        saved_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_workstream::jail_toggle;

    fn rows(states: &[(&str, bool)]) -> Vec<JailChecklistItem> {
        states
            .iter()
            .map(|(stem, checked)| JailChecklistItem {
                toggle: jail_toggle(stem).unwrap(),
                checked: *checked,
            })
            .collect()
    }

    fn saved(enabled: &[&str], disabled: &[&str]) -> Remembered {
        Remembered {
            enabled: enabled.iter().map(|s| (*s).to_owned()).collect(),
            disabled: disabled.iter().map(|s| (*s).to_owned()).collect(),
            folders: Vec::new(),
            saved_at: "2026-11-05T10:00:00Z".into(),
        }
    }

    #[test]
    fn remembered_answers_override_the_smart_defaults() {
        let mut items = rows(&[("github", true), ("ssh", true), ("docker", false)]);
        let applied = apply(&mut items, &saved(&["docker"], &["github", "ssh"]));
        let state: Vec<_> = items.iter().map(|i| (i.toggle.stem, i.checked)).collect();
        assert_eq!(state, [("github", false), ("ssh", false), ("docker", true)]);
        assert_eq!(applied, Applied::default());
    }

    /// A row the answers never covered keeps its default and is flagged, and
    /// a remembered credential that is no longer offered is reported, not
    /// silently dropped.
    #[test]
    fn new_rows_keep_their_default_and_vanished_ones_are_reported() {
        let mut items = rows(&[("github", true), ("worktree", true)]);
        let applied = apply(&mut items, &saved(&["github", "aws"], &[]));
        assert!(items.iter().all(|i| i.checked));
        assert_eq!(applied.new, ["worktree"]);
        assert_eq!(applied.gone, ["aws"]);
    }

    #[test]
    fn answers_round_trip_per_checkout_and_stay_out_of_other_checkouts() {
        let data = tempfile::tempdir().unwrap();
        let items = rows(&[("github", true), ("docker", false)]);
        let folders = vec![ExtraFolder {
            path: "/home/u/Projects/ai-jail".into(),
            writable: false,
        }];
        let answers = remember(&items, folders, "2026-11-05T10:00:00Z".into());
        save(data.path(), "checkout-a", answers.clone()).unwrap();
        assert_eq!(load(data.path(), "checkout-a"), Some(answers));
        assert_eq!(load(data.path(), "checkout-b"), None);
        assert!(data.path().join(FILE).is_file());
    }

    #[test]
    fn a_corrupt_file_means_nothing_remembered_and_is_replaced_on_save() {
        let data = tempfile::tempdir().unwrap();
        std::fs::write(data.path().join(FILE), "not json").unwrap();
        assert_eq!(load(data.path(), "checkout-a"), None);
        save(data.path(), "checkout-a", saved(&["github"], &[])).unwrap();
        assert_eq!(
            load(data.path(), "checkout-a"),
            Some(saved(&["github"], &[]))
        );
    }

    #[test]
    fn the_store_keeps_only_the_newest_checkouts() {
        let data = tempfile::tempdir().unwrap();
        for index in 0..=MAX_CHECKOUTS {
            let mut answers = saved(&[], &[]);
            answers.saved_at = format!("2026-11-05T10:00:{:03}Z", index);
            save(data.path(), &format!("checkout-{index}"), answers).unwrap();
        }
        assert_eq!(
            load(data.path(), "checkout-0"),
            None,
            "the oldest is dropped"
        );
        assert!(load(data.path(), &format!("checkout-{MAX_CHECKOUTS}")).is_some());
    }
}
