# Backup and Restore of AI Agent Assets (Skills, MCP, Plugins)

## 1. Context and Motivation

`ai-memory` coordinates memory and context across AI coding agents (Claude Code, OpenAI Codex, Antigravity CLI, Gemini CLI, Cursor, OpenCode, Devin, Grok, Kiro, etc.).

Before these commands were added:
- **Server Backup:** `ai-memory backup` takes a snapshot of the server's SQLite database (`memory.sqlite`), git wiki (`wiki/`), and `config.toml` via `POST /admin/backup`.
- **Client Configuration Knowledge:** `ai-memory` installers (`install_mcp.rs`, `install_skills.rs`, `install_hooks.rs`, `uninstall.rs`) already have detailed definitions of where each supported AI harness stores MCP servers, skills, plugins, and project instructions.
- **Gap:** There was no command to snapshot, export, backup, or restore host
  agent configurations (MCP servers, agent skills, plugins, instructions).
  Moving to a new machine required reconfiguring every agent manually.

---

## 2. Target Matrix by Agent Harness

| Harness | MCP Configuration | Skills (Global / Project) | Plugins / Extensions | Instructions / Rules |
| :--- | :--- | :--- | :--- | :--- |
| **Claude Code / Desktop** | `~/.claude.json`<br>`~/.claude/settings.json`<br>`claude_desktop_config.json` | `~/.claude/skills/`<br>`.claude/skills/` | `~/.claude/plugins/`<br>`Claude Extensions/` | `CLAUDE.md` |
| **OpenAI Codex CLI** | `~/.codex/config.toml`<br>`~/.codex/mcp.json` | `~/.agents/skills/`<br>`.agents/skills/`<br>`~/.codex/skills/` | `~/.codex/plugins/` | `AGENTS.md` |
| **Antigravity CLI (`agy`) / Gemini** | `~/.gemini/settings.json`<br>`~/.gemini/config/mcp_config.json`<br>`antigravity-cli/mcp_config.json` | `~/.gemini/antigravity-cli/skills/`<br>`~/.gemini/skills/`<br>`.gemini/skills/` | `antigravity-cli/plugins/`<br>`~/.gemini/plugins/` | `GEMINI.md`<br>`AGENTS.md`<br>`rules/` |
| **Cursor IDE** | `~/.cursor/mcp.json`<br>`.cursor/mcp.json` | — | VS Code / Cursor extensions | `.cursorrules`<br>`.cursor/rules/` |
| **OpenCode (v1 / v2)** | `opencode.json`<br>`opencode.jsonc`<br>`~/.config/opencode/opencode.json` | `~/.config/opencode/skills/` | `~/.config/opencode/plugins/` | `AGENTS.md` |
| **Devin CLI** | `~/.devin/config.json` | `~/.devin/skills/` | — | Devin instructions |
| **Grok Build CLI** | `~/.grok/config.toml` | `~/.grok/skills/` | — | Grok instructions |
| **Kiro CLI (AWS)** | `~/.kiro/settings/mcp.json` | `~/.kiro/agents/*.json` | — | — |
| **OpenClaw** | `~/.openclaw/config.json` | — | `~/.openclaw/extensions/` | — |
| **VS Code Copilot** | `.vscode/mcp.json` | — | Extensions | Copilot instructions |

Claude Desktop config paths use the same platform-aware resolver as
`install-mcp`, including packaged Windows installs. OpenCode uses
`%USERPROFILE%\.config\opencode` on Windows too, as its
[official troubleshooting guide](https://dev.opencode.ai/docs/troubleshooting/)
documents.

---

## 3. Architecture & CLI Design

### Dedicated Host CLI Subcommands: `backup-agents` and `restore-agents`

Host configurations live on the client filesystem, independent of whether the `ai-memory serve` daemon is running. Therefore, backup and restore of agent assets execute as client-side commands.

#### CLI Command Specification

```bash
# Backup all detected agents into an archive
ai-memory backup-agents -o agent-assets.tar.gz

# Filter specific agents or scopes
ai-memory backup-agents --agents claude,codex,antigravity --scope both -o backup.tar.gz

# Include raw MCP-config secrets (warns loudly; every archive is 0600 on Unix)
ai-memory backup-agents -o backup.tar.gz --include-secrets

# Dry-run inspection of an archive before restoring
ai-memory restore-agents -i backup.tar.gz

# Apply the reviewed restoration plan
ai-memory restore-agents -i backup.tar.gz --apply
```

---

## 4. Archive Manifest Schema (`manifest.json`)

Stored in the root of the generated tarball:

```json
{
  "$schema": "https://ai-memory.dev/schemas/agent-backup-v1.json",
  "version": 1,
  "created_at": "2026-09-28T14:30:00Z",
  "host": {
    "os": "macos",
    "arch": "aarch64",
    "home_dir": null
  },
  "sanitized": true,
  "entries": [
    {
      "agent": "claude-code",
      "asset_kind": "mcp-config",
      "scope": "global",
      "archive_path": "agents/claude-code/settings.json",
      "target_relative": ".claude/settings.json"
    },
    {
      "agent": "claude-code",
      "asset_kind": "skill",
      "scope": "global",
      "archive_path": "agents/claude-code/skills/custom-skill/SKILL.md",
      "target_relative": ".claude/skills/custom-skill/SKILL.md"
    },
    {
      "agent": "codex",
      "asset_kind": "mcp-config",
      "scope": "global",
      "archive_path": "agents/codex/config.toml",
      "target_relative": ".codex/config.toml"
    }
  ]
}
```

---

## 5. Security & Invariants

1. **Secret Redaction:**
   - By default, known tokens, bearer headers, and environment keys (`*_TOKEN`, `*_KEY`, `*_SECRET`) in UTF-8 MCP config files are sanitized using `ai-memory-hooks::sanitizer`.
   - Skills, plugins, and instruction files are copied verbatim and may contain secrets.
   - Passing `--include-secrets` bypasses MCP redaction. Every archive is set
     to mode `0600` on Unix because verbatim instructions and plugins may also
     contain credentials. Other platforms rely on their native ACLs.
2. **Symlink Safety:**
   - Archives do not traverse or include external symlinks (`follow_symlinks(false)`).
   - Restore rejects a symlink at the target or in any archive-controlled
     descendant below the approved home/project/config root.
3. **Path Traversal Protection:**
   - Restoring rejects traversal and permits only allowlisted agent destinations. Claude Desktop uses the same platform-specific path resolver as `install-mcp`.
   - Archive entries are regular UTF-8 paths only, and entry count, manifest,
     per-file, and total expanded sizes are bounded before content is staged.
   - Every manifest entry must have exactly one body and a well-formed SHA-256;
     duplicate paths/destinations, missing bodies, unmanifested bodies, and
     checksum mismatches are refused. Checksums detect corruption or internal
     inconsistency; they do not authenticate an archive's author.
4. **Atomic Restores:**
   - The output archive is completed and synced in a private sibling tempfile
     before it replaces the requested destination.
   - Target files use temporary file + rename + sync
     (`ai_memory_wiki::write_atomic`). If a later write fails, earlier writes
     are rolled back; timestamped copies of overwritten files remain available.
   - Non-managed custom files are never overwritten without explicit `--force`.
   - Portable Unix execute/read/write bits are restored; set-id and sticky bits
     are never accepted from a manifest.
5. **Active Content:**
   - Skills, plugins, and instruction files can contain executable code or
     model-visible instructions. Restore is dry-run by default and prints this
     warning; only use `--apply` for an archive whose source and listed paths
     you trust. SHA-256 consistency is not a signature.

---

## 6. Implementation Map

1. **`crates/ai-memory-core`**:
   - Add `agent_backup` module containing `AgentAssetKind`, `AgentAssetScope`, `AgentBackupEntry`, and `AgentBackupManifest`.
2. **`crates/ai-memory-cli`**:
   - `src/cli.rs`: Register `BackupAgents(BackupAgentsArgs)` and `RestoreAgents(RestoreAgentsArgs)`.
   - `src/commands/backup_agents.rs`: Scan host configs, package archive.
   - `src/commands/restore_agents.rs`: Validate manifest, present diffs, write files.
3. **Tests**:
   - Roundtrip tests: archive -> verify manifest -> extract into temporary directory -> assert equality.
   - Sanitization tests: ensure keys are redacted unless requested otherwise.
   - Security tests: reject path traversal payloads (`../../etc/passwd`).
