# Desktop support — phased plan (research output, reconciled with RFC #878)

> This file reconciles RFC #878 (github.com/akitaonrails/ai-memory#878) with
> the findings in this directory (2026-10-07). Nothing here is scheduled
> work; each phase lists what would need to change and where the risk sits.

## What the research changed vs the RFC

RFC #878's headline, "connection is solved, capture is the crux", still
holds, but two of its premises moved:

1. **"Most desktop chat apps expose no hook/event surface"** is now false
   for the *coding* desktop surfaces: Claude Desktop's Code tab is
   Claude Code with the user's hooks (captured today, zero code; Cowork
   unverified), Codex desktop shares `~/.codex` hooks config (its engine
   verified), and **Antigravity
   (IDE included) has first-class lifecycle hooks** with transcript paths
   in the payload. The truly hookless set is now: Claude Desktop Chat tab,
   hosted ChatGPT chat, Grok Bot.
2. **"Claude Desktop .mcpb bundle"** as the Phase-1 one-click route:
   re-validated 2026-10-08. Anthropic's support article presents `.mcpb`
   desktop extensions as the main way to install a local MCP server in the
   desktop app. Plugins (marketplace; can bundle MCP servers + hooks) are
   the Code tab's extension surface.

## Phase 0 — docs + verify the free wins (ship anytime)

Status 2026-10-08: docs done, verification mostly done; results in
[`verification-2026-10.md`](verification-2026-10.md).

1. Update `docs/mcp-install.md`: **done**. Covers the Linux beta and its config path
   (with the `--config-file` the CLI needs there), the Code tab's capture
   through Claude Code hooks, `.mcpb` status, the Cowork boundary.
2. Live verifications (machine with the apps installed):
   - [x] Codex desktop engine: `codex app-server` fires trusted hooks with
     the CLI payload; untrusted/modified hooks are skipped silently;
     `SessionEnd` on archive or shutdown.
   - [ ] Codex desktop app: one in-app prompt with the ai-memory hooks in
     place.
   - [x] Antigravity CLI: workspace hooks, per-turn `invocationNum` reset,
     transcript layout.
   - [ ] Antigravity IDE: one prompt in a workspace with a recorder
     `.agents/hooks.json`.
   - [x] Claude Desktop Code tab: hooks fire (from transcripts).
   - [ ] Claude Desktop Cowork: one task once the VM is available.
   - [x] Cursor desktop: runs `~/.claude/settings.json` hooks by default
     (a launch-time `sessionStart` for a draft composer was captured).
   - [ ] Cursor desktop: one conversation in the probe workspace.
3. Document that Claude Desktop Code-tab sessions already flow into
   ai-memory, including the scratch-workspace project naming behavior:
   **done** (`docs/mcp-install.md`, `docs/support-matrix.md`).

## Phase 1 — Antigravity IDE capture parity (smallest new-code step)

The research's best new target, ahead of the RFC's `.mcpb`:

- Ensure `install-hooks --agent antigravity-cli` output also satisfies the
  IDE (same global file; verify; add `.agents/hooks.json` workspace install
  option).
- Map hook payloads → `/hook` observations: agent kind `antigravity`
  (covers CLI+IDE+2.0), session = `conversationId`, cwd =
  `workspacePaths[0]`, tool events from PreToolUse/PostToolUse; session
  boundaries from PostInvocation/Stop flushes (no SessionStart/End events).
- Tests: payload-mapping unit tests against the documented schema
  (camelCase fields; `<app_data_dir>` variants per surface).

## Phase 2 — scope & dedup polish for desktop-originated sessions

- Decide scratch-workspace routing: single `desktop/claude-scratch` project
  vs per-scratch projects with a `claude-desktop` tag (see
  `capture-and-dedup.md` §2). This is the only dedup work the research
  found necessary: CLI↔Desktop handoffs keep one session id, so no
  id-level dedup layer is needed.
- Optional metadata enrichment from
  `~/.config/Claude/claude-code-sessions/**/local_*.json` (titles,
  account/org). Strictly optional, since these are closed-app internals.

## Phase 3 — explicit capture for hookless surfaces (RFC Phase 1, adjusted)

- Tool-surface change (23-tool count, MEMORY_INSTRUCTIONS/SNIPPET_BODY,
  prompt-surface regression tests, adversarial boundary test) for an
  explicit capture tool the model calls on apps without hooks (Claude Chat
  tab, Grok Bot, hosted ChatGPT chat). Scoping per RFC: fixed
  `personal/desktop-chat` (optionally per-app) since no cwd exists.
- Grok Bot MCP entry format: only worth reverse-engineering on demand.

## Phase 4 — the non-dev tray installer (RFC Phase 2, refined)

- Tauri v2 vs egui decision, packaging matrix, and the coexistence rules in
  `installer-and-coexistence.md` (detect-before-spawn server governance,
  `server_profiles` inheritance, native-runner path stability, no PATH
  stomping).
- Signing/notarization costs as in RFC #878 (unchanged).

## Explicitly deferred / rejected

- Hosted-ChatGPT plugin (remote MCP to a user-run server): real but a new
  distribution surface; needs published-plugin review requirements
  (<https://developers.openai.com/plugins/deploy/app-review>). Separate
  RFC if demand appears.
- Watching Zed's native-agent database: fragile, version-locked; ACP
  agents already give us capture for free.
- Building on `~/.claude/sessions/<pid>.json` cc-socks IPC or Grok Bot's
  local-exec daemon: closed-app internals, unstable by definition.
- A session-id mapping/alias table keyed on desktop `local_*` ids: not
  needed given one-id sessions; revisit only if Anthropic splits ids.

## Open questions (research-level, blocking nothing)

1. Codex-in-app hook firing: engine verified; one in-app prompt left.
2. Antigravity IDE honoring global hooks (Phase 0 verification).
3. `.mcpb` vs plugins for Claude Desktop one-click: resolved. `.mcpb` is
   Anthropic's current route for local MCP servers in the desktop app.
4. Scratch-workspace scoping product decision (Phase 2).
5. Tray stack choice + Flatpak feasibility (Phase 4).
6. Does the Claude Desktop Chat tab ever gain a local store/export? (No
   current path; re-check quarterly.)
7. Antigravity's SessionStart mapping fires on every user turn
   (`invocationNum` restarts per turn): keep, or gate per conversation?
8. Cowork capture on Linux, where tasks run in a VM.
9. Cursor runs the operator's Claude Code hooks by default, so a Cursor
   session reaches ai-memory twice when Cursor's own hooks are installed
   (the #721 de-duplication covers that) and once otherwise; the
   launch-time `empty-state-draft` session start needs handling.
10. Extensions (`extensions.md`): a Claude plugin loads hooks in Cowork but
    not in Chat, and local MCP servers only in Cowork-on-your-computer and
    Claude Code; Codex, Cursor and Antigravity plugins can bundle hooks and
    MCP. Distribution could move from writing config files to marketplace
    plugins; the server itself still needs a separate install. Design
    proposal: RFC #1166.
