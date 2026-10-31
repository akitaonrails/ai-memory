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
//! ```text
//! docker run --rm \
//!   -v "$HOME/.ai-memory:/host" \
//!   akitaonrails/ai-memory:latest \
//!   setup-agent \
//!     --agent claude-code \
//!     --to /host/hooks \
//!     --host-prefix "$HOME/.ai-memory/hooks" \
//!     --auth-token "$TOKEN"
//! ```
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

use crate::cli::{
    AgentChoice, HookInstallScope, InstallHooksArgs, InstallMcpArgs, McpClient, SetupAgentArgs,
};
use crate::commands::render_shared::{
    ANTIGRAVITY_LIFECYCLE_EVENTS, ANTIGRAVITY_TOOL_EVENTS, CODEX_PROFILE, COMMAND_CODE_PROFILE,
    COPILOT_CLI_PROFILE, CURSOR_PROFILE, GEMINI_PROFILE, KIMI_CODE_EVENTS, KIRO_CLI_V2_EVENTS,
    KIRO_CLI_V3_EVENTS, build_claude_code_payload, build_devin_payload, build_dsh_hooks_payload,
    build_grok_payload, build_pool_settings_yaml, hook_script_for_current_platform,
};
use crate::commands::{install_hooks, install_mcp};
use crate::config::{Config, DEFAULT_SERVER_URL};

/// Run the `setup-agent` subcommand.
///
/// # Errors
/// Returns an error if the source bundle can't be located, the
/// destination directory can't be created, any script copy fails,
/// or the JSON config can't be serialised.
pub fn run(config: &Config, mut args: SetupAgentArgs) -> Result<()> {
    let i18n = &config.i18n;
    args.agent = match (args.agent, args.opencode_dialect) {
        (AgentChoice::OpenCode, Some(choice)) => match choice.dialect() {
            ai_memory_workstream::OpenCodeDialect::V1 => AgentChoice::OpenCode,
            ai_memory_workstream::OpenCodeDialect::V2 => AgentChoice::OpenCode2,
        },
        (AgentChoice::OpenCode, None) => bail!(
            "{}", i18n.text("cli-setup-agent-setup-agent-agent-opencode-requires-opencode-dialect-v1-v2-because-a-contain")),
        (AgentChoice::OpenCode2, None) => AgentChoice::OpenCode2,
        (AgentChoice::OpenCode2, Some(_)) => bail!(
            "{}", i18n.text("cli-setup-agent-opencode-dialect-cannot-be-combined-with-the-force-v2-opencode2-compatibilit")),
        (agent, None) => agent,
        (agent, Some(_)) => bail!(
            "{}", i18n.format("cli-setup-agent-opencode-dialect-is-valid-only-with-agent-opencode-not", &[("value0", agent.kind().as_str().to_string())])),
    };
    let server_url = if args.server_url == DEFAULT_SERVER_URL && config.server_url_configured() {
        normalise_hook_server_url(&config.server_url)
    } else {
        normalise_hook_server_url(&args.server_url)
    };
    let args = SetupAgentArgs {
        server_url,
        // Only an inline token may be printed into snippets; one read from
        // `[auth].bearer_token_file` stays out of rendered configs (#552).
        auth_token: args
            .auth_token
            .or_else(|| config.auth.renderable_bearer_token().map(str::to_owned)),
        ..args
    };
    if matches!(
        args.agent,
        AgentChoice::OpenCode
            | AgentChoice::OpenCode2
            | AgentChoice::Pi
            | AgentChoice::Omp
            | AgentChoice::Openclaw
    ) {
        emit_extension_setup_hint(config, &args)?;
        return Ok(());
    }
    // Zero and ZCode run ai-memory's native `hook` command directly (exec
    // form, no scripts to stage) — setup-agent just prints their config.
    if matches!(args.agent, AgentChoice::Zero) {
        emit_zero(&args)?;
        return Ok(());
    }
    if matches!(args.agent, AgentChoice::Zcode) {
        emit_zcode(&args)?;
        return Ok(());
    }
    // Hermes also runs the native `hook` command directly (no shell), so there
    // are no scripts to stage — setup-agent prints the YAML block instead.
    if matches!(args.agent, AgentChoice::Hermes) {
        let plugin_dir = install_hooks::hermes_plugin_dir(&config.runtime_env);
        print!("{}", hermes_setup_output(&args, plugin_dir.as_deref()));
        return Ok(());
    }
    // Droid has no script bundle either: its hooks.json runs the native
    // `hook` command.
    if matches!(args.agent, AgentChoice::Droid) {
        emit_droid(&args)?;
        return Ok(());
    }
    // Muse too: its settings.json hooks run the native `hook` command.
    if matches!(args.agent, AgentChoice::Muse) {
        emit_muse(&args)?;
        return Ok(());
    }
    // CodeBuddy too: ~/.codebuddy/settings.json runs the native `hook` command.
    if matches!(args.agent, AgentChoice::CodeBuddy) {
        emit_codebuddy(&args)?;
        return Ok(());
    }
    // IBM Bob's settings run the native hook command directly too.
    if matches!(args.agent, AgentChoice::IbmBob) {
        emit_ibm_bob(&args)?;
        return Ok(());
    }
    if matches!(args.agent, AgentChoice::Cline) {
        bail!(
            "{}", i18n.text("cli-setup-agent-cline-discovers-generated-hook-files-in-the-host-user-s-hook-directory-run-a"));
    }
    // Qwen Code too: ~/.qwen/settings.json runs the native `hook` command.
    if matches!(args.agent, AgentChoice::QwenCode) {
        emit_qwen_code(&args)?;
        return Ok(());
    }
    let Some(agent_sub) = args.agent.script_hook_subdir() else {
        bail!("{}", i18n.text("cli-setup-agent-internal-generated-integration-should-have-returned-before-staging-hooks"))
    };
    eprintln!(
        "{}", i18n.format("cli-setup-agent-ai-memory-setup-agent-emits-shell-powershell-hook-bundles-for-agent-sub-thes", &[("value0", agent_sub.to_string())]));

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
        install_hooks::copy_replacing(&from, &to)?;
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
        "{}",
        i18n.format(
            "cli-setup-agent-extracted-copied-hook-script-s-from-to",
            &[
                ("value0", format!("{}", copied)),
                ("value1", format!("{}", source.display())),
                ("value2", format!("{}", dest_dir.display()))
            ]
        )
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
        AgentChoice::Codex => emit_other(
            &config.i18n,
            &emit_root,
            agent_sub,
            &args,
            &[CODEX_PROFILE.events],
        ),
        AgentChoice::CommandCode => emit_other(
            &config.i18n,
            &emit_root,
            agent_sub,
            &args,
            &[COMMAND_CODE_PROFILE.events],
        ),
        AgentChoice::Cursor => emit_other(
            &config.i18n,
            &emit_root,
            agent_sub,
            &args,
            &[CURSOR_PROFILE.events],
        ),
        AgentChoice::GeminiCli => {
            emit_other(
                &config.i18n,
                &emit_root,
                agent_sub,
                &args,
                &[GEMINI_PROFILE.events],
            );
        }
        AgentChoice::AntigravityCli => emit_other(
            &config.i18n,
            &emit_root,
            agent_sub,
            &args,
            &[&ANTIGRAVITY_TOOL_EVENTS, &ANTIGRAVITY_LIFECYCLE_EVENTS],
        ),
        // Kimi's success and failure post-tool events share one script. The
        // script listing is deduplicated below; apply-mode owns the exact
        // `[[hooks]]` TOML merge into the user's config.toml.
        AgentChoice::KimiCode => emit_other(
            &config.i18n,
            &emit_root,
            agent_sub,
            &args,
            &[&KIMI_CODE_EVENTS],
        ),
        AgentChoice::KiroCli => {
            emit_other(
                &config.i18n,
                &emit_root,
                agent_sub,
                &args,
                &[&KIRO_CLI_V2_EVENTS],
            );
        }
        AgentChoice::KiroCliV3 => {
            emit_other(
                &config.i18n,
                &emit_root,
                agent_sub,
                &args,
                &[&KIRO_CLI_V3_EVENTS],
            );
        }
        AgentChoice::Pool => emit_pool(&emit_root, &args),
        AgentChoice::Dsh => emit_dsh(&emit_root, &args),
        AgentChoice::CopilotCli => {
            emit_other(
                &config.i18n,
                &emit_root,
                agent_sub,
                &args,
                &[COPILOT_CLI_PROFILE.events],
            );
        }
        AgentChoice::OpenCode
        | AgentChoice::OpenCode2
        | AgentChoice::Pi
        | AgentChoice::Omp
        | AgentChoice::Openclaw
        | AgentChoice::Zero
        | AgentChoice::Zcode
        | AgentChoice::Hermes
        | AgentChoice::Droid
        | AgentChoice::Muse
        | AgentChoice::CodeBuddy
        | AgentChoice::IbmBob
        | AgentChoice::Cline
        | AgentChoice::QwenCode
        | AgentChoice::Qoder => {
            bail!(
                "{}", i18n.text("cli-setup-agent-internal-generated-integration-should-have-returned-before-emitting-staged-h"))
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
    print!(
        "{}",
        super::install_hooks::zero_install_notes("<ai-memory data dir>")
    );
    println!();
    println!("{serialized}");
    Ok(())
}

/// Print ZCode's `hooks` block (#512). No scripts are staged: ZCode
/// spawns the ai-memory binary exec-form (`type: "process"`) with the
/// event JSON on stdin, so the only artifact is the config block. The
/// binary must be reachable on the host that runs ZCode — for
/// docker-wrapper setups install the native binary or point `command`
/// at the wrapper.
fn emit_zcode(args: &SetupAgentArgs) -> Result<()> {
    let payload = crate::commands::render_shared::build_zcode_hooks_config(
        &args.server_url,
        args.auth_token.as_deref(),
        None,
        None,
    );
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing ZCode hook config")?;
    println!("# ZCode (z.ai) — merge the `hooks` block into ~/.zcode/cli/config.json");
    println!("# The `command` must be an ai-memory binary reachable on the host");
    println!("# that runs ZCode; prefer `ai-memory install-hooks --agent zcode --apply`");
    println!("# from that host so the path is resolved for you.");
    if args.auth_token.is_some() {
        println!("#       Treat the config as sensitive (chmod 600).");
    }
    println!("# NOTE: ZCode injects SessionStart stdout as model context, so the");
    println!("#       prior session's handoff is delivered automatically.");
    println!("# NOTE: ZCode fires `Stop` per turn and has no SessionEnd — close");
    println!("#       sessions with `ai-memory finalize-session --agent zcode`.");
    println!();
    println!("{serialized}");
    Ok(())
}

/// Print the `hooks:` block for `~/.hermes/config.yaml` (the follow-up to
/// #623, where Hermes was accepted for capture/storage but had no installer).
/// No scripts are staged: Hermes splits the configured `command` itself and
/// runs the ai-memory binary directly with the JSON payload on stdin. The
/// binary must be reachable on the host that runs Hermes — for docker-wrapper
/// setups install the native binary or point `command` at the wrapper.
///
/// The community memory-provider plugin is looked for where this command
/// runs. Inside a container that is not the host's Hermes home, which is why
/// the full block also says which entries to drop beside the plugin.
fn hermes_setup_output(args: &SetupAgentArgs, plugin_dir: Option<&Path>) -> String {
    let hook_set = install_hooks::hermes_hook_set(plugin_dir);
    let block = crate::commands::render_shared::build_hermes_hooks_yaml(
        &args.server_url,
        args.auth_token.as_deref(),
        None,
        None,
        hook_set,
    );
    let mut out = String::new();
    out.push_str("# Hermes Agent — merge the `hooks:` block into ~/.hermes/config.yaml\n");
    out.push_str("# The `command` must be an ai-memory binary reachable on the host that\n");
    out.push_str("# runs Hermes; prefer `ai-memory install-hooks --agent hermes` from\n");
    out.push_str("# that host so the path is resolved for you.\n");
    if args.auth_token.is_some() {
        out.push_str("#       Treat the config as sensitive (chmod 600).\n");
    }
    out.push_str("# NOTE: Hermes splits each `command` with shlex.split and runs it with\n");
    out.push_str("#       no shell, so the generated line is argv, not a shell command.\n");
    out.push_str(&install_hooks::hermes_hook_set_notes(hook_set, plugin_dir));
    out.push('\n');
    out.push_str(&block);
    out.push('\n');
    out
}

/// Print the event map for `~/.factory/hooks.json` (#1240). No scripts are
/// staged: each entry runs the ai-memory binary's native `hook` command with
/// Droid's event JSON on stdin, so the binary must be reachable on the host
/// that runs Droid.
fn emit_droid(args: &SetupAgentArgs) -> Result<()> {
    let payload = crate::commands::render_shared::build_droid_hooks_payload(
        &args.server_url,
        args.auth_token.as_deref(),
        None,
        None,
    );
    let serialized =
        serde_json::to_string_pretty(&payload["hooks"]).context("serializing Droid hook config")?;
    println!("# Factory Droid — merge into ~/.factory/hooks.json (the file is the");
    println!("# event map itself). The `command` must be an ai-memory binary reachable");
    println!("# on the host that runs Droid; prefer `ai-memory install-hooks --agent");
    println!("# droid --apply` from that host so the path is resolved for you.");
    if args.auth_token.is_some() {
        println!("#       Treat hooks.json as sensitive (chmod 600).");
    }
    println!();
    println!("{serialized}");
    Ok(())
}

/// Print the `hooks` block for Muse's `settings.json`. No scripts are staged:
/// each entry runs the ai-memory binary's native `hook` command with Muse's
/// event JSON on stdin, so the binary must be reachable on the host that runs
/// Muse.
fn emit_muse(args: &SetupAgentArgs) -> Result<()> {
    let payload = crate::commands::render_shared::build_muse_hooks_payload(
        &args.server_url,
        args.auth_token.as_deref(),
        None,
        None,
    );
    let serialized =
        serde_json::to_string_pretty(&payload["hooks"]).context("serializing Muse hook config")?;
    println!("# Meta Muse Code — merge under the `hooks` key of");
    println!("# ${{XDG_CONFIG_HOME:-~/.config}}/muse/settings.json, keeping its");
    println!("# \"schema_version\": 1. The `command` must be an ai-memory binary reachable");
    println!("# on the host that runs Muse; prefer `ai-memory install-hooks --agent muse");
    println!("# --apply` from that host so the path and data dir are resolved for you.");
    if args.auth_token.is_some() {
        println!("#       Treat settings.json as sensitive (chmod 600).");
    }
    println!();
    println!("{serialized}");
    Ok(())
}

/// Print the `hooks` block for `~/.codebuddy/settings.json`. No scripts are
/// staged: each entry runs the ai-memory binary's native `hook` command with
/// CodeBuddy's event JSON on stdin.
fn emit_codebuddy(args: &SetupAgentArgs) -> Result<()> {
    let payload = crate::commands::render_shared::build_codebuddy_hooks_payload(
        &args.server_url,
        args.auth_token.as_deref(),
        None,
        None,
    );
    let serialized = serde_json::to_string_pretty(&payload["hooks"])
        .context("serializing CodeBuddy hook config")?;
    println!("# CodeBuddy Code — merge under the `hooks` key of");
    println!("# ~/.codebuddy/settings.json. The `command` must be an ai-memory");
    println!("# binary reachable on the host that runs CodeBuddy; prefer");
    println!("# `ai-memory install-hooks --agent codebuddy --apply` from that host.");
    if args.auth_token.is_some() {
        println!("#       Treat settings.json as sensitive (chmod 600).");
    }
    println!();
    println!("{serialized}");
    Ok(())
}

/// Print the `hooks` block for IBM Bob's shared global settings. No scripts
/// are staged: each entry runs the native `hook` command.
fn emit_ibm_bob(args: &SetupAgentArgs) -> Result<()> {
    let payload = crate::commands::render_shared::build_ibm_bob_hooks_payload(
        &args.server_url,
        args.auth_token.as_deref(),
        None,
        None,
    );
    let serialized = serde_json::to_string_pretty(&payload["hooks"])
        .context("serializing IBM Bob hook config")?;
    println!("# IBM Bob — merge under the `hooks` key of");
    println!("# ~/.bob/settings/settings.json. The `command` must be an ai-memory");
    println!("# binary reachable on the host that runs Bob; prefer");
    println!("# `ai-memory install-hooks --agent ibm-bob --apply` from that host.");
    if args.auth_token.is_some() {
        println!("#       Treat settings.json as sensitive (chmod 600).");
    }
    println!();
    println!("{serialized}");
    Ok(())
}

/// Print the `hooks` block for `~/.qwen/settings.json`. No scripts are
/// staged: each entry runs the native `hook` command.
fn emit_qwen_code(args: &SetupAgentArgs) -> Result<()> {
    let payload = crate::commands::render_shared::build_qwen_code_hooks_payload(
        &args.server_url,
        args.auth_token.as_deref(),
        None,
        None,
    );
    let serialized = serde_json::to_string_pretty(&payload["hooks"])
        .context("serializing Qwen Code hook config")?;
    println!("# Qwen Code — merge under the `hooks` key of");
    println!("# ~/.qwen/settings.json. The `command` must be an ai-memory");
    println!("# binary reachable on the host that runs Qwen Code; prefer");
    println!("# `ai-memory install-hooks --agent qwen-code --apply` from that host.");
    if args.auth_token.is_some() {
        println!("#       Treat settings.json as sensitive (chmod 600).");
    }
    println!();
    println!("{serialized}");
    Ok(())
}

fn normalise_hook_server_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

fn extension_setup_metadata(
    agent: AgentChoice,
) -> Result<(&'static str, &'static str, &'static str, &'static str)> {
    Ok(match agent {
        AgentChoice::OpenCode => (
            "OpenCode",
            "opencode",
            "Then restart OpenCode so it loads ~/.config/opencode/plugins/ai-memory.ts.",
            "opencode",
        ),
        AgentChoice::OpenCode2 => (
            "OpenCode 2",
            "opencode2",
            "Then restart OpenCode 2 so it loads ~/.config/opencode/plugins/ai-memory-opencode2.ts.",
            "opencode2",
        ),
        AgentChoice::Omp => (
            "OMP",
            "omp",
            "Then restart OMP so it loads ~/.omp/agent/extensions/ai-memory-omp.ts.",
            "omp",
        ),
        AgentChoice::Pi => (
            "Pi",
            "pi",
            "Then restart Pi so it loads ~/.pi/agent/extensions/ai-memory-pi.ts.",
            "pi",
        ),
        AgentChoice::Openclaw => (
            "OpenClaw",
            "openclaw",
            "Then restart the OpenClaw gateway if it does not auto-restart after plugin install.",
            "openclaw",
        ),
        other => bail!("internal: {other:?} is not a generated-integration agent"),
    })
}

fn emit_extension_setup_hint(config: &Config, args: &SetupAgentArgs) -> Result<()> {
    let i18n = &config.i18n;
    let (label, agent, restart_note, mcp_client) = extension_setup_metadata(args.agent)?;
    println!(
        "{}",
        i18n.format(
            "cli-setup-agent-label-uses-a-typescript-extension-plugin-not-extracted-shell-scripts",
            &[("value0", label.to_string())]
        )
    );
    println!(
        "{}", i18n.text("cli-setup-agent-capture-policy-capability-v1-requires-a-freshly-generated-integration-re-run"));
    println!(
        "{}", i18n.text("cli-setup-agent-legacy-shell-powershell-and-remote-only-paths-are-unsupported-compatibility"));
    println!(
        "{}",
        i18n.text("cli-setup-agent-install-it-directly-instead")
    );
    if matches!(args.agent, AgentChoice::OpenCode | AgentChoice::OpenCode2) {
        let dialect = if args.agent == AgentChoice::OpenCode {
            ai_memory_workstream::OpenCodeDialect::V1
        } else {
            ai_memory_workstream::OpenCodeDialect::V2
        };
        install_hooks::run_with_opencode_dialect(
            config,
            InstallHooksArgs {
                agent: AgentChoice::OpenCode,
                hooks_dir: None,
                server_url: Some(args.server_url.clone()),
                auth_token: args.auth_token.clone(),
                as_user: None,
                apply: false,
                config_file: None,
                scope: HookInstallScope::Global,
                project_strategy: None,
                capture_assistant: false,
                capture_mode: None,
                no_capture_prompts: false,
                capture_prompts: false,
                profile: None,
            },
            Some(dialect),
        )?;
        install_mcp::run_with_opencode_dialect(
            config,
            InstallMcpArgs {
                client: McpClient::OpenCode,
                server_url: Some(args.server_url.clone()),
                name: "ai-memory".to_string(),
                auth_token: args.auth_token.clone(),
                apply: false,
                config_file: None,
                session_aware: false,
                flavor: None,
                token_env_var: false,
                stdio_bridge: false,
                bridge_data_dir: None,
            },
            Some(dialect),
        )?;
        println!();
        println!("{restart_note}");
        return Ok(());
    }
    println!("ai-memory install-hooks --agent {agent} --apply \\");
    if args.auth_token.is_some() {
        println!(
            "{}",
            i18n.format(
                "cli-setup-agent-server-url",
                &[("value0", args.server_url.to_string())]
            )
        );
        println!("{}", i18n.text("cli-setup-agent-auth-token-token"));
    } else {
        println!(
            "{}",
            i18n.format(
                "cli-setup-agent-server-url-2",
                &[("value0", args.server_url.to_string())]
            )
        );
        println!(
            "{}",
            i18n.text("cli-setup-agent-add-auth-token-token-if-the-server-requires-bearer-auth")
        );
    }
    println!();
    println!("{restart_note}");
    if matches!(args.agent, AgentChoice::Pi) {
        println!(
            "{}", i18n.text("cli-setup-agent-mcp-tools-come-through-the-same-generated-pi-bridge-extension-no-native-mcp"));
    } else {
        println!("{}", i18n.format("cli-setup-agent-also-run-ai-memory-install-mcp-client-mcp-client-to-wire-mcp-separately", &[("value0", mcp_client.to_string())]));
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
    println!("#       lifecycle events but does not inject handoffs on that event.");
    println!("#       `install-hooks --agent grok --apply` also writes");
    println!("#       $GROK_HOME/rules/ai-memory-handoff.md so the first turn");
    println!("#       calls memory_handoff_list (PostToolUse then claims it).");
    println!("# Tip: also run `ai-memory install-mcp --client grok --auth-token <…>`");
    println!(
        "#      to register the MCP endpoint in {}.",
        grok_home.join("config.toml").display()
    );
    println!();
    println!("{serialized}");
    Ok(())
}

/// Print Pool's `.poolside/settings.yaml` `hooks:` block. The staged `.sh`
/// scripts are the artifact — Pool's hook config is a project-scoped YAML
/// file at the repo root, so the snippet is pasted into each repository
/// rather than written by ai-memory.
fn emit_pool(emit_root: &Path, args: &SetupAgentArgs) {
    let snippet = build_pool_settings_yaml(emit_root, &args.server_url, args.auth_token.as_deref());
    println!("# Pool (Poolside Agent CLI) — merge into the repo-root .poolside/settings.yaml");
    println!("# of each project Pool runs in; ai-memory does not write committed project files.");
    println!("# Hook scripts (must be reachable from the host that runs Pool):");
    println!("#   {}", emit_root.display());
    println!("# AI-memory server: {}", args.server_url);
    if args.auth_token.is_some() {
        println!("# Auth: AI_MEMORY_AUTH_TOKEN embedded in each hook command below.");
        println!("#       Treat .poolside/settings.yaml as sensitive (chmod 600).");
    }
    println!("# NOTE: Pool's SessionStart stdout injection is not demonstrated — capture");
    println!("#       works, but handoff injection does not. Recover handoffs via the");
    println!("#       MCP memory_handoff_accept tool, and close finished sessions with");
    println!("#       `ai-memory finalize-session --agent pool` (no true session-end event).");
    println!();
    print!("{snippet}");
}

/// Print DSH's generated Claude Code `hooks.json` plus the Cordis patch
/// fragment that points the `@deepseek-ai/dsh-hooks-claude-code` bridge at
/// it. The staged `.sh`/`.ps1` scripts are the artifact; the bridge reads the
/// hooks.json via `configPath` after the operator merges the patch into
/// `$DSH_HOME/profiles/<profile>/cordis.patch.yml`.
fn emit_dsh(emit_root: &Path, args: &SetupAgentArgs) {
    let payload = build_dsh_hooks_payload(
        emit_root,
        &args.server_url,
        args.auth_token.as_deref(),
        None,
        None,
    );
    println!("# DeepSeek Harness (DSH) — DSH has no fixed agent config file.");
    println!("# Hook scripts (must be reachable from the host that runs DSH):");
    println!("#   {}", emit_root.display());
    println!("# AI-memory server: {}", args.server_url);
    if args.auth_token.is_some() {
        println!("# Auth: AI_MEMORY_AUTH_TOKEN embedded in each hook command below.");
        println!("#       Treat the hook config as sensitive (chmod 600).");
    }
    println!("# NOTE: the bridge delivers exactly seven events — no PreCompact or");
    println!("#       SessionEnd, and SessionStart runs detached, so capture works");
    println!("#       but handoff injection does not. Recover handoffs via the MCP");
    println!("#       memory_handoff_accept tool, and close finished sessions with");
    println!("#       `ai-memory finalize-session --agent dsh`.");
    println!();
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).expect("static hook payload serializes")
    );
    println!();
    print!("{}", install_hooks::dsh_bridge_patch_fragment(emit_root));
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
    i18n: &ai_memory_core::i18n::Localizer,
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
    println!(
        "{}",
        i18n.diagnostic(
            "Set AI_MEMORY_HOOK_URL in each hook's environment to override the default."
        )
    );
    println!(
        "{}",
        i18n.diagnostic(&format!(
            "Also run `ai-memory install-mcp --client {label}` to wire MCP separately."
        ))
    );
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
        install_hooks::copy_replacing(&shared_lib, &to)?;
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
        install_hooks::copy_replacing(&from, &to)?;
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
    let mut v = Vec::with_capacity(5);
    // Standard Unix prefix layout: `<prefix>/bin/<bin>` ships its bundle at
    // `<prefix>/share/ai-memory/hooks/<sub>` (Nix, Homebrew, custom prefix).
    // Probed before the fixed /usr paths, as install-hooks does, so a prefix
    // install never stages a stale system-wide bundle.
    if let Some(dir) = exe.as_deref().and_then(Path::parent)
        && dir.file_name().is_some_and(|name| name == "bin")
        && let Some(prefix) = dir.parent()
    {
        v.push(
            prefix
                .join("share")
                .join("ai-memory")
                .join(crate::install_layout::HOOKS_DIR_NAME)
                .join(sub),
        );
    }
    // Docker image lays them out under /usr/local/share/.
    v.push(PathBuf::from(format!(
        "/usr/local/share/ai-memory/hooks/{sub}"
    )));
    // Native Linux packages install hook sources under /usr/share.
    v.push(PathBuf::from(format!("/usr/share/ai-memory/hooks/{sub}")));
    if let Some(exe) = exe {
        // Release tarball: `hooks/` sits in the same dir as the binary.
        if let Some(dir) = exe.parent() {
            v.push(dir.join(crate::install_layout::HOOKS_DIR_NAME).join(sub));
        }
        // Repo-local fallback for `cargo run setup-agent` during dev:
        // target/<profile>/<bin> → repo root.
        if let Some(root) = exe.parent().and_then(Path::parent).and_then(Path::parent) {
            v.push(root.join(crate::install_layout::HOOKS_DIR_NAME).join(sub));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hermes_setup_args() -> SetupAgentArgs {
        use clap::Parser as _;
        let cli = crate::cli::Cli::try_parse_from([
            "ai-memory",
            "setup-agent",
            "--agent",
            "hermes",
            "--to",
            "/host/hooks",
        ])
        .unwrap();
        let crate::cli::Command::SetupAgent(args) = cli.command else {
            panic!("expected setup-agent");
        };
        args
    }

    /// `setup-agent --agent hermes` renders the same two blocks as
    /// `install-hooks`: the whole lifecycle, or the tool events alone when the
    /// community plugin is installed.
    #[test]
    fn hermes_setup_output_follows_the_plugin_check() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path().join("plugins").join("ai-memory");
        let full = hermes_setup_output(&hermes_setup_args(), Some(&plugin));
        assert!(full.contains("pre_llm_call:") && full.contains("on_session_finalize:"));
        assert!(full.contains("pre_tool_call:") && full.contains("post_tool_call:"));
        assert!(full.contains("keep only pre_tool_call"));

        std::fs::create_dir_all(&plugin).unwrap();
        let tools = hermes_setup_output(&hermes_setup_args(), Some(&plugin));
        assert!(tools.contains("pre_tool_call:") && tools.contains("post_tool_call:"));
        for event in [
            "on_session_start:",
            "pre_llm_call:",
            "on_session_end:",
            "on_session_finalize:",
        ] {
            assert!(!tools.contains(event), "{event}");
        }
        assert!(tools.contains("memory-provider plugin is installed at"));
    }

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
    fn source_candidates_include_standard_prefix_share_dir() {
        let exe = PathBuf::from("/nix/store/pkg/bin/ai-memory");
        let candidates = source_candidates(None, "claude-code", Some(exe));
        assert!(
            candidates.contains(&PathBuf::from(
                "/nix/store/pkg/share/ai-memory/hooks/claude-code"
            )),
            "prefix share hooks/ dir must be probed; got {candidates:?}"
        );
        let position = |p: &str| candidates.iter().position(|c| c == Path::new(p));
        assert!(
            position("/nix/store/pkg/share/ai-memory/hooks/claude-code")
                < position("/usr/share/ai-memory/hooks/claude-code"),
            "prefix share must be probed before /usr/share; got {candidates:?}"
        );
    }

    /// Nix ships hook sources 0444 and `fs::copy` keeps that mode, so a
    /// second `setup-agent` used to fail copying onto its own staged copy.
    #[cfg(unix)]
    #[test]
    fn staging_replaces_read_only_existing_copies() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let source = tmp.path().join("src/hooks/claude-code");
        fs::create_dir_all(source.parent().unwrap().join("lib")).unwrap();
        fs::create_dir_all(&source).unwrap();
        let script = source.join("session-start.sh");
        let shared = source.parent().unwrap().join("_lib.sh");
        let support = source.parent().unwrap().join("lib/helper.ps1");
        for path in [&script, &shared, &support] {
            fs::write(path, "new\n").unwrap();
        }
        let dest = tmp.path().join("dest/hooks/claude-code");
        fs::create_dir_all(dest.parent().unwrap().join("lib")).unwrap();
        fs::create_dir_all(&dest).unwrap();
        let staged = [
            dest.join("session-start.sh"),
            dest.parent().unwrap().join("_lib.sh"),
            dest.parent().unwrap().join("lib/helper.ps1"),
        ];
        for path in &staged {
            fs::write(path, "old\n").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o444)).unwrap();
        }

        install_hooks::copy_replacing(&script, &staged[0]).unwrap();
        copy_support_hook_scripts(&source, &dest).unwrap();

        for path in &staged {
            assert_eq!(
                fs::read_to_string(path).unwrap(),
                "new\n",
                "{}",
                path.display()
            );
        }
    }

    #[test]
    fn source_candidates_ignores_non_bin_parent_for_prefix_share() {
        let exe = PathBuf::from("/opt/ai-memory/ai-memory");
        let candidates = source_candidates(None, "claude-code", Some(exe));
        assert!(
            !candidates.contains(&PathBuf::from("/opt/share/ai-memory/hooks/claude-code")),
            "flat tarball directory must not treat parent as prefix; got {candidates:?}"
        );
    }

    #[test]
    fn pi_setup_prints_extension_hint_without_copying() {
        let tmp = tempfile::TempDir::new().unwrap();
        let args = SetupAgentArgs {
            agent: AgentChoice::Pi,
            opencode_dialect: None,
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
    fn pi_family_setup_hints_name_the_installed_extension_files() {
        let (_, _, omp_note, _) = extension_setup_metadata(AgentChoice::Omp).unwrap();
        let (_, _, pi_note, _) = extension_setup_metadata(AgentChoice::Pi).unwrap();

        assert!(omp_note.ends_with("/.omp/agent/extensions/ai-memory-omp.ts."));
        assert!(pi_note.ends_with("/.pi/agent/extensions/ai-memory-pi.ts."));
        assert!(!omp_note.ends_with("/ai-memory.ts."));
        assert!(!pi_note.ends_with("/ai-memory.ts."));
    }

    #[test]
    fn devin_setup_emits_script_hook_config() {
        let args = SetupAgentArgs {
            agent: AgentChoice::Devin,
            opencode_dialect: None,
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
            opencode_dialect: None,
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
        // Kimi Code's hook vocabulary is Claude Code's ten events,
        // PostToolUseFailure included; the failure entry reuses
        // post-tool-use.sh, so the script list carries nine unique files for
        // ten events.
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

    #[test]
    fn kiro_cli_manual_script_paths_cover_v2_event_set() {
        let root = Path::new("/hooks/kiro-cli");
        let v2_paths = event_script_paths(root, &[&KIRO_CLI_V2_EVENTS]);
        let rendered = v2_paths
            .iter()
            .map(|path| path.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n");

        for script in [
            "session-start",
            "user-prompt-submit",
            "pre-tool-use",
            "post-tool-use",
            "stop",
        ] {
            assert!(
                rendered.contains(script),
                "Kiro CLI setup-agent must list {script}; got:\n{rendered}"
            );
        }
        // The documented v2 vocabulary has no PreCompact, SessionEnd, or
        // subagent equivalents.
        assert_eq!(KIRO_CLI_V2_EVENTS.len(), 5);
        assert_eq!(v2_paths.len(), 5);
    }

    #[test]
    fn kiro_cli_manual_script_paths_cover_v3_event_set() {
        let root = Path::new("/hooks/kiro-cli");
        let paths = event_script_paths(root, &[&KIRO_CLI_V3_EVENTS]);
        assert_eq!(KIRO_CLI_V3_EVENTS.len(), 5);
        assert_eq!(paths.len(), 5);
        assert_eq!(
            paths,
            event_script_paths(root, &[&KIRO_CLI_V2_EVENTS]),
            "v2 and v3 use different trigger names but the same bounded script bundle"
        );
    }
}
