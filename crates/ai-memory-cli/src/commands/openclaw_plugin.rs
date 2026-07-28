//! Generated OpenClaw lifecycle plugin support.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::cli::InstallHooksArgs;
use crate::commands::apply_shared::{ApplyOutcome, apply_atomic};
use crate::commands::render_shared::{ts_capture_policy_v1, ts_string_literal};

pub(crate) const PLUGIN_ID: &str = "ai-memory";
pub(crate) const PACKAGE_NAME: &str = "@ai-memory/openclaw-plugin";
pub(crate) const PACKAGE_JSON: &str = "package.json";
pub(crate) const MANIFEST_JSON: &str = "openclaw.plugin.json";
pub(crate) const ENTRYPOINT_TS: &str = "index.ts";
const OPENCLAW_BIN: &str = "openclaw";

/// Write and install the generated OpenClaw plugin package.
pub(crate) fn apply(
    server_url: &str,
    auth_token: Option<&str>,
    args: &InstallHooksArgs,
) -> Result<()> {
    let plugin_dir = resolve_plugin_dir(args)?;
    let strategy = args
        .project_strategy
        .and_then(crate::cli::ProjectStrategyArg::baked);
    let outcomes = write_package(&plugin_dir, server_url, auth_token, strategy)?;
    for (path, outcome) in &outcomes {
        println!(
            "✓ {} {} ({})",
            outcome.verb(),
            path.display(),
            outcome_detail(*outcome)
        );
    }

    match install_plugin(&plugin_dir)? {
        InstallStatus::Installed => {
            println!();
            println!("OpenClaw plugin installed from {}.", plugin_dir.display());
            println!(
                "If your OpenClaw gateway did not auto-restart, run `openclaw gateway restart`."
            );
            println!("Verify with `openclaw plugins inspect ai-memory --runtime --json`.");
        }
        InstallStatus::CliMissing => {
            println!();
            println!("OpenClaw CLI not found on PATH; plugin package was written only.");
            println!("Install it with:");
            println!(
                "  openclaw plugins install --link {} --force",
                plugin_dir.display()
            );
            println!("  openclaw gateway restart");
            println!("  openclaw plugins inspect ai-memory --runtime --json");
        }
    }
    Ok(())
}

/// Print the generated package for manual installation.
pub(crate) fn render(server_url: &str, auth_token: Option<&str>, project_strategy: Option<&str>) {
    println!("# OpenClaw native plugin package");
    println!("# Re-run with `--apply` to write the package and call:");
    println!("#   openclaw plugins install --link <package-dir> --force");
    println!("# OpenClaw loads plugin code at gateway startup; restart if your");
    println!("# managed gateway does not auto-restart after install.");
    println!();
    println!("## {PACKAGE_JSON}");
    println!("{}", package_json());
    println!("## {MANIFEST_JSON}");
    println!("{}", manifest_json());
    println!("## {ENTRYPOINT_TS}");
    println!("{}", build_plugin(server_url, auth_token, project_strategy));
}

fn outcome_detail(outcome: ApplyOutcome) -> &'static str {
    match outcome {
        ApplyOutcome::Created => "new file",
        ApplyOutcome::Updated => "backup written next to it",
        ApplyOutcome::NoOp => "already up to date",
    }
}

fn resolve_plugin_dir(args: &InstallHooksArgs) -> Result<PathBuf> {
    if let Some(path) = &args.config_file {
        return Ok(path.clone());
    }
    default_plugin_dir()
}

pub(crate) fn default_plugin_dir() -> Result<PathBuf> {
    Ok(dirs::data_local_dir()
        .context("could not locate the user data-local directory")?
        .join("ai-memory")
        .join("openclaw-plugin"))
}

fn write_package(
    plugin_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
) -> Result<Vec<(PathBuf, ApplyOutcome)>> {
    let files = [
        (PACKAGE_JSON, package_json()),
        (MANIFEST_JSON, manifest_json()),
        (
            ENTRYPOINT_TS,
            build_plugin(server_url, auth_token, project_strategy),
        ),
    ];
    let mut outcomes = Vec::with_capacity(files.len());
    for (name, body) in files {
        let path = plugin_dir.join(name);
        let outcome = apply_atomic(&path, move |_existing| Ok(body.clone()))?;
        outcomes.push((path, outcome));
    }
    Ok(outcomes)
}

enum InstallStatus {
    Installed,
    CliMissing,
}

fn install_plugin(plugin_dir: &Path) -> Result<InstallStatus> {
    let output = match Command::new(OPENCLAW_BIN)
        .args(["plugins", "install", "--link"])
        .arg(plugin_dir)
        .arg("--force")
        .output()
    {
        Ok(output) => output,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(InstallStatus::CliMissing),
        Err(e) => return Err(e).context("running openclaw plugins install"),
    };
    if !output.status.success() {
        anyhow::bail!(
            "openclaw plugins install failed with status {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let enable = Command::new(OPENCLAW_BIN)
        .args(["plugins", "enable", PLUGIN_ID])
        .output()
        .context("running openclaw plugins enable")?;
    if !enable.status.success() {
        eprintln!(
            "# warning: `openclaw plugins enable ai-memory` exited with {}\n# stdout:\n{}\n# stderr:\n{}",
            enable.status,
            String::from_utf8_lossy(&enable.stdout),
            String::from_utf8_lossy(&enable.stderr)
        );
    }

    Ok(InstallStatus::Installed)
}

pub(crate) fn package_json() -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "name": PACKAGE_NAME,
        "version": env!("CARGO_PKG_VERSION"),
        "private": true,
        "type": "module",
        "openclaw": {
            "extensions": [format!("./{ENTRYPOINT_TS}")]
        }
    }))
    .expect("OpenClaw package metadata serializes")
        + "\n"
}

pub(crate) fn manifest_json() -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "id": PLUGIN_ID,
        "name": "ai-memory",
        "description": "Capture OpenClaw session lifecycle, tool use, compaction, and handoffs into ai-memory.",
        "activation": {
            "onCapabilities": ["hook"]
        },
        "configSchema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {}
        }
    }))
    .expect("OpenClaw manifest serializes")
        + "\n"
}

/// Emit the OpenClaw plugin's `applyMarkerParams` TypeScript function.
///
/// `None` reproduces the historical marker-only function byte-for-byte (the
/// OpenClaw variant always sets `cwd`, even with no marker). `Some(default)`
/// prepends a `DEFAULT_PROJECT_STRATEGY` const and applies that install-time
/// default when no marker pins a `project_strategy` (#128); a marker's own
/// `project` / `project_strategy` still win (§3.3). Mirrors the opencode/omp
/// `ts_apply_marker_params` in `install_hooks.rs`.
fn apply_marker_params_ts(default_strategy: Option<&str>) -> String {
    let toml_flag = super::install_hooks::TS_TOML_FLAG;
    let Some(default) = default_strategy else {
        return format!(
            "{toml_flag}\n{}",
            r#"function applyMarkerParams(url: URL, cwd: string | undefined): void {
  if (!cwd) return;
  url.searchParams.set("cwd", cwd);
  const marker = findMarker(cwd);
  if (!marker) return;
  try {
    const body = readFileSync(marker, "utf8");
    const workspace = tomlKey(body, "workspace");
    const project = tomlKey(body, "project");
    const projectStrategy = tomlKey(body, "project_strategy");
    const dropSubagent = tomlKey(body, "drop_subagent_captures");
    const defaultGlobal = tomlFlag(body, "default_global");
    const briefing = tomlFlag(body, "inject_on_session_start");
    const briefingBudget = tomlFlag(body, "max_chars");
    if (workspace) url.searchParams.set("workspace", workspace);
    if (project) url.searchParams.set("project", project);
    if (projectStrategy) url.searchParams.set("project_strategy", projectStrategy);
    if (dropSubagent) url.searchParams.set("drop_subagent", dropSubagent);
    if (defaultGlobal) url.searchParams.set("default_global", defaultGlobal);
    if (briefing) url.searchParams.set("briefing", briefing);
    if (briefingBudget) url.searchParams.set("briefing_budget", briefingBudget);
    if (!project && (projectStrategy === "repo-root" || projectStrategy === "repo_root")) {
      const repoProject = repoRootProject(cwd);
      if (repoProject) url.searchParams.set("project", repoProject);
    }
  } catch (_e) {
  }
}"#
        );
    };
    let body = r#"function applyMarkerParams(url: URL, cwd: string | undefined): void {
  if (!cwd) return;
  url.searchParams.set("cwd", cwd);
  let workspace: string | undefined;
  let project: string | undefined;
  let projectStrategy: string | undefined;
  let dropSubagent: string | undefined;
  let defaultGlobal: string | undefined;
  let briefing: string | undefined;
  let briefingBudget: string | undefined;
  const marker = findMarker(cwd);
  if (marker) {
    try {
      const body = readFileSync(marker, "utf8");
      workspace = tomlKey(body, "workspace");
      project = tomlKey(body, "project");
      projectStrategy = tomlKey(body, "project_strategy");
      dropSubagent = tomlKey(body, "drop_subagent_captures");
      defaultGlobal = tomlFlag(body, "default_global");
      briefing = tomlFlag(body, "inject_on_session_start");
      briefingBudget = tomlFlag(body, "max_chars");
    } catch (_e) {
    }
  }
  if (!projectStrategy) projectStrategy = DEFAULT_PROJECT_STRATEGY;
  if (!project && (projectStrategy === "repo-root" || projectStrategy === "repo_root")) {
    const repoProject = repoRootProject(cwd);
    if (repoProject) project = repoProject;
  }
  if (workspace) url.searchParams.set("workspace", workspace);
  if (project) url.searchParams.set("project", project);
  if (projectStrategy) url.searchParams.set("project_strategy", projectStrategy);
  if (dropSubagent) url.searchParams.set("drop_subagent", dropSubagent);
  if (defaultGlobal) url.searchParams.set("default_global", defaultGlobal);
  if (briefing) url.searchParams.set("briefing", briefing);
  if (briefingBudget) url.searchParams.set("briefing_budget", briefingBudget);
}"#;
    format!(
        "const DEFAULT_PROJECT_STRATEGY = {};\n{toml_flag}\n{body}",
        ts_string_literal(default)
    )
}

fn build_plugin(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
) -> String {
    let token_line = auth_token
        .map(|t| format!("const TOKEN: string | null = {};\n", ts_string_literal(t)))
        .unwrap_or_else(|| "const TOKEN: string | null = null;\n".to_string());
    let apply_marker_params = apply_marker_params_ts(project_strategy);
    let capture_policy = ts_capture_policy_v1();
    format!(
        r#"// Auto-generated by `ai-memory install-hooks --agent openclaw --apply`.
// Edit by re-running the command, not by hand. install-hooks owns
// this local OpenClaw plugin package.

import {{ definePluginEntry }} from "openclaw/plugin-sdk/plugin-entry";
import {{ execFileSync }} from "node:child_process";
import {{ closeSync, existsSync, openSync, readFileSync as readMarkerText, readSync }} from "node:fs";
import {{ basename, dirname, join, resolve, sep }} from "node:path";
import {{ homedir }} from "node:os";

const SERVER = {server_literal}.replace(/\/+$/, "");
const AGENT = "openclaw";
{token_line}
{capture_policy}

function timeoutSignal(ms: number): AbortSignal | undefined {{
  if (typeof AbortSignal === "undefined") return undefined;
  const factory = (AbortSignal as unknown as {{ timeout?: (ms: number) => AbortSignal }}).timeout;
  return factory ? factory(ms) : undefined;
}}

function authHeaders(): Record<string, string> {{
  return TOKEN ? {{ Authorization: `Bearer ${{TOKEN}}` }} : {{}};
}}

function findMarker(cwd: string | undefined): string | undefined {{
  if (!cwd) return undefined;
  let dir = resolve(cwd);
  const home = homedir();
  let boundary: string | undefined;
  if (home && (dir === home || dir.startsWith(home.endsWith(sep) ? home : home + sep))) {{
    boundary = home;
  }} else if (home) {{
    let probe = dir;
    while (probe && probe !== dirname(probe)) {{
      if (existsSync(join(probe, ".git"))) {{
        boundary = probe;
        break;
      }}
      probe = dirname(probe);
    }}
    boundary ??= dir;
  }}
  while (dir && dir !== dirname(dir)) {{
    const marker = join(dir, ".ai-memory.toml");
    if (existsSync(marker)) return marker;
    if (boundary && dir === boundary) return undefined;
    dir = dirname(dir);
  }}
  return undefined;
}}

function tomlKey(text: string, key: string): string | undefined {{
  const re = new RegExp(`^\\s*${{key}}\\s*=\\s*"([^"]*)"`);
  for (const line of text.split(/\r?\n/)) {{
    const match = re.exec(line);
    if (match) return match[1];
  }}
  return undefined;
}}


function repoRootProject(cwd: string | undefined): string | undefined {{
  if (!cwd) return undefined;
  try {{
    const inside = execFileSync("git", ["-C", cwd, "rev-parse", "--is-inside-work-tree"], {{
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }}).trim();
    if (inside !== "true") return undefined;
    const common = execFileSync("git", ["-C", cwd, "rev-parse", "--path-format=absolute", "--git-common-dir"], {{
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }}).trim();
    if (!common) return undefined;
    const root = dirname(common);
    if (!root || root === dirname(root)) return undefined;
    return basename(root);
  }} catch (_e) {{
    return undefined;
  }}
}}
{apply_marker_params}

function textFrom(value: unknown): string {{
  if (value === null || value === undefined) return "";
  if (typeof value === "string") return value;
  if (Array.isArray(value)) return value.map(textFrom).filter(Boolean).join("\n\n").trim();
  const obj = value as any;
  if (typeof obj.text === "string") return obj.text;
  if (typeof obj.content === "string") return obj.content;
  if (typeof obj.prompt === "string") return obj.prompt;
  try {{
    return JSON.stringify(value);
  }} catch (_e) {{
    return String(value);
  }}
}}

function sessionID(event: any, ctx: any): string | undefined {{
  const value = ctx?.sessionId ?? ctx?.sessionID ?? ctx?.sessionKey ?? event?.sessionId ?? event?.sessionID ?? event?.sessionKey;
  return typeof value === "string" && value.length > 0 ? value : undefined;
}}

function cwd(event: any, ctx: any): string | undefined {{
  const value = ctx?.workspaceDir ?? ctx?.cwd ?? event?.cwd ?? event?.workspaceDir;
  return typeof value === "string" && value.length > 0 ? value : undefined;
}}

function payload(event: any, ctx: any, extra: Record<string, unknown> = {{}}): Record<string, unknown> {{
  return {{
    sessionID: sessionID(event, ctx),
    cwd: cwd(event, ctx),
    agentID: ctx?.agentId,
    runID: ctx?.runId ?? event?.runId,
    jobID: ctx?.jobId,
    ...extra,
  }};
}}

const startedSessions = new Set<string>();
const handoffChecked = new Set<string>();
const preCompactLast = new Map<string, number>();

function rememberSession(event: any, ctx: any): void {{
  const id = sessionID(event, ctx);
  if (!id || startedSessions.has(id)) return;
  startedSessions.add(id);
  postHook("session-start", payload(event, ctx, {{ reason: event?.reason }}));
}}

function postPreCompact(event: any, ctx: any): void {{
  rememberSession(event, ctx);
  const key = sessionID(event, ctx) || "unknown";
  const now = Date.now();
  const last = preCompactLast.get(key) ?? 0;
  if (now - last < 1000) return;
  preCompactLast.set(key, now);
  postHook("pre-compact", payload(event, ctx, {{ reason: event?.reason }}));
}}

function postHook(eventName: string, body: Record<string, unknown>): void {{
  const url = new URL(`${{SERVER}}/hook`);
  url.searchParams.set("event", eventName);
  url.searchParams.set("agent", AGENT);
  applyMarkerParams(url, typeof body.cwd === "string" ? body.cwd : undefined);
  const policy = capturePolicy(body, typeof body.cwd === "string" ? body.cwd : undefined);
  if (policy.disposition === "drop") return;
  try {{
    void fetch(url, {{
      method: "POST",
      headers: {{ "Content-Type": "application/json", ...authHeaders() }},
      body: JSON.stringify(policy.payload),
      signal: timeoutSignal(500),
    }}).catch(() => undefined);
  }} catch (_e) {{
    // Fire-and-forget. Hooks must never block OpenClaw.
  }}
}}

async function fetchHandoff(event: any, ctx: any): Promise<string | undefined> {{
  const currentCwd = cwd(event, ctx);
  if (!currentCwd) return undefined;
  const url = new URL(`${{SERVER}}/handoff`);
  url.searchParams.set("agent", AGENT);
  applyMarkerParams(url, currentCwd);
  try {{
    const response = await fetch(url, {{
      headers: authHeaders(),
      signal: timeoutSignal(1000),
    }});
    const text = (await response.text()).trim();
    return text.length > 0 ? text : undefined;
  }} catch (_e) {{
    return undefined;
  }}
}}

export default definePluginEntry({{
  id: "ai-memory",
  name: "ai-memory",
  description: "Capture OpenClaw lifecycle events into ai-memory.",
  register(api) {{
    api.on("session_start", (event: any, ctx: any) => {{
      rememberSession(event, ctx);
    }});

    api.on("session_end", (event: any, ctx: any) => {{
      rememberSession(event, ctx);
      postHook("session-end", payload(event, ctx, {{ reason: event?.reason }}));
    }});

    api.on("before_prompt_build", async (event: any, ctx: any) => {{
      rememberSession(event, ctx);
      postHook("user-prompt", payload(event, ctx, {{
        prompt: textFrom(event?.prompt ?? event?.userPrompt ?? event?.message ?? event?.messages?.at?.(-1)),
      }}));

      const id = sessionID(event, ctx);
      if (!id || handoffChecked.has(id)) return;
      handoffChecked.add(id);
      const handoff = await fetchHandoff(event, ctx);
      return handoff ? {{ prependContext: handoff }} : undefined;
    }});

    api.on("before_tool_call", (event: any, ctx: any) => {{
      rememberSession(event, ctx);
      postHook("pre-tool-use", payload(event, ctx, {{
        tool: event?.toolName,
        toolKind: event?.toolKind,
        callID: event?.toolCallId,
        args: event?.params,
      }}));
    }});

    api.on("after_tool_call", (event: any, ctx: any) => {{
      rememberSession(event, ctx);
      postHook("post-tool-use", payload(event, ctx, {{
        tool: event?.toolName,
        toolKind: event?.toolKind,
        callID: event?.toolCallId,
        args: event?.params,
        output: textFrom(event?.result ?? event?.output ?? event?.content),
        error: event?.error,
        durationMs: event?.durationMs,
      }}));
    }});

    api.on("before_compaction", (event: any, ctx: any) => {{
      postPreCompact(event, ctx);
    }});

    api.on("agent_end", (event: any, ctx: any) => {{
      rememberSession(event, ctx);
      postHook("stop", payload(event, ctx, {{ success: event?.success }}));
    }});
  }},
}});
"#,
        server_literal = ts_string_literal(server_url),
        token_line = token_line,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn package_has_manifest_and_hook_entrypoint() {
        let package = package_json();
        let manifest = manifest_json();
        let plugin = build_plugin("http://127.0.0.1:49374", Some("tok"), None);

        assert!(package.contains(r#""extensions""#));
        assert!(package.contains(r#""./index.ts""#));
        assert!(manifest.contains(r#""id": "ai-memory""#));
        assert!(manifest.contains(r#""onCapabilities""#));
        assert!(manifest.contains(r#""hook""#));
        assert!(plugin.contains("definePluginEntry"));
        assert!(plugin.contains("api.on(\"session_start\""));
        assert!(plugin.contains("api.on(\"session_end\""));
        assert!(plugin.contains("api.on(\"before_prompt_build\""));
        assert!(plugin.contains("api.on(\"before_tool_call\""));
        assert!(plugin.contains("api.on(\"after_tool_call\""));
        assert!(plugin.contains("api.on(\"before_compaction\""));
        assert!(plugin.contains("api.on(\"agent_end\""));
        assert!(plugin.contains("postHook(\"session-start\""));
        assert!(plugin.contains("postHook(\"user-prompt\""));
        assert!(plugin.contains("function applyMarkerParams"));
        assert!(plugin.contains("tomlKey(body, \"project_strategy\")"));
        assert!(plugin.contains("tomlKey(body, \"drop_subagent_captures\")"));
        assert!(plugin.contains("url.searchParams.set(\"drop_subagent\", dropSubagent)"));
        assert!(plugin.contains("function tomlFlag"));
        assert!(plugin.contains("tomlFlag(body, \"default_global\")"));
        assert!(plugin.contains("tomlFlag(body, \"inject_on_session_start\")"));
        assert!(plugin.contains("url.searchParams.set(\"briefing_budget\", briefingBudget)"));
        assert!(plugin.contains("import { execFileSync } from \"node:child_process\";"));
        assert!(
            plugin.contains("import { basename, dirname, join, resolve, sep } from \"node:path\";")
        );
        assert!(plugin.contains("if (existsSync(join(probe, \".git\")))"));
        assert!(plugin.contains("boundary ??= dir;"));
        assert!(plugin.contains("function repoRootProject"));
        assert!(plugin.contains("--git-common-dir"));
        assert!(
            plugin
                .contains("projectStrategy === \"repo-root\" || projectStrategy === \"repo_root\"")
        );
        assert!(plugin.contains("url.searchParams.set(\"project\", repoProject)"));
        assert!(plugin.contains(
            "applyMarkerParams(url, typeof body.cwd === \"string\" ? body.cwd : undefined);"
        ));
        assert!(plugin.contains("applyMarkerParams(url, currentCwd);"));
        assert!(plugin.contains("fetchHandoff"));
        assert!(plugin.contains("// capture-policy-v1 (generated; do not fork between adapters)"));
        assert!(plugin.contains("const policy = capturePolicy(body"));
        assert!(plugin.contains("body: JSON.stringify(policy.payload)"));
        assert!(plugin.contains("prependContext: handoff"));
        assert!(plugin.contains("Bearer ${TOKEN}"));
        assert!(plugin.contains("tok"));
    }

    #[test]
    fn openclaw_plugin_bakes_repo_root_default() {
        let plugin = build_plugin("http://127.0.0.1:49374", Some("tok"), Some("repo-root"));
        assert!(
            plugin.contains("const DEFAULT_PROJECT_STRATEGY = \"repo-root\";"),
            "repo-root install default must bake the const: {plugin}"
        );
        assert!(
            plugin.contains("if (!projectStrategy) projectStrategy = DEFAULT_PROJECT_STRATEGY;"),
            "must apply the default when a marker pins no strategy: {plugin}"
        );
    }

    #[test]
    fn openclaw_plugin_default_omits_baked_strategy() {
        let plugin = build_plugin("http://127.0.0.1:49374", Some("tok"), None);
        assert!(
            !plugin.contains("DEFAULT_PROJECT_STRATEGY"),
            "basename default must bake no strategy: {plugin}"
        );
    }

    #[test]
    fn package_writes_all_required_files() {
        let tmp = TempDir::new().unwrap();
        let outcomes = write_package(tmp.path(), "http://127.0.0.1:49374", None, None).unwrap();

        assert_eq!(outcomes.len(), 3);
        assert!(tmp.path().join(PACKAGE_JSON).is_file());
        assert!(tmp.path().join(MANIFEST_JSON).is_file());
        assert!(tmp.path().join(ENTRYPOINT_TS).is_file());
        assert!(
            std::fs::read_to_string(tmp.path().join(ENTRYPOINT_TS))
                .unwrap()
                .contains("const TOKEN: string | null = null;")
        );
    }
}
