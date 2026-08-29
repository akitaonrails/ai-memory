//! `ai-memory serve` — MCP server with optional filesystem watcher.

use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ai_memory_consolidate::{
    AutoImproveReviewConfig, Consolidator, EmbedBackfillOptions, ObservationRetention,
    ScheduledAutoImproveSettings, run_auto_improve_scheduler_tick, run_embedding_backfill,
    run_lint, run_sweep_with_options,
};
use ai_memory_core::{ActiveProject, ProjectId, Sanitizer, WorkspaceId};
use ai_memory_hooks::{
    DEFAULT_HOOK_INGEST_MAX_IN_FLIGHT, HookState, ProjectCacheStore, WorkstreamState, hook_router,
    workstream_router,
};
use ai_memory_llm::{Embedder, LlmProvider, ProviderHealth, build_embedder, build_provider};
use ai_memory_mcp::{
    AdminState, AiMemoryServer, ScopeInvalidation, admin_router_with_sweep_tuning,
};
use ai_memory_store::{ReaderPool, Store, WriterHandle};
use ai_memory_web::{WebMountSpec, mount_web_router, normalize_prefix, web_base_href};
use ai_memory_wiki::{WatcherHandle, Wiki, migrations, run_wiki_migrations};
use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::cli::{ServeArgs, TransportKind};
use crate::config::{AuthSettings, AutoImproveSettings, Config, MaintenanceSettings};
use ai_memory_mcp::auth::{AuthState, require_bearer};

/// 10 MB cap on inbound HTTP bodies. The /hook ingress accepts the
/// agent's raw payload which can include a tool output excerpt
/// (capped at 2 KB on our side via `truncate_excerpt`), but Claude
/// Code et al. send the full envelope, which can run to a few KB.
/// 10 MB is generous headroom; without a cap, axum streams unbounded
/// bodies into memory (audit critical #2).
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// `POST /admin/bootstrap` may carry a large JSON array of sources even
/// after client-side prune; keep hooks/MCP at [`MAX_BODY_BYTES`].
const BOOTSTRAP_MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
/// Startup and failure retry delay, capped by each job's configured interval.
const MAINTENANCE_STARTUP_DELAY_CAP: Duration = Duration::from_secs(60);
/// How often the durable SessionEnd consolidation worker checks for work when
/// no hook notification arrives (including jobs recovered after restart).
const SESSION_CONSOLIDATION_POLL_INTERVAL: Duration = Duration::from_secs(15);
/// A claimed job may be recovered after this long without completion. Provider
/// requests are expected to finish well inside this lease.
const SESSION_CONSOLIDATION_LEASE: Duration = Duration::from_secs(10 * 60);

/// Validate the credentials that keep an existing multi-user installation
/// closed. Bootstrap installs have no user rows yet and retain their historical
/// compatibility behavior regardless of placeholder auth values.
fn validate_existing_users_auth(users_exist: bool, auth: &AuthSettings) -> Result<()> {
    if !users_exist {
        return Ok(());
    }

    let pepper_present = auth
        .token_pepper
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    let bearer_present = auth
        .bearer_token
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    match (pepper_present, bearer_present) {
        (true, true) => Ok(()),
        (false, false) => anyhow::bail!(
            "users exist but [auth].token_pepper and [auth].bearer_token are missing or blank; restore both original secrets from configuration backup before serving"
        ),
        (false, true) => anyhow::bail!(
            "users exist but [auth].token_pepper is missing or blank; restore the original pepper from configuration backup before serving"
        ),
        (true, false) => anyhow::bail!(
            "users exist but [auth].bearer_token is missing or blank; configure the original static root bearer token before serving"
        ),
    }
}

fn configured(value: Option<&String>) -> bool {
    value.is_some_and(|v| !v.trim().is_empty())
}

/// What the bound address tells us about network exposure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpExposure {
    /// Loopback-only, or authenticated. Nothing to warn about.
    Safe,
    /// Unauthenticated on a non-loopback address, allowed because the
    /// operator passed `--allow-insecure-no-auth`.
    InsecureByOverride,
    /// Unauthenticated on a non-loopback address inside a container, where
    /// the bind address is not evidence either way. See
    /// [`validate_http_exposure`].
    UndeterminedInContainer,
}

/// Detect that this process is running inside a container.
///
/// `/.dockerenv` is created by Docker, `/run/.containerenv` by Podman. The
/// official image also sets `AI_MEMORY_IN_CONTAINER`, so the signal survives
/// runtimes that create neither file.
fn running_in_container() -> bool {
    if std::env::var("AI_MEMORY_IN_CONTAINER").is_ok_and(|v| !v.trim().is_empty()) {
        return true;
    }
    Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists()
}

/// Refuse accidental unauthenticated network exposure after the listener has
/// selected its actual local address (which may differ from the bind input).
///
/// The check reads the bind address as evidence of reachability. That
/// inference holds on a host, but **not** inside a container: publishing a
/// port with `-p` requires binding `0.0.0.0` inside the namespace, and
/// whether that port reaches the network is decided by the host-side publish
/// spec — `-p 127.0.0.1:49374:49374` versus `-p 0.0.0.0:49374:49374` — which
/// the process cannot observe. Refusing there is a false positive that took
/// down the documented Quick Start container (#407), so containers get a
/// loud warning instead.
///
/// Note this is deliberately not backstopped by the `Host` allowlist: that
/// allowlist defends against DNS rebinding, where a browser sets the header.
/// A client that can route to the port sets `Host` freely, so it is not an
/// access control and cannot substitute for a token here.
fn validate_http_exposure(
    local_addr: SocketAddr,
    auth_enabled: bool,
    allow_insecure_no_auth: bool,
    containerized: bool,
) -> Result<HttpExposure> {
    if local_addr.ip().is_loopback() || auth_enabled {
        return Ok(HttpExposure::Safe);
    }
    if allow_insecure_no_auth {
        return Ok(HttpExposure::InsecureByOverride);
    }
    if containerized {
        return Ok(HttpExposure::UndeterminedInContainer);
    }

    anyhow::bail!(
        "refusing unauthenticated plain HTTP on non-loopback address {local_addr}: anyone on the network could access ai-memory. Configure AI_MEMORY_AUTH_TOKEN or bind to a loopback address. For intentional LAN use, pass --allow-insecure-no-auth explicitly and review TLS options in docs/https-via-proxy.md."
    );
}

/// Validate the trusted proxy's least-privilege credential and optional stable
/// root identity before binding the server.
fn validate_trusted_proxy_auth(auth: &AuthSettings) -> Result<()> {
    let proxy_enabled = configured(auth.actor_proxy_bearer_token.as_ref());
    if proxy_enabled && !configured(auth.bearer_token.as_ref()) {
        anyhow::bail!(
            "[auth].actor_proxy_bearer_token requires [auth].bearer_token so administration keeps a distinct root credential"
        );
    }
    if proxy_enabled
        && auth.actor_proxy_bearer_token.as_deref().map(str::trim)
            == auth.bearer_token.as_deref().map(str::trim)
    {
        anyhow::bail!(
            "[auth].actor_proxy_bearer_token must differ from [auth].bearer_token; sharing the root credential would let a missing proxy identity fall through as root"
        );
    }
    let root_issuer = configured(auth.root_issuer.as_ref());
    let root_subject = configured(auth.root_subject.as_ref());
    if root_issuer != root_subject {
        anyhow::bail!(
            "[auth].root_issuer and [auth].root_subject must be configured together because an OIDC subject is unique only within its issuer"
        );
    }
    Ok(())
}

/// Can a trusted proxy actually assert identities on this server?
///
/// The MCP admin gates read this to know that distinct operators are in play
/// even when a proxied deployment never writes a `users` row.
fn trusted_proxy_identity_enabled(auth: &AuthSettings) -> bool {
    configured(auth.actor_proxy_bearer_token.as_ref())
}

struct ConsolidatorSetup {
    server: AiMemoryServer,
    consolidator: Option<Arc<Consolidator>>,
    admin_llm: Option<Arc<dyn LlmProvider>>,
}

fn maintenance_start_delay(
    last_success_at: Option<i64>,
    now_microseconds: i64,
    interval: Duration,
) -> Duration {
    let fallback = interval.min(MAINTENANCE_STARTUP_DELAY_CAP);
    let Some(last_success_at) = last_success_at else {
        return fallback;
    };
    let interval_microseconds = i64::try_from(interval.as_micros()).unwrap_or(i64::MAX);
    let due_at = last_success_at.saturating_add(interval_microseconds);
    if due_at <= now_microseconds {
        fallback
    } else {
        // A future persisted timestamp (clock correction / restore) must not
        // defer maintenance longer than its configured interval.
        Duration::from_micros(due_at.saturating_sub(now_microseconds) as u64).min(interval)
    }
}

async fn run_persisted_maintenance_job<F, Fut, R, RFut, N>(
    writer: WriterHandle,
    job: ai_memory_store::MaintenanceJob,
    interval: Duration,
    mut load_last_success: R,
    now_microseconds: N,
    mut tick: F,
) where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send,
    R: FnMut() -> RFut + Send + 'static,
    RFut: Future<Output = Result<Option<i64>>> + Send,
    N: Fn() -> i64 + Send + 'static,
{
    let retry_delay = interval.min(MAINTENANCE_STARTUP_DELAY_CAP);
    let mut cadence_known = false;
    let mut delay = Duration::ZERO;

    loop {
        tokio::time::sleep(delay).await;
        if !cadence_known {
            match load_last_success().await {
                Ok(last_success_at) => {
                    delay = maintenance_start_delay(last_success_at, now_microseconds(), interval);
                    cadence_known = true;
                }
                Err(error) => {
                    tracing::warn!(%error, job = job.as_str(), "maintenance cadence state read failed; retrying before running work");
                    delay = retry_delay;
                }
            }
            continue;
        }
        match tick().await {
            Ok(()) => match writer.record_maintenance_job_success(job).await {
                Ok(()) => delay = interval,
                Err(error) => {
                    tracing::warn!(%error, job = job.as_str(), "maintenance success state write failed; retrying job");
                    delay = retry_delay;
                }
            },
            Err(error) => {
                tracing::warn!(%error, job = job.as_str(), "scheduled maintenance job failed; retrying");
                delay = retry_delay;
            }
        }
    }
}

fn session_consolidation_retry_delay(attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(4);
    Duration::from_secs(30_u64.saturating_mul(1_u64 << exponent))
}

async fn run_session_consolidation_worker(
    writer: WriterHandle,
    consolidator: Arc<Consolidator>,
    notify: Arc<tokio::sync::Notify>,
    cancel: CancellationToken,
    #[cfg(test)] completed: Arc<tokio::sync::Notify>,
) {
    loop {
        let now = jiff::Timestamp::now().as_microsecond();
        let lease_micros =
            i64::try_from(SESSION_CONSOLIDATION_LEASE.as_micros()).unwrap_or(i64::MAX);
        let job = match writer
            .claim_session_consolidation(now, now.saturating_sub(lease_micros))
            .await
        {
            Ok(job) => job,
            Err(error) => {
                tracing::warn!(%error, "SessionEnd consolidation queue claim failed");
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = notify.notified() => {},
                    () = tokio::time::sleep(SESSION_CONSOLIDATION_POLL_INTERVAL) => {},
                }
                continue;
            }
        };

        let Some(job) = job else {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = notify.notified() => {},
                () = tokio::time::sleep(SESSION_CONSOLIDATION_POLL_INTERVAL) => {},
            }
            continue;
        };

        let session_id = job.session_id();
        let generation = job.generation();
        let attempts = job.attempts();
        let consolidation = consolidator.consolidate_session(
            session_id,
            false,
            ai_memory_core::ActorContext::anonymous(),
            None,
            None,
        );
        tokio::pin!(consolidation);
        let result = tokio::select! {
            () = cancel.cancelled() => {
                if let Err(error) = writer.release_session_consolidation(job).await {
                    tracing::warn!(%error, %session_id, generation, "failed to release SessionEnd consolidation during shutdown");
                }
                return;
            }
            result = &mut consolidation => result,
        };

        match result {
            Ok(outcome) => match writer.complete_session_consolidation(job).await {
                Ok(()) => {
                    info!(
                        session = %session_id,
                        generation,
                        attempts,
                        path = %outcome.path,
                        "SessionEnd: queued LLM consolidation written (opt-in)",
                    );
                    #[cfg(test)]
                    completed.notify_one();
                }
                Err(error) => tracing::warn!(
                    %error,
                    session = %session_id,
                    generation,
                    "SessionEnd consolidation succeeded but queue completion failed",
                ),
            },
            Err(error) => {
                let retry_at = if attempts < ai_memory_store::SESSION_CONSOLIDATION_MAX_ATTEMPTS {
                    let delay = session_consolidation_retry_delay(attempts);
                    let delay_micros = i64::try_from(delay.as_micros()).unwrap_or(i64::MAX);
                    Some(
                        jiff::Timestamp::now()
                            .as_microsecond()
                            .saturating_add(delay_micros),
                    )
                } else {
                    None
                };
                let terminal = retry_at.is_none();
                if let Err(store_error) = writer
                    .fail_session_consolidation(job, error.to_string(), retry_at)
                    .await
                {
                    tracing::warn!(
                        error = %store_error,
                        session = %session_id,
                        generation,
                        "failed to persist SessionEnd consolidation failure",
                    );
                } else if terminal {
                    tracing::error!(
                        %error,
                        session = %session_id,
                        generation,
                        attempts,
                        "SessionEnd LLM consolidation exhausted retries; heuristic page remains",
                    );
                } else {
                    tracing::warn!(
                        %error,
                        session = %session_id,
                        generation,
                        attempts,
                        "SessionEnd LLM consolidation failed; queued for retry",
                    );
                }
            }
        }
    }
}

/// Run the `serve` subcommand.
///
/// # Errors
/// Returns an error if the store cannot be opened, the watcher cannot
/// install, or the transport setup fails.
pub async fn run(config: &Config, args: ServeArgs) -> Result<()> {
    validate_web_ui_args(args.enable_web, args.web_ui_dir.as_deref())?;

    // Merge config + CLI CORS origins (config first, CLI adds new entries).
    // Validation runs before binding so a misconfigured origin is caught early.
    let cors_origins = merge_cors_origins(&config.cors_allow_origins, &args.cors_allow_origin);
    validate_cors_origins(&cors_origins)?;

    let store = Store::open(&config.data_dir)
        .with_context(|| format!("opening store at {}", config.data_dir.display()))?;

    // One-shot legacy heal (issue #103): NULL out any project repo_path that
    // is a prefix-match catch-all. That means the $HOME and filesystem-root
    // sentinels, plus any path that exists locally but is not a git work-tree
    // root, so existing broken installs self-correct on upgrade. Uses the same
    // $HOME source as the router's match-time guard (captured once in `Config`)
    // so heal and guard agree on its meaning.
    let healed = store
        .writer
        .heal_catch_all_repo_paths(config.home_dir.clone())
        .await?;
    if healed > 0 {
        tracing::info!(
            healed,
            "healed catch-all project repo_path rows ($HOME, filesystem root, or non-git-root path)"
        );
    }

    validate_existing_users_auth(store.reader.users_exist().await?, &config.auth)?;
    validate_trusted_proxy_auth(&config.auth)?;

    // Run any outstanding wiki-structure migrations before the watcher starts
    // so file moves and renames are never raced by the reconciler.
    let wiki_root = config.data_dir.join("wiki");
    run_wiki_migrations(
        &store.writer,
        &store.reader,
        &wiki_root,
        &migrations::registry(),
    )
    .await
    .with_context(|| "applying wiki-structure migrations")?;

    let ws = store
        .writer
        .get_or_create_workspace(args.workspace.clone())
        .await?;
    let proj = store
        .writer
        .get_or_create_project(ws, args.project.clone(), None)
        .await?;
    // Build the privacy strip from config. Compile errors in
    // user-supplied regex abort startup with a clear message so
    // operators discover misconfiguration immediately.
    let sanitizer = Sanitizer::new(&config.sanitize)
        .context("compiling sanitizer.extra_patterns from config")?;
    let wiki = Wiki::new(&config.data_dir, store.writer.clone())?
        .with_sanitizer(sanitizer.clone())
        // Reader attached unconditionally: admission name-resolution uses it
        // when a chain is configured, and the startup scope-manifest backfill
        // (below) always needs it to enumerate scopes.
        .with_store_reader(store.reader.clone());
    // Attach the admission webhook chain (operator-configured via
    // `[[admission_webhooks]]` in config.toml or `AI_MEMORY_ADMISSION_WEBHOOKS__N__*`
    // env vars). Empty config = no chain attached, zero overhead. The store
    // reader is forwarded so the chain can resolve workspace_id/project_id
    // into the human names webhooks address pages by.
    let wiki = if config.admission_webhooks.is_empty() {
        wiki
    } else {
        let chain = ai_memory_wiki::AdmissionChain::new(config.admission_webhooks.clone())
            .context("building admission webhook chain")?;
        tracing::info!(
            count = config.admission_webhooks.len(),
            "admission webhook chain attached"
        );
        wiki.with_admission_chain(chain)
    };
    let provider_health = ProviderHealth::default();
    let (wiki, embedder) = configure_embedder(config, &store, wiki, &provider_health).await?;

    // Make the wiki tree self-describing: write each scope's `_meta.md`
    // (workspace/project name + repo_path) if missing, so the markdown alone
    // can rebuild the index via `ai-memory reindex`. Idempotent; non-fatal.
    match wiki.backfill_scope_manifests().await {
        Ok(0) => {}
        Ok(n) => tracing::info!(count = n, "wrote _meta.md scope manifests"),
        Err(e) => tracing::warn!(error = %e, "scope-manifest backfill failed (non-fatal)"),
    }
    match wiki.ensure_upgrade_baseline_checkpoint() {
        Ok(Some(oid)) => {
            tracing::info!(checkpoint = %oid, "created wiki upgrade baseline checkpoint")
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "wiki upgrade baseline checkpoint failed (non-fatal)"),
    }

    // Keep the guard alive for the lifetime of `serve`.
    let _watcher = start_watcher(&args, &wiki)?;

    // Shared between the MCP server and the hook router: the hook
    // router publishes the cwd-resolved project on each event; the MCP
    // read tools read it as their default so a shared HTTP server
    // answers for the project the agent is actually in, not the static
    // `--project` (issue #2). In stdio mode no hook router is built, so
    // this stays empty and the baked-in default is used.
    // Construct ActiveProject with the configured `[auto_scope]` mode +
    // TTL/cap. `single` (default) preserves the legacy behaviour; the
    // opt-in modes (`per_session`, `per_actor`) keyed-isolate concurrent
    // sessions / operators on shared installs.
    let active_project = ActiveProject::with_config(
        config.auto_scope.mode,
        std::time::Duration::from_secs(config.auto_scope.session_ttl_secs),
        config.auto_scope.max_entries,
    );
    tracing::info!(
        mode = ?config.auto_scope.mode,
        session_ttl_secs = config.auto_scope.session_ttl_secs,
        max_entries = config.auto_scope.max_entries,
        "active-project isolation mode"
    );
    let decay_params = config.decay.decay_params();
    let mut server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_wiki(wiki.clone())
        .with_decay_params(decay_params)
        .with_decay_breadth_weight(config.decay.breadth_weight)
        .with_observation_retention(config.decay.observation_retention())
        .with_auto_improve_require_approval(config.auto_improve.require_approval)
        .with_auto_improve_review_config(auto_improve_review_config_from_settings(
            &config.auto_improve,
        ))
        .with_active_project(active_project.clone())
        .with_sanitizer(sanitizer.clone())
        .with_trusted_proxy_identity(trusted_proxy_identity_enabled(&config.auth))
        .with_per_user_slots(config.slots.per_user)
        .with_strip_root_combinators(config.strip_root_combinators)
        .with_gemini_safe_schemas(config.gemini_safe_schemas);
    if let Some(e) = embedder.clone() {
        server = server.with_embedder(e);
    }
    let consolidator_setup =
        configure_consolidator(config, server, &store, &wiki, ws, proj, &provider_health)?;
    let server = consolidator_setup.server;
    let consolidator = consolidator_setup.consolidator;
    let admin_llm = consolidator_setup.admin_llm;
    let _maintenance_tasks = start_maintenance_scheduler(
        config.maintenance.clone(),
        config.auto_improve.clone(),
        store.reader.clone(),
        store.writer.clone(),
        wiki.clone(),
        embedder.clone(),
        admin_llm.clone(),
        config.decay,
    )
    .await;

    match args.transport {
        TransportKind::Stdio => {
            info!("MCP server ready on stdio (Ctrl-C to stop)");
            let service = server.serve(stdio()).await?;
            service.waiting().await?;
        }
        TransportKind::Http => {
            let bind = args.bind.unwrap_or_else(|| config.bind.clone());
            let cancel = CancellationToken::new();
            let (session_consolidation_notify, session_consolidation_task) =
                if config.consolidate_on_session_end {
                    if let Some(consolidator) = consolidator.clone() {
                        let notify = Arc::new(tokio::sync::Notify::new());
                        let task = tokio::spawn(run_session_consolidation_worker(
                            store.writer.clone(),
                            consolidator,
                            notify.clone(),
                            cancel.child_token(),
                            #[cfg(test)]
                            Arc::new(tokio::sync::Notify::new()),
                        ));
                        info!(
                            max_attempts = ai_memory_store::SESSION_CONSOLIDATION_MAX_ATTEMPTS,
                            "durable SessionEnd consolidation worker started"
                        );
                        (Some(notify), Some(task))
                    } else {
                        (None, None)
                    }
                } else {
                    (None, None)
                };
            let server_clone = server.clone();
            // `Host`-header allowlist for the HTTP DNS-rebinding guard.
            // Sourced from Config (which already handles the
            // `AI_MEMORY_ALLOWED_HOSTS=a,b,c` env-string vs.
            // config.toml sequence forms via the string-or-vec
            // deserializer). Logged so operators can verify the
            // effective list against what they intended.
            info!(
                allowed_hosts = ?config.allowed_hosts,
                "HTTP Host-header allowlist"
            );
            // Default to stateless Streamable HTTP: each POST is serviced
            // independently and answered as plain `application/json`, so
            // stateless clients (OpenCode `type: "remote"`, curl) work
            // without an `mcp-remote` shim (issue #3). ai-memory's tools
            // are pure request-response and project resolution rides the
            // in-process `ActiveProject` pointer, not the transport
            // session — so session mode buys us nothing. `--http-stateful`
            // restores rmcp's session+SSE behaviour for clients that want
            // it.
            info!(
                stateful = args.http_stateful,
                "MCP Streamable HTTP transport mode"
            );
            let mcp_service = StreamableHttpService::new(
                move || Ok(server_clone.clone()),
                LocalSessionManager::default().into(),
                StreamableHttpServerConfig::default()
                    .with_cancellation_token(cancel.child_token())
                    .with_allowed_hosts(config.allowed_hosts.clone())
                    .with_stateful_mode(args.http_stateful)
                    .with_json_response(!args.http_stateful),
            );
            // Shared per-cwd project cache: the hook router owns it; the admin
            // router gets an awaited eviction hook so scope mutations can
            // proactively drop stale entries before the next hook re-resolves.
            let project_cache: ai_memory_hooks::ProjectCache =
                std::sync::Arc::new(tokio::sync::Mutex::new(ProjectCacheStore::default()));
            let scope_invalidator: ai_memory_mcp::ScopeInvalidator = {
                let cache = project_cache.clone();
                std::sync::Arc::new(
                    move |target: ScopeInvalidation| -> std::pin::Pin<
                        Box<dyn std::future::Future<Output = ()> + Send + 'static>,
                    > {
                        let cache = cache.clone();
                        Box::pin(async move {
                            cache.lock().await.retain(|_, v| match target {
                                ScopeInvalidation::Project(proj) => v.1 != proj,
                                ScopeInvalidation::Workspace(ws) => v.0 != ws,
                            });
                        })
                    },
                )
            };
            // One shared counter set: the hook path writes it, /admin/status
            // reads it. Two instances would report zeros to the operator
            // while the real counts accumulated somewhere unreachable.
            let ingest_metrics = std::sync::Arc::new(ai_memory_core::IngestMetrics::default());
            let hooks = hook_router(HookState {
                ingest_metrics: ingest_metrics.clone(),
                workspace_id: ws,
                project_id: proj,
                writer: store.writer.clone(),
                reader: store.reader.clone(),
                wiki: wiki.clone(),
                consolidator: consolidator.clone(),
                sanitizer: sanitizer.clone(),
                project_cache: project_cache.clone(),
                active_project: active_project.clone(),
                ingest_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                    DEFAULT_HOOK_INGEST_MAX_IN_FLIGHT,
                )),
                ingest_gates: ai_memory_hooks::IngestGates::default(),
                consolidate_on_session_end: config.consolidate_on_session_end,
                session_consolidation_notify,
                capture_assistant_enabled: config.capture_assistant,
                per_user_slots: config.slots.per_user,
                subagent_sessions: std::sync::Arc::new(tokio::sync::Mutex::new(
                    ai_memory_hooks::SubagentSessionSet::default(),
                )),
                ingest_rate: std::sync::Arc::new(tokio::sync::Mutex::new(
                    ai_memory_hooks::IngestRateLimiter::new(
                        config.hook_rate_per_sec.max(0.0),
                        if config.hook_rate_burst > 0.0 {
                            config.hook_rate_burst
                        } else {
                            config.hook_rate_per_sec.max(1.0)
                        },
                    ),
                )),
                home_dir: config.home_dir.clone(),
                trusted_proxy_identity: trusted_proxy_identity_enabled(&config.auth),
                mid_session_routing: config.routing.mid_session,
            });
            let workstreams = workstream_router(WorkstreamState {
                writer: store.writer.clone(),
                reader: store.reader.clone(),
                sanitizer: sanitizer.clone(),
                data_dir: config.data_dir.clone(),
            });
            let admin = admin_router_with_sweep_tuning(
                AdminState {
                    ingest_metrics: ingest_metrics.clone(),
                    writer: store.writer.clone(),
                    reader: store.reader.clone(),
                    wiki: wiki.clone(),
                    llm: admin_llm,
                    auto_improve_require_approval: config.auto_improve.require_approval,
                    auto_improve_review_config: auto_improve_review_config_from_settings(
                        &config.auto_improve,
                    ),
                    embedder: embedder.clone(),
                    provider_health: provider_health.clone(),
                    decay_params,
                    data_dir: config.data_dir.clone(),
                    db_path: store.db_path().to_path_buf(),
                    bind: bind.clone(),
                    home_dir: config.home_dir.clone(),
                    bootstrap_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
                    token_pepper: config
                        .auth
                        .token_pepper
                        .as_ref()
                        .filter(|p| !p.trim().is_empty())
                        .map(|p| ai_memory_store::TokenPepper::new(p.clone())),
                    active_project: active_project.clone(),
                    scope_invalidator: Some(scope_invalidator),
                    trusted_proxy_identity: trusted_proxy_identity_enabled(&config.auth),
                },
                config.decay.breadth_weight,
                config.decay.observation_retention(),
            );
            // Multi-rung auth assembly:
            //   - rung 0 (no bearer_token configured) → AuthState::new
            //     stays as-is, middleware injects anonymous actor.
            //   - rung 1 (bearer_token set, no token_pepper) → root_actor
            //     stamps writes with [auth].root_* identity.
            //   - token_pepper present → unknown bearers always route through
            //     the users-table lookup, including before the first user is
            //     created. Admin mode separately switches on a fresh
            //     store-backed users-exist read.
            let mut auth_state = AuthState::new(config.auth.bearer_token.clone())
                .with_secure_cookie(config.auth.secure_cookie);
            let root_user = config
                .auth
                .root_username
                .clone()
                .filter(|value| !value.trim().is_empty());
            let root_issuer = config
                .auth
                .root_issuer
                .clone()
                .filter(|value| !value.trim().is_empty());
            let root_subject = config
                .auth
                .root_subject
                .clone()
                .filter(|value| !value.trim().is_empty());
            if root_user.is_some() || root_issuer.is_some() {
                auth_state = auth_state.with_root_actor(ai_memory_core::ActorContext {
                    user: root_user,
                    issuer: root_issuer,
                    sub: root_subject,
                    name: config.auth.root_name.clone(),
                    email: config.auth.root_email.clone(),
                    ..ai_memory_core::ActorContext::default()
                });
            }
            if let Some(proxy_token) = config
                .auth
                .actor_proxy_bearer_token
                .as_ref()
                .filter(|s| !s.trim().is_empty())
            {
                auth_state = auth_state.with_trusted_proxy_bearer(proxy_token.clone());
            }
            if let Some(pepper) = config
                .auth
                .token_pepper
                .as_ref()
                .filter(|p| !p.trim().is_empty())
            {
                auth_state = auth_state.with_multiuser(
                    ai_memory_store::TokenPepper::new(pepper.clone()),
                    store.reader.clone(),
                    store.writer.clone(),
                );
            }
            let auth_state = Arc::new(auth_state);
            let auth_enabled = auth_state.enabled();
            let router = axum::Router::new()
                .nest_service("/mcp", mcp_service)
                .merge(hooks)
                .merge(workstreams)
                .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
                .merge(admin.layer(DefaultBodyLimit::max(BOOTSTRAP_MAX_BODY_BYTES)));
            let base_path = normalize_prefix(&args.base_path);
            if base_path.is_empty() && !args.base_path.trim_matches('/').trim().is_empty() {
                tracing::warn!(
                    raw = %args.base_path,
                    "AI_MEMORY_BASE_PATH is not a safe path prefix; serving at root instead",
                );
            }
            // Symmetric warning for `--web-slug` / `AI_MEMORY_WEB_SLUG`. The
            // original commit only warned on `base_path` downgrade, so an
            // operator who set `AI_MEMORY_WEB_SLUG=/web space` would have
            // their slug silently collapsed to the empty mount with no
            // signal — same hazard as base-path.
            let web_slug_normal = normalize_prefix(&args.web_slug);
            if web_slug_normal.is_empty() && !args.web_slug.trim_matches('/').trim().is_empty() {
                tracing::warn!(
                    raw = %args.web_slug,
                    "AI_MEMORY_WEB_SLUG is not a safe path prefix; serving the web UI at the base-path root instead",
                );
            }
            let base_href = web_base_href(&args.base_path, &args.web_slug);
            let router = mount_web_router(
                router,
                args.enable_web,
                store.reader.clone(),
                wiki.clone(),
                WebMountSpec {
                    web_ui_dir: args.web_ui_dir.as_deref(),
                    cors_origins: &cors_origins,
                    web_slug: &args.web_slug,
                    base_href: &base_href,
                    base_path: &base_path,
                },
            )?;
            let router = apply_http_layers(router, auth_state, config.allowed_hosts.clone());
            // Host the entire surface under the configured base path. Empty
            // base = root (unchanged). The auth/host layers are already
            // attached to `router`, so they run for every nested route.
            let router = if base_path.is_empty() {
                router
            } else {
                axum::Router::new().nest(&base_path, router)
            };
            // Mount `/favicon.ico` at the absolute HOST root — outside the
            // auth gate, outside `--base-path`, outside the `/web` nest.
            // Browsers auto-fetch `<host>/favicon.ico` without auth headers
            // regardless of where the app is mounted; routing it under
            // `/web` (as PR #79 originally did) made it unreachable to the
            // browser's automatic fetch. Negligible info leak — the icon
            // is the same embedded PNG anyone hitting `/web` already sees.
            let router = if args.enable_web {
                router.merge(ai_memory_web::favicon_router())
            } else {
                router
            };
            let listener = tokio::net::TcpListener::bind(&bind)
                .await
                .with_context(|| format!("binding {bind}"))?;
            let local_addr = listener
                .local_addr()
                .context("reading bound HTTP listener address")?;
            let exposure = validate_http_exposure(
                local_addr,
                auth_enabled,
                args.allow_insecure_no_auth,
                running_in_container(),
            )?;
            info!(
                %local_addr,
                auth = auth_enabled,
                body_limit_mb = MAX_BODY_BYTES / 1024 / 1024,
                "MCP HTTP server ready (POST /mcp, POST /hook, Ctrl-C to stop)",
            );
            if exposure == HttpExposure::InsecureByOverride {
                tracing::warn!(
                    %local_addr,
                    "starting unauthenticated plain HTTP on a non-loopback address because \
                     --allow-insecure-no-auth was supplied — anyone on the network can call \
                     destructive MCP tools"
                );
            } else if exposure == HttpExposure::UndeterminedInContainer {
                tracing::warn!(
                    %local_addr,
                    "no AI_MEMORY_AUTH_TOKEN configured. Inside a container the bind address \
                     cannot show whether this port reaches the network — that is decided by \
                     the host publish spec. If you published it with `-p 127.0.0.1:49374:49374` \
                     you are fine; if you published it on 0.0.0.0 or to a LAN address, anyone \
                     on the network can call destructive MCP tools. Generate a token with \
                     `ai-memory generate-auth-token` and set AI_MEMORY_AUTH_TOKEN"
                );
            } else if auth_enabled && !local_addr.ip().is_loopback() {
                // Auth IS configured but the server is reachable from
                // the network on plain HTTP. The bearer token (and
                // multi-user per-user tokens from `ai-memory user
                // add`) ride cleartext — sniffable on the LAN. Advise
                // the operator to front with a TLS proxy. One-shot
                // log at startup, not refusal to serve (operators may
                // be testing, behind their own proxy already, etc.).
                tracing::warn!(
                    %local_addr,
                    "AI_MEMORY_AUTH_TOKEN is set but the server is bound to a \
                     non-loopback address on plain HTTP — bearer tokens travel \
                     cleartext on the network. Front ai-memory with a TLS-terminating \
                     reverse proxy (Caddy, Cloudflare Tunnel, nginx). See \
                     docs/https-via-proxy.md for copy-paste templates."
                );
            }
            let shutdown_cancel = cancel.clone();
            let serve_result = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    info!("ctrl-c received; shutting down");
                    shutdown_cancel.cancel();
                })
                .await;
            cancel.cancel();
            if let Some(task) = session_consolidation_task
                && let Err(error) = task.await
            {
                tracing::warn!(%error, "SessionEnd consolidation worker join failed");
            }
            serve_result?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn start_maintenance_scheduler(
    settings: MaintenanceSettings,
    auto_improve: AutoImproveSettings,
    reader: ReaderPool,
    writer: WriterHandle,
    wiki: Wiki,
    embedder: Option<Arc<dyn Embedder>>,
    llm: Option<Arc<dyn LlmProvider>>,
    decay: crate::config::DecaySettings,
) -> Vec<tokio::task::JoinHandle<()>> {
    let maintenance_enabled = settings.enabled;
    if !maintenance_enabled {
        info!("scheduled retention/lint/embed maintenance disabled");
    }

    let forget_sweep_interval_secs = settings.forget_sweep_interval_secs;
    let lint_interval_secs = settings.lint_interval_secs;
    let embedding_backfill_interval_secs = settings.embedding_backfill_interval_secs;

    let mut tasks = Vec::new();
    if maintenance_enabled && forget_sweep_interval_secs > 0 {
        let reader = reader.clone();
        let writer = writer.clone();
        let wiki = wiki.clone();
        tasks.push(tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(forget_sweep_interval_secs);
            run_persisted_maintenance_job(
                writer.clone(),
                ai_memory_store::MaintenanceJob::ForgetSweep,
                interval,
                {
                    let reader = reader.clone();
                    move || {
                        let reader = reader.clone();
                        async move {
                            Ok(reader
                                .maintenance_job_last_success(
                                    ai_memory_store::MaintenanceJob::ForgetSweep,
                                )
                                .await?)
                        }
                    }
                },
                || jiff::Timestamp::now().as_microsecond(),
                move || {
                    let reader = reader.clone();
                    let writer = writer.clone();
                    let wiki = wiki.clone();
                    let decay = decay;
                    async move {
                        let started = std::time::Instant::now();
                        let outcome = run_scheduled_sweep_tick(
                            &reader,
                            &writer,
                            &wiki,
                            &decay.decay_params(),
                            decay.breadth_weight,
                            decay.observation_retention(),
                        )
                        .await?;
                        if outcome.errors > 0 {
                            anyhow::bail!(
                                "scheduled forget sweep had {} scope errors",
                                outcome.errors
                            );
                        }
                        info!(
                            scopes = outcome.scopes,
                            candidates_evaluated = outcome.candidates_evaluated,
                            evicted = outcome.evicted,
                            expired = outcome.expired,
                            hard_deleted = outcome.hard_deleted,
                            observations_pruned = outcome.observations_pruned,
                            errors = outcome.errors,
                            elapsed_ms = started.elapsed().as_millis(),
                            "scheduled forget sweep completed"
                        );
                        Ok(())
                    }
                },
            )
            .await;
        }));
    }

    // Hollow-project sweep: deletes project rows with no pages, sessions,
    // observations, handoffs, managed workstreams, or auto-improvement data
    // once they are older than HOLLOW_PROJECT_MIN_AGE_DAYS. Safe by
    // construction — nothing exists to lose — which is why it runs
    // unconditionally under the maintenance flag with no extra config. Runs
    // once shortly after startup (so upgrades clean up immediately) and then
    // daily.
    if maintenance_enabled {
        /// A week of grace before a hollow row is considered noise, so a
        /// project created moments before its first real event is never
        /// racing the sweep.
        const HOLLOW_PROJECT_MIN_AGE_DAYS: u32 = 7;
        const HOLLOW_SWEEP_INTERVAL: std::time::Duration =
            std::time::Duration::from_secs(24 * 60 * 60);
        /// Short startup delay so the sweep never competes with migration
        /// and first-request work on boot.
        const HOLLOW_SWEEP_STARTUP_DELAY: std::time::Duration = std::time::Duration::from_secs(60);
        let writer = writer.clone();
        tasks.push(tokio::spawn(async move {
            tokio::time::sleep(HOLLOW_SWEEP_STARTUP_DELAY).await;
            loop {
                match writer
                    .sweep_hollow_projects(HOLLOW_PROJECT_MIN_AGE_DAYS)
                    .await
                {
                    Ok(deleted) if deleted.is_empty() => {}
                    Ok(deleted) => info!(
                        count = deleted.len(),
                        projects = deleted.join(", "),
                        "hollow-project sweep deleted empty project rows"
                    ),
                    Err(e) => tracing::warn!(error = %e, "hollow-project sweep failed"),
                }
                tokio::time::sleep(HOLLOW_SWEEP_INTERVAL).await;
            }
        }));
    }

    if maintenance_enabled && lint_interval_secs > 0 {
        let reader = reader.clone();
        let writer = writer.clone();
        let wiki = wiki.clone();
        let llm = llm.clone();
        tasks.push(tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(lint_interval_secs);
            run_persisted_maintenance_job(
                writer,
                ai_memory_store::MaintenanceJob::RuleLint,
                interval,
                {
                    let reader = reader.clone();
                    move || {
                        let reader = reader.clone();
                        async move {
                            Ok(reader
                                .maintenance_job_last_success(
                                    ai_memory_store::MaintenanceJob::RuleLint,
                                )
                                .await?)
                        }
                    }
                },
                || jiff::Timestamp::now().as_microsecond(),
                move || {
                    let reader = reader.clone();
                    let wiki = wiki.clone();
                    let llm = llm.clone();
                    let decay_lambda = decay.decay_params().lambda;
                    async move {
                        let started = std::time::Instant::now();
                        let outcome =
                            run_scheduled_lint_tick(&reader, &wiki, llm.as_ref(), decay_lambda)
                                .await?;
                        if outcome.errors > 0 {
                            anyhow::bail!(
                                "scheduled rule-based lint had {} scope errors",
                                outcome.errors
                            );
                        }
                        info!(
                            scopes = outcome.scopes,
                            findings = outcome.findings,
                            errors = outcome.errors,
                            elapsed_ms = started.elapsed().as_millis(),
                            "scheduled rule-based lint completed"
                        );
                        Ok(())
                    }
                },
            )
            .await;
        }));
    }

    if maintenance_enabled && embedding_backfill_interval_secs > 0 {
        if let Some(embedder) = embedder {
            let reader = reader.clone();
            let writer = writer.clone();
            let wiki = wiki.clone();
            tasks.push(tokio::spawn(async move {
                let interval = std::time::Duration::from_secs(embedding_backfill_interval_secs);
                loop {
                    tokio::time::sleep(interval).await;
                    let started = std::time::Instant::now();
                    match run_scheduled_embedding_backfill_tick(&reader, &writer, &wiki, &embedder)
                        .await
                    {
                        Ok(outcome) => info!(
                            scopes = outcome.scopes,
                            embedded = outcome.embedded,
                            skipped = outcome.skipped,
                            failed = outcome.failed,
                            errors = outcome.errors,
                            elapsed_ms = started.elapsed().as_millis(),
                            "scheduled embedding backfill completed"
                        ),
                        Err(e) => tracing::warn!(error = %e, "scheduled embedding backfill failed"),
                    }
                }
            }));
        } else {
            tracing::warn!(
                "maintenance.embedding_backfill_interval_secs is set but no embedder is configured"
            );
        }
    }

    let scheduler = auto_improve.scheduler.clone();
    if !scheduler.enabled || scheduler.interval_secs == 0 || scheduler.max_sessions_per_tick == 0 {
        info!("auto-improve scheduler disabled; manual auto-improve remains available");
    } else if let Some(llm) = llm.clone() {
        let reader = reader.clone();
        let writer = writer.clone();
        let wiki = wiki.clone();
        let scheduler_settings = ScheduledAutoImproveSettings {
            review: auto_improve_review_config_from_settings(&auto_improve),
            require_approval: auto_improve.require_approval,
            min_session_age_secs: scheduler.min_session_age_secs,
            max_sessions_per_tick: scheduler.max_sessions_per_tick,
        };
        match ai_memory_consolidate::initialize_auto_improve_scheduler_scopes(&reader, &writer)
            .await
        {
            Ok((scopes, errors)) => info!(
                scopes,
                errors, "auto-improve scheduler startup scope initialization completed"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                "auto-improve scheduler startup scope initialization failed"
            ),
        }
        tasks.push(tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(scheduler.interval_secs);
            // Sleep after each complete tick instead of driving work from a
            // fixed-rate interval. If reviewing all projects takes longer than
            // `interval`, the next tick is delayed rather than overlapping the
            // still-running one.
            loop {
                tokio::time::sleep(interval).await;
                let started = std::time::Instant::now();
                match run_auto_improve_scheduler_tick(
                    &reader,
                    &writer,
                    &wiki,
                    &llm,
                    &scheduler_settings,
                )
                .await
                {
                    Ok(outcome) => info!(
                        scopes = outcome.scopes,
                        scopes_with_candidates = outcome.scopes_with_candidates,
                        reviewed = outcome.reviewed,
                        skipped = outcome.skipped,
                        errors = outcome.errors,
                        elapsed_ms = started.elapsed().as_millis(),
                        "scheduled auto-improve tick completed"
                    ),
                    Err(e) => {
                        tracing::warn!(error = %e, "scheduled auto-improve tick failed")
                    }
                };
            }
        }));
    } else {
        info!("auto-improve scheduler enabled but no LLM provider is configured; job not started");
    }

    if tasks.is_empty() {
        info!("scheduled maintenance enabled but all intervals are disabled");
    } else {
        info!(jobs = tasks.len(), "scheduled maintenance started");
    }
    tasks
}

#[derive(Debug, Default)]
struct ScheduledSweepTickOutcome {
    scopes: usize,
    candidates_evaluated: usize,
    evicted: usize,
    expired: usize,
    hard_deleted: usize,
    observations_pruned: usize,
    errors: usize,
}

async fn run_scheduled_sweep_tick(
    reader: &ReaderPool,
    writer: &WriterHandle,
    wiki: &Wiki,
    decay: &ai_memory_store::DecayParams,
    breadth_weight: f64,
    retention: ObservationRetention,
) -> Result<ScheduledSweepTickOutcome> {
    let scopes = reader.list_all_scopes().await?;
    let mut outcome = ScheduledSweepTickOutcome {
        scopes: scopes.len(),
        ..ScheduledSweepTickOutcome::default()
    };

    for scope in scopes {
        match run_sweep_with_options(
            reader,
            writer,
            Some(wiki),
            scope.workspace_id,
            scope.project_id,
            decay,
            breadth_weight,
            retention,
            false,
        )
        .await
        {
            Ok(report) => {
                outcome.candidates_evaluated += report.candidates_evaluated;
                outcome.evicted += report.evicted.iter().filter(|page| page.deleted).count();
                outcome.expired += report.expired.len();
                outcome.hard_deleted += report.hard_deleted;
                outcome.observations_pruned += report.observations_pruned;
            }
            Err(e) => {
                outcome.errors += 1;
                tracing::warn!(
                    workspace = %scope.workspace_name,
                    project = %scope.project_name,
                    error = %e,
                    "scheduled forget sweep failed for scope"
                );
            }
        }
    }

    Ok(outcome)
}

#[derive(Debug, Default)]
struct ScheduledLintTickOutcome {
    scopes: usize,
    findings: usize,
    errors: usize,
}

async fn run_scheduled_lint_tick(
    reader: &ReaderPool,
    wiki: &Wiki,
    llm: Option<&Arc<dyn LlmProvider>>,
    decay_lambda: f64,
) -> Result<ScheduledLintTickOutcome> {
    let scopes = reader.list_all_scopes().await?;
    let mut outcome = ScheduledLintTickOutcome {
        scopes: scopes.len(),
        ..ScheduledLintTickOutcome::default()
    };

    for scope in scopes {
        match run_lint(
            reader,
            wiki,
            llm,
            scope.workspace_id,
            scope.project_id,
            ai_memory_consolidate::LintOptions {
                dry_run: false,
                use_llm: false,
                decay_lambda,
            },
        )
        .await
        {
            Ok(report) => outcome.findings += report.findings.len(),
            Err(e) => {
                outcome.errors += 1;
                tracing::warn!(
                    workspace = %scope.workspace_name,
                    project = %scope.project_name,
                    error = %e,
                    "scheduled lint failed for scope"
                );
            }
        }
    }

    Ok(outcome)
}

#[derive(Debug, Default)]
struct ScheduledEmbeddingBackfillTickOutcome {
    scopes: usize,
    embedded: usize,
    /// Pages that already had a current embedding. Reported because a
    /// tick that skipped everything and a tick that had nothing to do
    /// are otherwise indistinguishable in the log.
    skipped: usize,
    failed: usize,
    errors: usize,
}

async fn run_scheduled_embedding_backfill_tick(
    reader: &ReaderPool,
    writer: &WriterHandle,
    wiki: &Wiki,
    embedder: &Arc<dyn Embedder>,
) -> Result<ScheduledEmbeddingBackfillTickOutcome> {
    let scopes = reader.list_all_scopes().await?;
    let mut outcome = ScheduledEmbeddingBackfillTickOutcome {
        scopes: scopes.len(),
        ..ScheduledEmbeddingBackfillTickOutcome::default()
    };

    for scope in scopes {
        match run_embedding_backfill(
            reader,
            writer,
            wiki,
            embedder,
            scope.workspace_id,
            scope.project_id,
            EmbedBackfillOptions::default(),
        )
        .await
        {
            Ok(counts) => {
                outcome.embedded += counts.embedded;
                outcome.skipped += counts.skipped;
                outcome.failed += counts.failed;
            }
            Err(e) => {
                outcome.errors += 1;
                tracing::warn!(
                    workspace = %scope.workspace_name,
                    project = %scope.project_name,
                    error = %e,
                    "scheduled embedding backfill failed for scope"
                );
            }
        }
    }

    Ok(outcome)
}

fn auto_improve_review_config_from_settings(
    settings: &AutoImproveSettings,
) -> AutoImproveReviewConfig {
    AutoImproveReviewConfig {
        min_observations: settings.min_observations,
        min_session_duration_secs: settings.min_session_duration_secs,
        min_confidence: settings.min_confidence,
        max_input_tokens: settings.max_input_tokens,
        max_proposals_per_run: settings.max_proposals_per_run,
        include_raw_fallback: settings.include_raw_fallback,
        proposal_actor: settings.proposal_actor.clone(),
        pending_path: settings.pending_path.clone(),
        max_patchable_pages: settings.max_patchable_pages,
        max_patchable_body_chars: settings.max_patchable_body_chars,
        max_edits_per_proposal: settings.max_edits_per_proposal,
        max_edit_content_chars: settings.max_edit_content_chars,
        max_changed_chars_per_proposal: settings.max_changed_chars_per_proposal,
        max_patch_edits_per_run: settings.max_patch_edits_per_run,
        max_rejection_context: settings.max_rejection_context,
        rejection_context_days: settings.rejection_context_days,
        max_final_body_chars: settings.max_final_body_chars,
        max_rule_page_tokens: settings.max_rule_page_tokens,
        max_procedure_page_tokens: settings.max_procedure_page_tokens,
        eval: ai_memory_consolidate::AutoImproveEvalConfig {
            enabled: settings.eval.enabled,
            command: settings.eval.command.clone(),
            timeout_secs: settings.eval.timeout_secs,
            targets: settings.eval.targets.clone(),
            min_delta: settings.eval.min_delta,
        },
    }
}

async fn configure_embedder(
    config: &Config,
    store: &Store,
    wiki: Wiki,
    provider_health: &ProviderHealth,
) -> Result<(Wiki, Option<Arc<dyn Embedder>>)> {
    // M9 — pluggable embedder. Stored rows carry provider/model/dim so
    // query paths can ignore stale vectors after an embedding config change.
    let Some(cfg) = config.embedder_config()? else {
        info!(
            "AI_MEMORY_EMBEDDING_PROVIDER unset; vector search disabled (FTS5 + entity + graph active)"
        );
        return Ok((wiki, None));
    };
    let provider_name = cfg.provider.name().to_string();
    let model = cfg.model.clone();
    let dim = cfg.dim;
    let embedder = build_embedder(cfg).context("building embedder from config")?;
    let mismatch = store
        .reader
        .embedding_meta_for_mismatch(
            embedder.provider().into(),
            embedder.model().into(),
            embedder.dim(),
        )
        .await?;
    if !mismatch.is_empty() {
        // Mismatch handling applies to hybrid search (queries only load
        // rows matching the configured triple), not to process liveness.
        // Blocking startup made `embed --force` impossible because the
        // CLI is an HTTP client to this server.
        tracing::warn!(
            stored = ?mismatch,
            configured_provider = embedder.provider(),
            configured_model = embedder.model(),
            configured_dim = embedder.dim(),
            "stored embeddings use a different (provider, model, dim) than configured; \
             hybrid search ignores stale rows until pages are re-embedded — \
             run `ai-memory embed --force` (or wait for scheduled backfill)"
        );
    }
    info!(
        provider = embedder.provider(),
        model = embedder.model(),
        dim = embedder.dim(),
        "embedder enabled"
    );
    let embedder = provider_health.wrap_embedder(embedder, provider_name, model, dim);
    Ok((wiki.with_embedder(embedder.clone()), Some(embedder)))
}

fn start_watcher(args: &ServeArgs, wiki: &Wiki) -> Result<Option<WatcherHandle>> {
    if args.no_watcher {
        info!("watcher disabled by --no-watcher");
        return Ok(None);
    }
    info!(
        root = %wiki.root().display(),
        workspace = %args.workspace,
        project = %args.project,
        "starting wiki watcher",
    );
    Ok(Some(WatcherHandle::start(wiki.clone())?))
}

fn configure_consolidator(
    config: &Config,
    mut server: AiMemoryServer,
    store: &Store,
    wiki: &Wiki,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    provider_health: &ProviderHealth,
) -> Result<ConsolidatorSetup> {
    // Validate the reranker setting before the provider early-return, so
    // a typo'd or provider-less `AI_MEMORY_RERANKER` surfaces instead of
    // looking enabled while eligible queries keep their normal ranking.
    let want_reranker = config.reranker_choice()?;
    // Build the consolidator (if LLM configured) once, then share the
    // Arc between the MCP server (for `memory_consolidate` + lint),
    // the hook router (for PreCompact checkpointing), and the admin
    // router (for `POST /admin/bootstrap`).
    let Some(cfg) = config.llm_provider_config()? else {
        info!(
            "AI_MEMORY_LLM_PROVIDER unset; memory_consolidate disabled, PreCompact \
             falls back to rule-based checkpoint, lint runs rule-based only"
        );
        if want_reranker {
            anyhow::bail!(
                "AI_MEMORY_RERANKER=llm requires AI_MEMORY_LLM_PROVIDER: the reranker \
                 judges candidates with the configured LLM provider. Set a provider or \
                 unset AI_MEMORY_RERANKER."
            );
        }
        return Ok(ConsolidatorSetup {
            server,
            consolidator: None,
            admin_llm: None,
        });
    };
    let provider_name = cfg.provider.name().to_string();
    let model = cfg.model.clone();
    let retry_hint = llm_retry_hint(&provider_name, &model, cfg.base_url.as_deref());
    let llm = build_provider(cfg).context("building LLM provider from config")?;
    let llm = provider_health.wrap_llm_provider(llm, provider_name, model, Some(retry_hint));
    info!(
        provider = llm.name(),
        model = llm.model(),
        max_input_tokens = config.consolidation.max_input_tokens,
        max_output_tokens = config.consolidation.max_output_tokens,
        "memory_consolidate + PreCompact LLM checkpointing enabled",
    );
    let consolidator = Arc::new(
        Consolidator::new(
            store.reader.clone(),
            store.writer.clone(),
            wiki.clone(),
            llm.clone(),
            workspace_id,
            project_id,
        )
        .with_per_user_slots(config.slots.per_user)
        .with_prompt_limits(
            config.consolidation.max_input_tokens,
            config.consolidation.max_output_tokens,
        ),
    );
    server = server.with_consolidator_arc(wiki.clone(), llm.clone(), consolidator.clone());
    // Optional post-RRF reranking rides on the same provider, so it is
    // only reachable once an LLM is configured at all. Off unless the
    // operator asked for it: it puts an LLM call on the memory_query
    // hot path.
    if want_reranker {
        info!(
            provider = llm.name(),
            model = llm.model(),
            "project/scopes memory_query reranking enabled (adds LLM latency)",
        );
        server = server.with_reranker(Arc::new(ai_memory_llm::LlmReranker::new(llm.clone())));
    }
    Ok(ConsolidatorSetup {
        server,
        consolidator: Some(consolidator),
        admin_llm: Some(llm),
    })
}

/// Validate a list of CORS origins before the server binds.
///
/// Rules match the spec: wildcard + credentials is forbidden, each entry
/// must carry a scheme, and trailing slashes are rejected (they do not
/// match browser origins which never carry a trailing slash).
pub fn validate_cors_origins(origins: &[String]) -> Result<()> {
    for origin in origins {
        if origin == "*" {
            anyhow::bail!(
                "CORS origin `*` is not allowed: the CORS spec forbids credentials \
                 with a wildcard origin. Use explicit origins such as \
                 https://app.example.com"
            );
        }
        if !origin.starts_with("http://") && !origin.starts_with("https://") {
            anyhow::bail!(
                "CORS origin `{origin}` is missing a scheme. Each entry must start \
                 with http:// or https://"
            );
        }
        if origin.ends_with('/') {
            anyhow::bail!(
                "CORS origin `{origin}` has a trailing slash. Browser origins \
                 never carry a trailing slash — use `{}` instead",
                origin.trim_end_matches('/')
            );
        }
    }
    Ok(())
}

/// Merge config-file origins with CLI flag origins, preserving order and
/// deduplicating (config entries appear first).
fn merge_cors_origins(from_config: &[String], from_cli: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut merged = Vec::new();
    for origin in from_config.iter().chain(from_cli.iter()) {
        if seen.insert(origin.clone()) {
            merged.push(origin.clone());
        }
    }
    merged
}

fn validate_web_ui_args(enable_web: bool, web_ui_dir: Option<&Path>) -> Result<()> {
    if web_ui_dir.is_some() && !enable_web {
        anyhow::bail!("--web-ui-dir requires --enable-web");
    }

    if let Some(dir) = web_ui_dir {
        if !dir.is_dir() {
            anyhow::bail!("--web-ui-dir is not a directory: {}", dir.display());
        }
        if !dir.join("index.html").is_file() {
            anyhow::bail!("--web-ui-dir is missing index.html: {}", dir.display());
        }
    }

    Ok(())
}

fn llm_retry_hint(provider: &str, model: &str, base_url: Option<&str>) -> String {
    let mut command = format!("ai-memory llm-test --provider {provider} --model {model}");
    if let Some(base_url) = base_url {
        command.push_str(&format!(" --base-url {base_url}"));
    }
    command.push_str(" --prompt ping");
    command
}

fn apply_http_layers(
    router: axum::Router,
    auth_state: Arc<AuthState>,
    allowed_hosts: Vec<String>,
) -> axum::Router {
    router
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            require_bearer,
        ))
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(allowed_hosts),
            require_allowed_host,
        ))
}

async fn require_allowed_host(
    State(allowed_hosts): State<Arc<Vec<String>>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let Some(host) = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
    else {
        return (StatusCode::BAD_REQUEST, "missing Host header\n").into_response();
    };
    if host_allowed(host, &allowed_hosts) {
        return next.run(req).await;
    }
    tracing::warn!(host, allowed = ?allowed_hosts, "rejected request with disallowed Host header");
    (StatusCode::FORBIDDEN, "forbidden host\n").into_response()
}

fn host_allowed(host: &str, allowed_hosts: &[String]) -> bool {
    allowed_hosts.iter().any(|allowed| {
        host.eq_ignore_ascii_case(allowed) || host_without_port(host).eq_ignore_ascii_case(allowed)
    })
}

fn host_without_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[')
        && let Some((inside, _)) = rest.split_once(']')
    {
        return inside;
    }
    match host.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') && port.chars().all(|c| c.is_ascii_digit()) => {
            name
        }
        _ => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_core::{
        AgentKind, NewObservation, NewSession, ObservationKind, PagePath, Sanitized, Sanitizer,
        SessionId, Tier,
    };
    use ai_memory_llm::{ChatRequest, ChatResponse, LlmResult, SyntheticEmbedder};
    use ai_memory_wiki::WritePageRequest;
    use axum::http::Request;
    use std::future::Future;
    use std::pin::Pin;
    use tempfile::TempDir;
    use tower::ServiceExt;

    async fn wait_for_maintenance_success(
        store: &Store,
        job: ai_memory_store::MaintenanceJob,
    ) -> i64 {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(last_success) = store
                .reader
                .maintenance_job_last_success(job)
                .await
                .unwrap()
            {
                return last_success;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "maintenance writer did not persist successful completion"
            );
            tokio::task::yield_now().await;
        }
    }

    #[test]
    fn existing_users_require_nonempty_pepper_and_root_bearer() {
        for users_exist in [false, true] {
            for (pepper_label, token_pepper) in [
                ("missing", None),
                ("blank", Some("  ")),
                ("present", Some("pepper")),
            ] {
                for (bearer_label, bearer_token) in [
                    ("missing", None),
                    ("blank", Some("\t")),
                    ("present", Some("root-token")),
                ] {
                    let auth = AuthSettings {
                        token_pepper: token_pepper.map(str::to_string),
                        bearer_token: bearer_token.map(str::to_string),
                        ..AuthSettings::default()
                    };
                    let result = validate_existing_users_auth(users_exist, &auth);
                    let should_pass =
                        !users_exist || (pepper_label == "present" && bearer_label == "present");
                    assert_eq!(
                        result.is_ok(),
                        should_pass,
                        "users={users_exist}, pepper={pepper_label}, bearer={bearer_label}"
                    );
                }
            }
        }
    }

    #[test]
    fn unauthenticated_non_loopback_http_requires_explicit_override() {
        let addresses = [
            ("127.0.0.1:49374", true),
            ("[::1]:49374", true),
            ("0.0.0.0:49374", false),
            ("[::]:49374", false),
            ("192.168.1.90:49374", false),
        ];

        for (address, loopback) in addresses {
            let local_addr: SocketAddr = address.parse().expect("valid test address");
            for auth_enabled in [false, true] {
                for allow_override in [false, true] {
                    // On a host, the bind address IS the evidence: an
                    // unauthenticated non-loopback bind is refused unless
                    // the operator overrode it.
                    let allowed = loopback || auth_enabled || allow_override;
                    assert_eq!(
                        validate_http_exposure(local_addr, auth_enabled, allow_override, false)
                            .is_ok(),
                        allowed,
                        "address={address}, auth={auth_enabled}, override={allow_override}"
                    );
                }
            }
        }
    }

    /// Regression for #407. The published image binds `0.0.0.0` because that
    /// is the only way `-p` publishing works, so the host-side rule above
    /// refused every container started from the documented Quick Start and
    /// left it crash-looping under `--restart unless-stopped`.
    #[test]
    fn containers_warn_instead_of_refusing_because_the_bind_proves_nothing() {
        let quick_start: SocketAddr = "0.0.0.0:49374".parse().expect("valid test address");

        // The exact Quick Start shape: no token, no override, in a container.
        assert_eq!(
            validate_http_exposure(quick_start, false, false, true).expect("must not refuse"),
            HttpExposure::UndeterminedInContainer,
        );

        // Identical inputs on a host still refuse — the carve-out is scoped
        // to the container case and does not soften the host rule.
        assert!(validate_http_exposure(quick_start, false, false, false).is_err());

        // A container is not a blanket downgrade: with auth configured the
        // verdict is Safe, so the operator gets no spurious warning.
        assert_eq!(
            validate_http_exposure(quick_start, true, false, true).expect("auth is fine"),
            HttpExposure::Safe,
        );

        // An explicit override still reports as an override, not as the
        // container case, so the startup log keeps naming the real reason.
        assert_eq!(
            validate_http_exposure(quick_start, false, true, true).expect("override is fine"),
            HttpExposure::InsecureByOverride,
        );

        // Loopback inside a container is plain Safe.
        let loopback: SocketAddr = "127.0.0.1:49374".parse().expect("valid test address");
        assert_eq!(
            validate_http_exposure(loopback, false, false, true).expect("loopback is fine"),
            HttpExposure::Safe,
        );
    }

    #[test]
    fn trusted_proxy_configuration_requires_distinct_credentials() {
        let missing_root = AuthSettings {
            actor_proxy_bearer_token: Some("proxy-token".into()),
            ..AuthSettings::default()
        };
        assert!(validate_trusted_proxy_auth(&missing_root).is_err());

        let shared = AuthSettings {
            bearer_token: Some("same-token".into()),
            actor_proxy_bearer_token: Some(" same-token ".into()),
            ..AuthSettings::default()
        };
        assert!(validate_trusted_proxy_auth(&shared).is_err());

        let valid = AuthSettings {
            bearer_token: Some("root-token".into()),
            actor_proxy_bearer_token: Some("proxy-token".into()),
            ..AuthSettings::default()
        };
        assert!(validate_trusted_proxy_auth(&valid).is_ok());
        assert!(trusted_proxy_identity_enabled(&valid));
    }

    #[test]
    fn root_oidc_identity_requires_issuer_and_subject_together() {
        for (issuer, subject, valid) in [
            (None, None, true),
            (Some("https://idp.example"), None, false),
            (None, Some("root-subject"), false),
            (Some("https://idp.example"), Some("root-subject"), true),
        ] {
            let auth = AuthSettings {
                root_issuer: issuer.map(str::to_string),
                root_subject: subject.map(str::to_string),
                ..AuthSettings::default()
            };
            assert_eq!(
                validate_trusted_proxy_auth(&auth).is_ok(),
                valid,
                "issuer={issuer:?}, subject={subject:?}"
            );
        }
    }

    #[test]
    fn maintenance_start_delay_handles_never_overdue_and_remaining_cadence() {
        let interval = Duration::from_secs(120);
        let now = 1_000_000_000i64;
        assert_eq!(
            maintenance_start_delay(None, now, interval),
            MAINTENANCE_STARTUP_DELAY_CAP
        );
        assert_eq!(
            maintenance_start_delay(Some(now - 120_000_000), now, interval),
            MAINTENANCE_STARTUP_DELAY_CAP
        );
        assert_eq!(
            maintenance_start_delay(Some(now - 30_000_000), now, interval),
            Duration::from_secs(90)
        );
        assert_eq!(
            maintenance_start_delay(None, now, Duration::from_secs(10)),
            Duration::from_secs(10)
        );
        assert_eq!(
            maintenance_start_delay(Some(i64::MAX), now, interval),
            interval,
            "future/extreme persisted timestamps cannot defer beyond interval"
        );
        assert_eq!(
            maintenance_start_delay(Some(i64::MIN), i64::MAX, interval),
            MAINTENANCE_STARTUP_DELAY_CAP,
            "opposite-sign extremes must not overflow"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn persisted_maintenance_retries_failures_and_waits_after_success() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let attempts_for_tick = attempts.clone();
        let reader_for_state = store.reader.clone();
        let task = tokio::spawn(run_persisted_maintenance_job(
            store.writer.clone(),
            ai_memory_store::MaintenanceJob::ForgetSweep,
            Duration::from_secs(2),
            move || {
                let reader = reader_for_state.clone();
                async move {
                    Ok(reader
                        .maintenance_job_last_success(ai_memory_store::MaintenanceJob::ForgetSweep)
                        .await?)
                }
            },
            || jiff::Timestamp::now().as_microsecond(),
            move || {
                let attempts = attempts_for_tick.clone();
                let events = events.clone();
                async move {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                    events.send(attempt).unwrap();
                    if attempt == 1 {
                        anyhow::bail!("injected failure");
                    }
                    Ok(())
                }
            },
        ));

        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(received.recv().await, Some(1));
        assert_eq!(
            store
                .reader
                .maintenance_job_last_success(ai_memory_store::MaintenanceJob::ForgetSweep)
                .await
                .unwrap(),
            None,
            "failed ticks must not advance persisted cadence"
        );

        tokio::time::advance(Duration::from_millis(1999)).await;
        tokio::task::yield_now().await;
        assert!(received.try_recv().is_err(), "failure retry is not early");
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(received.try_recv(), Ok(2));
        wait_for_maintenance_success(&store, ai_memory_store::MaintenanceJob::ForgetSweep).await;

        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(
            received.try_recv().is_err(),
            "next tick waits full interval"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(received.recv().await, Some(3));
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn maintenance_state_read_failure_retries_before_running_work() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let work = Arc::new(AtomicUsize::new(0));
        let reads_for_loader = reads.clone();
        let work_for_tick = work.clone();
        let task = tokio::spawn(run_persisted_maintenance_job(
            store.writer.clone(),
            ai_memory_store::MaintenanceJob::RuleLint,
            Duration::from_secs(2),
            move || {
                let reads = reads_for_loader.clone();
                async move {
                    if reads.fetch_add(1, Ordering::SeqCst) == 0 {
                        anyhow::bail!("injected cadence read failure");
                    }
                    Ok(None)
                }
            },
            || jiff::Timestamp::now().as_microsecond(),
            move || {
                let work = work_for_tick.clone();
                async move {
                    work.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
        ));

        tokio::task::yield_now().await;
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(
            work.load(Ordering::SeqCst),
            0,
            "read failure must not run work"
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert_eq!(
            work.load(Ordering::SeqCst),
            0,
            "successful retry still observes startup delay"
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(work.load(Ordering::SeqCst), 1);
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn persisted_loop_runs_one_bounded_startup_catchup_for_never_and_overdue() {
        const NOW: i64 = 10_000_000;
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let never_events = events.clone();
        let overdue_events = events.clone();
        let never = tokio::spawn(run_persisted_maintenance_job(
            store.writer.clone(),
            ai_memory_store::MaintenanceJob::ForgetSweep,
            Duration::from_secs(2),
            || async { Ok(None) },
            || NOW,
            move || {
                let events = never_events.clone();
                async move {
                    events.send("never").unwrap();
                    Ok(())
                }
            },
        ));
        let overdue = tokio::spawn(run_persisted_maintenance_job(
            store.writer.clone(),
            ai_memory_store::MaintenanceJob::RuleLint,
            Duration::from_secs(2),
            || async { Ok(Some(NOW - 2_000_000)) },
            || NOW,
            move || {
                let events = overdue_events.clone();
                async move {
                    events.send("overdue").unwrap();
                    Ok(())
                }
            },
        ));
        tokio::task::yield_now().await;
        assert!(received.try_recv().is_err());
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        let mut seen = [false, false];
        while let Ok(event) = received.try_recv() {
            match event {
                "never" => seen[0] = true,
                "overdue" => seen[1] = true,
                _ => unreachable!(),
            }
        }
        assert_eq!(seen, [true, true]);
        tokio::task::yield_now().await;
        assert!(
            received.try_recv().is_err(),
            "no immediate repeat after catch-up"
        );
        never.abort();
        overdue.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn persisted_loop_waits_exact_remaining_interval_when_not_due() {
        const NOW: i64 = 20_000_000;
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run_persisted_maintenance_job(
            store.writer.clone(),
            ai_memory_store::MaintenanceJob::ForgetSweep,
            Duration::from_secs(4),
            || async { Ok(Some(NOW - 1_000_000)) },
            || NOW,
            move || {
                let events = events.clone();
                async move {
                    events.send(()).unwrap();
                    Ok(())
                }
            },
        ));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(2999)).await;
        tokio::task::yield_now().await;
        assert!(received.try_recv().is_err());
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(received.try_recv(), Ok(()));
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn persisted_loop_waits_after_long_tick_without_overlap() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run_persisted_maintenance_job(
            store.writer.clone(),
            ai_memory_store::MaintenanceJob::RuleLint,
            Duration::from_secs(2),
            || async { Ok(None) },
            || 0,
            move || {
                let events = events.clone();
                async move {
                    events.send("start").unwrap();
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    events.send("end").unwrap();
                    Ok(())
                }
            },
        ));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(received.try_recv(), Ok("start"));
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert!(
            received.try_recv().is_err(),
            "second tick cannot overlap long first tick"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(received.try_recv(), Ok("end"));
        wait_for_maintenance_success(&store, ai_memory_store::MaintenanceJob::RuleLint).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(
            received.try_recv().is_err(),
            "interval starts after completion"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            received.try_recv(),
            Ok("start"),
            "next tick starts after the full post-completion interval"
        );
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn persisted_loop_restart_waits_remaining_interval_from_real_state() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let interval = Duration::from_secs(4);
        let (first_events, mut first_received) = tokio::sync::mpsc::unbounded_channel();
        let first = tokio::spawn(run_persisted_maintenance_job(
            store.writer.clone(),
            ai_memory_store::MaintenanceJob::ForgetSweep,
            interval,
            || async { Ok(None) },
            || 0,
            move || {
                let events = first_events.clone();
                async move {
                    events.send(()).unwrap();
                    Ok(())
                }
            },
        ));
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(interval).await;
        tokio::task::yield_now().await;
        assert_eq!(first_received.try_recv(), Ok(()));

        let persisted =
            wait_for_maintenance_success(&store, ai_memory_store::MaintenanceJob::ForgetSweep)
                .await;
        first.abort();

        // Restart one second into the persisted cadence: exactly three seconds remain.
        let (second_events, mut second_received) = tokio::sync::mpsc::unbounded_channel();
        let (state_loaded, mut state_loaded_received) = tokio::sync::mpsc::unbounded_channel();
        let second_reader = store.reader.clone();
        let second = tokio::spawn(run_persisted_maintenance_job(
            store.writer.clone(),
            ai_memory_store::MaintenanceJob::ForgetSweep,
            interval,
            move || {
                let reader = second_reader.clone();
                let state_loaded = state_loaded.clone();
                async move {
                    let state = reader
                        .maintenance_job_last_success(ai_memory_store::MaintenanceJob::ForgetSweep)
                        .await?;
                    state_loaded.send(()).unwrap();
                    Ok(state)
                }
            },
            move || persisted + 1_000_000,
            move || {
                let events = second_events.clone();
                async move {
                    events.send(()).unwrap();
                    Ok(())
                }
            },
        ));
        assert_eq!(state_loaded_received.recv().await, Some(()));
        tokio::time::advance(Duration::from_millis(2999)).await;
        tokio::task::yield_now().await;
        assert!(second_received.try_recv().is_err());
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(second_received.try_recv(), Ok(()));
        second.abort();
    }

    #[tokio::test]
    async fn disabled_maintenance_creates_no_persisted_job_state() {
        let (_tmp, store, wiki, _ws, _first, _second) = two_project_wiki().await;
        let tasks = start_maintenance_scheduler(
            MaintenanceSettings {
                enabled: false,
                forget_sweep_interval_secs: 1,
                lint_interval_secs: 1,
                embedding_backfill_interval_secs: 1,
            },
            AutoImproveSettings::default(),
            store.reader.clone(),
            store.writer.clone(),
            wiki,
            None,
            None,
            crate::config::DecaySettings::default(),
        )
        .await;
        assert!(tasks.is_empty());
        for job in [
            ai_memory_store::MaintenanceJob::ForgetSweep,
            ai_memory_store::MaintenanceJob::RuleLint,
        ] {
            assert_eq!(
                store
                    .reader
                    .maintenance_job_last_success(job)
                    .await
                    .unwrap(),
                None
            );
        }
    }

    #[tokio::test]
    async fn zero_interval_omits_only_that_maintenance_job() {
        for settings in [
            MaintenanceSettings {
                enabled: true,
                forget_sweep_interval_secs: 0,
                lint_interval_secs: 60,
                embedding_backfill_interval_secs: 0,
            },
            MaintenanceSettings {
                enabled: true,
                forget_sweep_interval_secs: 60,
                lint_interval_secs: 0,
                embedding_backfill_interval_secs: 0,
            },
        ] {
            let (_tmp, store, wiki, _ws, _first, _second) = two_project_wiki().await;
            let tasks = start_maintenance_scheduler(
                settings,
                AutoImproveSettings::default(),
                store.reader.clone(),
                store.writer.clone(),
                wiki,
                None,
                None,
                crate::config::DecaySettings::default(),
            )
            .await;
            // One enabled lint/sweep job plus the independent hollow-project job.
            assert_eq!(tasks.len(), 2);
            for task in tasks {
                task.abort();
            }
        }
    }

    struct PanicLlm;

    impl LlmProvider for PanicLlm {
        fn name(&self) -> &'static str {
            "panic"
        }

        fn model(&self) -> &str {
            "panic"
        }

        fn complete<'life0, 'async_trait>(
            &'life0 self,
            _request: ChatRequest,
        ) -> Pin<Box<dyn Future<Output = LlmResult<ChatResponse>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move { panic!("preflight-skipped scheduler test must not call LLM") })
        }

        fn complete_structured_raw<'life0, 'async_trait>(
            &'life0 self,
            _request: ChatRequest,
            _schema: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = LlmResult<serde_json::Value>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move { panic!("preflight-skipped scheduler test must not call LLM") })
        }
    }

    struct SuccessfulConsolidationLlm;

    impl LlmProvider for SuccessfulConsolidationLlm {
        fn name(&self) -> &'static str {
            "successful-consolidation"
        }

        fn model(&self) -> &str {
            "test"
        }

        fn complete<'life0, 'async_trait>(
            &'life0 self,
            _request: ChatRequest,
        ) -> Pin<Box<dyn Future<Output = LlmResult<ChatResponse>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                Ok(ChatResponse {
                    text: "unused".into(),
                    usage: None,
                    model: "test".into(),
                })
            })
        }

        fn complete_structured_raw<'life0, 'async_trait>(
            &'life0 self,
            _request: ChatRequest,
            _schema: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = LlmResult<serde_json::Value>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                Ok(serde_json::json!({
                    "title": "Compiled session",
                    "body_markdown": "Durable worker completed this session.",
                    "tags": ["test"]
                }))
            })
        }
    }

    #[tokio::test]
    async fn session_consolidation_worker_consumes_and_completes_durable_job() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let workspace_id = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let project_id = store
            .writer
            .get_or_create_project(workspace_id, "project", None)
            .await
            .unwrap();
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id,
                project_id,
                agent_kind: AgentKind::Codex,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id,
                    workspace_id,
                    project_id,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "finish".into(),
                    body: "complete the durable job".into(),
                    importance: 8,
                },
                &Sanitizer::default(),
            ))
            .await
            .unwrap();
        store.writer.end_session(session_id, None).await.unwrap();
        store
            .writer
            .enqueue_session_consolidation(workspace_id, project_id, session_id)
            .await
            .unwrap();

        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let consolidator = Arc::new(Consolidator::new(
            store.reader.clone(),
            store.writer.clone(),
            wiki,
            Arc::new(SuccessfulConsolidationLlm),
            workspace_id,
            project_id,
        ));
        let notify = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(tokio::sync::Notify::new());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_session_consolidation_worker(
            store.writer.clone(),
            consolidator,
            notify.clone(),
            cancel.child_token(),
            completed.clone(),
        ));
        notify.notify_one();

        tokio::time::timeout(Duration::from_secs(5), completed.notified())
            .await
            .expect("worker should complete the queued job");

        let path = format!("sessions/{session_id}.md");
        assert!(
            store
                .reader
                .page_body_by_ids(workspace_id, project_id, &path)
                .await
                .unwrap()
                .is_some_and(|page| page.body.contains("Durable worker completed")),
            "queue completion must follow the durable wiki write"
        );

        cancel.cancel();
        task.await.unwrap();
        let now = jiff::Timestamp::now().as_microsecond();
        assert!(
            store
                .writer
                .claim_session_consolidation(now, now - 1)
                .await
                .unwrap()
                .is_none(),
            "completed work must not be claimed again"
        );
    }

    async fn two_project_wiki() -> (TempDir, Store, Wiki, WorkspaceId, ProjectId, ProjectId) {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let first = store
            .writer
            .get_or_create_project(ws, "first", None)
            .await
            .unwrap();
        let second = store
            .writer
            .get_or_create_project(ws, "second", None)
            .await
            .unwrap();
        (tmp, store, wiki, ws, first, second)
    }

    async fn write_test_page(
        wiki: &Wiki,
        ws: WorkspaceId,
        project: ProjectId,
        path: &str,
        title: &str,
        tier: Tier,
    ) {
        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: project,
            path: PagePath::new(path).unwrap(),
            frontmatter: serde_json::json!({"title": title}),
            body: format!("# {title}\n\nbody for {path}"),
            tier,
            pinned: false,
            title: Some(title.into()),
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn scheduled_maintenance_sweep_tick_covers_all_projects() {
        let (_tmp, store, wiki, ws, first, second) = two_project_wiki().await;
        for (project, name) in [(first, "first"), (second, "second")] {
            write_test_page(
                &wiki,
                ws,
                project,
                &format!("notes/{name}.md"),
                name,
                Tier::Episodic,
            )
            .await;
        }

        let decay = ai_memory_store::DecayParams {
            cold_threshold: 2.0,
            ..ai_memory_store::DecayParams::default()
        };
        let outcome = run_scheduled_sweep_tick(
            &store.reader,
            &store.writer,
            &wiki,
            &decay,
            0.0,
            ObservationRetention::default(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.scopes, 2);
        assert_eq!(outcome.errors, 0);
        assert_eq!(outcome.candidates_evaluated, 2);
        assert_eq!(outcome.evicted, 2);
        for project in [first, second] {
            assert!(
                store
                    .reader
                    .decay_candidates(ws, project)
                    .await
                    .unwrap()
                    .is_empty(),
                "sweep should evict the eligible page in every project"
            );
        }
    }

    #[tokio::test]
    async fn scheduled_maintenance_lint_tick_covers_all_projects() {
        let (_tmp, store, wiki, ws, first, second) = two_project_wiki().await;
        for project in [first, second] {
            write_test_page(
                &wiki,
                ws,
                project,
                "notes/a.md",
                "Duplicate",
                Tier::Semantic,
            )
            .await;
            write_test_page(
                &wiki,
                ws,
                project,
                "notes/b.md",
                "Duplicate",
                Tier::Semantic,
            )
            .await;
        }

        let panic_llm: Arc<dyn LlmProvider> = Arc::new(PanicLlm);
        let outcome = run_scheduled_lint_tick(
            &store.reader,
            &wiki,
            Some(&panic_llm),
            ai_memory_store::DecayParams::default().lambda,
        )
        .await
        .unwrap();

        assert_eq!(outcome.scopes, 2);
        assert_eq!(outcome.errors, 0);
        assert_eq!(outcome.findings, 2);
        for project in [first, second] {
            assert!(
                wiki.read_page(ws, project, &PagePath::new("_lint".to_string()).unwrap())
                    .is_err(),
                "lint reports are dated pages, not the directory itself"
            );
            let lint_pages = store
                .reader
                .decay_candidates(ws, project)
                .await
                .unwrap()
                .into_iter()
                .filter(|c| c.path.as_str().starts_with("_lint/"))
                .count();
            assert_eq!(lint_pages, 1, "lint should write one report per project");
        }
    }

    #[tokio::test]
    async fn scheduled_maintenance_embedding_backfill_tick_covers_all_projects() {
        let (_tmp, store, wiki, ws, first, second) = two_project_wiki().await;
        for (project, name) in [(first, "first"), (second, "second")] {
            write_test_page(
                &wiki,
                ws,
                project,
                &format!("notes/{name}.md"),
                name,
                Tier::Semantic,
            )
            .await;
        }
        let embedder: Arc<dyn Embedder> = Arc::new(SyntheticEmbedder::new(16));

        let outcome =
            run_scheduled_embedding_backfill_tick(&store.reader, &store.writer, &wiki, &embedder)
                .await
                .unwrap();

        assert_eq!(outcome.scopes, 2);
        assert_eq!(outcome.errors, 0);
        assert_eq!(outcome.failed, 0);
        assert_eq!(outcome.embedded, 2);
        for project in [first, second] {
            let embedded = store
                .reader
                .embedded_page_ids(
                    ws,
                    project,
                    embedder.provider().to_string(),
                    embedder.model().to_string(),
                    embedder.dim(),
                )
                .await
                .unwrap();
            assert_eq!(embedded.len(), 1, "each project should get embeddings");
        }
    }

    /// A tick that skipped every page and a tick that had nothing to do
    /// both embed zero pages. Without `skipped` in the completion line
    /// they are the same log entry, so a page being passed over every
    /// hour reads as a quiet, healthy scheduler (#509).
    #[tokio::test]
    async fn scheduled_embedding_tick_distinguishes_skipped_work_from_an_idle_pass() {
        let (_tmp, store, wiki, ws, first, second) = two_project_wiki().await;
        let embedder: Arc<dyn Embedder> = Arc::new(SyntheticEmbedder::new(16));

        let idle =
            run_scheduled_embedding_backfill_tick(&store.reader, &store.writer, &wiki, &embedder)
                .await
                .unwrap();
        assert_eq!(
            (idle.embedded, idle.skipped),
            (0, 0),
            "no pages yet: nothing embedded and nothing skipped"
        );

        for (project, name) in [(first, "first"), (second, "second")] {
            write_test_page(
                &wiki,
                ws,
                project,
                &format!("notes/{name}.md"),
                name,
                Tier::Semantic,
            )
            .await;
        }

        let first_pass =
            run_scheduled_embedding_backfill_tick(&store.reader, &store.writer, &wiki, &embedder)
                .await
                .unwrap();
        assert_eq!((first_pass.embedded, first_pass.skipped), (2, 0));

        let second_pass =
            run_scheduled_embedding_backfill_tick(&store.reader, &store.writer, &wiki, &embedder)
                .await
                .unwrap();
        assert_eq!(
            (second_pass.embedded, second_pass.failed),
            (0, 0),
            "the work is done, so the tick embeds nothing"
        );
        assert_eq!(
            second_pass.skipped, 2,
            "but it must still say it passed over two pages, or it is              indistinguishable from the idle tick above"
        );
    }

    #[test]
    fn host_allowed_accepts_host_with_port() {
        let allowed = vec!["127.0.0.1".to_string(), "localhost".to_string()];
        assert!(host_allowed("127.0.0.1:49374", &allowed));
        assert!(host_allowed("localhost", &allowed));
    }

    #[test]
    fn host_allowed_rejects_unknown_host() {
        let allowed = vec!["127.0.0.1".to_string()];
        assert!(!host_allowed("evil.example:49374", &allowed));
    }

    #[test]
    fn host_without_port_handles_ipv6_loopback() {
        assert_eq!(host_without_port("[::1]:49374"), "::1");
    }

    #[test]
    fn web_ui_dir_requires_enable_web() {
        let ui = TempDir::new().unwrap();
        std::fs::write(ui.path().join("index.html"), "custom ui").unwrap();

        let err = validate_web_ui_args(false, Some(ui.path())).unwrap_err();
        assert!(
            err.to_string()
                .contains("--web-ui-dir requires --enable-web"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn web_ui_dir_must_be_directory() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("index.html");
        std::fs::write(&file, "custom ui").unwrap();

        let err = validate_web_ui_args(true, Some(&file)).unwrap_err();
        assert!(
            err.to_string().contains("--web-ui-dir is not a directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn web_ui_dir_must_include_index_html() {
        let ui = TempDir::new().unwrap();

        let err = validate_web_ui_args(true, Some(ui.path())).unwrap_err();
        assert!(
            err.to_string()
                .contains("--web-ui-dir is missing index.html"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn valid_web_ui_dir_passes_validation() {
        let ui = TempDir::new().unwrap();
        std::fs::write(ui.path().join("index.html"), "custom ui").unwrap();

        validate_web_ui_args(true, Some(ui.path())).unwrap();
    }

    #[tokio::test]
    async fn web_routes_are_inside_auth_layer() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let router = mount_web_router(
            axum::Router::new(),
            true,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                web_ui_dir: None,
                cors_origins: &[],
                web_slug: "/web",
                base_href: "/web/",
                base_path: "",
            },
        )
        .unwrap();
        let router = apply_http_layers(
            router,
            Arc::new(AuthState::new(Some("secret".to_string()))),
            vec!["localhost".to_string()],
        );

        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/web")
                    .header("Host", "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn reranker_without_llm_provider_fails_server_setup() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj);
        let config = Config {
            reranker: Some("llm".into()),
            ..Config::default()
        };

        let result = configure_consolidator(
            &config,
            server,
            &store,
            &wiki,
            ws,
            proj,
            &ProviderHealth::default(),
        );
        let err = match result {
            Ok(_) => panic!("provider-less reranking must fail server setup"),
            Err(err) => err,
        };
        assert!(
            err.to_string()
                .contains("AI_MEMORY_RERANKER=llm requires AI_MEMORY_LLM_PROVIDER"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn embedder_mismatch_warns_but_keeps_server_startable() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();

        let synthetic: Arc<dyn Embedder> = Arc::new(SyntheticEmbedder::new(64));
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_embedder(synthetic);
        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/old-embedding.md").unwrap(),
            frontmatter: serde_json::json!({"title": "old embedding"}),
            body: "existing vector row".into(),
            tier: Tier::Semantic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        })
        .await
        .unwrap();

        let cfg = Config {
            data_dir: tmp.path().to_path_buf(),
            embedding_provider: Some("openai".into()),
            runtime_env: crate::config::RuntimeEnv::with_openai_api_key_for_tests("test-key"),
            ..Config::default()
        };
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

        let provider_health = ProviderHealth::default();
        let (_wiki, embedder) = configure_embedder(&cfg, &store, wiki, &provider_health)
            .await
            .unwrap();

        let embedder = embedder.expect("configured embedder should be enabled");
        assert_eq!(embedder.provider(), "openai");
        assert_eq!(
            provider_health.snapshot().embedding.status,
            ai_memory_llm::ProviderHealthStatus::Unknown
        );
    }

    // ── Part B: CORS validation tests ──────────────────────────────────────

    #[test]
    fn validate_cors_origins_rejects_wildcard() {
        let err = validate_cors_origins(&["*".to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("wildcard"),
            "error must mention wildcard: {err}"
        );
    }

    #[test]
    fn validate_cors_origins_rejects_missing_scheme() {
        let err = validate_cors_origins(&["app.example.com".to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("missing a scheme"),
            "error must mention missing scheme: {err}"
        );
    }

    #[test]
    fn validate_cors_origins_rejects_trailing_slash() {
        let err = validate_cors_origins(&["https://app.example.com/".to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("trailing slash"),
            "error must mention trailing slash: {err}"
        );
    }

    #[test]
    fn validate_cors_origins_accepts_well_formed() {
        validate_cors_origins(&[
            "https://app.example.com".to_string(),
            "http://localhost:5173".to_string(),
        ])
        .unwrap();
    }

    #[test]
    fn validate_cors_origins_accepts_empty_list() {
        validate_cors_origins(&[]).unwrap();
    }

    #[test]
    fn merge_cors_origins_deduplicates_preserving_order() {
        let merged = merge_cors_origins(
            &[
                "https://a.example.com".to_string(),
                "https://b.example.com".to_string(),
            ],
            &[
                "https://b.example.com".to_string(),
                "https://c.example.com".to_string(),
            ],
        );
        assert_eq!(
            merged,
            vec![
                "https://a.example.com",
                "https://b.example.com",
                "https://c.example.com"
            ]
        );
    }
}
