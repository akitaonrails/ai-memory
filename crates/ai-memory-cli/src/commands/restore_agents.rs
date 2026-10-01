//! `ai-memory restore-agents` - restore host agent assets from a bounded archive.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use ai_memory_core::agent_backup::{
    AGENT_BACKUP_SCHEMA_VERSION, AgentAssetScope, AgentBackupEntry, AgentBackupManifest,
};
use ai_memory_core::ids::AgentKind;
use anyhow::{Context as _, Result, anyhow, bail};
use flate2::read::GzDecoder;
use sha2::{Digest as _, Sha256};
use tar::Archive;
use tracing::info;

use crate::cli::{AgentBackupScope, McpClient, RestoreAgentsArgs};
use crate::commands::backup_agents::matches_agent_filter;
use crate::commands::path_util::{claude_config_dir, home_dir};
use crate::config::Config;

const MAX_ARCHIVE_ENTRIES: usize = 20_000;
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ASSET_BYTES: u64 = 128 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug)]
struct StagedAsset {
    path: PathBuf,
    size: u64,
    sha256: String,
}

#[derive(Debug)]
struct PlannedRestore<'a> {
    entry: &'a AgentBackupEntry,
    target_path: PathBuf,
    staged: &'a StagedAsset,
    exists: bool,
}

#[derive(Debug)]
struct AppliedRestore {
    target_path: PathBuf,
    backup_path: Option<PathBuf>,
    original_mode: Option<u32>,
}

/// Run the `restore-agents` subcommand.
///
/// # Errors
/// Returns an error when the archive is malformed, over budget, incomplete,
/// inconsistent with its manifest, or cannot be restored safely.
pub fn run(_config: &Config, args: RestoreAgentsArgs) -> Result<()> {
    let home = home_dir().context("locating user home directory")?;
    let cwd = std::env::current_dir().context("locating current working directory")?;

    if !args.from.is_file() {
        bail!("backup archive not found at {}", args.from.display());
    }

    let staging = tempfile::tempdir().context("creating restore staging directory")?;
    let (manifest_raw, files_by_path) = stage_archive(&args.from, staging.path())?;
    let manifest: AgentBackupManifest =
        serde_json::from_slice(&manifest_raw).context("parsing agent backup manifest")?;
    if manifest.version == 0 || manifest.version > AGENT_BACKUP_SCHEMA_VERSION {
        bail!(
            "archive manifest schema version {} is not supported (supported: 1..={})",
            manifest.version,
            AGENT_BACKUP_SCHEMA_VERSION
        );
    }

    let claude_override = claude_config_dir(std::env::var_os("CLAUDE_CONFIG_DIR"));
    let mut archive_paths = HashSet::with_capacity(manifest.entries.len());
    let mut target_paths = HashSet::with_capacity(manifest.entries.len());
    let mut planned = Vec::new();
    let mut identical_count = 0;

    for entry in &manifest.entries {
        validate_archive_path(&entry.archive_path)?;
        validate_target_relative(entry.scope, &entry.target_relative)?;
        if !archive_paths.insert(entry.archive_path.clone()) {
            bail!(
                "archive manifest contains duplicate asset path {:?}",
                entry.archive_path
            );
        }

        let staged = files_by_path.get(&entry.archive_path).ok_or_else(|| {
            anyhow!(
                "archive manifest references missing asset {:?}",
                entry.archive_path
            )
        })?;
        let expected_sha = entry.sha256.as_deref().ok_or_else(|| {
            anyhow!(
                "archive asset {:?} is missing its required SHA-256 checksum",
                entry.archive_path
            )
        })?;
        validate_sha256(expected_sha, &entry.archive_path)?;
        if !staged.sha256.eq_ignore_ascii_case(expected_sha) {
            bail!(
                "SHA-256 integrity mismatch for asset {}: expected {}, got {}",
                entry.archive_path,
                expected_sha,
                staged.sha256
            );
        }
        if entry.mode.is_some_and(|mode| mode > 0o777) {
            bail!(
                "archive asset {:?} has unsafe permission bits {:o}",
                entry.archive_path,
                entry.mode.unwrap_or_default()
            );
        }

        let (base, target_path) = resolve_target(entry, &home, &cwd, claude_override.as_deref())?;
        if !target_paths.insert(target_path.clone()) {
            bail!(
                "archive manifest maps more than one asset to {}",
                target_path.display()
            );
        }
        reject_symlinked_target(&base, &target_path)?;

        let matches_agent = args
            .agents
            .as_ref()
            .is_none_or(|filter| matches_agent_filter(entry.agent, filter));
        let matches_scope = match args.scope {
            AgentBackupScope::Both => true,
            AgentBackupScope::Global => entry.scope == AgentAssetScope::Global,
            AgentBackupScope::Project => entry.scope == AgentAssetScope::Project,
        };
        if !matches_agent || !matches_scope {
            continue;
        }

        let exists = match fs::symlink_metadata(&target_path) {
            Ok(metadata) if metadata.file_type().is_file() => true,
            Ok(_) => bail!(
                "refusing to replace non-file destination {}",
                target_path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(error).with_context(|| format!("inspecting {}", target_path.display()));
            }
        };
        let identical = exists && files_have_same_digest(&target_path, staged)?;
        if identical {
            identical_count += 1;
        } else {
            planned.push(PlannedRestore {
                entry,
                target_path,
                staged,
                exists,
            });
        }
    }

    for archive_path in files_by_path.keys() {
        if !archive_paths.contains(archive_path) {
            bail!("archive contains unmanifested asset {:?}", archive_path);
        }
    }

    if planned.is_empty() {
        if identical_count > 0 {
            println!(
                "All {identical_count} selected assets are already identical on host. Nothing to restore."
            );
        } else {
            println!("No archive assets matched the selected agent and scope filters.");
        }
        return Ok(());
    }

    let any_sanitized = manifest.sanitized || manifest.entries.iter().any(|entry| entry.sanitized);
    if !args.apply {
        print_dry_run(&planned, identical_count, any_sanitized);
        return Ok(());
    }

    let summary = apply_planned(&planned, args.force)?;
    info!(
        created = summary.created,
        updated = summary.updated,
        identical_count,
        skipped_existing = summary.skipped_existing,
        "restored agent assets"
    );
    println!(
        "\nRestoration complete: {} created, {} updated, {} identical, {} skipped (existing).",
        summary.created, summary.updated, identical_count, summary.skipped_existing
    );
    if any_sanitized {
        eprintln!(
            "Note: this archive contains sanitized configurations. Restored MCP configs may require replacing '[REDACTED:...]' values with valid credentials."
        );
    }
    Ok(())
}

fn stage_archive(src: &Path, staging: &Path) -> Result<(Vec<u8>, HashMap<String, StagedAsset>)> {
    let file = File::open(src).with_context(|| format!("opening archive at {}", src.display()))?;
    let mut archive = Archive::new(GzDecoder::new(file));
    let mut files_by_path = HashMap::new();
    let mut manifest_bytes = None;
    let mut total_bytes = 0_u64;

    for (index, entry) in archive.entries()?.enumerate() {
        if index >= MAX_ARCHIVE_ENTRIES {
            bail!("archive contains more than {MAX_ARCHIVE_ENTRIES} entries");
        }
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            bail!("archive entry {} is not a regular file", index + 1);
        }
        let path = entry
            .path()?
            .to_str()
            .context("archive contains a non-UTF-8 path")?
            .to_owned();
        validate_archive_path(&path)?;
        let size = entry.size();
        let limit = if path == "manifest.json" {
            MAX_MANIFEST_BYTES
        } else {
            MAX_ASSET_BYTES
        };
        if size > limit {
            bail!("archive entry {:?} exceeds the {} byte limit", path, limit);
        }
        total_bytes = total_bytes
            .checked_add(size)
            .filter(|total| *total <= MAX_ARCHIVE_BYTES)
            .ok_or_else(|| anyhow!("archive expands beyond the {MAX_ARCHIVE_BYTES} byte limit"))?;

        if path == "manifest.json" {
            if manifest_bytes.is_some() {
                bail!("archive contains more than one manifest.json");
            }
            let mut bytes = Vec::with_capacity(size as usize);
            entry.read_to_end(&mut bytes)?;
            manifest_bytes = Some(bytes);
            continue;
        }
        if files_by_path.contains_key(&path) {
            bail!("archive contains duplicate asset path {:?}", path);
        }
        let staged_path = staging.join(format!("asset-{index:05}"));
        let mut output = File::create(&staged_path)
            .with_context(|| format!("staging archive entry {:?}", path))?;
        let mut hasher = Sha256::new();
        let mut copied = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = entry.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            copied += read as u64;
            if copied > limit {
                bail!("archive entry {:?} exceeds the {} byte limit", path, limit);
            }
            hasher.update(&buffer[..read]);
            output.write_all(&buffer[..read])?;
        }
        if copied != size {
            bail!(
                "archive entry {:?} declared {size} bytes but yielded {copied}",
                path
            );
        }
        files_by_path.insert(
            path,
            StagedAsset {
                path: staged_path,
                size,
                sha256: format!("{:x}", hasher.finalize()),
            },
        );
    }

    Ok((
        manifest_bytes
            .ok_or_else(|| anyhow!("archive at {} is missing manifest.json", src.display()))?,
        files_by_path,
    ))
}

fn resolve_target(
    entry: &AgentBackupEntry,
    home: &Path,
    cwd: &Path,
    claude_override: Option<&Path>,
) -> Result<(PathBuf, PathBuf)> {
    if entry.scope == AgentAssetScope::Project {
        return Ok((cwd.to_path_buf(), cwd.join(&entry.target_relative)));
    }
    if entry.agent == AgentKind::ClaudeDesktop {
        if entry.target_relative != ".claude/claude_desktop_config.json" {
            bail!(
                "Claude Desktop asset has unexpected destination {:?}",
                entry.target_relative
            );
        }
        let target = crate::commands::install_mcp::mcp_config_path(McpClient::ClaudeDesktop)?;
        let base = target
            .parent()
            .context("Claude Desktop config path has no parent directory")?
            .to_path_buf();
        return Ok((base, target));
    }
    if entry.agent == AgentKind::OpenCode
        && let Some(relative) = entry.target_relative.strip_prefix(".config/opencode/")
    {
        let config = crate::commands::install_mcp::mcp_config_path(McpClient::OpenCode)?;
        let base = config
            .parent()
            .context("OpenCode config path has no parent directory")?
            .to_path_buf();
        return Ok((base.clone(), base.join(relative)));
    }
    if entry.agent == AgentKind::ClaudeCode
        && let Some(custom_dir) = claude_override
        && let Some(relative) = entry.target_relative.strip_prefix(".claude/")
    {
        return Ok((custom_dir.to_path_buf(), custom_dir.join(relative)));
    }
    Ok((home.to_path_buf(), home.join(&entry.target_relative)))
}

fn reject_symlinked_target(base: &Path, target: &Path) -> Result<()> {
    let relative = target.strip_prefix(base).with_context(|| {
        format!(
            "restore destination {} escaped its approved base {}",
            target.display(),
            base.display()
        )
    })?;
    let mut current = base.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            bail!("restore destination contains a non-normal path component");
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "refusing to restore through symlinked path {}",
                    current.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("inspecting {}", current.display()));
            }
        }
    }
    Ok(())
}

fn files_have_same_digest(path: &Path, staged: &StagedAsset) -> Result<bool> {
    if fs::metadata(path)?.len() != staged.size {
        return Ok(false);
    }
    Ok(hash_file(path)? == staged.sha256)
}

fn hash_file(path: &Path) -> Result<String> {
    let mut input = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[derive(Debug, Default)]
struct RestoreSummary {
    created: usize,
    updated: usize,
    skipped_existing: usize,
}

fn apply_planned(planned: &[PlannedRestore<'_>], force: bool) -> Result<RestoreSummary> {
    let mut summary = RestoreSummary::default();
    let mut applied = Vec::new();

    for item in planned {
        if item.exists && !force {
            println!(
                "  [SKIP] {} (already exists, pass --force to overwrite)",
                item.target_path.display()
            );
            summary.skipped_existing += 1;
            continue;
        }
        let result = apply_one(item, &mut applied);
        if let Err(error) = result {
            let rollback_errors = rollback_applied(&applied);
            if rollback_errors.is_empty() {
                return Err(error).context(format!(
                    "restoration failed after {} write(s); earlier writes were rolled back",
                    applied.len()
                ));
            }
            return Err(error).context(format!(
                "restoration failed and rollback was incomplete: {}",
                rollback_errors.join("; ")
            ));
        }
        if item.exists {
            println!("  updated {}", item.target_path.display());
            summary.updated += 1;
        } else {
            println!("  created {}", item.target_path.display());
            summary.created += 1;
        }
    }
    Ok(summary)
}

fn apply_one(item: &PlannedRestore<'_>, applied: &mut Vec<AppliedRestore>) -> Result<()> {
    if let Some(parent) = item.target_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
    }
    let original_mode = portable_mode(&item.target_path)?;
    let backup_path = if item.exists {
        let backup = unique_backup_path(&item.target_path)?;
        fs::copy(&item.target_path, &backup).with_context(|| {
            format!(
                "backing up {} to {}",
                item.target_path.display(),
                backup.display()
            )
        })?;
        Some(backup)
    } else {
        None
    };
    applied.push(AppliedRestore {
        target_path: item.target_path.clone(),
        backup_path,
        original_mode,
    });

    let content = fs::read(&item.staged.path)
        .with_context(|| format!("reading staged asset {}", item.entry.archive_path))?;
    ai_memory_wiki::write_atomic(&item.target_path, &content)
        .with_context(|| format!("writing {}", item.target_path.display()))?;
    set_portable_mode(&item.target_path, item.entry.mode)?;
    Ok(())
}

fn rollback_applied(applied: &[AppliedRestore]) -> Vec<String> {
    let mut errors = Vec::new();
    for item in applied.iter().rev() {
        let result = if let Some(backup) = &item.backup_path {
            fs::read(backup)
                .with_context(|| format!("reading rollback copy {}", backup.display()))
                .and_then(|content| {
                    ai_memory_wiki::write_atomic(&item.target_path, &content)
                        .with_context(|| format!("restoring {}", item.target_path.display()))
                })
                .and_then(|_| set_portable_mode(&item.target_path, item.original_mode))
        } else {
            fs::remove_file(&item.target_path)
                .with_context(|| format!("removing {}", item.target_path.display()))
        };
        if let Err(error) = result {
            errors.push(format!("{}: {error:#}", item.target_path.display()));
        }
    }
    errors
}

fn unique_backup_path(target: &Path) -> Result<PathBuf> {
    let stamp = jiff::Timestamp::now().as_microsecond();
    for suffix in 0_u32..=u32::MAX {
        let mut name = target.as_os_str().to_owned();
        name.push(if suffix == 0 {
            format!(".bak-{stamp}")
        } else {
            format!(".bak-{stamp}-{suffix}")
        });
        let candidate = PathBuf::from(name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    bail!(
        "could not allocate a unique backup path beside {}",
        target.display()
    )
}

fn portable_mode(path: &Path) -> Result<Option<u32>> {
    if !path.exists() {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        Ok(Some(fs::metadata(path)?.permissions().mode() & 0o777))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

fn set_portable_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))
            .with_context(|| format!("restoring permissions on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

fn print_dry_run(planned: &[PlannedRestore<'_>], identical_count: usize, any_sanitized: bool) {
    println!("Restore preview (dry-run, pass --apply to execute):\n");
    for item in planned {
        let status = if item.exists {
            "[OVERWRITE]"
        } else {
            "[CREATE]"
        };
        println!(
            "  {status:<12} [{}] {} ({} bytes) -> {}",
            item.entry.agent.as_str(),
            item.entry.asset_kind.label(),
            item.staged.size,
            item.target_path.display()
        );
    }
    if identical_count > 0 {
        println!("\n  (skipped {identical_count} identical files)");
    }
    if any_sanitized {
        println!("\n  Note: archive contains sanitized configurations with redacted credentials.");
    }
    println!(
        "\nReview the archive source and listed destinations: skills, plugins, and instructions are active content. Rerun with `--apply` to restore."
    );
}

fn validate_sha256(value: &str, archive_path: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!(
            "archive asset {:?} has a malformed SHA-256 checksum",
            archive_path
        );
    }
    Ok(())
}

fn validate_archive_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.contains('\\')
        || path.contains('\0')
        || (path.len() >= 2 && path.as_bytes()[1] == b':')
        || path
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        bail!("malformed or dangerous archive path: {:?}", path);
    }
    Ok(())
}

fn validate_target_relative(scope: AgentAssetScope, rel_path: &str) -> Result<()> {
    validate_archive_path(rel_path)?;
    if !Path::new(rel_path)
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        bail!(
            "target relative path contains non-normal components: {:?}",
            rel_path
        );
    }

    match scope {
        AgentAssetScope::Global => {
            const PREFIXES: &[&str] = &[
                ".claude/",
                ".codex/",
                ".agents/",
                ".gemini/",
                ".cursor/",
                ".config/opencode/",
                ".devin/",
                ".grok/",
                ".kiro/",
                ".openclaw/",
                ".commandcode/",
                ".kimi/",
            ];
            let allowed = rel_path == ".claude.json"
                || PREFIXES.iter().any(|prefix| rel_path.starts_with(prefix));
            if !allowed {
                bail!(
                    "refusing to restore global asset to unauthorized path: {:?}",
                    rel_path
                );
            }
        }
        AgentAssetScope::Project => {
            const EXACT: &[&str] = &[
                "CLAUDE.md",
                "AGENTS.md",
                "GEMINI.md",
                ".cursorrules",
                "opencode.json",
                "opencode.jsonc",
                ".vscode/mcp.json",
                ".cursor/mcp.json",
                ".grok/config.toml",
            ];
            const PREFIXES: &[&str] = &[
                ".claude/skills/",
                ".agents/skills/",
                ".cursor/rules/",
                ".gemini/skills/",
                ".devin/skills/",
                ".grok/skills/",
            ];
            let allowed = EXACT.contains(&rel_path)
                || PREFIXES.iter().any(|prefix| rel_path.starts_with(prefix));
            if !allowed {
                bail!(
                    "refusing to restore project asset to unauthorized path: {:?}",
                    rel_path
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_removes_new_files_and_restores_overwrites() {
        let root = tempfile::tempdir().unwrap();
        let existing = root.path().join("existing");
        let created = root.path().join("created");
        let backup = root.path().join("existing.bak");
        fs::write(&existing, b"new").unwrap();
        fs::write(&backup, b"old").unwrap();
        fs::write(&created, b"created").unwrap();

        let errors = rollback_applied(&[
            AppliedRestore {
                target_path: existing.clone(),
                backup_path: Some(backup),
                original_mode: None,
            },
            AppliedRestore {
                target_path: created.clone(),
                backup_path: None,
                original_mode: None,
            },
        ]);

        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(fs::read(existing).unwrap(), b"old");
        assert!(!created.exists());
    }
}
