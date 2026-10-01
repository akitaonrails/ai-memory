//! Domain types for backing up and restoring AI agent assets (skills, MCP configs, plugins, instructions).

use serde::{Deserialize, Serialize};

use crate::ids::AgentKind;

/// Current version of the agent backup archive manifest schema.
pub const AGENT_BACKUP_SCHEMA_VERSION: u32 = 1;

/// Classification of an asset used by an AI agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentAssetKind {
    /// MCP server configuration file (e.g. `settings.json`, `config.toml`, `mcp.json`).
    McpConfig,
    /// Agent skill file (e.g. `SKILL.md`).
    Skill,
    /// Agent plugin or extension asset.
    Plugin,
    /// Canonical project or agent instruction file (e.g. `CLAUDE.md`, `AGENTS.md`, `.cursorrules`).
    Instruction,
}

impl AgentAssetKind {
    /// Human-readable label for CLI reports.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::McpConfig => "MCP config",
            Self::Skill => "Skill",
            Self::Plugin => "Plugin",
            Self::Instruction => "Instruction",
        }
    }
}

/// Scope of the asset: global (user home / system) or project-specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentAssetScope {
    /// Global asset stored in the user profile (e.g. `~/.claude/settings.json`, `~/.agents/skills/`).
    Global,
    /// Project-scoped asset stored in the working repository (e.g. `.claude/skills/`, `CLAUDE.md`).
    Project,
}

/// Metadata describing host environment where the backup was captured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    /// Operating system name (e.g. "macos", "linux", "windows").
    pub os: String,
    /// Architecture (e.g. "aarch64", "x86_64").
    pub arch: String,
    /// Sanitized user home directory representation if recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub home_dir: Option<String>,
}

/// A single backed-up asset entry inside the archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentBackupEntry {
    /// AI agent harness associated with this asset.
    pub agent: AgentKind,
    /// Type of the asset (MCP config, skill, plugin, instruction).
    pub asset_kind: AgentAssetKind,
    /// Asset scope (global vs project).
    pub scope: AgentAssetScope,
    /// Relative path inside the tarball archive (e.g. `agents/claude-code/mcp/settings.json`).
    pub archive_path: String,
    /// Path relative to the destination base (e.g. home_dir for Global, project root for Project).
    pub target_relative: String,
    /// Whether this entry was sanitized to remove secrets/tokens.
    #[serde(default)]
    pub sanitized: bool,
    /// SHA-256 hash of the content for integrity verification.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Portable Unix permission bits, with set-id and sticky bits stripped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

/// Root manifest stored at `manifest.json` in the backup archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentBackupManifest {
    /// Schema version number.
    pub version: u32,
    /// ISO 8601 creation timestamp.
    pub created_at: String,
    /// Information about host where backup was produced.
    pub host: HostInfo,
    /// Whether any entries were sanitized.
    pub sanitized: bool,
    /// List of backed-up asset entries.
    pub entries: Vec<AgentBackupEntry>,
}

impl AgentBackupManifest {
    /// Create a new manifest with default version and given entries.
    #[must_use]
    pub fn new(host: HostInfo, sanitized: bool, entries: Vec<AgentBackupEntry>) -> Self {
        Self {
            version: AGENT_BACKUP_SCHEMA_VERSION,
            created_at: jiff::Timestamp::now().to_string(),
            host,
            sanitized,
            entries,
        }
    }
}
