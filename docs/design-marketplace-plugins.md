# Design proposal: ai-memory as marketplace plugins (#1166)

**Status: implemented for 3.0 (Claude Code and Codex plugins), 2026-10-13.
Cursor, Cowork capture, Antigravity and Gemini remain later work.** The
sections below keep the research and the reasoning; where the shipped code
differs from the original proposal, the text says so and
[Implementation notes](#implementation-notes-30) lists every change. The
generated files under `plugins/` and the renderer
(`crates/ai-memory-cli/src/commands/render_plugins.rs`) are authoritative over
the manifest listings here.
Builds on RFC #1166, the feedback comments on it, and the
[`docs/desktop/`](desktop/) notes (especially
[`verification-2026-10.md`](desktop/verification-2026-10.md) and
[`extensions.md`](desktop/extensions.md)).

## Summary

Ship ai-memory as a plugin in each coding app's plugin system, starting with
Claude Code. The plugin stays thin. It holds manifests, the routing skills,
an MCP entry that runs `ai-memory mcp-bridge`, and one small POSIX launcher
that hands every lifecycle event to the installed binary's native
`ai-memory hook` command. The server and the binary are still installed
separately. The plugin never captures through the curl script bundle, so
capture policy (`ignore_paths`, allowlist mode, server profiles) stays
enforced client-side exactly as it is for `install-hooks` today. When the
binary is missing, the plugin captures nothing, prints one line of guidance at
session start, and exits 0.

The marketplace lives in this repository. Catalog files sit at the repository
root and pin each plugin to the release tag, so `/plugin marketplace add
akitaonrails/ai-memory` only ever installs released plugin content.

Recommended 3.0 scope: the Claude Code plugin (CLI and the desktop Code tab),
its marketplace, CI validation, a sandboxed end-to-end test, and the
`setup`/`doctor`/`upgrade` integration. Codex follows in 3.0 if the trust
question below resolves cleanly. Cursor, Cowork capture, Antigravity and
Gemini come later.

## How this was checked

Two kinds of evidence, and the doc says which one backs each claim:

- **Docs.** Official documentation fetched on 2026-10-13. URLs are listed in
  [Sources](#sources).
- **Local runs.** Experiments on 2026-10-13 with Claude Code 2.1.296, Codex
  CLI 0.162.1, Gemini CLI 0.63.0 and `cursor-agent` 2026.10.01, each in a
  scratch config directory (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`). The
  operator's own agent configuration was not touched. A prototype Claude
  plugin with the manifests from this doc was validated, installed into the
  sandbox, and driven with `claude -p`.

What the local runs showed:

1. `claude plugin validate --strict <dir>` checks more than JSON syntax. It
   failed on an unknown hook event (`hooks.PostToolUze: unknown hook event;
   entry ignored at runtime`), on an unknown manifest key, and on a
   marketplace without `description`. Exit codes are usable in CI.
2. A sandboxed install works offline: `claude plugin marketplace add <path>`,
   `claude plugin install ai-memory@ai-memory`, `claude plugin details`.
3. **Plugin hooks fire in `claude -p` without any login.** With no
   credentials, `claude -p "say ok"` printed `Not logged in` but still ran
   the plugin's `SessionStart`, `UserPromptSubmit` and `SessionEnd` hooks.
   Through the prototype launcher and the 2.6.6 native binary, all three
   events landed in a scratch spool (`SessionStart` 38-63 ms including a
   failed handoff fetch, `UserPromptSubmit` 8-12 ms). The final run used the
   exact Claude manifests in this doc, loaded with `--plugin-dir`. CI can
   therefore test real hook wiring without an API key. Tool events need a
   model turn and cannot be driven this way.
4. **Shell-form hooks reject `${user_config.*}`.** Claude Code refused the
   hook with "references ${user_config.*} in a shell-form command. … Use exec
   form instead … or read $CLAUDE_PLUGIN_OPTION_<KEY> from the hook's
   environment."
5. **Option defaults are not applied in hooks.** With an option left unset,
   an exec-form hook referencing it failed ("Plugin option "server_url" isn't
   set"), and `CLAUDE_PLUGIN_OPTION_SERVER_URL` was absent from the hook
   environment. The same unset option *was* substituted with its default in
   the MCP server's `args`. Hooks must therefore never depend on an option
   being set; the launcher supplies its own defaults.
6. `CLAUDE_PLUGIN_OPTION_<KEY>` is exported to hook processes (shell and exec
   form) once the user sets the option. It is not exported to MCP servers.
7. The plugin's stdio MCP server receives `CLAUDE_CODE_SESSION_ID`, so the
   session-aware `ai-memory mcp-bridge` works unchanged. The server is named
   `plugin:ai-memory:ai-memory` and its tools are
   `mcp__plugin_ai-memory_ai-memory__<tool>`. Nothing in the codebase matches
   on the `mcp__ai-memory__` prefix, so the rename is harmless.
8. An exec-form hook whose binary is missing fails with a non-blocking
   "Executable not found in $PATH" error on every event.
9. **The Docker host wrapper runs `ai-memory hook` in a container.**
   `bin/ai-memory` sends `mcp-bridge`, `install-hooks` and a few other
   commands to the native host client, but not `hook`. A launcher that found
   the wrapper on `PATH` paid about 280 ms per event (a `docker run`), and
   inside the container `127.0.0.1` is the container. The native client
   took about 5 ms per event. The wrapper has to route `hook` to the native
   client before a plugin can rely on `PATH` (Phase 1, item 2).
10. Codex 0.162.1 accepted a plugin in the portable layout (root
    `plugin.json`, `mcp.json`, `hooks/hooks.json`) from a local
    `.agents/plugins/marketplace.json`: `codex plugin marketplace add` and
    `codex plugin add ai-memory@ai-memory` both succeeded in a scratch
    `CODEX_HOME`. **Corrected in Phase 0:** install succeeding was not the
    same as loading. With an Agent Plugins root `plugin.json` (`$schema`
    set), `plugin/read` over `codex app-server` reported the skills and the
    MCP server but **no hooks**, whether hooks were discovered by default or
    named with `hooks` or `extensions."com.openai".hooks`. With Codex's own
    `.codex-plugin/plugin.json` all seven hooks, the seven skills and (once
    the manifest says `"mcpServers": "./mcp.json"`) the MCP server load. The
    shipped Codex plugin uses that layout.

Phase 0 runs (2026-10-13, Claude Code 2.1.296 and Codex 0.162.1, scratch
`HOME`/`CLAUDE_CONFIG_DIR`/`CODEX_HOME`, the operator's configuration
untouched):

11. **Claude option storage.** `claude plugin install` records the plugin in
    `<config dir>/plugins/installed_plugins.json` (`version: 2`,
    `plugins."ai-memory@ai-memory"[]` with `scope`, `installPath`,
    `version`) and enables it in the user `settings.json` `enabledPlugins`.
    `claude plugin configure --values-stdin` stores option values under
    `pluginConfigs."ai-memory@ai-memory".options` in the same file, and an
    empty string clears an option.
12. **An option whose default is `""` is substituted as an empty argument**
    in the MCP `args` (`mcp-bridge --server-url ""`), while the hook
    environment still has no `CLAUDE_PLUGIN_OPTION_SERVER_URL`. Hooks get
    `CLAUDE_PROJECT_DIR`.
13. **Codex trust survives an upgrade.** Codex keys plugin hook trust as
    `ai-memory@ai-memory:hooks/hooks.json:<event>:<group>:<handler>` and
    hashes the command *before* `${PLUGIN_ROOT}` expansion: the same plugin
    installed into two `CODEX_HOME`s at different paths reported identical
    `currentHash` values, and a trusted hook stayed `trusted` after a
    version bump and reinstall, both from a local marketplace and through a
    `git-subdir` source with a moved `ref`. The Codex smoke test also
    reinstalls into a new `cache/ai-memory/ai-memory/<version>` directory,
    and every hook stays trusted. Editing `scripts/ai-memory-hook` itself
    does not change the hash.
14. **Codex plugin hooks fire without credentials.** Once trusted, a
    `thread/start` plus `turn/start` over `codex app-server` ran the plugin's
    `SessionStart` and `UserPromptSubmit` hooks through the launcher into the
    real binary, which spooled both as agent `codex` in the fixture marker's
    scope. With the model provider pointed at a closed loopback port, no
    request left the machine. `commandWindows` in the hooks file is accepted
    (no `hooks/list` error); it was not run.

## Per-app capability table

Docs fetched 2026-10-13 unless marked otherwise. "Local" means confirmed in
the runs above.

| App | Hooks in a plugin | MCP in a plugin | Skills | Config and secrets | Updates | Distribution | Review / submission |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **Claude Code** (CLI, desktop Code tab) | Yes, `hooks/hooks.json`; merged with user/project hooks; no per-hook approval prompt (install warning and workspace-trust rules only) | Yes, `.mcp.json` or `mcpServers`; stdio/http/sse/ws | Yes, `skills/` (namespaced `/ai-memory:<skill>`) | `userConfig`; `sensitive: true` goes to the OS credential store; `${user_config.KEY}` in MCP config and exec-form hook args; `CLAUDE_PLUGIN_OPTION_<KEY>` in hook env (local: only when set) | `version` pins users; third-party marketplaces have background auto-update **off** by default; `claude plugin update` | Own marketplace (`/plugin marketplace add owner/repo`), directory portal (`<name>@synced`), community marketplace | Directory portal needs a paid plan, README, license, no credentials in files; `claude-plugins-official` takes no portal submissions |
| **Claude Cowork** | Yes (loads) | Local stdio only "when the Cowork session runs on your computer"; remote listed as a connector | Yes | `${user_config.*}` in MCP "Ignored when a referenced option has no default; Cowork doesn't prompt" | Account-synced | Customize > Plugins, zip upload | Same as above; top-level `bin/` makes the plugin uninstallable |
| **Claude Chat tab** | Ignored | Remote only | Yes | Remote URL with `${user_config.*}` ignored | Account-synced | Customize > Plugins | Same |
| **Codex** (CLI + desktop) | Yes, `hooks/hooks.json` or `extensions."com.openai".hooks`; per-hook hash trust: "Installing or enabling a plugin doesn't automatically trust its hooks" | Yes, portable `mcp.json` (`type` required) or legacy `.mcp.json` | Yes | MCP auth types only; no documented per-plugin user settings | `codex plugin marketplace upgrade`; no documented auto-update | `.agents/plugins/marketplace.json` in a repo or `~/.agents/plugins/`; legacy `.claude-plugin/marketplace.json` | Public directory: "Plugins containing lifecycle hooks aren't eligible"; MCP must be public HTTPS |
| **Cursor** | Only in the Cursor Plugin format (`.cursor-plugin/plugin.json` + `hooks/hooks.json`); an Agent Plugin (root `plugin.json`) carries skills and MCP only | Yes, `mcp.json`; expands `${CURSOR_PLUGIN_ROOT}` and `${CLAUDE_PLUGIN_ROOT}` (not `${PLUGIN_ROOT}`) in MCP fields | Yes | `variables` schema, values entered in the dashboard, used as `${VAR}` in `mcp.json` | GitHub import auto-refresh (≤10 min) or manual Refresh | cursor.com/marketplace, team marketplaces, `.cursor-plugin/marketplace.json` at a repo root | "Every plugin is manually reviewed before it's listed"; no documented approval step for plugin hooks |
| **Gemini CLI** | Yes, `hooks/hooks.json` in the extension (timeouts in ms) | Yes, `gemini-extension.json` `mcpServers` | Yes | `settings[]` with `envVar`; `sensitive: true` uses the system keychain | Git HEAD or release tag; `gemini extensions update` | `gemini extensions install <git url>`; gallery crawls repos with topic `gemini-cli-extension` and a root manifest | Automated validation. Note: "Gemini CLI was replaced by Antigravity CLI on June 18th, 2026" for unpaid and Google One users |
| **Antigravity** | Yes, root `hooks.json`, documented for the CLI only; no SessionEnd or prompt event | Yes, `mcp_config.json` (`serverUrl`, not `url`) | Yes | Not documented | "automatically updated" (2.0) | `agy plugin install <name>@antigravity-plugins-official`, GitHub URL, local dir | Not documented; no plugin-root variable documented |
| **Copilot CLI** | Yes | Yes | Yes | Not documented | `copilot plugin update` | marketplace repos; also reads `.claude-plugin/plugin.json` and `.claude-plugin/marketplace.json` as fallbacks | Not documented |
| **Factory Droid** | Yes, Claude-style `hooks/hooks.json`, `${DROID_PLUGIN_ROOT}` or `${CLAUDE_PLUGIN_ROOT}` | Yes, `mcp.json` (no secrets) | Yes | Not documented | `droid plugin marketplace update` | Reads `.factory-plugin/marketplace.json`, falls back to `.claude-plugin/marketplace.json` | Not documented |
| **OpenCode** | Code-level plugins (JS/TS events), not a hooks file | From config, not the plugin | — | Config | npm via Bun at startup | npm or local | None. ai-memory already generates this plugin (`install-hooks --agent opencode`) |
| **Kimi Code** | No (plugins are tool bundles) | No | — | `config_file`/`inject` | Reinstall | Local, zip, git | None |
| **Zed** | No | MCP extensions, "We plan to deprecate MCP server extensions in favor of the official MCP registry" | — | — | Registry | Extension registry | Registry PR |

Corrections to the table in RFC #1166:

- **Codex.** Plugins with hooks cannot go in the public directory, so a
  capturing Codex plugin ships through our own marketplace file. Installing
  it still leaves every hook untrusted until the user approves it in `/hooks`.
- **Cursor.** Hooks require the `.cursor-plugin/` format. An Agent Plugin, or a
  `cursor://` MCP deeplink, adds no hooks and so does not trigger the #721
  de-duplication.
- **Antigravity** can bundle MCP (`mcp_config.json`).
- **Gemini CLI** has been replaced by Antigravity CLI for most free users, so
  it ranks low.
- **Claude.** The anthropic-official listing is out of reach in practice.
  Besides having no submission route, its review prompt
  (`anthropics/claude-plugins-official/.github/policy/prompt.md`, read
  2026-10-13) fails any plugin whose `UserPromptSubmit`, `PreToolUse` or
  `PostToolUse` hook "runs without a project-relevance gate". Capture hooks
  are ungated by design. The self-hosted marketplace is the channel; the
  directory portal is optional (see open questions).

## Design

### 1. What the plugin is, and what it is not

- **Manifests, skills, one launcher script.** No Node or Python, no
  compiled binary, no first-run download. This keeps the Rust-only core rule,
  passes the Claude directory checklist's "binary that isn't an image or
  font" hold, and avoids the patterns that made other memory plugins fragile
  (claude-mem's Bun bootstrap, cognee's 120 s blocking venv install,
  hindsight's unpinned `uvx …@latest`).
- **No top-level `bin/`.** Claude refuses to install such a plugin in Chat and
  Cowork. The launcher lives in `scripts/`.
- **The binary is installed once, separately** (install.sh, Homebrew, AUR,
  RPM, Docker wrapper, the macOS app). The plugin assumes it and says so
  plainly when it is missing.
- **The native hook is the only capture path.** The plugin never ships or
  calls `hooks/<agent>/*.sh`. Those scripts POST with curl and sit outside
  capture-policy v1 (row 11h in `security-boundaries.md`). If the plugin
  fell back to them when the binary is missing, `ignore_paths` and allowlist
  mode would be offered and silently not enforced. Without the binary the
  plugin captures nothing.

### 2. The launcher

One POSIX `sh` script, `scripts/ai-memory-hook`, shared by every plugin and
called as `ai-memory-hook <agent> <event>`. It:

1. picks the server URL: the plugin option (`CLAUDE_PLUGIN_OPTION_SERVER_URL`
   on Claude) when set, else an empty value, which the binary reads as "use
   the `server_url` of my config" (section 3). It cannot rely on
   `userConfig` defaults, which hooks do not receive (local finding 5).
   *Shipped differently from the proposal,* which fell back to
   `AI_MEMORY_HOOK_URL` and then to loopback: that would have sent a user
   whose config names a remote server to `127.0.0.1` unless they also set
   the plugin option;
2. finds the binary: the `ai_memory_bin` option if set, else `command -v
   ai-memory`, else the install locations a GUI-launched app may not have on
   `PATH` (`$HOME/.local/bin`, `$HOME/.cargo/bin`, `/opt/homebrew/bin`,
   `/usr/local/bin`, `/usr/bin`);
3. if nothing is found: reads and discards stdin, prints
   `{"systemMessage": "ai-memory: the ai-memory binary was not found, so this
   session is not captured. Install it and run ai-memory setup: <url>"}` on
   `session-start` and `{}` on every other event, and exits 0. This is the
   pattern Anthropic's own `claude-security` plugin uses for a missing
   Python;
4. otherwise runs `exec "$bin" hook --event <event> --agent <agent>
   --server-url <url> [--data-dir <dir>]` with `AI_MEMORY_HOOK_ORIGIN=<agent>-plugin`
   in the environment. The subcommand comes first so the Docker wrapper can
   intercept it.

The prototype took about 2 ms per call on the missing-binary path. The script
is short enough to read in full, which matters because plugin hooks run with
the user's full permissions.

Why a script and not exec form straight to `ai-memory`:

- exec form gives a raw "Executable not found" error on every tool call when
  the binary is missing (local finding 8);
- it cannot reach binaries that are off a GUI app's `PATH`;
- exec form with `${user_config.server_url}` fails outright when the option
  is unset (finding 5).

The script's cost is Windows. Claude Code runs shell-form hooks through Git
Bash on Windows, so the launcher works there when Git Bash is installed, and
fails without it (Claude then falls back to PowerShell). Codex has a
per-hook `commandWindows` override that can call `ai-memory.exe` directly.
See open question 4.

### 3. Changes to the binary (small, all in existing modules)

- **`hook`: origin-aware de-duplication.** Claude runs a plugin's hook and an
  identical settings-file hook separately ("A plugin's or skill's copy of the
  same handler stays separate"). A user with `install-hooks` already applied
  who adds the plugin would be captured twice. When
  `AI_MEMORY_HOOK_ORIGIN=claude-plugin` (read through the injectable lookup,
  invariant 1) and the Claude settings files already carry ai-memory hooks
  (`settings_file_carries_ai_memory_hooks` on the user file, honouring
  `CLAUDE_CONFIG_DIR`, and the repository's `.claude/settings.local.json`),
  the plugin copy is dropped and prints `{}`. This mirrors
  `is_redundant_cursor_copy` (#721). Settings hooks win because they carry
  flags the plugin does not (`--capture-assistant`, a baked
  `--project-strategy`). An env variable rather than a flag keeps an older
  binary working with a newer plugin; it just skips the de-duplication.
  *Shipped* as `AI_MEMORY_HOOK_ORIGIN=<agent>-plugin` (`claude-code-plugin`,
  `codex-plugin`), checked per event: the plugin copy is dropped only when a
  settings file carries an ai-memory hook for *that* event, so an older
  install wiring fewer events still gets the rest from the plugin. The
  checkout is `CLAUDE_PROJECT_DIR` when Claude sets it, else the payload
  `cwd`. Codex plugin events are never de-duplicated: Codex skips an
  untrusted settings hook without a trace, and nothing at hook time can tell
  a trusted one from a skipped one, so dropping the plugin copy could lose
  the event. `setup` and `doctor` handle a double Codex install instead.
  The same origin check skips a **folderless session** (decision 8): no
  `cwd`, or a cwd inside a Claude desktop `…/Claude/scratch-workspaces/…`
  directory, matched lexically.
- **`hook` and `mcp-bridge`: an empty `--server-url` means unset.** This lets
  `.mcp.json` pass `${user_config.server_url}` while a user who has a
  configured `server_url` keeps it.
- **Docker wrapper: route `hook` to the native host client**, as it already
  does for `mcp-bridge` (local finding 9). Without this, a Docker-wrapper
  user's plugin hooks run in a container, over budget and pointed at the
  wrong loopback.
- **`setup`, `install-hooks`, `install-skills`:** detect an enabled
  ai-memory Claude plugin (`installed_plugins.json` plus `enabledPlugins` in
  the user settings) and stop writing the parallel Claude settings hooks,
  skills and MCP entry, printing what was skipped. *Shipped:* `setup` and the
  `ai-memory run` auto-wire skip an agent (Claude Code or Codex) whose plugin
  is enabled. The explicit `install-hooks`, `install-mcp` and
  `install-skills` still write when asked; they are the override, so no
  `--force` flag was added (`install-skills --force` already means something
  else). `setup` has no `--server-url`, and none is needed: the plugin's
  `server_url` option defaults to empty, so hooks and MCP both follow the
  server in the ai-memory config, and nothing has to be copied into Claude's
  plugin options.
- **`doctor`:** report the plugin (version, enabled, options), version skew
  against the binary, duplicate wiring (plugin plus settings hooks, plugin
  skills plus installed skills), and a Docker wrapper on `PATH` without the
  routing fix. For Codex, extend `codex_hook_trust.rs` to plugin hooks.
  *Shipped:* plugin, version, enabled state, skew, settings hooks, installed
  skills, a second MCP entry, `disableAllHooks`/`allowManagedHooksOnly`, and
  Codex plugin hooks Codex skips (matched by `pluginId`). Plugin options are
  not reported, and neither is the Docker wrapper: the wrapper fix that
  routes `hook` to the native client lands separately on `main`, and a
  wrapper-run `doctor` executes inside the container, where it cannot see
  the host `PATH`.
- **`upgrade`:** when the plugin is installed, run `claude plugin
  marketplace update ai-memory` and `claude plugin update
  ai-memory@ai-memory` after replacing the binary, because background
  auto-update is off for third-party marketplaces. This keeps binary and
  plugin in step without asking users to opt into auto-update. *Shipped,*
  also when the binary is already current. For Codex, `upgrade` prints
  `codex plugin marketplace upgrade ai-memory` and `codex plugin add
  ai-memory@ai-memory` instead of running them. A package-managed binary
  (Homebrew, AUR, RPM) never reaches this path, so those users run the
  plugin update themselves (documented in `docs/install.md`).
- **`uninstall`:** offer `claude plugin uninstall ai-memory@ai-memory`.

None of this touched the MCP tool surface, so that implementation kept the
then-current count at 23.

### 4. Configuration surface

Zero required settings. The defaults reproduce a local `ai-memory setup`
install.

| Knob | Claude plugin | Default | Notes |
| --- | --- | --- | --- |
| Server URL | `userConfig.server_url` (string) | empty (shipped; proposed `http://127.0.0.1:49374`) | Read by the launcher from `CLAUDE_PLUGIN_OPTION_SERVER_URL`; MCP gets `${user_config.server_url}`. Empty means the `server_url` of the ai-memory config, for hooks and MCP alike |
| Binary path | `userConfig.ai_memory_bin` (string) | `ai-memory` | For installs off `PATH` |
| Data dir | `userConfig.data_dir` (string) | empty | Only for a non-default data dir |
| Token | **none in the plugin** | — | Hooks read `<data_dir>/auth-token`, the OIDC `auth.json` or a server profile; `mcp-bridge` uses configured auth, then the same stored hook key, then loopback-only discovered root auth. The discovered root key never leaves loopback. Same files `setup`, `install-hooks --auth-token` and `auth login` already write |
| Project override, capture mode, `ignore_paths`, briefing, server routing | `.ai-memory.toml` marker, persisted capture mode, `servers.toml` profiles | as today | The native hook already enforces all of these. Nothing is duplicated into plugin settings |

Keeping the token out of the plugin keeps one credential store for hooks, MCP
and the CLI. It also avoids a pattern Anthropic's directory scan flags: an
Arroway source comment records an earlier build being flagged for reading a
token from the environment. A `sensitive` token option is open question 6.

Cowork: an unset option in an MCP entry loads there only if the option has a
default, and Cowork never prompts. Every option above has a default.

### 5. Trust and security

- **Claude.** No per-hook approval for plugin hooks; installing the plugin is
  the trust decision, plus workspace trust in interactive sessions. Plugin
  hooks run outside Claude's sandbox with full user permissions. The README
  and the marketplace description must say exactly what runs and what is
  sent: lifecycle payloads, sanitized by the server, to the configured
  ai-memory server only; no telemetry. `disableAllHooks` disables plugin
  hooks too, and `allowManagedHooksOnly` blocks them unless the plugin is
  force-enabled in managed settings. `doctor` reports both.
- **Codex.** Every plugin hook stays untrusted until approved in `/hooks`,
  and a changed handler is skipped silently. Install docs must say this, and
  `doctor` must detect untrusted or modified plugin hooks. Hook commands are
  kept byte-identical across releases (they reference `${PLUGIN_ROOT}`, never
  a versioned path), so trust can survive upgrades. Whether Codex hashes the
  handler before or after variable expansion is not verified (open question
  2).
- **No secrets in plugin files.** CI greps the plugin tree for bearer-shaped
  strings, and `claude plugin validate` already warns on credential-looking
  header values and on non-loopback `http://` URLs.
- **Capture policy parity.** Every event goes through the native hook, which
  runs capture policy before spool and network (rows 11b, 11g, 11k). The
  launcher has no capture logic of its own.

**Third front door.** Shell hooks, the native client and the generated
TypeScript integrations are the existing three places marker, scope and
capture logic lives, and fixes have had to cover all three (#661, #668). A
plugin is a fourth entry point. It stays safe only if it adds no logic of its
own and the following stays true, each item backed by a test (see
[Tests](#tests)):

1. a plugin-launched event with an `ignore_paths` match never reaches the
   spool;
2. under allowlist capture mode, a repository without a marker emits nothing;
3. marker `workspace`/`project` and `servers.toml` routing apply to
   plugin-launched events exactly as to settings-launched ones;
4. a plugin copy and a settings copy of the same event produce one
   observation, and the plugin copy is the one dropped;
5. with no binary, nothing is sent anywhere and the agent is not blocked.

Implementation adds a row to `docs/security-boundaries.md` for the plugin
front door, citing these tests, in the same change.

### 6. Other harnesses reading our Claude files

Copilot CLI reads `.claude-plugin/plugin.json` and
`.claude-plugin/marketplace.json` as fallbacks, and Droid falls back to
`.claude-plugin/marketplace.json`. Codex also falls back to it when no
`.agents/plugins/marketplace.json` exists. A user who points one of those at
our repository would install the Claude plugin and send events tagged
`claude-code` in a payload dialect the server does not expect. Two guards:

- the launcher recognizes the other harnesses' markers (`DROID_PLUGIN_ROOT`,
  Codex's `PLUGIN_ROOT`, Cursor's `CURSOR_VERSION`). It hands the event to
  the matching ai-memory agent when one exists, and otherwise prints `{}` and
  exits. *Shipped:* `DROID_PLUGIN_ROOT` → `droid`; `PLUGIN_ROOT` without
  `CLAUDE_PLUGIN_ROOT` → `codex`; neither `CLAUDE_PLUGIN_ROOT` nor
  `PLUGIN_ROOT` → `{}`. A Droid-launched event carries
  `AI_MEMORY_HOOK_ORIGIN=droid-plugin` and gets the same folderless skip and
  de-duplication as the Claude Code plugin, against the one file Droid reads
  hooks from (`~/.factory/hooks.json`, else the `hooks` key of
  `~/.factory/settings.json`). Cursor is not remapped: when Cursor runs Claude
  hooks the event already reaches the existing #721 path as `claude-code`
  with `cursor_version`, and whether Cursor runs *plugin* hooks at all is
  one of the Cursor unknowns. *Shipped:* Meta Muse Code sets
  `CLAUDE_PLUGIN_ROOT` for the Claude plugins it loads, so it is told apart
  by `MUSE_PLUGIN_ID` together with `MUSE_SESSION_ID` (both seen in a Muse
  Code 1.4.7 plugin hook) → `muse`, with `AI_MEMORY_HOOK_ORIGIN=muse-plugin`,
  the folderless skip, de-duplication against the `hooks` key of
  `${XDG_CONFIG_HOME:-~/.config}/muse/settings.json`, and the plugin's
  subagent events dropped. Muse Code 1.4.7 refuses to install the published
  plugin because its `.mcp.json` uses `${user_config.*}` placeholders, so this
  path is exercised only with a copy that has no `.mcp.json`;
- we ship a native catalog for each harness we support, so the fallback is
  never used.

The server's `agent_from_payload` relabelling remains the last line of
defence.

## Repository layout

Recommendation: keep the plugins and the marketplace catalogs **in this
repository**, with catalog entries pinned to the release tag.

```
.claude-plugin/marketplace.json        Claude catalog (also read by Droid)
.agents/plugins/marketplace.json       Codex catalog
.cursor-plugin/marketplace.json        Cursor catalog (phase 3)
plugins/
  README.md                            what each plugin runs and sends
  claude-code/
    .claude-plugin/plugin.json
    hooks/hooks.json
    .mcp.json
    scripts/ai-memory-hook             generated copy of the shared launcher
    skills/<six routing skills>/SKILL.md
    skills/status/SKILL.md
    README.md  LICENSE
  codex/
    plugin.json  mcp.json  hooks/hooks.json  scripts/  skills/
  cursor/
    .cursor-plugin/plugin.json  mcp.json  hooks/hooks.json  scripts/  skills/
```

*Shipped layout:* as above without `cursor/`, and Codex's manifest is
`plugins/codex/.codex-plugin/plugin.json` (Phase 0 finding 10). The plugin
templates (launcher, READMEs, `status` skill) live in
`crates/ai-memory-cli/templates/plugin/`.

Each harness gets its own directory: event names, the `--agent` value and
the MCP flags (`--no-session-id` for everyone except Claude) all differ, and
Claude strips manifest keys it does not know while still loading
`hooks/hooks.json`. That rules out a shared hooks file in one directory
(Arroway and claude-mem keep separately named hooks files for the same
reason). Skills are copies generated from the same source, not symlinks;
symlinks break zip uploads and Windows checkouts.

Every file under `plugins/` is **generated** by Rust code from the sources
that already feed `install-hooks` and `install-skills`: `CLAUDE_CODE_EVENTS`,
`CODEX_EVENTS` and `CURSOR_EVENTS` in `render_shared.rs`, and the
`MANAGED_SKILLS` in `ai-memory-core::routing_skills`. A CLI suite test fails
when the committed tree differs from the renderer's output. That is how
plugin installs and CLI installs avoid drifting, the RFC's stated goal. The
renderer is a hidden `ai-memory render-plugins --out <dir>` subcommand with
two callers, the drift test and `bin/release`. A golden-file test with an
environment toggle would also work; the subcommand is easier to run by hand.

**Why in this repository:**

- Plugin files change in the same PR as the Rust code and tests they depend
  on, under the same CI and review.
- Claude (`git-subdir` with `ref`) and Codex (`git-subdir` with `ref` or
  `sha`) can pin each catalog entry to `vX.Y.Z`. Unreleased edits on `main`
  never reach users, and the catalog on `main` always points at the newest
  release. Cognee's Codex catalog already points back into its own repo this
  way.
- `bin/release` stamps every version and `ref` in the release commit, so the
  tag carries a catalog that points at itself. Nothing is pushed after the
  release and no cross-repository token is needed.
- The repository is small enough to serve as a marketplace: 1,011 tracked
  files and a 31 MiB pack, against the directory checklist's limits of
  10,000 files and 50 MiB. Both CLIs offer `--sparse` for marketplace adds.

**Why not a separate `ai-memory-plugins` repository (yet):** it adds a
release step that needs a write token for another repository, a second place
to review, and a drift risk. It becomes worth it for formats that need a
manifest at the repository root: the Gemini gallery (`gemini-extension.json`
at the root), `agy plugin install <github url>`, and possibly a directory
submission whose reviewer clones the whole repository. If that happens,
the release workflow can publish `plugins/<harness>/` into it as generated
output, the same way Arroway's public repo mirrors a private source.

The Cursor catalog cannot pin a ref as far as the docs say. Cursor imports a
repository and refreshes it. Phase 3 has to verify whether a branch can be
chosen (a `plugins-release` branch moved by the release workflow would solve
it) before Cursor ships.

## Manifests

These are the files the renderer would produce for 3.0.0. The Claude ones
passed `claude plugin validate --strict` in the prototype.

### Phase 1: Claude Code

`.claude-plugin/marketplace.json` (repository root):

```json
{
  "name": "ai-memory",
  "owner": { "name": "AkitaOnRails", "url": "https://github.com/akitaonrails" },
  "description": "ai-memory plugins: lifecycle capture and memory tools for coding agents, backed by your own ai-memory server.",
  "plugins": [
    {
      "name": "ai-memory",
      "description": "Long-term memory across sessions and harnesses.",
      "source": {
        "source": "git-subdir",
        "url": "https://github.com/akitaonrails/ai-memory.git",
        "path": "plugins/claude-code",
        "ref": "v3.0.0"
      }
    }
  ]
}
```

`plugins/claude-code/.claude-plugin/plugin.json`:

```json
{
  "name": "ai-memory",
  "displayName": "ai-memory",
  "version": "3.0.0",
  "description": "Long-term memory across sessions and harnesses. Hands each lifecycle event to the installed ai-memory binary, which applies your capture exclusions and sends a sanitized record only to your ai-memory server (default http://127.0.0.1:49374). Adds the ai-memory MCP tools. No telemetry. Requires the ai-memory binary.",
  "author": { "name": "AkitaOnRails", "url": "https://github.com/akitaonrails" },
  "homepage": "https://github.com/akitaonrails/ai-memory",
  "repository": "https://github.com/akitaonrails/ai-memory",
  "license": "MIT",
  "keywords": ["memory", "mcp", "hooks", "handoff", "context"],
  "userConfig": {
    "server_url": {
      "type": "string",
      "title": "ai-memory server URL",
      "description": "Base URL of the ai-memory server. Keep the default for a local install.",
      "default": "http://127.0.0.1:49374"
    },
    "ai_memory_bin": {
      "type": "string",
      "title": "ai-memory executable",
      "description": "Path to the ai-memory binary, only needed when it is not on PATH.",
      "default": "ai-memory"
    },
    "data_dir": {
      "type": "string",
      "title": "ai-memory data directory",
      "description": "Only for an install that uses a non-default data directory.",
      "default": ""
    }
  }
}
```

`plugins/claude-code/hooks/hooks.json`, the same events and empty matchers
`install-hooks` writes today (`PostToolUseFailure` was added later in the same
shape):

```json
{
  "description": "ai-memory lifecycle capture through the installed ai-memory binary",
  "hooks": {
    "SessionStart":     [{ "matcher": "", "hooks": [{ "type": "command", "command": "sh \"${CLAUDE_PLUGIN_ROOT}/scripts/ai-memory-hook\" claude-code session-start" }] }],
    "UserPromptSubmit": [{ "matcher": "", "hooks": [{ "type": "command", "command": "sh \"${CLAUDE_PLUGIN_ROOT}/scripts/ai-memory-hook\" claude-code user-prompt-submit" }] }],
    "PreToolUse":       [{ "matcher": "", "hooks": [{ "type": "command", "command": "sh \"${CLAUDE_PLUGIN_ROOT}/scripts/ai-memory-hook\" claude-code pre-tool-use" }] }],
    "PostToolUse":      [{ "matcher": "", "hooks": [{ "type": "command", "command": "sh \"${CLAUDE_PLUGIN_ROOT}/scripts/ai-memory-hook\" claude-code post-tool-use" }] }],
    "PreCompact":       [{ "matcher": "", "hooks": [{ "type": "command", "command": "sh \"${CLAUDE_PLUGIN_ROOT}/scripts/ai-memory-hook\" claude-code pre-compact" }] }],
    "Stop":             [{ "matcher": "", "hooks": [{ "type": "command", "command": "sh \"${CLAUDE_PLUGIN_ROOT}/scripts/ai-memory-hook\" claude-code stop" }] }],
    "SessionEnd":       [{ "matcher": "", "hooks": [{ "type": "command", "command": "sh \"${CLAUDE_PLUGIN_ROOT}/scripts/ai-memory-hook\" claude-code session-end" }] }],
    "SubagentStart":    [{ "matcher": "", "hooks": [{ "type": "command", "command": "sh \"${CLAUDE_PLUGIN_ROOT}/scripts/ai-memory-hook\" claude-code subagent-start" }] }],
    "SubagentStop":     [{ "matcher": "", "hooks": [{ "type": "command", "command": "sh \"${CLAUDE_PLUGIN_ROOT}/scripts/ai-memory-hook\" claude-code subagent-stop" }] }]
  }
}
```

No `timeout` is set. Unlike the settings entry written by `install-hooks`, a
plugin's own timeout cannot raise Claude's 1.5-second SessionEnd budget. The
native hook normally returns after a local spool write and hands draining to a
detached process; if it is killed during an unusually slow fsync, a later drain
adopts the complete old `*.json.tmp` entry. `async: true` on the tool events is
worth measuring later, but it changes how Claude reports output.

`plugins/claude-code/.mcp.json`:

```json
{
  "mcpServers": {
    "ai-memory": {
      "command": "${user_config.ai_memory_bin}",
      "args": ["mcp-bridge", "--server-url", "${user_config.server_url}"]
    }
  }
}
```

The session-aware bridge (no `--no-session-id`), the same one `install-mcp
--client claude-code` registers; the plugin MCP process receives
`CLAUDE_CODE_SESSION_ID` (local finding 7). No token: the bridge
authenticates with the bearer `Config::load` resolves. `.mcp.json` takes the
binary from an option rather than the launcher because a stdio MCP `command`
must be a real executable on Windows.

`plugins/claude-code/skills/` holds the six managed routing skills plus a
`status` skill with `disable-model-invocation: true`, invoked as
`/ai-memory:status`. It runs `ai-memory doctor` for the plugin's agent and
summarizes the result: binary found, server reachable, duplicate wiring, spool
backlog, and for Codex untrusted hooks. The skill only reports. Fixes
stay with the user.

The six routing skills duplicate what `install-skills` puts in
`~/.claude/skills`, and two copies of each description would sit in context.
`setup` and `install-skills` skip the Claude target when the plugin is
enabled (section 3).

### Phase 2: Codex

Portable layout, which Codex 0.162.1 accepted locally (finding 10).

`.agents/plugins/marketplace.json`:

```json
{
  "name": "ai-memory",
  "interface": { "displayName": "ai-memory" },
  "plugins": [
    {
      "name": "ai-memory",
      "source": {
        "source": "git-subdir",
        "url": "https://github.com/akitaonrails/ai-memory.git",
        "path": "plugins/codex",
        "ref": "v3.0.0"
      },
      "policy": { "installation": "AVAILABLE", "authentication": "ON_INSTALL" },
      "category": "Productivity"
    }
  ]
}
```

`plugins/codex/.codex-plugin/plugin.json` (shipped; the proposal's Agent
Plugins root `plugin.json` installs but loads no hooks, finding 10):

```json
{
  "name": "ai-memory",
  "version": "3.0.0",
  "description": "Long-term memory across sessions and harnesses, backed by the installed ai-memory binary and your ai-memory server. No telemetry. Codex runs the hooks only after you trust them in /hooks.",
  "author": { "name": "AkitaOnRails", "url": "https://github.com/akitaonrails" },
  "homepage": "https://github.com/akitaonrails/ai-memory",
  "repository": "https://github.com/akitaonrails/ai-memory",
  "license": "MIT",
  "mcpServers": "./mcp.json"
}
```

`plugins/codex/mcp.json`:

```json
{
  "$schema": "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json",
  "mcpServers": {
    "ai-memory": { "type": "stdio", "command": "ai-memory", "args": ["mcp-bridge", "--no-session-id"] }
  }
}
```

`plugins/codex/hooks/hooks.json` (default discovery; seven `CODEX_EVENTS`):

```json
{
  "hooks": {
    "SessionStart": [{ "hooks": [{ "type": "command",
      "command": "sh \"${PLUGIN_ROOT}/scripts/ai-memory-hook\" codex session-start",
      "commandWindows": "ai-memory hook --event session-start --agent codex" }] }]
  }
}
```

The remaining events follow the same shape. `SessionEnd` stays within Codex's
1 s default (3 s maximum). Open questions for Codex: which `authentication`
policy value suits a plugin with no auth of its own, whether omitting
`matcher` is valid for every event, and whether a hook trusted once stays
trusted after a plugin upgrade (open question 2). The catalog
`ref` is stamped by `bin/release` exactly like Claude's. *Resolved in Phase
0:* omitting `matcher` loads all seven events, `ON_INSTALL` installs without
prompting for anything, and trust survives an upgrade (finding 13). The
`commandWindows` value omits `--server-url`, which the binary now treats as
"use the config"; it passes no empty argument because Windows PowerShell 5.1
drops empty arguments to native commands.

### Phase 3: Cursor

`.cursor-plugin/plugin.json` plus Cursor's camelCase events from
`CURSOR_EVENTS` (`sessionStart`, `sessionEnd`, `beforeSubmitPrompt`,
`preToolUse`, `postToolUse`, `postToolUseFailure`, `preCompact`, `stop`):

```json
{ "name": "ai-memory", "version": "3.0.0",
  "description": "Long-term memory across sessions and harnesses, backed by the installed ai-memory binary. No telemetry.",
  "author": { "name": "AkitaOnRails" }, "license": "MIT" }
```

```json
{ "hooks": { "sessionStart": [ { "command": "sh \"${CURSOR_PLUGIN_ROOT}/scripts/ai-memory-hook\" cursor session-start" } ] } }
```

```json
{ "mcpServers": { "ai-memory": { "command": "ai-memory", "args": ["mcp-bridge", "--no-session-id"] } } }
```

Not verified, and must be settled before Cursor ships:

- whether `${CURSOR_PLUGIN_ROOT}` expands in hook commands (the docs only
  promise it for MCP fields);
- what a relative `./scripts/…` resolves against;
- whether the plugin `hooks.json` needs `"version": 1`;
- whether Cursor's third-party import also runs hooks from Claude *plugins*,
  or only from Claude settings files.

The #721 de-duplication has to learn about the Cursor plugin. Today
`cursor_native_hooks_installed` looks only at `~/.cursor/hooks.json`.

### Later: Antigravity, Gemini CLI, Copilot CLI, Droid

- **Antigravity.** It documents no plugin-root variable, so a hook cannot
  find a bundled launcher. Its hook would have to call `ai-memory` from
  `PATH`, which brings back the GUI `PATH` problem. Install by GitHub URL
  probably needs a root manifest. The per-turn `invocationNum` issue
  (verification notes) should be fixed first.
- **Gemini CLI.** Needs a root `gemini-extension.json` for the gallery and for
  install-by-URL. Low priority given the move to Antigravity.
- **Copilot CLI and Droid.** Both read Claude-style layouts and have Claude-style
  hooks. Each needs its own directory and catalog (`.github/plugin/`,
  `.factory-plugin/`) with its own `--agent`. The launcher guard in section 6
  covers the meantime.

## Automation

### On pull requests (`.github/workflows/plugins.yml`, path-filtered)

*Shipped:* jobs `validate` (items 2), `launcher` (item 3) and `e2e` (items
4 and 6, plus a cheap `render-plugins` drift check for pushes to `main`).
Item 1 runs in `ci.yml`'s existing `cargo nextest` job, item 5 in the CLI
suite. Claude Code 2.1.296 and Codex 0.162.1 are pinned as npm installs in
the workflow `env`. The Codex smoke points the model provider at a closed
loopback port, so it needs no secret and no outbound request. Item 7
(Windows) stays open: the launcher tests are POSIX-only, and nothing has
run on Windows yet (see the unverified list).

Paths: `plugins/**`, the three catalog files, `render_shared.rs`,
`ai-memory-core/src/routing_skills/**`, `bin/ai-memory`, the workflow itself.

1. **Drift and versions.** `cargo nextest run -p ai-memory-cli -E
   'test(/plugin/)'`: the committed `plugins/` tree equals the renderer
   output, every manifest version equals `workspace.package.version`, every
   catalog `ref` is `v<that version>`, and the plugin tree has no
   bearer-shaped strings.
2. **Claude validation.** Install a pinned Claude Code CLI (as a CI tool,
   not a build dependency), then `claude plugin validate --strict .` (marketplace)
   and `claude plugin validate --strict plugins/claude-code`.
3. **Launcher tests.** `shellcheck -s sh` plus `tests/hooks/test_plugin_launcher.sh`:
   missing binary (exit 0, `systemMessage` only on session-start, stdin
   drained), option precedence, the Docker wrapper on `PATH`, the
   foreign-harness guard, and a timing bound.
4. **Sandboxed Claude end-to-end.** Build the binary, start `ai-memory serve`
   on a scratch data dir and a free port, then:

   ```bash
   export CLAUDE_CONFIG_DIR="$RUNNER_TEMP/claude"
   mkdir -p "$CLAUDE_CONFIG_DIR"
   # Options for a --plugin-dir plugin live under the id "ai-memory@inline".
   jq -n --arg url "$URL" --arg bin "$PWD/target/debug/ai-memory" --arg dd "$RUNNER_TEMP/data" \
     '{pluginConfigs: {"ai-memory@inline": {options: {server_url: $url, ai_memory_bin: $bin, data_dir: $dd}}}}' \
     > "$CLAUDE_CONFIG_DIR/settings.json"
   cd "$FIXTURE_REPO"
   env -u ANTHROPIC_API_KEY claude -p "hi" --plugin-dir "$GITHUB_WORKSPACE/plugins/claude-code" || true
   ```

   Then assert through `/api/v1` or `ai-memory search` that the server holds
   `session-start`, `user-prompt-submit` and `session-end` observations for
   agent `claude-code` in the project the fixture's marker names. This works
   without credentials (local finding 3). `--plugin-dir` loads the PR's files
   directly; the catalog itself pins a tag that does not exist yet, so a
   second step only runs `claude plugin marketplace add "$GITHUB_WORKSPACE"`
   and checks that the catalog loads. Scrub `AI_MEMORY_*` and managed-run
   variables first: in the prototype run they leaked a `managed_run`
   parameter into the spooled URL.
5. **Front-door parity.** Tool events cannot be driven without a model, so
   policy parity is tested one level down. Pipe recorded Claude payloads
   through the launcher into the real binary and assert that an
   `ignore_paths` match never reaches the spool, that allowlist mode without
   a marker emits nothing, that marker and `servers.toml` routing apply, and
   that a plugin copy is dropped when settings hooks exist. These are the
   adversarial tests the security-boundary row cites.
6. **Codex smoke.** In a scratch `CODEX_HOME`: `codex plugin marketplace add`
   on the checkout, then `codex plugin add ai-memory@ai-memory`, and assert
   the cached plugin root contains the expected files. Optionally, drive `codex
   app-server` with `hooks/list` (the method used in the verification notes)
   to assert the hooks are discovered and reported `untrusted`.
7. **Windows.** Add `plugins/**` to `windows.yml`'s path filter once the
   Windows story is decided.

### On release

- `bin/release <version>` stamps `version` in every plugin manifest and
  catalog entry and sets every catalog `ref` to `v<version>`, in the same
  commit that bumps `Cargo.toml`.
- `release.yml`'s `validate-version` job also checks those values against
  the tag.
- The `github-release` job attaches `ai-memory-claude-plugin.zip` (the
  `plugins/claude-code` tree) and its `.sha256`, for **Customize > Plugins >
  Upload plugin** in the desktop app and Cowork.
- Nothing else is published. The catalogs on `main` point at the new tag as
  soon as the release commit lands. Existing users get the update through
  `ai-memory upgrade` (section 3), `claude plugin update`, or auto-update if
  they turned it on.
- Add "plugin catalogs point at the new tag; `claude plugin validate` passes
  on the tag" to the release checklist next to the Homebrew tap step.

`claude plugin tag` creates `{name}--v{version}` tags. We already tag
`vX.Y.Z`, and our catalogs point at that tag, so it is not needed.

## Tests

Mapped to the boundary inventory, all at integration level:

| Test | Proves | Where (shipped) |
| --- | --- | --- |
| Committed `plugins/` equals renderer output; versions and refs match the workspace; no credential in any plugin file | No drift between CLI and plugin installs | CLI `tests/suite/plugins.rs` `committed_plugins_match_the_renderer`, `plugin_versions_and_catalog_refs_match_the_workspace`, `plugin_tree_carries_no_credentials`; `render_plugins.rs` unit tests |
| Launcher, missing binary | Fail-open, guidance once, nothing sent | `tests/hooks/test_plugin_launcher.sh`; `plugins.rs` `without_a_binary_nothing_is_sent_and_the_agent_is_not_blocked` |
| `ignore_paths` and allowlist through the launcher | Capture policy parity (front door 1, 2) | `plugins.rs` `ignore_paths_hold_through_the_plugin`, `allowlist_mode_holds_through_the_plugin` (real binary, recorded payloads) |
| Marker and server-profile routing through the launcher | Scope parity (front door 3) | `plugins.rs` `marker_scope_routes_plugin_events`, `server_profiles_route_plugin_events` |
| Plugin copy plus settings copy gives one observation; control: plugin alone delivers | De-duplication (front door 4), adversarial with a control | `plugins.rs` `a_settings_hook_for_the_same_event_drops_the_plugin_copy`; `hook.rs` `plugin_copy_is_dropped_only_with_the_plugin_origin`; `plugin_origin.rs` unit tests; `claude_plugin_smoke.py` scenario 2 (real `claude -p`) |
| Folderless sessions and foreign hosts | Decision 8; section 6 guard | `plugins.rs` `folderless_sessions_are_not_captured_through_the_plugin`, `foreign_hosts_are_attributed_or_ignored`; `test_plugin_launcher.sh` |
| Docker wrapper routes `hook` to the native client | Wrapper fix | Lands on `main` separately (not in this change) |
| Sandboxed Claude install fires hooks into a scratch server | Real wiring, no API key | `tests/e2e/claude_plugin_smoke.py` in `plugins.yml` |
| Codex install, untrusted by default, trusted hooks fire, trust survives a bump | Real wiring, no API key | `tests/e2e/codex_plugin_smoke.py` in `plugins.yml` |
| `setup`/`run` skip, `doctor`, `upgrade`, `uninstall` | Integration | `setup.rs` `an_enabled_plugin_takes_over_the_agents_wiring`, `run_autowire.rs` `an_enabled_plugin_leaves_the_harness_unwired`, `doctor.rs` `plugin_report_names_double_wiring_and_skew`, `upgrade.rs` `upgrade_updates_an_installed_claude_code_plugin_only`, `uninstall.rs` `uninstall_removes_each_plugin_through_its_harness`, `codex_hook_trust.rs` `summarizes_the_ai_memory_plugin_hooks_by_plugin_id` |

## Phased plan for 3.0

Estimates are engineer-days for one person familiar with the codebase.

| Phase | Work | Estimate |
| --- | --- | --- |
| **0. Verification spikes** | Codex: does trust survive a plugin version bump? (two installs, `hooks/list` on each). Claude Desktop Code tab on macOS: what `PATH` do plugin hooks get? Cowork on macOS/Windows: do plugin hooks run on the host and reach `127.0.0.1:49374`? Claude Code on Windows with and without Git Bash. Cursor: the four unknowns above | 1-1.5 |
| **1. Claude Code plugin** | Renderer and hidden `render-plugins`; launcher; `plugins/claude-code`; root catalog; `hook` origin de-duplication; empty `--server-url` handling; Docker wrapper `hook` routing; `setup`/`install-*`/`doctor`/`upgrade`/`uninstall` integration; drift, launcher, parity and de-duplication tests; `docs/install.md` and `docs/mcp-install.md` sections, `plugins/README.md`, CHANGELOG `Added`, security-boundaries row | 4-5 |
| **2. CI and release automation** | `plugins.yml` (validate, launcher, sandboxed e2e, Codex smoke); `bin/release` stamping; `validate-version` extension; release zip asset | 1.5-2 |
| **3. Codex plugin** | `plugins/codex`, `.agents/plugins/marketplace.json`, `commandWindows`, trust guidance in docs, `doctor` plugin-hook trust check | 2 |
| **4. Cursor plugin** (3.0 stretch, else 3.1) | Depends on the phase 0 answers; #721 detection of the Cursor plugin; branch strategy for the Cursor import | 2-3 |
| **Later** | Cowork capture if phase 0 shows hooks run in the VM; Antigravity; Gemini; Copilot CLI and Droid catalogs; a directory submission | — |

Phases 1 to 3 come to roughly 9 to 10.5 days. This is additive work, so it targets
`release/3.0`, not `main`.

*Delivered for 3.0:* phase 0 (the locally runnable spikes, findings 10-14),
phase 1 except the Docker wrapper routing (landing separately on `main`),
phase 2, and phase 3. Phase 4 (Cursor) waits for 3.1: its four unknowns
need a Cursor install with plugin support, and the #721 detection would have
to learn the plugin format first.

## Open questions for the maintainer

All eight were decided on 2026-10-13. The original question follows each
decision in brief.

1. **Directory listing. Decided: no submission** to the claude.ai or
   Anthropic directory for the capture plugin; capture hooks are ungated by
   design and do not qualify. The plugin ships through our own marketplace
   in this repository. An MCP-only listing may come later, not now.
   (Asked: also list in the directory portal, possibly as an
   allowlist-capture variant?)
2. **Codex trust across upgrades. Decided: accept and document it** as
   Codex's security model. Phase 0 found that trust survives an upgrade
   anyway, because Codex hashes the unexpanded command (finding 13); a
   release that changes a hook command needs one re-trust, which the
   changelog will say. `doctor` and the `status` skill flag skipped hooks.
3. **Cowork. Decided:** the plugin says in one line that capture runs on the
   user's computer and needs ai-memory installed there, and that the memory
   tools still work (manifest description and README). No extra
   session-start message is added.
4. **Windows. Decided: Git Bash** is fine for Claude plugin hooks, since
   Claude Code requires it already. A PowerShell launcher only if Codex
   needs one; it does not, because Codex's `commandWindows` calls
   `ai-memory hook` directly.
5. **Which wiring wins. Decided:** settings hooks win at runtime; the
   plugin's launcher path skips the events the settings hooks already
   deliver, and `doctor` reports the double install and suggests keeping
   one. `setup` stops writing settings wiring while the plugin is enabled.
6. **Token option. Decided: no** stored-token option in the plugin; it reuses
   the credential files the binary already reads.
7. **Names. Decided: keep `ai-memory@ai-memory`.**
8. **Folderless sessions. Decided: skip capture** for a session with no
   project folder (through the plugin; settings-file hooks keep today's
   `default/scratch` routing).

Target release: 3.0.0 (the branch formerly called `release/2.7`).

## Implementation notes (3.0)

Where the shipped code differs from the proposal above:

- **Server URL.** The `server_url` option defaults to empty and the launcher
  passes it through unchanged; `hook` and `mcp-bridge` read an empty
  `--server-url` as "use the config". `hook --server-url` may now be
  omitted (the Codex `commandWindows` commands rely on it). No
  `AI_MEMORY_HOOK_URL` fallback and no `setup --server-url` → `claude plugin
  configure` step: one source of truth, `config.toml`.
- **Codex manifest.** `.codex-plugin/plugin.json` with `"mcpServers":
  "./mcp.json"`, not the Agent Plugins root manifest (finding 10).
- **No runtime de-duplication for Codex** (section 3).
- **No `--force`.** `setup` and `run` skip plugin-covered agents; the
  explicit `install-*` commands are the override.
- **`upgrade` for Codex prints** the update commands rather than running
  them.
- **`doctor`** does not report plugin options or the Docker wrapper (section
  3).
- **Catalog entry `version`.** The Claude catalog entry also carries the
  plugin version; `claude plugin validate --strict` accepts it.
- **Version.** The renderer stamps the binary's own version, so until
  `bin/release 3.0.0` runs, the committed manifests say the current
  workspace version (2.6.6) and the catalogs point at `v2.6.6`, a tag
  without `plugins/`. A marketplace add of this branch therefore cannot
  install the plugin yet; `--plugin-dir` and local marketplaces can.

## Still unverified, and how to check

Each needs hardware or software this work did not have. Run every check with
a scratch config dir, never the operator's own.

- **Claude Code on Windows (Git Bash present / absent).** In a Windows VM
  with Claude Code: `claude plugin marketplace add <checkout>` and `claude
  --plugin-dir <checkout>\plugins\claude-code -p hi` with
  `CLAUDE_CONFIG_DIR` set to a temp dir and options pointing at a scratch
  server (as in `tests/e2e/claude_plugin_smoke.py`); confirm the
  observations land. Repeat with Git Bash removed from `PATH` and record what
  Claude does with the shell-form hook.
- **Claude desktop Code tab (macOS) hook `PATH`.** Install the plugin, start
  a Code-tab session in a repository, and check that events arrive with
  only `/opt/homebrew/bin/ai-memory` or `~/.local/bin/ai-memory` installed
  (the launcher's fallbacks). `ai-memory doctor` coverage, or a temporary
  `env > /tmp/x` line in a copy of the launcher, shows the `PATH`.
- **Cowork.** Upload `ai-memory-claude-plugin.zip`, start a Cowork session,
  and check whether the hooks run on the host or in the VM (new files under
  the host's `<data_dir>/hook-spool/`, or the missing-binary
  `systemMessage`), and whether the host loopback answers.
- **Codex on Windows.** In Codex for Windows, trust the plugin hooks and
  check that `commandWindows` (`ai-memory hook --event … --agent codex`)
  spools events; `codex app-server` `hooks/list` shows the command Codex
  will run.
- **Codex from the GitHub marketplace.** After the 3.0.0 tag exists: `codex
  plugin marketplace add akitaonrails/ai-memory`, `codex plugin add
  ai-memory@ai-memory`, trust, then after 3.0.1 `codex plugin marketplace
  upgrade ai-memory` + `codex plugin add ai-memory@ai-memory` and confirm
  `hooks/list` still reports `trusted` (finding 13 was proven with local
  and `file://` `git-subdir` sources).
- **Claude install from GitHub at the tag.** After the tag: `claude plugin
  marketplace add akitaonrails/ai-memory`, `claude plugin install
  ai-memory@ai-memory`, then `claude plugin details ai-memory@ai-memory`.
- **Tool events from a live model turn** (PreToolUse, PostToolUse, Stop,
  subagents) need a logged-in model; the CLI suite covers them one level
  down with recorded payloads.
- **Cursor:** the four unknowns in the Cursor section.

## What we borrowed and what we avoided

From the memory tools surveyed (claude-mem, supermemory, mem0, cognee,
hindsight, Arroway, engram) and Anthropic's own plugins:

- **Borrowed:**
  - one dependency-free hook entry point per host;
  - fail open with `{"systemMessage": …}` once when a runtime is missing
    (Anthropic's `claude-security`, `security-guidance`);
  - generated per-host directories with a drift check in CI (mem0's
    `build.py --check`);
  - a clean-profile `marketplace add` + `install --json` smoke test (cognee);
  - a version-consistency check across manifests (cognee, engram);
  - a description that states what is sent and that there is no telemetry.
- **Avoided:**
  - first-run installs inside hooks (claude-mem's Bun bootstrap, cognee's
    120 s blocking venv install, hindsight's unpinned `uvx …@latest`);
  - hooks that scan the plugin cache for the newest version (claude-mem);
  - tokens in plugin-directory or repository-local config files (claude-mem
    Cowork, supermemory);
  - blocking hooks with long timeouts (hindsight's 45 s recall, Arroway's
    700 s `Stop`);
  - default-on identified telemetry (mem0);
  - a top-level `bin/`.

## Appendix: the prototype launcher

The script used in the local runs. The shipped version adds the
foreign-harness guard from section 6, per-harness option variables, and the
`shellcheck` and shell tests listed under Automation.

```sh
#!/bin/sh
# ai-memory plugin hook launcher. Usage: ai-memory-hook <agent> <event>
# Resolves the installed ai-memory binary and hands the event to the native
# `ai-memory hook` command, which owns capture policy, spooling and routing.
# Never blocks the agent: a missing binary drains stdin and exits 0.
agent="$1"
event="$2"
url="${CLAUDE_PLUGIN_OPTION_SERVER_URL:-${AI_MEMORY_HOOK_URL:-http://127.0.0.1:49374}}"
bin="${CLAUDE_PLUGIN_OPTION_AI_MEMORY_BIN:-}"
if [ -z "$bin" ] || [ "$bin" = "ai-memory" ]; then
    bin=""
    if command -v ai-memory >/dev/null 2>&1; then
        bin=$(command -v ai-memory)
    else
        for candidate in "$HOME/.local/bin/ai-memory" /opt/homebrew/bin/ai-memory \
            /usr/local/bin/ai-memory /usr/bin/ai-memory; do
            if [ -x "$candidate" ]; then bin="$candidate"; break; fi
        done
    fi
fi
if [ -z "$bin" ] || [ ! -x "$bin" ]; then
    cat >/dev/null
    if [ "$event" = "session-start" ]; then
        printf '%s\n' '{"systemMessage":"ai-memory: the ai-memory binary was not found, so this session is not captured. Install it and run `ai-memory setup`: https://github.com/akitaonrails/ai-memory#install"}'
    else
        printf '{}\n'
    fi
    exit 0
fi
set -- --event "$event" --agent "$agent" --server-url "$url"
if [ -n "${CLAUDE_PLUGIN_OPTION_DATA_DIR:-}" ]; then
    set -- --data-dir "$CLAUDE_PLUGIN_OPTION_DATA_DIR" "$@"
fi
AI_MEMORY_HOOK_ORIGIN="$agent-plugin" exec "$bin" hook "$@"
```

## Sources

All fetched 2026-10-13.

Claude:
- Plugin manifest reference: <https://code.claude.com/docs/en/plugins/manifest-reference>
- Plugin components (hooks, MCP, `bin/`, data dir): <https://code.claude.com/docs/en/plugins/components>
- Hooks reference (shell vs exec form, Windows shells, SessionEnd budget, workspace trust, deduplication): <https://code.claude.com/docs/en/hooks>
- MCP (plugin servers, variable expansion, `headersHelper`): <https://code.claude.com/docs/en/mcp>
- Marketplace reference (sources, `strict`, versions): <https://code.claude.com/docs/en/plugins/marketplace-reference>
- Host a marketplace (auto-update off by default): <https://code.claude.com/docs/en/plugins/host-marketplace>
- CLI reference (`validate`, `configure`, `tag`, `details`): <https://code.claude.com/docs/en/plugins/cli-reference>
- Loading and local testing: <https://code.claude.com/docs/en/plugins/loading>
- Publish and Anthropic marketplaces: <https://code.claude.com/docs/en/plugins/publish>, <https://code.claude.com/docs/en/plugins/anthropic-marketplaces>
- Plugin security: <https://code.claude.com/docs/en/plugins/security>
- Plugin evals: <https://code.claude.com/docs/en/plugin-evals>
- Settings (`disableAllHooks`, `allowManagedHooksOnly`): <https://code.claude.com/docs/en/settings-reference>
- Platform support (Chat, Cowork, Code): <https://claude.com/docs/plugins/platform-support>
- Pre-submission checklist and submission: <https://claude.com/docs/plugins/pre-submission-checklist>, <https://claude.com/docs/plugins/submit>
- Desktop extensions and Cowork VM note: <https://claude.com/docs/third-party/claude-desktop/extensions>
- Official marketplace review prompt: `anthropics/claude-plugins-official`, `.github/policy/prompt.md`

Codex:
- Build plugins: <https://developers.openai.com/plugins/build/plugins> (same content at <https://developers.openai.com/codex/plugins/build>)
- Hooks and plugin hook trust: <https://learn.chatgpt.com/docs/hooks> (redirect target of <https://developers.openai.com/codex/hooks>)

Cursor:
- Plugins: <https://cursor.com/docs/plugins>
- Plugin reference: <https://cursor.com/docs/reference/plugins>
- Hooks: <https://cursor.com/docs/hooks>
- Third-party hooks: <https://cursor.com/docs/reference/third-party-hooks>

Others:
- Gemini CLI extensions: <https://geminicli.com/docs/extensions/>, <https://geminicli.com/docs/extensions/reference/>, <https://geminicli.com/docs/extensions/releasing/>, <https://geminicli.com/docs/hooks/>
- Antigravity: <https://antigravity.google/docs/plugins.md>, <https://antigravity.google/docs/hooks.md>, <https://antigravity.google/docs/mcp.md>
- Copilot CLI plugin reference: <https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-plugin-reference>
- Factory Droid plugins: <https://docs.factory.com/cli/configuration/plugins.md>
- OpenCode plugins: <https://opencode.ai/docs/plugins/>
- Kimi Code plugins and hooks: <https://moonshotai.github.io/kimi-cli/en/customization/plugins.md>, <https://moonshotai.github.io/kimi-cli/en/customization/hooks.md>
- Agent Plugins 1.0.0: <https://agent-plugins.org/specification>
- Zed MCP extensions: <https://zed.dev/docs/extensions/mcp-extensions>

Example repositories read: `thedotmack/claude-mem`,
`supermemoryai/claude-supermemory`, `mem0ai/mem0` (`integrations/`),
`topoteretes/cognee-integrations`, `vectorize-io/hindsight`,
`oaleviola/arroway-plugin`, `anthropics/claude-plugins-official`,
`anthropics/claude-code` (`plugins/`), `Gentleman-Programming/engram`.
