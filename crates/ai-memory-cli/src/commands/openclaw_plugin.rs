//! Generated OpenClaw lifecycle plugin support.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::cli::InstallHooksArgs;
use crate::commands::apply_shared::{ApplyOutcome, apply_atomic};
use crate::commands::render_shared::{
    ts_capture_policy_v1, ts_resolve_token_fn, ts_spool_runtime, ts_string_literal,
    ts_timeout_signal,
};

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
    capture_mode: &str,
) -> Result<()> {
    let plugin_dir = resolve_plugin_dir(args)?;
    let strategy = args
        .project_strategy
        .and_then(crate::cli::ProjectStrategyArg::baked);
    let outcomes = write_package(&plugin_dir, server_url, auth_token, strategy, capture_mode)?;
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
pub(crate) fn render(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
    capture_mode: &str,
) {
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
    println!(
        "{}",
        build_plugin(server_url, auth_token, project_strategy, capture_mode)
    );
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
    capture_mode: &str,
) -> Result<Vec<(PathBuf, ApplyOutcome)>> {
    let files = [
        (PACKAGE_JSON, package_json()),
        (MANIFEST_JSON, manifest_json()),
        (
            ENTRYPOINT_TS,
            build_plugin(server_url, auth_token, project_strategy, capture_mode),
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

/// Emit OpenClaw's `applyMarkerParams` TypeScript from the shared generator.
///
/// OpenClaw omits managed-run forwarding but otherwise uses the shared
/// repository-identity/default-path routing generated for TypeScript clients.
fn apply_marker_params_ts(default_strategy: Option<&str>) -> String {
    super::install_hooks::ts_apply_marker_params(default_strategy, true).replacen(
        "  const managedRun = process.env.AI_MEMORY_RUN_ID;\n  if (managedRun) url.searchParams.set(\"managed_run\", managedRun);\n",
        "",
        1,
    )
}

fn build_plugin(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
    capture_mode: &str,
) -> String {
    let token_line = auth_token
        .map(|t| format!("const TOKEN: string | null = {};\n", ts_string_literal(t)))
        .unwrap_or_else(|| "const TOKEN: string | null = null;\n".to_string());
    let resolve_fn = ts_resolve_token_fn();
    let apply_marker_params = apply_marker_params_ts(project_strategy);
    let capture_policy = ts_capture_policy_v1(capture_mode);
    format!(
        r#"// Auto-generated by `ai-memory install-hooks --agent openclaw --apply`.
// Edit by re-running the command, not by hand. install-hooks owns
// this local OpenClaw plugin package.

import {{ definePluginEntry }} from "openclaw/plugin-sdk/plugin-entry";
import {{ execFileSync }} from "node:child_process";
import {{ closeSync, existsSync, mkdirSync, openSync, readFileSync as readMarkerText, readSync, readdirSync, renameSync, statSync, unlinkSync, writeFileSync }} from "node:fs";
import {{ basename, dirname, join, resolve, sep }} from "node:path";
import {{ homedir }} from "node:os";

const SERVER = {server_literal}.replace(/\/+$/, "");
const AGENT = "openclaw";
{token_line}{resolve_fn}
{capture_policy}

{timeout_signal}
function authHeaders(): Record<string, string> {{
  const token = resolveToken();
  return token ? {{ Authorization: `Bearer ${{token}}` }} : {{}};
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


{repo_root_project}
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

const HOOK_DISPOSE_DRAIN_BUDGET_MS = 2000;
const pendingHookRequests = new Set<Promise<void>>();

function trackHookRequest(request: Promise<void>): void {{
  pendingHookRequests.add(request);
  void request.finally(() => pendingHookRequests.delete(request));
}}

function disposeDrainTimeout(): Promise<void> {{
  return new Promise((resolve) => {{
    const timer = setTimeout(resolve, HOOK_DISPOSE_DRAIN_BUDGET_MS);
    timer.unref?.();
  }});
}}

async function drainHookQueueForDispose(): Promise<void> {{
  await Promise.race([
    Promise.allSettled(Array.from(pendingHookRequests)),
    disposeDrainTimeout(),
  ]);
}}

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

{spool_runtime}function postHook(eventName: string, body: Record<string, unknown>): void {{
  const url = new URL(`${{SERVER}}/hook`);
  url.searchParams.set("event", eventName);
  url.searchParams.set("agent", AGENT);
  if (!applyMarkerParams(url, typeof body.cwd === "string" ? body.cwd : undefined)) return;
  const policy = capturePolicy(body, typeof body.cwd === "string" ? body.cwd : undefined);
  if (policy.disposition === "drop") return;
  try {{
    // Fire-and-forget, but never silent loss: an unreachable server, a
    // 5xx or a transient 4xx spools the event in the CLI hook-spool format
    // for a later drain (#580); a delivered post opportunistically drains
    // backlog.
    // Tracked in pendingHookRequests so session_end can await a bounded
    // flush instead of letting teardown kill the in-flight fetch (#676).
    const request = fetch(url, {{
      method: "POST",
      headers: {{ "Content-Type": "application/json", ...authHeaders() }},
      body: JSON.stringify(policy.payload),
      signal: timeoutSignal(500),
    }})
      .catch(() => undefined)
      .then((resp) => {{
        if (!resp || hookStatusRetryable(resp.status)) spoolFailedHook(url, policy.payload);
        else requestSpoolDrain();
      }})
      .catch(() => undefined);
    trackHookRequest(request);
  }} catch (_e) {{
    try {{ spoolFailedHook(url, policy.payload); }} catch (_e2) {{}}
  }}
}}

async function fetchHandoff(event: any, ctx: any): Promise<string | undefined> {{
  const currentCwd = cwd(event, ctx);
  if (!currentCwd) return undefined;
  if (captureServerRouted(currentCwd)) return undefined;
  const url = new URL(`${{SERVER}}/handoff`);
  url.searchParams.set("agent", AGENT);
  if (!applyMarkerParams(url, currentCwd)) return undefined;
  try {{
    const response = await fetch(url, {{
      headers: authHeaders(),
      signal: timeoutSignal(1000),
    }});
    if (!response.ok) return undefined;
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

    api.on("session_end", async (event: any, ctx: any) => {{
      rememberSession(event, ctx);
      postHook("session-end", payload(event, ctx, {{ reason: event?.reason }}));
      await drainHookQueueForDispose();
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
        repo_root_project = super::install_hooks::TS_REPO_ROOT_PROJECT,
        timeout_signal = ts_timeout_signal(),
        spool_runtime = ts_spool_runtime(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn openclaw_plugin_spools_failed_deliveries() {
        // #580: OpenClaw's fire-and-forget postHook must persist failures
        // in the CLI hook-spool format and drain the backlog on success.
        let plugin = build_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist");
        assert!(plugin.contains("function spoolFailedHook("));
        assert!(plugin.contains("async function drainHookSpool()"));
        assert!(plugin.contains(
            "if (!resp || hookStatusRetryable(resp.status)) spoolFailedHook(url, policy.payload);"
        ));
        assert!(plugin.contains("else requestSpoolDrain();"));
        crate::commands::render_shared::assert_shared_ts_delivery_runtime("openclaw", &plugin);
        assert!(plugin.contains(r#"return join(env, "hook-spool");"#));
        for f in [
            "mkdirSync",
            "writeFileSync",
            "renameSync",
            "readdirSync",
            "unlinkSync",
        ] {
            assert!(
                plugin.contains(&format!("{f},")) || plugin.contains(&format!("{f} }}")),
                "node:fs import missing {f}"
            );
        }
    }

    #[test]
    fn package_has_manifest_and_hook_entrypoint() {
        let package = package_json();
        let manifest = manifest_json();
        let plugin = build_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist");

        assert!(package.contains(r#""extensions""#));
        assert!(package.contains(r#""./index.ts""#));
        assert!(manifest.contains(r#""id": "ai-memory""#));
        assert!(manifest.contains(r#""onCapabilities""#));
        assert!(manifest.contains(r#""hook""#));
        assert!(plugin.contains("definePluginEntry"));
        assert!(plugin.contains("api.on(\"session_start\""));
        assert!(plugin.contains("api.on(\"session_end\""));
        // #676: session_end must be async and await a bounded flush of the
        // in-flight postHook fetch, otherwise the gateway tears the plugin
        // down before the session-end request reaches the server.
        assert!(plugin.contains("const HOOK_DISPOSE_DRAIN_BUDGET_MS = 2000;"));
        assert!(plugin.contains("function disposeDrainTimeout(): Promise<void>"));
        assert!(plugin.contains("async function drainHookQueueForDispose(): Promise<void>"));
        assert!(plugin.contains("function trackHookRequest("));
        assert!(plugin.contains("api.on(\"session_end\", async (event: any, ctx: any) => {"));
        assert!(plugin.contains("await drainHookQueueForDispose();"));
        assert!(
            !plugin.contains("api.on(\"session_end\", (event: any, ctx: any) => {"),
            "session_end must not regress to the sync fire-and-forget form: {plugin}"
        );
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
        assert!(plugin.contains("url.searchParams.set(\"profile_contribute\", profileContribute)"));
        assert!(plugin.contains("url.searchParams.set(\"profile_consume\", profileConsume)"));
        assert!(plugin.contains("function tomlFlag"));
        assert!(plugin.contains("tomlFlag(body, \"default_global\")"));
        assert!(plugin.contains("tomlFlag(body, \"inject_on_session_start\")"));
        assert!(plugin.contains("url.searchParams.set(\"briefing_budget\", briefingBudget)"));
        // #668: same settings-walk as the shared ts_apply_marker_params
        // (install_hooks.rs) — the two applyMarkerParams copies stay
        // equivalent, so a nested capture-only marker does not shadow an
        // outer marker's scope here either.
        assert!(plugin.contains("function findSettingsMarker"));
        assert!(plugin.contains("function declaresSettings"));
        assert!(plugin.contains("let marker = findSettingsMarker(cwd);"));
        assert!(plugin.contains("if (declaresSettings(text)) return marker;"));
        assert!(plugin.contains("import { execFileSync } from \"node:child_process\";"));
        assert!(
            plugin.contains("import { basename, dirname, join, resolve, sep } from \"node:path\";")
        );
        assert!(plugin.contains("if (existsSync(join(probe, \".git\")))"));
        assert!(plugin.contains("boundary ??= dir;"));
        assert!(plugin.contains("function repoRootProject"));
        assert!(plugin.contains("repoProjectCache.set(cwd, project);"));
        assert_eq!(plugin.matches("windowsHide: true").count(), 3);
        assert!(plugin.contains("--git-common-dir"));
        assert!(
            plugin
                .contains("projectStrategy === \"repo-root\" || projectStrategy === \"repo_root\"")
        );
        assert!(plugin.contains("project = repoProject;"));
        assert!(plugin.contains("projectSource = \"repo-root\";"));
        assert!(plugin.contains(
            "if (!applyMarkerParams(url, typeof body.cwd === \"string\" ? body.cwd : undefined)) return;"
        ));
        assert!(plugin.contains("if (!applyMarkerParams(url, currentCwd)) return undefined;"));
        assert!(plugin.contains("fetchHandoff"));
        assert!(plugin.contains("// capture-policy-v1 (generated; do not fork between adapters)"));
        assert!(plugin.contains("const policy = capturePolicy(body"));
        assert!(plugin.contains("body: JSON.stringify(policy.payload)"));
        assert!(plugin.contains("prependContext: handoff"));
        assert!(plugin.contains("Bearer ${token}"));
        assert!(plugin.contains("tok"));
    }

    #[test]
    fn openclaw_plugin_resolves_token_at_runtime_when_not_embedded() {
        let plugin = build_plugin("http://127.0.0.1:49374", None, None, "denylist");
        assert!(plugin.contains("function resolveToken("));
        assert!(plugin.contains("const token = resolveToken();"));
        assert!(plugin.contains("if (!response.ok) return undefined;"));
    }

    #[test]
    fn openclaw_plugin_bakes_repo_root_default() {
        let plugin = build_plugin(
            "http://127.0.0.1:49374",
            Some("tok"),
            Some("repo-root"),
            "denylist",
        );
        assert!(
            plugin.contains("const DEFAULT_PROJECT_STRATEGY = \"repo-root\";"),
            "repo-root install default must bake the const: {plugin}"
        );
        assert!(
            plugin.contains("if (!projectStrategy) projectStrategy = DEFAULT_PROJECT_STRATEGY;"),
            "must apply the default when a marker pins no strategy: {plugin}"
        );
        assert!(
            plugin.contains("let marker = findSettingsMarker(cwd);"),
            "the default-strategy variant must also walk past a capture-only marker (#668): {plugin}"
        );
    }

    #[test]
    fn openclaw_executes_shared_home_route_runtime() {
        if !std::process::Command::new("node")
            .args(["--experimental-strip-types", "-e", "0"])
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let repo_root = home.join("src/api");
        let repo = repo_root.join("lib");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&repo_root)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args(["remote", "add", "origin", "git@github.com:acme/api.git"])
                .current_dir(&repo_root)
                .status()
                .unwrap()
                .success()
        );
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../ai-memory-core/fixtures/home_route_cases.json"
        ))
        .unwrap();
        let text = fixture["cases"][0]["toml"]
            .as_str()
            .unwrap()
            .replace("{{HOME}}", &home.to_string_lossy());
        std::fs::write(home.join(".ai-memory.toml"), text).unwrap();
        let module = tmp.path().join("openclaw-route.ts");
        let plugin = build_plugin("http://127.0.0.1:49374", None, None, "denylist");
        let helpers = plugin
            .split_once("function tomlFlag")
            .unwrap()
            .1
            .split_once("function postHook")
            .unwrap()
            .0;
        let source = format!(
            "import {{ execFileSync }} from \"node:child_process\";\nimport {{ closeSync, existsSync, openSync, readFileSync as readMarkerText, readSync, statSync }} from \"node:fs\";\nimport {{ basename, dirname, join, resolve, sep }} from \"node:path\";\nimport {{ homedir }} from \"node:os\";\nfunction readFileSync(path:string,encoding?:\"utf8\"):any{{return readMarkerText(path,encoding);}}\nfunction tomlKey(text:string,key:string):string|undefined{{const m=new RegExp(`^\\\\s*${{key}}\\\\s*=\\\\s*\"([^\"]*)\"`,`m`).exec(text);return m?.[1];}}\nfunction tomlFlag{}\nconst routeCwd={1};const direct=homeRoute(readHomeRouteText(join(homedir(),\".ai-memory.toml\")),routeCwd,\"github.com/acme/api\");const url=new URL(\"http://h/hook\");const ok=applyMarkerParams(url,routeCwd);process.stdout.write((direct?.project??\"none\")+\":\"+String(ok)+url.search);",
            helpers,
            serde_json::to_string(&repo.to_string_lossy()).unwrap()
        );
        std::fs::write(&module, source).unwrap();
        let output = std::process::Command::new("node")
            .args(["--experimental-strip-types", "--no-warnings"])
            .arg(&module)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let query = String::from_utf8(output.stdout).unwrap();
        assert!(query.starts_with("identity-api:true?cwd="), "{query}");
        for expected in [
            "workspace=identity",
            "project=identity-api",
            "identity=github.com%2Facme%2Fapi",
            "identity_src=git_remote",
            "identity_style=path",
            "aliases=%5B%22main%22%5D",
            "drop_subagent=true",
            "default_global=true",
            "briefing=true",
            "briefing_budget=3210",
            "profile_consume=0",
        ] {
            assert!(query.contains(expected), "{expected}: {query}");
        }
    }

    #[test]
    fn openclaw_executes_every_shared_home_route_fixture() {
        if !std::process::Command::new("node")
            .args(["--experimental-strip-types", "-e", "0"])
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../ai-memory-core/fixtures/home_route_cases.json"
        ))
        .unwrap();
        let cases = fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|case| {
                let text = match case["generate"].as_str() {
                    Some("65_routes") => (0..65)
                        .map(|index| format!("[routes.path.\"/route/{index}\"]\nroute_workspace=\"ws\"\nroute_project=\"p{index}\"\n"))
                        .collect(),
                    Some("selector_512") => format!("[routes.path.\"/{}\"]\nroute_workspace=\"bounds\"\nroute_project=\"valid\"\n", "a".repeat(511)),
                    Some("selector_513") => format!("[routes.path.\"/{}\"]\nroute_workspace=\"bounds\"\nroute_project=\"invalid\"\n", "a".repeat(512)),
                    Some("oversized_file") => format!("#{}\n", "x".repeat(65_536)),
                    Some(other) => panic!("unknown fixture generator {other}"),
                    None => case["toml"]
                        .as_str()
                        .unwrap()
                        .replace("{{HOME}}", &home.to_string_lossy())
                        .replace("{{LONG_513}}", &"a".repeat(513)),
                };
                let mut cwd = match case["cwd_generate"].as_str() {
                    Some("selector_512_child") => format!("/{}/child", "a".repeat(511)),
                    Some(other) => panic!("unknown cwd fixture generator {other}"),
                    None => case["cwd"]
                        .as_str()
                        .unwrap()
                        .replace("{{HOME}}", &home.to_string_lossy()),
                };
                if let Some(identity) = case["identity"].as_str() {
                    let repo = home
                        .join("fixture-repos")
                        .join(case["name"].as_str().unwrap());
                    std::fs::create_dir_all(&repo).unwrap();
                    assert!(
                        std::process::Command::new("git")
                            .args(["init", "-q"])
                            .current_dir(&repo)
                            .status()
                            .unwrap()
                            .success()
                    );
                    assert!(
                        std::process::Command::new("git")
                            .args([
                                "remote",
                                "add",
                                "origin",
                                &format!("https://{identity}.git"),
                            ])
                            .current_dir(&repo)
                            .status()
                            .unwrap()
                            .success()
                    );
                    cwd = repo.to_string_lossy().into_owned();
                } else if cwd.starts_with(&*home.to_string_lossy()) {
                    std::fs::create_dir_all(&cwd).unwrap();
                }
                serde_json::json!({
                    "name": case["name"],
                    "text": text,
                    "cwd": cwd,
                    "identity": case["identity"],
                    "status": case["status"],
                    "source": case["source"],
                    "workspace": case["workspace"],
                    "project": case["project"],
                    "style": case["style"],
                    "aliases": case["aliases"],
                    "flags": case["flags"]
                })
            })
            .collect::<Vec<_>>();
        let plugin = build_plugin("http://127.0.0.1:49374", None, None, "denylist");
        let helpers = plugin
            .split_once("function tomlFlag")
            .unwrap()
            .1
            .split_once("function postHook")
            .unwrap()
            .0;
        let module = tmp.path().join("openclaw-all-routes.ts");
        let source = format!(
            "import {{ execFileSync as realExecFileSync }} from \"node:child_process\";\nimport {{ closeSync, existsSync, openSync, readFileSync as readMarkerText, readSync, statSync, writeFileSync }} from \"node:fs\";\nimport {{ basename, dirname, join, resolve, sep }} from \"node:path\";\nimport {{ homedir }} from \"node:os\";\nfunction execFileSync(command:string,args:string[],options?:any):string{{return realExecFileSync(command,args,options) as string;}}\nfunction readFileSync(path:string,encoding?:\"utf8\"):any{{return readMarkerText(path,encoding);}}\nfunction tomlKey(text:string,key:string):string|undefined{{const m=new RegExp(`^\\\\s*${{key}}\\\\s*=\\\\s*\"([^\"]*)\"`,`m`).exec(text);return m?.[1];}}\nfunction tomlFlag{}\nconst cases={} as any[];for(const c of cases){{const file=join(homedir(),\".ai-memory.toml\");writeFileSync(file,c.text);const url=new URL(\"http://h/hook\");let applied=false;let helper=\"none\";try{{const route=homeRoute(readHomeRouteText(file),c.cwd,c.identity);helper=route?route.workspace+\"/\"+route.project:\"none\";}}catch{{helper=\"invalid\";}}try{{applied=applyMarkerParams(url,c.cwd);}}catch{{applied=false;}}process.stdout.write(JSON.stringify({{name:c.name,applied,helper,params:Object.fromEntries(url.searchParams)}})+\"\\n\");}}",
            helpers,
            serde_json::to_string(&cases).unwrap()
        );
        std::fs::write(&module, source).unwrap();
        // A Windows cwd such as `c:/work/api/lib` is relative on Unix, so the
        // marker walk starts in node's working directory; run it from the temp
        // dir so a contributor's `.ai-memory.toml` in the checkout is not found.
        let output = std::process::Command::new("node")
            .args(["--experimental-strip-types", "--no-warnings"])
            .arg(&module)
            .current_dir(tmp.path())
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let results = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| {
                let result: serde_json::Value = serde_json::from_str(line).unwrap();
                (result["name"].as_str().unwrap().to_owned(), result)
            })
            .collect::<std::collections::HashMap<_, _>>();
        for case in &cases {
            let name = case["name"].as_str().unwrap();
            let result = &results[name];
            match case["status"].as_str().unwrap() {
                "invalid" => {
                    assert_eq!(result["helper"], "invalid", "{name}: {result}");
                    assert_eq!(result["applied"], false, "{name}: {result}");
                }
                "none" => {
                    assert_eq!(result["helper"], "none", "{name}: {result}");
                    assert_eq!(result["applied"], true, "{name}: {result}");
                    assert!(
                        result["params"].get("workspace").is_none(),
                        "{name}: {result}"
                    );
                    assert!(
                        result["params"].get("project").is_none(),
                        "{name}: {result}"
                    );
                }
                "valid" => {
                    assert_eq!(result["applied"], true, "{name}: {result}");
                    assert_eq!(
                        result["params"]["workspace"], case["workspace"],
                        "{name}: {result}"
                    );
                    assert_eq!(
                        result["params"]["project"], case["project"],
                        "{name}: {result}"
                    );
                    assert_eq!(
                        result["params"]["project_src"], "marker",
                        "{name}: {result}"
                    );
                    let has_remote = result["params"]["identity_src"] == "git_remote";
                    if case["source"] == "identity" && has_remote {
                        assert_eq!(
                            result["params"]["identity"], case["identity"],
                            "{name}: {result}"
                        );
                    } else if case["source"] == "path" {
                        assert!(
                            result["params"].get("aliases").is_none(),
                            "{name}: {result}"
                        );
                    }
                    if let Some(style) = case["style"].as_str()
                        && has_remote
                    {
                        assert_eq!(
                            result["params"]["identity_style"], style,
                            "{name}: {result}"
                        );
                    }
                    if let Some(aliases) = case["aliases"].as_str()
                        && has_remote
                    {
                        assert_eq!(result["params"]["aliases"], aliases, "{name}: {result}");
                    }
                    if let Some(flags) = case["flags"].as_str() {
                        for pair in flags.split('&') {
                            let (key, value) = pair.split_once('=').unwrap();
                            assert_eq!(result["params"][key], value, "{name}: {result}");
                        }
                    }
                }
                status => panic!("unknown fixture status {status}"),
            }
        }
    }

    #[test]
    fn openclaw_runtime_preserves_marker_and_home_identity_precedence() {
        if !std::process::Command::new("node")
            .args(["--experimental-strip-types", "-e", "0"])
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args(["remote", "add", "origin", "git@github.com:acme/api.git"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        let marker = repo.join(".ai-memory.toml");
        let home_marker = home.join(".ai-memory.toml");
        let module = tmp.path().join("openclaw-identity-gate.ts");
        let plugin = build_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist");
        let helpers = plugin
            .split_once("function tomlFlag")
            .unwrap()
            .1
            .split_once("function postHook")
            .unwrap()
            .0;
        let source = format!(
            "import {{ execFileSync }} from \"node:child_process\";\nimport {{ closeSync, existsSync, openSync, readFileSync as readMarkerText, readSync, statSync, unlinkSync, writeFileSync }} from \"node:fs\";\nimport {{ basename, dirname, join, resolve, sep }} from \"node:path\";\nimport {{ homedir }} from \"node:os\";\nfunction readFileSync(path:string,encoding?:\"utf8\"):any{{return readMarkerText(path,encoding);}}\nfunction tomlKey(text:string,key:string):string|undefined{{const m=new RegExp(`^\\\\s*${{key}}\\\\s*=\\\\s*\"([^\"]*)\"`,`m`).exec(text);return m?.[1];}}\nfunction tomlFlag{}\nconst repoCwd={};const marker={};const homeMarker={};const run=(body:string)=>{{writeFileSync(marker,body);const url=new URL(\"http://h/hook\");applyMarkerParams(url,repoCwd);return Object.fromEntries(url.searchParams);}};const noMarkerUrl=new URL(\"http://h/hook\");applyMarkerParams(noMarkerUrl,repoCwd);const noMarker=Object.fromEntries(noMarkerUrl.searchParams);const plain=run('project=\"api\"\\n');const aliases=run('project=\"api\"\\naliases=[\"old-api\"]\\n');unlinkSync(marker);writeFileSync(homeMarker,'[routes.identity.\"github.com/acme/api\"]\\nroute_workspace=\"oss\"\\nroute_project=\"api\"\\n');const routedUrl=new URL(\"http://h/hook\");applyMarkerParams(routedUrl,repoCwd);process.stdout.write(JSON.stringify({{noMarker,plain,aliases,routed:Object.fromEntries(routedUrl.searchParams)}}));",
            helpers,
            serde_json::to_string(&repo.to_string_lossy()).unwrap(),
            serde_json::to_string(&marker.to_string_lossy()).unwrap(),
            serde_json::to_string(&home_marker.to_string_lossy()).unwrap(),
        );
        std::fs::write(&module, source).unwrap();
        let output = std::process::Command::new("node")
            .args(["--experimental-strip-types", "--no-warnings"])
            .arg(&module)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["noMarker"]["identity_src"], "git_remote", "{result}");
        assert_eq!(result["noMarker"]["identity_style"], "path", "{result}");
        assert!(result["plain"].get("identity").is_none(), "{result}");
        assert_eq!(result["aliases"]["identity_src"], "git_remote", "{result}");
        assert_eq!(result["aliases"]["aliases"], "[\"old-api\"]", "{result}");
        assert_eq!(result["routed"]["identity_src"], "git_remote", "{result}");
        assert_eq!(result["routed"]["workspace"], "oss", "{result}");
        assert_eq!(result["routed"]["project_src"], "marker", "{result}");
    }

    #[test]
    fn openclaw_plugin_default_omits_baked_strategy() {
        let plugin = build_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist");
        assert!(
            !plugin.contains("DEFAULT_PROJECT_STRATEGY"),
            "basename default must bake no strategy: {plugin}"
        );
    }

    #[test]
    fn openclaw_plugin_bakes_allowlist_admit_gate() {
        // #661: the generated OpenClaw plugin POSTs to `<server>/hook` directly
        // and never runs through the native `ai-memory hook` admit gate
        // (`repository_admits_capture` in `ai-memory-hooks::capture_policy`), so
        // the shared TS template must carry an equivalent gate keyed on marker
        // *presence*, baked from `--capture-mode`.
        let plugin = build_plugin("http://127.0.0.1:49374", Some("tok"), None, "allowlist");
        assert!(
            plugin.contains("const CAPTURE_MODE: \"allowlist\" | \"denylist\" = \"allowlist\";"),
            "allowlist mode must be baked into the emitted constant: {plugin}"
        );
        assert!(
            plugin.contains(
                "const markerPresent = !!findMarker(cwd); if (CAPTURE_MODE === \"allowlist\" && !markerPresent) return { disposition: \"drop\", payload };"
            ),
            "allowlist build must carry the marker-presence admit gate: {plugin}"
        );
    }

    /// #992: OpenClaw posts to `SERVER` directly and does not route `server`
    /// profiles, so a routed repository must emit nothing and fetch no handoff.
    #[test]
    fn openclaw_plugin_fails_closed_on_a_server_profile_marker() {
        let plugin = build_plugin("http://127.0.0.1:49374", None, None, "denylist");
        assert!(
            plugin.contains(
                "if (captureServerRouted(cwd)) return { disposition: \"drop\", payload };"
            ),
            "{plugin}"
        );
        let handoff = plugin.split_once("async function fetchHandoff(").unwrap().1;
        let guard = handoff
            .find("if (captureServerRouted(currentCwd)) return undefined;")
            .expect("handoff fetch must be gated");
        assert!(guard < handoff.find("/handoff`").unwrap());
    }

    #[test]
    fn openclaw_plugin_denylist_bakes_inert_gate() {
        let plugin = build_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist");
        assert!(
            plugin.contains("const CAPTURE_MODE: \"allowlist\" | \"denylist\" = \"denylist\";"),
            "denylist mode must be baked into the emitted constant: {plugin}"
        );
        // The gate expression is present in every build (it's part of the one
        // shared template) but is inert under denylist, since `CAPTURE_MODE`
        // never equals `"allowlist"`.
        assert!(
            plugin.contains(
                "const markerPresent = !!findMarker(cwd); if (CAPTURE_MODE === \"allowlist\" && !markerPresent) return { disposition: \"drop\", payload };"
            )
        );
    }

    #[test]
    fn package_writes_all_required_files() {
        let tmp = TempDir::new().unwrap();
        let outcomes =
            write_package(tmp.path(), "http://127.0.0.1:49374", None, None, "denylist").unwrap();

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
