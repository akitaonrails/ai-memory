//! `ai-memory uninstall` — the symmetric inverse of install-hooks /
//! install-mcp / install-instructions. Detects ai-memory's wiring in
//! every supported agent's config and removes only that, never
//! third-party entries. Optional `--purge-data` wipes wiki/db/raw via
//! the reset path. Docker teardown is printed, never executed.
//!
//! Design: docs/superpowers/specs/2026-05-24-uninstall-command-design.md

use crate::cli::McpClient;
use crate::cli::UninstallArgs;
use crate::commands::apply_shared::apply_atomic;
use crate::commands::apply_shared::mutate_json;
use crate::commands::apply_shared::mutate_toml;
use crate::commands::{data_purge, install_hooks, install_mcp, openclaw_plugin};
use crate::config::Config;
use ai_memory_core::{MARKER_END, MARKER_START};
use anyhow::{Context, Result};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

/// One rewrite operation to apply to a config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RewriteOp {
    /// CLAUDE.md / AGENTS.md routing block.
    Instructions,
    /// Standard JSON hook table under `hooks`.
    HooksJson,
    /// Antigravity CLI named hook group under top-level `ai-memory`.
    AntigravityHooksJson,
    /// MCP JSON config for one client shape.
    McpJson(McpClient),
    /// Codex TOML MCP config.
    McpToml,
}

/// Generated files that uninstall may delete after content re-validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeleteKind {
    OpenCodePlugin,
    OmpExtension,
    OpenClawPackageJson,
    OpenClawManifest,
    OpenClawEntrypoint,
}

impl DeleteKind {
    const fn label(self) -> &'static str {
        match self {
            Self::OpenCodePlugin => "OpenCode plugin",
            Self::OmpExtension => "OMP extension",
            Self::OpenClawPackageJson => "OpenClaw package manifest",
            Self::OpenClawManifest => "OpenClaw plugin manifest",
            Self::OpenClawEntrypoint => "OpenClaw plugin entrypoint",
        }
    }
}

/// One file the uninstall will touch, plus what it will do to it.
#[derive(Debug)]
enum PlannedChange {
    /// JSON/TOML rewrite removing the listed items (events or server names).
    Rewrite {
        path: PathBuf,
        removed: Vec<String>,
        ops: Vec<RewriteOp>,
    },
    /// Whole-file delete, limited to generated files whose contents still
    /// prove they are ai-memory-owned at apply time.
    DeleteFile { path: PathBuf, kind: DeleteKind },
}

fn push_rewrite(plan: &mut Vec<PlannedChange>, path: PathBuf, removed: Vec<String>, op: RewriteOp) {
    if removed.is_empty() {
        return;
    }
    for change in plan.iter_mut() {
        if let PlannedChange::Rewrite {
            path: existing,
            removed: existing_removed,
            ops,
        } = change
            && *existing == path
        {
            existing_removed.extend(removed);
            if !ops.contains(&op) {
                ops.push(op);
            }
            return;
        }
    }
    plan.push(PlannedChange::Rewrite {
        path,
        removed,
        ops: vec![op],
    });
}

fn push_generated_delete(plan: &mut Vec<PlannedChange>, path: PathBuf, kind: DeleteKind) {
    if generated_file_is_ours(&path, kind) {
        plan.push(PlannedChange::DeleteFile { path, kind });
    }
}

/// Build the full removal plan by reading each existing config file and
/// running the matching pure stripper. Missing files / no-matches
/// produce no entry. `name`/`url` identify the MCP server.
fn build_plan(args: &UninstallArgs) -> anyhow::Result<Vec<PlannedChange>> {
    let mut plan = Vec::new();
    let want = |k: crate::cli::UninstallOnly| args.only.is_none() || args.only == Some(k);
    let name = args.mcp_name.as_deref();
    let url = args.mcp_url.as_str();

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
            push_rewrite(
                &mut plan,
                path,
                removal.removed_events,
                RewriteOp::HooksJson,
            );
        }

        let antigravity = install_hooks::antigravity_hooks_path()?;
        if antigravity.exists() {
            let content = std::fs::read_to_string(&antigravity)
                .with_context(|| format!("reading {}", antigravity.display()))?;
            let removal = strip_antigravity_hooks(&content)?;
            push_rewrite(
                &mut plan,
                antigravity,
                removal.removed_events,
                RewriteOp::AntigravityHooksJson,
            );
        }

        let plugin = install_hooks::opencode_plugin_path()?;
        push_generated_delete(&mut plan, plugin, DeleteKind::OpenCodePlugin);

        let omp = install_hooks::omp_extension_path()?;
        push_generated_delete(&mut plan, omp, DeleteKind::OmpExtension);

        let openclaw_dir = openclaw_plugin::default_plugin_dir()?;
        push_generated_delete(
            &mut plan,
            openclaw_dir.join(openclaw_plugin::PACKAGE_JSON),
            DeleteKind::OpenClawPackageJson,
        );
        push_generated_delete(
            &mut plan,
            openclaw_dir.join(openclaw_plugin::MANIFEST_JSON),
            DeleteKind::OpenClawManifest,
        );
        push_generated_delete(
            &mut plan,
            openclaw_dir.join(openclaw_plugin::ENTRYPOINT_TS),
            DeleteKind::OpenClawEntrypoint,
        );
    }

    // ---- MCP (per client) ----
    if want(crate::cli::UninstallOnly::Mcp) {
        use crate::cli::McpClient::*;
        for client in [
            ClaudeCode,
            Codex,
            OpenCode,
            Cursor,
            ClaudeDesktop,
            GeminiCli,
            Openclaw,
            Pi,
            AntigravityCli,
        ] {
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
            let op = if matches!(client, Codex) {
                RewriteOp::McpToml
            } else {
                RewriteOp::McpJson(client)
            };
            push_rewrite(&mut plan, path, removed, op);
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
                push_rewrite(
                    &mut plan,
                    path,
                    vec!["instruction block".to_string()],
                    RewriteOp::Instructions,
                );
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
            PlannedChange::Rewrite { path, removed, .. } => {
                println!(
                    "would remove {} from {}",
                    removed.join(", "),
                    path.display()
                );
            }
            PlannedChange::DeleteFile { path, kind } => {
                println!("would delete {} ({})", path.display(), kind.label());
            }
        }
    }
}

/// Re-run the planned strippers inside `apply_atomic` so the actual write is
/// atomic + backed up. Planning records exact operations per file, so shared
/// files such as `~/.gemini/settings.json` only apply the selected concerns.
fn apply_change(change: &PlannedChange, name: Option<&str>, url: &str) -> anyhow::Result<()> {
    match change {
        PlannedChange::DeleteFile { path, kind } => {
            if !path.exists() {
                return Ok(());
            }
            if !generated_file_is_ours(path, *kind) {
                println!(
                    "skipped {} because it no longer looks like an ai-memory-generated {}",
                    path.display(),
                    kind.label()
                );
                return Ok(());
            }
            std::fs::remove_file(path).with_context(|| format!("deleting {}", path.display()))?;
            println!("✓ deleted {}", path.display());
        }
        PlannedChange::Rewrite { path, ops, .. } => {
            let outcome = apply_atomic(path, |existing| {
                let mut out = existing.to_string();
                for op in ops {
                    out = match *op {
                        RewriteOp::Instructions => strip_instructions_block(&out).0,
                        RewriteOp::HooksJson => strip_ai_memory_hooks(&out)?.new_content,
                        RewriteOp::AntigravityHooksJson => {
                            strip_antigravity_hooks(&out)?.new_content
                        }
                        RewriteOp::McpJson(client) => strip_mcp_json(&out, client, name, url)?.0,
                        RewriteOp::McpToml => strip_mcp_toml(&out, name, url)?.0,
                    };
                }
                Ok(out)
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
    let name = args.mcp_name.clone();
    let url = args.mcp_url.clone();

    let plan = build_plan(&args)?;
    print_plan(&plan);
    if args.purge_data {
        for path in data_purge::purge_preview(&config.data_dir) {
            println!("would purge {}", path.display());
        }
    }
    if !args.apply {
        println!("(dry-run; pass --apply to remove)");
        return Ok(());
    }
    if plan.is_empty() && !args.purge_data {
        return Ok(());
    }

    // All-or-nothing: when we're going to purge data, refuse before touching
    // anything if an ai-memory process is alive (matches reset's guard-at-top).
    // Wiring-only uninstall stays unguarded — it edits agent config files the
    // server never touches.
    if args.purge_data {
        let siblings = crate::process_guard::sibling_processes();
        if !siblings.is_empty() {
            anyhow::bail!(crate::process_guard::busy_message("purge data", &siblings));
        }
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
        apply_change(change, name.as_deref(), &url)?;
    }

    if args.purge_data {
        for path in data_purge::purge_data_dirs(&config.data_dir)? {
            println!("✓ purged {}", path.display());
        }
    }

    print_docker_hint(args.purge_data);

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
        println!(
            "Local data dir was left intact. To wipe it: `ai-memory reset --confirm` (or re-run with --purge-data)."
        );
    }
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

/// True when a hook command string was written by ai-memory. Install renders
/// one of two shapes (render_shared.rs), each with its own reliable signature:
///
/// - **Shell scripts** (`.sh`/`.ps1`, the Docker / `setup-agent` path) inline
///   `AI_MEMORY_HOOK_URL=<url> [AI_MEMORY_AUTH_TOKEN=…] <path>` — the
///   `AI_MEMORY_HOOK_URL=` prefix is unconditional.
/// - **Native binary call** (`windows-native` / `posix-native`, the default for
///   a local-binary Claude Code install) is `<exe> hook --event <e> --agent
///   <a> --server-url <url> [--auth-token <t>]` — the
///   `hook --event …` / `--agent …` / `--server-url …` triple is unique to
///   ai-memory's own `hook` subcommand.
///
/// Both signatures are independent of auth, the `--server-url` value,
/// `--hooks-dir`, and `--host-prefix`.
fn hook_command_is_ours(command: &str) -> bool {
    command.contains("AI_MEMORY_HOOK_URL=")
        || (command.contains("hook --event ")
            && command.contains(" --agent ")
            && command.contains(" --server-url "))
}

/// Result of stripping ai-memory entries from a hooks JSON file.
struct HookRemoval {
    new_content: String,
    removed_events: Vec<String>,
}

/// Remove ai-memory commands from one hook entry. Returns `(removed_any,
/// remove_entry)`. Flat entries are removed whole; nested entries only lose the
/// matching inner commands and survive when third-party inner hooks remain.
fn strip_hook_entry(entry: &mut serde_json::Value) -> (bool, bool) {
    if let Some(cmd) = entry.get("command").and_then(|c| c.as_str())
        && hook_command_is_ours(cmd)
    {
        return (true, true);
    }
    if let Some(inner) = entry.get_mut("hooks").and_then(|h| h.as_array_mut()) {
        let before = inner.len();
        inner.retain(|h| {
            !h.get("command")
                .and_then(|c| c.as_str())
                .is_some_and(hook_command_is_ours)
        });
        let removed = inner.len() != before;
        return (removed, inner.is_empty());
    }
    (false, false)
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
            let mut removed_from_event = false;
            arr.retain_mut(|entry| {
                let (removed, remove_entry) = strip_hook_entry(entry);
                removed_from_event |= removed;
                !remove_entry
            });
            if removed_from_event {
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

/// Remove ai-memory's named Antigravity CLI hook group entries. The group
/// name alone is not enough to prove ownership; every removed entry must still
/// carry ai-memory's hook command signature.
fn strip_antigravity_hooks(content: &str) -> Result<HookRemoval> {
    let mut removed_events = Vec::new();
    let new_content = mutate_json(content, |root| {
        let Some(group) = root.get_mut("ai-memory").and_then(|g| g.as_object_mut()) else {
            return Ok(());
        };
        let events: Vec<String> = group.keys().cloned().collect();
        for event in events {
            let Some(arr) = group.get_mut(&event).and_then(|v| v.as_array_mut()) else {
                continue;
            };
            let mut removed_from_event = false;
            arr.retain_mut(|entry| {
                let (removed, remove_entry) = strip_hook_entry(entry);
                removed_from_event |= removed;
                !remove_entry
            });
            if removed_from_event {
                removed_events.push(format!("ai-memory.{event}"));
            }
            if arr.is_empty() {
                group.remove(&event);
            }
        }
        if group.is_empty() {
            root.remove("ai-memory");
        }
        Ok(())
    })?;
    Ok(HookRemoval {
        new_content,
        removed_events,
    })
}

fn generated_file_is_ours(path: &Path, kind: DeleteKind) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    match kind {
        DeleteKind::OpenCodePlugin => {
            content.contains("Auto-generated by `ai-memory install-hooks --agent opencode --apply`")
                && content.contains("const AGENT = \"open-code\";")
        }
        DeleteKind::OmpExtension => {
            content.contains("Auto-generated by `ai-memory install-hooks --agent omp --apply`")
                && content.contains("const AGENT = \"omp\";")
        }
        DeleteKind::OpenClawEntrypoint => {
            content.contains("Auto-generated by `ai-memory install-hooks --agent openclaw --apply`")
                && content.contains("definePluginEntry")
                && content.contains("id: \"ai-memory\"")
        }
        DeleteKind::OpenClawPackageJson => serde_json::from_str::<serde_json::Value>(&content)
            .ok()
            .is_some_and(|v| {
                v.get("name").and_then(|name| name.as_str()) == Some(openclaw_plugin::PACKAGE_NAME)
                    && v.get("private").and_then(|private| private.as_bool()) == Some(true)
                    && v.get("type").and_then(|ty| ty.as_str()) == Some("module")
                    && v.pointer("/openclaw/extensions")
                        .and_then(|extensions| extensions.as_array())
                        .is_some_and(|extensions| {
                            extensions.len() == 1
                                && extensions[0].as_str()
                                    == Some(&format!("./{}", openclaw_plugin::ENTRYPOINT_TS))
                        })
            }),
        DeleteKind::OpenClawManifest => serde_json::from_str::<serde_json::Value>(&content)
            .ok()
            .is_some_and(|v| {
                v.get("id").and_then(|id| id.as_str()) == Some(openclaw_plugin::PLUGIN_ID)
                    && v.get("name").and_then(|name| name.as_str()) == Some("ai-memory")
                    && v.pointer("/activation/onCapabilities")
                        .and_then(|capabilities| capabilities.as_array())
                        .is_some_and(|capabilities| {
                            capabilities
                                .iter()
                                .any(|entry| entry.as_str() == Some("hook"))
                        })
                    && v.pointer("/configSchema/additionalProperties")
                        .and_then(|additional| additional.as_bool())
                        == Some(false)
            }),
    }
}

/// Where the servers object lives in each JSON client's config.
/// (Codex is TOML — handled separately in Task 5.)
fn mcp_servers_path(client: McpClient) -> Option<&'static [&'static str]> {
    match client {
        McpClient::ClaudeCode
        | McpClient::ClaudeDesktop
        | McpClient::Cursor
        | McpClient::GeminiCli
        | McpClient::Pi
        | McpClient::AntigravityCli => Some(&["mcpServers"]),
        McpClient::OpenCode => Some(&["mcp"]),
        McpClient::Openclaw => Some(&["mcp", "servers"]),
        McpClient::Codex => None,
    }
}

/// True when an MCP server entry is ai-memory's: its url/httpUrl/serverUrl
/// equals the endpoint, or it is a `mcp-remote` stdio shim whose args contain
/// the endpoint. The key/name alone is intentionally not enough: users may
/// have unrelated entries named `ai-memory`, and uninstall must not remove
/// them unless the endpoint also matches.
fn mcp_entry_is_ours(key: &str, entry: &serde_json::Value, name: Option<&str>, url: &str) -> bool {
    if name.is_some_and(|name| key != name) {
        return false;
    }
    for field in ["url", "httpUrl", "serverUrl"] {
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
    name: Option<&str>,
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
fn strip_mcp_toml(content: &str, name: Option<&str>, url: &str) -> Result<(String, Vec<String>)> {
    use toml_edit::{Item, Value};

    let mut removed = Vec::new();
    let new_content = mutate_toml(content, |doc| {
        let Some(servers_item) = doc.get_mut("mcp_servers") else {
            return Ok(());
        };
        let mut remove_mcp_servers = false;
        match servers_item {
            Item::Table(servers) => {
                let keys: Vec<String> = servers.iter().map(|(k, _)| k.to_string()).collect();
                for k in keys {
                    let matches_url = servers
                        .get(&k)
                        .and_then(|item| item.as_table())
                        .and_then(|t| t.get("url"))
                        .and_then(|u| u.as_str())
                        == Some(url);
                    if name.is_none_or(|name| k == name) && matches_url {
                        servers.remove(&k);
                        removed.push(k);
                    }
                }
            }
            Item::Value(Value::InlineTable(servers)) => {
                let keys: Vec<String> = servers.iter().map(|(k, _)| k.to_string()).collect();
                for k in keys {
                    let matches_url = servers
                        .get(&k)
                        .and_then(|value| value.as_inline_table())
                        .and_then(|table| table.get("url"))
                        .and_then(|value| value.as_str())
                        == Some(url);
                    if name.is_none_or(|name| k == name) && matches_url {
                        servers.remove(&k);
                        removed.push(k);
                    }
                }
                if servers.is_empty() {
                    remove_mcp_servers = true;
                }
            }
            _ => {}
        }
        if remove_mcp_servers {
            doc.remove("mcp_servers");
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
        assert_eq!(
            stripped, original,
            "uninstall must restore the original file"
        );
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
    fn hook_signature_matches_native_binary_command() {
        // `posix-native` / `windows-native`: a direct binary call, no
        // `AI_MEMORY_HOOK_URL=` prefix. The `hook --event`/`--agent`/
        // `--server-url` triple is the signature.
        let posix = "'/home/u/.cargo/bin/ai-memory' hook --event stop --agent claude-code --server-url 'http://127.0.0.1:49374'";
        assert!(hook_command_is_ours(posix));
        let win = "\"C:\\Users\\u\\.cargo\\bin\\ai-memory.exe\" hook --event session-start --agent claude-code --server-url \"http://h:49374\" --auth-token \"tok\"";
        assert!(hook_command_is_ours(win));
    }

    #[test]
    fn hook_signature_rejects_third_party_with_generic_name() {
        // A user's own hook that happens to be named stop.sh — no prefix.
        assert!(!hook_command_is_ours("/usr/local/bin/my-stop.sh"));
        assert!(!hook_command_is_ours("/opt/tools/hooks/session-start.sh"));
        // A third-party tool that merely mentions "hook" is not ours: it
        // lacks the full subcommand triple.
        assert!(!hook_command_is_ours("/usr/bin/git-hook --event push"));
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
        assert!(
            arr[0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("/home/u/scripts/stop.sh")
        );
    }

    #[test]
    fn strip_hooks_nested_mixed_inner_commands_preserves_user_hook() {
        let content = r#"{
      "hooks": {
        "Stop": [
          {"matcher":"","hooks":[
            {"type":"command","command":"AI_MEMORY_HOOK_URL=x /a/stop.sh"},
            {"type":"command","command":"/home/u/scripts/my-stop.sh"}
          ]}
        ]
      }
    }"#;

        let out = strip_ai_memory_hooks(content).unwrap();
        assert_eq!(out.removed_events, vec!["Stop".to_string()]);
        let v: serde_json::Value = serde_json::from_str(&out.new_content).unwrap();
        let inner = v["hooks"]["Stop"][0]["hooks"].as_array().unwrap();
        assert_eq!(inner.len(), 1, "third-party inner hook must survive");
        assert_eq!(
            inner[0]["command"].as_str(),
            Some("/home/u/scripts/my-stop.sh")
        );
    }

    #[test]
    fn strip_hooks_no_hooks_key_is_noop() {
        let content = r#"{"unrelated":true}"#;
        let out = strip_ai_memory_hooks(content).unwrap();
        assert!(out.removed_events.is_empty());
    }

    #[test]
    fn strip_antigravity_hooks_removes_only_signed_entries() {
        let content = r#"{
          "ai-memory": {
            "PreInvocation": [
              {"type":"command","command":"AI_MEMORY_HOOK_URL=http://h /x/session-start.sh"},
              {"type":"command","command":"/usr/bin/user-hook"}
            ],
            "Stop": [
              {"type":"command","command":"AI_MEMORY_HOOK_URL=http://h /x/stop.sh"}
            ]
          },
          "my-group": {
            "Stop": [{"type":"command","command":"/usr/bin/other"}]
          }
        }"#;

        let out = strip_antigravity_hooks(content).unwrap();
        assert_eq!(
            out.removed_events,
            vec![
                "ai-memory.PreInvocation".to_string(),
                "ai-memory.Stop".to_string()
            ]
        );
        let v: serde_json::Value = serde_json::from_str(&out.new_content).unwrap();
        assert_eq!(v["ai-memory"]["PreInvocation"].as_array().unwrap().len(), 1);
        assert!(v["ai-memory"].get("Stop").is_none());
        assert!(v.get("my-group").is_some());
    }

    #[test]
    fn strip_antigravity_hooks_preserves_mixed_nested_user_hook() {
        let content = r#"{
          "ai-memory": {
            "Stop": [
              {"matcher":"","hooks":[
                {"type":"command","command":"AI_MEMORY_HOOK_URL=http://h /x/stop.sh"},
                {"type":"command","command":"/usr/bin/user-stop"}
              ]}
            ]
          }
        }"#;

        let out = strip_antigravity_hooks(content).unwrap();
        assert_eq!(out.removed_events, vec!["ai-memory.Stop".to_string()]);
        let v: serde_json::Value = serde_json::from_str(&out.new_content).unwrap();
        let inner = v["ai-memory"]["Stop"][0]["hooks"].as_array().unwrap();
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0]["command"].as_str(), Some("/usr/bin/user-stop"));
    }

    #[test]
    fn generated_file_detection_rejects_user_files_at_ours_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ai-memory.ts");
        std::fs::write(&path, "// my personal plugin named ai-memory\n").unwrap();

        assert!(!generated_file_is_ours(&path, DeleteKind::OpenCodePlugin));

        std::fs::write(
            &path,
            "// Auto-generated by `ai-memory install-hooks --agent opencode --apply`.\nconst AGENT = \"open-code\";\n",
        )
        .unwrap();
        assert!(generated_file_is_ours(&path, DeleteKind::OpenCodePlugin));
    }

    #[test]
    fn generated_openclaw_package_detection_requires_our_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package.json");
        std::fs::write(&path, r#"{"name":"@ai-memory/openclaw-plugin"}"#).unwrap();

        assert!(!generated_file_is_ours(
            &path,
            DeleteKind::OpenClawPackageJson
        ));

        std::fs::write(&path, openclaw_plugin::package_json()).unwrap();
        assert!(generated_file_is_ours(
            &path,
            DeleteKind::OpenClawPackageJson
        ));

        std::fs::write(
            &path,
            r#"{"name":"@ai-memory/openclaw-plugin","version":"0.0.1","private":true,"type":"module","openclaw":{"extensions":["./index.ts"]}}"#,
        )
        .unwrap();
        assert!(
            generated_file_is_ours(&path, DeleteKind::OpenClawPackageJson),
            "older generated package versions should still uninstall"
        );
    }

    #[test]
    fn generated_openclaw_manifest_detection_requires_our_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("openclaw.plugin.json");
        std::fs::write(
            &path,
            r#"{"id":"ai-memory","name":"ai-memory","description":"custom user plugin"}"#,
        )
        .unwrap();

        assert!(!generated_file_is_ours(&path, DeleteKind::OpenClawManifest));

        std::fs::write(&path, openclaw_plugin::manifest_json()).unwrap();
        assert!(generated_file_is_ours(&path, DeleteKind::OpenClawManifest));

        std::fs::write(
            &path,
            r#"{"id":"ai-memory","name":"ai-memory","description":"older generated text","activation":{"onCapabilities":["hook"]},"configSchema":{"type":"object","additionalProperties":false,"properties":{}}}"#,
        )
        .unwrap();
        assert!(
            generated_file_is_ours(&path, DeleteKind::OpenClawManifest),
            "older generated manifest descriptions should still uninstall"
        );
    }

    #[test]
    fn strip_mcp_claude_by_name_keeps_others() {
        let content = r#"{"mcpServers":{"ai-memory":{"type":"http","url":"http://127.0.0.1:49374/mcp"},"other":{"url":"http://x"}}}"#;
        let (out, removed) = strip_mcp_json(
            content,
            McpClient::ClaudeCode,
            Some("ai-memory"),
            "http://127.0.0.1:49374/mcp",
        )
        .unwrap();
        assert_eq!(removed, vec!["ai-memory".to_string()]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["mcpServers"].get("ai-memory").is_none());
        assert!(v["mcpServers"].get("other").is_some());
    }

    #[test]
    fn strip_mcp_by_endpoint_under_custom_name() {
        let content = r#"{"mcpServers":{"my-mem":{"url":"http://127.0.0.1:49374/mcp"}}}"#;
        let (out, removed) = strip_mcp_json(
            content,
            McpClient::ClaudeCode,
            None,
            "http://127.0.0.1:49374/mcp",
        )
        .unwrap();
        assert_eq!(
            removed,
            vec!["my-mem".to_string()],
            "matched by endpoint, not name"
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(
            v.get("mcpServers").is_none(),
            "emptied servers object pruned"
        );
    }

    #[test]
    fn strip_mcp_name_only_does_not_remove_user_entry() {
        let content = r#"{"mcpServers":{"ai-memory":{"url":"http://example.invalid/mcp"}}}"#;
        let (out, removed) = strip_mcp_json(
            content,
            McpClient::ClaudeCode,
            Some("ai-memory"),
            "http://127.0.0.1:49374/mcp",
        )
        .unwrap();

        assert!(removed.is_empty());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["mcpServers"].get("ai-memory").is_some());
    }

    #[test]
    fn strip_mcp_antigravity_server_url() {
        let content = r#"{"mcpServers":{"mem":{"serverUrl":"http://127.0.0.1:49374/mcp"},"other":{"serverUrl":"http://x"}}}"#;
        let (out, removed) = strip_mcp_json(
            content,
            McpClient::AntigravityCli,
            None,
            "http://127.0.0.1:49374/mcp",
        )
        .unwrap();

        assert_eq!(removed, vec!["mem".to_string()]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["mcpServers"].get("mem").is_none());
        assert!(v["mcpServers"].get("other").is_some());
    }

    #[test]
    fn strip_mcp_pi_root_servers() {
        let content = r#"{"mcpServers":{"ai-memory":{"type":"http","url":"http://127.0.0.1:49374/mcp","enabled":true}}}"#;
        let (out, removed) = strip_mcp_json(
            content,
            McpClient::Pi,
            Some("ai-memory"),
            "http://127.0.0.1:49374/mcp",
        )
        .unwrap();

        assert_eq!(removed, vec!["ai-memory".to_string()]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("mcpServers").is_none());
    }

    #[test]
    fn strip_mcp_claude_desktop_mcp_remote_args() {
        let content = r#"{"mcpServers":{"weird-name":{"command":"npx","args":["-y","mcp-remote","http://127.0.0.1:49374/mcp"]}}}"#;
        let (_out, removed) = strip_mcp_json(
            content,
            McpClient::ClaudeDesktop,
            None,
            "http://127.0.0.1:49374/mcp",
        )
        .unwrap();
        assert_eq!(removed, vec!["weird-name".to_string()]);
    }

    #[test]
    fn strip_mcp_openclaw_nested_servers() {
        let content = r#"{"mcp":{"servers":{"ai-memory":{"url":"http://127.0.0.1:49374/mcp"}}}}"#;
        let (out, removed) = strip_mcp_json(
            content,
            McpClient::Openclaw,
            Some("ai-memory"),
            "http://127.0.0.1:49374/mcp",
        )
        .unwrap();
        assert_eq!(removed, vec!["ai-memory".to_string()]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["mcp"].get("servers").is_none());
    }

    #[test]
    fn strip_mcp_no_match_is_noop() {
        let content = r#"{"mcpServers":{"other":{"url":"http://x"}}}"#;
        let (_out, removed) = strip_mcp_json(
            content,
            McpClient::ClaudeCode,
            Some("ai-memory"),
            "http://127.0.0.1:49374/mcp",
        )
        .unwrap();
        assert!(removed.is_empty());
    }

    #[test]
    fn strip_mcp_toml_by_name_keeps_comments_and_tables() {
        let content = "# my codex config\n[other]\nkeep = true\n\n[mcp_servers.ai-memory]\nurl = \"http://127.0.0.1:49374/mcp\"\n";
        let (out, removed) =
            strip_mcp_toml(content, Some("ai-memory"), "http://127.0.0.1:49374/mcp").unwrap();
        assert_eq!(removed, vec!["ai-memory".to_string()]);
        assert!(out.contains("# my codex config"));
        assert!(out.contains("[other]"));
        assert!(!out.contains("[mcp_servers.ai-memory]"));
    }

    #[test]
    fn strip_mcp_toml_by_url_under_custom_name() {
        let content = "[mcp_servers.custom]\nurl = \"http://127.0.0.1:49374/mcp\"\n";
        let (out, removed) = strip_mcp_toml(content, None, "http://127.0.0.1:49374/mcp").unwrap();
        assert_eq!(removed, vec!["custom".to_string()]);
        assert!(!out.contains("custom"));
    }

    #[test]
    fn strip_mcp_toml_inline_table_by_url_under_custom_name() {
        let content = "mcp_servers = { custom = { url = \"http://127.0.0.1:49374/mcp\" }, other = { url = \"http://x\" } }\n";
        let (out, removed) = strip_mcp_toml(content, None, "http://127.0.0.1:49374/mcp").unwrap();
        assert_eq!(removed, vec!["custom".to_string()]);
        assert!(!out.contains("custom"));
        assert!(out.contains("other"));
    }

    #[test]
    fn strip_mcp_toml_inline_table_prunes_when_empty() {
        let content = "mcp_servers = { ai-memory = { url = \"http://127.0.0.1:49374/mcp\" } }\n";
        let (out, removed) = strip_mcp_toml(content, None, "http://127.0.0.1:49374/mcp").unwrap();
        assert_eq!(removed, vec!["ai-memory".to_string()]);
        assert!(!out.contains("mcp_servers"));
    }

    #[test]
    fn strip_mcp_toml_no_match_is_noop() {
        let content = "[mcp_servers.other]\nurl = \"http://x\"\n";
        let (_out, removed) =
            strip_mcp_toml(content, Some("ai-memory"), "http://127.0.0.1:49374/mcp").unwrap();
        assert!(removed.is_empty());
    }
}
