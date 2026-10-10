# Installer & CLI coexistence for a desktop companion (research)

> Research for RFC #878. What it takes to ship an `ai-memory` tray app that
> a non-developer can install, and how it coexists with an existing
> `ai-memory` CLI install without breaking it. Verified sources: Tauri v2
> docs (fetched 2026-10), the existing Swift companion
> (`companions/ai-memory-macos/README.md`), ai-memory repo layout
> (`packaging/`, `crates/ai-memory-cli/src/server_profiles*`, hook runner
> layout observed locally), and well-known behavior of Docker Desktop /
> Tailscale (pattern references; deep docs not fetched, so treat as
> directional).

## 1. Tray-app implementation options

### Option A — Tauri v2 (recommended baseline)

- **Tray**: first-class `tray-icon` feature (`TrayIconBuilder`, menus,
  events). Linux note: tray mouse-move/enter events unsupported, but icon +
  context menu work (<https://v2.tauri.app/learn/system-tray/>).
- **Bundling the Rust binary**: sidecar/external-binary support
  (<https://v2.tauri.app/develop/sidecar/>), so the tray app can embed the
  exact pinned `ai-memory` release binary.
- **Desktop-app table stakes as plugins**: `single-instance` (avoid two
  trays), `autostart` (login launch), `updater` (signed updates),
  deep-linking (`ai-memory://`) (<https://v2.tauri.app/plugin/>).
- **Packaging matrix from one toolchain**, with official distribute targets:
  Linux deb / rpm / AppImage / AUR / Flathub / Snap, macOS DMG + app bundle
  (+ pkg), Windows installer (NSIS/MSI) + Microsoft Store; per-OS signing
  guides (<https://v2.tauri.app/distribute/>).
- Cost: webview-based UI (small), Rust core is fine; maturities match our
  Rust-first stack.

### Option B — pure Rust GUI: egui + `tray-icon`

- `tray-icon` (tauri-apps org, <https://docs.rs/tray-icon>) + `muda` menus;
  egui for the settings window. Lightest stack, no webview, one binary.
- Cost: we own everything Tauri gives for free: single-instance, updater,
  signing/packaging pipelines, deep links. Only worth it if the UI stays
  trivial (menu + one settings pane), which the Swift companion's scope
  suggests it might.

### Option C — keep Swift companion + add a Linux counterpart

- macOS: `companions/ai-memory-macos` already works (ships runtime, governs
  LaunchAgent, data dir survives app replacement, philosophy: "wrapper, not
  a second operator console").
- Linux: a counterpart would duplicate the service governance (systemd user
  unit; templates already exist in `packaging/systemd/`) with a GTK/libadw
  or Tauri menu. Two codebases for one product; the existing companion
  explicitly defers notarization/Homebrew cask, so it has not reached the
  non-dev bar yet either.

**Recommendation (research-level):** Tauri v2 for a cross-platform tray
companion that *subsumes* the Swift companion's role over time, keeping the
Swift app as the macOS-first path until parity; OR B if we commit to a
minimal menu-only surface. Decision point for RFC follow-up, not settled
here.

## 2. Packaging per OS (end-user installers)

| OS | Formats | Notes |
| --- | --- | --- |
| macOS | `.dmg` (drag-install), `.pkg`, Homebrew cask | Gatekeeper: Developer ID + hardened runtime + notarize/staple (RFC #878 cost analysis stands); Swift companion currently ships un-notarized |
| Windows | NSIS or MSI (WiX) via Tauri; winget | MSIX optional; AppContainer path quirks for Claude Desktop config already handled by `install-mcp`; Authenticode signing (RFC #878: SignPath Foundation free for OSS) |
| Linux | `.deb`, `.rpm`, AppImage; AUR (we already ship AUR assets in `packaging/aur/`); Flatpak is **problematic** (sandbox vs loopback server + hook execution into arbitrary agent configs; needs portal work, so defer) | systemd user unit from `packaging/systemd/`; sysusers/tmpfiles assets exist |

## 3. Coexistence with the existing CLI (the hard constraints)

Ground truth from the repo/local machine:

- **Data dir** holds wiki + SQLite + `config.toml` + spool + models:
  `~/.local/share/ai-memory` (Linux), `~/Library/Application Support/ai-memory`
  (macOS, the Swift companion's documented default), `%LOCALAPPDATA%`
  (Windows). The desktop app MUST detect and reuse it, never fork a second
  one silently.
- **One writer, one server**: ai-memory's SQLite is single-writer-actor by
  design (repo invariants #2, #9). Two server processes on one data dir is
  forbidden, so the tray app's first job is detection rather than spawning:
  1. Probe the configured/default server (`GET /admin/status` on the port
     from `config.toml` / default `127.0.0.1:49374`).
  2. Check the **`server_profiles` registry** (CLI:
     `crates/ai-memory-cli/src/server_profiles.rs`: named profiles with
     url + project roots + token, used by hooks via `--server-url`). On this
     machine the hooks point at a LAN server (`--server-url
     http://192.168.0.90:49374`), and the desktop app must respect that
     instead of assuming localhost.
  3. Only if no reachable server: offer (a) start bundled server as a
     managed service (LaunchAgent/systemd user unit/Startup task, reusing
     `packaging/` templates), or (b) connect to remote with URL+token.
- **Hooks are decoupled from app bundles already**: hooks installed by
  ai-memory invoke `~/.local/share/ai-memory/native-runner/ai-memory` (the
  native-runner copy inside the data dir; verified in local
  `~/.claude/settings.json` and `~/.codex/hooks.json`). Replacing a desktop
  app bundle therefore never rewrites hooks or breaks capture. Preserve
  this invariant: **the tray app must not move or duplicate the
  native-runner path**; at most refresh it with the user's consent.
- **Version independence**: bundled binary (in the .app/resources) and any
  system CLI can differ. Rules: the *server* version wins schema truth
  (migrations run under the single-writer server); the tray UI talks HTTP
  only (`/admin/status`, `/web`, `/api/v1`) so it degrades gracefully
  against older/newer servers; `ai-memory` CLI on PATH is never overwritten
  by the app (no symlink stomping).
- **Auto-wiring MCP configs**: reuse `install-mcp` per client (it already
  knows claude-desktop incl. MSIX paths, codex, antigravity-cli, zed, grok,
  …). The desktop app should call the same library code, show a checklist
  of detected apps, and write configs idempotently (preserve unrelated
  servers, as it already does).

## 4. The "desktop users don't know Docker" constraint

ai-memory today is a self-contained Rust binary (bundled SQLite, vendored
libgit2), so **Docker is already optional** (`docs/install.md` ships native
packages + systemd). The tray app completes that story:

- Default path: bundled binary + local data dir + loopback bind + zero
  auth (single-user posture from `docs/install.md`).
- Team/remote path: URL + bearer token (AI_MEMORY_AUTH_TOKEN) fields in the
  tray settings → written to `server_profiles` so hooks and MCP installs
  point at the same remote.
- Anti-goal: the tray app becoming an admin console. Keep the Swift
  companion's rule: open `/web` and the config file; don't duplicate.

## 5. Comparable tools (pattern reference)

- **Docker Desktop**: bundles engine + CLI, manages the VM/service, and
  exposes `docker` CLI on PATH from the app bundle; starting the app is
  starting the service. Pattern to copy: tray == service governor, CLI
  stays first-class. Pattern to avoid: version-lock pain when the desktop
  app lags the CLI.
- **Tailscale**: menu-bar app + `tailscaled` system service + `tailscale`
  CLI; on macOS the daemon ships inside the .app with a launchd agent, and
  the CLI is a symlink into the bundle. Pattern to copy: explicit
  "connected/backend state" tray affordance; CLI/daemon version skew
  tolerated by protocol.
- **Raycast**: tray-first launcher without a bundled background service.
  It is mostly useful as a UI scope reference (menu + extensions), not for
  service coexistence.

(These are well-known public behaviors; not re-verified against docs in
this pass, so treat them as directional only.)

## 6. Open questions

1. Tauri vs egui decision (scope of UI: menu-only vs settings panes vs
   conversation viewer).
2. Flatpak feasibility (portals for background service + host file access
   to agent configs) or skip Flatpak and ship deb/rpm/AppImage/AUR only.
3. Auto-update channel policy for the bundled server binary vs CLI-managed
   installs (Tauri updater is for the app; the embedded `ai-memory`
   sidecar needs its own update story).
4. Whether the tray app should also own `install-hooks` for detected
   agents (it is the natural place for the "enable capture" toggle).
