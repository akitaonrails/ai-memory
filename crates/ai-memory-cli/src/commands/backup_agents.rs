//! `ai-memory backup-agents` — snapshot AI agent configurations,
//! skills, plugins, and instructions into a portable archive.

use std::fs::{self, File};
use std::io::Cursor;
use std::path::{Path, PathBuf};

use ai_memory_core::agent_backup::{
    AgentAssetKind, AgentAssetScope, AgentBackupEntry, AgentBackupManifest, HostInfo,
};
use ai_memory_core::ids::AgentKind;
use ai_memory_core::routing_skills::{COPILOT_SKILL_DIR, GITHUB_SKILL_DIR, SKILLS_DIR};
use ai_memory_core::sanitize::{SanitizeConfig, Sanitizer};
use anyhow::{Context, Result};
use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use tar::{Builder, Header};
use tracing::info;

use crate::cli::{AgentBackupScope, BackupAgentsArgs, McpClient};
use crate::commands::backup::human_bytes;
use crate::commands::path_util::{
    agent_config_home, claude_config_dir, devin_user_config_dir_in, home_dir,
};
use crate::config::{Config, RuntimeEnv};

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
pub fn run(config: &Config, args: BackupAgentsArgs) -> Result<()> {
    let i18n = &config.i18n;
    let home = home_dir().context(i18n.text("cli-backup-agents-locating-user-home-directory"))?;
    let cwd = std::env::current_dir()
        .context(i18n.text("cli-backup-agents-locating-current-working-directory"))?;

    let mut discovered = discover_assets(&home, &cwd, &args, &config.runtime_env)?;

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
        println!(
            "{}",
            i18n.text("cli-backup-agents-no-ai-agent-assets-detected-for-backup")
        );
        return Ok(());
    }

    if args.dry_run {
        print_dry_run(i18n, &discovered);
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
            .context(i18n.text("cli-backup-agents-reopening-temporary-agent-archive"))?,
        &discovered,
        args.include_secrets,
    )?;
    temporary
        .as_file()
        .sync_all()
        .context(i18n.text("cli-backup-agents-syncing-temporary-agent-archive"))?;
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
        "{}",
        i18n.format(
            "cli-backup-agents-backed-up-agent-assets-to",
            &[
                ("value0", format!("{}", count)),
                ("value1", format!("{}", dest.display())),
                ("value2", human_bytes(size).to_string())
            ]
        )
    );

    if args.include_secrets {
        eprintln!(
            "{}", i18n.text("cli-backup-agents-warning-secrets-were-included-without-sanitization-archive-mode-is-0600-on-u"));
    }

    Ok(())
}

fn print_dry_run(i18n: &ai_memory_core::i18n::Localizer, discovered: &[DiscoveredAsset]) {
    println!(
        "{}",
        i18n.format(
            "cli-backup-agents-found-assets-across-ai-agents-dry-run",
            &[("value0", format!("{}", discovered.len()))]
        )
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
    env: &RuntimeEnv,
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
        let desktop_cfg = crate::commands::install_mcp::mcp_config_path(McpClient::ClaudeDesktop)?;
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

    // 2. OpenAI Codex CLI
    if filter_agent(AgentKind::Codex) {
        if want_global {
            let codex_dir =
                agent_config_home(env.var_os("CODEX_HOME")).unwrap_or_else(|| home.join(".codex"));
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
            // Hook commands can contain tokens, so use config-file redaction.
            push_if_file(
                &mut assets,
                AgentKind::Codex,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Global,
                codex_dir.join("hooks.json"),
                ".codex/hooks.json",
                "codex/hooks.json",
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
            let [app_skills, cli_skills] =
                super::install_skills::antigravity_global_skill_roots(home);
            collect_dir_assets(
                &mut assets,
                AgentKind::AntigravityCli,
                AgentAssetKind::Skill,
                AgentAssetScope::Global,
                &app_skills,
                ".gemini/config/skills",
                "antigravity/app-ide-skills",
            )?;
            collect_dir_assets(
                &mut assets,
                AgentKind::AntigravityCli,
                AgentAssetKind::Skill,
                AgentAssetScope::Global,
                &cli_skills,
                ".gemini/antigravity-cli/skills",
                "antigravity/cli-skills",
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
        // `.agents/skills` is shared with Codex. Attribute it to Codex when
        // both agents are selected so one archive never maps two entries to
        // the same restore target; an Antigravity-only backup still owns it.
        if want_project && !filter_agent(AgentKind::Codex) {
            collect_dir_assets(
                &mut assets,
                AgentKind::AntigravityCli,
                AgentAssetKind::Skill,
                AgentAssetScope::Project,
                &cwd.join(".agents").join("skills"),
                ".agents/skills",
                "project/antigravity/skills",
            )?;
        }
    }

    // 4. Gemini CLI
    if filter_agent(AgentKind::GeminiCli) {
        if want_global {
            let gemini_dir = agent_config_home(env.var_os("GEMINI_CLI_HOME"))
                .as_deref()
                .unwrap_or(home)
                .join(".gemini");
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
            collect_dir_assets(
                &mut assets,
                AgentKind::GeminiCli,
                AgentAssetKind::Plugin,
                AgentAssetScope::Global,
                &gemini_dir.join("plugins"),
                ".gemini/plugins",
                "gemini/plugins",
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
            collect_dir_assets(
                &mut assets,
                AgentKind::GeminiCli,
                AgentAssetKind::Skill,
                AgentAssetScope::Project,
                &cwd.join(".gemini").join("skills"),
                ".gemini/skills",
                "project/gemini/skills",
            )?;
        }
    }

    // 4b. Qwen Code
    if filter_agent(AgentKind::QwenCode) {
        let qwen_dir = home.join(".qwen");
        if want_global {
            push_if_file(
                &mut assets,
                AgentKind::QwenCode,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Global,
                qwen_dir.join("settings.json"),
                ".qwen/settings.json",
                "qwen/settings.json",
            );
            collect_dir_assets(
                &mut assets,
                AgentKind::QwenCode,
                AgentAssetKind::Skill,
                AgentAssetScope::Global,
                &qwen_dir.join("skills"),
                ".qwen/skills",
                "qwen/skills",
            )?;
            // `~/.agents/skills` is shared with Codex. Attribute it to Codex
            // when both are selected; a Qwen-only backup must still preserve
            // the shared skills Qwen documents and loads.
            if !filter_agent(AgentKind::Codex) {
                collect_dir_assets(
                    &mut assets,
                    AgentKind::QwenCode,
                    AgentAssetKind::Skill,
                    AgentAssetScope::Global,
                    &home.join(".agents").join("skills"),
                    ".agents/skills",
                    "qwen/skills/agents",
                )?;
            }
        }
        if want_project {
            push_if_file(
                &mut assets,
                AgentKind::QwenCode,
                AgentAssetKind::Instruction,
                AgentAssetScope::Project,
                cwd.join("QWEN.md"),
                "QWEN.md",
                "project/QWEN.md",
            );
            // Qwen also reads AGENTS.md. Codex owns the shared file in a
            // combined backup so the archive remains one-to-one.
            if !filter_agent(AgentKind::Codex) {
                push_if_file(
                    &mut assets,
                    AgentKind::QwenCode,
                    AgentAssetKind::Instruction,
                    AgentAssetScope::Project,
                    cwd.join("AGENTS.md"),
                    "AGENTS.md",
                    "project/qwen/AGENTS.md",
                );
            }
        }
    }

    // 4c. Qoder CLI and IDE
    if filter_agent(AgentKind::Qoder) {
        if want_global {
            push_if_file(
                &mut assets,
                AgentKind::Qoder,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Global,
                home.join(".qoder").join("settings.json"),
                ".qoder/settings.json",
                "qoder/settings.json",
            );
            if !filter_agent(AgentKind::Codex) && !filter_agent(AgentKind::QwenCode) {
                collect_dir_assets(
                    &mut assets,
                    AgentKind::Qoder,
                    AgentAssetKind::Skill,
                    AgentAssetScope::Global,
                    &home.join(".agents").join("skills"),
                    ".agents/skills",
                    "qoder/skills/agents",
                )?;
            }
        }
        if want_project && !filter_agent(AgentKind::Codex) && !filter_agent(AgentKind::QwenCode) {
            push_if_file(
                &mut assets,
                AgentKind::Qoder,
                AgentAssetKind::Instruction,
                AgentAssetScope::Project,
                cwd.join("AGENTS.md"),
                "AGENTS.md",
                "project/qoder/AGENTS.md",
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
            collect_dir_assets(
                &mut assets,
                AgentKind::OpenCode,
                AgentAssetKind::Skill,
                AgentAssetScope::Global,
                &opencode_dir.join("skills"),
                ".config/opencode/skills",
                "opencode/skills",
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
            // Devin reads its user config from %APPDATA%\devin on Windows, where
            // the installers write it; `.config/devin/` stays the archive path.
            let devin_dir = devin_user_config_dir_in(env.var_os("APPDATA"), None, cfg!(windows))
                .unwrap_or_else(|| home.join(".config").join("devin"));
            push_if_file(
                &mut assets,
                AgentKind::Devin,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Global,
                devin_dir.join("config.json"),
                ".config/devin/config.json",
                "devin/config.json",
            );
            push_if_file(
                &mut assets,
                AgentKind::Devin,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Global,
                devin_dir.join("mcp_config.json"),
                ".config/devin/mcp_config.json",
                "devin/mcp_config.json",
            );
            collect_dir_assets(
                &mut assets,
                AgentKind::Devin,
                AgentAssetKind::Skill,
                AgentAssetScope::Global,
                &devin_dir.join("skills"),
                ".config/devin/skills",
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
            let grok_dir =
                agent_config_home(env.var_os("GROK_HOME")).unwrap_or_else(|| home.join(".grok"));
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
        let kiro_dir =
            agent_config_home(env.var_os("KIRO_HOME")).unwrap_or_else(|| home.join(".kiro"));
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
        let kimi_dir = cwd.join(crate::commands::install_mcp::kimi_code_home(
            std::env::var_os("KIMI_CODE_HOME"),
        )?);
        push_if_file(
            &mut assets,
            AgentKind::KimiCode,
            AgentAssetKind::McpConfig,
            AgentAssetScope::Global,
            kimi_dir.join("config.toml"),
            ".kimi-code/config.toml",
            "kimi-code/config.toml",
        );
        let legacy_dir = home.join(".kimi");
        for (dir, name) in [(&kimi_dir, "kimi-code"), (&legacy_dir, "kimi")] {
            // An override can point at the legacy directory. Archive it once
            // using the portable Kimi Code destination in that case.
            if name == "kimi" && kimi_dir == legacy_dir {
                continue;
            }
            push_if_file(
                &mut assets,
                AgentKind::KimiCode,
                AgentAssetKind::McpConfig,
                AgentAssetScope::Global,
                dir.join("mcp.json"),
                &format!(".{name}/mcp.json"),
                &format!("{name}/mcp.json"),
            );
            collect_dir_assets(
                &mut assets,
                AgentKind::KimiCode,
                AgentAssetKind::Skill,
                AgentAssetScope::Global,
                &dir.join("skills"),
                &format!(".{name}/skills"),
                &format!("{name}/skills"),
            )?;
        }
    }

    // 12. GitHub Copilot CLI: the same skill roots `install-skills --agent
    // copilot-cli` writes, `$COPILOT_HOME/skills` (default `~/.copilot/skills`)
    // and the repository's `.github/skills`. The shared roots the same page
    // lists (`~/.agents/skills`, `.agents/skills`, `.claude/skills`) are
    // collected above for Codex and Claude Code. See
    // https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/add-skills
    if filter_agent(AgentKind::CopilotCli) {
        if want_global {
            let copilot_dir =
                crate::commands::path_util::agent_config_home(std::env::var_os("COPILOT_HOME"))
                    .unwrap_or_else(|| home.join(COPILOT_SKILL_DIR));
            collect_dir_assets(
                &mut assets,
                AgentKind::CopilotCli,
                AgentAssetKind::Skill,
                AgentAssetScope::Global,
                &copilot_dir.join(SKILLS_DIR),
                ".copilot/skills",
                "copilot-cli/skills",
            )?;
        }

        if want_project {
            collect_dir_assets(
                &mut assets,
                AgentKind::CopilotCli,
                AgentAssetKind::Skill,
                AgentAssetScope::Project,
                &cwd.join(GITHUB_SKILL_DIR).join(SKILLS_DIR),
                ".github/skills",
                "project/copilot-cli/skills",
            )?;
        }
    }

    // 13. VS Code / Copilot
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
            // A plugin's manifest directory is the one dot-directory a
            // restored plugin or marketplace cannot load without.
            if (dir_name_str.starts_with('.')
                && dir_name_str != ".claude-plugin"
                && dir_name_str != ".codex-plugin")
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
                let (sanitized_text, changed) = scrub_mcp_config(&asset.source_path, text, sc)?;
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

/// Redact secrets inside an MCP configuration file while preserving valid
/// TOML, JSON, or JSONC syntax. Falls back to free-text scrubbing when the
/// source file is already malformed on disk or uses an unrecognized extension.
fn scrub_mcp_config(path: &Path, text: &str, sanitizer: &Sanitizer) -> Result<(String, bool)> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("toml") => scrub_toml_config(path, text, sanitizer),
        Some("json") => scrub_json_config(path, text, sanitizer),
        Some("jsonc") => scrub_jsonc_config(path, text, sanitizer),
        _ => Ok(scrub_plain_text(text, sanitizer)),
    }
}

fn scrub_plain_text(text: &str, sanitizer: &Sanitizer) -> (String, bool) {
    let scrubbed = sanitizer.scrub(text);
    let changed = scrubbed != text;
    (scrubbed, changed)
}

fn scrub_string_value(key: Option<&str>, value: &str, sanitizer: &Sanitizer) -> Option<String> {
    let direct = sanitizer.scrub(value);
    if direct != value {
        return Some(direct);
    }

    let key = key?;
    if value.trim().is_empty() {
        return None;
    }

    let quoted = serde_json::to_string(value).ok()?;
    let probes = [
        format!("{key} = {quoted}"),
        format!("{key}: {quoted}"),
        format!("{key}={value}"),
    ];
    for probe in &probes {
        let scrubbed = sanitizer.scrub(probe);
        if scrubbed != *probe {
            let marker =
                extract_redaction_marker(&scrubbed).unwrap_or_else(|| "[REDACTED]".to_owned());
            return (marker != value).then_some(marker);
        }
    }
    None
}

fn extract_redaction_marker(scrubbed: &str) -> Option<String> {
    let start = scrubbed.find("[REDACTED:")?;
    let rest = &scrubbed[start..];
    let end = rest.find(']')?;
    Some(rest[..=end].to_owned())
}

fn scrub_toml_config(path: &Path, text: &str, sanitizer: &Sanitizer) -> Result<(String, bool)> {
    let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else {
        return Ok(scrub_plain_text(text, sanitizer));
    };

    let mut changed = scrub_toml_table(doc.as_table_mut(), sanitizer);
    if let Some(raw) = doc.trailing().as_str()
        && let Some(scrubbed) = scrub_toml_comments(raw, sanitizer)
    {
        doc.set_trailing(scrubbed);
        changed = true;
    }

    if !changed {
        return Ok((text.to_owned(), false));
    }

    let rendered = doc.to_string();
    rendered
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("verifying redacted config for {}", path.display()))?;
    Ok((rendered, true))
}

fn scrub_toml_table(table: &mut toml_edit::Table, sanitizer: &Sanitizer) -> bool {
    let mut changed = scrub_toml_decor(table.decor_mut(), sanitizer);
    for (mut key, item) in table.iter_mut() {
        changed |= scrub_toml_decor(key.leaf_decor_mut(), sanitizer);
        changed |= scrub_toml_decor(key.dotted_decor_mut(), sanitizer);
        let key_name = key.get().to_owned();
        match item {
            toml_edit::Item::Value(val) => {
                changed |= scrub_toml_value(Some(&key_name), val, sanitizer);
            }
            toml_edit::Item::Table(sub) => {
                changed |= scrub_toml_table(sub, sanitizer);
            }
            toml_edit::Item::ArrayOfTables(aot) => {
                for sub in aot.iter_mut() {
                    changed |= scrub_toml_table(sub, sanitizer);
                }
            }
            toml_edit::Item::None => {}
        }
    }
    changed
}

fn scrub_toml_value(key: Option<&str>, val: &mut toml_edit::Value, sanitizer: &Sanitizer) -> bool {
    let mut changed = scrub_toml_decor(val.decor_mut(), sanitizer);
    match val {
        toml_edit::Value::String(formatted) => {
            if let Some(redacted) = scrub_string_value(key, formatted.value(), sanitizer) {
                let decor = formatted.decor().clone();
                *formatted = toml_edit::Formatted::new(redacted);
                *formatted.decor_mut() = decor;
                changed = true;
            }
        }
        toml_edit::Value::Array(arr) => {
            if let Some(raw) = arr.trailing().as_str()
                && let Some(scrubbed) = scrub_toml_comments(raw, sanitizer)
            {
                arr.set_trailing(scrubbed);
                changed = true;
            }
            for elem in arr.iter_mut() {
                changed |= scrub_toml_value(None, elem, sanitizer);
            }
        }
        toml_edit::Value::InlineTable(inline) => {
            if let Some(raw) = inline.preamble().as_str()
                && let Some(scrubbed) = scrub_toml_comments(raw, sanitizer)
            {
                inline.set_preamble(scrubbed);
                changed = true;
            }
            for (mut k, v) in inline.iter_mut() {
                changed |= scrub_toml_decor(k.leaf_decor_mut(), sanitizer);
                changed |= scrub_toml_decor(k.dotted_decor_mut(), sanitizer);
                let key_name = k.get().to_owned();
                changed |= scrub_toml_value(Some(&key_name), v, sanitizer);
            }
        }
        _ => {}
    }
    changed
}

fn scrub_toml_decor(decor: &mut toml_edit::Decor, sanitizer: &Sanitizer) -> bool {
    let mut changed = false;
    if let Some(prefix) = decor.prefix().and_then(toml_edit::RawString::as_str)
        && let Some(scrubbed) = scrub_toml_comments(prefix, sanitizer)
    {
        decor.set_prefix(scrubbed);
        changed = true;
    }
    if let Some(suffix) = decor.suffix().and_then(toml_edit::RawString::as_str)
        && let Some(scrubbed) = scrub_toml_comments(suffix, sanitizer)
    {
        decor.set_suffix(scrubbed);
        changed = true;
    }
    changed
}

fn scrub_toml_comments(raw: &str, sanitizer: &Sanitizer) -> Option<String> {
    if !raw.contains('#') {
        return None;
    }
    let mut out = String::with_capacity(raw.len());
    let mut changed = false;
    for line in raw.split_inclusive('\n') {
        if let Some(hash_idx) = line.find('#') {
            let prefix = &line[..=hash_idx];
            let rest = &line[hash_idx + 1..];
            let (body, ending) = if let Some(stripped) = rest.strip_suffix("\r\n") {
                (stripped, "\r\n")
            } else if let Some(stripped) = rest.strip_suffix('\n') {
                (stripped, "\n")
            } else {
                (rest, "")
            };
            let scrubbed = sanitizer.scrub(body).replace(['\r', '\n'], " ");
            if scrubbed != body {
                changed = true;
            }
            out.push_str(prefix);
            out.push_str(&scrubbed);
            out.push_str(ending);
        } else {
            out.push_str(line);
        }
    }
    changed.then_some(out)
}

fn scrub_json_config(path: &Path, text: &str, sanitizer: &Sanitizer) -> Result<(String, bool)> {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(text) else {
        return scrub_jsonc_config(path, text, sanitizer);
    };
    if !scrub_json_value(None, &mut value, sanitizer) {
        return Ok((text.to_owned(), false));
    }
    let mut rendered = serde_json::to_string_pretty(&value)
        .with_context(|| format!("serializing redacted config for {}", path.display()))?;
    if text.ends_with('\n') {
        rendered.push('\n');
    }
    serde_json::from_str::<serde_json::Value>(&rendered)
        .with_context(|| format!("verifying redacted config for {}", path.display()))?;
    Ok((rendered, true))
}

fn scrub_json_value(
    key: Option<&str>,
    value: &mut serde_json::Value,
    sanitizer: &Sanitizer,
) -> bool {
    match value {
        serde_json::Value::String(s) => {
            if let Some(redacted) = scrub_string_value(key, s, sanitizer) {
                *s = redacted;
                true
            } else {
                false
            }
        }
        serde_json::Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= scrub_json_value(None, item, sanitizer);
            }
            changed
        }
        serde_json::Value::Object(map) => {
            let mut changed = false;
            for (k, v) in map.iter_mut() {
                changed |= scrub_json_value(Some(k), v, sanitizer);
            }
            changed
        }
        _ => false,
    }
}

fn scrub_jsonc_config(path: &Path, text: &str, sanitizer: &Sanitizer) -> Result<(String, bool)> {
    let collect = jsonc_parser::CollectOptions {
        comments: jsonc_parser::CommentCollectionStrategy::Separate,
        tokens: false,
    };
    let Ok(ast) =
        jsonc_parser::parse_to_ast(text, &collect, &jsonc_parser::ParseOptions::default())
    else {
        return Ok(scrub_plain_text(text, sanitizer));
    };

    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    if let Some(ref root) = ast.value {
        collect_jsonc_value_edits(None, root, sanitizer, &mut edits)?;
    }
    if let Some(comments) = ast.comments {
        for list in comments.values() {
            for comment in list.iter() {
                match comment {
                    jsonc_parser::ast::Comment::Line(line) => {
                        let scrubbed = sanitizer.scrub(line.text).replace(['\r', '\n'], " ");
                        if scrubbed != line.text {
                            edits.push((line.range.start..line.range.end, format!("//{scrubbed}")));
                        }
                    }
                    jsonc_parser::ast::Comment::Block(block) => {
                        let scrubbed = sanitizer.scrub(block.text).replace("*/", "* /");
                        if scrubbed != block.text {
                            edits.push((
                                block.range.start..block.range.end,
                                format!("/*{scrubbed}*/"),
                            ));
                        }
                    }
                }
            }
        }
    }

    if edits.is_empty() {
        return Ok((text.to_owned(), false));
    }

    edits.sort_by(|a, b| {
        b.0.start
            .cmp(&a.0.start)
            .then_with(|| b.0.end.cmp(&a.0.end))
    });
    edits.dedup_by(|a, b| a.0 == b.0);
    let mut rendered = text.to_owned();
    for (range, replacement) in edits {
        rendered.replace_range(range, &replacement);
    }

    jsonc_parser::parse_to_ast(
        &rendered,
        &jsonc_parser::CollectOptions::default(),
        &jsonc_parser::ParseOptions::default(),
    )
    .with_context(|| format!("verifying redacted config for {}", path.display()))?;
    Ok((rendered, true))
}

fn collect_jsonc_value_edits(
    key: Option<&str>,
    value: &jsonc_parser::ast::Value<'_>,
    sanitizer: &Sanitizer,
    edits: &mut Vec<(std::ops::Range<usize>, String)>,
) -> Result<()> {
    match value {
        jsonc_parser::ast::Value::StringLit(lit) => {
            if let Some(redacted) = scrub_string_value(key, &lit.value, sanitizer) {
                let encoded = serde_json::to_string(&redacted)?;
                edits.push((lit.range.start..lit.range.end, encoded));
            }
        }
        jsonc_parser::ast::Value::Array(arr) => {
            for elem in &arr.elements {
                collect_jsonc_value_edits(None, elem, sanitizer, edits)?;
            }
        }
        jsonc_parser::ast::Value::Object(obj) => {
            for prop in &obj.properties {
                collect_jsonc_value_edits(Some(prop.name.as_str()), &prop.value, sanitizer, edits)?;
            }
        }
        _ => {}
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_sanitizer() -> Sanitizer {
        Sanitizer::new(&SanitizeConfig::default()).unwrap()
    }

    #[test]
    fn scrub_toml_handles_inline_tables_arrays_and_trailing_comments() {
        let sc = test_sanitizer();
        let secret = "fixture-only-abcdef00112233445566778899";
        let input = format!(
            "[mcp_servers.demo]\n\
             url = \"http://127.0.0.1:49374/mcp\"\n\
             http_headers = {{ \"X-API-Key\" = \"{secret}\" }}\n\
             args = [\"--token\", \"ghp_FAKEfakeFAKEfakeFAKEfake012345678\"]\n\
             \n\
             [[servers]]\n\
             api_key = \"{secret}\"\n\
             # trailing comment: X-API-Key = \"{secret}\"\n"
        );
        let (scrubbed, changed) = scrub_mcp_config(Path::new("config.toml"), &input, &sc).unwrap();
        assert!(changed);
        assert!(!scrubbed.contains(secret));
        assert!(!scrubbed.contains("ghp_FAKEfakeFAKEfakeFAKEfake012345678"));

        let doc: toml_edit::DocumentMut = scrubbed.parse().expect("must remain valid TOML");
        assert_eq!(
            doc["mcp_servers"]["demo"]["url"].as_str(),
            Some("http://127.0.0.1:49374/mcp")
        );
        assert!(
            doc["mcp_servers"]["demo"]["http_headers"]["X-API-Key"]
                .as_str()
                .unwrap()
                .starts_with("[REDACTED:")
        );
        assert!(
            doc["mcp_servers"]["demo"]["args"][1]
                .as_str()
                .unwrap()
                .starts_with("[REDACTED:")
        );
        assert!(
            doc["servers"][0]["api_key"]
                .as_str()
                .unwrap()
                .starts_with("[REDACTED:")
        );

        // Idempotent on second pass.
        let (second, changed_again) =
            scrub_mcp_config(Path::new("config.toml"), &scrubbed, &sc).unwrap();
        assert!(!changed_again);
        assert_eq!(second, scrubbed);
    }

    #[test]
    fn scrub_clean_configs_are_returned_verbatim() {
        let sc = test_sanitizer();
        let clean_json = r#"{"mcpServers":{"local":{"url":"http://127.0.0.1:49374/mcp"}}}"#;
        let (out, changed) = scrub_mcp_config(Path::new("settings.json"), clean_json, &sc).unwrap();
        assert!(!changed);
        assert_eq!(out, clean_json);

        let clean_toml = "[mcp_servers.local]\nurl = \"http://127.0.0.1:49374/mcp\"\n";
        let (out, changed) = scrub_mcp_config(Path::new("config.toml"), clean_toml, &sc).unwrap();
        assert!(!changed);
        assert_eq!(out, clean_toml);
    }
}
