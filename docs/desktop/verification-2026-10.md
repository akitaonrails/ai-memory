# Live verification pass (2026-10-08)

> Follow-up to the RFC #878 research notes in this directory. Each finding
> below was observed on the research machine (Omarchy/Arch, Linux) or read
> from current official documentation on this date. Experiments ran in
> scratch directories with scratch `CODEX_HOME`s and workspace-level hook
> files; the operator's global agent configuration was never modified. The
> notes in this directory were corrected where these findings contradicted
> them.

Versions: Claude Desktop 2.9939.4 (bundled Claude Code 2.1.280), Codex
desktop 26.707.31428 (community Linux build) driving Codex CLI 0.156.0,
Antigravity CLI (`agy`) 1.3.0, Antigravity IDE 2.5.5, Cursor 3.24.9.

## Codex desktop

**How the app runs Codex.** The process tree of the running app shows
`codex -c features.code_mode_host=true app-server --remote-control
--analytics-default-enabled`, spawned by Electron. The binary is the `codex`
found on `PATH` (here the Codex CLI 0.156.0), and the launcher exports
`CODEX_HOME=$HOME/.codex` unless it is already set
(`/opt/codex-desktop/start.sh`). The desktop app and the CLI therefore share
one config, hooks file, trust state and session store.

**Hooks fire under `app-server`.** A scratch `CODEX_HOME` with a recorder
`hooks.json` for six events was driven over stdio with the app-server
protocol (`initialize`, `hooks/list`, `thread/start`, `turn/start`):

| Run | Result |
| --- | --- |
| Hooks untrusted | `hooks/list` reports `trustStatus: untrusted`; no hook runs, no `hook/started` notification, nothing tells the user |
| Hooks trusted (`[hooks.state."<path>:<event>:0:0"] trusted_hash = "<currentHash>"`) | `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop` run, each with `hook/started` and `hook/completed` notifications |

Payload keys match the CLI's: `session_id`, `transcript_path`, `cwd`,
`hook_event_name`, `model`, `permission_mode`, plus `source` (`startup`) on
`SessionStart`, `prompt` and `turn_id` on `UserPromptSubmit`, `tool_name`,
`tool_input`, `tool_use_id` (and `tool_response` after the call) on the tool
events, and `last_assistant_message` on `Stop`. Nothing in the payload
identifies the desktop app; the client name only reaches the rollout's
`session_meta.originator` (whatever the client sends as `clientInfo.name`).

**`SessionEnd` timing.** `thread/unsubscribe` does not fire it.
`thread/archive` fires it at once, and a clean app-server shutdown fires it
for a still-open thread. In the desktop app a conversation left open
therefore gets its `SessionEnd` only when it is archived or the app quits.

**Trust is keyed by content, not by file.** Codex hashes a hook's event,
matcher and handler (`codex-rs/hooks/src/engine/discovery.rs` `hook_hash`);
the path only names the state entry. Loading the operator's restored
`~/.codex/hooks.json` content into a scratch home and comparing the reported
`currentHash` values with the operator's stored `trusted_hash` values showed
all seven ai-memory hooks as trusted. Any change to a hook command (a new
binary path, data dir or server URL) makes it `modified`, and Codex then
skips it without telling the user until it is trusted again.

**Usage on this machine.** No rollout under `~/.codex/sessions` was created
by the desktop app (originators are `codex-tui`, `codex_cli_rs`,
`codex_exec`), so no desktop conversation has been run here yet.

## Antigravity

**CLI hooks, verified with `agy` 1.3.0** (workspace `.agents/hooks.json`
recorder, `agy -p` and the documented stream-json input
`{"event":"user","message":{"content":"…"}}` for two turns in one process):

- The workspace hook file was honored with no trust prompt.
- Events per user turn: `PreInvocation` → (`PreToolUse` → `PostToolUse`)* →
  `PostInvocation`, repeated per model call, then `Stop`
  (`terminationReason: NO_TOOL_CALL`, `fullyIdle: true`). No prompt event
  and no session start or end event, as the docs say.
- **`invocationNum` restarts at 0 on every user turn** within one
  conversation, and `executionNum` was 0 on both turns. `invocationNum == 0`
  therefore marks a user turn, not a new conversation. ai-memory maps that
  event to SessionStart (`should_process_hook_event` in
  `crates/ai-memory-cli/src/commands/hook.rs`), so it posts session-start
  and fetches the handoff on every turn. The brief and the profile digest
  have their own once-per-session markers; whether the per-turn
  session-start post and handoff fetch are acceptable is open.
- **A `PreToolUse` reply without a `decision` denies the tool.** A recorder
  that printed `{}` made the transcript record `tool call denied by
  pre-tool hook` and `PostToolUse` never fired. ai-memory's shell and native
  hooks both print `{"decision": "allow"}` and are unaffected.
- `PostToolUse` carries `toolCall {name, args}`, `stepIdx` and `error`, not
  the tool's output (ai-memory reads it from
  `.system_generated/steps/<stepIdx>/output.txt`, #966).
- `transcriptPath` points at
  `~/.gemini/antigravity-cli/brain/<conversationId>/.system_generated/logs/transcript_full.jsonl`;
  `transcript.jsonl` sits next to it. The docs name only `transcript.jsonl`.
  Both start with a `USER_INPUT` step whose `content` holds the prompt in
  `<USER_REQUEST>` tags, in print mode as well.
- Print mode writes no `~/.gemini/antigravity-cli/history.jsonl` line (the
  #1160 replay therefore has nothing to replay for `agy -p`); the transcript
  does have the prompt.
- Conversation state is kept per conversation in
  `~/.gemini/antigravity-cli/conversations/<id>.db`, with `brain/<id>/`,
  `annotations/<id>.pbtxt` and `presence/<id>.lock` alongside.

**IDE: not verified.** `~/.gemini/antigravity-ide/brain` and
`conversations` are empty: the IDE agent has never run on this machine, and
the IDE has no headless chat command. The docs say the IDE reads
`~/.gemini/config/hooks.json` and `.agents/hooks.json`; one interactive
prompt in a workspace carrying a recorder `.agents/hooks.json` would confirm
it without touching the global config.

## Claude Desktop

**Linux status (official docs).** A beta for Debian-based distributions
only (Ubuntu 22.04+, Debian 12+); other distributions are pointed at the
CLI. Cowork on Linux runs its tasks inside a QEMU/KVM virtual machine the
app hosts. Not in the Linux beta: computer use, dictation, Fedora/RHEL.
`/desktop` and `claude --desktop` are documented for macOS and x64 Windows
only. (<https://code.claude.com/docs/en/desktop-linux>,
<https://code.claude.com/docs/en/desktop>)

**Code tab sessions run the user's hooks.** Every Claude Code transcript
under `~/.claude/projects` was classified by `entrypoint`. The three with
`entrypoint: claude-desktop` (Claude Code 2.1.280, 2026-10-02 to 10-04)
carry `hook_success` attachments for `SessionStart:startup` and
`PreToolUse:Bash` from the ai-memory hooks in `~/.claude/settings.json`, and
an `mcp_instructions_delta` adding the `ai-memory` server.

**Scratch workspaces are Code-tab sessions, not Cowork.** Those three
sessions ran in `~/.config/Claude/scratch-workspaces/<account>/<org>/scratch-<date>-<hex>/`.
Their desktop records sit under `~/.config/Claude/claude-code-sessions/`
with `envScopeId: builtin_local` and `scratchOfferFolder: true`: a Code
session started without a project folder. Cowork keeps its own store under
`~/.config/Claude/local-agent-mode-sessions/`.

**Cowork has never run here.** `~/.config/Claude/logs/cowork_vm_node.log`
reports the VM images missing (`rootfs.img`, `vmlinuz`, `initrd`), so there
is no local evidence about hooks or MCP inside Cowork.

**Chat tab.** `~/.config/Claude/claude_desktop_config.json` exists on Linux
and holds no `mcpServers`; `~/.config/Claude/logs/mcp.log` is empty.
Anthropic's docs say servers defined there reach both the Chat surface and
local Code-tab sessions, and the Code tab prefers that definition on a name
clash. Anthropic's support article now presents `.mcpb` desktop extensions
as the main way to install a local MCP server (it mentions Linux keychains
for their secrets), which settles the `.mcpb` question left open in
`claude-desktop.md`.

`install-mcp --client claude-desktop` refuses to guess a path on Linux and
needs `--config-file ~/.config/Claude/claude_desktop_config.json`.

## Cursor (added the same day; details in `cursor.md`)

- The running desktop app executed the operator's `~/.claude/settings.json`
  ai-memory `SessionStart` hook at launch, before any conversation, with
  Cursor's payload: `conversation_id` = `session_id` = `"empty-state-draft"`,
  `cursor_version`, `composer_mode`, `is_background_agent`, empty
  `workspace_roots`, null `transcript_path`. Cursor's docs say Claude Code
  hooks load by default; this confirms it for the desktop app. The server
  relabels such a payload as `cursor`.
- Replayed against a scratch server, that event creates one `cursor`
  session with no `cwd` in `default/scratch`; each later launch adds another
  `session-start` observation to the same placeholder session.
- The empty window loaded only Cursor's built-in MCP servers, not the
  Claude config's `ai-memory`.
- No conversation has run yet; `cursor-agent` is not logged in.

## Still unverified

| Question | What would settle it |
| --- | --- |
| Does the Codex desktop app pass anything at `thread/start` that disables hooks? | One prompt in the desktop app with the trusted ai-memory hooks in place, then check the hook spool or server for a `codex` session whose rollout originator is the desktop's |
| Does the Antigravity IDE run `.agents/hooks.json` / `~/.gemini/config/hooks.json`, with the same payload as the CLI? | One IDE prompt in a scratch workspace carrying a recorder `.agents/hooks.json` |
| Do host hooks and MCP servers reach Cowork's VM on Linux (and on macOS/Windows)? | One Cowork task after the VM images download, then look for hook output in its transcript |
| Cursor: the hook set and payloads of a real desktop conversation (native and Claude-format), and where its transcript lands | One prompt in the probe workspace, which carries recorder `.cursor/hooks.json` and `.claude/settings.json` files |
| Is a per-turn Antigravity session-start acceptable, or should it be gated per conversation? | A product decision once the server-side effect of repeated session-start posts is measured |
