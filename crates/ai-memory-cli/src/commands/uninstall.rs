//! `ai-memory uninstall` — the symmetric inverse of install-hooks /
//! install-mcp / install-instructions. Detects ai-memory's wiring in
//! every supported agent's config and removes only that, never
//! third-party entries. Optional `--purge-data` wipes wiki/db/raw via
//! the reset path. Docker teardown is printed, never executed.
//!
//! Design: docs/superpowers/specs/2026-05-24-uninstall-command-design.md

use std::path::PathBuf;
use std::io::IsTerminal;
use anyhow::{Context, Result};
use ai_memory_core::{MARKER_END, MARKER_START};
use crate::commands::apply_shared::mutate_json;
use crate::commands::apply_shared::mutate_toml;
use crate::commands::apply_shared::apply_atomic;
use crate::commands::{install_hooks, install_mcp};
use crate::cli::McpClient;
use crate::cli::UninstallArgs;
use crate::config::{Config, DEFAULT_MCP_URL};

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
fn build_plan(args: &UninstallArgs) -> anyhow::Result<Vec<PlannedChange>> {
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
        let plugin = install_hooks::opencode_plugin_path()?;
        if plugin.exists() {
            plan.push(PlannedChange::DeleteFile { path: plugin });
        }
    }

    // ---- MCP (per client) ----
    if want(crate::cli::UninstallOnly::Mcp) {
        use crate::cli::McpClient::*;
        for client in [ClaudeCode, Codex, OpenCode, Cursor, ClaudeDesktop, GeminiCli, Openclaw] {
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

/// Re-run the matching stripper inside `apply_atomic` so the actual
/// write is atomic + backed up. The stripper is chosen by filename.
fn apply_change(change: &PlannedChange, name: &str, url: &str) -> anyhow::Result<()> {
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

/// Run the `uninstall` subcommand.
///
/// # Errors
/// Returns an error if a config file is malformed or a removal write
/// fails. Absent files / nothing-to-remove are not errors.
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

    // --purge-data + Docker hint land in the next task.
    let _ = config;
    Ok(())
}

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

/// True when a hook command string was written by ai-memory. Install
/// inlines `AI_MEMORY_HOOK_URL=<url> [AI_MEMORY_AUTH_TOKEN=…] <path>`
/// into the command (render_shared.rs); the `AI_MEMORY_HOOK_URL=`
/// prefix is unconditional, so it is the reliable signature —
/// independent of auth, --server-url, --hooks-dir, --host-prefix.
fn hook_command_is_ours(command: &str) -> bool {
    command.contains("AI_MEMORY_HOOK_URL=")
}

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
                .and_then(|item| item.as_table())
                .and_then(|t| t.get("url"))
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
        assert!(v["mcp"].get("servers").is_none());
    }

    #[test]
    fn strip_mcp_no_match_is_noop() {
        let content = r#"{"mcpServers":{"other":{"url":"http://x"}}}"#;
        let (_out, removed) = strip_mcp_json(content, McpClient::ClaudeCode, "ai-memory", "http://127.0.0.1:49374/mcp").unwrap();
        assert!(removed.is_empty());
    }

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
}
