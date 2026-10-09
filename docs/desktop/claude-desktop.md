# Claude Desktop (research notes)

> Research for RFC #878. Verified against official docs and the local Linux
> install on the research machine (Omarchy/Arch, `~/.config/Claude`). Closed
> source. Facts below cite either a URL or a local path found on this machine.

## Product shape (as of 2026-10)

One Electron app, three tabs: **Chat** (claude.ai consumer chat), **Cowork**
(long-running agentic work / Dispatch), **Code** (Claude Code sessions).
Officially on macOS and Windows; **Linux is an official beta** (apt/.deb for
Ubuntu 22.04+ and Debian 12+ only; other distributions are pointed at the
CLI — <https://code.claude.com/docs/en/desktop>,
<https://code.claude.com/docs/en/desktop-linux>). On Linux, Cowork runs its
tasks inside a QEMU/KVM virtual machine the app hosts. The local machine runs the
Linux build (`/usr/share/applications/com.anthropic.Claude.desktop`,
Electron profile at `~/.config/Claude/`).

The **Code tab runs the same Claude Code engine as the CLI**:
"Desktop runs the same underlying engine with a graphical interface… They
share configuration and project memory via CLAUDE.md files" and "The desktop
app reads the same settings files as the CLI"
(<https://code.claude.com/docs/en/desktop>).

The desktop app ships and auto-manages its own Claude Code CLI copy:
`~/.config/Claude/claude-code/2.1.280/claude` (+ `.payload`, `.verified`
marker files) — found locally.

## MCP client support

Two distinct MCP surfaces:

1. **Chat tab (consumer app)** — local stdio MCP servers via the classic
   config file `claude_desktop_config.json` under an `mcpServers` key, plus
   remote HTTPS connectors configured in-app ("Connectors"). Config paths:
   - macOS: `~/Library/Application Support/Claude/claude_desktop_config.json`
   - Windows: `%APPDATA%\Claude\claude_desktop_config.json` (or the MSIX
     AppContainer path — ai-memory's `install-mcp --client claude-desktop`
     already detects both)
   - Linux: `~/.config/Claude/claude_desktop_config.json` (**verified
     locally**; the file exists with a `preferences` object — the Linux beta
     reuses the same file). Note ai-memory's own `docs/mcp-install.md` still
     says "Linux: not officially distributed by Anthropic" — that is now
     outdated versus the official Linux beta and should be updated in a
     follow-up docs PR.
   - Source: <https://modelcontextprotocol.io/quickstart/user> (mac/win
     paths), local file inspection, ai-memory `docs/mcp-install.md`.
   - Local MCP logs: `~/Library/Logs/Claude/mcp*.log` (mac),
     `%APPDATA%\Claude\logs` (win) — same source.
2. **Code tab (Claude Code host)** — the same MCP configuration the CLI uses:
   `~/.claude.json` (local/user scope), `.mcp.json` (project scope), settings
   files, and **plugins** (a plugin can bundle MCP servers + hooks; the
   desktop app has a plugin browser). Connectors in the prompt "+" menu are
   "MCP servers with a graphical setup flow"
   (<https://code.claude.com/docs/en/desktop#connect-external-tools>).
   Transports: stdio, SSE, HTTP (streamable), WebSocket, and an SDK-only
   in-process `type: "sdk"` reserved for host applications such as the
   desktop app (<https://code.claude.com/docs/en/mcp>).

Remote per-session MCP servers also appear in desktop session state —
`~/.config/Claude/claude-code-sessions/<account>/<org>/local_*.json` contains
a `remoteMcpServersConfig` list (verified locally; two remote servers incl.
Anthropic's own Docs MCP).

**Desktop Extensions (`.mcpb`)** are current: Anthropic's support article
presents them as the main way to install a local MCP server in Claude
Desktop, Linux included
(<https://support.claude.com/en/articles/10949351-getting-started-with-local-mcp-servers-on-claude-desktop>,
re-checked 2026-10-08, see `verification-2026-10.md`). Plugins are the Code
tab's extension surface.

## Lifecycle hooks / capture surface

- **No push hooks in the Chat/Cowork tabs.** Capture for consumer-chat
  conversations stays model-discretion (memory tool calls) — unchanged from
  RFC #878's conclusion.
- **The Code tab is full Claude Code**, and it reads the user's
  `~/.claude/settings.json` — where ai-memory's hooks are already installed
  (verified locally: `hooks.SessionStart` →
  `ai-memory ... hook --event session-start --agent claude-code ...`).
  Therefore **ai-memory already captures desktop Code sessions today**, keyed
  `agent=claude-code`, `session_id=<CLI UUID>`, `cwd=<session workspace>`.
- Code-tab sessions started without a project folder run in per-session
  scratch workspaces:
  `~/.config/Claude/scratch-workspaces/<account>/<org>/scratch-<date>-<hex>/`
  (verified locally), which show up in `~/.claude/projects/` as ordinary
  per-directory projects (verified: several
  `-home-...-Claude-scratch-workspaces-...` project dirs). Their desktop
  records sit under `claude-code-sessions/` with `envScopeId: builtin_local`
  and `scratchOfferFolder: true`; an earlier version of this note
  attributed them to Cowork, which keeps its own store under
  `~/.config/Claude/local-agent-mode-sessions/`. Cowork has not run on this
  machine (its VM images were never downloaded), so its capture is
  unverified.
- Consumer-chat transcripts are **cloud-stored**; no stable local transcript
  store for the Chat tab. Code-tab transcripts are the standard
  `~/.claude/projects/<munged-cwd>/<session-uuid>.jsonl` files.

## Session bridging (CLI ↔ Desktop) — verified mechanism

The user report "desktop catches CLI sessions and chat continues on both" is
real and observable on this machine. Mechanism, from local artifacts:

1. **Every running `claude` process registers a live descriptor** at
   `~/.claude/sessions/<pid>.json` (verified locally). Fields include:
   - `sessionId` — the Claude Code session UUID
   - `entrypoint: "claude-desktop"` when desktop-spawned
   - `hostSessionId: "local_<uuid>"` — the desktop app's own session id
   - `cwd`, `version`, `peerProtocol`, `peerFeatures`
     (`notify_idle`, `reply_across_default_dirs`, `artifact_yield`)
   - `messagingSocketPath: /run/user/<uid>/cc-socks/<pid>.sock` — a Unix
     socket used for cross-surface messaging between desktop and CLI peers.
2. **Desktop-side session records** live at
   `~/.config/Claude/claude-code-sessions/<account>/<org>/local_<uuid>.json`
   (verified locally) and carry the join key:
   - `sessionId: "local_<uuid>"` (desktop id)
   - `cliSessionId: "<CLI session UUID>"` — **the same UUID as the
     `~/.claude/projects/.../<uuid>.jsonl` transcript**
   - plus `cwd`, `originCwd`, `model`, `permissionMode`, `title`,
     `remoteMcpServersConfig`, timestamps.
3. **Explicit moves**: `/desktop` inside the CLI "saves your session and
   opens it in the desktop app, then exits the CLI"; from the shell,
   `claude --desktop [--continue | --resume <session-id>]` opens an existing
   CLI session in Desktop and the **session keeps its ID**
   (<https://code.claude.com/docs/en/desktop#coming-from-the-cli>). Both are
   documented for macOS and x64 Windows only; on any platform, `/resume`
   inside a local Desktop session picks up a CLI session from this
   computer, again continuing the same session.
4. **Deep links**: `claude-cli://` URL scheme handled by
   `~/.local/share/applications/claude-code-url-handler.desktop` →
   `claude --handle-uri %u` (verified locally) — how other apps hand a
   session to Claude Code.
5. Desktop's "work across sessions" surface only sees sessions the desktop
   app itself runs (local/SSH/WSL), **not** terminal CLI or VS Code extension
   sessions; cross-session messaging can additionally reach terminal Claude
   Code sessions (same doc).

### What ai-memory's `(agent, session_id)` key sees

- A desktop Code session and the CLI session it came from (via
  `--resume`/`/desktop`) are **one session id** — capture dedups naturally.
- A desktop Code session started without a folder is a *new* CLI session
  (new UUID) whose cwd is a fresh scratch workspace; ai-memory sees it as a
  claude-code session in an auto-named project derived from the scratch
  path. Cowork sessions are a separate, unverified case (see above). The desktop↔CLI mapping
  (`local_* ↔ cliSessionId`) is available on disk if we ever want to join or
  relabel desktop-originated sessions (see `capture-and-dedup.md`).
- The `entrypoint: "claude-desktop"` marker in `~/.claude/sessions/<pid>.json`
  is a reliable way to distinguish desktop-spawned sessions post-hoc while
  the process lives; it is transient (pid-keyed), not archival.

## Open-source status

- Claude Desktop app: **closed** (Electron).
- Claude Code CLI: distributed via npm/`~/.local/bin/claude`; the GitHub repo
  `anthropics/claude-code` hosts docs/issues, not full open source. The
  desktop app's bundled copy (`~/.config/Claude/claude-code/2.1.280/claude`)
  is the same distribution.

## Sources

- <https://code.claude.com/docs/en/desktop> (desktop app reference, CLI
  comparison, `/desktop`, settings sharing)
- <https://code.claude.com/docs/en/mcp> (Claude Code MCP reference)
- <https://modelcontextprotocol.io/quickstart/user> (claude_desktop_config
  paths, mcp logs)
- <https://code.claude.com/docs/en/desktop-linux> (Linux beta)
- Local: `~/.config/Claude/` (config, sessions, scratch workspaces, bundled
  CLI), `~/.claude/sessions/`, `~/.claude/projects/`,
  `~/.local/share/applications/claude-code-url-handler.desktop`
- ai-memory repo: `docs/mcp-install.md` (claude-desktop section), RFC #878.
