# Grok desktop surfaces (research notes)

> Research for RFC #878. "Grok desktop" on this machine is two different
> things, both verified locally: (1) a browser webapp launcher for grok.com,
  and (2) **Grok Bot**, a real native desktop agent from Cursor. xAI's own
> official desktop app status is also noted.

## 1. Grok webapp launcher (local `Grok.desktop`)

`~/.local/share/applications/Grok.desktop` →
`Exec=omarchy-launch-webapp https://grok.com/`. This is an Omarchy browser
wrapper, not a native app, so there is nothing to integrate beyond grok.com's
own web features. Remote MCP support on grok.com is **unverified**: xAI docs
were not fetched, so do not assume it.

## 2. Grok Bot — Cursor's desktop agent (native, closed)

`/usr/share/applications/grok-bot.desktop` → `/usr/bin/grok-bot`, pacman
package `grok-bot-bin 0.66.0-1` (AUR; local install). Verified facts:

- **Distributed by Cursor**: the AUR PKGBUILD pulls
  `https://downloads.cursor.com/grokbot/stable/<commit>/linux/x64/grok-bot_<ver>_amd64.deb`;
  package URL/keywords say cursor.com; license `LicenseRef-Proprietary`;
  installs to `/opt/Grok Bot` (Electron). Formerly/probably codenamed
  "sand" (`provides=sand`, `sand-*` config files). AUR:
  <https://aur.archlinux.org/packages/grok-bot-bin>.
- **Config home**: `~/.grokbot/` (asar strings `.grokbot`,
  `.grokbot-data-root-v1`; verified `~/.grokbot/settings.json` locally) and
  Electron profile at `~/.config/Grok Bot/` (Local Storage leveldb, sentry,
  `dune-reliability` telemetry, `Partitions`, `attachment-image-cache`,
  `plugin-logo-cache`).
- **MCP support: yes, embedded.** `strings /opt/Grok Bot/resources/app.asar`
  shows full MCP machinery (`mcpServers`, `mcpConfigJson`, `McpServer`,
  `McpExec`, `McpAccount`, `mcp-app`, `McpCustomInstructions`,
  `mcpDisabledToolsByServerId`, …), and `~/.grokbot/settings.json` has
  `mcpBoxServers: []`, `mcpCustomInstructions`,
  `mcpCustomInstructionsByServerId`, `mcpDisabledToolsByServerId` keys
  (empty, because the user hasn't configured any). "Box servers" suggests MCP
  servers run inside Grok Bot's sandboxed "boxes". **No public official docs
  found** (cursor.com product pages were not fetched; Grok Bot appears to be
  invite/rollout-gated). The config format, and whether stdio, remote, or both
  are supported, is **unverified**. Next step if pursued: add an MCP entry
  via the app UI and diff `~/.grokbot/settings.json`.
- **Local-exec daemon**: `~/.grokbot/local-exec-daemon.log`,
  `local-exec-supervisor.json`, `local-exec-daemon-credential.json`,
  `gateway-descriptor.json`. The agent executes tasks locally through a
  supervised daemon. This matters only if we need to coexist with it; it is
  not an integration surface.
- **No lifecycle-hook surface** documented or evident; no transcript store
  documented. Conversation data appears cloud-side (leveldb holds UI state
  only). Capture options collapse to model-discretion via MCP tools, the
  same as RFC #878's closed-app bucket.

## 3. xAI official desktop app

Not verified. No local install; no official docs fetched. Grok Build CLI
(xAI's coding CLI) is a *different* product, and ai-memory already supports it
(`install-mcp --client grok`, `install-hooks --agent grok`, repo
`docs/mcp-install.md`). Keep the two separate in any plan.

## Open-source status

Grok Bot: **closed** (Cursor). No repo to clone. Only external artifact
inspected: the AUR PKGBUILD + local binary/asar strings.

## Sources

- <https://aur.archlinux.org/packages/grok-bot-bin> (PKGBUILD, cursor.com
  download URL, comments)
- Local: `/opt/Grok Bot/resources/app.asar` (strings), `~/.grokbot/`,
  `~/.config/Grok Bot/`, `/usr/share/applications/grok-bot.desktop`
- ai-memory repo: `docs/mcp-install.md` (Grok Build CLI section, a separate
  product)
