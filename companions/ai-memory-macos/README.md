# AI Memory (macOS menu bar)

Self-contained macOS accessory app. It ships the `ai-memory` runtime, manages the LaunchAgent, and opens the surfaces the tool already has. It is a wrapper around those tools and does not add a second operator console.

This companion is not a root Cargo workspace member. Durable memory stays in the user data directory so replacing the `.app` does not rewrite wiki, SQLite, config, models, or logs.

| In the `.app` (replaceable) | In the user data dir (survives updates) |
|---|---|
| Swift menu bar UI | wiki, SQLite, `config.toml` |
| `ai-memory` binary | hook spool, `auth.json`, capture-mode |
| bundled `hooks/` | downloaded embedding models |
| LaunchAgent template | rendered plist in `~/Library/LaunchAgents/` |
| | logs in `~/Library/Logs/ai-memory/` |

Data directory: `~/Library/Application Support/ai-memory` (the binary's existing macOS default). Optional override in Settings writes `AI_MEMORY_DATA_DIR` into the LaunchAgent plist only.

The app does not replace `ai-memory status`, `/web`, or hand-editing `config.toml`. You keep using those; the menu opens them.

## Build

From the repository root (needs a Rust toolchain and Xcode / Swift 6):

```bash
chmod +x companions/ai-memory-macos/build.sh
./companions/ai-memory-macos/build.sh
open "companions/ai-memory-macos/dist/AI Memory.app"
```

`build.sh` compiles `ai-memory` with Cargo, compiles the Swift menu extra, and stages:

```text
AI Memory.app/Contents/Resources/runtime/
  ai-memory
  hooks/
  packaging/launchd/com.github.akitaonrails.ai-memory.plist
```

Drag the `.app` to `/Applications` for a stable LaunchAgent path. Notarization, Developer ID, and a Homebrew cask are out of scope for this companion's first version.

## Use

1. Open the app (menu bar extra; no Dock icon).
2. **Install & Start Server** runs the bundled `ai-memory init` if `config.toml` is missing, renders the existing launchd template, and `launchctl bootstrap`s `com.github.akitaonrails.ai-memory`.
3. The status item turns green when `GET /admin/status` succeeds.
4. **Open Web UI**, **Show Status…** (bundled `ai-memory status`), **Open Config**, **Open Data Directory**, **Open Logs**.

Updates: replace `/Applications/AI Memory.app`. The data dir is untouched. If the helper path inside the bundle changed, **Restart Server** re-renders the plist.

## Tests

```bash
swift test --package-path companions/ai-memory-macos
```

Root `cargo t` / `cargo tf` do not cover this package.

## Open in Xcode

Open `companions/ai-memory-macos/Package.swift`. `swift run` from the package directory will not include the staged runtime; use `build.sh` (or set `AI_MEMORY_MENU_RUNTIME` at the tarball-equivalent `runtime/` directory) to manage the service.
