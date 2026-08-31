//! Command-line interface definition (clap derive).

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use clap_complete::Shell;

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
    /// Launch an agent in an opt-in, cross-harness managed workstream.
    /// Native arguments are forwarded except exact wrapper flags such as
    /// `--yolo` and `--fresh`.
    Run(RunArgs),
    /// Pick a local project and installed harness, then launch from that
    /// checkout. Removes the `cd` step `run` requires.
    Show(ShowArgs),
    /// Resume the most recently launched managed checkout from anywhere,
    /// without `cd` and without picking from a list.
    Continue(ContinueArgs),
    /// List open cross-agent handoffs so a stale one can be cancelled by id.
    Handoffs(HandoffsArgs),
    /// List recent managed workstreams selectable from the current checkout.
    Workstreams(WorkstreamsArgs),
    /// Search the complete visible event ledger for a managed workstream.
    WorkstreamSearch(WorkstreamSearchArgs),
    /// Audit the store for likely cross-project contamination (read-only,
    /// SQL-only). Flags sessions whose cwd resolves to a different project and
    /// observations whose project disagrees with their session.
    AuditContamination(AuditContaminationArgs),
    /// Full-text search the wiki via FTS5.
    Search(SearchArgs),
    /// Fetch and display the full body of a wiki page.
    /// Accepts either `--path` (exact path) or a positional query that
    /// searches FTS5 and fetches the top matching page.
    ReadPage(ReadPageArgs),
    /// Write or update a wiki page atomically (also indexes it in the store).
    WritePage(WritePageArgs),
    /// Delete a single wiki page. The server routes scope resolution
    /// through `resolve_ws_proj`, so a delete targeting a project that
    /// exists in multiple workspaces never silently lands in the wrong
    /// slot (the MCP `memory_delete_page` had that gap until this build).
    DeletePage(DeletePageArgs),
    /// Run the MCP server (with watcher) over stdio or HTTP.
    Serve(ServeArgs),
    /// Wipe the data directory's wiki/, db/, raw/ contents.
    Reset(ResetArgs),
    /// Snapshot wiki/, db/, and config.toml into a gzipped tarball.
    Backup(BackupArgs),
    /// Restore a backup tarball into the data directory.
    Restore(RestoreArgs),
    /// Rebuild the SQLite index from the wiki/ markdown (the "DB is
    /// rebuildable from files" guarantee). Recreates workspaces/projects from
    /// each scope's `_meta.md` manifest and reindexes every page. Run with the
    /// server stopped, against a freshly-migrated (clean) data dir.
    Reindex(ReindexArgs),
    /// Print (or apply) lifecycle-hook configuration for an agent CLI.
    InstallHooks(InstallHooksArgs),
    /// Emit a single lifecycle hook natively (reads the event payload
    /// from stdin), avoiding a shell spawn. Used by the WindowsNative
    /// hook config; mirrors hooks/<agent>/<event>.sh.
    Hook(HookArgs),
    /// Hidden hook spool drainer used by native session-end hooks.
    #[command(hide = true, name = "hook-drain")]
    HookDrain(HookDrainArgs),
    /// Print MCP server registration snippets for any supported client.
    /// Current values are listed under `--client`; see docs/mcp-install.md
    /// for the full guide.
    InstallMcp(InstallMcpArgs),
    /// Internal stdio-to-HTTP MCP bridge for session-aware Claude Code installs.
    #[command(hide = true)]
    McpBridge(McpBridgeArgs),
    /// Stage + commit the wiki tree under git.
    Commit(CommitArgs),
    /// List recent wiki git checkpoints for recovery.
    Checkpoints(CheckpointsArgs),
    /// Restore a single wiki page from a git checkpoint and reindex it.
    RestorePage(RestorePageArgs),
    /// Smoke-test an LLM provider by sending one prompt.
    LlmTest(LlmTestArgs),
    /// Run the M8 retention sweep over episodic pages.
    ForgetSweep(ForgetSweepArgs),
    /// Run the M8 lint pass (stale / duplicates + optional LLM contradiction).
    Lint(LintArgs),
    /// Run the rule-based curator report.
    Curator(CuratorArgs),
    /// Run a read-only auto-improvement telemetry report.
    AutoImproveReport(AutoImproveReportArgs),
    /// Run auto-improvement for one completed session.
    AutoImprove(AutoImproveArgs),
    /// Manually finalize the latest open session for one agent in this project.
    FinalizeSession(FinalizeSessionArgs),
    /// Review, approve, or reject staged auto-improvement proposals.
    PendingWrites(PendingWritesArgs),
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
    /// Install the ai-memory usage snippet and managed Agent Skills into the
    /// project (or any markdown file / skill root you specify).
    /// Idempotent — bracketed by `<!-- ai-memory:start -->` /
    /// `<!-- ai-memory:end -->` markers so re-running replaces the
    /// block in place without duplicating.
    InstallInstructions(InstallInstructionsArgs),
    /// Install core-managed ai-memory Agent Skills into agent skill directories.
    InstallSkills(InstallSkillsArgs),
    /// Retro-fit existing sessions + observations to per-cwd projects
    /// based on the cwd captured at session-start. Pages are marked
    /// `is_latest=false` (they were a multi-project mash-up) so the
    /// next consolidation can regenerate them per-project. Idempotent.
    Reorg(ReorgArgs),
    /// Delete a project: its pages, sessions, observations, handoffs,
    /// embeddings, managed workstreams and on-disk wiki files.
    ///
    /// Irreversible — requires `--confirm`. By default this is a logical
    /// delete: the rows are gone and nothing can reach them through the API
    /// or search, but their bytes remain in free pages of the database file
    /// until it is rewritten, as with any SQLite delete. `--compact`
    /// reclaims them; neither mode is forensic erasure, because the wiki git
    /// history and any earlier backup still hold the content.
    PurgeProject(PurgeProjectArgs),
    /// Permanently delete ONE session and everything derived from it.
    ///
    /// Removes the session row, its observations, the handoffs it authored,
    /// its derived wiki page (every version) and their embeddings — scoped
    /// strictly to the named workspace and project. Handoffs this session
    /// only *accepted* are left alone: their text belongs to the session that
    /// wrote them.
    PurgeSession(PurgeSessionArgs),
    /// Rename a project within its workspace. No files move on disk —
    /// the wiki is flat and pages are differentiated by project_id only.
    /// Useful after renaming the project's directory on disk so the hook
    /// router keeps writing into the same logical project.
    RenameProject(RenameProjectArgs),
    /// Move a project into another workspace. A fresh destination is a
    /// lossless TRUE MOVE (re-stamp workspace_id, keep project_id, rename the
    /// dir) — sessions/observations/handoffs and history all survive. A
    /// destination that already holds a same-named project MERGES via
    /// copy+purge (only durable pages migrate, source purged). Either way the
    /// operation is irreversible — requires `--confirm`.
    MoveProject(MoveProjectArgs),
    /// Move one session (or every session of a project) to another project,
    /// re-stamping its observations, handoffs, consolidation jobs and its
    /// `sessions/<id>.md` page in one transaction. Without `--confirm` it
    /// prints what would move (a real dry run, rolled back server-side).
    MoveSession(MoveSessionArgs),
    /// Remove ai-memory's wiring (hooks, MCP, instructions, and default-root
    /// managed skills) from all detected agents. Dry-run unless `--apply`.
    Uninstall(UninstallArgs),
    /// Manage optional upstream LLM provider authentication.
    Auth(AuthArgs),
    /// Manage registered users for multi-user attribution. Each user
    /// has a stable username, optional name + email, and one current
    /// token. All subcommands require the root bearer token; non-root
    /// requests get 403. Existing single-user installs unaffected —
    /// these endpoints 503 until `[auth].token_pepper` is set
    /// (auto-generated by `ai-memory init`).
    User(UserArgs),
    /// Print a shell-completion script to stdout. Generated from this
    /// binary's own command tree, so it never drifts from the real CLI
    /// surface. See `docs/shell-completions.md` for install paths.
    Completions(CompletionsArgs),
}

/// Arguments for `run`. Wrapper-owned flags must precede `harness`; the
/// trailing native argv is deliberately opaque to clap.
#[derive(Debug, Args)]
#[command(trailing_var_arg = true)]
pub struct RunArgs {
    /// Workspace containing the managed workstream. Defaults to the nearest
    /// `.ai-memory.toml` marker's `workspace`, else `default`.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Project override. Defaults to the current repository project.
    #[arg(long)]
    pub project: Option<String>,
    /// Select an existing named workstream instead of the current one.
    #[arg(long, conflicts_with = "new_workstream")]
    pub workstream: Option<String>,
    /// Create and select a fresh named workstream.
    #[arg(long = "new", conflicts_with = "workstream")]
    pub new_workstream: Option<String>,
    /// Override the native harness executable path.
    #[arg(long)]
    pub executable: Option<PathBuf>,
    /// Disable native permission prompts using the selected harness's
    /// equivalent dangerous-mode option.
    #[arg(long)]
    pub yolo: bool,
    /// Start a new native session in the selected workstream instead of
    /// resuming or adopting an existing harness session.
    #[arg(long)]
    pub fresh: bool,
    /// Agent harness to launch. When omitted, continue the newest managed or
    /// checkout-local session among the auto-detected harnesses.
    #[arg(value_enum)]
    pub harness: Option<RunHarnessChoice>,
    /// Native harness arguments, forwarded byte-for-byte and in order.
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    pub native_args: Vec<OsString>,
}

/// Harnesses supported by managed workstreams.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum RunHarnessChoice {
    /// Anthropic Claude Code (`claude`).
    #[value(alias = "claude-code")]
    Claude,
    /// OpenAI Codex CLI.
    Codex,
    /// OpenCode.
    #[value(name = "opencode", alias = "open-code")]
    OpenCode,
    /// Pi coding agent.
    Pi,
    /// Charmbracelet Crush.
    Crush,
    /// Oh My Pi.
    #[value(alias = "oh-my-pi")]
    Omp,
    /// Moonshot AI Kimi Code.
    #[value(name = "kimi", alias = "kimi-code", alias = "kimi-cli")]
    Kimi,
    /// Command Code CLI.
    #[value(
        name = "command-code",
        alias = "commandcode",
        alias = "cmdc",
        alias = "cmd"
    )]
    CommandCode,
    /// Amazon Kiro CLI (v2 engine).
    #[value(name = "kiro", alias = "kiro-cli")]
    Kiro,
    /// Grok Build CLI (xAI).
    #[value(alias = "grok-build")]
    Grok,
    /// Google Antigravity CLI (`agy`).
    #[value(name = "antigravity", alias = "antigravity-cli", alias = "agy")]
    Antigravity,
}

/// Arguments for `show`.
#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Only list local projects resolving to this workspace.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Use saved local checkout links only; skip the depth-1 directory scan.
    #[arg(long)]
    pub no_scan: bool,
    /// Print structured project and harness choices instead of opening menus.
    /// Required when stdin or stdout is not a terminal.
    #[arg(long)]
    pub json: bool,
    /// Disable native permission prompts using the selected harness's
    /// equivalent dangerous-mode option. Forwarded to `run`.
    #[arg(long)]
    pub yolo: bool,
    /// Start a new native session instead of resuming or adopting an existing
    /// harness session. Forwarded to `run`.
    #[arg(long)]
    pub fresh: bool,
    /// Native harness arguments, forwarded byte-for-byte and in order.
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    pub native_args: Vec<OsString>,
}

/// Arguments for `continue`.
///
/// Deliberately smaller than [`ShowArgs`]: `continue` always delegates to
/// `run`'s bare mode, which rejects native argv and `--executable` because
/// their meaning depends on a harness the user did not name.
#[derive(Debug, Args)]
pub struct ContinueArgs {
    /// Only consider checkouts resolving to this workspace.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Disable native permission prompts using the resolved harness's
    /// equivalent dangerous-mode option. Forwarded to `run`.
    #[arg(long)]
    pub yolo: bool,
    /// Start a new native session instead of resuming the linked one.
    /// Forwarded to `run`.
    #[arg(long)]
    pub fresh: bool,
}

/// Arguments for `workstreams`.
/// Arguments for `handoffs`.
#[derive(Debug, Args)]
pub struct HandoffsArgs {
    /// Workspace to inspect (defaults to the resolved scope).
    #[arg(long)]
    pub workspace: Option<String>,
    /// Project to inspect (defaults to the resolved scope).
    #[arg(long)]
    pub project: Option<String>,
    /// Maximum handoffs to list.
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u16).range(1..=500))]
    pub limit: u16,
    /// Emit JSON instead of the human listing.
    #[arg(long)]
    pub json: bool,
    /// Expire every open handoff in the scope instead of listing them.
    ///
    /// Unlike the automatic sweep this does NOT spare manual handoffs or ones
    /// from another directory — those exemptions are exactly what a leftover
    /// backlog is made of, so honouring them would clear nothing. Pair with
    /// `--older-than-days` to keep recent batons.
    ///
    /// This is a state change, not a delete: the summary and provenance
    /// survive and the handoff simply stops being consumable. Requires
    /// `--confirm`.
    #[arg(long)]
    pub expire_all: bool,
    /// With `--expire-all`, only expire handoffs at least this many days old.
    #[arg(long)]
    pub older_than_days: Option<u32>,
    /// REQUIRED by `--expire-all`.
    #[arg(long)]
    pub confirm: bool,
}

#[derive(Debug, Args)]
pub struct WorkstreamsArgs {
    /// Workspace containing the managed workstreams. Defaults to the nearest
    /// `.ai-memory.toml` marker's `workspace`, else `default`.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Project override. Defaults to the current repository project.
    #[arg(long)]
    pub project: Option<String>,
    /// Maximum workstreams to return, current first then newest activity.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u8).range(1..=100))]
    pub limit: u8,
    /// Emit JSON instead of readable rows.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `workstream-search`.
#[derive(Debug, Args)]
pub struct WorkstreamSearchArgs {
    /// Natural-language or FTS query. Omit to show the newest events.
    #[arg(default_value = "")]
    pub query: String,
    /// Workstream to search. Managed child processes receive this through the
    /// environment, so agents normally do not need to pass it.
    #[arg(long, env = "AI_MEMORY_WORKSTREAM_ID")]
    pub workstream_id: ai_memory_core::WorkstreamId,
    /// Maximum events to return.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u8).range(1..=100))]
    pub limit: u8,
    /// Emit JSON instead of readable event blocks.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `user`.
#[derive(Debug, Args)]
pub struct UserArgs {
    /// User-management action to run.
    #[command(subcommand)]
    pub command: UserCommand,
}

/// Arguments for `completions`.
#[derive(Debug, Args)]
pub struct CompletionsArgs {
    /// Shell to generate a completion script for.
    #[arg(value_enum)]
    pub shell: Shell,
}

/// Subcommands for `user`.
#[derive(Debug, Subcommand)]
pub enum UserCommand {
    /// Create a new user and issue their initial token. The token is
    /// printed to stdout exactly once — store it now; only its SHA-256
    /// digest is kept in the DB.
    Add(UserAddArgs),
    /// List every registered user. Tokens are NEVER surfaced (the row
    /// only stores the hash); the `expired` column shows whether the
    /// user's token currently authenticates.
    List(UserListArgs),
    /// Expire the user's current token. Auth requests with that token
    /// start returning 401 immediately. The user row stays put so
    /// historical attribution references survive.
    Expire(UserExpireArgs),
    /// Re-activate a previously-expired token. No-op when the user's
    /// token is already active.
    Revive(UserReviveArgs),
    /// Issue a fresh token, replacing the existing one. Implicitly
    /// revives an expired user — rotating a token makes it usable
    /// again. The new plaintext is printed exactly once.
    RotateToken(UserRotateTokenArgs),
}

/// Arguments for `user add`.
#[derive(Debug, Args)]
pub struct UserAddArgs {
    /// Stable username. Required. Validation rejects empty, internal
    /// whitespace, control chars, and the path/quoting separators
    /// `/ \ : ; , " ' \``. UTF-8 letters and emails-as-usernames
    /// (alice@home) are allowed.
    #[arg(long)]
    pub username: String,
    /// Display name (e.g. `"Alice Smith"`). Surfaced in the web UI +
    /// `/api/v1` responses alongside the username. Internal whitespace
    /// is fine — only the edges are trimmed.
    #[arg(long)]
    pub name: Option<String>,
    /// Email. Basic validation: exactly one `@`, both sides non-empty,
    /// no whitespace. Intranet-style addresses (`alice@home`) are
    /// accepted. Lowercased before storage; case-insensitive unique.
    #[arg(long)]
    pub email: Option<String>,
    /// Emit the response as JSON instead of human-readable text. The
    /// token field is included in either case.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `user list`.
#[derive(Debug, Args)]
pub struct UserListArgs {
    /// Emit the response as JSON instead of a human-readable table.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `user expire`.
#[derive(Debug, Args)]
pub struct UserExpireArgs {
    /// Username to expire.
    pub username: String,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub yes: bool,
}

/// Arguments for `user revive`.
#[derive(Debug, Args)]
pub struct UserReviveArgs {
    /// Username to revive.
    pub username: String,
}

/// Arguments for `user rotate-token`.
#[derive(Debug, Args)]
pub struct UserRotateTokenArgs {
    /// Username to rotate.
    pub username: String,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub yes: bool,
    /// Emit the response as JSON instead of human-readable text.
    #[arg(long)]
    pub json: bool,
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
    /// Generic OIDC device-authorization grant (e.g. Keycloak). Stores a
    /// per-developer token the lifecycle hooks use to authenticate to the
    /// ai-memory server, instead of a shared static `--auth-token`.
    OidcDevice,
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
    /// OAuth/OIDC public client id. Required for `oidc-device`; an optional
    /// override for `copilot` device auth.
    #[arg(long)]
    pub client_id: Option<String>,
    /// OIDC issuer URL for `oidc-device` login, e.g. a Keycloak realm
    /// (`https://keycloak.example.com/realms/ai-memory`). Endpoints are
    /// discovered from `<issuer>/.well-known/openid-configuration`.
    #[arg(long)]
    pub issuer: Option<String>,
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

/// Which concern `uninstall` should touch. Omitted = all four.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum UninstallOnly {
    Hooks,
    Mcp,
    Instructions,
    Skills,
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
    /// Limit to one concern. Omitted = hooks + mcp + instructions + skills.
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
    /// Profile to use for OMP extensions, which relocates the path to
    /// `~/.omp/profiles/<profile>/agent/extensions/`.
    #[arg(long)]
    pub profile: Option<String>,
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
    /// Purge even when a managed workstream under this project still holds a
    /// live run lease. Workstreams cascade out of the project row, so without
    /// this the purge refuses rather than deleting a running session's lease.
    #[arg(long)]
    pub force: bool,
    /// Workspace name. Defaults to the nearest `.ai-memory.toml` marker's
    /// `workspace`, else `default`.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Project name. When omitted, auto-derived from the basename of
    /// the current git repo root (or CWD if no git repo).
    #[arg(long)]
    pub project: Option<String>,
    /// REQUIRED for the purge to run. Without this flag the CLI errors
    /// out — purging is destructive and irreversible.
    #[arg(long)]
    pub confirm: bool,
    /// Also reclaim the freed bytes: rebuild the FTS indexes and VACUUM the
    /// database after the delete commits.
    ///
    /// Without this the purge is a logical delete — the project is gone from
    /// the API and from search, but its bytes stay in free pages of the
    /// database file until it is next rewritten, as with any SQLite delete.
    ///
    /// This rewrites the WHOLE database and needs free disk space of about
    /// its size, so it takes minutes on a large store. It also does NOT make
    /// the content forensically unrecoverable: the wiki git history and any
    /// backup taken before the purge still contain it.
    #[arg(long)]
    pub compact: bool,
}

/// Arguments for `purge-session`.
#[derive(Debug, Args)]
pub struct PurgeSessionArgs {
    /// Full session UUID, exactly as `sessions.id` stores it.
    #[arg(long)]
    pub session_id: String,
    /// Workspace name. Defaults to the nearest `.ai-memory.toml` marker's
    /// `workspace`, else `default`.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Project name. When omitted, auto-derived from the basename of
    /// the current git repo root (or CWD if no git repo).
    #[arg(long)]
    pub project: Option<String>,
    /// REQUIRED for the purge to run. Without this flag the CLI errors
    /// out — purging is destructive and irreversible.
    #[arg(long)]
    pub confirm: bool,
    /// Also reclaim the freed bytes: rebuild the affected FTS indexes and
    /// VACUUM the database after the delete commits.
    ///
    /// Without this the purge is a logical delete — the session is gone from
    /// the API and from search, but its bytes stay in free pages of the
    /// database file until it is next rewritten, as with any SQLite delete.
    ///
    /// This rewrites the WHOLE database and needs free disk space of about
    /// its size, so it takes minutes on a large store. It also does NOT make
    /// the content forensically unrecoverable: the wiki git history and any
    /// backup taken before the purge still contain it.
    #[arg(long)]
    pub compact: bool,
}

/// Arguments for `rename-project`.
#[derive(Debug, Args)]
pub struct RenameProjectArgs {
    /// Workspace name. Defaults to the nearest `.ai-memory.toml` marker's
    /// `workspace`, else `default`.
    #[arg(long)]
    pub workspace: Option<String>,
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

/// Arguments for `move-project`.
#[derive(Debug, Args)]
pub struct MoveProjectArgs {
    /// Source workspace. Defaults to the nearest `.ai-memory.toml` marker's
    /// `workspace`, else `default`. Only the source is marker-resolved;
    /// `--to-workspace` names the destination and stays literal.
    #[arg(long)]
    pub from_workspace: Option<String>,
    /// Project name to move. When omitted, auto-derived from the basename
    /// of the current git repo root (or CWD if no git repo).
    #[arg(long)]
    pub project: Option<String>,
    /// Destination workspace. Auto-created if it doesn't exist.
    #[arg(long)]
    pub to_workspace: String,
    /// REQUIRED — the move re-stamps (true-move) or copies+purges (merge) the
    /// source, both irreversible. Without this flag the CLI errors out.
    #[arg(long)]
    pub confirm: bool,
    /// Override the active-project guard. In a copy-purge merge this never
    /// overrides a live managed-workstream lease, because deleting that lease
    /// would strand the running agent's transcript.
    #[arg(long)]
    pub force: bool,
    /// Merge conflict policy (copy-purge path only): what to do when a source
    /// page's path already exists in the destination with different content.
    /// `block` (default) aborts and lists the conflicts; `overwrite` lets the
    /// source supersede the destination page; `duplicate` keeps both (source
    /// lands under a de-duplicated path).
    #[arg(long, value_parser = ["block", "overwrite", "duplicate"], default_value = "block")]
    pub on_conflict: String,
}

/// Arguments for `move-session`.
///
/// Exactly one of `<SESSION_ID>` and `--from-project` names what moves; the
/// `what` group makes clap's error name both when neither is given.
#[derive(Debug, Args)]
#[command(group = clap::ArgGroup::new("what").required(true).args(["session_id", "from_project"]))]
pub struct MoveSessionArgs {
    /// Session id (UUID) to move. Omit it and pass `--from-project` to move
    /// every session touching one project. A session already rooted in the
    /// destination is re-homed: its row stays and only its stray rows move.
    pub session_id: Option<ai_memory_core::SessionId>,
    /// Batch form: move every session touching this project (a session row
    /// here or observations stamped into it). Resolved like other commands
    /// (marker, else literal); the sessions move one at a time and the batch
    /// stops at the first refusal, reporting how far it got.
    #[arg(long)]
    pub from_project: Option<String>,
    /// Workspace of `--from-project`. Defaults to the nearest
    /// `.ai-memory.toml` marker's `workspace`, else `default`.
    #[arg(long, requires = "from_project")]
    pub from_workspace: Option<String>,
    /// Destination project name (literal, not marker-resolved).
    #[arg(long)]
    pub to: String,
    /// Destination workspace. Defaults to the source workspace.
    #[arg(long)]
    pub to_workspace: Option<String>,
    /// What happens to the session's `sessions/<id>.md` page: `move` carries
    /// the page and its history along (refused when the destination already
    /// has one at that path); `regenerate` retires it so the next
    /// consolidation writes a fresh page in the destination.
    #[arg(long, value_parser = ["move", "regenerate"], default_value = "move")]
    pub pages: String,
    /// REQUIRED to apply. Without it the command prints the dry-run summary
    /// and the exact command to re-run.
    #[arg(long)]
    pub confirm: bool,
    /// Skip the live-session guards (open session, pending consolidation job,
    /// active project of the hook router in the batch form).
    #[arg(long)]
    pub force: bool,
    /// Create the destination workspace/project when it does not exist yet.
    #[arg(long)]
    pub create: bool,
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
    /// Print the snippet to stdout instead of mutating files.
    /// The default IS mutation here (the print form is also
    /// available without this command — copy the block from the
    /// README). Pass `--print` to preview what would land in
    /// the file. This does not print skill payloads; use
    /// `install-skills --print` to preview managed Agent Skills.
    #[arg(long)]
    pub print: bool,
    /// Skip installing/updating the managed ai-memory Agent Skills.
    #[arg(long)]
    pub no_skills: bool,
    /// Scope for managed ai-memory skill installation.
    #[arg(long = "skills-scope", value_enum)]
    pub skills_scope: Option<InstallSkillsScope>,
    /// Agent skill directory family for managed ai-memory skill installation.
    #[arg(long = "skills-agent", value_enum)]
    pub skills_agent: Option<InstallSkillsAgent>,
    /// Override the managed skill root directory.
    #[arg(long = "skills-target-dir")]
    pub skills_target_dir: Option<PathBuf>,
    /// Overwrite same-named unmanaged skills while installing from `install-instructions`.
    #[arg(long = "skills-force")]
    pub skills_force: bool,
}

/// Skill install scope for `install-skills`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum InstallSkillsScope {
    /// Install into this project's agent skill directories.
    Project,
    /// Install into the user's global agent skill directories.
    Global,
}

/// Agent skill directory family for `install-skills`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum InstallSkillsAgent {
    /// Claude Code's `.claude/skills` directory.
    ClaudeCode,
    /// Cross-agent `.agents/skills` directory.
    Agents,
    /// Devin's `.devin/skills` directory.
    Devin,
    /// Grok Build CLI's `.grok/skills` directory.
    Grok,
    /// Install into both Claude Code and `.agents` skill directories.
    Both,
}

/// Arguments for `install-skills`.
#[derive(Debug, Args)]
pub struct InstallSkillsArgs {
    /// Install project-local skills or global user skills.
    #[arg(long, value_enum, default_value_t = InstallSkillsScope::Project)]
    pub scope: InstallSkillsScope,
    /// Which agent skill directory family to install into.
    #[arg(long, value_enum, default_value_t = InstallSkillsAgent::ClaudeCode)]
    pub agent: InstallSkillsAgent,
    /// Override the skill root directory. When set, `--scope` and
    /// `--agent` are ignored and the managed skill directories are
    /// written below this root.
    #[arg(long)]
    pub target_dir: Option<PathBuf>,
    /// Print target paths and SKILL.md contents without writing files.
    #[arg(long)]
    pub print: bool,
    /// Overwrite same-named existing skills that do not contain the
    /// ai-memory managed marker. Without this flag, unmanaged skills
    /// are preserved and the command exits with an actionable error.
    #[arg(long)]
    pub force: bool,
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
    /// Workspace name. Defaults to the nearest `.ai-memory.toml` marker's
    /// `workspace`, else `default` — the same resolution the lifecycle hooks
    /// use, so bootstrap pages land where the session captures do.
    #[arg(long)]
    pub workspace: Option<String>,
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

/// Arguments for `audit-contamination`.
#[derive(Debug, Args)]
pub struct AuditContaminationArgs {
    /// Restrict the audit to one workspace (use together with `--project`).
    #[arg(long)]
    pub workspace: Option<String>,
    /// Restrict the audit to one project (use together with `--workspace`).
    #[arg(long)]
    pub project: Option<String>,
    /// Emit the report as JSON instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `search`.
#[derive(Debug, Args)]
pub struct SearchArgs {
    /// FTS5 query string (e.g. `"karpathy wiki"` or `quick OR slow`).
    pub query: String,
    /// Workspace name. Defaults to the nearest `.ai-memory.toml` marker's
    /// `workspace`, else `default`.
    #[arg(long)]
    pub workspace: Option<String>,
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

/// Arguments for `read-page`.
#[derive(Debug, Args)]
pub struct ReadPageArgs {
    /// FTS5 query to find the page (searches and fetches the top hit).
    /// Ignored when `--path` is provided.
    pub query: Option<String>,
    /// Exact wiki path (e.g. `notes/foo.md`). Takes precedence over `query`.
    #[arg(long)]
    pub path: Option<String>,
    /// Workspace name. Defaults to the nearest `.ai-memory.toml` marker's
    /// `workspace`, else `default`.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Project name. When omitted, auto-derived from the current project.
    #[arg(long)]
    pub project: Option<String>,
    /// Emit the page as JSON (includes frontmatter).
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `delete-page`.
#[derive(Debug, Args)]
pub struct DeletePageArgs {
    /// Exact wiki path to delete (e.g. `notes/foo.md`).
    #[arg(long)]
    pub path: String,
    /// Workspace name. Defaults to the nearest `.ai-memory.toml` marker's
    /// `workspace`, else `default`. Resolution is announced on stderr so a
    /// cross-workspace project-name collision can never silently route the
    /// delete to the wrong slot.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Project name. When omitted, auto-derived from the current project
    /// (same heuristic write-page/read-page use).
    #[arg(long)]
    pub project: Option<String>,
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

/// Arguments for `checkpoints`.
#[derive(Debug, Args)]
pub struct CheckpointsArgs {
    /// Maximum number of checkpoints to list.
    #[arg(short = 'n', long, default_value_t = 20)]
    pub limit: usize,
    /// Emit checkpoints as JSON instead of human-readable rows.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `restore-page`.
#[derive(Debug, Args)]
pub struct RestorePageArgs {
    /// Exact wiki path to restore (e.g. `notes/foo.md`).
    #[arg(long)]
    pub path: String,
    /// Git checkpoint/revision to restore from.
    #[arg(long)]
    pub from: String,
    /// Workspace name. Defaults to the nearest `.ai-memory.toml` marker's
    /// `workspace`, else `default`.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Project name. When omitted, auto-derived from the current project.
    #[arg(long)]
    pub project: Option<String>,
    /// Emit the server response as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `reindex`.
#[derive(Debug, Args)]
pub struct ReindexArgs {}

/// Agent CLI to install hooks/extensions for. For MCP-only clients
/// (Claude Desktop), use `install-mcp --client <name>` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
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
    /// Real Pi coding agent. The generated TypeScript extension provides
    /// lifecycle capture and bridges ai-memory's HTTP MCP tools into Pi.
    Pi,
    /// Oh My Pi (`omp`) — TypeScript extension
    /// under `~/.omp/agent/extensions/`. `--apply` writes the extension
    /// file directly; restart `omp` for it to load.
    #[value(alias = "oh-my-pi")]
    Omp,
    /// OpenClaw personal AI gateway — native plugin package with
    /// session/tool/compaction hooks.
    Openclaw,
    /// Google Antigravity CLI (`agy`) — JSON-config hooks in
    /// `~/.gemini/config/hooks.json`.
    #[value(alias = "antigravity", alias = "agy")]
    AntigravityCli,
    /// xAI Grok Build CLI — JSON-config hooks in
    /// `~/.grok/hooks/ai-memory.json`. Native `ai-memory hook --event`
    /// integration using Grok-specific hook scripts. NOTE: Grok ignores
    /// hook stdout on `SessionStart`, so
    /// capture works but handoff injection does not — recover the prior
    /// session's handoff via the MCP `memory_handoff_accept` tool.
    Grok,
    /// Zero coding agent (Gitlawb/zero) — JSON-config lifecycle hooks in
    /// `$XDG_CONFIG_HOME/zero/hooks.json` (exec-form `command` + `args`,
    /// so ai-memory's native `hook` command runs with no shell). NOTE:
    /// Zero discards `sessionStart` hook stdout, so capture works but
    /// handoff injection does not — recover the prior session's handoff
    /// via the MCP `memory_handoff_accept` tool.
    Zero,
    /// Devin CLI — JSON-config hooks in
    /// `~/.devin/hooks.v1.json` or `~/.devin/config.json` hooks key.
    /// Native `ai-memory hook --event` integration using Devin-specific
    /// hook scripts. Devin consumes the handoff via
    /// `hookSpecificOutput.additionalContext` on `SessionStart`.
    Devin,
    /// Kimi Code CLI (Moonshot AI).
    #[value(alias = "kimi")]
    KimiCode,
    /// Kiro CLI (AWS), v2 agent engine — camelCase lifecycle hooks embedded
    /// in agent configs under `~/.kiro/agents/*.json`.
    #[value(alias = "kiro")]
    KiroCli,
    /// Kiro CLI (AWS), v3 agent engine — PascalCase lifecycle hooks in the
    /// standalone `$KIRO_HOME/hooks/ai-memory.json` registration file.
    #[value(alias = "kiro-v3")]
    KiroCliV3,
    /// Command Code CLI — stable JSON-config shell hooks in
    /// `~/.commandcode/settings.json`.
    #[value(alias = "commandcode", alias = "cmdc", alias = "cmd")]
    CommandCode,
    /// Pool (Poolside Agent CLI, `pool`) — project-scoped YAML hooks in
    /// the repo-root `.poolside/settings.yaml`. ai-memory stages the hook
    /// scripts and prints a ready-to-paste `hooks:` snippet; it does not
    /// write project-local files. NOTE: Pool's `SessionStart` stdout
    /// injection is not demonstrated, so capture works but handoff
    /// injection does not — recover the prior session's handoff via the
    /// MCP `memory_handoff_accept` tool, and close sessions with
    /// `ai-memory finalize-session --agent pool` (Pool has no true
    /// session-end event).
    #[value(alias = "poolside")]
    Pool,
    /// ZCode (z.ai) — JSON-config lifecycle hooks in the root `hooks` block
    /// of `~/.zcode/cli/config.json` (the same file `install-mcp --client
    /// zcode` registers MCP servers in). Entries are exec-form
    /// `type: "process"` (`command` + `args`, no shell), so ai-memory's
    /// native `hook` command runs directly. ZCode injects `SessionStart`
    /// stdout as model context (`hookSpecificOutput.additionalContext`,
    /// verified live against engine v0.16.5), so handoff delivery works.
    /// `Stop` fires at the end of every turn and there is no `SessionEnd`,
    /// so close sessions with `ai-memory finalize-session --agent zcode`.
    #[value(alias = "zai")]
    Zcode,
}

impl AgentChoice {
    /// The core [`AgentKind`] this CLI choice selects — the single mapping
    /// point between the clap surface and the domain enum. Wire strings,
    /// hook-bundle directory names, and session attribution all derive from
    /// the returned kind's `as_str()`; adding an agent means extending this
    /// match once instead of hunting per-command copies.
    #[must_use]
    pub const fn kind(self) -> ai_memory_core::AgentKind {
        use ai_memory_core::AgentKind;
        match self {
            Self::ClaudeCode => AgentKind::ClaudeCode,
            Self::Codex => AgentKind::Codex,
            Self::Cursor => AgentKind::Cursor,
            Self::GeminiCli => AgentKind::GeminiCli,
            Self::OpenCode => AgentKind::OpenCode,
            Self::Pi => AgentKind::Pi,
            Self::Omp => AgentKind::Omp,
            Self::Openclaw => AgentKind::OpenClaw,
            Self::AntigravityCli => AgentKind::AntigravityCli,
            Self::Grok => AgentKind::Grok,
            Self::Zero => AgentKind::Zero,
            Self::Devin => AgentKind::Devin,
            Self::KimiCode => AgentKind::KimiCode,
            Self::KiroCli | Self::KiroCliV3 => AgentKind::KiroCli,
            Self::CommandCode => AgentKind::CommandCode,
            Self::Pool => AgentKind::Pool,
            Self::Zcode => AgentKind::Zcode,
        }
    }

    /// `hooks/<subdir>` bundle name for agents that install script hooks.
    /// `None` for agents wired through a generated integration (plugin /
    /// extension / exec-form native commands) instead of a script
    /// directory. The subdir equals the kind's wire string for every
    /// script agent.
    #[must_use]
    pub const fn script_hook_subdir(self) -> Option<&'static str> {
        match self {
            Self::OpenCode | Self::Pi | Self::Omp | Self::Openclaw | Self::Zero | Self::Zcode => {
                None
            }
            _ => Some(self.kind().as_str()),
        }
    }
}

/// Arguments for `finalize-session`.
#[derive(Debug, Args)]
pub struct FinalizeSessionArgs {
    /// Agent kind to finalize. Defaults to Codex for backward compatibility;
    /// Codex, Antigravity CLI, Pool, and ZCode have no reliable true
    /// SessionEnd hook.
    #[arg(long, value_enum, default_value_t = AgentChoice::Codex)]
    pub agent: AgentChoice,
    /// Workspace name. Defaults to the nearest `.ai-memory.toml` marker's
    /// `workspace`, else `default`.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Project name. When omitted, auto-derived from the current project.
    #[arg(long)]
    pub project: Option<String>,
    /// Finalize sessions belonging to OTHER operators too.
    ///
    /// By default only your own (and unattributed) sessions are considered, so
    /// finalizing cannot end a colleague's live session. Use this to recover a
    /// teammate's session that died without emitting SessionEnd.
    #[arg(long, default_value_t = false)]
    pub all_owners: bool,
    /// Finalize every matching open session instead of just the latest one.
    #[arg(long, conflicts_with = "session_id")]
    pub all: bool,
    /// Finalize exactly this session id instead of "the latest open one"
    /// (still subject to `--all-owners`). Use this when other sessions for
    /// the same agent may be open concurrently in the same project (e.g.
    /// multiple terminal tabs each running Kiro CLI against one repo) —
    /// picking "latest" in that case risks closing out the wrong
    /// (still-active) session.
    #[arg(long, conflicts_with = "all")]
    pub session_id: Option<ai_memory_core::SessionId>,
    /// Emit a JSON summary.
    #[arg(long)]
    pub json: bool,
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
    /// Real Pi coding agent. Uses ai-memory's generated bridge extension
    /// because Pi has no native MCP config.
    Pi,
    /// Oh My Pi (`omp`) — `~/.omp/agent/mcp.json`.
    #[value(alias = "oh-my-pi")]
    Omp,
    /// Google Antigravity CLI (`agy`) — `~/.gemini/config/mcp_config.json`.
    #[value(alias = "antigravity", alias = "agy")]
    AntigravityCli,
    /// Zero coding agent (Gitlawb/zero) — `~/.config/zero/config.json`,
    /// `mcp.servers` map with native HTTP transport + bearer headers.
    Zero,
    /// ZCode (z.ai) — `~/.zcode/cli/config.json`, nested `mcp.servers`
    /// map with `type: "http"` + `url` + optional `headers`. ZCode's
    /// entry schema is strict: unknown keys make it drop the server
    /// silently, so the generated entry carries nothing else.
    Zcode,
    /// Devin CLI — `~/.devin/config.json`.
    Devin,
    /// xAI Grok Build CLI — `~/.grok/config.toml` under
    /// `[mcp_servers.<name>]` with native HTTP `url` + `headers`.
    /// Pair with `install-hooks --agent grok` for lifecycle capture.
    /// Grok ignores SessionStart stdout, so handoffs are recovered via
    /// MCP `memory_handoff_accept` rather than hook injection.
    Grok,
    /// Kimi Code CLI (Moonshot AI).
    #[value(alias = "kimi")]
    KimiCode,
    /// Kiro CLI - `$KIRO_HOME/settings/mcp.json` (default
    /// `~/.kiro/settings/mcp.json`). Pair with
    /// `install-hooks --agent kiro-cli` for verified v2 lifecycle capture.
    #[value(alias = "kiro")]
    KiroCli,
    /// Command Code CLI — `~/.commandcode/mcp.json`.
    #[value(alias = "commandcode", alias = "cmdc", alias = "cmd")]
    CommandCode,
    /// Swival CLI — project-scoped `.swival/mcp.json` using native HTTP.
    /// This integration is MCP-only; Swival's lifecycle callback does not
    /// expose a stable session identifier for reliable capture correlation.
    Swival,
    /// VS Code GitHub Copilot (agent mode) — per-workspace
    /// `.vscode/mcp.json`. Copilot's agent mode reads MCP servers
    /// from VS Code's own MCP framework (top-level `servers` key),
    /// so the same JSON file works for any MCP-capable VS Code
    /// extension, not just Copilot. Default scope is the current
    /// workspace; pass `--config-file ~/path/to/mcp.json` to target
    /// the user-level config instead.
    ///
    /// The hook surface (PreToolUse/PostToolUse/SessionStart) does
    /// not yet exist in VS Code Copilot — this is MCP-only by
    /// design. See `install-mcp --client vscode-copilot`.
    #[value(name = "vscode-copilot", alias = "copilot", alias = "github-copilot")]
    VsCodeCopilot,
    /// Zed editor - user-level `settings.json` under the platform config
    /// directory. Zed reads remote MCP servers from the top-level
    /// `context_servers` map. This integration is MCP-only because Zed
    /// does not expose ai-memory-compatible lifecycle hooks.
    Zed,
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
    /// OpenCode Zen/Go cloud API.
    Opencode,
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
    #[arg(long)]
    pub workspace: Option<String>,
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
    #[arg(long)]
    pub workspace: Option<String>,
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
    #[arg(long)]
    pub workspace: Option<String>,
    /// Project name. When omitted, auto-derived from the basename of
    /// the current git repo root (or CWD if no git repo).
    #[arg(long)]
    pub project: Option<String>,
}

/// Arguments for `curator`.
#[derive(Debug, Args)]
pub struct CuratorArgs {
    /// Return a report without staging a pending write. Default when no mode flag is set.
    #[arg(long)]
    pub dry_run: bool,
    /// Stage one pending curator report page for approval.
    #[arg(long)]
    pub stage: bool,
    /// Workspace name. Defaults to the nearest `.ai-memory.toml` marker's
    /// `workspace`, else `default`.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Project name. When omitted, auto-derived from the basename of
    /// the current git repo root (or CWD if no git repo).
    #[arg(long)]
    pub project: Option<String>,
    /// Emit only machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `auto-improve-report`.
#[derive(Debug, Args)]
pub struct AutoImproveReportArgs {
    /// Workspace name. Defaults to the nearest `.ai-memory.toml` marker's
    /// `workspace`, else `default`.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Project name. When omitted, auto-derived from the basename of
    /// the current git repo root (or CWD if no git repo).
    #[arg(long)]
    pub project: Option<String>,
    /// Lookback window in days.
    #[arg(long, default_value_t = ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_TELEMETRY_SINCE_DAYS)]
    pub days: u32,
    /// Maximum rows in each top-N count table.
    #[arg(long, default_value_t = ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_TELEMETRY_TOP_LIMIT)]
    pub limit: usize,
    /// Stage one pending telemetry report page for later approval.
    #[arg(long)]
    pub stage: bool,
    /// Emit only machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `auto-improve`.
#[derive(Debug, Args)]
pub struct AutoImproveArgs {
    /// Completed session UUID to review.
    #[arg(long)]
    pub session_id: String,
    /// Workspace name. Defaults to the nearest `.ai-memory.toml` marker's
    /// `workspace`, else `default`.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Project name. When omitted, auto-derived from the basename of
    /// the current git repo root (or CWD if no git repo).
    #[arg(long)]
    pub project: Option<String>,
    /// Override `[auto_improve].min_observations` for this run.
    #[arg(long)]
    pub min_observations: Option<usize>,
    /// Override `[auto_improve].min_session_duration_secs` for this run.
    #[arg(long)]
    pub min_session_duration_secs: Option<u64>,
    /// Override `[auto_improve].min_confidence` for this run.
    #[arg(long)]
    pub min_confidence: Option<f32>,
    /// Override `[auto_improve].max_input_tokens` for this run.
    #[arg(long)]
    pub max_input_tokens: Option<usize>,
    /// Override `[auto_improve].max_proposals_per_run` for this run.
    #[arg(long)]
    pub max_proposals: Option<usize>,
    /// Include raw fallback context when the reviewer supports it.
    #[arg(long)]
    pub include_raw_fallback: bool,
    /// Emit only the machine-readable JSON report.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct PendingWritesArgs {
    #[command(subcommand)]
    pub command: PendingWritesCommand,
}

#[derive(Debug, Subcommand)]
pub enum PendingWritesCommand {
    List(PendingWritesListArgs),
    Show(PendingWriteIdArgs),
    Diff(PendingWriteIdArgs),
    Approve(PendingWriteIdArgs),
    Reject(PendingWriteRejectArgs),
}

#[derive(Debug, Args)]
pub struct PendingWritesListArgs {
    #[arg(long)]
    pub workspace: Option<String>,
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long)]
    pub status: Option<String>,
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct PendingWriteIdArgs {
    pub id: String,
    #[arg(long)]
    pub workspace: Option<String>,
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct PendingWriteRejectArgs {
    pub id: String,
    #[arg(long)]
    pub workspace: Option<String>,
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long, default_value = "rejected by reviewer")]
    pub reason: String,
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `llm-test`.
#[derive(Debug, Args)]
pub struct LlmTestArgs {
    /// Provider to test.
    #[arg(long, value_enum)]
    pub provider: LlmProviderChoice,
    /// Model identifier (e.g. `claude-haiku-4-5`, `gpt-5.4-mini`, `llama3.1:8b`).
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

/// Project-resolution strategy to bake into installed hooks.
///
/// `basename` (the default) bakes nothing — generated hooks behave
/// exactly as before. `repo-root` bakes a default so every session
/// resolves its project from the main git repo root (collapsing
/// subdirectories and worktrees) without a per-repo `.ai-memory.toml`
/// marker. A marker's own `project_strategy` still wins.
/// Which way capture fails when a repository has no `.ai-memory.toml`
/// marker (#446).
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum CaptureModeArg {
    /// Capture unless a marker excludes it — the historical default.
    Denylist,
    /// Capture only repositories that carry a marker. A repository without
    /// one emits no lifecycle events at all.
    Allowlist,
}

impl CaptureModeArg {
    /// The policy value this flag selects.
    #[must_use]
    pub const fn mode(self) -> ai_memory_hooks::CaptureMode {
        match self {
            Self::Denylist => ai_memory_hooks::CaptureMode::Denylist,
            Self::Allowlist => ai_memory_hooks::CaptureMode::Allowlist,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ProjectStrategyArg {
    /// `project = basename(cwd)` — the default; bakes nothing.
    Basename,
    /// `project = basename(main git repo root)` — collapses subdirs/worktrees.
    /// clap renders this value as `repo-root`.
    RepoRoot,
}

impl ProjectStrategyArg {
    /// Normalize to what the hook command should bake. `None` bakes
    /// nothing (behavior unchanged); `Some("repo-root")` bakes the
    /// repo-root default into the generated hooks.
    #[must_use]
    pub fn baked(self) -> Option<&'static str> {
        match self {
            Self::Basename => None,
            Self::RepoRoot => Some("repo-root"),
        }
    }
}

/// Arguments for `hook` — emit one lifecycle event natively.
#[derive(Debug, Args)]
pub struct HookArgs {
    /// Lifecycle event, e.g. `pre-tool-use`, `session-start`.
    #[arg(long)]
    pub event: String,
    /// Agent name to attribute the event to, e.g. `claude-code`.
    #[arg(long)]
    pub agent: String,
    /// Base URL of the ai-memory hook server.
    #[arg(long)]
    pub server_url: String,
    /// Optional bearer token (`Authorization: Bearer <token>`).
    #[arg(long, hide_env_values = true)]
    pub auth_token: Option<String>,
    /// Default project strategy baked in by `install-hooks
    /// --project-strategy`. Applies only when a `.ai-memory.toml`
    /// marker does not pin its own `project_strategy`.
    #[arg(long, value_enum)]
    pub project_strategy: Option<ProjectStrategyArg>,
    /// Inspect capture policy without spooling, draining, or contacting the server.
    #[arg(long)]
    pub check_capture: bool,
    /// Capture failure mode baked in by `install-hooks --capture-mode`.
    /// Under `allowlist`, a repository with no `.ai-memory.toml` marker emits
    /// no lifecycle event at all — the event is dropped before it can reach
    /// the local spool or the wire.
    #[arg(long, value_enum)]
    pub capture_mode: Option<CaptureModeArg>,
    /// Opt in to assistant/Stop capture: on a Claude Code `stop` event, attach a
    /// sanitized, capped excerpt of the assistant's final turn as the Stop body.
    /// Baked onto the native `stop` command by
    /// `install-hooks --capture-assistant`; the server must also enable
    /// `capture_assistant`. No effect on other events (#196).
    #[arg(long)]
    pub capture_assistant: bool,
}

/// Arguments for hidden `hook-drain`.
#[derive(Debug, Args)]
pub struct HookDrainArgs {}

/// Arguments for `install-hooks`.
#[derive(Debug, Args)]
pub struct InstallHooksArgs {
    /// Which agent's hooks to render.
    #[arg(long, value_enum, default_value_t = AgentChoice::ClaudeCode)]
    pub agent: AgentChoice,
    /// Filesystem root that contains the vendored hook scripts (defaults
    /// to the repo's `hooks/` if known, else `/usr/local/share/ai-memory/hooks`).
    /// Ignored for generated TypeScript integrations (OpenCode, OMP, Pi,
    /// OpenClaw).
    #[arg(long)]
    pub hooks_dir: Option<PathBuf>,
    /// Server URL the hooks will POST to. Defaults to the configured
    /// `server_url` / AI_MEMORY_SERVER_URL when set, else loopback. If neither
    /// is configured, apply-mode also reuses an existing ai-memory MCP entry
    /// for the same agent when one is present.
    // Keep this optional so effective_hook_server_url can distinguish an
    // omitted flag from an explicit URL that equals the compiled default.
    #[arg(long)]
    pub server_url: Option<String>,
    /// Bearer token to embed in the hook config's `env` block. When
    /// set, every hook call carries `Authorization: Bearer <token>`,
    /// matching what the server requires when AI_MEMORY_AUTH_TOKEN
    /// is set there. Generate one with `ai-memory generate-auth-token`.
    #[arg(long, hide_env_values = true)]
    pub auth_token: Option<String>,
    /// Stamp the installed hooks with a registered username for operator
    /// visibility. This flag is metadata: the server resolves attribution
    /// from the token passed via `--auth-token`. Recommended workflow:
    ///
    /// ```bash
    /// # 1. create the user (prints the token once)
    /// ai-memory user add --username alice --email alice@home
    ///
    /// # 2. wire alice's token into the agent's hooks
    /// ai-memory install-hooks --apply --agent claude-code \
    ///     --as-user alice --auth-token <alice-token>
    /// ```
    ///
    /// Without this flag, the installer omits the username label;
    /// attribution still resolves from the bearer token at request time.
    #[arg(long)]
    pub as_user: Option<String>,
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
    /// Default project strategy to bake into the installed hooks.
    /// `repo-root` makes every session resolve its project from the main
    /// git repo root (collapsing subdirectories and worktrees) without a
    /// per-repo `.ai-memory.toml` marker. A marker's own `project_strategy`
    /// still wins. `basename` bakes nothing and is identical to prior
    /// behavior. Omitting the flag leaves it unset: an `--apply` re-run then
    /// preserves whatever strategy an earlier `--apply` baked, so a bare
    /// re-apply (e.g. the auto-refresh in `ai-memory upgrade`) does not
    /// silently revert `repo-root` back to `basename`.
    #[arg(long, value_enum)]
    pub project_strategy: Option<ProjectStrategyArg>,
    /// Bake `--capture-assistant` onto the installed native `stop` command so a
    /// Claude Code `stop` event carries a sanitized excerpt of the assistant's
    /// final turn (#196). Only valid for `--agent claude-code` on a native
    /// platform; the server must also set `capture_assistant = true`. Re-running
    /// without this flag removes it (idempotent). Default off.
    #[arg(long)]
    pub capture_assistant: bool,
    /// Persist the capture failure mode for this install (#446). Under
    /// `allowlist`, a repository with no `.ai-memory.toml` marker emits no
    /// lifecycle event at all, for every agent. Stored in the data dir, so a
    /// later bare `--apply` — including the auto-refresh inside `upgrade` —
    /// cannot regenerate it away. Omitting the flag leaves the stored mode
    /// untouched; `--capture-mode denylist` restores the default.
    #[arg(long, value_enum)]
    pub capture_mode: Option<CaptureModeArg>,
    /// Do not install Claude Code's `UserPromptSubmit` capture hook. Prompt
    /// text is then excluded before it can enter the local spool or wire.
    /// A bare `--apply` re-run preserves an existing opt-out; use
    /// `--capture-prompts` to enable prompt capture again explicitly.
    #[arg(long, conflicts_with = "capture_prompts")]
    pub no_capture_prompts: bool,
    /// Explicitly enable Claude Code prompt capture after an earlier
    /// `--no-capture-prompts` install. Only valid for Claude Code.
    #[arg(long, conflicts_with = "no_capture_prompts")]
    pub capture_prompts: bool,
    /// Profile to use for OMP extensions, which relocates the path to
    /// `~/.omp/profiles/<profile>/agent/extensions/`.
    #[arg(long)]
    pub profile: Option<String>,
}

/// Arguments for `install-mcp`.
#[derive(Debug, Clone, Args)]
pub struct InstallMcpArgs {
    /// Which MCP client to render configuration for.
    #[arg(long, value_enum, default_value_t = McpClient::ClaudeCode)]
    pub client: McpClient,
    /// Server URL the client should connect to — either the base URL
    /// (`https://host:49374`, the same value `install-hooks --server-url`
    /// takes) or the full MCP endpoint (`…/mcp`); a missing `/mcp` suffix
    /// is appended automatically. Defaults to the configured
    /// `server_url` / AI_MEMORY_SERVER_URL plus `/mcp` when set, else
    /// loopback.
    // Keep this optional for the same explicit-URL precedence as hook install.
    #[arg(long)]
    pub server_url: Option<String>,
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
    /// absent (e.g. `~/.claude.json` for Claude Code).
    #[arg(long)]
    pub config_file: Option<PathBuf>,
    /// For Claude Code, register an ai-memory stdio bridge that forwards the
    /// current lifecycle session id to the HTTP server. This enables
    /// `[auto_scope] mode = "per_session"` for concurrent Claude Code sessions.
    #[arg(long)]
    pub session_aware: bool,
}

/// Arguments for the internal Claude Code session-aware MCP bridge.
#[derive(Debug, Clone, Args)]
pub struct McpBridgeArgs {
    /// Remote ai-memory base URL or full `/mcp` endpoint.
    #[arg(long)]
    pub server_url: Option<String>,
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
    /// DANGEROUS: allow unauthenticated plain HTTP on a non-loopback bind.
    ///
    /// HTTP-only. Unnecessary for loopback binds; prefer AI_MEMORY_AUTH_TOKEN
    /// or a loopback bind instead.
    #[arg(long)]
    pub allow_insecure_no_auth: bool,
    /// Skip the filesystem watcher; useful for transient debugging.
    #[arg(long)]
    pub no_watcher: bool,
    /// Workspace name (auto-created).
    ///
    /// Not marker-aware, unlike the client commands: the server has no
    /// caller cwd to walk up from, and this is the baked fallback for hook
    /// events that arrive without a usable one.
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
    /// Base path the whole HTTP surface is served under. Empty (default)
    /// keeps every route at the host root — byte-identical to previous
    /// behaviour. Set e.g. `/wiki` to host ai-memory under a URL subpath
    /// behind a reverse proxy that preserves the prefix; then `/mcp`,
    /// `/api/v1`, `/hook` and the web UI all live under it (`/wiki/mcp`,
    /// `/wiki/api/v1`, …). The value is normalised to `/<core>` (leading
    /// slash, no trailing); `/` and `` both mean root.
    #[arg(long, env = "AI_MEMORY_BASE_PATH", default_value = "")]
    pub base_path: String,
    /// Slug the web UI is mounted at, WITHIN `--base-path`. Default `/web`
    /// (the read-only `/api/v1` API always stays at `<base>/api/v1`). Set
    /// `/` to serve the UI at the base root itself (e.g. `/wiki` instead of
    /// `/wiki/web`). The server injects a normalised `<base href>` into the
    /// served HTML so the built-in UI and a custom `--web-ui-dir` SPA both
    /// resolve their assets under the prefix without a rebuild.
    #[arg(long, env = "AI_MEMORY_WEB_SLUG", default_value = "/web")]
    pub web_slug: String,
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
    /// Origin allowed to call /api/v1 cross-origin (CORS). Repeat the flag
    /// for multiple origins, or set AI_MEMORY_CORS_ALLOW_ORIGINS to a
    /// comma-separated list. Must be a fully-qualified origin
    /// (e.g. https://app.example.com); `*` is rejected (CORS spec forbids
    /// credentials + wildcard). Empty = same-origin only.
    #[arg(long = "cors-allow-origin")]
    pub cors_allow_origin: Vec<String>,
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
    #[arg(long)]
    pub workspace: Option<String>,
    /// Project name within the workspace. When omitted, auto-detect from the
    /// current project using the same resolver as read-page/search.
    #[arg(long)]
    pub project: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};
    use std::collections::BTreeSet;

    #[test]
    fn serve_parses_insecure_no_auth_override() {
        let parsed = Cli::try_parse_from([
            "ai-memory",
            "serve",
            "--transport",
            "http",
            "--allow-insecure-no-auth",
        ])
        .expect("serve override parses");
        let Command::Serve(args) = parsed.command else {
            panic!("expected serve command");
        };
        assert!(args.allow_insecure_no_auth);
    }

    #[test]
    fn finalize_session_parses_typed_id_and_rejects_ambiguous_selection() {
        let session_id = ai_memory_core::SessionId::new();
        let parsed = Cli::try_parse_from([
            "ai-memory",
            "finalize-session",
            "--session-id",
            &session_id.to_string(),
        ])
        .expect("valid session id parses");
        let Command::FinalizeSession(args) = parsed.command else {
            panic!("expected finalize-session command");
        };
        assert_eq!(args.session_id, Some(session_id));

        assert!(
            Cli::try_parse_from([
                "ai-memory",
                "finalize-session",
                "--session-id",
                "not-a-uuid",
            ])
            .is_err(),
            "malformed session ids must fail at the CLI boundary"
        );
        assert!(
            Cli::try_parse_from([
                "ai-memory",
                "finalize-session",
                "--all",
                "--session-id",
                &session_id.to_string(),
            ])
            .is_err(),
            "--all and --session-id must be mutually exclusive"
        );
    }

    #[test]
    fn architecture_lists_every_visible_cli_subcommand() {
        let architecture = include_str!("../../../docs/ARCHITECTURE.md");
        let cli_section = architecture
            .split_once("## CLI subcommand surface")
            .expect("architecture must have a CLI subcommand section")
            .1;
        let command_block = cli_section
            .split_once("```")
            .expect("CLI subcommand section must have a fenced block")
            .1
            .split_once("```")
            .expect("CLI subcommand fence must be closed")
            .0;
        let documented = command_block.split_whitespace().collect::<BTreeSet<_>>();
        let command = Cli::command();
        let visible = command
            .get_subcommands()
            .filter(|subcommand| !subcommand.is_hide_set())
            .map(|subcommand| subcommand.get_name())
            .collect::<BTreeSet<_>>();

        assert_eq!(
            documented, visible,
            "docs/ARCHITECTURE.md CLI subcommands must match `ai-memory --help`"
        );
    }

    /// `continue` always delegates to `run`'s bare mode, which refuses native
    /// argv and `--executable`. Rejecting them at parse time keeps that
    /// contract visible in `--help` instead of failing after the launch has
    /// already started resolving a workstream.
    #[test]
    fn continue_takes_wrapper_flags_only() {
        let parsed =
            Cli::try_parse_from(["ai-memory", "continue", "--workspace", "work", "--yolo"])
                .expect("continue parses wrapper flags");
        let Command::Continue(args) = parsed.command else {
            panic!("expected continue command");
        };
        assert_eq!(args.workspace.as_deref(), Some("work"));
        assert!(args.yolo);
        assert!(!args.fresh);

        for rejected in [
            vec!["ai-memory", "continue", "claude"],
            vec!["ai-memory", "continue", "--model", "opus"],
            vec!["ai-memory", "continue", "--executable", "/bin/claude"],
        ] {
            assert!(
                Cli::try_parse_from(&rejected).is_err(),
                "{rejected:?} must not parse"
            );
        }
    }

    #[test]
    fn show_parses_listing_and_launch_flags_without_consuming_native_args() {
        let listing = Cli::try_parse_from(["ai-memory", "show", "--json", "--no-scan"])
            .expect("show listing parses");
        let Command::Show(listing) = listing.command else {
            panic!("expected show command");
        };
        assert!(listing.json);
        assert!(listing.no_scan);

        let launch = Cli::try_parse_from([
            "ai-memory",
            "show",
            "--workspace",
            "team",
            "--yolo",
            "--fresh",
            "--model",
            "fast",
        ])
        .expect("show launch parses");
        let Command::Show(launch) = launch.command else {
            panic!("expected show command");
        };
        assert_eq!(launch.workspace.as_deref(), Some("team"));
        assert!(launch.yolo);
        assert!(launch.fresh);
        assert_eq!(launch.native_args, ["--model", "fast"]);
    }

    #[test]
    fn claude_session_aware_mcp_flag_parses() {
        let cli = Cli::try_parse_from([
            "ai-memory",
            "install-mcp",
            "--client",
            "claude-code",
            "--session-aware",
            "--apply",
        ])
        .unwrap();

        let Command::InstallMcp(args) = cli.command else {
            panic!("expected install-mcp command");
        };
        assert!(args.session_aware);
        assert!(args.apply);
        assert!(matches!(args.client, McpClient::ClaudeCode));
    }

    #[test]
    fn pi_and_omp_mcp_clients_parse_to_distinct_variants() {
        for (alias, expected_pi) in [("pi", true), ("omp", false), ("oh-my-pi", false)] {
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
                matches!(args.client, McpClient::Pi) == expected_pi,
                "alias {alias} resolved to unexpected MCP client: {:?}",
                args.client
            );
        }
    }

    #[test]
    fn write_page_project_is_optional_for_shared_resolution() {
        let cli = Cli::try_parse_from([
            "ai-memory",
            "write-page",
            "--path",
            "notes/x.md",
            "--body",
            "hello",
        ])
        .expect("write-page parses without --project");

        let Command::WritePage(args) = cli.command else {
            panic!("expected write-page command");
        };
        assert_eq!(args.project, None);

        let cli = Cli::try_parse_from([
            "ai-memory",
            "write-page",
            "--path",
            "notes/x.md",
            "--body",
            "hello",
            "--project",
            "explicit",
        ])
        .expect("write-page parses with --project");

        let Command::WritePage(args) = cli.command else {
            panic!("expected write-page command");
        };
        assert_eq!(args.project.as_deref(), Some("explicit"));
    }

    #[test]
    fn auto_improve_parses_required_session() {
        let cli = Cli::try_parse_from([
            "ai-memory",
            "auto-improve",
            "--session-id",
            "00000000-0000-0000-0000-000000000000",
            "--project",
            "scratch",
            "--max-proposals",
            "2",
        ])
        .expect("auto-improve parses");

        let Command::AutoImprove(args) = cli.command else {
            panic!("expected auto-improve command");
        };
        assert_eq!(args.session_id, "00000000-0000-0000-0000-000000000000");
        assert_eq!(args.max_proposals, Some(2));
        assert_eq!(args.project.as_deref(), Some("scratch"));
    }

    #[test]
    fn curator_parses_default_dry_run_and_mode_flags() {
        let cli = Cli::try_parse_from(["ai-memory", "curator", "--project", "scratch"])
            .expect("curator parses without mode flags");
        let Command::Curator(args) = cli.command else {
            panic!("expected curator command");
        };
        assert!(!args.dry_run);
        assert!(!args.stage);
        assert_eq!(args.project.as_deref(), Some("scratch"));

        let cli = Cli::try_parse_from([
            "ai-memory",
            "curator",
            "--dry-run",
            "--workspace",
            "default",
        ])
        .expect("curator dry-run parses");
        let Command::Curator(args) = cli.command else {
            panic!("expected curator command");
        };
        assert!(args.dry_run);
        assert!(!args.stage);

        let cli =
            Cli::try_parse_from(["ai-memory", "curator", "--stage"]).expect("curator stage parses");
        let Command::Curator(args) = cli.command else {
            panic!("expected curator command");
        };
        assert!(args.stage);
        assert!(!args.dry_run);
    }

    #[test]
    fn auto_improve_report_parses_scope_window_limit_and_json() {
        let cli = Cli::try_parse_from([
            "ai-memory",
            "auto-improve-report",
            "--project",
            "scratch",
            "--days",
            "14",
            "--limit",
            "3",
            "--stage",
            "--json",
        ])
        .expect("auto-improve-report parses");

        let Command::AutoImproveReport(args) = cli.command else {
            panic!("expected auto-improve-report command");
        };
        assert_eq!(args.project.as_deref(), Some("scratch"));
        assert_eq!(args.days, 14);
        assert_eq!(args.limit, 3);
        assert!(args.stage);
        assert!(args.json);
    }

    #[test]
    fn pending_writes_subcommands_parse() {
        let id = "00000000-0000-0000-0000-000000000000";
        let list = Cli::try_parse_from([
            "ai-memory",
            "pending-writes",
            "list",
            "--project",
            "scratch",
            "--status",
            "pending",
            "--limit",
            "5",
        ])
        .expect("pending-writes list parses");
        let Command::PendingWrites(args) = list.command else {
            panic!("expected pending-writes command");
        };
        let PendingWritesCommand::List(args) = args.command else {
            panic!("expected list subcommand");
        };
        assert_eq!(args.project.as_deref(), Some("scratch"));
        assert_eq!(args.status.as_deref(), Some("pending"));
        assert_eq!(args.limit, 5);

        for subcommand in ["show", "diff", "approve"] {
            let cli = Cli::try_parse_from([
                "ai-memory",
                "pending-writes",
                subcommand,
                id,
                "--project",
                "scratch",
            ])
            .unwrap_or_else(|e| panic!("pending-writes {subcommand} parses: {e}"));
            let Command::PendingWrites(args) = cli.command else {
                panic!("expected pending-writes command");
            };
            match args.command {
                PendingWritesCommand::Show(args)
                | PendingWritesCommand::Diff(args)
                | PendingWritesCommand::Approve(args) => {
                    assert_eq!(args.id, id);
                    assert_eq!(args.project.as_deref(), Some("scratch"));
                }
                PendingWritesCommand::List(_) | PendingWritesCommand::Reject(_) => {
                    panic!("wrong subcommand parsed")
                }
            }
        }

        let reject = Cli::try_parse_from([
            "ai-memory",
            "pending-writes",
            "reject",
            id,
            "--project",
            "scratch",
            "--reason",
            "not now",
        ])
        .expect("pending-writes reject parses");
        let Command::PendingWrites(args) = reject.command else {
            panic!("expected pending-writes command");
        };
        let PendingWritesCommand::Reject(args) = args.command else {
            panic!("expected reject subcommand");
        };
        assert_eq!(args.id, id);
        assert_eq!(args.reason, "not now");
    }

    #[test]
    fn pi_and_omp_hook_agents_parse_to_distinct_variants() {
        for (alias, expected_pi) in [("pi", true), ("omp", false), ("oh-my-pi", false)] {
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
                matches!(args.agent, AgentChoice::Pi) == expected_pi,
                "alias {alias} resolved to unexpected hook agent: {:?}",
                args.agent
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
    fn grok_hook_agent_parses() {
        let hook_cli = Cli::try_parse_from([
            "ai-memory",
            "install-hooks",
            "--agent",
            "grok",
            "--server-url",
            "http://example.test:49374",
        ])
        .unwrap_or_else(|e| panic!("failed to parse install-hooks --agent grok: {e}"));
        let Command::InstallHooks(hook_args) = hook_cli.command else {
            panic!("expected install-hooks command for grok");
        };
        assert!(matches!(hook_args.agent, AgentChoice::Grok));
    }

    #[test]
    fn grok_mcp_client_parses() {
        let mcp_cli = Cli::try_parse_from([
            "ai-memory",
            "install-mcp",
            "--client",
            "grok",
            "--server-url",
            "http://example.test:49374",
        ])
        .unwrap_or_else(|e| panic!("failed to parse install-mcp --client grok: {e}"));
        let Command::InstallMcp(mcp_args) = mcp_cli.command else {
            panic!("expected install-mcp command for grok");
        };
        assert!(matches!(mcp_args.client, McpClient::Grok));
    }

    #[test]
    fn kiro_mcp_aliases_parse() {
        for alias in ["kiro-cli", "kiro"] {
            let cli = Cli::try_parse_from([
                "ai-memory",
                "install-mcp",
                "--client",
                alias,
                "--server-url",
                "https://memory.example/mcp",
            ])
            .unwrap_or_else(|e| panic!("failed to parse Kiro MCP alias {alias}: {e}"));
            let Command::InstallMcp(args) = cli.command else {
                panic!("expected install-mcp command for Kiro alias {alias}");
            };
            assert!(matches!(args.client, McpClient::KiroCli));
        }
    }

    #[test]
    fn devin_hook_agent_parses() {
        let hook_cli = Cli::try_parse_from([
            "ai-memory",
            "install-hooks",
            "--agent",
            "devin",
            "--server-url",
            "http://example.test:49374",
        ])
        .unwrap_or_else(|e| panic!("failed to parse install-hooks --agent devin: {e}"));
        let Command::InstallHooks(hook_args) = hook_cli.command else {
            panic!("expected install-hooks command for devin");
        };
        assert!(matches!(hook_args.agent, AgentChoice::Devin));
    }

    #[test]
    fn kiro_hook_engine_aliases_parse_explicitly() {
        for alias in ["kiro-cli", "kiro"] {
            let cli = Cli::try_parse_from([
                "ai-memory",
                "install-hooks",
                "--agent",
                alias,
                "--server-url",
                "http://127.0.0.1:49374",
            ])
            .unwrap_or_else(|error| panic!("failed to parse Kiro v2 alias {alias}: {error}"));
            let Command::InstallHooks(args) = cli.command else {
                panic!("expected install-hooks for Kiro v2 alias {alias}");
            };
            assert_eq!(args.agent, AgentChoice::KiroCli);
        }
        for alias in ["kiro-cli-v3", "kiro-v3"] {
            let cli = Cli::try_parse_from([
                "ai-memory",
                "install-hooks",
                "--agent",
                alias,
                "--server-url",
                "http://127.0.0.1:49374",
            ])
            .unwrap_or_else(|error| panic!("failed to parse Kiro v3 alias {alias}: {error}"));
            let Command::InstallHooks(args) = cli.command else {
                panic!("expected install-hooks for Kiro v3 alias {alias}");
            };
            assert_eq!(args.agent, AgentChoice::KiroCliV3);
            assert_eq!(args.agent.kind(), ai_memory_core::AgentKind::KiroCli);
        }
    }

    #[test]
    fn pool_hook_and_finalize_aliases_parse() {
        for alias in ["pool", "poolside"] {
            let cli = Cli::try_parse_from([
                "ai-memory",
                "install-hooks",
                "--agent",
                alias,
                "--server-url",
                "http://127.0.0.1:49374",
            ])
            .unwrap_or_else(|error| panic!("failed to parse Pool alias {alias}: {error}"));
            let Command::InstallHooks(args) = cli.command else {
                panic!("expected install-hooks for Pool alias {alias}");
            };
            assert_eq!(args.agent, AgentChoice::Pool);
            assert_eq!(args.agent.kind(), ai_memory_core::AgentKind::Pool);
            assert_eq!(args.agent.script_hook_subdir(), Some("pool"));
        }
        let cli = Cli::try_parse_from(["ai-memory", "finalize-session", "--agent", "pool"])
            .expect("failed to parse finalize-session --agent pool");
        let Command::FinalizeSession(args) = cli.command else {
            panic!("expected finalize-session for pool");
        };
        assert_eq!(args.agent, AgentChoice::Pool);
    }

    #[test]
    fn zcode_hook_and_finalize_aliases_parse() {
        for alias in ["zcode", "zai"] {
            let cli = Cli::try_parse_from([
                "ai-memory",
                "install-hooks",
                "--agent",
                alias,
                "--server-url",
                "http://127.0.0.1:49374",
            ])
            .unwrap_or_else(|error| panic!("failed to parse ZCode alias {alias}: {error}"));
            let Command::InstallHooks(args) = cli.command else {
                panic!("expected install-hooks for ZCode alias {alias}");
            };
            assert_eq!(args.agent, AgentChoice::Zcode);
            assert_eq!(args.agent.kind(), ai_memory_core::AgentKind::Zcode);
            // Native exec-form integration: no script bundle to stage.
            assert_eq!(args.agent.script_hook_subdir(), None);
        }
        let cli = Cli::try_parse_from(["ai-memory", "finalize-session", "--agent", "zcode"])
            .expect("failed to parse finalize-session --agent zcode");
        let Command::FinalizeSession(args) = cli.command else {
            panic!("expected finalize-session for zcode");
        };
        assert_eq!(args.agent, AgentChoice::Zcode);
    }

    #[test]
    fn command_code_mcp_and_hook_aliases_parse() {
        for alias in ["command-code", "commandcode", "cmdc", "cmd"] {
            let mcp = Cli::try_parse_from([
                "ai-memory",
                "install-mcp",
                "--client",
                alias,
                "--server-url",
                "http://memory.example:49374",
            ])
            .unwrap_or_else(|error| panic!("failed to parse MCP alias {alias}: {error}"));
            let Command::InstallMcp(args) = mcp.command else {
                panic!("expected install-mcp for {alias}");
            };
            assert_eq!(args.client, McpClient::CommandCode);

            let hooks = Cli::try_parse_from([
                "ai-memory",
                "install-hooks",
                "--agent",
                alias,
                "--server-url",
                "http://memory.example:49374",
            ])
            .unwrap_or_else(|error| panic!("failed to parse hook alias {alias}: {error}"));
            let Command::InstallHooks(args) = hooks.command else {
                panic!("expected install-hooks for {alias}");
            };
            assert_eq!(args.agent, AgentChoice::CommandCode);
        }
    }

    #[test]
    fn swival_mcp_client_parses() {
        let cli = Cli::try_parse_from([
            "ai-memory",
            "install-mcp",
            "--client",
            "swival",
            "--server-url",
            "http://memory.example:49374",
        ])
        .unwrap_or_else(|error| panic!("failed to parse MCP client swival: {error}"));
        let Command::InstallMcp(args) = cli.command else {
            panic!("expected install-mcp for swival");
        };
        assert_eq!(args.client, McpClient::Swival);
    }

    #[test]
    fn zcode_mcp_client_parses() {
        let cli = Cli::try_parse_from([
            "ai-memory",
            "install-mcp",
            "--client",
            "zcode",
            "--server-url",
            "http://memory.example:49374",
        ])
        .unwrap_or_else(|error| panic!("failed to parse MCP client zcode: {error}"));
        let Command::InstallMcp(args) = cli.command else {
            panic!("expected install-mcp for zcode");
        };
        assert_eq!(args.client, McpClient::Zcode);
    }

    #[test]
    fn install_hooks_project_strategy_repo_root_parses() {
        let cli = Cli::try_parse_from([
            "ai-memory",
            "install-hooks",
            "--agent",
            "claude-code",
            "--project-strategy",
            "repo-root",
        ])
        .unwrap_or_else(|e| panic!("failed to parse --project-strategy repo-root: {e}"));
        let Command::InstallHooks(args) = cli.command else {
            panic!("expected install-hooks command");
        };
        assert!(matches!(
            args.project_strategy,
            Some(ProjectStrategyArg::RepoRoot)
        ));
        assert_eq!(
            args.project_strategy.and_then(ProjectStrategyArg::baked),
            Some("repo-root")
        );
    }

    #[test]
    fn install_hooks_prompt_capture_flags_parse_and_conflict() {
        let disabled = Cli::try_parse_from([
            "ai-memory",
            "install-hooks",
            "--agent",
            "claude-code",
            "--no-capture-prompts",
        ])
        .expect("--no-capture-prompts parses");
        let Command::InstallHooks(disabled) = disabled.command else {
            panic!("expected install-hooks command");
        };
        assert!(disabled.no_capture_prompts);
        assert!(!disabled.capture_prompts);

        let enabled = Cli::try_parse_from([
            "ai-memory",
            "install-hooks",
            "--agent",
            "claude-code",
            "--capture-prompts",
        ])
        .expect("--capture-prompts parses");
        let Command::InstallHooks(enabled) = enabled.command else {
            panic!("expected install-hooks command");
        };
        assert!(!enabled.no_capture_prompts);
        assert!(enabled.capture_prompts);

        assert!(
            Cli::try_parse_from([
                "ai-memory",
                "install-hooks",
                "--no-capture-prompts",
                "--capture-prompts",
            ])
            .is_err(),
            "the two prompt-capture choices must conflict"
        );
    }

    #[test]
    fn install_hooks_project_strategy_defaults_to_unset() {
        let cli = Cli::try_parse_from(["ai-memory", "install-hooks", "--agent", "claude-code"])
            .expect("install-hooks parses without --project-strategy");
        let Command::InstallHooks(args) = cli.command else {
            panic!("expected install-hooks command");
        };
        // No flag → None, so a re-apply preserves whatever is already baked.
        assert!(args.project_strategy.is_none());
        assert_eq!(
            args.project_strategy.and_then(ProjectStrategyArg::baked),
            None
        );
    }

    #[test]
    fn install_hooks_explicit_basename_still_parses() {
        let cli = Cli::try_parse_from([
            "ai-memory",
            "install-hooks",
            "--agent",
            "claude-code",
            "--project-strategy",
            "basename",
        ])
        .expect("install-hooks parses --project-strategy basename");
        let Command::InstallHooks(args) = cli.command else {
            panic!("expected install-hooks command");
        };
        // Explicit basename is distinct from "unset": it forces basename and
        // overrides an already-baked repo-root on re-apply.
        assert!(matches!(
            args.project_strategy,
            Some(ProjectStrategyArg::Basename)
        ));
        assert_eq!(
            args.project_strategy.and_then(ProjectStrategyArg::baked),
            None
        );
    }

    #[test]
    fn install_hooks_project_strategy_rejects_invalid_value() {
        let result =
            Cli::try_parse_from(["ai-memory", "install-hooks", "--project-strategy", "bogus"]);
        assert!(
            result.is_err(),
            "an unknown --project-strategy value must be rejected by value_enum"
        );
    }

    #[test]
    fn hook_project_strategy_rejects_invalid_value() {
        let result = Cli::try_parse_from([
            "ai-memory",
            "hook",
            "--event",
            "session-start",
            "--agent",
            "claude-code",
            "--server-url",
            "http://127.0.0.1:49374",
            "--project-strategy",
            "bogus",
        ]);
        assert!(
            result.is_err(),
            "an unknown hook --project-strategy value must be rejected by value_enum"
        );
    }

    #[test]
    fn vscode_copilot_aliases_parse_to_same_variant() {
        for alias in ["vscode-copilot", "copilot", "github-copilot"] {
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
                matches!(args.client, McpClient::VsCodeCopilot),
                "alias {alias} must resolve to the VS Code Copilot MCP client"
            );
        }
    }

    #[test]
    fn zed_mcp_client_parses() {
        let cli = Cli::try_parse_from([
            "ai-memory",
            "install-mcp",
            "--client",
            "zed",
            "--server-url",
            "http://example.test:49374/mcp",
        ])
        .unwrap();

        let Command::InstallMcp(args) = cli.command else {
            panic!("expected install-mcp command");
        };
        assert!(matches!(args.client, McpClient::Zed));
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

    #[test]
    fn completions_parses_every_supported_shell() {
        for (arg, expected) in [
            ("bash", Shell::Bash),
            ("elvish", Shell::Elvish),
            ("fish", Shell::Fish),
            ("powershell", Shell::PowerShell),
            ("zsh", Shell::Zsh),
        ] {
            let cli = Cli::try_parse_from(["ai-memory", "completions", arg]).unwrap();
            let Command::Completions(args) = cli.command else {
                panic!("expected completions command for {arg}");
            };
            assert_eq!(args.shell, expected);
        }
    }

    #[test]
    fn completions_rejects_an_unknown_shell() {
        assert!(Cli::try_parse_from(["ai-memory", "completions", "nushell"]).is_err());
    }

    #[test]
    fn completions_requires_a_shell() {
        assert!(Cli::try_parse_from(["ai-memory", "completions"]).is_err());
    }
}
