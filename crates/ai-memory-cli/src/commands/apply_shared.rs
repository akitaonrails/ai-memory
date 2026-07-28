//! Filesystem-mutation primitives shared by the `--apply` modes on
//! `install-mcp`, `install-hooks`, `setup-agent`, and the new
//! `install-instructions`.
//!
//! Every write goes through [`apply_atomic`], which:
//!
//! 1. Resolves a symlink chain without replacing the link, then reads the
//!    existing target file (or empty string if absent).
//! 2. Runs the caller-supplied mutator to compute the new content.
//! 3. If the new content equals the old, returns `NoOp` — never
//!    touches the disk on a redundant call.
//! 4. Otherwise copies the existing file to `<path>.bak-<unix-ts>`
//!    so the user has a recovery path.
//! 5. Writes the new content via the canonical
//!    [`ai_memory_wiki::write_atomic`] (sibling tempfile + fsync +
//!    rename + parent-dir fsync).
//!
//! Every `--apply` mode (install-mcp, install-hooks, install-instructions, …)
//! routes through this function. The mutator decides the format (JSON /
//! TOML / markdown) and the idempotency rule; the I/O atomics live here.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use jiff::Timestamp;

/// What the mutation did to the target file. Surfaced to the user
/// so they can tell a meaningful change from a redundant re-run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// File didn't exist; we created it.
    Created,
    /// File existed; our mutation changed it. A backup at
    /// `<path>.bak-<ts>` records the prior content.
    Updated,
    /// File existed and our mutation produced the same content.
    /// No write happened. No backup written.
    NoOp,
}

impl ApplyOutcome {
    /// Short verb for the CLI report line.
    #[must_use]
    pub const fn verb(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::NoOp => "no-op",
        }
    }
}

/// Apply an idempotent mutation to `path`.
///
/// `mutator` receives the existing file content (`""` if absent) and
/// returns the desired new content. The atomicity, backup, and
/// no-op detection happen here.
///
/// # Errors
/// Propagates IO + mutator failures.
pub fn apply_atomic<F>(path: &Path, mutator: F) -> Result<ApplyOutcome>
where
    F: FnOnce(&str) -> Result<String>,
{
    let write_target = resolve_write_target(path)?;
    let existed = write_target.exists();
    let original = if existed {
        fs::read_to_string(&write_target).with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new()
    };

    let new_content = mutator(&original)?;

    if existed && new_content == original {
        return Ok(ApplyOutcome::NoOp);
    }

    if let Some(parent) = write_target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("ensuring parent directory {}", parent.display()))?;
    }

    if existed {
        let backup = backup_path_for(path);
        fs::copy(&write_target, &backup)
            .with_context(|| format!("backing up {} → {}", path.display(), backup.display()))?;
    }

    write_atomic(&write_target, &new_content)?;
    Ok(if existed {
        ApplyOutcome::Updated
    } else {
        ApplyOutcome::Created
    })
}

/// Follow final-component symlinks to the path that an atomic rename should
/// replace. This also handles a dangling final target, which `canonicalize`
/// cannot resolve, without replacing the user's symlink itself.
fn resolve_write_target(path: &Path) -> Result<PathBuf> {
    const MAX_SYMLINK_DEPTH: usize = 40;

    let mut target = path.to_path_buf();
    for _ in 0..MAX_SYMLINK_DEPTH {
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let destination = fs::read_link(&target)
                    .with_context(|| format!("resolving symlink {}", target.display()))?;
                target = if destination.is_absolute() {
                    destination
                } else {
                    target
                        .parent()
                        .filter(|parent| !parent.as_os_str().is_empty())
                        .unwrap_or_else(|| Path::new("."))
                        .join(destination)
                };
            }
            Ok(_) => return Ok(target),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(target),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspecting symlink {}", target.display()));
            }
        }
    }

    anyhow::bail!(
        "refusing to write through a symlink chain deeper than {MAX_SYMLINK_DEPTH}: {}",
        path.display()
    )
}

fn backup_path_for(path: &Path) -> PathBuf {
    let stamp = Timestamp::now().as_second();
    let mut bak = path.as_os_str().to_owned();
    bak.push(format!(".bak-{stamp}"));
    PathBuf::from(bak)
}

/// Atomic write via the canonical [`ai_memory_wiki::write_atomic`]
/// (tempfile + fsync + rename + parent-dir fsync). The one wrinkle kept
/// here: the tempfile MUST land in the same directory as the resolved target so
///   `rename(2)` stays intra-filesystem — otherwise we get EXDEV
///   ("Invalid cross-device link"). A bare relative path like `CLAUDE.md`
///   has an *empty* parent, so treat it as `.` (current directory); a
///   `$TMPDIR` fallback would sit on a different filesystem than the
///   project in just about every realistic setup.
fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let path = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => path.to_path_buf(),
        _ => Path::new(".").join(path),
    };
    ai_memory_wiki::write_atomic(&path, content.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

// --------------------------------------------------------------------
// JSON mutation helpers
// --------------------------------------------------------------------

/// Parse `original` as JSON (or yield an empty object if blank),
/// hand the mutable object to `mutator`, and return the
/// pretty-printed result with a trailing newline.
///
/// Errors out with a clear "this file isn't JSON" message rather
/// than silently overwriting; the user gets a chance to investigate.
///
/// # Errors
/// Returns an error if the input is non-empty and not parseable as
/// a JSON object.
pub fn mutate_json<F>(original: &str, mutator: F) -> Result<String>
where
    F: FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> Result<()>,
{
    let mut root: serde_json::Map<String, serde_json::Value> = if original.trim().is_empty() {
        serde_json::Map::new()
    } else {
        let parsed: serde_json::Value = serde_json::from_str(original).with_context(|| {
            "existing file isn't valid JSON; refusing to overwrite. Inspect by hand, \
             rename it, or delete it before re-running --apply."
        })?;
        match parsed {
            serde_json::Value::Object(m) => m,
            _ => {
                anyhow::bail!(
                    "existing file is JSON but not an object at the root \
                     (top-level array / string / number). Refusing to overwrite."
                );
            }
        }
    };
    mutator(&mut root)?;
    let mut out = serde_json::to_string_pretty(&serde_json::Value::Object(root))
        .context("serialising merged JSON")?;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Read-mutate-write for TOML files via `toml_edit` (preserves
/// comments + formatting from the original).
///
/// `mutator` receives the parsed `DocumentMut` and can use the full
/// `toml_edit` API to make changes. Returns the rendered TOML.
///
/// # Errors
/// Returns an error if the input is non-empty and not parseable.
pub fn mutate_toml<F>(original: &str, mutator: F) -> Result<String>
where
    F: FnOnce(&mut toml_edit::DocumentMut) -> Result<()>,
{
    let mut doc: toml_edit::DocumentMut = if original.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        original.parse().with_context(|| {
            "existing file isn't valid TOML; refusing to overwrite. Inspect by hand, \
             rename it, or delete it before re-running --apply."
        })?
    };
    mutator(&mut doc)?;
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn apply_to_missing_file_creates() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("nested/dir/foo.json");
        let outcome = apply_atomic(&p, |_| Ok("hello\n".into())).unwrap();
        assert_eq!(outcome, ApplyOutcome::Created);
        assert_eq!(fs::read_to_string(&p).unwrap(), "hello\n");
    }

    #[test]
    fn apply_to_unchanged_file_is_noop_and_no_backup() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("foo.json");
        fs::write(&p, "same\n").unwrap();
        let outcome = apply_atomic(&p, |_| Ok("same\n".into())).unwrap();
        assert_eq!(outcome, ApplyOutcome::NoOp);
        // No .bak-<ts> file should appear.
        let backups: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".bak-"))
            .collect();
        assert!(backups.is_empty(), "no-op must not create a backup");
    }

    #[test]
    fn apply_to_changed_file_backs_up_then_writes() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("foo.json");
        fs::write(&p, "old\n").unwrap();
        let outcome = apply_atomic(&p, |_| Ok("new\n".into())).unwrap();
        assert_eq!(outcome, ApplyOutcome::Updated);
        assert_eq!(fs::read_to_string(&p).unwrap(), "new\n");
        let backups: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".bak-"))
            .collect();
        assert_eq!(backups.len(), 1, "exactly one backup file expected");
        let bak_content = fs::read_to_string(backups[0].path()).unwrap();
        assert_eq!(bak_content, "old\n");
    }

    #[cfg(unix)]
    #[test]
    fn apply_through_symlink_updates_target_and_keeps_link() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        // A tracked "dotfiles" file, plus a symlink standing in for the config
        // path the tool is pointed at (e.g. ~/.claude/settings.json).
        let target = tmp.path().join("dotfiles/settings.json");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "old\n").unwrap();
        let link = tmp.path().join("settings.json");
        symlink(&target, &link).unwrap();

        let outcome = apply_atomic(&link, |_| Ok("new\n".into())).unwrap();

        assert_eq!(outcome, ApplyOutcome::Updated);
        // The link must survive as a link, not be replaced by a regular file.
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink must survive the write"
        );
        // The write landed on the real file the link points at.
        assert_eq!(fs::read_to_string(&target).unwrap(), "new\n");
        assert_eq!(fs::read_to_string(&link).unwrap(), "new\n");
    }

    #[cfg(unix)]
    #[test]
    fn apply_through_dangling_relative_symlink_creates_target_and_keeps_link() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let target_dir = tmp.path().join("dotfiles");
        fs::create_dir(&target_dir).unwrap();
        let target = target_dir.join("settings.json");
        let link = tmp.path().join("settings.json");
        symlink("dotfiles/settings.json", &link).unwrap();

        let outcome = apply_atomic(&link, |_| Ok("new\n".into())).unwrap();

        assert_eq!(outcome, ApplyOutcome::Created);
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "new\n");
    }

    #[cfg(unix)]
    #[test]
    fn apply_rejects_a_symlink_loop_without_replacing_it() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let link = tmp.path().join("settings.json");
        symlink("settings.json", &link).unwrap();

        let error = apply_atomic(&link, |_| Ok("new\n".into())).unwrap_err();

        assert!(format!("{error:#}").contains("symlink chain deeper"));
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn json_mutator_preserves_user_keys() {
        let original = r#"{"unrelated":"keep me","mcpServers":{"foo":{"url":"http://foo"}}}"#;
        let out = mutate_json(original, |m| {
            let servers = m
                .entry("mcpServers")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .unwrap();
            servers.insert(
                "ai-memory".into(),
                serde_json::json!({"url": "http://homelab:49374/mcp"}),
            );
            Ok(())
        })
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        // Unrelated key survives.
        assert_eq!(parsed["unrelated"], "keep me");
        // Sibling MCP server survives.
        assert_eq!(parsed["mcpServers"]["foo"]["url"], "http://foo");
        // Ours is added.
        assert_eq!(
            parsed["mcpServers"]["ai-memory"]["url"],
            "http://homelab:49374/mcp"
        );
    }

    #[test]
    fn json_mutator_rejects_non_object_root() {
        let err = mutate_json("[1,2,3]", |_| Ok(())).unwrap_err();
        assert!(format!("{err:?}").contains("not an object"));
    }

    #[test]
    fn json_mutator_rejects_invalid_json() {
        let err = mutate_json("{not valid", |_| Ok(())).unwrap_err();
        assert!(format!("{err:?}").contains("isn't valid JSON"));
    }

    #[test]
    fn toml_mutator_preserves_comments_and_other_tables() {
        let original = "# top comment kept\n\
                        [other]\n\
                        keep = \"this\"\n";
        let out = mutate_toml(original, |doc| {
            doc["mcp_servers"]["ai-memory"]["url"] = toml_edit::value("http://homelab:49374/mcp");
            Ok(())
        })
        .unwrap();
        assert!(out.contains("# top comment kept"));
        assert!(out.contains("[other]"));
        assert!(out.contains("keep = \"this\""));
        assert!(out.contains("ai-memory"));
        assert!(out.contains("http://homelab:49374/mcp"));
    }

    #[test]
    fn idempotent_double_apply_second_is_noop() {
        // The realistic flow: user runs --apply twice in a row,
        // second call should be a clean no-op.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("settings.json");

        let mutator = |s: &str| {
            mutate_json(s, |m| {
                m.insert("foo".into(), serde_json::json!("bar"));
                Ok(())
            })
        };
        let first = apply_atomic(&p, mutator).unwrap();
        assert_eq!(first, ApplyOutcome::Created);
        let second = apply_atomic(&p, mutator).unwrap();
        assert_eq!(second, ApplyOutcome::NoOp);
    }
}
