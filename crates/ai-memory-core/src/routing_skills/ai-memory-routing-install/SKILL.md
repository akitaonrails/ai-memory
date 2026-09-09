---
name: ai-memory-routing-install
description: "Use this skill for any request to install, refresh, repair, inspect, or remove ai-memory's agent-facing routing: managed instruction snippets, Agent Skills, CLAUDE.md/AGENTS.md integration, or local/global skill roots. Trigger by semantic intent rather than exact wording."
---
<!-- ai-memory-managed: routing-skill -->

# ai-memory routing install

Use this skill when the user wants ai-memory's agent-facing routing instructions or managed Agent Skills installed, refreshed, repaired, or removed.

## Tools in this cluster

- `memory_install_self_routing` returns the canonical markered instruction block, marker strings, filename hints, notes, and managed skill payloads for agents that cannot let the MCP server write the host filesystem directly. Pass `compact: true` when managed Agent Skills are installed or when refreshing a file that already uses the compact snippet.

## Managed instruction marker

The always-loaded instruction block is owned by ai-memory only between these markers.

- Start marker: `<!-- ai-memory:start -->`
- End marker: `<!-- ai-memory:end -->`

Refresh must replace the first complete marker-bounded block whose start and end delimiters appear alone on their own lines, preserving unrelated content before and after it. Ignore inline mentions of marker strings inside prose or code; they are not delimiters. If no complete line-delimited block exists, append the canonical block with one blank line of separation. Never edit unrelated instructions while refreshing ai-memory routing.

## Managed skill marker

Every ai-memory-managed `SKILL.md` file contains this ownership marker.

`<!-- ai-memory-managed: routing-skill -->`

Installers and uninstallers should overwrite or remove same-name skill files only when that marker is present, unless the user supplied an explicit force option. If a same-name skill lacks the marker, skip it with an actionable message so user-authored skills are preserved.

## Skill install targets

Managed skills are ordinary Agent Skills. Their relative file path is `<skill>/SKILL.md`, and the installer prepends the selected skill root.

Project-local targets:

- `.claude/skills/<skill>/SKILL.md` for Claude-compatible installs.
- `.agents/skills/<skill>/SKILL.md` for cross-client installs.
- `.grok/skills/<skill>/SKILL.md` for Grok Build CLI installs.

Global targets:

- `~/.claude/skills/<skill>/SKILL.md` for Claude-compatible installs.
- `~/.agents/skills/<skill>/SKILL.md` for cross-client installs.
- `$GROK_HOME/skills/<skill>/SKILL.md` for Grok Build CLI installs (default:
  `~/.grok/skills/<skill>/SKILL.md`).

Use platform-aware path joining. Do not build paths by string concatenation.

## Refresh guidance

For an agent-side refresh, call the install-routing tool (pass `compact: true` if the target file already uses the compact snippet or if managed Agent Skills handle detailed routing). Its returned `target_hints` are authoritative for skill roots: choose the right instruction filename from `agent_filenames`, write the markered block with the agent's file-edit tool, and write each managed skill file below the selected hint using its `relative_path`. Claude Code normally uses `CLAUDE.md` and `.claude/skills`; Codex, OpenCode, Cursor, Gemini CLI, and AGENTS-aware clients normally use `AGENTS.md` and `.agents/skills`; Grok Build CLI uses `AGENTS.md` and `.grok/skills` unless the project says otherwise.

For a CLI refresh, prefer the canonical install command. The snippet and skills must be updated from the same core-owned assets so they do not drift.
