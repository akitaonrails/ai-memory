# ChatGPT desktop (research notes)

> Research for RFC #878. Two different things get called "ChatGPT desktop"
> in late 2026; both are covered. The local machine has no official ChatGPT
> desktop app. Its `ChatGPT.desktop` is an Omarchy webapp launcher
> (`Exec=omarchy-launch-webapp https://chatgpt.com/`), a browser wrapper
> rather than the app, so this research rests on official docs.

## 1. The ChatGPT desktop app (Codex host) — official

As of 2026-10 OpenAI ships one desktop app that hosts both ChatGPT
(chat/Work) and Codex (coding agent): "Download ChatGPT for macOS,
Windows, or Linux… Choose ChatGPT or Codex. In ChatGPT, use the toggle above
the composer to select Chat or Work" (<https://developers.openai.com/codex/app>).
All MCP/hook mechanics of this app are covered in
[`codex-desktop.md`](codex-desktop.md). In summary:

- **MCP: yes** (stdio + streamable HTTP), configured in
  `~/.codex/config.toml` `[mcp_servers.*]` or Settings → MCP servers; config
  shared with Codex CLI and IDE extension.
- **Lifecycle hooks: yes** (Codex hooks framework, `~/.codex/hooks.json`) on
  the Codex side. Whether hooks fire for Chat-mode conversations is **not
  documented**: Chat/Work conversations are cloud-thread-shaped, and the
  hooks doc speaks of Codex sessions/turns. Treat Chat-mode auto-capture as
  unavailable and Codex-mode capture as available (verify live).
- **Transcripts**: Codex-mode sessions land in `~/.codex/sessions/`
  rollout JSONL on the Codex host. Chat-mode conversations are cloud-stored;
  no local transcript store documented.

## 2. ChatGPT web/chat surfaces — connectors (remote MCP only)

- "ChatGPT web can use remote MCP-backed tools supplied by plugins. Local
  Codex clients can also connect directly to MCP servers… ChatGPT web
  doesn't read local Codex configuration files"
  (<https://developers.openai.com/codex/extend/mcp>).
- The extension surface is plugins: MCP server (+ optional UI, events,
  skills) published to the ChatGPT plugin directory
  (<https://developers.openai.com/plugins>). Auth is OAuth/Sign-in-with-
  ChatGPT; no stdio/local servers in hosted Chat.
- Plugins/connections are administered per workspace for ChatGPT Work
  (<https://developers.openai.com/codex/enterprise/apps-and-connectors>).
- **Implication for ai-memory:** an ai-memory *plugin* (hosted remote MCP
  pointing at a user-run server) is the only sanctioned path into hosted
  ChatGPT chat, which matches RFC #878's "remote-only connectors"
  conclusion. A local loopback ai-memory server cannot be reached from
  hosted Chat; a tunnel (e.g. OpenAI's "Secure MCP Tunnel" docs exist at
  <https://developers.openai.com/api/docs/guides/secure-mcp-tunnels>) or a
  hosted deployment would be required. Not researched further here.

## Linux status

The official app now supports Linux
(<https://developers.openai.com/codex/linux/linux-app>); historically Linux
users only had web/wrappers (as on this machine). The AUR `codex-desktop`
family (distsystem fork) predates/parallels official Linux availability;
see [`codex-desktop.md`](codex-desktop.md).

## Capture / dedup notes

- Chat-mode: no hooks, no local transcripts → model-discretion memory tool
  calls only (option (a) in RFC #878) unless a hosted plugin is built.
- Codex-mode in the desktop app: same store/session identity as Codex CLI
  (`session_id` from hooks/rollouts under `~/.codex/sessions/`), so
  ai-memory's `(agent=codex, session_id)` keying applies unchanged.
- No documented CLI↔desktop session handoff (unlike Claude's `/desktop`).

## Sources

- <https://developers.openai.com/codex/app>,
  <https://developers.openai.com/codex/extend/mcp>,
  <https://developers.openai.com/codex/linux/linux-app>,
  <https://developers.openai.com/plugins>,
  <https://developers.openai.com/api/docs/guides/secure-mcp-tunnels>
- Local: `~/.local/share/applications/ChatGPT.desktop` (Omarchy webapp
  launcher, which shows no official app is installed here)
