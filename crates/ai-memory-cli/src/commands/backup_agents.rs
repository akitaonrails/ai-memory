//! `ai-memory backup-agents` — snapshot AI agent configurations,
//! skills, plugins, and instructions into a portable archive.

use std::fs::{self, File};
use std::io::Cursor;
use std::path::{Path, PathBuf};

use ai_memory_core::agent_backup::{
    AgentAssetKind, AgentAssetScope, AgentBackupEntry, AgentBackupManifest, HostInfo,
};
use ai_memory_core::ids::AgentKind;
use ai_memory_core::sanitize::{SanitizeConfig, Sanitizer};
use anyhow::{Context, Result};
use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use tar::{Builder, Header};
use tracing::info;

use crate::cli::{AgentBackupScope, BackupAgentsArgs, McpClient};
use crate::commands::path_util::{claude_config_dir, home_dir};
use crate::config::Config;

const MAX_BACKUP_ASSET_BYTES: u64 = 128 * 1024 * 1024;

/// An asset candidate discovered on the host filesystem.
#[derive(Debug, Clone)]
pub struct DiscoveredAsset {
    /// Agent owning the asset.
    pub agent: AgentKind,
    /// Kind of asset.
    pub kind: AgentAssetKind,
    /// Global or project-scoped.
    pub scope: AgentAssetScope,
    /// Absolute path on host disk.
    pub source_path: PathBuf,
    /// Target path relative to destination root (home or cwd).
    pub target_relative: String,
    /// Archive destination path inside the tarball.
    pub archive_path: String,
}

/// Run the `backup-agents` subcommand.
///
/// # Errors
/// Returns an error if directory traversal fails, source files cannot be read,
/// or writing the output archive fails.
pub fn run(_config: &Config, args: BackupAgentsArgs) -> Result<()> {
    let home = home_dir().context("locating user home directory")?;
    let cwd = std::env::current_dir().context("locating current working directory")?;

    let mut discovered = discover_assets(&home, &cwd, &args)?;

    // Never consume the archive while replacing it. This matters when an
    // existing destination sits under one of the collected plugin trees.
    let destination_identity = fs::canonicalize(&args.to).ok();
    discovered.retain(|asset| {
        destination_identity
            .as_ref()
            .is_none_or(|dest| fs::canonicalize(&asset.source_path).ok().as_ref() != Some(dest))
    });
    discovered.sort_by(|a, b| a.archive_path.cmp(&b.archive_path));

    if discovered.is_empty() {
        println!("No AI agent assets detected for backup.");
        return Ok(());
    }

    if args.dry_run {
        print_dry_run(&discovered);
        return Ok(());
    }

    let dest = &args.to;
    let parent = dest
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("creating parent dir for {}", dest.display()))?;

    // Agent instructions and plugins are copied verbatim and may contain
    // credentials even in the default sanitized mode. Keep every archive
    // private, and replace the destination only after the gzip stream closes.
    let temporary = tempfile::Builder::new()
        .prefix(".ai-memory-agent-backup.")
        .tempfile_in(parent)
        .with_context(|| format!("creating temporary archive beside {}", dest.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| format!("securing temporary archive beside {}", dest.display()))?;
    }

    let count = build_archive(
        temporary
            .reopen()
            .context("reopening temporary agent archive")?,
        &discovered,
        args.include_secrets,
    )?;
    temporary
        .as_file()
        .sync_all()
        .context("syncing temporary agent archive")?;
    let persisted = temporary
        .persist(dest)
        .map_err(|error| error.error)
        .with_context(|| format!("replacing output archive at {}", dest.display()))?;
    persisted
        .sync_all()
        .with_context(|| format!("syncing output archive at {}", dest.display()))?;
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }

    let size = fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    info!(path = %dest.display(), bytes = size, count, "agent assets backup written");

    println!(
        "✓ Backed up {} agent assets to {} ({})",
        count,
        dest.display(),
        human_bytes(size)
    );

    if args.include_secrets {
        eprintln!(
            "⚠️  Warning: Secrets were included without sanitization. Archive mode is 0600 on Unix; protect the file on other platforms."
        );
    }

    Ok(())
}

fn print_dry_run(discovered: &[DiscoveredAsset]) {
    println!(
        "Found {} assets across AI agents (dry-run):",
        discovered.len()
    );
    for asset in discovered {
        println!(
            "  [{:?}] {:<12} {:<8} -> {}",
            asset.agent,
            asset.kind.label(),
            match asset.scope {
                AgentAssetScope::Global => "global",
                AgentAssetScope::Project => "project",
            },
            asset.source_path.display()
        );
    }
}

/// Discover agent assets based on configuration and scopes.
pub fn discover_assets(
    home: &Path,
    cwd: &Path,
    args: &BackupAgentsArgs,
) -> Result<Vec<DiscoveredAsset>> {
    let mut assets = Vec::new();
    let want_global = matches!(
        args.scope,
        AgentBackupScope::Global | AgentBackupScope::Both
    );
    let want_project = matches!(
        args.scope,
        AgentBackupScope::Project | AgentBackupScope::Both
    );

    let filter_agent = |agent: AgentKind| -> bool {
        let Some(filter) = &args.agents else {
            return true;
        };
        matches_agent_filter(agent, filter)
    };

    // 1. Claude Code / Desktop
    if filter_agent(AgentKind::ClaudeCode) {
        if want_global {
            let claude_dir = claude_config_dir(std::env::var_os("CLAUDE_CONFIG_DIR"))
                .unwrap_or_else(|| home.join(".claude"));
            // Global MCP / Settings
            push_if_file(
                &mut assets,
                AgentKind::ClaudeCode,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Global,
                claude_dir.join("settings.json"),
                ".claude/settings.json",
                "claude/mcp/settings.json",
            );
            push_if_file(
                &mut assets,
                AgentKind::ClaudeCode,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Global,
                home.join(".claude.json"),
                ".claude.json",
                "claude/mcp/claude.json",
            );
            // Global Skills
            collect_dir_assets(
                &mut assets,
                AgentKind::ClaudeCode,
                AgentAssetKind::Skill,
                AgentAssetScope::Global,
                &claude_dir.join("skills"),
                ".claude/skills",
                "claude/skills",
            )?;
            // Plugins
            collect_dir_assets(
                &mut assets,
                AgentKind::ClaudeCode,
                AgentAssetKind::Plugin,
                AgentAssetScope::Global,
                &claude_dir.join("plugins"),
                ".claude/plugins",
                "claude/plugins",
            )?;
        }
        if want_project {
            // Project Skills
            collect_dir_assets(
                &mut assets,
                AgentKind::ClaudeCode,
                AgentAssetKind::Skill,
                AgentAssetScope::Project,
                &cwd.join(".claude").join("skills"),
                ".claude/skills",
                "project/claude/skills",
            )?;
            // Project Instructions
            push_if_file(
                &mut assets,
                AgentKind::ClaudeCode,
                AgentAssetKind::Instruction,
                AgentAssetScope::Project,
                cwd.join("CLAUDE.md"),
                "CLAUDE.md",
                "project/CLAUDE.md",
            );
        }
    }

    // Reuse the installer path so platform-specific Claude Desktop configs
    // are backed up and restored at the same location.
    if filter_agent(AgentKind::ClaudeDesktop) && want_global {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let desktop_cfg =
                crate::commands::install_mcp::mcp_config_path(McpClient::ClaudeDesktop)?;
            push_if_file(
                &mut assets,
                AgentKind::ClaudeDesktop,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Global,
                desktop_cfg,
                ".claude/claude_desktop_config.json",
                "claude-desktop/claude_desktop_config.json",
            );
        }
    }

    // 2. OpenAI Codex CLI
    if filter_agent(AgentKind::Codex) {
        if want_global {
            let codex_dir = home.join(".codex");
            push_if_file(
                &mut assets,
                AgentKind::Codex,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Global,
                codex_dir.join("config.toml"),
                ".codex/config.toml",
                "codex/config.toml",
            );
            push_if_file(
                &mut assets,
                AgentKind::Codex,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Global,
                codex_dir.join("mcp.json"),
                ".codex/mcp.json",
                "codex/mcp.json",
            );
            collect_dir_assets(
                &mut assets,
                AgentKind::Codex,
                AgentAssetKind::Skill,
                AgentAssetScope::Global,
                &home.join(".agents").join("skills"),
                ".agents/skills",
                "codex/skills/agents",
            )?;
            collect_dir_assets(
                &mut assets,
                AgentKind::Codex,
                AgentAssetKind::Skill,
                AgentAssetScope::Global,
                &codex_dir.join("skills"),
                ".codex/skills",
                "codex/skills/codex",
            )?;
            collect_dir_assets(
                &mut assets,
                AgentKind::Codex,
                AgentAssetKind::Plugin,
                AgentAssetScope::Global,
                &codex_dir.join("plugins"),
                ".codex/plugins",
                "codex/plugins",
            )?;
        }
        if want_project {
            collect_dir_assets(
                &mut assets,
                AgentKind::Codex,
                AgentAssetKind::Skill,
                AgentAssetScope::Project,
                &cwd.join(".agents").join("skills"),
                ".agents/skills",
                "project/agents/skills",
            )?;
            push_if_file(
                &mut assets,
                AgentKind::Codex,
                AgentAssetKind::Instruction,
                AgentAssetScope::Project,
                cwd.join("AGENTS.md"),
                "AGENTS.md",
                "project/AGENTS.md",
            );
        }
    }

    // 3. Antigravity CLI
    if filter_agent(AgentKind::AntigravityCli) {
        let gemini_dir = home.join(".gemini");
        if want_global {
            push_if_file(
                &mut assets,
                AgentKind::AntigravityCli,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Global,
                gemini_dir.join("config").join("mcp_config.json"),
                ".gemini/config/mcp_config.json",
                "antigravity/mcp_config.json",
            );
            push_if_file(
                &mut assets,
                AgentKind::AntigravityCli,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Global,
                gemini_dir.join("antigravity-cli").join("mcp_config.json"),
                ".gemini/antigravity-cli/mcp_config.json",
                "antigravity/antigravity_mcp.json",
            );
            collect_dir_assets(
                &mut assets,
                AgentKind::AntigravityCli,
                AgentAssetKind::Skill,
                AgentAssetScope::Global,
                &gemini_dir.join("antigravity-cli").join("skills"),
                ".gemini/antigravity-cli/skills",
                "antigravity/skills",
            )?;
            collect_dir_assets(
                &mut assets,
                AgentKind::AntigravityCli,
                AgentAssetKind::Plugin,
                AgentAssetScope::Global,
                &gemini_dir.join("antigravity-cli").join("plugins"),
                ".gemini/antigravity-cli/plugins",
                "antigravity/plugins",
            )?;
        }
        if want_project {
            collect_dir_assets(
                &mut assets,
                AgentKind::AntigravityCli,
                AgentAssetKind::Skill,
                AgentAssetScope::Project,
                &cwd.join(".gemini").join("skills"),
                ".gemini/skills",
                "project/gemini/skills",
            )?;
        }
    }

    // 4. Gemini CLI
    if filter_agent(AgentKind::GeminiCli) {
        let gemini_dir = home.join(".gemini");
        if want_global {
            push_if_file(
                &mut assets,
                AgentKind::GeminiCli,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Global,
                gemini_dir.join("settings.json"),
                ".gemini/settings.json",
                "gemini/settings.json",
            );
            collect_dir_assets(
                &mut assets,
                AgentKind::GeminiCli,
                AgentAssetKind::Skill,
                AgentAssetScope::Global,
                &gemini_dir.join("skills"),
                ".gemini/skills",
                "gemini/skills",
            )?;
        }
        if want_project {
            push_if_file(
                &mut assets,
                AgentKind::GeminiCli,
                AgentAssetKind::Instruction,
                AgentAssetScope::Project,
                cwd.join("GEMINI.md"),
                "GEMINI.md",
                "project/GEMINI.md",
            );
        }
    }

    // 4. Cursor
    if filter_agent(AgentKind::Cursor) {
        if want_global {
            push_if_file(
                &mut assets,
                AgentKind::Cursor,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Global,
                home.join(".cursor").join("mcp.json"),
                ".cursor/mcp.json",
                "cursor/mcp.json",
            );
        }
        if want_project {
            push_if_file(
                &mut assets,
                AgentKind::Cursor,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Project,
                cwd.join(".cursor").join("mcp.json"),
                ".cursor/mcp.json",
                "project/cursor/mcp.json",
            );
            push_if_file(
                &mut assets,
                AgentKind::Cursor,
                AgentAssetKind::Instruction,
                AgentAssetScope::Project,
                cwd.join(".cursorrules"),
                ".cursorrules",
                "project/cursor/.cursorrules",
            );
            collect_dir_assets(
                &mut assets,
                AgentKind::Cursor,
                AgentAssetKind::Instruction,
                AgentAssetScope::Project,
                &cwd.join(".cursor").join("rules"),
                ".cursor/rules",
                "project/cursor/rules",
            )?;
        }
    }

    // 5. OpenCode (v1 / v2)
    if filter_agent(AgentKind::OpenCode) {
        if want_global {
            let opencode_config =
                crate::commands::install_mcp::mcp_config_path(McpClient::OpenCode)?;
            let opencode_dir = opencode_config
                .parent()
                .map(Path::to_path_buf)
                .context("OpenCode config path has no parent directory")?;
            push_if_file(
                &mut assets,
                AgentKind::OpenCode,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Global,
                opencode_config,
                ".config/opencode/opencode.json",
                "opencode/opencode.json",
            );
            push_if_file(
                &mut assets,
                AgentKind::OpenCode,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Global,
                opencode_dir.join("opencode.jsonc"),
                ".config/opencode/opencode.jsonc",
                "opencode/opencode.jsonc",
            );
            collect_dir_assets(
                &mut assets,
                AgentKind::OpenCode,
                AgentAssetKind::Plugin,
                AgentAssetScope::Global,
                &opencode_dir.join("plugins"),
                ".config/opencode/plugins",
                "opencode/plugins",
            )?;
        }
        if want_project {
            push_if_file(
                &mut assets,
                AgentKind::OpenCode,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Project,
                cwd.join("opencode.json"),
                "opencode.json",
                "project/opencode/opencode.json",
            );
            push_if_file(
                &mut assets,
                AgentKind::OpenCode,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Project,
                cwd.join("opencode.jsonc"),
                "opencode.jsonc",
                "project/opencode/opencode.jsonc",
            );
        }
    }

    // 6. Devin
    if filter_agent(AgentKind::Devin) {
        if want_global {
            let devin_dir = home.join(".devin");
            push_if_file(
                &mut assets,
                AgentKind::Devin,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Global,
                devin_dir.join("config.json"),
                ".devin/config.json",
                "devin/config.json",
            );
            collect_dir_assets(
                &mut assets,
                AgentKind::Devin,
                AgentAssetKind::Skill,
                AgentAssetScope::Global,
                &devin_dir.join("skills"),
                ".devin/skills",
                "devin/skills",
            )?;
        }
        if want_project {
            collect_dir_assets(
                &mut assets,
                AgentKind::Devin,
                AgentAssetKind::Skill,
                AgentAssetScope::Project,
                &cwd.join(".devin").join("skills"),
                ".devin/skills",
                "project/devin/skills",
            )?;
        }
    }

    // 7. Grok
    if filter_agent(AgentKind::Grok) {
        if want_global {
            let grok_dir = home.join(".grok");
            push_if_file(
                &mut assets,
                AgentKind::Grok,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Global,
                grok_dir.join("config.toml"),
                ".grok/config.toml",
                "grok/config.toml",
            );
            collect_dir_assets(
                &mut assets,
                AgentKind::Grok,
                AgentAssetKind::Skill,
                AgentAssetScope::Global,
                &grok_dir.join("skills"),
                ".grok/skills",
                "grok/skills",
            )?;
        }
        if want_project {
            push_if_file(
                &mut assets,
                AgentKind::Grok,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Project,
                cwd.join(".grok").join("config.toml"),
                ".grok/config.toml",
                "project/grok/config.toml",
            );
            collect_dir_assets(
                &mut assets,
                AgentKind::Grok,
                AgentAssetKind::Skill,
                AgentAssetScope::Project,
                &cwd.join(".grok").join("skills"),
                ".grok/skills",
                "project/grok/skills",
            )?;
        }
    }

    // 8. Kiro CLI
    if filter_agent(AgentKind::KiroCli) && want_global {
        let kiro_dir = home.join(".kiro");
        push_if_file(
            &mut assets,
            AgentKind::KiroCli,
            AgentAssetKind::McpConfig,
            AgentAssetScope::Global,
            kiro_dir.join("settings").join("mcp.json"),
            ".kiro/settings/mcp.json",
            "kiro/mcp.json",
        );
        collect_dir_assets(
            &mut assets,
            AgentKind::KiroCli,
            AgentAssetKind::Skill,
            AgentAssetScope::Global,
            &kiro_dir.join("agents"),
            ".kiro/agents",
            "kiro/agents",
        )?;
    }

    // 9. OpenClaw
    if filter_agent(AgentKind::OpenClaw) && want_global {
        let openclaw_dir = home.join(".openclaw");
        push_if_file(
            &mut assets,
            AgentKind::OpenClaw,
            AgentAssetKind::McpConfig,
            AgentAssetScope::Global,
            openclaw_dir.join("config.json"),
            ".openclaw/config.json",
            "openclaw/config.json",
        );
        collect_dir_assets(
            &mut assets,
            AgentKind::OpenClaw,
            AgentAssetKind::Plugin,
            AgentAssetScope::Global,
            &openclaw_dir.join("extensions"),
            ".openclaw/extensions",
            "openclaw/extensions",
        )?;
    }

    // 10. Command Code
    if filter_agent(AgentKind::CommandCode) && want_global {
        let cmdc_dir = home.join(".commandcode");
        push_if_file(
            &mut assets,
            AgentKind::CommandCode,
            AgentAssetKind::McpConfig,
            AgentAssetScope::Global,
            cmdc_dir.join("mcp.json"),
            ".commandcode/mcp.json",
            "commandcode/mcp.json",
        );
    }

    // 11. Kimi Code
    if filter_agent(AgentKind::KimiCode) && want_global {
        let kimi_dir = home.join(".kimi");
        push_if_file(
            &mut assets,
            AgentKind::KimiCode,
            AgentAssetKind::McpConfig,
            AgentAssetScope::Global,
            kimi_dir.join("mcp.json"),
            ".kimi/mcp.json",
            "kimi/mcp.json",
        );
        collect_dir_assets(
            &mut assets,
            AgentKind::KimiCode,
            AgentAssetKind::Skill,
            AgentAssetScope::Global,
            &kimi_dir.join("skills"),
            ".kimi/skills",
            "kimi/skills",
        )?;
    }

    // 12. VS Code / Copilot
    if want_project {
        push_if_file(
            &mut assets,
            AgentKind::Other,
            AgentAssetKind::McpConfig,
            AgentAssetScope::Project,
            cwd.join(".vscode").join("mcp.json"),
            ".vscode/mcp.json",
            "project/vscode/mcp.json",
        );
    }

    Ok(assets)
}

fn push_if_file(
    assets: &mut Vec<DiscoveredAsset>,
    agent: AgentKind,
    kind: AgentAssetKind,
    scope: AgentAssetScope,
    source: PathBuf,
    target_relative: &str,
    archive_path: &str,
) {
    if fs::symlink_metadata(&source).is_ok_and(|metadata| metadata.file_type().is_file()) {
        assets.push(DiscoveredAsset {
            agent,
            kind,
            scope,
            source_path: source,
            target_relative: target_relative.to_string(),
            archive_path: archive_path.to_string(),
        });
    }
}

fn collect_dir_assets(
    assets: &mut Vec<DiscoveredAsset>,
    agent: AgentKind,
    kind: AgentAssetKind,
    scope: AgentAssetScope,
    dir: &Path,
    target_prefix: &str,
    archive_prefix: &str,
) -> Result<()> {
    if !fs::symlink_metadata(dir).is_ok_and(|metadata| metadata.file_type().is_dir()) {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            let dir_name = entry.file_name();
            let dir_name_str = dir_name.to_string_lossy();
            if dir_name_str.starts_with('.')
                || dir_name_str == "cache"
                || dir_name_str == "node_modules"
                || dir_name_str == "target"
                || dir_name_str == "venv"
                || dir_name_str == "__pycache__"
                || dir_name_str == "site-packages"
            {
                continue;
            }
            let sub_target = format!("{target_prefix}/{dir_name_str}");
            let sub_archive = format!("{archive_prefix}/{dir_name_str}");
            collect_dir_assets(assets, agent, kind, scope, &path, &sub_target, &sub_archive)?;
        } else if file_type.is_file() {
            let file_name = entry.file_name();
            let file_name_str = file_name.to_string_lossy();
            if file_name_str == ".DS_Store"
                || file_name_str.ends_with(".pyc")
                || file_name_str.ends_with(".pyo")
            {
                continue;
            }
            let target_rel = format!("{target_prefix}/{file_name_str}");
            let archive_path = format!("{archive_prefix}/{file_name_str}");
            assets.push(DiscoveredAsset {
                agent,
                kind,
                scope,
                source_path: path,
                target_relative: target_rel,
                archive_path,
            });
        }
    }
    Ok(())
}

fn build_archive(
    output_file: File,
    discovered: &[DiscoveredAsset],
    include_secrets: bool,
) -> Result<usize> {
    let encoder = GzEncoder::new(output_file, Compression::default());
    let mut tar = Builder::new(encoder);
    tar.mode(tar::HeaderMode::Deterministic);
    tar.follow_symlinks(false);

    let sanitizer = if include_secrets {
        None
    } else {
        Some(Sanitizer::new(&SanitizeConfig::default())?)
    };

    let mut entries = Vec::with_capacity(discovered.len());

    for asset in discovered {
        let source_size = fs::symlink_metadata(&asset.source_path)
            .with_context(|| format!("inspecting {}", asset.source_path.display()))?
            .len();
        if source_size > MAX_BACKUP_ASSET_BYTES {
            anyhow::bail!(
                "refusing to archive {}: {} bytes exceeds the per-asset limit of {}",
                asset.source_path.display(),
                source_size,
                MAX_BACKUP_ASSET_BYTES
            );
        }
        let raw_bytes = fs::read(&asset.source_path)
            .with_context(|| format!("reading {}", asset.source_path.display()))?;

        let (final_bytes, is_sanitized) = if let Some(ref sc) = sanitizer
            && matches!(asset.kind, AgentAssetKind::McpConfig)
        {
            if let Ok(text) = std::str::from_utf8(&raw_bytes) {
                let sanitized_text = sc.scrub(text);
                let changed = sanitized_text != text;
                (sanitized_text.into_bytes(), changed)
            } else {
                (raw_bytes, false)
            }
        } else {
            (raw_bytes, false)
        };

        let mut hasher = Sha256::new();
        hasher.update(&final_bytes);
        let sha256 = format!("{:x}", hasher.finalize());

        let mut header = Header::new_gnu();
        header.set_size(final_bytes.len() as u64);
        let mode = portable_mode(&asset.source_path)?;
        header.set_mode(mode.unwrap_or(0o600));
        header.set_mtime(0);
        header.set_cksum();

        tar.append_data(&mut header, &asset.archive_path, Cursor::new(&final_bytes))
            .with_context(|| format!("archiving {}", asset.archive_path))?;

        entries.push(AgentBackupEntry {
            agent: asset.agent,
            asset_kind: asset.kind,
            scope: asset.scope,
            archive_path: asset.archive_path.clone(),
            target_relative: asset.target_relative.clone(),
            sanitized: is_sanitized,
            sha256: Some(sha256),
            mode,
        });
    }

    let any_sanitized = entries.iter().any(|e| e.sanitized);
    let manifest = AgentBackupManifest::new(
        HostInfo {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            home_dir: None,
        },
        any_sanitized,
        entries,
    );

    let manifest_bytes =
        serde_json::to_vec_pretty(&manifest).context("serializing agent backup manifest")?;

    let mut manifest_header = Header::new_gnu();
    manifest_header.set_size(manifest_bytes.len() as u64);
    manifest_header.set_mode(0o644);
    manifest_header.set_mtime(0);
    manifest_header.set_cksum();

    tar.append_data(
        &mut manifest_header,
        "manifest.json",
        Cursor::new(&manifest_bytes),
    )
    .context("archiving manifest.json")?;

    let encoder = tar.into_inner().context("finalising tar archive")?;
    let output = encoder.finish().context("finalising gzip stream")?;
    output.sync_all().context("syncing gzip stream")?;
    Ok(discovered.len())
}

fn portable_mode(path: &Path) -> Result<Option<u32>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(path)
            .with_context(|| format!("reading permissions for {}", path.display()))?
            .permissions()
            .mode()
            & 0o777;
        Ok(Some(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} {}", UNITS[0])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// Match an agent against a list of filter strings, handling kebab-case, snake_case, and aliases.
pub(crate) fn matches_agent_filter(agent: AgentKind, filter_list: &[String]) -> bool {
    filter_list.iter().any(|f| {
        let normalized = f.trim().to_ascii_lowercase().replace('_', "-");
        if agent == AgentKind::Other {
            return matches!(
                normalized.as_str(),
                "other" | "vscode" | "vs-code" | "copilot" | "vscode-copilot"
            );
        }
        AgentKind::from_wire(&normalized) == agent
    })
}
