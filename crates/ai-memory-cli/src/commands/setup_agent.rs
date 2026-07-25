//! `ai-memory setup-agent` — one-shot agent integration for the
//! docker-primary workflow.
//!
//! Solves the problem that `install-hooks` alone can't handle in a
//! docker-only deploy: the JSON snippet `install-hooks` emits
//! references absolute paths to hook scripts, and those paths must
//! exist on the host machine that runs the agent CLI (Claude Code et
//! al. shell out from the host, not inside the container).
//!
//! `setup-agent` bundles the extract + render into one command:
//!
//!     docker run --rm \
//!       -v "$HOME/.ai-memory:/host" \
//!       akitaonrails/ai-memory:latest \
//!       setup-agent \
//!         --agent claude-code \
//!         --to /host/hooks \
//!         --host-prefix "$HOME/.ai-memory/hooks" \
//!         --auth-token "$TOKEN"
//!
//! 1. Copies `/usr/local/share/ai-memory/hooks/claude-code/*.{sh,ps1}` into
//!    `/host/hooks/claude-code/` (which on the host is
//!    `$HOME/.ai-memory/hooks/claude-code/`).
//! 2. Prints the JSON config snippet whose `command` fields point at
//!    `$HOME/.ai-memory/hooks/claude-code/*.{sh,ps1}` (via `--host-prefix`)
//!    so Claude Code on the host can exec them.
//!
//! When `--host-prefix` is omitted it defaults to `--to`, which is
//! the right behaviour for a non-docker (`cargo run`) invocation
//! where the in-container path and the host path are the same thing.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::cli::{AgentChoice, SetupAgentArgs};
use crate::commands::install_mcp;
use crate::commands::render_shared::{
    ANTIGRAVITY_LIFECYCLE_EVENTS, ANTIGRAVITY_TOOL_EVENTS, CODEX_PROFILE, CURSOR_PROFILE,
    GEMINI_PROFILE, KIMI_CODE_EVENTS, build_claude_code_payload, build_devin_payload,
    build_grok_payload, hook_script_for_current_platform,
};
use crate::config::{Config, DEFAULT_SERVER_URL};

/// Run the `setup-agent` subcommand.
///
/// # Errors
/// Returns an error if the source bundle can't be located, the
/// destination directory can't be created, any script copy fails,
/// or the JSON config can't be serialised.
pub fn run(config: &Config, args: SetupAgentArgs) -> Result<()> {
    let server_url = if args.server_url == DEFAULT_SERVER_URL && config.server_url_configured() {
        normalise_hook_server_url(&config.server_url)
    } else {
        normalise_hook_server_url(&args.server_url)
    };
    let args = SetupAgentArgs {
        server_url,
        auth_token: args.auth_token.or_else(|| config.auth.bearer_token.clone()),
        ..args
    };
    if matches!(
        args.agent,
        AgentChoice::OpenCode | AgentChoice::Pi | AgentChoice::Omp | AgentChoice::Openclaw
    ) {
        emit_extension_setup_hint(&args)?;
        return Ok(());
    }
    // Zero runs ai-memory's native `hook` command directly (exec form,
    // no scripts to stage) — setup-agent just prints the hooks.json.
    if matches!(args.agent, AgentChoice::Zero) {
        emit_zero(&args)?;
        return Ok(());
    }
    let Some(agent_sub) = args.agent.script_hook_subdir() else {
        bail!("internal: generated integration should have returned before staging hooks")
    };
    eprintln!(
        "[ai-memory] setup-agent emits shell/PowerShell hook bundles for {agent_sub}; these remote/compatibility bundles do not enforce capture-policy capability v1. Install local native hooks instead when policy enforcement is required."
    );

    let source = resolve_source(args.source.as_deref(), agent_sub)?;
    let dest_dir = args.to.join(agent_sub);

    fs::create_dir_all(&dest_dir)
        .with_context(|| format!("creating destination {}", dest_dir.display()))?;

    let mut copied = 0_usize;
    for entry in fs::read_dir(&source)
        .with_context(|| format!("reading source bundle {}", source.display()))?
    {
        let entry = entry?;
        let from = entry.path();
        if !from.is_file() || !is_hook_script_file(&from) {
            continue;
        }
        let file_name = from
            .file_name()
            .with_context(|| format!("invalid hook script path {}", from.display()))?;
        let to = dest_dir.join(file_name);
        fs::copy(&from, &to)
            .with_context(|| format!("copying {} → {}", from.display(), to.display()))?;
        // Preserve executable bit so the agent CLI can actually run
        // the scripts. On Windows this is a no-op.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&to)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&to, perms)?;
        }
        copied += 1;
    }

    copy_support_hook_scripts(&source, &dest_dir)?;

    eprintln!(
        "✓ Extracted {copied} hook script(s) from {} to {}",
        source.display(),
        dest_dir.display(),
    );

    // The path the rendered JSON should reference. Defaults to where
    // we just copied the scripts; override with --host-prefix when
    // running inside docker against a mounted volume.
    let emit_root = args
        .host_prefix
        .as_deref()
        .unwrap_or(&args.to)
        .join(agent_sub);

    match args.agent {
        AgentChoice::ClaudeCode => emit_claude_code(&emit_root, &args)?,
        AgentChoice::Grok => emit_grok(&emit_root, &args)?,
        AgentChoice::Devin => emit_devin(&emit_root, &args)?,
        AgentChoice::Codex => emit_other(&emit_root, agent_sub, &args, &[CODEX_PROFILE.events]),
        AgentChoice::Cursor => emit_other(&emit_root, agent_sub, &args, &[CURSOR_PROFILE.events]),
        AgentChoice::GeminiCli => {
            emit_other(&emit_root, agent_sub, &args, &[GEMINI_PROFILE.events]);
        }
        AgentChoice::AntigravityCli => emit_other(
            &emit_root,
            agent_sub,
            &args,
            &[&ANTIGRAVITY_TOOL_EVENTS, &ANTIGRAVITY_LIFECYCLE_EVENTS],
        ),
        // Kimi's success and failure post-tool events share one script. The
        // script listing is deduplicated below; apply-mode owns the exact
        // `[[hooks]]` TOML merge into the user's config.toml.
        AgentChoice::KimiCode => emit_other(&emit_root, agent_sub, &args, &[&KIMI_CODE_EVENTS]),
        AgentChoice::OpenCode
        | AgentChoice::Pi
        | AgentChoice::Omp
        | AgentChoice::Openclaw
        | AgentChoice::Zero => {
            bail!(
                "internal: generated integration should have returned before emitting staged hooks"
            )
        }
    }
    Ok(())
}

/// Print Zero's hooks.json (issue #156). No scripts are staged: Zero
/// executes the ai-memory binary directly with the JSON payload on stdin,
/// so the only artifact is the config itself. The binary must be reachable
/// on the host that runs Zero — for docker-wrapper setups install the
/// native binary or point `command` at the wrapper.
fn emit_zero(args: &SetupAgentArgs) -> Result<()> {
    let payload = crate::commands::render_shared::build_zero_hooks_config(
        &args.server_url,
        args.auth_token.as_deref(),
        None,
        None,
    );
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing Zero hook config")?;
    println!("# Zero (Gitlawb/zero) — merge into ~/.config/zero/hooks.json");
    println!("# The `command` must be an ai-memory binary reachable on the host");
    println!("# that runs Zero; prefer `ai-memory install-hooks --agent zero --apply`");
    println!("# from that host so the path is resolved for you.");
    if args.auth_token.is_some() {
        println!("#       Treat hooks.json as sensitive (chmod 600).");
    }
    println!("# NOTE: Zero discards sessionStart stdout, so this config captures");
    println!("#       but does not inject handoffs; recover them via the MCP");
    println!("#       `memory_handoff_accept` tool.");
    println!();
    println!("{serialized}");
    Ok(())
}

fn normalise_hook_server_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

fn emit_extension_setup_hint(args: &SetupAgentArgs) -> Result<()> {
    let (label, agent, restart_note, mcp_client) = match args.agent {
        AgentChoice::OpenCode => (
            "OpenCode",
            "opencode",
            "Then restart OpenCode so it loads ~/.config/opencode/plugins/ai-memory.ts.",
            "opencode",
        ),
        AgentChoice::Omp => (
            "OMP",
            "omp",
            "Then restart OMP so it loads ~/.omp/agent/extensions/ai-memory.ts.",
            "omp",
        ),
        AgentChoice::Pi => (
            "Pi",
            "pi",
            "Then restart Pi so it loads ~/.pi/agent/extensions/ai-memory.ts.",
            "pi",
        ),
        AgentChoice::Openclaw => (
            "OpenClaw",
            "openclaw",
            "Then restart the OpenClaw gateway if it does not auto-restart after plugin install.",
            "openclaw",
        ),
        other => bail!("internal: {other:?} is not a generated-integration agent"),
    };
    println!("# {label} uses a TypeScript extension/plugin, not extracted shell scripts.");
    println!(
        "# Capture-policy capability v1 requires a freshly generated integration; re-run --apply after upgrades."
    );
    println!(
        "# Legacy shell/PowerShell and remote-only paths are unsupported compatibility modes."
    );
    println!("# Install it directly instead:");
    println!("ai-memory install-hooks --agent {agent} --apply \\");
    if args.auth_token.is_some() {
        println!("  --server-url {} \\", args.server_url);
        println!("  --auth-token <token>");
    } else {
        println!("  --server-url {}", args.server_url);
        println!("  # add --auth-token <token> if the server requires bearer auth");
    }
    println!();
    println!("{restart_note}");
    if matches!(args.agent, AgentChoice::Pi) {
        println!(
            "MCP tools come through the same generated Pi bridge extension; no native mcp.json is written."
        );
    } else {
        println!("Also run `ai-memory install-mcp --client {mcp_client}` to wire MCP separately.");
    }
    Ok(())
}

fn emit_claude_code(emit_root: &Path, args: &SetupAgentArgs) -> Result<()> {
    let payload =
        build_claude_code_payload(emit_root, &args.server_url, args.auth_token.as_deref());
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing Claude Code hook config")?;
    let settings_path = crate::commands::install_hooks::claude_settings_path()?;
    println!("# Claude Code — merge into {}", settings_path.display());
    println!("# Hook scripts (must be reachable from the host that runs Claude Code):");
    println!("#   {}", emit_root.display());
    println!("# AI-memory server: {}", args.server_url);
    if args.auth_token.is_some() {
        println!("# Auth: AI_MEMORY_AUTH_TOKEN embedded in each hook's env block.");
        println!(
            "#       Treat {} as sensitive (chmod 600).",
            settings_path.display()
        );
    }
    println!("# Tip: also run `ai-memory install-mcp --client claude-code --auth-token <…>`");
    println!("#      to register the MCP endpoint (separate from hooks).");
    println!();
    println!("{serialized}");
    Ok(())
}

fn emit_grok(emit_root: &Path, args: &SetupAgentArgs) -> Result<()> {
    let payload = build_grok_payload(emit_root, &args.server_url, args.auth_token.as_deref());
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing Grok hook config")?;
    let grok_home = install_mcp::grok_home()?;
    let hooks_path = grok_home.join("hooks/ai-memory.json");
    println!("# Grok Build CLI — write to {}", hooks_path.display());
    println!("# Hook scripts (must be reachable from the host that runs Grok):");
    println!("#   {}", emit_root.display());
    println!("# AI-memory server: {}", args.server_url);
    if args.auth_token.is_some() {
        println!("# Auth: AI_MEMORY_AUTH_TOKEN embedded in each hook command below.");
        println!(
            "#       Treat {} as sensitive (chmod 600).",
            hooks_path.display()
        );
    }
    println!("# NOTE: Grok ignores SessionStart stdout, so this config captures");
    println!("#       lifecycle events but does not inject handoffs automatically.");
    println!("#       Recover handoffs via the MCP memory_handoff_accept tool.");
    println!("# Tip: also run `ai-memory install-mcp --client grok --auth-token <…>`");
    println!(
        "#      to register the MCP endpoint in {}.",
        grok_home.join("config.toml").display()
    );
    println!();
    println!("{serialized}");
    Ok(())
}

fn emit_devin(emit_root: &Path, args: &SetupAgentArgs) -> Result<()> {
    print!("{}", render_devin_setup_output(emit_root, args)?);
    Ok(())
}

fn render_devin_setup_output(emit_root: &Path, args: &SetupAgentArgs) -> Result<String> {
    let payload = build_devin_payload(emit_root, &args.server_url, args.auth_token.as_deref());
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing Devin hook config")?;
    let mut out = String::new();
    out.push_str(
        "# Devin CLI — write to ~/.devin/hooks.v1.json or ~/.devin/config.json hooks key\n",
    );
    out.push_str("# Hook scripts (must be reachable from the host that runs Devin):\n");
    out.push_str(&format!("#   {}\n", emit_root.display()));
    out.push_str(&format!("# AI-memory server: {}\n", args.server_url));
    if args.auth_token.is_some() {
        out.push_str("# Auth: AI_MEMORY_AUTH_TOKEN embedded in each hook command below.\n");
        out.push_str(
            "#       Treat ~/.devin/hooks.v1.json or ~/.devin/config.json as sensitive (chmod 600).\n",
        );
    }
    out.push_str(
        "# NOTE: Devin consumes the handoff via hookSpecificOutput.additionalContext on SessionStart.\n",
    );
    out.push('\n');
    out.push_str(&serialized);
    out.push('\n');
    Ok(out)
}

fn emit_other(
    emit_root: &Path,
    label: &str,
    args: &SetupAgentArgs,
    event_lists: &[&[(&str, &str)]],
) {
    // These clients have hook surfaces, but their print-mode config
    // snippets are intentionally conservative: apply-mode owns the
    // exact merge/plugin generation where ai-memory knows the format.
    println!("# {label} hook scripts (manual wire-up; use install-hooks --apply when available)");
    println!("# Scripts located at: {}", emit_root.display());
    println!("# Server URL:         {}", args.server_url);
    if args.auth_token.is_some() {
        println!("# Auth: set AI_MEMORY_AUTH_TOKEN in each hook's environment to the");
        println!("#       value you passed via --auth-token (not echoed).");
    }
    println!();
    for path in event_script_paths(emit_root, event_lists) {
        println!("- {}", path.display());
    }
    println!();
    println!("Set AI_MEMORY_HOOK_URL in each hook's environment to override the default.");
    println!("Also run `ai-memory install-mcp --client {label}` to wire MCP separately.");
}

fn event_script_paths(emit_root: &Path, event_lists: &[&[(&str, &str)]]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for events in event_lists {
        for (_, script) in *events {
            let script = hook_script_for_current_platform(script);
            let path = emit_root.join(script.as_ref());
            // Two events may share one script (Kimi Code's PostToolUseFailure
            // reuses post-tool-use); list each file once.
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

fn is_hook_script_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("sh" | "ps1")
    )
}

fn copy_support_hook_scripts(source_dir: &Path, dest_dir: &Path) -> Result<()> {
    let Some(source_hooks_root) = source_dir.parent() else {
        return Ok(());
    };
    let Some(dest_hooks_root) = dest_dir.parent() else {
        return Ok(());
    };
    let shared_lib = source_hooks_root.join("_lib.sh");
    if shared_lib.is_file() {
        let to = dest_hooks_root.join("_lib.sh");
        fs::copy(&shared_lib, &to)
            .with_context(|| format!("copying {} → {}", shared_lib.display(), to.display()))?;
    }

    let source_lib = source_hooks_root.join("lib");
    if !source_lib.is_dir() {
        return Ok(());
    }

    let dest_lib = dest_hooks_root.join("lib");
    fs::create_dir_all(&dest_lib)
        .with_context(|| format!("creating hook support dir {}", dest_lib.display()))?;
    for entry in fs::read_dir(&source_lib)
        .with_context(|| format!("reading hook support dir {}", source_lib.display()))?
    {
        let entry = entry?;
        let from = entry.path();
        if !from.is_file() || from.extension().and_then(|s| s.to_str()) != Some("ps1") {
            continue;
        }
        let to = dest_lib.join(
            from.file_name()
                .with_context(|| format!("invalid hook support path {}", from.display()))?,
        );
        fs::copy(&from, &to)
            .with_context(|| format!("copying {} → {}", from.display(), to.display()))?;
    }
    Ok(())
}

fn resolve_source(explicit: Option<&Path>, sub: &str) -> Result<PathBuf> {
    let candidates = source_candidates(explicit, sub, std::env::current_exe().ok());
    for path in &candidates {
        if path.is_dir() {
            return Ok(path.clone());
        }
    }
    bail!(
        "could not locate hook source bundle for {sub}. \
         Tried: {candidates:?}. Pass --source <dir> to override."
    );
}

/// Ordered directories to probe for the `<sub>` hook bundle.
///
/// `exe` is the running binary's path (`std::env::current_exe()`), threaded
/// in so the derivation is unit-testable. An `explicit` `--source` is trusted
/// verbatim; otherwise we try the packaged install locations plus two
/// binary-relative spots:
///   * `<exe_dir>/hooks/<sub>` — the **release tarball** ships `hooks/` right
///     beside the binary (macOS/Windows/Linux archives), and
///   * `<exe_dir>/../../hooks/<sub>` — `cargo run` from the repo, where the
///     binary lives under `target/<profile>/`.
///
/// Without the binary-sibling entry the flat tarball layout was unreachable:
/// from `/private/tmp/<dir>/ai-memory` the `parent×3` dev fallback derived a
/// bogus `/private/hooks/<sub>` and discovery failed (issue #107).
fn source_candidates(explicit: Option<&Path>, sub: &str, exe: Option<PathBuf>) -> Vec<PathBuf> {
    if let Some(p) = explicit {
        return vec![p.join(sub)];
    }
    let mut v = vec![
        // Docker image lays them out under /usr/local/share/.
        PathBuf::from(format!("/usr/local/share/ai-memory/hooks/{sub}")),
        // Native Linux packages install hook sources under /usr/share.
        PathBuf::from(format!("/usr/share/ai-memory/hooks/{sub}")),
    ];
    if let Some(exe) = exe {
        // Release tarball: `hooks/` sits in the same dir as the binary.
        if let Some(dir) = exe.parent() {
            v.push(dir.join("hooks").join(sub));
        }
        // Repo-local fallback for `cargo run setup-agent` during dev:
        // target/<profile>/<bin> → repo root.
        if let Some(root) = exe.parent().and_then(Path::parent).and_then(Path::parent) {
            v.push(root.join("hooks").join(sub));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_candidates_include_binary_sibling_for_flat_tarball() {
        // Flat release tarball: the binary and its `hooks/` bundle are
        // extracted side by side. On macOS the path resolves under
        // /private/..., which the old `parent×3` dev fallback turned into a
        // bogus `/private/hooks/...` (issue #107).
        let exe = PathBuf::from("/private/tmp/ai-memory-macos-aarch64/ai-memory");
        let candidates = source_candidates(None, "claude-code", Some(exe));

        assert!(
            candidates.contains(&PathBuf::from(
                "/private/tmp/ai-memory-macos-aarch64/hooks/claude-code"
            )),
            "binary-sibling hooks/ dir must be probed; got {candidates:?}"
        );
        // Packaged install locations still take precedence.
        assert_eq!(
            candidates[0],
            PathBuf::from("/usr/local/share/ai-memory/hooks/claude-code")
        );
    }

    #[test]
    fn source_candidates_preserve_cargo_run_repo_root() {
        // `cargo run`: target/<profile>/<bin> → repo root holds `hooks/`.
        let exe = PathBuf::from("/home/dev/ai-memory/target/debug/ai-memory");
        let candidates = source_candidates(None, "claude-code", Some(exe));
        assert!(
            candidates.contains(&PathBuf::from("/home/dev/ai-memory/hooks/claude-code")),
            "repo-root hooks/ dir must still be probed; got {candidates:?}"
        );
    }

    #[test]
    fn source_candidates_honour_explicit_override() {
        let candidates = source_candidates(Some(Path::new("/custom/src")), "codex", None);
        assert_eq!(candidates, vec![PathBuf::from("/custom/src/codex")]);
    }

    #[test]
    fn pi_setup_prints_extension_hint_without_copying() {
        let tmp = tempfile::TempDir::new().unwrap();
        let args = SetupAgentArgs {
            agent: AgentChoice::Pi,
            to: tmp.path().join("hooks"),
            host_prefix: None,
            server_url: "http://127.0.0.1:49374".into(),
            auth_token: None,
            source: Some(tmp.path().join("source")),
        };

        run(&Config::default(), args).unwrap();

        assert!(!tmp.path().join("hooks").exists());
    }

    #[test]
    fn devin_setup_emits_script_hook_config() {
        let args = SetupAgentArgs {
            agent: AgentChoice::Devin,
            to: PathBuf::from("/container/hooks"),
            host_prefix: Some(PathBuf::from("/host/hooks")),
            server_url: "http://127.0.0.1:49374".into(),
            auth_token: Some("token-for-test".into()),
            source: None,
        };
        let emit_root = Path::new("/host/hooks/devin");

        let rendered = render_devin_setup_output(emit_root, &args).unwrap();

        assert!(rendered.contains("# Devin CLI"));
        assert!(rendered.contains("~/.devin/hooks.v1.json"));
        assert!(rendered.contains("hookSpecificOutput.additionalContext"));
        assert!(rendered.contains("AI_MEMORY_AUTH_TOKEN"));

        let json_start = rendered.find('{').expect("rendered Devin config JSON");
        let parsed: serde_json::Value = serde_json::from_str(&rendered[json_start..]).unwrap();
        let hooks = parsed["hooks"].as_object().expect("hooks object");
        assert!(hooks["SessionStart"].is_array());
        assert!(hooks["PostCompaction"].is_array());
        assert!(hooks.get("PreCompact").is_none());
        assert!(hooks.get("SubagentStart").is_none());
        assert!(hooks.get("SubagentStop").is_none());

        let session_start = hooks["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(session_start.contains("/host/hooks/devin/session-start.sh"));
        // Script-mode handlers inline env vars into the command string
        // itself (see `claude_code_payload_embeds_auth_token_when_provided`
        // in render_shared.rs) rather than using a separate `env` field.
        assert!(
            session_start.contains("AI_MEMORY_AUTH_TOKEN=token-for-test"),
            "command should inline the auth token; got: {session_start}"
        );

        let post_compaction = hooks["PostCompaction"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(post_compaction.contains("/host/hooks/devin/post-compaction.sh"));
    }

    #[test]
    fn devin_setup_copies_event_scripts_and_shared_lib() {
        let tmp = tempfile::TempDir::new().unwrap();
        let source_root = tmp.path().join("source");
        let source_devin = source_root.join("devin");
        fs::create_dir_all(&source_devin).unwrap();
        fs::write(
            source_devin.join("session-start.sh"),
            "#!/bin/sh\n. \"$(dirname \"$0\")/../_lib.sh\"\n",
        )
        .unwrap();
        fs::write(
            source_root.join("_lib.sh"),
            "ai_memory_json_string() { cat; }\n",
        )
        .unwrap();

        let dest = tmp.path().join("dest");
        let args = SetupAgentArgs {
            agent: AgentChoice::Devin,
            to: dest.clone(),
            host_prefix: None,
            server_url: "http://127.0.0.1:49374".into(),
            auth_token: None,
            source: Some(source_root),
        };

        run(&Config::default(), args).unwrap();

        assert!(dest.join("devin/session-start.sh").exists());
        assert!(dest.join("_lib.sh").exists());
    }

    #[test]
    fn manual_script_paths_use_agent_specific_events() {
        let root = Path::new("/hooks/gemini-cli");
        let paths = event_script_paths(root, &[GEMINI_PROFILE.events]);
        let rendered = paths
            .iter()
            .map(|path| path.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("session-start"));
        assert!(rendered.contains("session-end"));
        assert!(rendered.contains("pre-tool-use"));
        assert!(rendered.contains("post-tool-use"));
        assert!(rendered.contains("pre-compact"));
        assert!(
            !rendered.contains("user-prompt-submit"),
            "Gemini has no UserPromptSubmit hook; setup-agent must not print Claude-only scripts"
        );
        assert!(
            !rendered.contains("subagent-start") && !rendered.contains("subagent-stop"),
            "Gemini has no subagent hook events; setup-agent must not print nonexistent scripts"
        );
    }

    #[test]
    fn kimi_code_manual_script_paths_cover_kimi_event_set() {
        // Kimi Code's hook vocabulary is Claude Code's nine events plus
        // PostToolUseFailure; the failure entry reuses post-tool-use.sh,
        // so the script list carries nine unique files for ten events.
        let root = Path::new("/hooks/kimi-code");
        let paths = event_script_paths(root, &[&KIMI_CODE_EVENTS]);
        let rendered = paths
            .iter()
            .map(|path| path.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n");

        for script in [
            "session-start",
            "session-end",
            "user-prompt-submit",
            "pre-tool-use",
            "post-tool-use",
            "pre-compact",
            "stop",
            "subagent-start",
            "subagent-stop",
        ] {
            assert!(
                rendered.contains(script),
                "Kimi Code setup-agent must list {script}; got:\n{rendered}"
            );
        }
        assert!(
            KIMI_CODE_EVENTS
                .iter()
                .any(|(event, _)| *event == "PostToolUseFailure")
        );
        assert_eq!(KIMI_CODE_EVENTS.len(), 10);
        assert_eq!(paths.len(), 9);
    }
}
