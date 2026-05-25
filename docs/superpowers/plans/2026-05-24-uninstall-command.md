# Uninstall Command Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `ai-memory uninstall` — the symmetric inverse of `install-hooks` / `install-mcp` / `install-instructions` — that detects and removes ai-memory's wiring across all agents, optionally purges data via the `reset` path, and prints (never executes) a Docker teardown hint.

**Architecture:** A single local (non-HTTP) subcommand in `commands/uninstall.rs`. The detection/removal logic is a set of **pure functions over file content** (string in → string out + a list of removed items), so the hard parts are unit-testable without the filesystem. The orchestrator resolves per-agent config paths (helpers extracted into the `install_*` modules), reads each file, runs the matching pure function, and writes back through the existing `apply_shared::apply_atomic` (atomic + timestamped backup). Hook entries are matched by the unconditional `AI_MEMORY_HOOK_URL=` prefix install inlines into the command; MCP servers by name **or** endpoint; the instructions block by its `<!-- ai-memory:start/end -->` markers.

**Tech Stack:** Rust 2024, `clap` derive, `serde_json`, `toml_edit`, `anyhow`, `sysinfo` (via `process_guard`), `tempfile` (via `apply_shared`). Spec: `docs/superpowers/specs/2026-05-24-uninstall-command-design.md`.

---

## File structure

| File | Responsibility | Change |
|---|---|---|
| `crates/ai-memory-cli/src/commands/uninstall.rs` | Pure detection/removal fns + typed plan + orchestrator `run` | **Create** |
| `crates/ai-memory-cli/src/commands/mod.rs` | Module registry | Modify: add `pub mod uninstall;` |
| `crates/ai-memory-cli/src/cli.rs` | `Command::Uninstall`, `UninstallArgs`, `UninstallOnly` | Modify |
| `crates/ai-memory-cli/src/main.rs` | Dispatch arm | Modify (~line 63) |
| `crates/ai-memory-cli/src/commands/install_hooks.rs` | Extract `pub(crate)` per-agent hook-config path helpers | Modify |
| `crates/ai-memory-cli/src/commands/install_mcp.rs` | Extract `pub(crate) fn mcp_config_path(client)` | Modify |
| `tests/` (workspace) or `crates/ai-memory-cli/tests/uninstall.rs` | Integration test of the orchestrator on a temp HOME | **Create** (Task 11) |
| `CHANGELOG.md` | Note the new command | Modify (Task 11) |

All pure functions and their unit tests live in `commands/uninstall.rs` under `#[cfg(test)] mod tests`. The orchestrator depends only on those pure fns + `apply_shared` + `process_guard` + the path helpers.

---

## Task 1: Scaffold module, plan types, and the instructions stripper

The instructions block is the simplest removal (pure string surgery against the markers) and a good first pure function. It also forces the module to exist and compile.

**Files:**
- Create: `crates/ai-memory-cli/src/commands/uninstall.rs`
- Modify: `crates/ai-memory-cli/src/commands/mod.rs`

- [ ] **Step 1: Register the module**

In `crates/ai-memory-cli/src/commands/mod.rs`, add the line in alphabetical position (after `setup_agent` or wherever it fits the existing order):

```rust
pub mod uninstall;
```

- [ ] **Step 2: Write the failing test for the instructions stripper**

Create `crates/ai-memory-cli/src/commands/uninstall.rs` with:

```rust
//! `ai-memory uninstall` — the symmetric inverse of install-hooks /
//! install-mcp / install-instructions. Detects ai-memory's wiring in
//! every supported agent's config and removes only that, never
//! third-party entries. Optional `--purge-data` wipes wiki/db/raw via
//! the reset path. Docker teardown is printed, never executed.
//!
//! Design: docs/superpowers/specs/2026-05-24-uninstall-command-design.md

use ai_memory_core::{MARKER_END, MARKER_START};

/// Remove the `<!-- ai-memory:start -->`…`<!-- ai-memory:end -->`
/// block (inclusive) from a CLAUDE.md / AGENTS.md. Returns the new
/// content and whether a block was found. Inverse of
/// `install_instructions::merge_instructions_block`: an install
/// followed by an uninstall round-trips to the original file.
fn strip_instructions_block(content: &str) -> (String, bool) {
    let Some(start) = content.find(MARKER_START) else {
        return (content.to_string(), false);
    };
    let Some(end_rel) = content[start..].find(MARKER_END) else {
        return (content.to_string(), false);
    };
    let end = start + end_rel + MARKER_END.len();
    // Consume a trailing newline after the end marker if present.
    let after = if content.as_bytes().get(end).copied() == Some(b'\n') {
        end + 1
    } else {
        end
    };
    let mut head = content[..start].to_string();
    let tail = &content[after..];
    // When the block sat at EOF, install added a blank-line separator
    // before it; drop that artifact so install→uninstall round-trips.
    if tail.is_empty() && head.ends_with("\n\n") {
        head.pop();
    }
    (format!("{head}{tail}"), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_instructions_round_trips_with_install_append() {
        let original = "# Title\n";
        // Mirror install_instructions::merge append behavior:
        let block = format!("{MARKER_START}\nBODY\n{MARKER_END}\n");
        let installed = format!("{original}\n{block}");
        let (stripped, found) = strip_instructions_block(&installed);
        assert!(found);
        assert_eq!(stripped, original, "uninstall must restore the original file");
    }

    #[test]
    fn strip_instructions_preserves_surrounding_content() {
        let content = format!("# Top\n\n{MARKER_START}\nBODY\n{MARKER_END}\n\nMore notes.\n");
        let (stripped, found) = strip_instructions_block(&content);
        assert!(found);
        assert!(stripped.contains("# Top"));
        assert!(stripped.contains("More notes."));
        assert!(!stripped.contains("BODY"));
        assert!(!stripped.contains(MARKER_START));
    }

    #[test]
    fn strip_instructions_no_block_is_noop() {
        let content = "# Just a readme\n";
        let (stripped, found) = strip_instructions_block(content);
        assert!(!found);
        assert_eq!(stripped, content);
    }
}
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p ai-memory-cli uninstall::tests::strip_instructions -- --nocapture`
Expected: 3 tests PASS. (`MARKER_START`/`MARKER_END` are re-exported from `ai_memory_core` — verified in `routing_snippet.rs`.)

- [ ] **Step 4: Verify it compiles clean**

Run: `cargo clippy -p ai-memory-cli --all-targets -- -D warnings`
Expected: no warnings. (`run` doesn't exist yet — that's fine; the dispatch arm comes in Task 7.)

- [ ] **Step 5: Commit**

```bash
git add crates/ai-memory-cli/src/commands/uninstall.rs crates/ai-memory-cli/src/commands/mod.rs
git commit -m "feat(uninstall): module scaffold + instructions-block stripper"
```

---

## Task 2: Hook command signature predicate

The single most important detection rule: a hook entry is ai-memory's iff its command carries the `AI_MEMORY_HOOK_URL=` prefix (inlined unconditionally by install — `render_shared.rs:239`, present even with no auth).

**Files:**
- Modify: `crates/ai-memory-cli/src/commands/uninstall.rs`

- [ ] **Step 1: Write the failing tests**

Add to `uninstall.rs` (above the `#[cfg(test)]` module, add the fn; add the tests inside the module):

```rust
/// True when a hook command string was written by ai-memory. Install
/// inlines `AI_MEMORY_HOOK_URL=<url> [AI_MEMORY_AUTH_TOKEN=…] <path>`
/// into the command (render_shared.rs); the `AI_MEMORY_HOOK_URL=`
/// prefix is unconditional, so it is the reliable signature —
/// independent of auth, --server-url, --hooks-dir, --host-prefix.
fn hook_command_is_ours(command: &str) -> bool {
    command.contains("AI_MEMORY_HOOK_URL=")
}
```

Inside `mod tests`:

```rust
#[test]
fn hook_signature_matches_no_auth_default() {
    let cmd = "AI_MEMORY_HOOK_URL=http://127.0.0.1:49374 /home/u/.local/share/ai-memory/hooks/claude-code/stop.sh";
    assert!(hook_command_is_ours(cmd));
}

#[test]
fn hook_signature_matches_with_auth_and_custom_prefix() {
    let cmd = "AI_MEMORY_HOOK_URL=http://lan:49374 AI_MEMORY_AUTH_TOKEN=abc /etc/custom/session-start.sh";
    assert!(hook_command_is_ours(cmd));
}

#[test]
fn hook_signature_rejects_third_party_with_generic_name() {
    // A user's own hook that happens to be named stop.sh — no prefix.
    assert!(!hook_command_is_ours("/usr/local/bin/my-stop.sh"));
    assert!(!hook_command_is_ours("/opt/tools/hooks/session-start.sh"));
}
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p ai-memory-cli uninstall::tests::hook_signature`
Expected: 3 tests PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/ai-memory-cli/src/commands/uninstall.rs
git commit -m "feat(uninstall): hook command signature predicate"
```

---

## Task 3: Hook JSON stripper (both shapes, prune empties)

Removes ai-memory entries from a hooks JSON file. Handles Nested (Claude/Codex/Gemini: `{matcher, hooks:[{command}]}`) and Flat (Cursor: `{command, matcher}`) shapes by inspecting each entry. Prunes an event key when its array empties, and the `hooks` object when it empties.

**Files:**
- Modify: `crates/ai-memory-cli/src/commands/uninstall.rs`

- [ ] **Step 1: Write the failing tests**

Add the functions to `uninstall.rs`:

```rust
use anyhow::Result;
use crate::commands::apply_shared::mutate_json;

/// Result of stripping ai-memory entries from a hooks JSON file.
struct HookRemoval {
    new_content: String,
    removed_events: Vec<String>,
}

/// An entry (one element of an event's array) is ai-memory's when its
/// command carries the signature — at the entry level (Flat shape) or
/// inside its nested `hooks` array (Nested shape).
fn hook_entry_is_ours(entry: &serde_json::Value) -> bool {
    if let Some(cmd) = entry.get("command").and_then(|c| c.as_str())
        && hook_command_is_ours(cmd)
    {
        return true;
    }
    if let Some(inner) = entry.get("hooks").and_then(|h| h.as_array()) {
        return inner.iter().any(|h| {
            h.get("command")
                .and_then(|c| c.as_str())
                .is_some_and(hook_command_is_ours)
        });
    }
    false
}

/// Remove ai-memory hook entries from a settings/hooks JSON document.
/// Preserves third-party entries (including siblings under the same
/// event). Prunes an event key when emptied and the `hooks` object
/// when emptied. Detection is by signature, so stale event keys
/// outside the current vocabulary are caught too.
fn strip_ai_memory_hooks(content: &str) -> Result<HookRemoval> {
    let mut removed_events = Vec::new();
    let new_content = mutate_json(content, |root| {
        let Some(hooks) = root.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
            return Ok(());
        };
        let events: Vec<String> = hooks.keys().cloned().collect();
        for event in events {
            let Some(arr) = hooks.get_mut(&event).and_then(|v| v.as_array_mut()) else {
                continue;
            };
            let before = arr.len();
            arr.retain(|entry| !hook_entry_is_ours(entry));
            if arr.len() != before {
                removed_events.push(event.clone());
            }
            if arr.is_empty() {
                hooks.remove(&event);
            }
        }
        if hooks.is_empty() {
            root.remove("hooks");
        }
        Ok(())
    })?;
    Ok(HookRemoval {
        new_content,
        removed_events,
    })
}
```

Inside `mod tests`:

```rust
#[test]
fn strip_hooks_nested_removes_ours_keeps_third_party() {
    let content = r#"{
      "hooks": {
        "SessionStart": [
          {"matcher":"","hooks":[{"type":"command","command":"AI_MEMORY_HOOK_URL=http://h /x/session-start.sh"}]}
        ],
        "Notification": [
          {"matcher":"","hooks":[{"type":"command","command":"/usr/bin/notify.sh"}]}
        ]
      }
    }"#;
    let out = strip_ai_memory_hooks(content).unwrap();
    assert_eq!(out.removed_events, vec!["SessionStart".to_string()]);
    let v: serde_json::Value = serde_json::from_str(&out.new_content).unwrap();
    assert!(v["hooks"].get("SessionStart").is_none(), "our event pruned");
    assert!(v["hooks"].get("Notification").is_some(), "third-party kept");
}

#[test]
fn strip_hooks_flat_cursor_shape() {
    let content = r#"{
      "version": 1,
      "hooks": {
        "stop": [
          {"type":"command","command":"AI_MEMORY_HOOK_URL=http://h /x/stop.sh","matcher":""}
        ]
      }
    }"#;
    let out = strip_ai_memory_hooks(content).unwrap();
    assert_eq!(out.removed_events, vec!["stop".to_string()]);
    let v: serde_json::Value = serde_json::from_str(&out.new_content).unwrap();
    assert!(v["hooks"].get("stop").is_none());
    assert_eq!(v["version"], 1, "sibling top-level key preserved");
}

#[test]
fn strip_hooks_prunes_emptied_hooks_object() {
    let content = r#"{"hooks":{"Stop":[{"type":"command","command":"AI_MEMORY_HOOK_URL=x /a/stop.sh"}]}}"#;
    let out = strip_ai_memory_hooks(content).unwrap();
    let v: serde_json::Value = serde_json::from_str(&out.new_content).unwrap();
    assert!(v.get("hooks").is_none(), "emptied hooks object removed");
}

#[test]
fn strip_hooks_preserves_third_party_with_generic_basename() {
    // Same event, two entries: ours + a user hook named stop.sh w/o signature.
    let content = r#"{
      "hooks": {
        "Stop": [
          {"matcher":"","hooks":[{"type":"command","command":"AI_MEMORY_HOOK_URL=x /a/stop.sh"}]},
          {"matcher":"","hooks":[{"type":"command","command":"/home/u/scripts/stop.sh"}]}
        ]
      }
    }"#;
    let out = strip_ai_memory_hooks(content).unwrap();
    let v: serde_json::Value = serde_json::from_str(&out.new_content).unwrap();
    let arr = v["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(arr.len(), 1, "only ours removed");
    assert!(arr[0]["hooks"][0]["command"].as_str().unwrap().contains("/home/u/scripts/stop.sh"));
}

#[test]
fn strip_hooks_no_hooks_key_is_noop() {
    let content = r#"{"unrelated":true}"#;
    let out = strip_ai_memory_hooks(content).unwrap();
    assert!(out.removed_events.is_empty());
}
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p ai-memory-cli uninstall::tests::strip_hooks`
Expected: 5 tests PASS.

- [ ] **Step 3: Run clippy**

Run: `cargo clippy -p ai-memory-cli --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 4: Commit**

```bash
git add crates/ai-memory-cli/src/commands/uninstall.rs
git commit -m "feat(uninstall): shape-aware hook JSON stripper"
```

---

## Task 4: MCP JSON stripper (name OR endpoint match)

Removes the ai-memory MCP server from a JSON client config. Matches by key name (default `ai-memory`) OR endpoint (`url`/`httpUrl` field equals the server URL, or a `mcp-remote` `args` array containing it). Navigates to the correct servers object per client.

**Files:**
- Modify: `crates/ai-memory-cli/src/commands/uninstall.rs`

- [ ] **Step 1: Write the failing tests**

Add to `uninstall.rs`:

```rust
use crate::cli::McpClient;

/// Where the servers object lives in each JSON client's config.
/// (Codex is TOML — handled separately in Task 5.)
fn mcp_servers_path(client: McpClient) -> Option<&'static [&'static str]> {
    match client {
        McpClient::ClaudeCode
        | McpClient::ClaudeDesktop
        | McpClient::Cursor
        | McpClient::GeminiCli => Some(&["mcpServers"]),
        McpClient::OpenCode => Some(&["mcp"]),
        McpClient::Openclaw => Some(&["mcp", "servers"]),
        McpClient::Codex | McpClient::Pi => None,
    }
}

/// True when an MCP server entry is ai-memory's: keyed by the expected
/// name, OR its url/httpUrl equals the endpoint, OR it is a
/// `mcp-remote` stdio shim whose args contain the endpoint.
fn mcp_entry_is_ours(
    key: &str,
    entry: &serde_json::Value,
    name: &str,
    url: &str,
) -> bool {
    if key == name {
        return true;
    }
    for field in ["url", "httpUrl"] {
        if entry.get(field).and_then(|v| v.as_str()) == Some(url) {
            return true;
        }
    }
    if let Some(args) = entry.get("args").and_then(|a| a.as_array()) {
        let has_remote = args.iter().any(|a| a.as_str() == Some("mcp-remote"));
        let has_url = args.iter().any(|a| a.as_str() == Some(url));
        if has_remote && has_url {
            return true;
        }
    }
    false
}

/// Remove ai-memory's MCP server from a JSON client config. Returns
/// the new content and the names removed. Prunes the (possibly nested)
/// servers object and its parents if they empty.
fn strip_mcp_json(
    content: &str,
    client: McpClient,
    name: &str,
    url: &str,
) -> Result<(String, Vec<String>)> {
    let Some(path) = mcp_servers_path(client) else {
        return Ok((content.to_string(), Vec::new()));
    };
    let mut removed = Vec::new();
    let new_content = mutate_json(content, |root| {
        // Walk down to the servers object without creating missing parents.
        let mut cursor: &mut serde_json::Map<String, serde_json::Value> = root;
        for (depth, key) in path.iter().enumerate() {
            let is_last = depth == path.len() - 1;
            if is_last {
                let Some(servers) = cursor.get_mut(*key).and_then(|v| v.as_object_mut()) else {
                    return Ok(());
                };
                let keys: Vec<String> = servers.keys().cloned().collect();
                for k in keys {
                    let ours = servers
                        .get(&k)
                        .is_some_and(|e| mcp_entry_is_ours(&k, e, name, url));
                    if ours {
                        servers.remove(&k);
                        removed.push(k);
                    }
                }
                if servers.is_empty() {
                    cursor.remove(*key);
                }
            } else {
                let Some(next) = cursor.get_mut(*key).and_then(|v| v.as_object_mut()) else {
                    return Ok(());
                };
                cursor = next;
            }
        }
        Ok(())
    })?;
    Ok((new_content, removed))
}
```

Inside `mod tests`:

```rust
#[test]
fn strip_mcp_claude_by_name_keeps_others() {
    let content = r#"{"mcpServers":{"ai-memory":{"type":"http","url":"http://127.0.0.1:49374/mcp"},"other":{"url":"http://x"}}}"#;
    let (out, removed) = strip_mcp_json(content, McpClient::ClaudeCode, "ai-memory", "http://127.0.0.1:49374/mcp").unwrap();
    assert_eq!(removed, vec!["ai-memory".to_string()]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v["mcpServers"].get("ai-memory").is_none());
    assert!(v["mcpServers"].get("other").is_some());
}

#[test]
fn strip_mcp_by_endpoint_under_custom_name() {
    let content = r#"{"mcpServers":{"my-mem":{"url":"http://127.0.0.1:49374/mcp"}}}"#;
    let (out, removed) = strip_mcp_json(content, McpClient::ClaudeCode, "ai-memory", "http://127.0.0.1:49374/mcp").unwrap();
    assert_eq!(removed, vec!["my-mem".to_string()], "matched by endpoint, not name");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v.get("mcpServers").is_none(), "emptied servers object pruned");
}

#[test]
fn strip_mcp_claude_desktop_mcp_remote_args() {
    let content = r#"{"mcpServers":{"weird-name":{"command":"npx","args":["-y","mcp-remote","http://127.0.0.1:49374/mcp"]}}}"#;
    let (_out, removed) = strip_mcp_json(content, McpClient::ClaudeDesktop, "ai-memory", "http://127.0.0.1:49374/mcp").unwrap();
    assert_eq!(removed, vec!["weird-name".to_string()]);
}

#[test]
fn strip_mcp_openclaw_nested_servers() {
    let content = r#"{"mcp":{"servers":{"ai-memory":{"url":"http://127.0.0.1:49374/mcp"}}}}"#;
    let (out, removed) = strip_mcp_json(content, McpClient::Openclaw, "ai-memory", "http://127.0.0.1:49374/mcp").unwrap();
    assert_eq!(removed, vec!["ai-memory".to_string()]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    // mcp.servers emptied → pruned; mcp may remain empty or be pruned by parent logic (servers removed).
    assert!(v["mcp"].get("servers").is_none());
}

#[test]
fn strip_mcp_no_match_is_noop() {
    let content = r#"{"mcpServers":{"other":{"url":"http://x"}}}"#;
    let (_out, removed) = strip_mcp_json(content, McpClient::ClaudeCode, "ai-memory", "http://127.0.0.1:49374/mcp").unwrap();
    assert!(removed.is_empty());
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p ai-memory-cli uninstall::tests::strip_mcp`
Expected: 5 tests PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/ai-memory-cli/src/commands/uninstall.rs
git commit -m "feat(uninstall): MCP JSON stripper (name or endpoint match)"
```

---

## Task 5: MCP TOML stripper (Codex)

Codex registers `[mcp_servers.<name>]` in `~/.codex/config.toml`. Remove the matching child table by name or `url`, preserving comments and other tables (via `toml_edit`). Leave an emptied `[mcp_servers]` header in place (cosmetic, valid) per the spec.

**Files:**
- Modify: `crates/ai-memory-cli/src/commands/uninstall.rs`

- [ ] **Step 1: Write the failing tests**

Add to `uninstall.rs`:

```rust
use crate::commands::apply_shared::mutate_toml;

/// Remove ai-memory's Codex MCP table by name or `url`. Returns new
/// content and removed names. Preserves comments + other tables.
fn strip_mcp_toml(content: &str, name: &str, url: &str) -> Result<(String, Vec<String>)> {
    let mut removed = Vec::new();
    let new_content = mutate_toml(content, |doc| {
        let Some(servers) = doc.get_mut("mcp_servers").and_then(|i| i.as_table_mut()) else {
            return Ok(());
        };
        let keys: Vec<String> = servers.iter().map(|(k, _)| k.to_string()).collect();
        for k in keys {
            let matches_url = servers
                .get(&k)
                .and_then(|item| item.get("url"))
                .and_then(|u| u.as_str())
                == Some(url);
            if k == name || matches_url {
                servers.remove(&k);
                removed.push(k);
            }
        }
        Ok(())
    })?;
    Ok((new_content, removed))
}
```

Inside `mod tests`:

```rust
#[test]
fn strip_mcp_toml_by_name_keeps_comments_and_tables() {
    let content = "# my codex config\n[other]\nkeep = true\n\n[mcp_servers.ai-memory]\nurl = \"http://127.0.0.1:49374/mcp\"\n";
    let (out, removed) = strip_mcp_toml(content, "ai-memory", "http://127.0.0.1:49374/mcp").unwrap();
    assert_eq!(removed, vec!["ai-memory".to_string()]);
    assert!(out.contains("# my codex config"));
    assert!(out.contains("[other]"));
    assert!(!out.contains("[mcp_servers.ai-memory]"));
}

#[test]
fn strip_mcp_toml_by_url_under_custom_name() {
    let content = "[mcp_servers.custom]\nurl = \"http://127.0.0.1:49374/mcp\"\n";
    let (out, removed) = strip_mcp_toml(content, "ai-memory", "http://127.0.0.1:49374/mcp").unwrap();
    assert_eq!(removed, vec!["custom".to_string()]);
    assert!(!out.contains("custom"));
}

#[test]
fn strip_mcp_toml_no_match_is_noop() {
    let content = "[mcp_servers.other]\nurl = \"http://x\"\n";
    let (_out, removed) = strip_mcp_toml(content, "ai-memory", "http://127.0.0.1:49374/mcp").unwrap();
    assert!(removed.is_empty());
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p ai-memory-cli uninstall::tests::strip_mcp_toml`
Expected: 3 tests PASS. (If the `toml_edit` `Item::get("url")` access doesn't resolve a child table's value, adjust to `servers.get(&k).and_then(|i| i.as_table()).and_then(|t| t.get("url"))` — same intent.)

- [ ] **Step 3: Commit**

```bash
git add crates/ai-memory-cli/src/commands/uninstall.rs
git commit -m "feat(uninstall): Codex MCP TOML stripper"
```

---

## Task 6: Extract per-agent config-path helpers

The orchestrator needs each agent's config path. Today those paths are inlined inside the private `apply_to_*` functions. Extract them into `pub(crate)` helpers in the same module (light, in-module refactor — DRY, no new cross-cutting registry per workflow rule #6) and have the install path call them too.

**Files:**
- Modify: `crates/ai-memory-cli/src/commands/install_hooks.rs`
- Modify: `crates/ai-memory-cli/src/commands/install_mcp.rs`

- [ ] **Step 1: Add hook-config path helpers in `install_hooks.rs`**

Add near the top of `install_hooks.rs` (after imports):

```rust
/// `~/.claude/settings.json` — Claude Code hooks live under `hooks`.
pub(crate) fn claude_settings_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(dirs::home_dir()
        .context("could not locate $HOME for ~/.claude/settings.json")?
        .join(".claude")
        .join("settings.json"))
}

/// `~/.codex/hooks.json`.
pub(crate) fn codex_hooks_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(dirs::home_dir()
        .context("could not locate $HOME for ~/.codex/hooks.json")?
        .join(".codex")
        .join("hooks.json"))
}

/// `~/.cursor/hooks.json`.
pub(crate) fn cursor_hooks_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(dirs::home_dir()
        .context("could not locate $HOME for ~/.cursor/hooks.json")?
        .join(".cursor")
        .join("hooks.json"))
}

/// `~/.gemini/settings.json`.
pub(crate) fn gemini_settings_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(dirs::home_dir()
        .context("could not locate $HOME for ~/.gemini/settings.json")?
        .join(".gemini")
        .join("settings.json"))
}

/// `~/.config/opencode/plugins/ai-memory.ts` — OpenCode's plugin file.
pub(crate) fn opencode_plugin_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(dirs::home_dir()
        .context("could not locate $HOME for ~/.config/opencode")?
        .join(".config")
        .join("opencode")
        .join("plugins")
        .join("ai-memory.ts"))
}
```

- [ ] **Step 2: Point the install fns at the helpers (DRY)**

In `apply_to_claude_code_settings`, replace the inline path block:

```rust
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => dirs::home_dir()
            .context("could not locate $HOME for ~/.claude/settings.json")?
            .join(".claude")
            .join("settings.json"),
    };
```

with:

```rust
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => claude_settings_path()?,
    };
```

Do the equivalent substitution in `apply_to_codex_settings` (`codex_hooks_path()?`), the Cursor apply fn (`cursor_hooks_path()?`), and the Gemini apply fn (`gemini_settings_path()?`). Confirm the OpenCode apply fn already resolves `~/.config/opencode/plugins/ai-memory.ts`; if it inlines that path, swap it to `opencode_plugin_path()?`.

- [ ] **Step 3: Add `mcp_config_path` in `install_mcp.rs`**

`resolve_config_file(args)` (lines ~64-110) maps client → path but takes the full `InstallMcpArgs` (for the `--config-file` override). Lift the `home` binding + the `match args.client { … }` body into a standalone `mcp_config_path(client)`, and make `resolve_config_file` delegate. Note: Claude Code's **MCP** config is `~/.claude.json` (NOT `~/.claude/settings.json`, which is the hooks file).

Add this function (the match body is moved verbatim from the current `resolve_config_file`):

```rust
/// Default MCP config-file path for a client (ignores any
/// `--config-file` override). Shared by install and uninstall.
///
/// # Errors
/// Returns an error for `Pi` (no MCP config), for Claude Desktop on
/// unsupported OSes, or when `$HOME` can't be resolved.
pub(crate) fn mcp_config_path(client: crate::cli::McpClient) -> Result<PathBuf> {
    use crate::cli::McpClient;
    let home = dirs::home_dir().context("could not locate $HOME for config-file auto-detect")?;
    Ok(match client {
        // Claude Code reads MCP registrations from ~/.claude.json (the
        // file `claude mcp add` operates on), NOT ~/.claude/settings.json.
        McpClient::ClaudeCode => home.join(".claude.json"),
        McpClient::Codex => home.join(".codex").join("config.toml"),
        McpClient::OpenCode => home.join(".config").join("opencode").join("opencode.json"),
        McpClient::Cursor => home.join(".cursor").join("mcp.json"),
        McpClient::ClaudeDesktop => {
            #[cfg(target_os = "macos")]
            {
                home.join("Library")
                    .join("Application Support")
                    .join("Claude")
                    .join("claude_desktop_config.json")
            }
            #[cfg(target_os = "windows")]
            {
                home.join("AppData")
                    .join("Roaming")
                    .join("Claude")
                    .join("claude_desktop_config.json")
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                bail!(
                    "Claude Desktop is not officially distributed for this OS. \
                     Pass --config-file explicitly if you know where it lives."
                );
            }
        }
        McpClient::GeminiCli => home.join(".gemini").join("settings.json"),
        McpClient::Openclaw => home.join(".openclaw").join("config.json"),
        McpClient::Pi => bail!("pi has no MCP config file (MCP not supported)"),
    })
}
```

Then rewrite `resolve_config_file` to delegate (preserving the `--config-file` override):

```rust
fn resolve_config_file(args: &InstallMcpArgs) -> Result<PathBuf> {
    if let Some(p) = &args.config_file {
        return Ok(p.clone());
    }
    mcp_config_path(args.client)
}
```

This is behavior-preserving: the same paths, the same `bail!`s, just relocated so uninstall can call `mcp_config_path` without constructing `InstallMcpArgs`.

- [ ] **Step 4: Build and run the existing install tests to confirm no regression**

Run: `cargo test -p ai-memory-cli install_hooks install_mcp`
Expected: all existing install tests still PASS (the refactor is behavior-preserving).

Run: `cargo clippy -p ai-memory-cli --all-targets -- -D warnings`
Expected: no warnings (watch for an unused `mcp_config_path` until Task 8 uses it — add `#[allow(dead_code)]` only if the lint fires before Task 8; remove it in Task 8).

- [ ] **Step 5: Commit**

```bash
git add crates/ai-memory-cli/src/commands/install_hooks.rs crates/ai-memory-cli/src/commands/install_mcp.rs
git commit -m "refactor(install): extract per-agent config-path helpers for reuse"
```

---

## Task 7: CLI surface + dispatch wiring

Add the subcommand, its args, and the `--only` enum, then wire dispatch. The orchestrator `run` is stubbed to a clean "nothing to do yet" so the binary compiles and the command is reachable; the real logic lands in Tasks 8–10.

**Files:**
- Modify: `crates/ai-memory-cli/src/cli.rs`
- Modify: `crates/ai-memory-cli/src/main.rs`
- Modify: `crates/ai-memory-cli/src/commands/uninstall.rs`

- [ ] **Step 1: Add the `Command::Uninstall` variant**

In `cli.rs`, inside `pub enum Command { … }`, add (near the other lifecycle commands):

```rust
    /// Remove ai-memory's wiring (hooks, MCP, instructions) from all
    /// detected agents. Dry-run unless `--apply`.
    Uninstall(UninstallArgs),
```

- [ ] **Step 2: Define `UninstallArgs` and `UninstallOnly`**

In `cli.rs`, add (next to `ResetArgs`):

```rust
/// Which concern `uninstall` should touch. Omitted = all three.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum UninstallOnly {
    Hooks,
    Mcp,
    Instructions,
}

/// Arguments for `uninstall`.
#[derive(Debug, Args)]
pub struct UninstallArgs {
    /// Actually modify files. Without it, prints the removal plan and
    /// exits (dry-run), mirroring `reset` without `--confirm`.
    #[arg(long)]
    pub apply: bool,
    /// After removing the wiring, wipe wiki/, db/, raw/ via the reset
    /// path (refuses if another ai-memory process is alive). Only
    /// meaningful with `--apply`.
    #[arg(long)]
    pub purge_data: bool,
    /// Limit to one concern. Omitted = hooks + mcp + instructions.
    #[arg(long, value_enum)]
    pub only: Option<UninstallOnly>,
    /// Skip the interactive confirmation when a TTY is attached.
    #[arg(long)]
    pub yes: bool,
}
```

- [ ] **Step 3: Add the dispatch arm in `main.rs`**

After the `Command::RenameProject(...)` arm (~line 63):

```rust
        Command::Uninstall(args) => commands::uninstall::run(&config, args),
```

(`uninstall::run` is synchronous like `reset::run` — no `.await`.)

- [ ] **Step 4: Add the stub `run` in `uninstall.rs`**

Add to `uninstall.rs`:

```rust
use crate::config::Config;
use crate::cli::UninstallArgs;

/// Run the `uninstall` subcommand.
///
/// # Errors
/// Returns an error if a config file is malformed or a removal write
/// fails. Absent files / nothing-to-remove are not errors.
pub fn run(_config: &Config, _args: UninstallArgs) -> anyhow::Result<()> {
    // Implemented in Tasks 8–10.
    println!("uninstall: not yet implemented");
    Ok(())
}
```

- [ ] **Step 5: Build and verify the command is reachable**

Run: `cargo run -p ai-memory-cli -- uninstall --help`
Expected: clap prints the `--apply`, `--purge-data`, `--only`, `--yes` flags.

Run: `cargo clippy -p ai-memory-cli --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/ai-memory-cli/src/cli.rs crates/ai-memory-cli/src/main.rs crates/ai-memory-cli/src/commands/uninstall.rs
git commit -m "feat(uninstall): CLI surface + dispatch (stub run)"
```

---

## Task 8: Orchestrator — build the plan and dry-run print

Collect a typed plan across agents using the pure functions and the path helpers. Print it grouped by file. Without `--apply`, stop here.

**Files:**
- Modify: `crates/ai-memory-cli/src/commands/uninstall.rs`

- [ ] **Step 1: Add plan types and the planning function**

Add to `uninstall.rs`:

```rust
use std::path::PathBuf;
use crate::commands::{install_hooks, install_mcp};
use crate::config::DEFAULT_MCP_URL;

/// One file the uninstall will touch, plus what it will do to it.
#[derive(Debug)]
enum PlannedChange {
    /// JSON/TOML rewrite removing the listed items (events or server names).
    Rewrite { path: PathBuf, removed: Vec<String> },
    /// Whole-file delete (OpenCode plugin).
    DeleteFile { path: PathBuf },
}

/// Build the full removal plan by reading each existing config file and
/// running the matching pure stripper. Missing files / no-matches
/// produce no entry. `name`/`url` identify the MCP server.
fn build_plan(args: &UninstallArgs) -> Result<Vec<PlannedChange>> {
    let mut plan = Vec::new();
    let want = |k: crate::cli::UninstallOnly| args.only.is_none() || args.only == Some(k);
    let name = "ai-memory";
    let url = DEFAULT_MCP_URL;

    // ---- Hooks (JSON configs) ----
    if want(crate::cli::UninstallOnly::Hooks) {
        let hook_files = [
            install_hooks::claude_settings_path()?,
            install_hooks::codex_hooks_path()?,
            install_hooks::cursor_hooks_path()?,
            install_hooks::gemini_settings_path()?,
        ];
        for path in hook_files {
            if !path.exists() {
                continue;
            }
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let removal = strip_ai_memory_hooks(&content)?;
            if !removal.removed_events.is_empty() {
                plan.push(PlannedChange::Rewrite {
                    path,
                    removed: removal.removed_events,
                });
            }
        }
        // OpenCode plugin file: presence = it's ours (we wrote it).
        let plugin = install_hooks::opencode_plugin_path()?;
        if plugin.exists() {
            plan.push(PlannedChange::DeleteFile { path: plugin });
        }
    }

    // ---- MCP (per client) ----
    if want(crate::cli::UninstallOnly::Mcp) {
        use crate::cli::McpClient::*;
        for client in [ClaudeCode, Codex, OpenCode, Cursor, ClaudeDesktop, GeminiCli, Openclaw] {
            // Pi has no MCP config — skip silently (never bail on uninstall).
            let Ok(path) = install_mcp::mcp_config_path(client) else {
                continue;
            };
            if !path.exists() {
                continue;
            }
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let (_new, removed) = if matches!(client, Codex) {
                strip_mcp_toml(&content, name, url)?
            } else {
                strip_mcp_json(&content, client, name, url)?
            };
            if !removed.is_empty() {
                plan.push(PlannedChange::Rewrite { path, removed });
            }
        }
    }

    // ---- Instructions (cwd CLAUDE.md / AGENTS.md) ----
    if want(crate::cli::UninstallOnly::Instructions) {
        let cwd = std::env::current_dir().context("getting CWD for instruction removal")?;
        for name_md in ["CLAUDE.md", "AGENTS.md"] {
            let path = cwd.join(name_md);
            if !path.exists() {
                continue;
            }
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let (_new, found) = strip_instructions_block(&content);
            if found {
                plan.push(PlannedChange::Rewrite {
                    path,
                    removed: vec!["instruction block".to_string()],
                });
            }
        }
    }

    Ok(plan)
}

/// Print the plan, one line per file, mirroring `reset`'s dry-run style.
fn print_plan(plan: &[PlannedChange]) {
    if plan.is_empty() {
        println!("Nothing to remove. ai-memory wiring not found.");
        return;
    }
    for change in plan {
        match change {
            PlannedChange::Rewrite { path, removed } => {
                println!("would remove {} from {}", removed.join(", "), path.display());
            }
            PlannedChange::DeleteFile { path } => {
                println!("would delete {}", path.display());
            }
        }
    }
}
```

- [ ] **Step 2: Wire the dry-run into `run`**

Replace the stub body of `run`:

```rust
pub fn run(config: &Config, args: UninstallArgs) -> anyhow::Result<()> {
    let plan = build_plan(&args)?;
    print_plan(&plan);
    if !args.apply {
        println!("(dry-run; pass --apply to remove)");
        return Ok(());
    }
    // --apply path lands in Task 9.
    let _ = config;
    Ok(())
}
```

Add the needed `use anyhow::Context;` at the top if not already present.

- [ ] **Step 3: Manually exercise the dry-run**

Run: `cargo run -p ai-memory-cli -- uninstall`
Expected: either "Nothing to remove…" or a list of `would remove …` lines, then `(dry-run; pass --apply to remove)`. No files changed.

Run: `cargo clippy -p ai-memory-cli --all-targets -- -D warnings`
Expected: no warnings (remove any temporary `#[allow(dead_code)]` from Task 6 now that `mcp_config_path` is used).

- [ ] **Step 4: Commit**

```bash
git add crates/ai-memory-cli/src/commands/uninstall.rs
git commit -m "feat(uninstall): build removal plan + dry-run output"
```

---

## Task 9: Orchestrator — apply the wiring removal

With `--apply`, rewrite each planned file via `apply_atomic` (re-running the pure stripper so the write is atomic + backed up) and delete the OpenCode plugin. Confirm interactively when a TTY is attached.

**Files:**
- Modify: `crates/ai-memory-cli/src/commands/uninstall.rs`

- [ ] **Step 1: Add the apply function**

Add to `uninstall.rs`:

```rust
use std::io::IsTerminal;
use crate::commands::apply_shared::apply_atomic;

/// Re-run the matching stripper inside `apply_atomic` so the actual
/// write is atomic + backed up. `client` is `Some` for MCP rewrites,
/// `None` for hook/instruction rewrites (distinguished by filename).
fn apply_change(change: &PlannedChange, name: &str, url: &str) -> Result<()> {
    match change {
        PlannedChange::DeleteFile { path } => {
            std::fs::remove_file(path)
                .with_context(|| format!("deleting {}", path.display()))?;
            println!("✓ deleted {}", path.display());
        }
        PlannedChange::Rewrite { path, .. } => {
            let file = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
            let outcome = apply_atomic(path, |existing| {
                if file == "CLAUDE.md" || file == "AGENTS.md" {
                    Ok(strip_instructions_block(existing).0)
                } else if file == "config.toml" {
                    Ok(strip_mcp_toml(existing, name, url)?.0)
                } else {
                    // hooks settings/hooks.json OR an mcpServers JSON file:
                    // run BOTH strippers; each is a no-op if its key is absent.
                    let after_hooks = strip_ai_memory_hooks(existing)?.new_content;
                    // Try every JSON MCP client shape; the right one matches.
                    let mut out = after_hooks;
                    for client in [
                        crate::cli::McpClient::ClaudeCode,
                        crate::cli::McpClient::OpenCode,
                        crate::cli::McpClient::Openclaw,
                    ] {
                        out = strip_mcp_json(&out, client, name, url)?.0;
                    }
                    Ok(out)
                }
            })?;
            println!("✓ {} {}", outcome.verb(), path.display());
        }
    }
    Ok(())
}
```

> Note: a hooks file and an MCP file can be the **same physical file** — Gemini CLI keeps both hooks and `mcpServers` in `~/.gemini/settings.json` (Claude Code, by contrast, splits them: hooks in `~/.claude/settings.json`, MCP in `~/.claude.json`). Two consequences: (1) running both the hook stripper and the JSON MCP strippers on any non-`config.toml` JSON file is safe — each only removes keys it recognizes (`hooks`, `mcpServers`, `mcp`, `mcp.servers`); (2) `build_plan` may list the same Gemini path twice (once from the hooks pass, once from the MCP pass). That's fine: `apply_change` runs both strippers each time, so the first `Rewrite` removes everything and the second is a clean `apply_atomic` NoOp (one extra "no-op" report line, no double backup).

- [ ] **Step 2: Extend `run` to apply with confirmation**

Replace the `--apply` placeholder in `run`:

```rust
pub fn run(config: &Config, args: UninstallArgs) -> anyhow::Result<()> {
    let name = "ai-memory";
    let url = crate::config::DEFAULT_MCP_URL;

    let plan = build_plan(&args)?;
    print_plan(&plan);
    if !args.apply {
        println!("(dry-run; pass --apply to remove)");
        return Ok(());
    }
    if plan.is_empty() && !args.purge_data {
        return Ok(());
    }

    if std::io::stdin().is_terminal() && !args.yes {
        eprint!("Proceed with removal? [y/N] ");
        use std::io::Write as _;
        std::io::stderr().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
        if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("aborted.");
            return Ok(());
        }
    }

    for change in &plan {
        apply_change(change, name, url)?;
    }

    // --purge-data + Docker hint land in Task 10.
    let _ = config;
    Ok(())
}
```

- [ ] **Step 3: Manual end-to-end test against a fake HOME**

```bash
# Build once.
cargo build -p ai-memory-cli
# Seed a fake claude settings.json with our hook + a third-party hook.
mkdir -p /tmp/fakehome/.claude
cat > /tmp/fakehome/.claude/settings.json <<'JSON'
{"hooks":{"Stop":[{"matcher":"","hooks":[{"type":"command","command":"AI_MEMORY_HOOK_URL=http://h /x/stop.sh"}]}],"Notification":[{"matcher":"","hooks":[{"type":"command","command":"/usr/bin/n.sh"}]}]}}
JSON
HOME=/tmp/fakehome ./target/debug/ai-memory uninstall --apply --only hooks --yes
```
Expected: `✓ updated /tmp/fakehome/.claude/settings.json`; the file now keeps `Notification`, drops `Stop`; a `.bak-<ts>` sits next to it. Verify:
```bash
cat /tmp/fakehome/.claude/settings.json   # no "Stop", "Notification" intact
ls /tmp/fakehome/.claude/                  # settings.json + settings.json.bak-...
```

- [ ] **Step 4: Run clippy + full crate tests**

Run: `cargo clippy -p ai-memory-cli --all-targets -- -D warnings`
Run: `cargo test -p ai-memory-cli uninstall`
Expected: clean; all unit tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ai-memory-cli/src/commands/uninstall.rs
git commit -m "feat(uninstall): apply wiring removal with backups + confirmation"
```

---

## Task 10: `--purge-data`, Docker hint, and exit codes

After wiring removal, optionally purge data through the same guard the `reset` command uses. Always print the Docker teardown hint. If purge is refused because a process is alive, report that wiring succeeded but data was not purged, and exit non-zero.

**Files:**
- Modify: `crates/ai-memory-cli/src/commands/uninstall.rs`

- [ ] **Step 1: Add purge + hint logic to `run`**

Replace the tail of `run` (the `// --purge-data … ` and `let _ = config;` lines) with:

```rust
    let mut purge_refused = false;
    if args.purge_data {
        let siblings = crate::process_guard::sibling_processes();
        if !siblings.is_empty() {
            eprintln!(
                "{}",
                crate::process_guard::busy_message("purge data", &siblings)
            );
            eprintln!("wiring was removed, but data was NOT purged (process alive).");
            purge_refused = true;
        } else {
            for sub in ["wiki", "db", "raw"] {
                let path = config.data_dir.join(sub);
                if path.exists() {
                    std::fs::remove_dir_all(&path)
                        .with_context(|| format!("removing {}", path.display()))?;
                    std::fs::create_dir_all(&path)
                        .with_context(|| format!("recreating {}", path.display()))?;
                    println!("✓ purged {}", path.display());
                }
            }
        }
    }

    print_docker_hint(args.purge_data && !purge_refused);

    if purge_refused {
        anyhow::bail!("uninstall completed wiring removal but could not purge data");
    }
    Ok(())
}

/// Print the manual Docker teardown steps (never executed). When the
/// data was purged locally, note that; otherwise remind how to wipe it.
fn print_docker_hint(data_purged: bool) {
    println!();
    println!("Wiring removed. ai-memory's server + data live in its container/volume —");
    println!("tear those down manually:");
    println!("  docker compose -f docker/docker-compose.yml down -v");
    println!("  docker volume rm ai-memory-data   # if you used the default volume");
    println!("  rm -f bin/ai-memory               # the wrapper script, if installed");
    if !data_purged {
        println!();
        println!("Local data dir was left intact. To wipe it: `ai-memory reset --confirm` (or re-run with --purge-data).");
    }
}
```

> The purge loop mirrors `reset::run` (same `["wiki","db","raw"]`, same `process_guard`, same remove+recreate). It is duplicated rather than calling `reset::run` because `reset` builds its own `ResetArgs` and prints its own messages; the loop here is three lines and keeps uninstall's reporting coherent. If you prefer reuse, factor `reset`'s body into a `pub(crate) fn purge_data_dirs(config)` and call it from both — either is acceptable.

- [ ] **Step 2: Manual test — purge with no server running**

```bash
HOME=/tmp/fakehome AI_MEMORY_DATA_DIR=/tmp/fakedata ./target/debug/ai-memory init
mkdir -p /tmp/fakedata/wiki/x
HOME=/tmp/fakehome AI_MEMORY_DATA_DIR=/tmp/fakedata ./target/debug/ai-memory uninstall --apply --purge-data --yes
echo "exit: $?"
ls /tmp/fakedata/wiki    # empty (recreated)
```
Expected: `✓ purged …/wiki` etc., the Docker hint, exit 0.

- [ ] **Step 3: Manual test — purge refused while a server is alive**

```bash
HOME=/tmp/fakehome AI_MEMORY_DATA_DIR=/tmp/fakedata ./target/debug/ai-memory serve --transport http --bind 127.0.0.1:49999 &
SERVER=$!
sleep 1
HOME=/tmp/fakehome AI_MEMORY_DATA_DIR=/tmp/fakedata ./target/debug/ai-memory uninstall --apply --purge-data --yes
echo "exit: $?"   # expect non-zero
kill $SERVER
```
Expected: wiring removal lines, then the busy message + "data was NOT purged", the Docker hint, and a non-zero exit.

- [ ] **Step 4: Run clippy + crate tests**

Run: `cargo clippy -p ai-memory-cli --all-targets -- -D warnings`
Run: `cargo test -p ai-memory-cli`
Expected: clean; all PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ai-memory-cli/src/commands/uninstall.rs
git commit -m "feat(uninstall): --purge-data via reset guard + Docker teardown hint"
```

---

## Task 11: Integration test, docs, and final verification

End-to-end test on a temp HOME exercising the orchestrator (install → uninstall round-trip), plus CHANGELOG and a full workspace gate.

**Files:**
- Create: `crates/ai-memory-cli/tests/uninstall.rs`
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Write the integration test**

Create `crates/ai-memory-cli/tests/uninstall.rs`:

```rust
//! End-to-end: install hooks into a temp HOME, then uninstall, and
//! assert the file round-trips (our entries gone, third-party intact).

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ai-memory")
}

#[test]
fn install_then_uninstall_round_trip_claude_hooks() {
    let home = tempfile::tempdir().unwrap();
    let claude = home.path().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    // Pre-seed a third-party hook we must NOT touch.
    std::fs::write(
        claude.join("settings.json"),
        r#"{"hooks":{"Notification":[{"matcher":"","hooks":[{"type":"command","command":"/usr/bin/n.sh"}]}]}}"#,
    )
    .unwrap();

    // Install ai-memory hooks for Claude Code.
    let status = Command::new(bin())
        .args(["install-hooks", "--agent", "claude-code", "--apply"])
        .env("HOME", home.path())
        .status()
        .unwrap();
    assert!(status.success());

    // Uninstall (hooks only) and verify.
    let status = Command::new(bin())
        .args(["uninstall", "--apply", "--only", "hooks", "--yes"])
        .env("HOME", home.path())
        .status()
        .unwrap();
    assert!(status.success());

    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(claude.join("settings.json")).unwrap())
            .unwrap();
    // Third-party hook survived.
    assert!(after["hooks"]["Notification"].is_array());
    // None of our events remain (SessionStart etc. all gone).
    for ours in ["SessionStart", "SessionEnd", "PreToolUse", "PostToolUse", "Stop", "PreCompact", "UserPromptSubmit"] {
        assert!(after["hooks"].get(ours).is_none(), "{ours} should be removed");
    }
}

#[test]
fn uninstall_dry_run_changes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let claude = home.path().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    let original = r#"{"hooks":{"Stop":[{"matcher":"","hooks":[{"type":"command","command":"AI_MEMORY_HOOK_URL=x /a/stop.sh"}]}]}}"#;
    std::fs::write(claude.join("settings.json"), original).unwrap();

    let status = Command::new(bin())
        .args(["uninstall", "--only", "hooks"]) // no --apply
        .env("HOME", home.path())
        .status()
        .unwrap();
    assert!(status.success());

    let after = std::fs::read_to_string(claude.join("settings.json")).unwrap();
    assert_eq!(after, original, "dry-run must not modify the file");
}
```

- [ ] **Step 2: Run the integration test**

Run: `cargo test -p ai-memory-cli --test uninstall`
Expected: both tests PASS. (If `install-hooks` requires `--server-url` or staging args in this environment, pass the same defaults the other install tests use — check `crates/ai-memory-cli/tests/` for the established pattern and mirror it.)

- [ ] **Step 3: Update CHANGELOG**

In `CHANGELOG.md`, under the unreleased/top section, add:

```markdown
### Added
- `ai-memory uninstall` — removes ai-memory's hooks, MCP registration, and
  CLAUDE.md/AGENTS.md instruction block across all detected agents (dry-run by
  default; `--apply` to execute, with timestamped backups). `--purge-data`
  wipes wiki/db/raw via the reset guard. `--only hooks|mcp|instructions` to
  narrow. Docker/volume teardown is printed as a hint, not executed.
```

- [ ] **Step 4: Full workspace gate (CLAUDE.md rule #3)**

Run:
```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Expected: no fmt diffs, no clippy warnings, all tests green. Fix anything that fails before committing.

- [ ] **Step 5: Commit**

```bash
git add crates/ai-memory-cli/tests/uninstall.rs CHANGELOG.md
git commit -m "test(uninstall): install→uninstall round-trip + dry-run integration tests; changelog"
```

---

## Self-review notes (for the implementer)

- **Spec coverage:** §3 CLI → Tasks 7; §4.1 hooks → Tasks 2–3; §4.2 MCP → Tasks 4–5; §4.3 instructions → Task 1; §4.4 OpenCode plugin delete → Tasks 8–9; §5 architecture (pure fns + extracted path helpers, no central registry) → Tasks 1–6; §6 execution flow → Tasks 8–10; §7 error handling (absent=no-op, malformed=bail via `mutate_json`/`mutate_toml`, purge-refused=non-zero) → Tasks 8–10; §8 tests incl. no-auth + third-party preserve + endpoint match + dry-run → Tasks 2–4, 11; §9 limitations (per-project not scanned, Docker printed, plugin unconditional) → honored by construction.
- **Out of scope confirmed:** no `--config-file` CLI flag (path override is a function param only); no `--force-remove-all`; no per-project config scanning.
- **Type consistency:** `strip_ai_memory_hooks → HookRemoval{new_content, removed_events}`; `strip_mcp_json/strip_mcp_toml → (String, Vec<String>)`; `strip_instructions_block → (String, bool)`; `PlannedChange::{Rewrite{path,removed}, DeleteFile{path}}`; `mcp_config_path(McpClient)`, `claude_settings_path()` etc. — names used identically across Tasks 8–9.
- **Watch:** `toml_edit` child-value access in Task 5 (adjust `.get("url")` to go through `as_table()` if needed); the same physical file serving both hooks + MCP (Task 9 note) — running both strippers is safe and idempotent.
