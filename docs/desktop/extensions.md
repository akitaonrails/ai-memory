# Extension and plugin surfaces (research, 2026-10-08)

> Can an ai-memory extension in each app's marketplace do what an installer
> cannot? Sourced from current official docs and, for Codex, its source.

## Claude (one plugin format, three surfaces)

Per <https://claude.com/docs/plugins/platform-support>, the same plugin
folder installs everywhere but each surface loads a different subset:

| Component | Chat | Cowork | Claude Code (CLI, IDE, desktop Code tab) |
| --- | --- | --- | --- |
| Skills, commands | Load (commands as skills) | Load | Load |
| Hooks | **Ignored** | **Load** | Load |
| Remote MCP (fixed `http`/`sse` URL) | Listed under the plugin's Connectors; works once connected | Loads; connect from Connectors | Loads |
| Local MCP (a command, incl. `.mcpb`) | **Ignored** | Loads when the Cowork session runs on your computer | Loads |
| MCP using `${user_config.*}` | Ignored when the URL references it | Ignored without a default; no prompt | Loads; prompts |
| Top-level `bin/` executables | Can't be installed | Can't be installed | Loads |

Chat and Cowork plugins are added from **Customize > Plugins** in claude.ai
or the desktop app (one click, from the directory or an added GitHub/GitLab/
Bitbucket marketplace, or an uploaded zip), are attached to the account, and
reach Claude Code as synced plugins. Claude Code installs per machine
(`/plugin`, `claude plugin install`).

Consequences for ai-memory:

- **Cowork gains a capture path**: a plugin's hooks load there, which
  `~/.claude/settings.json` hooks are not documented to do. Unverified: on
  Linux Cowork runs in a VM, so a hook command or a loopback server URL that
  works on the host may not work inside it.
- **Chat still cannot capture**: hooks and local MCP servers are ignored.
  Only skills and a remote MCP connector load, and a remote connector has to
  be reachable as configured (whether claude.ai brokers it from Anthropic's
  side, which would rule out a loopback server, was not verified here).
- A plugin cannot install the ai-memory binary on Chat/Cowork (`bin/` is
  refused there), so the server still has to be installed separately; hook
  scripts shipped as plugin files (like the POSIX bundle in `hooks/`) are
  not top-level executables.

## Other apps

| App | Extension unit | Can bundle hooks? | Can bundle MCP? | Install path | Source |
| --- | --- | --- | --- | --- | --- |
| Codex (CLI + desktop) | Plugin | Yes | Yes | Plugin marketplace | <https://developers.openai.com/codex/hooks>; plugin hook trust in `codex-rs/app-server/src/effective_plugin_change.rs` |
| Cursor | Plugin | Yes | Yes | Cursor Marketplace / Customize page; `cursor://` MCP install deeplinks | <https://cursor.com/docs/plugins> |
| Antigravity | Plugin | Yes, CLI only per docs | — | Marketplace | <https://antigravity.google/docs/hooks> |
| Zed | Extension | No | Yes (MCP extensions) | Extension registry | <https://zed.dev/docs/extensions/mcp-extensions> |
| ChatGPT (hosted chat) | Plugin | No | Remote only, needs a reachable server | Published directory with review | <https://developers.openai.com/plugins> |

Codex trusts plugin hooks automatically only for plugins a ChatGPT
workspace lists for the signed-in account (`trust_materialized_plugin_hooks`);
a plugin from any other source goes through the normal per-hook trust, so a
plugin does not remove the silent "untrusted hook" failure. Whether Cursor
plugin hooks need approval is not stated in its docs.
