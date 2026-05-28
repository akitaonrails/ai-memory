//! Command-line interface definition (clap derive).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Top-level CLI for the `ai-memory` binary.
#[derive(Debug, Parser)]
#[command(name = "ai-memory", version, about, long_about = None)]
pub struct Cli {
    /// Override the data directory.
    ///
    /// Defaults to a platform path under `dirs::data_local_dir()`. The
    /// config loader also honours `AI_MEMORY_DATA_DIR`.
    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,

    /// Path to an explicit config file (defaults to `<data_dir>/config.toml`).
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialise the data directory layout.
    Init(InitArgs),
    /// Print runtime status (counts, paths, version).
    Status(StatusArgs),
    /// Full-text search the wiki via FTS5.
    Search(SearchArgs),
    /// Write or update a wiki page atomically (also indexes it in the store).
    WritePage(WritePageArgs),
    /// Run the MCP server (with watcher) over stdio or HTTP.
    Serve(ServeArgs),
    /// Wipe the data directory's wiki/, db/, raw/ contents.
    Reset(ResetArgs),
    /// Snapshot wiki/, db/, and config.toml into a gzipped tarball.
    Backup(BackupArgs),
    /// Restore a backup tarball into the data directory.
    Restore(RestoreArgs),
    /// Print (or apply) lifecycle-hook configuration for an agent CLI.
    InstallHooks(InstallHooksArgs),
    /// Print MCP server registration snippets for any supported client
    /// (Claude Code, Codex, OpenCode, Cursor, Claude Desktop, Gemini
    /// CLI, OpenClaw, pi). See docs/mcp-install.md for the full guide.
    InstallMcp(InstallMcpArgs),
    /// Stage + commit the wiki tree under git.
    Commit(CommitArgs),
    /// Smoke-test an LLM provider by sending one prompt.
    LlmTest(LlmTestArgs),
    /// Run the M8 retention sweep over episodic pages.
    ForgetSweep(ForgetSweepArgs),
    /// Run the M8 lint pass (stale / duplicates + optional LLM contradiction).
    Lint(LintArgs),
    /// Compute + store embeddings for every latest page (M9).
    Embed(EmbedArgs),
    /// Generate a random hex bearer token for AI_MEMORY_AUTH_TOKEN.
    GenerateAuthToken(GenerateAuthTokenArgs),
    /// One-shot agent setup for docker deploys: extract the bundled
    /// hook scripts to a host-mounted directory AND print the matching
    /// config snippet. Replaces the "clone the repo + cargo build"
    /// workflow for users who never want a local Rust toolchain.
    SetupAgent(SetupAgentArgs),
    /// Pre-load an existing project's history into the wiki by
    /// LLM-summarising git log, README, docs/, and module headers
    /// into seed wiki pages. Run once when adopting ai-memory in a
    /// project that's been around for a while. Requires
    /// AI_MEMORY_LLM_PROVIDER configured on the server.
    Bootstrap(BootstrapArgs),
    /// Install the ai-memory usage snippet into the project's
    /// CLAUDE.md / AGENTS.md (or any markdown file you specify).
    /// Idempotent — bracketed by `<!-- ai-memory:start -->` /
    /// `<!-- ai-memory:end -->` markers so re-running replaces the
    /// block in place without duplicating.
    InstallInstructions(InstallInstructionsArgs),
    /// Retro-fit existing sessions + observations to per-cwd projects
    /// based on the cwd captured at session-start. Pages are marked
    /// `is_latest=false` (they were a multi-project mash-up) so the
    /// next consolidation can regenerate them per-project. Idempotent.
    Reorg(ReorgArgs),
    /// Permanently delete a project and ALL its data (pages, sessions,
    /// observations, handoffs, embeddings, on-disk wiki files).
    /// This is irreversible — requires `--confirm`.
    PurgeProject(PurgeProjectArgs),
    /// Rename a project within its workspace. No files move on disk —
    /// the wiki is flat and pages are differentiated by project_id only.
    /// Useful after renaming the project's directory on disk so the hook
    /// router keeps writing into the same logical project.
    RenameProject(RenameProjectArgs),
    /// Remove ai-memory's wiring (hooks, MCP, instructions) from all
    /// detected agents. Dry-run unless `--apply`.
    Uninstall(UninstallArgs),
    /// Manage optional upstream LLM provider authentication.
    Auth(AuthArgs),
}

/// Arguments for `auth`.
#[derive(Debug, Args)]
pub struct AuthArgs {
    /// Auth action to run.
    #[command(subcommand)]
    pub command: AuthCommand,
}

/// Subcommands for `auth`.
#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Sign in to an upstream provider.
    Login(AuthLoginArgs),
    /// Remove stored provider credentials.
    Logout(AuthLogoutArgs),
    /// Show stored provider auth state without printing secrets.
    Status(AuthStatusArgs),
}

/// Provider choices for `auth`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum AuthProviderChoice {
    /// OpenAI ChatGPT/Codex OAuth backend.
    OpenaiOauth,
    /// GitHub Copilot Chat backend.
    Copilot,
}

/// Arguments for `auth login`.
#[derive(Debug, Args)]
pub struct AuthLoginArgs {
    /// Provider to sign in to.
    #[arg(value_enum)]
    pub provider: AuthProviderChoice,
    /// Stop waiting for browser/device authorization after this many seconds.
    #[arg(long, default_value_t = 600)]
    pub timeout_secs: u64,
    /// GitHub token to persist for Copilot instead of running device auth.
    #[arg(long, hide_env_values = true)]
    pub github_token: Option<String>,
    /// OAuth client id override for Copilot device auth.
    #[arg(long)]
    pub client_id: Option<String>,
}

/// Arguments for `auth logout`.
#[derive(Debug, Args)]
pub struct AuthLogoutArgs {
    /// Provider to sign out from.
    #[arg(value_enum)]
    pub provider: AuthProviderChoice,
}

/// Arguments for `auth status`.
#[derive(Debug, Args)]
pub struct AuthStatusArgs {}

/// Which concern `uninstall` should touch. Omitted = all three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
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
    /// Optional MCP server entry-name filter. Uninstall never matches by name
    /// alone; when this is set, the entry must match both name and `--mcp-url`.
    #[arg(long = "mcp-name")]
    pub mcp_name: Option<String>,
    /// MCP endpoint URL used to identify ai-memory server entries. Defaults to
    /// the standard local endpoint; pass this when you installed with a custom
    /// `install-mcp --server-url`.
    #[arg(long = "mcp-url", visible_alias = "server-url", default_value_t = crate::config::DEFAULT_MCP_URL.to_string())]
    pub mcp_url: String,
    /// Skip the interactive confirmation when a TTY is attached.
    #[arg(long)]
    pub yes: bool,
}

/// Arguments for `reorg`.
#[derive(Debug, Args)]
pub struct ReorgArgs {
    /// Show what would change without writing.
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments for `purge-project`.
#[derive(Debug, Args)]
pub struct PurgeProjectArgs {
    /// Workspace name. Defaults to `default`.
    #[arg(long, default_value_t = crate::config::DEFAULT_WORKSPACE.to_string())]
    pub workspace: String,
    /// Project name. When omitted, auto-derived from the basename of
    /// the current git repo root (or CWD if no git repo).
    #[arg(long)]
    pub project: Option<String>,
    /// REQUIRED for the purge to run. Without this flag the CLI errors
    /// out — purging is destructive and irreversible.
    #[arg(long)]
    pub confirm: bool,
}

/// Arguments for `rename-project`.
#[derive(Debug, Args)]
pub struct RenameProjectArgs {
    /// Workspace name. Defaults to `default`.
    #[arg(long, default_value_t = crate::config::DEFAULT_WORKSPACE.to_string())]
    pub workspace: String,
    /// Current project name. When omitted, auto-derives from the
    /// basename of the current git repo root (or CWD) — handy when
    /// running `ai-memory rename-project --to new-name` from a dir
    /// that was JUST renamed (the basename will be the new name, so
    /// you'll want to pass --from explicitly in that workflow).
    #[arg(long)]
    pub from: Option<String>,
    /// New project name. Must be non-empty and contain no slashes.
    #[arg(long)]
    pub to: String,
}

/// Arguments for `install-instructions`.
#[derive(Debug, Args)]
pub struct InstallInstructionsArgs {
    /// Markdown file to write into. When omitted, the command picks
    /// whichever of `CLAUDE.md` or `AGENTS.md` already exists in
    /// $PWD; if both exist it writes to both; if neither exists it
    /// creates `CLAUDE.md` (Claude Code's convention) and prints a
    /// hint that Codex / OpenCode / Cursor / Gemini users likely
    /// want `--target AGENTS.md` instead. Pass `--target` explicitly
    /// to override the auto-detection.
    #[arg(long)]
    pub target: Option<PathBuf>,
    /// Print the snippet to stdout instead of mutating the file.
    /// The default IS mutation here (the print form is also
    /// available without this command — copy the block from the
    /// README). Pass `--print` to preview what would land in
    /// the file.
    #[arg(long)]
    pub print: bool,
}

/// Arguments for `bootstrap`.
#[derive(Debug, Args)]
pub struct BootstrapArgs {
    /// Path on this client whose history to collect (server never
    /// sees this path). Defaults to `git rev-parse --show-toplevel`
    /// resolved from the current directory (so running bootstrap from
    /// any subdir of the project works).
    #[arg(long)]
    pub repo_path: Option<PathBuf>,
    /// Workspace name. Defaults to `default` (the single workspace
    /// all hook-captured sessions land in today).
    #[arg(long, default_value_t = crate::config::DEFAULT_WORKSPACE.to_string())]
    pub workspace: String,
    /// Project name. When omitted, auto-derived from the basename of
    /// the resolved repo path — same heuristic the hook router uses
    /// to bucket per-cwd observations, so the bootstrap pages land
    /// in the same project as future session captures.
    ///
    /// Dot-prefixed dirs are preserved verbatim — `~/.config` becomes
    /// project `.config`, matching what the router does for sessions
    /// launched from there. Pass `--project` explicitly to override.
    #[arg(long)]
    pub project: Option<String>,
    /// Maximum total tokens of source text sent to the LLM in one
    /// run. When the collected sources exceed this, lower-priority
    /// inputs (older git commits, then code module headers, then
    /// docs) are dropped first. Default is 150K — comfortably under
    /// Haiku/Sonnet 4.5's 200K context (leaves room for the ~64K
    /// output budget) — so the model sees as much of your project
    /// as possible. Lower it explicitly only if you're cost-
    /// sensitive or running against a smaller-context provider.
    #[arg(long, default_value_t = 150_000)]
    pub max_input_tokens: usize,
    /// Max estimated input tokens per LLM call. When pruned sources exceed
    /// this, bootstrap runs multiple sequential LLM chunks instead of one
    /// giant prompt (avoids provider context limits / Cursor bridge failures).
    /// Set to `0` to disable chunking (single call with the full pruned bundle).
    #[arg(long, default_value_t = ai_memory_consolidate::DEFAULT_CHUNK_INPUT_TOKENS)]
    pub chunk_input_tokens: usize,
    /// Skip git-commit history ingestion.
    #[arg(long)]
    pub exclude_git: bool,
    /// Skip README ingestion.
    #[arg(long)]
    pub exclude_readme: bool,
    /// Skip docs/**/*.md ingestion.
    #[arg(long)]
    pub exclude_docs: bool,
    /// Skip code module headers (Rust `//!` doc-comments at the top
    /// of `**/*.rs` files).
    #[arg(long)]
    pub exclude_code: bool,
    /// git-log time filter (passed through to `git log --since`).
    /// Useful when a repo is years old and you only want recent
    /// history (e.g. `--since "180 days ago"`). Default: no limit.
    #[arg(long)]
    pub since: Option<String>,
    /// LLM-call dry run: collects sources, builds the prompt, and
    /// prints what *would* be sent — but never calls the provider
    /// and never writes to the wiki. Useful for verifying the source
    /// selection before paying for a real run.
    #[arg(long)]
    pub dry_run: bool,
    /// Re-bootstrap a project that already has a bootstrap manifest
    /// page. Without this flag, `bootstrap` refuses to run twice on
    /// the same project (the manifest is `wiki/bootstrap.md`).
    #[arg(long)]
    pub force: bool,
}

/// Arguments for `setup-agent`.
#[derive(Debug, Args)]
pub struct SetupAgentArgs {
    /// Which agent's hook bundle to extract + render.
    #[arg(long, value_enum, default_value_t = AgentChoice::ClaudeCode)]
    pub agent: AgentChoice,
    /// Filesystem directory the hook scripts get copied into. In a
    /// docker context this is the in-container path; mount a host
    /// directory there. Example:
    ///     docker run --rm -v $HOME/.ai-memory:/host ai-memory \
    ///       setup-agent --to /host/hooks ...
    #[arg(long)]
    pub to: PathBuf,
    /// Directory the rendered config JSON should reference for the
    /// hook commands. Defaults to `--to`. Set this when the path on
    /// the host (where the agent CLI runs) differs from the in-
    /// container path. Example:
    ///     --to /host/hooks  --host-prefix $HOME/.ai-memory/hooks
    #[arg(long)]
    pub host_prefix: Option<PathBuf>,
    /// MCP / hook ingress URL the agent should POST to. Defaults to the
    /// configured `server_url` / AI_MEMORY_SERVER_URL when set, else loopback.
    #[arg(long, default_value_t = crate::config::DEFAULT_SERVER_URL.to_string())]
    pub server_url: String,
    /// Bearer token embedded into each hook's env block. When omitted,
    /// uses the token resolved by the config loader.
    #[arg(long, hide_env_values = true)]
    pub auth_token: Option<String>,
    /// Source directory for the embedded hook bundle. Defaults to
    /// `/usr/local/share/ai-memory/hooks` (the docker image's
    /// bundled path) and falls back to a repo-local `hooks/` for
    /// `cargo run setup-agent` during development.
    #[arg(long)]
    pub source: Option<PathBuf>,
}

/// Arguments for `generate-auth-token`.
#[derive(Debug, Args)]
pub struct GenerateAuthTokenArgs {
    /// Number of random bytes of entropy. The token printed is hex-
    /// encoded, so the output length is 2× this value. 32 bytes
    /// (256 bits) is plenty for any homelab threat model.
    #[arg(long, default_value_t = 32)]
    pub bytes: usize,
}

/// Arguments for `init`.
#[derive(Debug, Args)]
pub struct InitArgs {
    /// Overwrite an existing `config.toml` if present.
    #[arg(long)]
    pub force: bool,
}

/// Arguments for `status`.
#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Emit the report as JSON instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `search`.
#[derive(Debug, Args)]
pub struct SearchArgs {
    /// FTS5 query string (e.g. `"karpathy wiki"` or `quick OR slow`).
    pub query: String,
    /// Workspace name. Defaults to `default`.
    #[arg(long, default_value_t = crate::config::DEFAULT_WORKSPACE.to_string())]
    pub workspace: String,
    /// Project name. When omitted, auto-derived from the current project.
    #[arg(long)]
    pub project: Option<String>,
    /// Maximum number of hits to return.
    #[arg(short = 'n', long, default_value_t = 10)]
    pub limit: usize,
    /// Emit results as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `reset`.
#[derive(Debug, Args)]
pub struct ResetArgs {
    /// Required to actually wipe data. Without this we just dry-run.
    #[arg(long)]
    pub confirm: bool,
}

/// Arguments for `backup`.
#[derive(Debug, Args)]
pub struct BackupArgs {
    /// Destination tarball (`.tar.gz`).
    #[arg(long, short = 'o')]
    pub to: PathBuf,
}

/// Arguments for `restore`.
#[derive(Debug, Args)]
pub struct RestoreArgs {
    /// Source tarball.
    #[arg(long, short = 'i')]
    pub from: PathBuf,
    /// Overwrite an existing non-empty data dir.
    #[arg(long)]
    pub force: bool,
}

/// Agent CLI to install hooks/extensions for. For MCP-only clients
/// (Claude Desktop), use `install-mcp --client <name>` instead.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum AgentChoice {
    /// Anthropic Claude Code.
    ClaudeCode,
    /// OpenAI Codex CLI.
    Codex,
    /// Cursor IDE agent — JSON-config hooks in `~/.cursor/hooks.json`.
    Cursor,
    /// Google Gemini CLI — JSON-config hooks in `~/.gemini/settings.json`.
    GeminiCli,
    /// OpenCode (open-source coding agent) — TypeScript plugin hooks
    /// under `~/.config/opencode/plugins/`. `--apply` writes the plugin
    /// file directly; restart OpenCode for it to load.
    ///
    /// The `opencode` (no hyphen) alias matches both the staged hook
    /// dir on disk (`~/.local/share/ai-memory/hooks/opencode/`) and
    /// what users commonly type. Without it, `ai-memory upgrade`'s
    /// hook-refresh loop iterates the staged dir names and passes
    /// them straight to `--agent`, which used to fail on this one.
    #[value(alias = "opencode")]
    OpenCode,
    /// Oh My Pi (`omp`) / Pi coding agent fork — TypeScript extension
    /// under `~/.omp/agent/extensions/`. `--apply` writes the extension
    /// file directly; restart `omp` for it to load.
    #[value(alias = "pi", alias = "oh-my-pi")]
    Omp,
    /// OpenClaw personal AI gateway — native plugin package with
    /// session/tool/compaction hooks.
    Openclaw,
    /// Google Antigravity CLI (`agy`) — JSON-config hooks in
    /// `~/.gemini/config/hooks.json`.
    #[value(alias = "antigravity", alias = "agy")]
    AntigravityCli,
}

/// MCP client to render configuration for. Includes both the
/// hook-capable agents (Claude Code / Codex / OpenCode — same MCP
/// surface, also covered by `install-hooks`) and the MCP-only
/// clients researched in docs/mcp-install.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum McpClient {
    /// Anthropic Claude Code — `claude mcp add`.
    ClaudeCode,
    /// OpenAI Codex CLI — `~/.codex/config.toml`.
    Codex,
    /// OpenCode — `opencode.json`. Accepts `opencode` (no hyphen) as
    /// an alias for symmetry with `AgentChoice` and the on-disk
    /// hook-staging dir name.
    #[value(alias = "opencode")]
    OpenCode,
    /// Cursor IDE — `~/.cursor/mcp.json` or `.cursor/mcp.json`.
    Cursor,
    /// Anthropic Claude Desktop — uses the `mcp-remote` stdio shim
    /// to talk to ai-memory's HTTP endpoint (Claude Desktop's JSON
    /// config does not register HTTP transports directly).
    ClaudeDesktop,
    /// Google Gemini CLI — `~/.gemini/settings.json`.
    GeminiCli,
    /// OpenClaw personal AI gateway — `~/.openclaw/config.json`.
    Openclaw,
    /// Oh My Pi (`omp`) / Pi-compatible coding agent — `~/.omp/agent/mcp.json`.
    #[value(alias = "omp", alias = "oh-my-pi")]
    Pi,
    /// Google Antigravity CLI (`agy`) — `~/.gemini/antigravity-cli/mcp_config.json`.
    #[value(alias = "antigravity", alias = "agy")]
    AntigravityCli,
}

/// Arguments for `commit`.
#[derive(Debug, Args)]
pub struct CommitArgs {
    /// Commit message.
    #[arg(long, short = 'm', default_value = "manual commit")]
    pub message: String,
}

/// LLM provider for `llm-test`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum LlmProviderChoice {
    /// Anthropic Messages API.
    Anthropic,
    /// Anthropic Messages API using a Claude subscription OAuth token.
    AnthropicOauth,
    /// OpenAI Chat Completions.
    Openai,
    /// Google Gemini (Generative Language API).
    Gemini,
    /// OpenAI-compatible local (Ollama, vLLM, LM Studio).
    OpenaiCompat,
    /// OpenAI ChatGPT/Codex OAuth backend.
    OpenaiOauth,
    /// GitHub Copilot Chat backend.
    Copilot,
}

/// Arguments for `embed`.
#[derive(Debug, Args)]
pub struct EmbedArgs {
    /// Report what would be embedded without actually mutating.
    #[arg(long)]
    pub dry_run: bool,
    /// Re-embed pages even when they already have a row with the
    /// currently-configured `(provider, model, dim)`.
    #[arg(long)]
    pub force: bool,
    /// Workspace name (auto-created if absent).
    #[arg(long, default_value_t = crate::config::DEFAULT_WORKSPACE.to_string())]
    pub workspace: String,
    /// Project name. When omitted, auto-derived from the basename of
    /// the current git repo root (or CWD if no git repo). Matches the
    /// hook router's per-cwd convention so this command targets the
    /// same project sessions write into.
    #[arg(long)]
    pub project: Option<String>,
}

/// Arguments for `forget-sweep`.
#[derive(Debug, Args)]
pub struct ForgetSweepArgs {
    /// Report what would be evicted without actually mutating.
    #[arg(long)]
    pub dry_run: bool,
    /// Workspace name (auto-created if absent).
    #[arg(long, default_value_t = crate::config::DEFAULT_WORKSPACE.to_string())]
    pub workspace: String,
    /// Project name. When omitted, auto-derived from the basename of
    /// the current git repo root (or CWD if no git repo).
    #[arg(long)]
    pub project: Option<String>,
}

/// Arguments for `lint`.
#[derive(Debug, Args)]
pub struct LintArgs {
    /// Compute findings but don't write `wiki/_lint/<date>.md`.
    #[arg(long)]
    pub dry_run: bool,
    /// Run only rule-based checks; skip the LLM contradiction pass.
    /// Lets you get fast results without consuming LLM tokens when a
    /// provider is configured.
    #[arg(long)]
    pub no_llm: bool,
    /// Workspace name (auto-created if absent).
    #[arg(long, default_value_t = crate::config::DEFAULT_WORKSPACE.to_string())]
    pub workspace: String,
    /// Project name. When omitted, auto-derived from the basename of
    /// the current git repo root (or CWD if no git repo).
    #[arg(long)]
    pub project: Option<String>,
}

/// Arguments for `llm-test`.
#[derive(Debug, Args)]
pub struct LlmTestArgs {
    /// Provider to test.
    #[arg(long, value_enum)]
    pub provider: LlmProviderChoice,
    /// Model identifier (e.g. `claude-sonnet-4-6`, `gpt-4o-mini`, `llama3.1:8b`).
    #[arg(long)]
    pub model: String,
    /// Prompt to send.
    #[arg(long)]
    pub prompt: String,
    /// Base URL override (required for openai-compat).
    #[arg(long)]
    pub base_url: Option<String>,
    /// Optional API key override (otherwise pulled from env).
    #[arg(long, hide_env_values = true)]
    pub api_key: Option<String>,
}

/// Arguments for `install-hooks`.
#[derive(Debug, Args)]
pub struct InstallHooksArgs {
    /// Which agent's hooks to render.
    #[arg(long, value_enum, default_value_t = AgentChoice::ClaudeCode)]
    pub agent: AgentChoice,
    /// Filesystem root that contains the vendored hook scripts (defaults
    /// to the repo's `hooks/` if known, else `/usr/local/share/ai-memory/hooks`).
    /// Ignored for generated TypeScript integrations (OpenCode, OMP, OpenClaw).
    #[arg(long)]
    pub hooks_dir: Option<PathBuf>,
    /// Server URL the hooks will POST to. Defaults to the configured
    /// `server_url` / AI_MEMORY_SERVER_URL when set, else loopback. If neither
    /// is configured, apply-mode also reuses an existing ai-memory MCP entry
    /// for the same agent when one is present.
    #[arg(long, default_value_t = crate::config::DEFAULT_SERVER_URL.to_string())]
    pub server_url: String,
    /// Bearer token to embed in the hook config's `env` block. When
    /// set, every hook call carries `Authorization: Bearer <token>`,
    /// matching what the server requires when AI_MEMORY_AUTH_TOKEN
    /// is set there. Generate one with `ai-memory generate-auth-token`.
    #[arg(long, hide_env_values = true)]
    pub auth_token: Option<String>,
    /// **Mutate** the selected agent's hook config in place instead of
    /// printing the snippet/plugin. Idempotent — replaces the ai-memory
    /// hook entries or generated plugin and preserves unrelated config
    /// where the agent format supports merging. A timestamped backup is
    /// written next to the original before each modifying write.
    #[arg(long)]
    pub apply: bool,
    /// Override the settings/plugin/extension file path for the selected agent.
    /// For OpenClaw, this is the generated plugin package directory.
    #[arg(long)]
    pub config_file: Option<PathBuf>,
}

/// Arguments for `install-mcp`.
#[derive(Debug, Args)]
pub struct InstallMcpArgs {
    /// Which MCP client to render configuration for.
    #[arg(long, value_enum, default_value_t = McpClient::ClaudeCode)]
    pub client: McpClient,
    /// MCP HTTP endpoint URL the client should connect to. Defaults to the
    /// configured `server_url` / AI_MEMORY_SERVER_URL plus `/mcp` when set,
    /// else loopback.
    #[arg(long, default_value_t = crate::config::DEFAULT_MCP_URL.to_string())]
    pub server_url: String,
    /// Friendly name the client should show for this server entry.
    #[arg(long, default_value = "ai-memory")]
    pub name: String,
    /// Bearer token to embed in the client config. When set, the
    /// rendered snippet includes an `Authorization: Bearer <token>`
    /// header so the client can authenticate against a server that
    /// requires it. When omitted, uses the token resolved by the config loader.
    #[arg(long, hide_env_values = true)]
    pub auth_token: Option<String>,
    /// **Mutate** the client's config file in place instead of just
    /// printing the snippet. Idempotent: replaces any existing entry
    /// named `<name>` (default `ai-memory`); preserves every other
    /// MCP server the user has configured. A timestamped backup is
    /// written next to the original before each modifying write.
    #[arg(long)]
    pub apply: bool,
    /// Override the config-file path. Auto-detected per client when
    /// absent (e.g. `~/.claude/settings.json` for Claude Code).
    #[arg(long)]
    pub config_file: Option<PathBuf>,
}

/// Transport for the MCP server.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum TransportKind {
    /// Stdio — what `claude mcp add` uses.
    Stdio,
    /// Streamable HTTP — for HTTP clients and `mcp-inspector`.
    Http,
}

/// Arguments for `serve`.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Transport to expose the MCP server on.
    #[arg(long, value_enum, default_value_t = TransportKind::Stdio)]
    pub transport: TransportKind,
    /// Bind address for `--transport http` (default: from config).
    #[arg(long)]
    pub bind: Option<String>,
    /// Skip the filesystem watcher; useful for transient debugging.
    #[arg(long)]
    pub no_watcher: bool,
    /// Workspace name (auto-created).
    #[arg(long, default_value_t = crate::config::DEFAULT_WORKSPACE.to_string())]
    pub workspace: String,
    /// Project name within the workspace (auto-created).
    #[arg(long, default_value_t = crate::config::DEFAULT_PROJECT.to_string())]
    pub project: String,
    /// Mount the read-only wiki browser at /web. Off by default. When
    /// enabled, anyone who can reach the MCP endpoint can also browse
    /// the wiki and the read-only frontend API under /api/v1 — keep
    /// the bind loopback-only or front with a reverse proxy that
    /// handles its own auth if you expose it.
    #[arg(long)]
    pub enable_web: bool,
    /// Serve this static directory at /web instead of the built-in UI.
    ///
    /// The read-only /api/v1 frontend API is still mounted when
    /// --enable-web is set.
    #[arg(long)]
    pub web_ui_dir: Option<PathBuf>,
    /// Run the HTTP transport in stateful (session) mode: the server
    /// issues an `Mcp-Session-Id` on `initialize` and requires it on
    /// every later request, with SSE-framed responses. Off by default —
    /// the HTTP transport is stateless and returns plain JSON, so
    /// stateless clients (OpenCode `type: "remote"`, `curl`) work without
    /// an `mcp-remote` stdio shim (issue #3). Enable this only for
    /// clients that need session continuity or server-initiated SSE
    /// streams. No effect on `--transport stdio`.
    #[arg(long)]
    pub http_stateful: bool,
}

/// Arguments for `write-page`.
#[derive(Debug, Args)]
pub struct WritePageArgs {
    /// Relative wiki path (e.g. `notes/foo.md`).
    #[arg(long, visible_alias = "p")]
    pub path: String,
    /// Markdown body. Use `-` to read from stdin.
    #[arg(long, visible_alias = "b")]
    pub body: String,
    /// Optional page title; otherwise derived from the first `# heading`
    /// in the body, or the path stem.
    #[arg(long)]
    pub title: Option<String>,
    /// Semantic kind: fact | rule | decision | gotcha (stored in frontmatter)
    #[arg(long)]
    pub kind: Option<String>,
    /// Repeatable tag to add to the frontmatter `tags` array.
    #[arg(long, short = 't')]
    pub tag: Vec<String>,
    /// Tier (`working`, `episodic`, `semantic`, `procedural`).
    #[arg(long, default_value = "semantic")]
    pub tier: String,
    /// Pin the page so the future decay sweep skips it.
    #[arg(long)]
    pub pinned: bool,
    /// Workspace name (auto-created if absent).
    #[arg(long, default_value_t = crate::config::DEFAULT_WORKSPACE.to_string())]
    pub workspace: String,
    /// Project name within the workspace (auto-created if absent).
    #[arg(long, default_value_t = crate::config::DEFAULT_PROJECT.to_string())]
    pub project: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn pi_mcp_client_aliases_parse_to_same_variant() {
        for alias in ["pi", "omp", "oh-my-pi"] {
            let cli = Cli::try_parse_from([
                "ai-memory",
                "install-mcp",
                "--client",
                alias,
                "--server-url",
                "http://example.test:49374/mcp",
            ])
            .unwrap_or_else(|e| panic!("failed to parse install-mcp alias {alias}: {e}"));

            let Command::InstallMcp(args) = cli.command else {
                panic!("expected install-mcp command for alias {alias}");
            };
            assert!(
                matches!(args.client, McpClient::Pi),
                "alias {alias} must resolve to the Pi/OMP MCP client"
            );
        }
    }

    #[test]
    fn pi_hook_agent_aliases_parse_to_same_variant() {
        for alias in ["omp", "pi", "oh-my-pi"] {
            let cli = Cli::try_parse_from([
                "ai-memory",
                "install-hooks",
                "--agent",
                alias,
                "--server-url",
                "http://example.test:49374",
            ])
            .unwrap_or_else(|e| panic!("failed to parse install-hooks alias {alias}: {e}"));

            let Command::InstallHooks(args) = cli.command else {
                panic!("expected install-hooks command for alias {alias}");
            };
            assert!(
                matches!(args.agent, AgentChoice::Omp),
                "alias {alias} must resolve to the OMP hook agent"
            );
        }
    }

    #[test]
    fn antigravity_aliases_parse_to_same_variant() {
        for alias in ["antigravity-cli", "antigravity", "agy"] {
            let mcp_cli = Cli::try_parse_from([
                "ai-memory",
                "install-mcp",
                "--client",
                alias,
                "--server-url",
                "http://example.test:49374/mcp",
            ])
            .unwrap_or_else(|e| panic!("failed to parse install-mcp alias {alias}: {e}"));
            let Command::InstallMcp(mcp_args) = mcp_cli.command else {
                panic!("expected install-mcp command for alias {alias}");
            };
            assert!(matches!(mcp_args.client, McpClient::AntigravityCli));

            let hook_cli = Cli::try_parse_from([
                "ai-memory",
                "install-hooks",
                "--agent",
                alias,
                "--server-url",
                "http://example.test:49374",
            ])
            .unwrap_or_else(|e| panic!("failed to parse install-hooks alias {alias}: {e}"));
            let Command::InstallHooks(hook_args) = hook_cli.command else {
                panic!("expected install-hooks command for alias {alias}");
            };
            assert!(matches!(hook_args.agent, AgentChoice::AntigravityCli));
        }
    }

    #[test]
    fn auth_login_openai_oauth_parses() {
        let cli = Cli::try_parse_from([
            "ai-memory",
            "auth",
            "login",
            "openai-oauth",
            "--timeout-secs",
            "30",
        ])
        .unwrap();

        let Command::Auth(args) = cli.command else {
            panic!("expected auth command");
        };
        let AuthCommand::Login(login) = args.command else {
            panic!("expected auth login command");
        };
        assert!(matches!(login.provider, AuthProviderChoice::OpenaiOauth));
        assert_eq!(login.timeout_secs, 30);
        assert!(login.github_token.is_none());
        assert!(login.client_id.is_none());
    }

    #[test]
    fn auth_login_copilot_parses_token_and_client_override() {
        let cli = Cli::try_parse_from([
            "ai-memory",
            "auth",
            "login",
            "copilot",
            "--github-token",
            "ghu-test",
            "--client-id",
            "Iv1.test",
        ])
        .unwrap();

        let Command::Auth(args) = cli.command else {
            panic!("expected auth command");
        };
        let AuthCommand::Login(login) = args.command else {
            panic!("expected auth login command");
        };
        assert!(matches!(login.provider, AuthProviderChoice::Copilot));
        assert_eq!(login.github_token.as_deref(), Some("ghu-test"));
        assert_eq!(login.client_id.as_deref(), Some("Iv1.test"));
    }

    #[test]
    fn llm_test_anthropic_oauth_parses() {
        let cli = Cli::try_parse_from([
            "ai-memory",
            "llm-test",
            "--provider",
            "anthropic-oauth",
            "--model",
            "claude-sonnet-4-6",
            "--prompt",
            "ping",
        ])
        .unwrap();

        let Command::LlmTest(args) = cli.command else {
            panic!("expected llm-test command");
        };
        assert!(matches!(args.provider, LlmProviderChoice::AnthropicOauth));
        assert_eq!(args.model, "claude-sonnet-4-6");
        assert_eq!(args.prompt, "ping");
    }
}
