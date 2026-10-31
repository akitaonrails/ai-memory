//! `ai-memory serve` — MCP server with optional filesystem watcher.

use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ai_memory_consolidate::{
    AutoImproveReviewConfig, Consolidator, EmbedBackfillOptions, ObservationRetention,
    ScheduledAutoImproveSettings, run_auto_improve_scheduler_tick, run_embedding_backfill,
    run_lint,
};
use ai_memory_core::{ActiveProject, IdentityKey, ProjectId, Sanitizer, WorkspaceId};
use ai_memory_hooks::{
    CLIENT_MARKER_HEADER, DEFAULT_HOOK_INGEST_MAX_IN_FLIGHT, HookState, ProjectCacheStore,
    WorkstreamState, hook_router, workstream_router,
};
use ai_memory_llm::{
    Embedder, EmbeddingCapacity, LlmProvider, ProviderHealth, build_embedder, local_capacity,
};
use ai_memory_mcp::human_auth::{Cidr, HumanAuthRuntime, LoginLimiter};
use ai_memory_mcp::{
    AdminState, AiMemoryServer, ScopeInvalidation, admin_router_with_sweep_tuning,
    expire_legacy_cookie_mw, internal_auth_router, public_auth_router, require_dual_auth,
    session_auth_router,
};
use ai_memory_store::{
    ReaderPool, Store, TokenPepper, WriterHandle, hash_session_secret, hash_token,
};
use ai_memory_web::{
    HtmlAuthRedirectConfig, WebMountSpec, html_auth_redirect_mw, normalize_prefix,
    split_web_routers_with_metrics, web_base_href,
};
use ai_memory_wiki::{WatcherHandle, Wiki, WikiError, WikiResult, migrations, run_wiki_migrations};
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
use secrecy::ExposeSecret;
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

/// Lock file guarding a data dir against a second `ai-memory serve` (#563).
const SERVE_LOCK_FILE: &str = ".serve.lock";

/// How long a wait on the shutdown path may run before the drain it is
/// waiting for is abandoned. axum's graceful shutdown waits for every
/// in-flight connection and a stateful or SSE MCP client can hold one open
/// indefinitely, so an unbounded drain is indistinguishable from ignoring the
/// signal: `docker stop` and `systemctl stop` would still burn their own
/// grace period and finish with SIGKILL (#699). Each wait is bounded on its
/// own, so a stop can take a small multiple of this.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// The signals that stop a running server, listened for on both transports.
///
/// Installed before the transport starts. The server runs as PID 1 under
/// `docker run` (no init shim), and for PID 1 the kernel discards any signal
/// whose handler is not installed — so a SIGTERM arriving during a slow boot
/// (the pre-migration archive, wiki migrations) must already have a listener
/// waiting for it. A tokio `Signal` queues a signal received before the first
/// `recv`, so registering early loses nothing (#699).
struct ShutdownSignals {
    #[cfg(unix)]
    interrupt: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    terminate: Option<tokio::signal::unix::Signal>,
}

impl ShutdownSignals {
    /// Install the listeners.
    ///
    /// A listener that cannot be registered degrades to the remaining one with
    /// a warning: losing one way to stop the server is bad, refusing to start
    /// over it is worse.
    #[cfg(unix)]
    fn install() -> Self {
        use tokio::signal::unix::{SignalKind, signal};

        fn listen(kind: SignalKind, name: &str) -> Option<tokio::signal::unix::Signal> {
            match signal(kind) {
                Ok(stream) => Some(stream),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        signal = name,
                        "cannot listen for this shutdown signal; the server will not stop on it"
                    );
                    None
                }
            }
        }

        Self {
            interrupt: listen(SignalKind::interrupt(), "SIGINT"),
            terminate: listen(SignalKind::terminate(), "SIGTERM"),
        }
    }

    /// Install the listeners. Non-unix has only ctrl-c.
    #[cfg(not(unix))]
    fn install() -> Self {
        Self {}
    }

    /// Resolve with the name of the first shutdown signal to arrive.
    #[cfg(unix)]
    async fn recv(&mut self) -> &'static str {
        match (self.interrupt.as_mut(), self.terminate.as_mut()) {
            (Some(interrupt), Some(terminate)) => tokio::select! {
                _ = interrupt.recv() => "SIGINT",
                _ = terminate.recv() => "SIGTERM",
            },
            (Some(interrupt), None) => {
                interrupt.recv().await;
                "SIGINT"
            }
            (None, Some(terminate)) => {
                terminate.recv().await;
                "SIGTERM"
            }
            // Both registrations failed: there is nothing left to wait for.
            (None, None) => std::future::pending().await,
        }
    }

    /// Resolve with the name of the first shutdown signal to arrive.
    #[cfg(not(unix))]
    async fn recv(&mut self) -> &'static str {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "ctrl-c listener failed; the server will not stop on it");
            std::future::pending::<()>().await;
        }
        "ctrl-c"
    }
}

/// The single-instance guard for `ai-memory serve`: an exclusive `flock` on
/// `<data-dir>/.serve.lock` held for the process lifetime. The OS releases it
/// when the process exits, so a crashed server never locks the operator out of
/// their own data.
#[derive(Debug)]
struct ServeLock {
    _file: std::fs::File,
}

/// Unlocked sidecar naming the lock holder (`pid=<n>`). Separate from the
/// lock file because Windows' exclusive lock blocks reads of the locked
/// file from other handles; informational only, rewritten by each holder.
fn holder_info_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join(".serve.lock.holder")
}

/// Take the single-instance serve lock for `data_dir`.
///
/// A contended lock refuses startup naming the holder, unless `force` is set:
/// the operator who knows the previous server is gone (a hung holder, or a
/// mount with unreliable locking) must not be stranded. A filesystem that
/// cannot lock at all only downgrades the guard to a warning — refusing to
/// start there would be worse than the unguarded risk.
/// Transient failures `open`/`try_lock_exclusive` can raise on a healthy but
/// loaded machine: fd exhaustion (EMFILE per-process, ENFILE system-wide) and
/// interrupted syscalls (EINTR). These deserve a short retry. A `WouldBlock`
/// (another server already holds the lock, classified by
/// `is_drain_lock_busy_error`) is deliberately excluded — that is the correct
/// "someone else owns it" refusal and must surface immediately, never retried.
fn is_transient_serve_lock_error(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::Interrupted {
        return true;
    }
    #[cfg(unix)]
    if let Some(code) = err.raw_os_error() {
        // EMFILE (per-process fd limit) / ENFILE (system-wide fd limit).
        const EMFILE: i32 = 24;
        const ENFILE: i32 = 23;
        if code == EMFILE || code == ENFILE {
            return true;
        }
    }
    false
}

fn acquire_serve_lock(data_dir: &Path, force: bool) -> Result<Option<ServeLock>> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating data directory {}", data_dir.display()))?;
    let path = data_dir.join(SERVE_LOCK_FILE);
    use fs2::FileExt as _;

    // Open the file and take the exclusive flock under a short bounded retry
    // for transient errors only. A loaded machine (parallel test runs, an fd
    // storm) can bounce `open` with EMFILE/ENFILE or interrupt the lock call
    // with EINTR; a real server must not hard-fail on that. Mirrors
    // `acquire_drain_lock`'s bounded ~25ms backoff. A `WouldBlock` (another
    // holder) is never retried here — it falls through to the refusal path.
    const SERVE_LOCK_ACQUIRE_ATTEMPTS: u32 = 5;
    const SERVE_LOCK_ACQUIRE_BACKOFF: Duration = Duration::from_millis(25);
    let mut attempt: u32 = 0;
    let (file, lock_result) = loop {
        attempt += 1;
        let retriable = attempt < SERVE_LOCK_ACQUIRE_ATTEMPTS;
        let file = match std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(err) if retriable && is_transient_serve_lock_error(&err) => {
                std::thread::sleep(SERVE_LOCK_ACQUIRE_BACKOFF);
                continue;
            }
            Err(err) => {
                return Err(err).with_context(|| format!("opening serve lock {}", path.display()));
            }
        };
        match file.try_lock_exclusive() {
            Err(err) if retriable && is_transient_serve_lock_error(&err) => {
                std::thread::sleep(SERVE_LOCK_ACQUIRE_BACKOFF);
                continue;
            }
            result => break (file, result),
        }
    };

    match lock_result {
        Ok(()) => {
            // Informational only: the flock is the guard, and this names the
            // holder in a later refusal message. Best-effort, and written to
            // an UNLOCKED sidecar — on Windows the exclusive lock blocks
            // other handles from reading the locked file itself, which made
            // every refusal report "holder unknown".
            let _ = std::fs::write(
                holder_info_path(data_dir),
                format!("pid={}\n", std::process::id()),
            );
            tracing::info!(lock = %path.display(), "single-instance serve lock held");
            Ok(Some(ServeLock { _file: file }))
        }
        Err(err) if crate::commands::hook_spool::is_drain_lock_busy_error(&err) => {
            let holder = std::fs::read_to_string(holder_info_path(data_dir))
                .map(|text| text.trim().to_string())
                .unwrap_or_default();
            if force {
                tracing::warn!(
                    lock = %path.display(),
                    holder = %holder,
                    "another process holds the serve lock; continuing unguarded per --force"
                );
                return Ok(None);
            }
            anyhow::bail!(
                "another ai-memory serve process appears to be using {} ({}); two servers on one data directory corrupt wiki and index state — stop the other process, or pass --force if you are certain it is gone",
                data_dir.display(),
                if holder.is_empty() {
                    "holder unknown".to_owned()
                } else {
                    holder
                },
            )
        }
        Err(err) => {
            tracing::warn!(
                lock = %path.display(),
                error = %err,
                "cannot lock the data directory; running without the single-instance guard"
            );
            Ok(None)
        }
    }
}

fn validate_api_credential_pepper(api_credentials_exist: bool, auth: &AuthSettings) -> Result<()> {
    let pepper_present = auth
        .token_pepper
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    if api_credentials_exist && !pepper_present {
        anyhow::bail!(
            "api credentials exist but [auth].token_pepper is missing or blank; restore the original pepper from configuration backup before serving"
        );
    }
    Ok(())
}

/// Bind native API-key hashes to the pepper that produced them.
///
/// The Argon2id verifier travels with SQLite backups without storing the
/// pepper. A different pepper is safe to bind while no credentials exist;
/// once hashes exist, a mismatch means the restored database and config do
/// not belong together and serving would silently reject every key.
async fn validate_api_credential_pepper_binding(
    store: &Store,
    api_credentials_exist: bool,
    auth: &AuthSettings,
) -> Result<()> {
    validate_api_credential_pepper(api_credentials_exist, auth)?;
    let Some(pepper) = auth
        .token_pepper
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(());
    };

    if let Some(verifier) = store.reader.api_credential_pepper_verifier().await? {
        let matches = ai_memory_store::password::verify_password(pepper.to_owned(), verifier)
            .await
            .context("verifying the configured API credential pepper")?;
        if matches {
            return Ok(());
        }
        if api_credentials_exist {
            anyhow::bail!(
                "[auth].token_pepper does not match the pepper that hashed the restored API \
                 credentials; restore the original external config or token_pepper before \
                 serving (ai-memory backup does not archive config files outside the data dir)"
            );
        }
    }

    let verifier = ai_memory_store::password::hash_password(pepper.to_owned())
        .await
        .context("binding the configured API credential pepper")?;
    let bound = store
        .writer
        .bind_api_credential_pepper_verifier(verifier)
        .await?;
    if !bound {
        anyhow::bail!(
            "API credentials were added while binding [auth].token_pepper; restart so the \
             server can verify the final credential state"
        );
    }
    Ok(())
}

/// Validate the credentials that keep an existing multi-user installation
/// closed. Bootstrap installs have no user rows yet and retain their historical
/// compatibility behavior regardless of placeholder auth values.
fn validate_existing_users_auth(
    users_exist: bool,
    api_credentials_exist: bool,
    human_mode: bool,
    auth: &AuthSettings,
) -> Result<()> {
    validate_api_credential_pepper(api_credentials_exist, auth)?;
    let pepper_present = auth
        .token_pepper
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    let bearer_present = auth
        .bearer_token
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    if users_exist && !human_mode {
        return match (pepper_present, bearer_present) {
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
        };
    }
    Ok(())
}

fn secret_configured(secret: Option<&secrecy::SecretString>) -> bool {
    secret.is_some_and(|s| !s.expose_secret().trim().is_empty())
}

fn parse_trusted_proxy_cidrs(auth: &AuthSettings) -> Result<Vec<Cidr>> {
    auth.trusted_proxy_cidrs
        .iter()
        .map(|spec| {
            Cidr::parse(spec).map_err(|e| {
                anyhow::anyhow!("invalid [auth].trusted_proxy_cidrs entry {spec:?}: {e}")
            })
        })
        .collect()
}

async fn maybe_bootstrap_root(store: &Store, auth: &AuthSettings) -> Result<()> {
    if store.reader.bootstrap_completed().await? {
        if secret_configured(auth.initial_root_password.as_ref()) {
            tracing::warn!(
                "[auth].initial_root_password is ignored because bootstrap already completed; unset AI_MEMORY_AUTH__INITIAL_ROOT_PASSWORD"
            );
        }
        return Ok(());
    }
    let Some(password) = auth
        .initial_root_password
        .as_ref()
        .map(|s| s.expose_secret().to_string())
        .filter(|s| !s.trim().is_empty())
    else {
        return Ok(());
    };
    let username = auth
        .root_username
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("root");
    let recovery = auth
        .recovery_token
        .as_ref()
        .map(ExposeSecret::expose_secret)
        .filter(|s| !s.trim().is_empty());
    let reserved: Vec<&str> = [
        auth.bearer_token
            .as_deref()
            .filter(|s| !s.trim().is_empty()),
        auth.actor_proxy_bearer_token
            .as_deref()
            .filter(|s| !s.trim().is_empty()),
        recovery,
    ]
    .into_iter()
    .flatten()
    .collect();
    ai_memory_core::validate_human_password(&password, Some(username), &reserved)
        .context("initial root password does not meet policy")?;
    if let Some(pepper) = auth
        .token_pepper
        .as_deref()
        .filter(|p| !p.trim().is_empty())
    {
        let hash = hash_token(&password, &TokenPepper::new(pepper.to_string()));
        if store.reader.token_hash_exists(hash).await? {
            anyhow::bail!(
                "[auth].initial_root_password collides with an existing API credential; choose a different password"
            );
        }
    }
    let phc = ai_memory_store::password::hash_password(password)
        .await
        .context("hashing initial root password")?;
    store
        .writer
        .bootstrap_root(
            username.to_string(),
            auth.root_name.clone(),
            auth.root_email.clone(),
            phc,
        )
        .await
        .context("bootstrapping root user")?;
    tracing::warn!(
        "root user bootstrapped; unset AI_MEMORY_AUTH__INITIAL_ROOT_PASSWORD so the plaintext is not kept in the process environment"
    );
    Ok(())
}

async fn validate_configured_secret_collisions(
    store: &Store,
    auth: &AuthSettings,
    include_initial_password: bool,
) -> Result<()> {
    let Some(pepper) = auth
        .token_pepper
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| TokenPepper::new(value.to_string()))
    else {
        return Ok(());
    };
    let mut secrets = Vec::new();
    if let Some(value) = auth
        .bearer_token
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        secrets.push(("[auth].bearer_token", value));
    }
    if let Some(value) = auth
        .actor_proxy_bearer_token
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        secrets.push(("[auth].actor_proxy_bearer_token", value));
    }
    if let Some(value) = auth
        .recovery_token
        .as_ref()
        .map(ExposeSecret::expose_secret)
        .filter(|value| !value.trim().is_empty())
    {
        secrets.push(("[auth].recovery_token", value));
    }
    if include_initial_password
        && let Some(value) = auth
            .initial_root_password
            .as_ref()
            .map(ExposeSecret::expose_secret)
            .filter(|value| !value.trim().is_empty())
    {
        secrets.push(("[auth].initial_root_password", value));
    }
    for (name, value) in secrets {
        if store
            .reader
            .token_hash_exists(hash_token(value, &pepper))
            .await?
        {
            anyhow::bail!(
                "{name} collides with an existing API credential; configure a distinct secret"
            );
        }
    }
    Ok(())
}

fn human_auth_intended(auth: &AuthSettings, bootstrap_completed: bool, any_password: bool) -> bool {
    bootstrap_completed
        || any_password
        || secret_configured(auth.initial_root_password.as_ref())
        || secret_configured(auth.recovery_token.as_ref())
}

fn require_recoverable_root_or_recovery(
    human_mode: bool,
    recoverable_roots: i64,
    recovery_configured: bool,
) -> Result<()> {
    if human_mode && recoverable_roots == 0 && !recovery_configured {
        anyhow::bail!(
            "human authentication is enabled but no recoverable root user exists; set [auth].recovery_token to rebuild root via POST /auth/recovery"
        );
    }
    Ok(())
}

fn reserved_human_passwords(auth: &AuthSettings) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(v) = auth
        .bearer_token
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        out.push(v.to_string());
    }
    if let Some(v) = auth
        .actor_proxy_bearer_token
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        out.push(v.to_string());
    }
    out
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
    /// Human passwords and cookies are crossing a non-loopback plain-HTTP
    /// connection because the operator accepted that transport risk.
    InsecureHumanAuthByOverride,
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
    human_mode: bool,
    secure_cookie: bool,
    allow_insecure_no_auth: bool,
    allow_insecure_human_auth: bool,
    containerized: bool,
) -> Result<HttpExposure> {
    if local_addr.ip().is_loopback() {
        return Ok(HttpExposure::Safe);
    }
    if human_mode && !secure_cookie {
        if auth_enabled && allow_insecure_human_auth {
            return Ok(HttpExposure::InsecureHumanAuthByOverride);
        }
        anyhow::bail!(
            "refusing human authentication on non-loopback plain HTTP address {local_addr}: \
             passwords and session cookies require [auth].secure_cookie=true behind a trusted \
             HTTPS reverse proxy. Non-Secure human cookies are allowed only on loopback unless \
             --allow-insecure-human-auth explicitly accepts plaintext transport."
        );
    }
    if auth_enabled {
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

/// The operator-facing banner for an exposure that is not [`HttpExposure::Safe`].
///
/// Returned as text rather than printed so a test can assert it, and printed at
/// the call site with `eprintln!` rather than only `tracing::warn!`: the tracing
/// stderr layer sits behind `EnvFilter`, so `RUST_LOG=error` — or
/// `log_level = "error"` in the config, which feeds the same filter — silences a
/// warning whose whole job is to say the server is reachable from the network
/// without a credential. A security notice a log level can switch off is not a
/// notice. The structured `tracing::warn!` stays for log collectors.
fn exposure_banner(exposure: HttpExposure, local_addr: SocketAddr) -> Option<String> {
    match exposure {
        HttpExposure::Safe => None,
        HttpExposure::InsecureByOverride => Some(format!(
            "WARNING: ai-memory is listening on {local_addr} with NO AUTHENTICATION because \
             --allow-insecure-no-auth was supplied. Anyone who can reach this address can call \
             destructive MCP tools."
        )),
        HttpExposure::InsecureHumanAuthByOverride => Some(format!(
            "WARNING: ai-memory is serving HUMAN AUTHENTICATION on plain HTTP at {local_addr} \
             because --allow-insecure-human-auth was supplied. Passwords, API credentials, \
             session cookies, and memory data are unencrypted; a network peer can steal or \
             modify them. Use this only on a network whose plaintext risk you explicitly accept."
        )),
        HttpExposure::UndeterminedInContainer => Some(format!(
            "WARNING: ai-memory is listening on {local_addr} with NO AUTHENTICATION. Inside a \
             container the bind address cannot show whether this port reaches the network - the \
             host publish spec decides. Published with `-p 127.0.0.1:49374:49374` you are fine; \
             published on 0.0.0.0 or a LAN address, anyone on the network can call destructive \
             MCP tools. Run `ai-memory generate-auth-token` and set AI_MEMORY_AUTH_TOKEN."
        )),
    }
}

/// A configured `[auth].bearer_token_file` that holds no token must stop the
/// server: starting without one would serve every route unauthenticated, the
/// opposite of what the key asks for.
fn refuse_a_missing_token_file(auth: &AuthSettings) -> Result<()> {
    if let Some(path) = auth.missing_token_file() {
        anyhow::bail!(
            "[auth].bearer_token_file names {}, which is missing or empty; refusing to serve \
             without the bearer token it should hold. Run `ai-memory auth enable` to create a \
             new one, or restore it from your separate secret backup (`ai-memory backup` \
             intentionally excludes auth token files).",
            path.display()
        );
    }
    Ok(())
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

fn mount_machine_routes(
    machine: axum::Router,
    reader: ai_memory_store::ReaderPool,
    auth: Arc<AuthState>,
) -> axum::Router {
    machine
        .merge(ai_memory_mcp::identity::router(
            reader,
            auth.actor_proxy_bearer().is_some(),
        ))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn_with_state(auth, require_bearer))
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

/// Failures that must terminate a SessionEnd queue job: re-sending the same
/// expensive prompt cannot clear them.
///
/// An LLM failure defers to [`ai_memory_llm::LlmError::is_transient`], which
/// is the same predicate the two in-process consolidation retry loops already
/// gate on — `complete_structured_with_retry` in `ai-memory-consolidate`'s
/// `consolidator.rs` and the bootstrap chunk loop in its `bootstrap.rs`. One
/// definition of "worth another expensive call" then covers all three retry
/// sites: a provider `429` or `5xx` and an `Http` timeout/connect failure
/// never reached the model, so they keep the queue's bounded,
/// backoff-scheduled retry up to the attempt cap, while everything else — a
/// malformed (`Serde`) or unexpected (`UnexpectedShape`) structured response,
/// an output-budget stop (`TruncatedResponse`), empty content
/// (`EmptyContent`), a rejected or expired credential (`Auth`), a missing
/// provider configuration (`NotConfigured`), a schema failure (`Schema`), a
/// malformed-request `4xx` — reproduces on identical inputs, so the job goes
/// terminal and the heuristic page the SessionEnd hook already wrote remains.
///
/// A `ConsolidatorError::Serde` is not an `LlmError`: it is this crate's own
/// JSON failure on an unchanged input and stays terminal on its own.
fn is_terminal_session_consolidation_error(
    error: &ai_memory_consolidate::ConsolidatorError,
) -> bool {
    match error {
        ai_memory_consolidate::ConsolidatorError::Llm(llm) => !llm.is_transient(),
        ai_memory_consolidate::ConsolidatorError::Serde(_) => true,
        _ => false,
    }
}

/// Delay before the first profile pass, so it never competes with migration
/// and first-request work on boot.
const PROFILE_PASS_STARTUP_DELAY: Duration = Duration::from_secs(120);
/// Wait after a SessionEnd wake-up before the pass runs, so a burst of
/// session ends costs one pass.
const PROFILE_PASS_DEBOUNCE: Duration = Duration::from_secs(30);
/// Cadence of the profile pass when no session ends wake it.
const PROFILE_PASS_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// What the cross-project profile worker needs.
struct ProfileWorker {
    reader: ai_memory_store::ReaderPool,
    writer: WriterHandle,
    wiki: Wiki,
    llm: Option<Arc<dyn LlmProvider>>,
    settings: ai_memory_core::profile::ProfileSettings,
    trusted_proxy_identity: bool,
}

/// Harvest and converge the cross-project profile
/// (`docs/design-cross-project-profile.md`): once shortly after startup, then
/// after every SessionEnd (debounced) and hourly. Each pass reads only what is
/// new past the per-project marks. The deployment's operator topology is read
/// per pass, so adding the first database user turns an `auto` profile off
/// without a restart.
async fn run_profile_worker(
    worker: ProfileWorker,
    notify: Arc<tokio::sync::Notify>,
    cancel: CancellationToken,
) {
    tokio::select! {
        () = cancel.cancelled() => return,
        () = tokio::time::sleep(PROFILE_PASS_STARTUP_DELAY) => {},
    }
    loop {
        let distinguishes = match worker
            .reader
            .distinguishes_operators(worker.trusted_proxy_identity)
            .await
        {
            Ok(distinguishes) => distinguishes,
            Err(error) => {
                tracing::warn!(%error, "profile pass could not read the operator topology");
                true
            }
        };
        if worker.settings.effective_share(distinguishes).is_some() {
            let config = ai_memory_consolidate::profile::ProfilePassConfig {
                settings: worker.settings.clone(),
                distinguishes_operators: distinguishes,
            };
            let started = std::time::Instant::now();
            match ai_memory_consolidate::profile::run_profile_pass(
                &worker.reader,
                &worker.writer,
                &worker.wiki,
                worker.llm.as_ref(),
                &config,
            )
            .await
            {
                Ok(report) if report.candidates_added > 0 || report.entries_written > 0 => info!(
                    share = report.share,
                    projects = report.projects_harvested,
                    candidates = report.candidates_added,
                    written = report.entries_written,
                    skipped_manual = report.skipped_manual.len(),
                    llm_calls = report.llm_calls,
                    llm_fallbacks = report.llm_fallbacks,
                    errors = report.errors.len(),
                    elapsed_ms = started.elapsed().as_millis(),
                    "profile pass updated the cross-project profile"
                ),
                Ok(report) if !report.errors.is_empty() => tracing::warn!(
                    errors = ?report.errors,
                    "profile pass finished with errors"
                ),
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "profile pass failed"),
            }
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = notify.notified() => {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(PROFILE_PASS_DEBOUNCE) => {},
                }
            }
            () = tokio::time::sleep(PROFILE_PASS_INTERVAL) => {},
        }
    }
}

async fn session_consolidation_actor(
    reader: &ReaderPool,
    session_id: ai_memory_core::SessionId,
) -> ai_memory_core::ActorContext {
    match reader.session_actor_user(session_id).await {
        Ok(Some(owner)) => IdentityKey::from_storage_key(&owner)
            .map(|key| key.to_actor_context())
            .unwrap_or_else(|| {
                tracing::warn!(
                    session = %session_id,
                    "session owner is not a valid identity key; consolidating as anonymous"
                );
                ai_memory_core::ActorContext::anonymous()
            }),
        Ok(None) => ai_memory_core::ActorContext::anonymous(),
        Err(error) => {
            tracing::warn!(
                %error,
                session = %session_id,
                "could not read session owner; consolidating as anonymous"
            );
            ai_memory_core::ActorContext::anonymous()
        }
    }
}

async fn run_session_consolidation_worker(
    writer: WriterHandle,
    reader: ReaderPool,
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
                tracing::warn!(%error, "session consolidation queue claim failed");
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
        // An agent may have written the session page with its own model
        // while the session was still open, before this job existed. Leave
        // that page alone: the agent's own tool call, the Stop and the
        // SessionEnd always land after it, so no observation count can tell
        // a stale page from a current one.
        match consolidator.session_page_written_by_agent(session_id).await {
            Ok(true) => {
                match writer.complete_session_consolidation(job).await {
                    Ok(()) => info!(
                        session = %session_id,
                        generation,
                        "session page was written by the agent; queued consolidation skipped",
                    ),
                    Err(error) => tracing::warn!(
                        %error,
                        session = %session_id,
                        generation,
                        "session consolidation skip could not complete the queue job",
                    ),
                }
                #[cfg(test)]
                completed.notify_one();
                continue;
            }
            Ok(false) => {}
            Err(error) => tracing::warn!(
                %error,
                session = %session_id,
                generation,
                "could not read the session page before queued consolidation; consolidating",
            ),
        }
        let actor = session_consolidation_actor(&reader, session_id).await;
        let consolidation = consolidator.consolidate_session(session_id, false, actor, None, None);
        tokio::pin!(consolidation);
        let result = tokio::select! {
            () = cancel.cancelled() => {
                if let Err(error) = writer.release_session_consolidation(job).await {
                    tracing::warn!(%error, %session_id, generation, "failed to release session consolidation during shutdown");
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
                        "queued LLM consolidation written",
                    );
                    #[cfg(test)]
                    completed.notify_one();
                }
                Err(error) => tracing::warn!(
                    %error,
                    session = %session_id,
                    generation,
                    "session consolidation succeeded but queue completion failed",
                ),
            },
            Err(error) => {
                let retry_at = if is_terminal_session_consolidation_error(&error)
                    || attempts >= ai_memory_store::SESSION_CONSOLIDATION_MAX_ATTEMPTS
                {
                    None
                } else {
                    let delay = session_consolidation_retry_delay(attempts);
                    let delay_micros = i64::try_from(delay.as_micros()).unwrap_or(i64::MAX);
                    Some(
                        jiff::Timestamp::now()
                            .as_microsecond()
                            .saturating_add(delay_micros),
                    )
                };
                let terminal = retry_at.is_none();
                if let Err(store_error) = writer
                    .fail_session_consolidation(
                        job,
                        ai_memory_consolidate::redacted_error_summary(&error),
                        retry_at,
                    )
                    .await
                {
                    tracing::warn!(
                        error = %store_error,
                        session = %session_id,
                        generation,
                        "failed to persist session consolidation failure",
                    );
                } else if terminal {
                    tracing::error!(
                        error_summary = %ai_memory_consolidate::redacted_error_summary(&error),
                        session = %session_id,
                        generation,
                        attempts,
                        "queued LLM consolidation exhausted retries; heuristic page remains",
                    );
                } else {
                    tracing::warn!(
                        error_summary = %ai_memory_consolidate::redacted_error_summary(&error),
                        session = %session_id,
                        generation,
                        attempts,
                        "queued LLM consolidation failed; queued for retry",
                    );
                }
            }
        }
    }
}

/// Wraps [`tokio::net::TcpListener`] to enable TCP keepalive on every
/// accepted connection.
///
/// Without this, a hook client whose peer dies without sending FIN (laptop
/// sleep, a VPN/Tailscale flap, an abrupt kill) leaves its socket
/// `ESTABLISHED` forever: the OS default is keepalive off, so the fd is
/// never reclaimed. Over days that leaks one fd per dead peer until
/// `accept()` starts failing with `EMFILE` and the healthcheck breaks (#792).
/// Keepalive makes the kernel probe idle connections and close ones whose
/// peer no longer answers.
///
/// This is built on axum's own [`axum::serve::ListenerExt::tap_io`] rather
/// than a hand-rolled `impl axum::serve::Listener`. A hand-rolled newtype
/// was tried first: it compiles as a `Listener`, but
/// `into_make_service_with_connect_info::<SocketAddr>()` additionally needs
/// `SocketAddr: Connected<IncomingStream<'_, L>>`, and axum only ships that
/// impl for its own `TcpListener` and for `TapIo<L, F>` (generically, for any
/// `L: Listener`) — never for an arbitrary third-party `L`. Implementing
/// `Connected` ourselves is blocked by the orphan rule: neither `Connected`,
/// `SocketAddr`, nor `IncomingStream` (a plain, non-fundamental axum type) is
/// local to this crate. `tap_io` is the extension point axum actually
/// provides for exactly this "touch every accepted `Io`" case, and it keeps
/// `ConnectInfo` (real peer `SocketAddr`) working for free.
fn keepalive_listener(
    listener: tokio::net::TcpListener,
    keepalive_secs: u64,
) -> axum::serve::TapIo<
    tokio::net::TcpListener,
    impl FnMut(&mut tokio::net::TcpStream) + Send + 'static,
> {
    // `None` when `tcp_keepalive_secs = 0` (keepalive disabled) — pass
    // accepted sockets through unmodified.
    let keepalive = (keepalive_secs > 0).then(|| {
        let idle = Duration::from_secs(keepalive_secs);
        socket2::TcpKeepalive::new()
            .with_time(idle)
            .with_interval(idle)
    });
    axum::serve::ListenerExt::tap_io(listener, move |stream: &mut tokio::net::TcpStream| {
        let Some(keepalive) = keepalive.as_ref() else {
            return;
        };
        let sock_ref = socket2::SockRef::from(&*stream);
        if let Err(error) = sock_ref.set_tcp_keepalive(keepalive) {
            // Guard, don't panic (runtime paths never unwrap/expect): a
            // platform or socket-state quirk here should not take down the
            // connection, just leave it without the reaping this wrapper
            // exists to provide.
            tracing::warn!(%error, "failed to set TCP keepalive on accepted connection");
        }
    })
}

/// Run the `serve` subcommand.
///
/// # Errors
/// Returns an error if the store cannot be opened, the watcher cannot
/// install, or the transport setup fails.
pub async fn run(config: &Config, args: ServeArgs) -> Result<()> {
    // Before anything slow: boot takes the pre-migration archive and runs the
    // wiki migrations, and a signal arriving in that window has to be caught
    // rather than fall through to the default disposition (#699).
    let mut shutdown = ShutdownSignals::install();

    validate_web_ui_args(args.enable_web, args.web_ui_dir.as_deref())?;
    #[cfg(feature = "management-ui")]
    if args.enable_management_ui && !matches!(args.transport, TransportKind::Http) {
        anyhow::bail!("--enable-management-ui requires --transport http");
    }

    #[cfg(feature = "management-ui")]
    if args.enable_management_ui && args.enable_web && normalize_prefix(&args.web_slug) == "/manage"
    {
        anyhow::bail!("the management console reserves /manage; choose a different --web-slug");
    }

    refuse_a_missing_token_file(&config.auth)?;
    if args.enable_session_management && !matches!(args.transport, TransportKind::Http) {
        anyhow::bail!("--enable-session-management requires --transport http");
    }
    if args.enable_self_service_keys && !matches!(args.transport, TransportKind::Http) {
        anyhow::bail!("--enable-self-service-keys requires --transport http");
    }
    config.require_llm_fallback_credentials()?;

    // Merge config + CLI CORS origins (config first, CLI adds new entries).
    // Validation runs before binding so a misconfigured origin is caught early.
    let cors_origins = merge_cors_origins(&config.cors_allow_origins, &args.cors_allow_origin);
    validate_cors_origins(&cors_origins)?;

    // Guard the data dir before anything opens it: a second serve process
    // means a second writer actor, a second git handle on the wiki, and a
    // second active-project pointer (#563). Held until `run` returns.
    let _serve_lock = acquire_serve_lock(&config.data_dir, args.force)?;

    // #633: take the pre-migration safety archive BEFORE `Store::open` advances
    // the SQLite schema, so the archive captures the true pre-2.0 state a 1.x
    // binary can reopen. `Store::open` below runs the DB migrations; if the
    // backup ran after it (as it used to, inside the wiki migration), the
    // archived `db/` would already be at the 2.x schema and the documented
    // rollback to 1.x would be impossible. A no-op for a fresh install or an
    // already-migrated store; the OKF wiki migration reuses this archive instead
    // of taking a second, post-DB-migration one.
    let backup_dest_override = std::env::var("AI_MEMORY_BACKUP_DIR")
        .ok()
        .map(std::path::PathBuf::from);
    migrations::snapshot_before_db_migration(&config.data_dir, backup_dest_override.as_deref())
        .with_context(|| "taking the pre-migration safety backup")?;

    let mut store = Store::open(&config.data_dir)
        .with_context(|| format!("opening store at {}", config.data_dir.display()))?;
    // Every reader handle below is cloned from this one, so the opt-in
    // ranking signals are set once, here, and inherited everywhere.
    store.reader.set_retrieval_tuning(config.retrieval.tuning());
    // Same reasoning for the FTS stopword list (issue #953): resolved once
    // from `[search.fts]` here, then shared by every cloned reader handle.
    store
        .reader
        .set_fts_stopwords(config.search.fts.stopwords());
    let store = store;

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

    let api_credentials_exist = store.reader.api_credentials_exist().await?;
    validate_api_credential_pepper_binding(&store, api_credentials_exist, &config.auth).await?;
    let bootstrap_completed_before_start = store.reader.bootstrap_completed().await?;
    validate_configured_secret_collisions(&store, &config.auth, !bootstrap_completed_before_start)
        .await?;
    maybe_bootstrap_root(&store, &config.auth).await?;
    let bootstrap_completed = store.reader.bootstrap_completed().await?;
    let any_password = store.reader.any_password_hash().await?;
    let human_mode = human_auth_intended(&config.auth, bootstrap_completed, any_password);
    require_recoverable_root_or_recovery(
        human_mode,
        store.reader.count_recoverable_roots().await?,
        secret_configured(config.auth.recovery_token.as_ref()),
    )?;
    validate_existing_users_auth(
        store.reader.users_exist().await?,
        api_credentials_exist,
        human_mode,
        &config.auth,
    )?;
    validate_trusted_proxy_auth(&config.auth)?;
    let trusted_proxy_cidrs = parse_trusted_proxy_cidrs(&config.auth)?;

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
        .with_store_reader(store.reader.clone())
        // `[maintenance] reconcile_tombstones_deleted_pages` (#929), read once
        // by `Config::load` above and threaded through here — the one place
        // this flag is consulted outside `Config`, per invariant #1.
        .with_reconcile_tombstones_deleted_pages(
            config.maintenance.reconcile_tombstones_deleted_pages,
        );
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
    // Pages conformed by an older build carry a date-only OKF `stale_after`
    // copied from `expires_at`; repair them in place. Idempotent; non-fatal.
    match wiki.repair_date_only_stale_after().await {
        Ok((0, 0)) => {}
        Ok((rows, files)) => tracing::info!(
            rows,
            files,
            "repaired date-only OKF stale_after on existing pages"
        ),
        Err(e) => tracing::warn!(error = %e, "OKF stale_after repair failed (non-fatal)"),
    }
    let baseline_checkpoint = wiki.ensure_upgrade_baseline_checkpoint();
    match classify_baseline_checkpoint(&baseline_checkpoint) {
        BaselineCheckpointLog::Created => {
            if let Ok(Some(oid)) = &baseline_checkpoint {
                tracing::info!(checkpoint = %oid, "created wiki upgrade baseline checkpoint");
            }
        }
        BaselineCheckpointLog::Clean => {}
        // An owner-check failure is silent-but-fatal to the wiki git history:
        // capture keeps working, but no checkpoint is ever committed, so it
        // must not hide in a WARN. This is the Windows LocalSystem-service /
        // user-owned-data-dir case (docs/windows.md Scenario E).
        BaselineCheckpointLog::OwnerFailure => tracing::error!(
            error = %baseline_checkpoint.as_ref().err().map(ToString::to_string).unwrap_or_default(),
            "wiki git commits are failing libgit2's owner check: the wiki repository is not \
             owned by the account running ai-memory. Run the service AS THE OWNING USER (see \
             docs/windows.md Scenario E) — capture continues, but no wiki checkpoints will be \
             committed until this is fixed."
        ),
        BaselineCheckpointLog::OtherFailure => {
            if let Err(e) = &baseline_checkpoint {
                tracing::warn!(error = %e, "wiki upgrade baseline checkpoint failed (non-fatal)");
            }
        }
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
    // TTL/cap. `per_actor` (default) and `per_session` keyed-isolate
    // concurrent sessions / operators; `single` preserves the pre-v1.39
    // process-wide slot.
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
        .with_ledger_retention(config.decay.ledger_retention())
        .with_compact_cold_episodic(config.decay.compact_cold_episodic)
        .with_contradiction_band(config.contradiction_band_min, config.contradiction_band_max)
        .with_auto_improve_require_approval(config.auto_improve.require_approval)
        .with_auto_improve_review_config(auto_improve_review_config_from_settings(
            &config.auto_improve,
        ))
        .with_active_project(active_project.clone())
        .with_sanitizer(sanitizer.clone())
        .with_trusted_proxy_identity(trusted_proxy_identity_enabled(&config.auth))
        .with_per_user_slots(config.slots.per_user)
        .with_profile(config.profile.clone())
        .with_answer_context_budget(config.query_answer.max_context_tokens)
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
    let llm_configured = admin_llm.is_some();
    let profile_llm = admin_llm.clone();
    // Share the tool router's last-activity clock with the B3 dream scheduler so
    // it can tell an idle box from a busy one and cancel a run on the operator's
    // return.
    let activity_clock = server.activity_clock();
    let _maintenance_tasks = start_maintenance_scheduler(
        config.maintenance.clone(),
        config.auto_improve.clone(),
        store.reader.clone(),
        store.writer.clone(),
        wiki.clone(),
        embedder.clone(),
        admin_llm.clone(),
        config.decay,
        config.sessions,
        config.dream,
        activity_clock,
        config.contradiction_band_min,
        config.contradiction_band_max,
        config.consolidate_on_session_end,
        consolidator.is_some(),
    )
    .await;

    match args.transport {
        TransportKind::Stdio => {
            info!("MCP server ready on stdio (Ctrl-C to stop)");
            // `serve` resolves only once a client has completed the MCP
            // `initialize` handshake, so the signal races the handshake as
            // well as the session that follows it: a Ctrl-C before any client
            // connected is the exact state a launched-but-unused server sits
            // in, and until #699 nothing here listened for one at all.
            let service = tokio::select! {
                // A handshake that completes in the same poll a signal
                // arrives in counts as connected: an unbiased select would
                // pick either branch at random.
                biased;
                service = server.serve(stdio()) => Some(service?),
                signal = shutdown.recv() => {
                    info!(signal, "shutdown signal received before a client connected; stopping");
                    None
                }
            };
            if let Some(service) = service {
                // Take the token before `waiting` consumes the service:
                // stopping the transport is the only way out of that await.
                let stop = service.cancellation_token();
                let mut waiting = std::pin::pin!(service.waiting());
                let signal = tokio::select! {
                    result = &mut waiting => {
                        result?;
                        None
                    }
                    signal = shutdown.recv() => Some(signal),
                };
                if let Some(signal) = signal {
                    info!(
                        signal,
                        "shutdown signal received; stopping the stdio transport"
                    );
                    stop.cancel();
                    // A signal is a normal stop, so nothing below turns it
                    // into a failing exit — but neither does it wait forever.
                    match tokio::time::timeout(SHUTDOWN_GRACE, waiting).await {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => {
                            tracing::warn!(%error, "stdio transport ended abnormally during shutdown");
                        }
                        Err(_) => tracing::warn!(
                            grace_secs = SHUTDOWN_GRACE.as_secs(),
                            "stdio transport did not stop within the shutdown grace period; exiting anyway"
                        ),
                    }
                }
            }
            close_store(&store).await;
        }
        TransportKind::Http => {
            seed_active_project_fallback(&store.reader, &active_project).await;
            let bind = args.bind.unwrap_or_else(|| config.bind.clone());
            let cancel = CancellationToken::new();
            let profile_notify = Arc::new(tokio::sync::Notify::new());
            let profile_task = tokio::spawn(run_profile_worker(
                ProfileWorker {
                    reader: store.reader.clone(),
                    writer: store.writer.clone(),
                    wiki: wiki.clone(),
                    llm: profile_llm.clone(),
                    settings: config.profile.clone(),
                    trusted_proxy_identity: trusted_proxy_identity_enabled(&config.auth),
                },
                profile_notify.clone(),
                cancel.child_token(),
            ));
            let (session_consolidation_notify, session_consolidation_task) =
                if let Some(consolidator) = consolidator.clone() {
                    let notify = Arc::new(tokio::sync::Notify::new());
                    let task = tokio::spawn(run_session_consolidation_worker(
                        store.writer.clone(),
                        store.reader.clone(),
                        consolidator,
                        notify.clone(),
                        cancel.child_token(),
                        #[cfg(test)]
                        Arc::new(tokio::sync::Notify::new()),
                    ));
                    info!(
                        max_attempts = ai_memory_store::SESSION_CONSOLIDATION_MAX_ATTEMPTS,
                        "durable session consolidation worker started"
                    );
                    (Some(notify), Some(task))
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
                cwd_probes: ai_memory_hooks::CwdProbes::default(),
                consolidate_on_session_end: config.consolidate_on_session_end,
                session_consolidation_notify,
                profile_notify: Some(profile_notify.clone()),
                capture_assistant_enabled: config.capture_assistant,
                claim_handoff_on_session_start: config.handoff.claim_on_session_start,
                create_handoff_on_session_end: config.handoff.create_on_session_end,
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
                profile: config.profile.clone(),
                ingest_drop_list: (*config.ingest.drop_list).clone(),
                record_tool_names: config.ingest.record_tool_names,
                // Benchmark-only escape (docs/benchmarks/README.md), read once
                // here so the hooks crate keeps making no environment reads.
                skip_wiki_checkpoints: benchmark_skip_wiki_checkpoints(),
            });
            let workstreams = workstream_router(WorkstreamState {
                writer: store.writer.clone(),
                reader: store.reader.clone(),
                sanitizer: sanitizer.clone(),
                wiki: wiki.clone(),
                data_dir: config.data_dir.clone(),
                home_dir: config.home_dir.clone(),
                trusted_proxy_identity: trusted_proxy_identity_enabled(&config.auth),
            });
            let admin = admin_router_with_sweep_tuning(
                AdminState {
                    ingest_metrics: ingest_metrics.clone(),
                    // The tool server's own instance, so /admin/metrics reads
                    // what memory_query records.
                    mcp_metrics: server.mcp_metrics(),
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
                    contradiction_band_min: config.contradiction_band_min,
                    contradiction_band_max: config.contradiction_band_max,
                    data_dir: config.data_dir.clone(),
                    db_path: store.db_path().to_path_buf(),
                    bind: bind.clone(),
                    home_dir: config.home_dir.clone(),
                    bootstrap_gate: ai_memory_mcp::BootstrapGate::new(
                        config.bootstrap_max_concurrent,
                    ),
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
                config.decay.ledger_retention(),
                config.decay.compact_cold_episodic,
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
            store
                .writer
                .set_new_project_mode(if config.auth.new_projects_restricted {
                    ai_memory_store::AccessMode::Restricted
                } else {
                    ai_memory_store::AccessMode::Open
                })
                .await?;
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
            let recovery_token_hash = config
                .auth
                .recovery_token
                .as_ref()
                .map(ExposeSecret::expose_secret)
                .filter(|s| !s.trim().is_empty())
                .map(hash_session_secret);
            let root_username = config
                .auth
                .root_username
                .clone()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "root".to_string());
            auth_state = auth_state.with_human_runtime(
                HumanAuthRuntime {
                    reader: store.reader.clone(),
                    writer: store.writer.clone(),
                    recovery_token_hash,
                    root_username,
                    root_name: config.auth.root_name.clone(),
                    root_email: config.auth.root_email.clone(),
                    reserved_passwords: reserved_human_passwords(&config.auth),
                    trusted_proxy_cidrs: trusted_proxy_cidrs.clone(),
                    limiter: Arc::new(LoginLimiter::default()),
                },
                human_mode,
            );
            let auth_state = Arc::new(auth_state);
            let auth_enabled = auth_state.enabled();
            let machine = mount_machine_routes(
                axum::Router::new()
                    .nest_service("/mcp", mcp_service)
                    .merge(hooks)
                    .merge(workstreams),
                store.reader.clone(),
                auth_state.clone(),
            );
            let admin = admin
                .merge(ai_memory_mcp::admin_profile::profile_admin_router(
                    ai_memory_mcp::admin_profile::ProfileAdminState {
                        reader: store.reader.clone(),
                        writer: store.writer.clone(),
                        wiki: wiki.clone(),
                        llm: profile_llm.clone(),
                        profile: config.profile.clone(),
                        trusted_proxy_identity: trusted_proxy_identity_enabled(&config.auth),
                    },
                ))
                .layer(DefaultBodyLimit::max(BOOTSTRAP_MAX_BODY_BYTES))
                .layer(axum::middleware::from_fn_with_state(
                    auth_state.clone(),
                    require_dual_auth,
                ));
            let configured_base_path = args.base_path.as_deref().unwrap_or(&config.base_path);
            let base_path = normalize_prefix(configured_base_path);
            if base_path.is_empty() && !configured_base_path.trim_matches('/').trim().is_empty() {
                tracing::warn!(
                    raw = %configured_base_path,
                    "configured base path is not a safe path prefix; serving at root instead",
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
            let base_href = web_base_href(configured_base_path, &args.web_slug);
            let web = split_web_routers_with_metrics(
                args.enable_web,
                store.reader.clone(),
                wiki.clone(),
                Some(ingest_metrics.clone()),
                WebMountSpec {
                    enable_api: args.enable_api,
                    web_ui_dir: args.web_ui_dir.as_deref(),
                    cors_origins: &cors_origins,
                    web_slug: &args.web_slug,
                    base_href: &base_href,
                    base_path: &base_path,
                    trusted_proxy_identity: trusted_proxy_identity_enabled(&config.auth),
                    // The same embedder and rerank stage `memory_query`
                    // holds (the stage shares its concurrency gate), so web
                    // project search ranks like the MCP tool.
                    recall: ai_memory_web::Recall {
                        embedder: embedder.clone(),
                        rerank: server.rerank_stage(),
                    },
                    runtime: Some(ai_memory_web::features::RuntimeFeatures {
                        llm_configured,
                        consolidate_on_session_end: config.consolidate_on_session_end,
                        capture_assistant_default: config.capture_assistant,
                        backfill_on_start: config.backfill_on_start,
                        learning_scheduled: config.auto_improve.scheduler.enabled
                            && config.auto_improve.scheduler.interval_secs > 0
                            && config.auto_improve.scheduler.max_sessions_per_tick > 0,
                        learning_on_session_end: config.auto_improve.on_session_end,
                        review_requires_approval: config.auto_improve.require_approval,
                        maintenance_enabled: config.maintenance.enabled,
                        retention_scheduled: config.maintenance.forget_sweep_interval_secs > 0,
                        human_login: human_mode,
                        observation_stream: config.retrieval.observation_stream,
                    }),
                    health: Some(Arc::new(ai_memory_web::health::HealthSources {
                        ingest: ingest_metrics.clone(),
                        providers: provider_health.clone(),
                        probe: Arc::new(ServeHealthProbe {
                            writer: store.writer.clone(),
                            data_dir: config.data_dir.clone(),
                        }),
                    })),
                },
            )?;
            // HTML navigational 401/403 → builtin login / change-password.
            // Outer layer so it sees dual-auth responses; `/api/v1` stays JSON
            // (`html_auth_redirect_mw` skips that prefix). Builtin wiki returns
            // the cfg from split; custom SPA / web-off still builds one so the
            // layer always has concrete login/change-password targets.
            let html_auth = web.html_auth.unwrap_or_else(|| {
                Arc::new(HtmlAuthRedirectConfig::from_mount(
                    &base_path,
                    &args.web_slug,
                ))
            });
            let router = refuse_cross_origin_browser_requests(machine.merge(admin), auth_enabled)
                .merge(healthz_router())
                .merge(public_auth_router(auth_state.clone()))
                .merge(session_auth_router(auth_state.clone()))
                .merge(internal_auth_router(auth_state.clone()))
                .merge(
                    web.protected
                        .layer(axum::middleware::from_fn_with_state(
                            auth_state.clone(),
                            require_dual_auth,
                        ))
                        .layer(axum::middleware::from_fn_with_state(
                            html_auth,
                            html_auth_redirect_mw,
                        )),
                )
                .merge(web.public.layer(axum::middleware::from_fn_with_state(
                    auth_state.clone(),
                    expire_legacy_cookie_mw,
                )));
            #[cfg(not(feature = "management-ui"))]
            let management_ui_enabled = false;
            #[cfg(feature = "management-ui")]
            let management_ui_enabled = args.enable_management_ui;
            let router = if args.enable_session_management || management_ui_enabled {
                router.merge(ai_memory_mcp::session_management::router(
                    ai_memory_mcp::session_management::SessionManagementState {
                        reader: store.reader.clone(),
                        writer: store.writer.clone(),
                        wiki: wiki.clone(),
                    },
                    auth_state.clone(),
                ))
            } else {
                router
            };
            let router = if args.enable_self_service_keys || management_ui_enabled {
                router.merge(ai_memory_mcp::self_service_keys::router(
                    ai_memory_mcp::self_service_keys::SelfServiceKeysState {
                        reader: store.reader.clone(),
                        writer: store.writer.clone(),
                        pepper: config
                            .auth
                            .token_pepper
                            .clone()
                            .filter(|p| !p.trim().is_empty())
                            .map(TokenPepper::new),
                    },
                    auth_state.clone(),
                ))
            } else {
                router
            };
            #[cfg(feature = "management-ui")]
            let router = if management_ui_enabled {
                // Delegate forms to the same already-authenticated engine routers.
                let backend = router.clone();
                router.merge(ai_memory_web::management::router(&base_path, backend))
            } else {
                router
            };
            let router = apply_host_layer(router, config.allowed_hosts.clone());
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
                human_mode,
                config.auth.secure_cookie,
                args.allow_insecure_no_auth,
                args.allow_insecure_human_auth,
                running_in_container(),
            )?;
            info!(
                %local_addr,
                auth = auth_enabled,
                body_limit_mb = MAX_BODY_BYTES / 1024 / 1024,
                "MCP HTTP server ready (POST /mcp, POST /hook, Ctrl-C to stop)",
            );
            // Unconditional: see `exposure_banner` for why this does not ride
            // on the tracing filter.
            if let Some(banner) = exposure_banner(exposure, local_addr) {
                eprintln!("{banner}");
            }
            if exposure == HttpExposure::InsecureByOverride {
                tracing::warn!(
                    %local_addr,
                    "starting unauthenticated plain HTTP on a non-loopback address because \
                     --allow-insecure-no-auth was supplied — anyone on the network can call \
                     destructive MCP tools"
                );
            } else if exposure == HttpExposure::InsecureHumanAuthByOverride {
                tracing::warn!(
                    %local_addr,
                    "starting human authentication on non-loopback plain HTTP because \
                     --allow-insecure-human-auth was supplied — passwords, API credentials, \
                     session cookies, and memory data cross the network unencrypted"
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
                // the network on plain HTTP. Machine bearer credentials
                // ride cleartext — sniffable on the LAN. Advise the
                // operator to front with a TLS proxy. One-shot log at
                // startup, not refusal to serve (operators may be testing,
                // behind their own proxy already, etc.).
                tracing::warn!(
                    %local_addr,
                    "authentication is enabled but the server is bound to a \
                     non-loopback address on plain HTTP — bearer credentials travel \
                     cleartext on the network. Front ai-memory with a TLS-terminating \
                     reverse proxy (Caddy, Cloudflare Tunnel, nginx). See \
                     docs/https-via-proxy.md for copy-paste templates."
                );
            }
            let listener = keepalive_listener(listener, config.tcp_keepalive_secs);
            let shutdown_cancel = cancel.clone();
            let serve_result = {
                let serve = axum::serve(
                    listener,
                    router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .with_graceful_shutdown(async move {
                    let signal = shutdown.recv().await;
                    info!(signal, "shutdown signal received; draining");
                    shutdown_cancel.cancel();
                })
                .into_future();
                let mut serve = std::pin::pin!(serve);
                // Bound the drain. axum waits for every in-flight connection
                // to close, and a stateful or SSE MCP client never closes one
                // on its own — without this the shutdown outlives the
                // supervisor's own patience and ends in SIGKILL (#699).
                tokio::select! {
                    result = &mut serve => Some(result),
                    () = async {
                        cancel.cancelled().await;
                        tokio::time::sleep(SHUTDOWN_GRACE).await;
                    } => {
                        tracing::warn!(
                            grace_secs = SHUTDOWN_GRACE.as_secs(),
                            "connections still open past the shutdown grace period; exiting anyway"
                        );
                        None
                    }
                }
            };
            cancel.cancel();
            // The profile pass checks no cancellation mid-pass; a pass that
            // is mid-write finishes its current page within the grace period
            // or is abandoned, and the next start resumes from its marks.
            match tokio::time::timeout(SHUTDOWN_GRACE, profile_task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "profile worker join failed"),
                Err(_) => tracing::warn!(
                    grace_secs = SHUTDOWN_GRACE.as_secs(),
                    "profile worker did not stop within the shutdown grace period; exiting anyway"
                ),
            }
            if let Some(task) = session_consolidation_task {
                // Bounded like the drain above. The worker can be parked in
                // `claim_session_consolidation` or `release_session_consolidation`,
                // neither of which races the cancellation, behind the
                // single-writer actor's queue — an unbounded join here would
                // sit outside the shutdown bound entirely (#699).
                match tokio::time::timeout(SHUTDOWN_GRACE, task).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "session consolidation worker join failed");
                    }
                    Err(_) => tracing::warn!(
                        grace_secs = SHUTDOWN_GRACE.as_secs(),
                        "session consolidation worker did not stop within the shutdown \
                         grace period; exiting anyway"
                    ),
                }
            }
            close_store(&store).await;
            if let Some(serve_result) = serve_result {
                serve_result?;
            }
        }
    }
    Ok(())
}

/// Checkpoint the WAL and close the store's connections on the way out.
///
/// `main` ends the process without joining the store writer (detached tasks
/// still hold clones of its handle), so the connection is never closed and the
/// whole `-wal` is left for the next start to replay: 9 minutes for a 17.8 GB
/// one after a `VACUUM`. Bounded by [`SHUTDOWN_GRACE`] because the close queues
/// behind whatever the writer is running, and a stuck query must not turn a
/// stop back into a SIGKILL.
async fn close_store(store: &Store) {
    close_store_within(store, SHUTDOWN_GRACE).await;
}

/// [`close_store`] with the deadline as a parameter, so a test can pin the
/// bound without waiting out the real one. `true` when the store closed in
/// time.
async fn close_store_within(store: &Store, grace: Duration) -> bool {
    if tokio::time::timeout(grace, store.close()).await.is_err() {
        tracing::warn!(
            grace_secs = grace.as_secs(),
            "store did not close within the shutdown grace period; exiting anyway"
        );
        return false;
    }
    true
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
    sessions: crate::config::SessionsSettings,
    dream: crate::config::DreamSettings,
    activity_clock: ai_memory_consolidate::ActivityClock,
    contradiction_band_min: f32,
    contradiction_band_max: f32,
    consolidate_on_session_end: bool,
    has_session_consolidator: bool,
) -> Vec<tokio::task::JoinHandle<()>> {
    let maintenance_enabled = settings.enabled;
    if !maintenance_enabled {
        info!("scheduled retention/lint/embed maintenance disabled");
    }

    let forget_sweep_interval_secs = settings.forget_sweep_interval_secs;
    let lint_interval_secs = settings.lint_interval_secs;
    let embedding_backfill_interval_secs = settings.embedding_backfill_interval_secs;

    // A3 cold-cluster dedup targets the running server's configured embedder
    // coordinate; with no embedder it is `None`, making A3 a clean no-op even
    // when the flag is set (there are no stored vectors to cluster).
    let dedup_embedding = embedder
        .as_ref()
        .map(|e| ai_memory_consolidate::EmbeddingCoord {
            provider: e.provider().to_string(),
            model: e.model_identity(),
            dim: e.dim(),
        });

    let mut tasks = Vec::new();
    if maintenance_enabled && forget_sweep_interval_secs > 0 {
        let reader = reader.clone();
        let writer = writer.clone();
        let wiki = wiki.clone();
        let dedup_embedding = dedup_embedding.clone();
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
                    let dedup_embedding = dedup_embedding.clone();
                    async move {
                        let started = std::time::Instant::now();
                        let outcome = run_scheduled_sweep_tick(
                            &reader,
                            &writer,
                            &wiki,
                            &decay.decay_params(),
                            decay.breadth_weight,
                            decay.observation_retention(),
                            decay.ledger_retention(),
                            decay.compact_cold_episodic,
                            decay.cold_cluster_dedup(dedup_embedding),
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
                            compacted = outcome.compacted,
                            expired = outcome.expired,
                            hard_deleted = outcome.hard_deleted,
                            observations_pruned = outcome.observations_pruned,
                            abandoned_sessions_closed = outcome.abandoned_sessions_closed,
                            ledgers_pruned = outcome.ledgers_pruned,
                            ledger_bytes_pruned = outcome.ledger_bytes_pruned,
                            ledgers_undeleted = outcome.ledgers_undeleted,
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
    // observations, handoffs, managed workstreams, agent messages (sent or
    // received, in any state), or auto-improvement data
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
                        let outcome = run_scheduled_lint_tick(
                            &reader,
                            &wiki,
                            llm.as_ref(),
                            decay_lambda,
                            contradiction_band_min,
                            contradiction_band_max,
                        )
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

    // One-shot startup backfill (2.0): with an embedder present, pages
    // written before it existed - a fresh upgrade onto the default local
    // embedder, or a provider switch - get their vectors without any
    // maintenance config. Skips already-embedded pages, so a settled
    // store logs one cheap no-op pass. Runs in the background; startup
    // is never blocked on it.
    if let Some(embedder) = embedder.clone() {
        let reader = reader.clone();
        let writer = writer.clone();
        let wiki = wiki.clone();
        tasks.push(tokio::spawn(async move {
            let started = std::time::Instant::now();
            match run_scheduled_embedding_backfill_tick(&reader, &writer, &wiki, &embedder).await {
                Ok(outcome) if outcome.embedded > 0 || outcome.failed > 0 || outcome.errors > 0 => {
                    info!(
                        scopes = outcome.scopes,
                        embedded = outcome.embedded,
                        skipped = outcome.skipped,
                        failed = outcome.failed,
                        errors = outcome.errors,
                        elapsed_ms = started.elapsed().as_millis(),
                        "startup embedding backfill completed"
                    );
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "startup embedding backfill failed"),
            }
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
            experience: (scheduler.experience_every_sessions > 0).then(|| {
                ai_memory_consolidate::ExperienceConfig {
                    sessions: scheduler.experience_sessions.max(1),
                    min_new_sessions: scheduler.experience_every_sessions,
                    entropy_filter: scheduler.experience_entropy_filter,
                    ..ai_memory_consolidate::ExperienceConfig::default()
                }
            }),
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

    // B2/B3/B4 — the opt-in LLM dream pass. OFF by default; it starts only when
    // `[dream] enabled` is set AND a provider AND an embedder are configured (a
    // provider-less store keeps the zero-LLM A3 path, invariant #13). It never
    // contends with live work: it runs only after `idle_window_secs` of quiet and
    // cancels the moment activity resumes (invariant #5, cancellable + bounded).
    if dream.enabled {
        match (llm.clone(), dedup_embedding.clone()) {
            (Some(llm), Some(embedding)) => {
                let reader = reader.clone();
                let wiki = wiki.clone();
                let activity_clock = activity_clock.clone();
                let interval = std::time::Duration::from_secs(dream.effective_interval_secs());
                tasks.push(tokio::spawn(async move {
                    run_dream_scheduler_loop(
                        reader,
                        wiki,
                        llm,
                        decay,
                        dream,
                        embedding,
                        activity_clock,
                        interval,
                    )
                    .await;
                }));
            }
            (None, _) => info!(
                "dream pass enabled but no LLM provider is configured; job not started (the zero-LLM A3 path is unaffected)"
            ),
            (_, None) => info!(
                "dream pass enabled but no embedder is configured; job not started (nothing to cluster)"
            ),
        }
    }

    // Idle session auto-end: end the open sessions that never received a
    // `SessionEnd` (crashed harness, closed terminal, timed-out hook) with
    // the effects the hook path would have given them — summary page and
    // consolidation job, never a handoff. Bounded batch every fixed
    // interval; the scan is stateless and idempotent (the writer-side
    // `ended_at IS NULL` CAS plus the conditional page write make a double
    // end impossible), so it needs no persisted cadence state.
    let idle_end_after_hours = sessions.idle_end_after_hours;
    if maintenance_enabled && idle_end_after_hours > 0 {
        let writer = writer.clone();
        let reader = reader.clone();
        let wiki = wiki.clone();
        tasks.push(tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(
                ai_memory_hooks::IDLE_END_INTERVAL_SECS,
            ));
            // The first tick fires immediately; let the server come up and
            // the writer settle before the first scan.
            interval.tick().await;
            loop {
                interval.tick().await;
                let now_us = jiff::Timestamp::now().as_microsecond();
                // The config load bounds the value to a microsecond-safe
                // range; the checked arithmetic is the runtime belt (an
                // overflow would flip the cutoff and mass-end sessions).
                let Some(cutoff_us) = i64::from(idle_end_after_hours)
                    .checked_mul(3_600_000_000)
                    .and_then(|window_us| now_us.checked_sub(window_us))
                else {
                    tracing::warn!(
                        idle_end_after_hours,
                        "idle session auto-end tick: cutoff arithmetic overflowed; skipping this pass"
                    );
                    continue;
                };
                // Over-fetch the selection past the active-run sessions:
                // the tick excludes them (their lifecycle belongs to the
                // run), and the exclusion must not starve the batch bound
                // behind a backlog of persistently excluded sessions.
                let active_runs = match reader
                    .active_managed_run_native_session_ids()
                    .await
                {
                    Ok(active_runs) => active_runs,
                    Err(error) => {
                        tracing::warn!(error = %error, "idle session auto-end run check failed");
                        continue;
                    }
                };
                let idle = match reader
                    .idle_open_sessions(
                        cutoff_us,
                        ai_memory_hooks::IDLE_END_BATCH
                            .saturating_add(active_runs.len()),
                    )
                    .await
                {
                    Ok(idle) => idle,
                    Err(error) => {
                        tracing::warn!(error = %error, "idle session auto-end selection failed");
                        continue;
                    }
                };
                if idle.is_empty() {
                    continue;
                }
                match ai_memory_hooks::run_idle_end_tick(&ai_memory_hooks::IdleEndTickInput {
                    writer: &writer,
                    reader: &reader,
                    wiki: &wiki,
                    sessions: idle,
                    now_us: jiff::Timestamp::now().as_microsecond(),
                    consolidate_on_session_end,
                    has_consolidator: has_session_consolidator,
                })
                .await
                {
                    // Success is the tick's own log (idle_end.rs); the
                    // scheduler only surfaces failures.
                    Ok(_outcome) => {}
                    Err(error) => tracing::warn!(error = %error, "idle session auto-end tick failed"),
                }
            }
        }));
    }

    if tasks.is_empty() {
        info!("scheduled maintenance enabled but all intervals are disabled");
    } else {
        info!(jobs = tasks.len(), "scheduled maintenance started");
    }
    tasks
}

/// The B3 dream scheduler loop: on its interval, run the dream pass across every
/// scope ONLY when the operator has been idle for the configured window, and
/// cancel the in-flight run the moment activity resumes. A cheap watcher task
/// flips the shared [`ai_memory_consolidate::DreamCancel`] when the activity
/// clock advances past the run's start; `run_dream_pass` polls it between
/// clusters.
#[allow(clippy::too_many_arguments)]
async fn run_dream_scheduler_loop(
    reader: ReaderPool,
    wiki: Wiki,
    llm: Arc<dyn LlmProvider>,
    decay: crate::config::DecaySettings,
    dream: crate::config::DreamSettings,
    embedding: ai_memory_consolidate::EmbeddingCoord,
    activity_clock: ai_memory_consolidate::ActivityClock,
    interval: std::time::Duration,
) {
    /// How often the cancel watcher samples the activity clock during a run.
    const DREAM_ACTIVITY_POLL: std::time::Duration = std::time::Duration::from_secs(2);
    let cfg = dream.dream_config(Some(embedding));
    let decay_params = decay.decay_params();
    loop {
        tokio::time::sleep(interval).await;
        let now_us = jiff::Timestamp::now().as_microsecond();
        if !ai_memory_consolidate::dream_idle_ready(&cfg, activity_clock.last_activity_us(), now_us)
        {
            continue;
        }

        // Cancel-on-activity: snapshot the last activity, then spawn a watcher
        // that flips the cancel as soon as the clock moves past that snapshot.
        let cancel = ai_memory_consolidate::DreamCancel::new();
        let run_start_activity = activity_clock.last_activity_us();
        let watcher = {
            let cancel = cancel.clone();
            let activity_clock = activity_clock.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(DREAM_ACTIVITY_POLL).await;
                    if activity_clock.last_activity_us() > run_start_activity {
                        cancel.cancel();
                        return;
                    }
                }
            })
        };

        let started = std::time::Instant::now();
        let scopes = match reader.list_all_scopes().await {
            Ok(scopes) => scopes,
            Err(error) => {
                tracing::warn!(%error, "dream scheduler: could not list scopes; skipping tick");
                watcher.abort();
                continue;
            }
        };
        let mut merged = 0usize;
        let mut superseded = 0usize;
        let mut cancelled = false;
        for scope in scopes {
            if cancel.is_cancelled() {
                cancelled = true;
                break;
            }
            match ai_memory_consolidate::run_dream_pass(
                &reader,
                &wiki,
                Some(llm.as_ref()),
                scope.workspace_id,
                scope.project_id,
                &decay_params,
                decay.breadth_weight,
                &cfg,
                &cancel,
                false,
            )
            .await
            {
                Ok(report) => {
                    merged += report.clusters_merged;
                    superseded += report.pages_superseded;
                    cancelled |= report.cancelled;
                }
                Err(error) => tracing::warn!(
                    workspace = %scope.workspace_name,
                    project = %scope.project_name,
                    %error,
                    "dream pass failed for scope"
                ),
            }
        }
        watcher.abort();
        info!(
            merged,
            superseded,
            cancelled,
            elapsed_ms = started.elapsed().as_millis(),
            "dream pass tick completed"
        );
    }
}

/// How long a session may sit open with no activity before the scheduled
/// sweep closes it (safety net) — deliberately much more
/// conservative than the `SessionStart`-triggered backfill-close's 15
/// minutes (`ai_memory_hooks::router::BACKFILL_CLOSE_MIN_IDLE`), since this
/// path has no positive "someone is continuing the work" signal at all; it
/// is a blind time-based guess and must essentially never fire on a
/// genuinely slow-but-alive session.
const ABANDONED_SESSION_CUTOFF: std::time::Duration = std::time::Duration::from_secs(48 * 60 * 60);

#[derive(Debug, Default)]
struct ScheduledSweepTickOutcome {
    scopes: usize,
    candidates_evaluated: usize,
    evicted: usize,
    compacted: usize,
    expired: usize,
    hard_deleted: usize,
    observations_pruned: usize,
    ledgers_pruned: usize,
    ledger_bytes_pruned: u64,
    ledgers_undeleted: usize,
    errors: usize,
    /// Sessions closed by the abandoned-session safety net (no usage, no
    /// consolidation — see `ai_memory_store::ops::close_abandoned_sessions`).
    abandoned_sessions_closed: u64,
}

#[allow(clippy::too_many_arguments)]
async fn run_scheduled_sweep_tick(
    reader: &ReaderPool,
    writer: &WriterHandle,
    wiki: &Wiki,
    decay: &ai_memory_store::DecayParams,
    breadth_weight: f64,
    retention: ObservationRetention,
    ledgers: ai_memory_consolidate::LedgerRetention,
    compact_cold_episodic: bool,
    dedup: ai_memory_consolidate::ColdClusterDedup,
) -> Result<ScheduledSweepTickOutcome> {
    let scopes = reader.list_all_scopes().await?;
    let mut outcome = ScheduledSweepTickOutcome {
        scopes: scopes.len(),
        ..ScheduledSweepTickOutcome::default()
    };

    for scope in scopes {
        match ai_memory_consolidate::run_sweep_with_ledgers(
            reader,
            writer,
            Some(wiki),
            scope.workspace_id,
            scope.project_id,
            decay,
            breadth_weight,
            retention,
            compact_cold_episodic,
            dedup.clone(),
            ledgers,
            false,
        )
        .await
        {
            Ok(report) => {
                outcome.candidates_evaluated += report.candidates_evaluated;
                outcome.evicted += report.evicted.iter().filter(|page| page.deleted).count();
                outcome.compacted += report
                    .compacted
                    .iter()
                    .filter(|page| page.compacted)
                    .count();
                outcome.expired += report.expired.len();
                outcome.hard_deleted += report.hard_deleted;
                outcome.observations_pruned += report.observations_pruned;
                outcome.ledgers_pruned += report.ledgers_pruned;
                outcome.ledger_bytes_pruned += report.ledger_bytes_pruned;
                outcome.ledgers_undeleted += report.ledgers_undeleted;
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

    let cutoff_us = jiff::Timestamp::now().as_microsecond()
        - i64::try_from(ABANDONED_SESSION_CUTOFF.as_micros()).unwrap_or(i64::MAX);
    match writer.close_abandoned_sessions(cutoff_us).await {
        Ok(closed) => outcome.abandoned_sessions_closed = closed,
        Err(e) => {
            outcome.errors += 1;
            tracing::warn!(error = %e, "scheduled abandoned-session close failed");
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
    contradiction_band_min: f32,
    contradiction_band_max: f32,
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
                // The automatic scheduled lint stays rule-based: the A5
                // contradiction detector is on for the user-invoked
                // `memory_lint` / admin lint, not the background sweep.
                embedding: None,
                contradiction_band_min,
                contradiction_band_max,
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
        patchable_page_prefixes: settings.patchable_page_prefixes.clone(),
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
            builtin: settings.eval.builtin.trim().to_owned(),
            timeout_secs: settings.eval.timeout_secs,
            targets: settings.eval.targets.clone(),
            min_delta: settings.eval.min_delta,
        },
    }
}

/// The retrieval-benchmark escape hatch (docs/benchmarks/README.md): the
/// LongMemEval harness replays ~24k session ends through one server, and
/// each SessionEnd auto-commit walks a wiki tree and object database that
/// keep growing, which dominates ingest wall clock. `retrieval` never
/// reads git history, so the harness runs its servers with this variable
/// set and the commits skipped. Read once at startup; nothing else may
/// read it, and no product path sets it.
fn benchmark_skip_wiki_checkpoints() -> bool {
    matches!(
        std::env::var("AI_MEMORY_EVAL_SKIP_WIKI_CHECKPOINTS").as_deref(),
        Ok("1") | Ok("true")
    )
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
    let is_local = cfg.provider == ai_memory_llm::EmbedderChoice::Local;
    let model = cfg.model.clone();
    let dim = cfg.dim;
    let defaulted = cfg.defaulted;
    // Local embeddings: fetch the model once, checksum-pinned, before
    // the loader runs (docs/local-embeddings.md). Offline installs drop
    // the files into models/ by hand and never hit the network.
    if cfg.provider == ai_memory_llm::EmbedderChoice::Local
        && let Some(models_dir) = cfg.models_dir.as_deref()
        && !ai_memory_llm::model_present(models_dir)
    {
        if defaulted {
            // Best-effort default (docs/local-embeddings.md): never
            // block startup on an ~87 MB download. Fetch in the
            // background; THIS boot runs without an embedder (the
            // pre-2.0 behaviour), the next start finds the files and
            // enables hybrid search. Offline hosts just log the warn.
            let models_dir = models_dir.to_path_buf();
            tokio::spawn(async move {
                tracing::info!(
                    dir = %models_dir.display(),
                    "fetching the default local embedding model in the \
                     background (~87 MB, one time); hybrid search enables \
                     on the next start"
                );
                if let Err(e) = ai_memory_llm::fetch_model(&models_dir).await {
                    tracing::warn!(
                        error = %e,
                        "default local embedding model fetch failed; running \
                         without an embedder. Set embedding_provider = \"none\" \
                         to opt out, or install offline per \
                         docs/local-embeddings.md"
                    );
                }
            });
            return Ok((wiki, None));
        }
        tracing::info!(
            dir = %models_dir.display(),
            "local embedding model not present; fetching (~87 MB, one time)"
        );
        ai_memory_llm::fetch_model(models_dir).await.context(
            "fetching the local embedding model (set up offline per \
             docs/local-embeddings.md if this host has no network)",
        )?;
    }
    let embedder = match build_embedder(cfg) {
        Ok(e) => e,
        Err(e) if defaulted => {
            tracing::warn!(
                error = %e,
                "default local embeddings unavailable (model load failed); \
                 continuing without an embedder"
            );
            return Ok((wiki, None));
        }
        Err(e) => return Err(e).context("building embedder from config"),
    };
    // Not `.model()`: the running triple must match what pages are
    // actually stored under, which a configured document prefix changes.
    // See `Embedder::model_identity`.
    let configured_model_identity = embedder.model_identity();
    let mismatch = store
        .reader
        .embedding_meta_for_mismatch(
            embedder.provider().into(),
            configured_model_identity.clone(),
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
            configured_model = %configured_model_identity,
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
    let embedder: Arc<dyn Embedder> = if is_local {
        // all-MiniLM inference parallelizes internally, so capacity bounds
        // concurrent calls rather than threads. Sizing from the CPU count
        // recovers the ingest throughput a single background document slot
        // lost (#1580) while keeping one slot query-reserved; see
        // `local_capacity` for the halve-and-clamp rationale.
        let cores = std::thread::available_parallelism()
            .map(|cores| cores.get())
            .unwrap_or(4);
        Arc::new(EmbeddingCapacity::new(embedder, local_capacity(cores))?)
    } else {
        embedder
    };
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
    let retry_hint = llm_retry_hint(&provider_name, &model);
    // Routes through the same construction `llm_provider_config` just
    // confirmed is `Some`: wraps `[primary, fallbacks...]` in a
    // `FallbackLlmProvider` when `llm_fallbacks` is non-empty, else returns
    // the plain provider unchanged (existing single-provider behavior).
    let llm = config
        .llm_provider_chain()
        .context("building LLM provider chain from config")?
        .context("LLM provider configured but chain construction returned none")?;
    let llm = provider_health.wrap_llm_provider(llm, provider_name, model, Some(retry_hint));
    info!(
        provider = llm.name(),
        model = llm.model(),
        max_input_tokens = config.consolidation.max_input_tokens,
        max_output_tokens = config.consolidation.max_output_tokens,
        verbatim_residue = config.consolidation.verbatim_residue,
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
        .with_verbatim_residue(config.consolidation.verbatim_residue)
        .with_prompt_limits(
            config.consolidation.max_input_tokens,
            config.consolidation.max_output_tokens,
            config.consolidation.input_token_safety_margin,
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
            timeout_secs = config.rerank_timeout_secs,
            "project/scopes memory_query reranking enabled (adds LLM latency)",
        );
        let stage =
            ai_memory_llm::RerankStage::new(Arc::new(ai_memory_llm::LlmReranker::new(llm.clone())))
                .with_timeout(std::time::Duration::from_secs(config.rerank_timeout_secs));
        server = server.with_rerank_stage(stage);
    }
    Ok(ConsolidatorSetup {
        server,
        consolidator: Some(consolidator),
        admin_llm: Some(llm),
    })
}

/// What the web health page reads from this process: the writer queue,
/// and the hook spool and recovery journal under this server's data
/// directory (a hook client on another machine spools there, not here).
struct ServeHealthProbe {
    writer: ai_memory_store::WriterHandle,
    data_dir: std::path::PathBuf,
}

impl ai_memory_web::health::HealthProbe for ServeHealthProbe {
    fn process_state(&self) -> ai_memory_web::health::ProcessState {
        use ai_memory_web::health::{JournalState, ProcessState, SpoolState};
        let spool = super::hook_spool::spool_health(&super::hook_spool::spool_dir(&self.data_dir));
        let (pending, degraded, finish_failed, error) =
            super::recovery::journal_summary(&self.data_dir);
        ProcessState {
            write_queue: Some(self.writer.queue_depth()),
            spool: Some(SpoolState {
                pending: spool.pending,
                oldest_age_ms: spool.oldest_age_ms,
                retries_total: spool.retries_total,
                capacity: spool.capacity,
            }),
            journal: Some(JournalState {
                pending,
                degraded,
                finish_failed,
                unreadable: error.is_some(),
            }),
        }
    }
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

/// What to run when the server's provider call failed.
///
/// The probe runs *on the server* (`--server`): a client-side `llm-test`
/// builds its provider from the caller's environment, and when the server runs
/// in a container or on another host the keys exist only there, so the hint
/// answered "no API key" or a provider 400 while the server was healthy
/// (#1639). `provider`/`model` are still named, so the report is readable
/// without opening the config.
fn llm_retry_hint(provider: &str, model: &str) -> String {
    format!("ai-memory llm-test --server   # server provider: {provider} / {model}")
}

/// Liveness probe for process supervisors.
///
/// Unauthenticated on purpose: launchd, systemd and `HEALTHCHECK` have no
/// bearer token, and the answer ("this process is listening") is already
/// observable by connecting to the port. It reads nothing and reports no
/// store, provider or auth state.
///
/// Without it the only live signal is `GET /mcp` answering 405, which is an
/// accident of method routing rather than a contract a supervisor can rely on.
fn healthz_router() -> axum::Router {
    axum::Router::new().route(
        "/healthz",
        axum::routing::get(|| async { axum::Json(serde_json::json!({ "status": "ok" })) }),
    )
}

fn apply_host_layer(router: axum::Router, allowed_hosts: Vec<String>) -> axum::Router {
    router.layer(axum::middleware::from_fn_with_state(
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

/// Refuse browser requests from a foreign page on a server with no credential
/// gate.
///
/// With auth off, the machine and admin routes trust whoever can reach the
/// port, and a page in the operator's browser can reach `127.0.0.1`. Browsers
/// send `GET` and body-less `POST` cross-origin without a preflight, and the
/// JSON extractors cannot refuse a request that has no body: `GET /handoff`
/// claims the single-use baton, `POST /admin/profile/rebuild` runs a profile
/// pass. Hooks, the CLI and MCP clients never send `Origin` or
/// `Sec-Fetch-Site`; the built-in web UI's same-origin calls into `/admin`
/// pass. With auth on, the bearer token (which a foreign page cannot attach)
/// and the session CSRF token are the gate instead.
///
/// The same layer also refuses any browser request whose `Sec-Fetch-Dest` is
/// not `empty`. Script `fetch()`/XHR, which is how the web UI calls `/admin`,
/// sends `empty`; images, navigations, link clicks, iframes and form posts
/// send another destination. That keeps a link or image stored in a page, or
/// a link opened from another application, off these routes even when the
/// browser labels it `same-origin` or `none`. Non-browser clients send no
/// fetch metadata and are unaffected.
///
/// Both checks rely on fetch metadata, which browsers send only to
/// potentially trustworthy origins: loopback names and addresses, and HTTPS.
/// A server reached over plain HTTP through a LAN address or another allowed
/// host name gets an `<img>` request with neither fetch metadata nor `Origin`.
/// For that case the session-start handoff fetch, the one state-changing `GET`
/// on these routes, must also carry [`CLIENT_MARKER_HEADER`], which a page
/// cannot add without a preflight these routes never answer. A request that
/// carries a proxy's forwarding header counts as non-loopback even when the
/// proxy rewrote `Host` to a loopback address. Loopback hosts
/// keep working without it, so hook scripts staged before the header existed
/// still deliver handoffs on a default install. `POST`s need no marker: a
/// browser sends `Origin` on every cross-origin `POST`, metadata or not.
fn refuse_cross_origin_browser_requests(router: axum::Router, auth_enabled: bool) -> axum::Router {
    if auth_enabled {
        return router;
    }
    router.layer(axum::middleware::from_fn(require_same_origin_browser))
}

/// Body of the 403 a handoff fetch without [`CLIENT_MARKER_HEADER`] gets. It
/// reaches an operator through a hook staged before the header existed, so it
/// names both ways out.
const CLIENT_MARKER_REFUSAL: &str = "handoff fetch refused: this server has no bearer token and \
     was reached through a non-loopback host name or a proxy, so it accepts the handoff fetch only from \
     current ai-memory hooks; re-stage them with `ai-memory install-hooks` (or `ai-memory \
     upgrade`), or configure AI_MEMORY_AUTH_TOKEN\n";

async fn require_same_origin_browser(req: Request<Body>, next: Next) -> Response {
    let headers = req.headers();
    if is_cross_origin_browser_request(headers) || is_non_script_browser_request(headers) {
        // Any page the operator opens can trigger this at will, so it stays
        // out of the default log level.
        tracing::debug!(
            path = req.uri().path(),
            origin = ?headers.get(header::ORIGIN),
            dest = ?headers.get("sec-fetch-dest"),
            "rejected browser request to an unauthenticated server"
        );
        return (StatusCode::FORBIDDEN, "browser request refused\n").into_response();
    }
    if handoff_fetch_lacks_client_marker(req.method(), req.uri().path(), headers) {
        tracing::debug!(
            host = ?headers.get(header::HOST),
            "rejected handoff fetch without the client marker on a non-loopback host"
        );
        return (StatusCode::FORBIDDEN, CLIENT_MARKER_REFUSAL).into_response();
    }
    next.run(req).await
}

/// Whether this is a handoff fetch (`GET`, or `HEAD`, which axum routes to the
/// same handler) without [`CLIENT_MARKER_HEADER`] on a host where a browser
/// sends no fetch metadata, or through a forwarding proxy. The path is the one inside any `--base-path`
/// mount, which strips its prefix before this layer runs.
fn handoff_fetch_lacks_client_marker(
    method: &axum::http::Method,
    path: &str,
    headers: &axum::http::HeaderMap,
) -> bool {
    if path != "/handoff" || !matches!(*method, axum::http::Method::GET | axum::http::Method::HEAD)
    {
        return false;
    }
    if headers
        .get(CLIENT_MARKER_HEADER)
        .is_some_and(|value| !value.as_bytes().is_empty())
    {
        return false;
    }
    // A plain-HTTP reverse proxy can rewrite `Host` to the loopback address
    // it forwards to, while the browser behind it saw a non-loopback origin
    // and so sent no fetch metadata. Such a request is not a loopback one.
    // First-party clients talk to the server directly and send none of these.
    let proxied = PROXY_HEADERS.iter().any(|name| headers.contains_key(*name));
    proxied
        || !headers
            .get(header::HOST)
            .and_then(|host| host.to_str().ok())
            .is_some_and(is_loopback_host)
}

/// Headers a forwarding proxy adds; any of them means the `Host` a request
/// carries may not be the one the browser addressed.
/// CDNs and load balancers name the client in their own headers
/// (`CF-Connecting-IP`, `True-Client-IP`, `X-Client-IP`), and some add only
/// the scheme, server or port they forwarded (`X-Forwarded-Proto`, …).
const PROXY_HEADERS: [&str; 12] = [
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-forwarded-server",
    "x-forwarded-port",
    "x-original-forwarded-for",
    "x-real-ip",
    "x-client-ip",
    "cf-connecting-ip",
    "true-client-ip",
    "via",
];

/// Whether a browser treats this `Host` as a potentially trustworthy origin
/// and so sends fetch metadata to it over plain HTTP: `localhost`, a
/// `*.localhost` name, `127.0.0.0/8` or `[::1]`. Anything else, including a
/// LAN address, `0.0.0.0` or `host.docker.internal`, is not.
fn is_loopback_host(host: &str) -> bool {
    let name = host_without_port(host).to_ascii_lowercase();
    name == "localhost"
        || name
            .strip_suffix(".localhost")
            .is_some_and(|label| !label.is_empty())
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Whether a browser marks this request as issued by a page of another origin.
///
/// `Sec-Fetch-Site` is computed by the browser and decides when present. A
/// browser too old to send it still sends `Origin` on every cross-origin
/// `POST`, so that falls back to comparing `Origin` with `Host`; `Origin: null`
/// (sandboxed frames, some redirects) never matches.
fn is_cross_origin_browser_request(headers: &axum::http::HeaderMap) -> bool {
    if let Some(site) = headers.get("sec-fetch-site") {
        return !matches!(site.to_str(), Ok("same-origin" | "none"));
    }
    let Some(origin) = headers.get(header::ORIGIN) else {
        return false;
    };
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    let authority = origin
        .to_str()
        .ok()
        .and_then(|origin| origin.split_once("://"))
        .map(|(_, authority)| authority);
    match (authority, host) {
        (Some(authority), Some(host)) => !authority.eq_ignore_ascii_case(host),
        _ => true,
    }
}

/// Whether a browser says this request was not issued by script `fetch()` or
/// XHR (`Sec-Fetch-Dest` other than `empty`). A value that is not valid UTF-8
/// is refused; an absent header leaves the decision to the origin check.
fn is_non_script_browser_request(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("sec-fetch-dest")
        .is_some_and(|dest| !dest.to_str().is_ok_and(|d| d.eq_ignore_ascii_case("empty")))
}

fn host_allowed(host: &str, allowed_hosts: &[String]) -> bool {
    allowed_hosts.iter().any(|allowed| {
        host.eq_ignore_ascii_case(allowed) || host_without_port(host).eq_ignore_ascii_case(allowed)
    })
}

/// Seed the read-side active-project fallback from the most recently active
/// project already on disk.
///
/// The pointer lives only in process memory, so restarting the daemon
/// mid-session drops it. An unscoped read then resolves through the baked
/// default scope and answers zero counts for a project holding thousands of
/// observations — through the SUCCESS path, with nothing in the log, so
/// neither the agent nor the operator can tell it apart from a genuinely
/// empty project (#678). Failing such a read closed is not the fix: a keyed
/// miss is also the normal shape of the pre-publish window (hooks are
/// fire-and-forget) and of TTL/cap eviction, both of which must keep
/// degrading gracefully.
///
/// So make the degraded answer a real one. The seed lands in a slot only READS
/// consult: a keyed hit still wins, an unscoped write from an unrecognized
/// caller still fails closed rather than being attributed to a reconstructed
/// project, and the first hook event publishes straight over it. The recency
/// bound is the pointer's own per-key TTL: activity older than that would have
/// aged out of a live pointer anyway. Non-fatal — a server that cannot read
/// this starts exactly as it does today.
async fn seed_active_project_fallback(reader: &ReaderPool, active_project: &ActiveProject) {
    if active_project.get().is_some() {
        return;
    }
    let ttl_us = i64::try_from(active_project.per_key_ttl().as_micros()).unwrap_or(i64::MAX);
    let since = jiff::Timestamp::now()
        .as_microsecond()
        .saturating_sub(ttl_us);
    match reader.most_recently_active_scope(since).await {
        Ok(Some((workspace_id, project_id))) => {
            active_project.seed_read_fallback(workspace_id, project_id);
            let workspace = reader
                .workspace_name_by_id(workspace_id)
                .await
                .ok()
                .flatten();
            let project = reader
                .project_name_by_id(workspace_id, project_id)
                .await
                .ok()
                .flatten();
            info!(
                workspace = workspace.as_deref().unwrap_or("<unknown>"),
                project = project.as_deref().unwrap_or("<unknown>"),
                "seeded active-project fallback from the last recorded activity"
            );
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(error = %e, "active-project fallback seed skipped (non-fatal)");
        }
    }
}

/// How a wiki baseline-checkpoint attempt should be surfaced at startup.
/// Extracted from the logging call so the owner-vs-other classification is
/// unit-testable without standing up a server: a real LocalSystem-service
/// owner failure needs a native Windows box, which is out of scope here.
#[derive(Debug, PartialEq, Eq)]
enum BaselineCheckpointLog {
    /// A checkpoint commit was created (INFO).
    Created,
    /// Nothing to commit (silent).
    Clean,
    /// libgit2 refused on its ownership guard — actionable, logged at ERROR.
    OwnerFailure,
    /// Any other failure — non-fatal, logged at WARN as before.
    OtherFailure,
}

fn classify_baseline_checkpoint<T>(result: &WikiResult<Option<T>>) -> BaselineCheckpointLog {
    match result {
        Ok(Some(_)) => BaselineCheckpointLog::Created,
        Ok(None) => BaselineCheckpointLog::Clean,
        Err(WikiError::GitOwner(_)) => BaselineCheckpointLog::OwnerFailure,
        Err(_) => BaselineCheckpointLog::OtherFailure,
    }
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
    /// The retry hint must name the probe that runs where the keys are (#1639):
    /// a client-side `llm-test` builds its provider from the caller's
    /// environment, which is the wrong one for a container or a remote server.
    #[test]
    fn the_retry_hint_probes_the_server_not_the_clients_environment() {
        let hint = super::llm_retry_hint("anthropic", "claude-haiku-4-5");
        assert!(
            hint.starts_with("ai-memory llm-test --server"),
            "the hint must run the probe on the server: {hint}"
        );
        assert!(
            !hint.contains("--provider") && !hint.contains("--api-key"),
            "the server-side probe takes no client-side provider flags: {hint}"
        );
        assert!(
            hint.contains("anthropic") && hint.contains("claude-haiku-4-5"),
            "the hint must still name what failed: {hint}"
        );
    }

    #[cfg(feature = "management-ui")]
    #[tokio::test]
    async fn management_ui_rejects_invalid_mounts_before_opening_the_store() {
        use clap::Parser;
        let tmp = tempfile::tempdir().unwrap();
        let config = crate::config::Config {
            data_dir: tmp.path().join("unopened"),
            ..Default::default()
        };
        for args in [
            vec!["ai-memory", "serve", "--enable-management-ui"],
            vec![
                "ai-memory",
                "serve",
                "--transport",
                "http",
                "--enable-management-ui",
                "--enable-web",
                "--web-slug",
                "/manage/",
            ],
        ] {
            let parsed = crate::cli::Cli::try_parse_from(args).unwrap();
            let crate::cli::Command::Serve(args) = parsed.command else {
                panic!("serve")
            };
            let error = super::run(&config, args).await.unwrap_err();
            assert!(
                error.to_string().contains("--transport http")
                    || error.to_string().contains("reserves /manage")
            );
            assert!(!config.data_dir.exists());
        }
    }

    use super::*;
    use ai_memory_core::{
        AgentKind, ApiCredentialId, NewObservation, NewSession, NewUser, ObservationKind, PagePath,
        Sanitized, Sanitizer, SessionId, Tier,
    };
    use ai_memory_llm::{ChatRequest, ChatResponse, LlmError, LlmResult, SyntheticEmbedder};
    use ai_memory_web::split_web_routers;
    use ai_memory_wiki::WritePageRequest;
    use axum::http::Request;
    use secrecy::SecretString;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use tower::ServiceExt;

    /// The shutdown close queues behind whatever the writer is running, so the
    /// deadline is what keeps a stuck query from turning a stop back into a
    /// SIGKILL. The writer is made busy for real: another connection holds the
    /// write lock, so the writer's next command (the close itself, or the
    /// write queued before it) waits in SQLite's 5s busy handler.
    #[tokio::test]
    async fn close_store_returns_within_its_grace_while_the_writer_is_busy() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let blocker = rusqlite::Connection::open(store.db_path()).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let writer = store.writer.clone();
        let queued = tokio::spawn(async move { writer.get_or_create_workspace("blocked").await });

        let started = std::time::Instant::now();
        let closed = close_store_within(&store, Duration::from_millis(200)).await;
        let elapsed = started.elapsed();

        // Let the writer go before asserting, so a failure does not leave it
        // parked for the rest of its busy timeout.
        blocker.execute_batch("ROLLBACK").unwrap();
        let _ = queued.await;
        assert!(!closed, "the close cannot finish while the writer is stuck");
        assert!(
            elapsed < Duration::from_secs(3),
            "close_store waited {elapsed:?} for a stuck writer instead of giving up"
        );
    }

    /// An owner-check failure of the startup wiki baseline checkpoint must be
    /// classified as an ERROR-worthy `OwnerFailure`, not the WARN-only
    /// `OtherFailure` that hid it before (#872). A generic error stays
    /// `OtherFailure`, and the success/clean cases are unchanged — this is the
    /// seam that decides the log level, so it bites here.
    ///
    /// A full native-Windows LocalSystem-service repro (the environment that
    /// actually raises `code=Owner`) is out of scope; this covers the mapping
    /// from `WikiError::GitOwner` to the ERROR branch.
    #[test]
    fn owner_failure_baseline_checkpoint_is_error_not_warn() {
        let owner: WikiResult<Option<()>> = Err(WikiError::GitOwner("not owned".into()));
        assert_eq!(
            classify_baseline_checkpoint(&owner),
            BaselineCheckpointLog::OwnerFailure,
            "owner-check failure must be surfaced at ERROR, not buried in a WARN"
        );

        let other: WikiResult<Option<()>> = Err(WikiError::Io(std::io::Error::other("disk gone")));
        assert_eq!(
            classify_baseline_checkpoint(&other),
            BaselineCheckpointLog::OtherFailure,
            "a non-owner failure stays a non-fatal WARN"
        );

        assert_eq!(
            classify_baseline_checkpoint(&Ok::<_, WikiError>(Some(()))),
            BaselineCheckpointLog::Created
        );
        assert_eq!(
            classify_baseline_checkpoint(&Ok::<Option<()>, WikiError>(None)),
            BaselineCheckpointLog::Clean
        );
    }

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

    /// Assert `acquire_serve_lock` returned a real held lock, panicking with
    /// the concrete cause otherwise. The bare `.unwrap()...is_some()` collapsed
    /// a transient `Err` (EMFILE/ENFILE/EINTR under parallel fd pressure) or an
    /// `Ok(None)` downgrade into an un-actionable flake; this turns the next
    /// occurrence into a one-line errno diagnosis while still requiring the
    /// lock to be genuinely held.
    fn assert_serve_lock_held(result: Result<Option<ServeLock>>) -> ServeLock {
        match result {
            Ok(Some(lock)) => lock,
            Ok(None) => panic!(
                "acquire_serve_lock downgraded to an unguarded start (Ok(None)) although no other holder exists in this test"
            ),
            Err(err) => panic!("acquire_serve_lock failed: {err:?}"),
        }
    }

    /// Acquire the serve lock after a prior holder was released, tolerating the
    /// brief window in which a just-released `flock` can still report busy when
    /// the release and the re-acquire race in the *same* process under heavy
    /// parallel test load. This asserts the guarantee that actually matters — a
    /// released lock is not *permanently* held — rather than instant
    /// availability; a real server releases on process exit, so production never
    /// hits this same-process window (and `acquire_serve_lock` rightly never
    /// retries a genuine `WouldBlock`). Still requires the lock to be genuinely
    /// acquired within the window, and panics with the concrete cause otherwise.
    fn acquire_released_serve_lock(dir: &Path) -> ServeLock {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match acquire_serve_lock(dir, false) {
                Ok(Some(lock)) => return lock,
                other => {
                    if std::time::Instant::now() >= deadline {
                        return assert_serve_lock_held(other);
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        }
    }

    #[test]
    fn transient_errors_are_retriable_but_a_busy_lock_is_not() {
        use std::io::{Error, ErrorKind};
        // EINTR is transient and cross-platform via ErrorKind::Interrupted.
        assert!(is_transient_serve_lock_error(&Error::from(
            ErrorKind::Interrupted
        )));
        #[cfg(unix)]
        {
            // EMFILE / ENFILE fd exhaustion is transient.
            assert!(is_transient_serve_lock_error(&Error::from_raw_os_error(24)));
            assert!(is_transient_serve_lock_error(&Error::from_raw_os_error(23)));
        }
        // A contended lock (WouldBlock) is the "someone else owns it" signal:
        // it is busy, never transient, and must not be retried away.
        let busy = Error::from(ErrorKind::WouldBlock);
        assert!(!is_transient_serve_lock_error(&busy));
        assert!(crate::commands::hook_spool::is_drain_lock_busy_error(&busy));
    }

    #[test]
    fn second_server_on_the_same_data_dir_is_refused_and_names_the_holder() {
        let dir = TempDir::new().unwrap();
        let _first = assert_serve_lock_held(acquire_serve_lock(dir.path(), false));
        let err = acquire_serve_lock(dir.path(), false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("--force"),
            "refusal must name the override: {err}"
        );
        assert!(
            err.contains(&format!("pid={}", std::process::id())),
            "refusal must name the holding process: {err}"
        );
    }

    #[test]
    fn force_starts_unguarded_while_the_holder_keeps_the_lock() {
        let dir = TempDir::new().unwrap();
        let _first = assert_serve_lock_held(acquire_serve_lock(dir.path(), false));
        assert!(acquire_serve_lock(dir.path(), true).unwrap().is_none());
        // --force bypasses the refusal, not the holder: a plain attempt still sees it.
        assert!(acquire_serve_lock(dir.path(), false).is_err());
    }

    #[test]
    fn a_released_serve_lock_does_not_lock_out_the_next_server() {
        let dir = TempDir::new().unwrap();
        {
            let _first = acquire_serve_lock(dir.path(), false).unwrap();
            // Dropping the holder is what process exit does to the flock: the
            // leftover .serve.lock file must not outlive the lock it named.
        }
        // Under heavy parallel `cargo test --workspace` load the just-released
        // flock can momentarily still report busy in this same process; retry
        // briefly so the assertion checks "not permanently locked out" rather
        // than instant availability.
        let _ = acquire_released_serve_lock(dir.path());
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
                    let result =
                        validate_existing_users_auth(users_exist, users_exist, false, &auth);
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
    fn native_api_key_pepper_preflight_is_independent_of_human_mode() {
        let missing = AuthSettings::default();
        assert!(validate_api_credential_pepper(true, &missing).is_err());

        let configured = AuthSettings {
            token_pepper: Some("pepper".into()),
            ..AuthSettings::default()
        };
        assert!(validate_api_credential_pepper(true, &configured).is_ok());
        assert!(validate_api_credential_pepper(false, &missing).is_ok());
    }

    #[tokio::test]
    async fn api_key_pepper_binding_refuses_a_mismatch_once_credentials_exist() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let original = AuthSettings {
            token_pepper: Some("original-pepper-with-enough-entropy".into()),
            ..AuthSettings::default()
        };
        let replacement = AuthSettings {
            token_pepper: Some("replacement-pepper-with-enough-entropy".into()),
            ..AuthSettings::default()
        };

        validate_api_credential_pepper_binding(&store, false, &original)
            .await
            .unwrap();
        assert!(
            store
                .reader
                .api_credential_pepper_verifier()
                .await
                .unwrap()
                .is_some()
        );
        validate_api_credential_pepper_binding(&store, true, &original)
            .await
            .unwrap();

        let mismatch = validate_api_credential_pepper_binding(&store, true, &replacement)
            .await
            .unwrap_err();
        assert!(
            mismatch.to_string().contains("does not match"),
            "unexpected error: {mismatch:#}"
        );

        validate_api_credential_pepper_binding(&store, false, &replacement)
            .await
            .unwrap();
        validate_api_credential_pepper_binding(&store, true, &replacement)
            .await
            .unwrap();
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
                        validate_http_exposure(
                            local_addr,
                            auth_enabled,
                            false,
                            false,
                            allow_override,
                            false,
                            false,
                        )
                        .is_ok(),
                        allowed,
                        "address={address}, auth={auth_enabled}, override={allow_override}"
                    );
                }
            }
        }
    }

    #[test]
    fn human_auth_non_loopback_requires_secure_cookie_posture() {
        let remote: SocketAddr = "192.168.1.90:49374".parse().unwrap();
        assert!(validate_http_exposure(remote, true, true, false, false, false, false).is_err());
        assert!(validate_http_exposure(remote, true, true, false, true, false, true).is_err());
        assert_eq!(
            validate_http_exposure(remote, true, true, true, false, false, false).unwrap(),
            HttpExposure::Safe
        );
        assert_eq!(
            validate_http_exposure(remote, true, true, false, false, true, false).unwrap(),
            HttpExposure::InsecureHumanAuthByOverride
        );

        // The human-auth override must not become a second way to expose a
        // server with no authentication at all.
        assert!(validate_http_exposure(remote, false, false, false, false, true, false).is_err());
        assert!(validate_http_exposure(remote, false, true, false, false, true, false).is_err());

        let loopback: SocketAddr = "127.0.0.1:49374".parse().unwrap();
        assert_eq!(
            validate_http_exposure(loopback, true, true, false, false, true, false).unwrap(),
            HttpExposure::Safe
        );
    }

    /// The exposure notice must not be something a log level can switch off.
    /// `exposure_banner` is printed with `eprintln!`, outside the tracing
    /// `EnvFilter`, so `RUST_LOG=error` or `log_level = "error"` cannot hide
    /// that the server is reachable without a credential.
    #[test]
    fn every_unsafe_exposure_produces_an_operator_banner() {
        let addr: SocketAddr = "0.0.0.0:49374".parse().expect("valid test address");

        assert_eq!(exposure_banner(HttpExposure::Safe, addr), None);

        let override_banner = exposure_banner(HttpExposure::InsecureByOverride, addr)
            .expect("an unauthenticated override must be announced");
        assert!(override_banner.contains("NO AUTHENTICATION"));
        assert!(override_banner.contains("0.0.0.0:49374"));
        assert!(override_banner.contains("--allow-insecure-no-auth"));

        let human_banner = exposure_banner(HttpExposure::InsecureHumanAuthByOverride, addr)
            .expect("an insecure human-auth override must be announced");
        assert!(human_banner.contains("HUMAN AUTHENTICATION"));
        assert!(human_banner.contains("--allow-insecure-human-auth"));
        assert!(human_banner.contains("session cookies"));

        let container_banner = exposure_banner(HttpExposure::UndeterminedInContainer, addr)
            .expect("an unauthenticated container bind must be announced");
        assert!(container_banner.contains("NO AUTHENTICATION"));
        assert!(container_banner.contains("0.0.0.0:49374"));
        // The remedy has to be in the text: the operator reading this on a
        // terminal has no log collector to go digging in.
        assert!(container_banner.contains("AI_MEMORY_AUTH_TOKEN"));
    }

    /// The Quick Start shape from #407 is exactly the one #902 reports as
    /// silently exposed, so the two must agree: still not refused, but now
    /// unconditionally announced.
    #[test]
    fn the_quick_start_container_bind_is_announced_not_refused() {
        let quick_start: SocketAddr = "0.0.0.0:49374".parse().expect("valid test address");

        let exposure = validate_http_exposure(quick_start, false, false, false, false, false, true)
            .expect("must not refuse");
        assert_eq!(exposure, HttpExposure::UndeterminedInContainer);
        assert!(exposure_banner(exposure, quick_start).is_some());

        // With a token configured there is nothing to announce.
        let authed = validate_http_exposure(quick_start, true, false, false, false, false, true)
            .expect("auth is fine");
        assert_eq!(exposure_banner(authed, quick_start), None);
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
            validate_http_exposure(quick_start, false, false, false, false, false, true)
                .expect("must not refuse"),
            HttpExposure::UndeterminedInContainer,
        );

        // Identical inputs on a host still refuse — the carve-out is scoped
        // to the container case and does not soften the host rule.
        assert!(
            validate_http_exposure(quick_start, false, false, false, false, false, false).is_err()
        );

        // A container is not a blanket downgrade: with machine auth configured
        // the verdict is Safe, so the operator gets no spurious warning.
        assert_eq!(
            validate_http_exposure(quick_start, true, false, false, false, false, true)
                .expect("auth is fine"),
            HttpExposure::Safe,
        );

        // An explicit override still reports as an override, not as the
        // container case, so the startup log keeps naming the real reason.
        assert_eq!(
            validate_http_exposure(quick_start, false, false, false, true, false, true)
                .expect("override is fine"),
            HttpExposure::InsecureByOverride,
        );

        // Loopback inside a container is plain Safe.
        let loopback: SocketAddr = "127.0.0.1:49374".parse().expect("valid test address");
        assert_eq!(
            validate_http_exposure(loopback, false, false, false, false, false, true)
                .expect("loopback is fine"),
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
                reconcile_tombstones_deleted_pages: false,
            },
            AutoImproveSettings::default(),
            store.reader.clone(),
            store.writer.clone(),
            wiki,
            None,
            None,
            crate::config::DecaySettings::default(),
            crate::config::SessionsSettings::default(),
            crate::config::DreamSettings::default(),
            ai_memory_consolidate::ActivityClock::default(),
            ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_LOW,
            ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
            false,
            false,
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
                reconcile_tombstones_deleted_pages: false,
            },
            MaintenanceSettings {
                enabled: true,
                forget_sweep_interval_secs: 60,
                lint_interval_secs: 0,
                embedding_backfill_interval_secs: 0,
                reconcile_tombstones_deleted_pages: false,
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
                crate::config::SessionsSettings::default(),
                crate::config::DreamSettings::default(),
                ai_memory_consolidate::ActivityClock::default(),
                ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_LOW,
                ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
                false,
                false,
            )
            .await;
            // One enabled lint/sweep job plus the independent hollow-project job.
            assert_eq!(tasks.len(), 2);
            for task in tasks {
                task.abort();
            }
        }
    }

    /// The idle auto-end task starts only with maintenance enabled AND
    /// `idle_end_after_hours > 0`; either switch off leaves the task count
    /// unchanged.
    #[tokio::test]
    async fn idle_end_tick_does_not_start_when_disabled_or_maintenance_off() {
        let baseline = MaintenanceSettings {
            enabled: true,
            forget_sweep_interval_secs: 0,
            lint_interval_secs: 60,
            embedding_backfill_interval_secs: 0,
            reconcile_tombstones_deleted_pages: false,
        };
        // Key at 0: the baseline task count (one lint job + hollow-project job).
        let (_tmp, store, wiki, _ws, _first, _second) = two_project_wiki().await;
        let tasks = start_maintenance_scheduler(
            baseline.clone(),
            AutoImproveSettings::default(),
            store.reader.clone(),
            store.writer.clone(),
            wiki,
            None,
            None,
            crate::config::DecaySettings::default(),
            crate::config::SessionsSettings::default(),
            crate::config::DreamSettings::default(),
            ai_memory_consolidate::ActivityClock::default(),
            ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_LOW,
            ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
            false,
            false,
        )
        .await;
        assert_eq!(tasks.len(), 2);
        for task in tasks {
            task.abort();
        }

        // Key on: exactly one extra task — the idle auto-end loop.
        let (_tmp, store, wiki, _ws, _first, _second) = two_project_wiki().await;
        let tasks = start_maintenance_scheduler(
            baseline.clone(),
            AutoImproveSettings::default(),
            store.reader.clone(),
            store.writer.clone(),
            wiki,
            None,
            None,
            crate::config::DecaySettings::default(),
            crate::config::SessionsSettings {
                idle_end_after_hours: 1,
            },
            crate::config::DreamSettings::default(),
            ai_memory_consolidate::ActivityClock::default(),
            ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_LOW,
            ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
            false,
            false,
        )
        .await;
        assert_eq!(tasks.len(), 3);
        for task in tasks {
            task.abort();
        }

        // Maintenance disabled with the key on: nothing starts.
        let (_tmp, store, wiki, _ws, _first, _second) = two_project_wiki().await;
        let tasks = start_maintenance_scheduler(
            MaintenanceSettings {
                enabled: false,
                ..baseline
            },
            AutoImproveSettings::default(),
            store.reader.clone(),
            store.writer.clone(),
            wiki,
            None,
            None,
            crate::config::DecaySettings::default(),
            crate::config::SessionsSettings {
                idle_end_after_hours: 1,
            },
            crate::config::DreamSettings::default(),
            ai_memory_consolidate::ActivityClock::default(),
            ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_LOW,
            ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
            true,
            true,
        )
        .await;
        assert!(tasks.is_empty());
    }

    /// With the key on, the first real pass (one fixed interval after start,
    /// fast-forwarded with paused `tokio::time`) ends an idle session with
    /// the page, the watermark, the audit row and the consolidation job —
    /// no real sleeping.
    #[tokio::test]
    async fn idle_end_tick_ends_an_idle_session_on_the_first_pass() {
        let (_tmp, store, wiki, ws, proj, _second) = two_project_wiki().await;
        tokio::time::pause();
        let now = jiff::Timestamp::now().as_microsecond();
        let last_activity = now - 2 * 3_600_000_000; // two hours ago.
        let sid = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: sid,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Codex,
                cwd: Some("/repo".into()),
                actor_user: None,
                occurred_at: Some(last_activity),
            })
            .await
            .unwrap();
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id: sid,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "idle scheduler test".into(),
                    body: "prompt two hours ago".into(),
                    importance: 5,
                    occurred_at: Some(last_activity),
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();

        let tasks = start_maintenance_scheduler(
            MaintenanceSettings {
                enabled: true,
                forget_sweep_interval_secs: 0,
                lint_interval_secs: 0,
                embedding_backfill_interval_secs: 0,
                reconcile_tombstones_deleted_pages: false,
            },
            AutoImproveSettings::default(),
            store.reader.clone(),
            store.writer.clone(),
            wiki,
            None,
            None,
            crate::config::DecaySettings::default(),
            crate::config::SessionsSettings {
                idle_end_after_hours: 1,
            },
            crate::config::DreamSettings::default(),
            ai_memory_consolidate::ActivityClock::default(),
            ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_LOW,
            ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
            true,
            true,
        )
        .await;

        // The settle tick already fired; let the spawned task register its
        // first real interval timer, then fast-forward one fixed interval so
        // the first real pass runs.
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(std::time::Duration::from_secs(
            ai_memory_hooks::IDLE_END_INTERVAL_SECS,
        ))
        .await;
        // The writer runs on its own OS thread, which the paused clock does
        // not pace: under a loaded test run the end-session transaction can
        // still be in flight after any number of virtual advances, so each
        // poll also yields a little real time, bounded by a real deadline.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut ended = false;
        while std::time::Instant::now() < deadline {
            let disp = store
                .reader
                .session_end_disposition(sid, ws, proj, AgentKind::Codex)
                .await
                .unwrap();
            if matches!(disp, ai_memory_store::SessionEndDisposition::AlreadyEnded) {
                ended = true;
                break;
            }
            tokio::time::advance(std::time::Duration::from_millis(10)).await;
            tokio::task::spawn_blocking(|| {
                std::thread::sleep(std::time::Duration::from_millis(5));
            })
            .await
            .unwrap();
        }
        assert!(ended, "the first pass must end the idle session");

        // Page written, tick-clock ended_at (last activity in the audit
        // detail), audit row, consolidation job.
        let page = store
            .reader
            .latest_page_id_by_ids(ws, proj, format!("sessions/{sid}.md"))
            .await
            .unwrap();
        assert!(page.is_some());
        let audit = store
            .reader
            .list_audit_events(ai_memory_store::AuditLogFilter {
                op: Some("idle_session_end".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(audit.len(), 1);
        assert!(
            audit[0].at >= now && audit[0].at < now + 60_000_000_000,
            "ended_at is the tick's clock (the discovery is ordered by ended_at): {}",
            audit[0].at
        );
        assert!(
            audit[0].detail.contains(&last_activity.to_string()),
            "the last activity survives in the audit detail: {}",
            audit[0].detail
        );
        // Claim with a fresh clock: the enqueue's `requested_at` is the
        // tick's wall time, after this test's `now`.
        let now_after = jiff::Timestamp::now().as_microsecond();
        let job = store
            .writer
            .claim_session_consolidation(now_after + 1_000, now_after - 1_000)
            .await
            .unwrap();
        assert_eq!(job.map(|j| j.session_id()), Some(sid));
        for task in tasks {
            task.abort();
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

    /// A provider that answers HTTP 400 with a private body; the persisted
    /// `last_error` must stay a redacted class/status summary.
    struct Provider400ConsolidationLlm;

    impl LlmProvider for Provider400ConsolidationLlm {
        fn name(&self) -> &'static str {
            "provider-400-consolidation"
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
                Err(ai_memory_llm::LlmError::Provider {
                    status: 400,
                    body: "SENTINEL_PRIVATE_BODY".into(),
                    retry_after: None,
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
                Err(ai_memory_llm::LlmError::Provider {
                    status: 400,
                    body: "SENTINEL_PRIVATE_BODY".into(),
                    retry_after: None,
                })
            })
        }
    }

    /// Which fixed failure a [`FailingConsolidationLlm`] surfaces.
    #[derive(Clone, Copy)]
    enum ConsolidationFailure {
        /// The provider's structured JSON did not parse (deterministic).
        MalformedJson,
        /// The provider answered with a shape the structured decoder rejects.
        UnexpectedShape,
        /// The provider stopped at its output budget before completing JSON.
        TruncatedResponse,
        /// The provider returned HTTP 2xx with no usable content.
        EmptyContent,
        /// The configured credential is rejected or expired: the same key
        /// fails identically on every later attempt (deterministic).
        ExpiredAuth,
        /// The provider rejected the request itself with a 4xx other than
        /// `429`, which an unchanged prompt reproduces (deterministic).
        MalformedRequest,
        /// The provider is rate-limiting: a later attempt may clear it.
        RateLimited,
    }

    /// Structured-output failure fixture: counts calls (proving the prompt
    /// is never re-sent) and fails every structured completion with the
    /// fixed error.
    struct FailingConsolidationLlm {
        failure: ConsolidationFailure,
        calls: std::sync::Arc<AtomicUsize>,
    }

    impl LlmProvider for FailingConsolidationLlm {
        fn name(&self) -> &'static str {
            "failing-consolidation"
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
            Box::pin(async move { panic!("session consolidation uses structured output") })
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
            let calls = self.calls.clone();
            let failure = self.failure;
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let error = match failure {
                    ConsolidationFailure::MalformedJson => {
                        LlmError::Serde("missing field `title`".into())
                    }
                    ConsolidationFailure::UnexpectedShape => {
                        LlmError::UnexpectedShape("no tool block".into())
                    }
                    ConsolidationFailure::TruncatedResponse => LlmError::TruncatedResponse {
                        model: "test-model".into(),
                        completion_tokens: Some(8000),
                    },
                    ConsolidationFailure::EmptyContent => LlmError::EmptyContent {
                        model: "test-model".into(),
                    },
                    ConsolidationFailure::ExpiredAuth => LlmError::Auth("api key expired".into()),
                    ConsolidationFailure::MalformedRequest => LlmError::Provider {
                        status: 400,
                        body: "bad request".into(),
                        retry_after: None,
                    },
                    ConsolidationFailure::RateLimited => LlmError::Provider {
                        status: 429,
                        body: "rate limited".into(),
                        retry_after: None,
                    },
                };
                Err(error)
            })
        }
    }

    // The classifier decides the queue's terminal set by variant, never by
    // message text, and for an LLM failure it decides it with
    // `LlmError::is_transient` — the predicate the in-process consolidation
    // and bootstrap retry loops already use. Every deterministic failure is
    // terminal at any queue attempt, and every failure a later attempt may
    // plausibly clear is not.
    #[tokio::test]
    async fn is_terminal_session_consolidation_error_classifies_by_variant() {
        use ai_memory_consolidate::ConsolidatorError;

        let terminal = [
            ConsolidatorError::Serde("truncated json".into()),
            ConsolidatorError::Llm(LlmError::Serde("missing field `title`".into())),
            ConsolidatorError::Llm(LlmError::UnexpectedShape("no tool block".into())),
            ConsolidatorError::Llm(LlmError::TruncatedResponse {
                model: "test-model".into(),
                completion_tokens: Some(8000),
            }),
            ConsolidatorError::Llm(LlmError::EmptyContent {
                model: "test-model".into(),
            }),
            // Deterministic provider-side failures the queue must not re-send
            // either: the credential, the missing configuration, the derived
            // schema and a malformed-request 4xx all fail identically on the
            // next attempt.
            ConsolidatorError::Llm(LlmError::Auth("expired".into())),
            ConsolidatorError::Llm(LlmError::NotConfigured("no key".into())),
            ConsolidatorError::Llm(LlmError::Schema("bad".into())),
            ConsolidatorError::Llm(LlmError::Provider {
                status: 401,
                body: "invalid api key".into(),
                retry_after: None,
            }),
            ConsolidatorError::Llm(LlmError::Provider {
                status: 400,
                body: "bad request".into(),
                retry_after: None,
            }),
        ];
        for error in &terminal {
            assert!(
                is_terminal_session_consolidation_error(error),
                "a deterministic structured-response failure must be terminal: {error:?}"
            );
        }

        // A refused connection, built the same way the LLM suite builds one:
        // a closed loopback port refuses immediately.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let connect = reqwest::get(format!("http://{addr}/v1"))
            .await
            .expect_err("a closed loopback port must refuse the connection");
        assert!(
            connect.is_connect(),
            "the fixture must be a pre-send connection failure"
        );

        // Pre-send and provider-status failures a later attempt may plausibly
        // clear keep the queue's bounded backoff, whatever the message says.
        let not_terminal = [
            ConsolidatorError::Llm(LlmError::Provider {
                status: 429,
                body: "rate limited".into(),
                retry_after: None,
            }),
            ConsolidatorError::Llm(LlmError::Provider {
                status: 503,
                body: "capacity".into(),
                retry_after: None,
            }),
            ConsolidatorError::Llm(LlmError::Provider {
                status: 502,
                body: "bad gateway".into(),
                retry_after: None,
            }),
            ConsolidatorError::Llm(LlmError::Http(connect)),
        ];
        for error in &not_terminal {
            assert!(
                !is_terminal_session_consolidation_error(error),
                "a failure a later attempt may clear must keep the queue retrying: {error:?}"
            );
        }
    }

    /// #678: the pointer is process memory, so `systemctl restart` mid-session
    /// drops it. An unscoped read then resolved through the baked default scope
    /// and reported zero counts for a project holding thousands of observations,
    /// through the success path. Seeding the shared slot from the last recorded
    /// activity makes that degraded answer a real one.
    #[tokio::test]
    async fn a_restart_seeds_the_active_project_fallback_from_the_last_activity() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let workspace_id = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        // The baked default scope: empty, and where the bug parked every read.
        let scratch = store
            .writer
            .get_or_create_project(workspace_id, "scratch", None)
            .await
            .unwrap();
        let worked_in = store
            .writer
            .get_or_create_project(workspace_id, "real-project", None)
            .await
            .unwrap();
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                occurred_at: None,
                id: session_id,
                workspace_id,
                project_id: worked_in,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    occurred_at: None,
                    session_id,
                    workspace_id,
                    project_id: worked_in,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "work".into(),
                    body: "the session that outlived the daemon".into(),
                    importance: 5,
                },
                &Sanitizer::default(),
            ))
            .await
            .unwrap();

        // A pointer as empty as it is one instruction after a restart.
        let active_project = ActiveProject::new();
        seed_active_project_fallback(&store.reader, &active_project).await;
        assert_eq!(
            active_project.seeded(),
            Some((workspace_id, worked_in)),
            "the seed must name the project the last activity landed in"
        );
        assert_eq!(
            active_project.get(),
            None,
            "a reconstruction is not a publish: the shared slot stays empty"
        );

        // The live session's keyed entry died with the process, so its read
        // misses the map — and must now degrade to real data, not to `scratch`.
        let actor = ai_memory_core::ActorKey {
            user: None,
            session_id: Some(session_id.to_string()),
        };
        let resolved = ai_memory_store::ScopeResolver::new(&store.reader, workspace_id, scratch)
            .with_active_project(&active_project)
            .resolve_read_args(None, None, &actor)
            .await
            .unwrap();
        assert_eq!(
            resolved.as_tuple(),
            (workspace_id, worked_in),
            "an unscoped read after a restart must not silently answer for the baked default"
        );

        // ...while an unscoped WRITE from a caller the pointer cannot place is
        // untouched by the seed: it resolves exactly where it did before, so a
        // page is never attributed to a project reconstructed from a session
        // that is not this caller's.
        let written = ai_memory_store::ScopeResolver::new(&store.reader, workspace_id, scratch)
            .with_writer(&store.writer)
            .with_active_project(&active_project)
            .resolve_write_args(None, None, &actor)
            .await
            .unwrap();
        assert_eq!(
            written.scope.as_tuple(),
            (workspace_id, scratch),
            "the seed must not retarget unscoped writes"
        );
    }

    #[tokio::test]
    async fn seeding_never_overwrites_a_pointer_a_hook_already_published() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let workspace_id = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let published = store
            .writer
            .get_or_create_project(workspace_id, "published", None)
            .await
            .unwrap();
        // Activity in a DIFFERENT project, or the seed no-ops and this test
        // passes with the guard deleted.
        let elsewhere = store
            .writer
            .get_or_create_project(workspace_id, "elsewhere", None)
            .await
            .unwrap();
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                occurred_at: None,
                id: session_id,
                workspace_id,
                project_id: elsewhere,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    occurred_at: None,
                    session_id,
                    workspace_id,
                    project_id: elsewhere,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "work".into(),
                    body: "activity the seed would otherwise reach for".into(),
                    importance: 5,
                },
                &Sanitizer::default(),
            ))
            .await
            .unwrap();

        let active_project = ActiveProject::new();
        active_project.set(workspace_id, published);
        seed_active_project_fallback(&store.reader, &active_project).await;

        assert_eq!(
            active_project.get(),
            Some((workspace_id, published)),
            "a live pointer outranks anything the DB remembers"
        );
        assert_eq!(
            active_project.seeded(),
            None,
            "no seed is taken while a real publish already stands"
        );
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
                occurred_at: None,
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
                    occurred_at: None,
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
            store.reader.clone(),
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

    /// A PreCompact checkpoint job is an open session. The same worker must
    /// consume it without `end_session` and without the SessionEnd opt-in.
    #[tokio::test]
    async fn session_consolidation_worker_consumes_an_open_session_checkpoint() {
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
                occurred_at: None,
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
                    occurred_at: None,
                    session_id,
                    workspace_id,
                    project_id,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "checkpoint".into(),
                    body: "open session still running".into(),
                    importance: 8,
                },
                &Sanitizer::default(),
            ))
            .await
            .unwrap();
        store
            .writer
            .enqueue_session_checkpoint(workspace_id, project_id, session_id)
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
            store.reader.clone(),
            consolidator,
            notify.clone(),
            cancel.child_token(),
            completed.clone(),
        ));
        notify.notify_one();

        tokio::time::timeout(Duration::from_secs(5), completed.notified())
            .await
            .expect("worker should complete the open-session checkpoint");

        let path = format!("sessions/{session_id}.md");
        assert!(
            store
                .reader
                .page_body_by_ids(workspace_id, project_id, &path)
                .await
                .unwrap()
                .is_some_and(|page| page.body.contains("Durable worker completed")),
            "checkpoint completion must follow the durable wiki write"
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
            "completed checkpoint must not be claimed again"
        );
    }

    /// The worker attributes the rewrite to the session owner, not anonymous.
    #[tokio::test]
    async fn session_consolidation_worker_attributes_checkpoint_to_session_owner() {
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
                occurred_at: None,
                id: session_id,
                workspace_id,
                project_id,
                agent_kind: AgentKind::Codex,
                cwd: None,
                actor_user: Some("user:alice".into()),
            })
            .await
            .unwrap();
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    occurred_at: None,
                    session_id,
                    workspace_id,
                    project_id,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "checkpoint".into(),
                    body: "owned session still running".into(),
                    importance: 8,
                },
                &Sanitizer::default(),
            ))
            .await
            .unwrap();
        store
            .writer
            .enqueue_session_checkpoint(workspace_id, project_id, session_id)
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
            store.reader.clone(),
            consolidator,
            notify.clone(),
            cancel.child_token(),
            completed.clone(),
        ));
        notify.notify_one();

        tokio::time::timeout(Duration::from_secs(5), completed.notified())
            .await
            .expect("worker should complete the owned checkpoint");
        cancel.cancel();
        task.await.unwrap();

        let path = format!("sessions/{session_id}.md");
        let page = store
            .reader
            .page_body_by_ids(workspace_id, project_id, &path)
            .await
            .unwrap()
            .expect("checkpoint page must exist");
        let frontmatter: serde_json::Value = serde_json::from_str(&page.frontmatter_json).unwrap();
        assert_eq!(
            frontmatter["last_modified_by"]["username"].as_str(),
            Some("alice"),
            "checkpoint rewrite must be attributed to the session owner: {frontmatter}"
        );
    }

    // Mutation captured: persisting the error's `Display` writes the private
    // provider body into the queue row; the worker must persist only the
    // redacted class/status summary, and a deterministic `400` settles the
    // job terminal instead of re-sending the same prompt.
    #[tokio::test]
    async fn session_end_worker_persists_redacted_summary_not_provider_body() {
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
                occurred_at: None,
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
                    occurred_at: None,
                    session_id,
                    workspace_id,
                    project_id,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "finish".into(),
                    body: "end the session".into(),
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

        let consolidator = Arc::new(Consolidator::new(
            store.reader.clone(),
            store.writer.clone(),
            Wiki::new(tmp.path(), store.writer.clone()).unwrap(),
            Arc::new(Provider400ConsolidationLlm),
            workspace_id,
            project_id,
        ));
        let notify = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(tokio::sync::Notify::new());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_session_consolidation_worker(
            store.writer.clone(),
            store.reader.clone(),
            consolidator,
            notify.clone(),
            cancel.child_token(),
            completed.clone(),
        ));
        notify.notify_one();

        // Read the queue row through a read-only connection (WAL readers do
        // not block the writer): the row exists from enqueue, is `running`
        // while claimed, and settles with a `last_error` once the failure
        // lands.
        let db = rusqlite::Connection::open(store.db_path()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let (state, last_error, completed_at) = loop {
            let row = db
                .query_row(
                    "SELECT state, last_error, completed_at \
                     FROM session_consolidation_jobs WHERE session_id = ?1",
                    rusqlite::params![session_id.as_bytes()],
                    |r| {
                        Ok::<(String, Option<String>, Option<i64>), _>((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                        ))
                    },
                )
                .ok();
            if let Some(row) = &row
                && &row.0 != "running"
                && row.1.is_some()
            {
                break row.clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "failure row never settled"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        cancel.cancel();
        task.await.unwrap();

        assert_eq!(
            last_error.as_deref(),
            Some("consolidation failed: class=provider status=400"),
            "only the redacted class/status summary may be persisted"
        );
        let last_error = last_error.expect("checked above");
        assert!(
            !last_error.contains("SENTINEL_PRIVATE_BODY"),
            "provider body leaked into the queue row: {last_error}"
        );
        // A malformed-request `400` is deterministic (`LlmError::is_transient`
        // is false for every 4xx but `429`), so the job settles terminal on
        // the first attempt rather than re-sending the same prompt. The
        // terminal update stamps `completed_at` and leaves `state` out of
        // `pending`, which is what the claim query filters on.
        assert_eq!(
            state, "failed",
            "a deterministic 4xx must settle the job terminal"
        );
        assert!(
            completed_at.is_some(),
            "a terminal job must be stamped completed"
        );
    }

    /// An ended session with one observation and a queued SessionEnd job,
    /// plus a session page carrying `frontmatter`, as `memory_write_page`
    /// leaves it when the agent writes the page itself.
    async fn session_with_written_page(
        frontmatter: serde_json::Value,
    ) -> (TempDir, Store, Wiki, WorkspaceId, ProjectId, SessionId) {
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
                occurred_at: None,
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
                    occurred_at: None,
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
        wiki.write_page(WritePageRequest {
            workspace_id,
            project_id,
            path: PagePath::new(format!("sessions/{session_id}.md")).unwrap(),
            frontmatter,
            body: "# Agent page\n\nWritten by the agent before the session ended.".into(),
            tier: Tier::Episodic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
        (tmp, store, wiki, workspace_id, project_id, session_id)
    }

    /// Run the worker once over the queued job and return the session page body.
    async fn run_worker_once(
        store: &Store,
        wiki: Wiki,
        llm: Arc<dyn LlmProvider>,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
    ) -> String {
        let consolidator = Arc::new(Consolidator::new(
            store.reader.clone(),
            store.writer.clone(),
            wiki,
            llm,
            workspace_id,
            project_id,
        ));
        let notify = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(tokio::sync::Notify::new());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_session_consolidation_worker(
            store.writer.clone(),
            store.reader.clone(),
            consolidator,
            notify.clone(),
            cancel.child_token(),
            completed.clone(),
        ));
        notify.notify_one();
        tokio::time::timeout(Duration::from_secs(5), completed.notified())
            .await
            .expect("worker should settle the queued job");
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
            "a settled job must not be claimed again"
        );
        store
            .reader
            .page_body_by_ids(
                workspace_id,
                project_id,
                &format!("sessions/{session_id}.md"),
            )
            .await
            .unwrap()
            .map(|page| page.body)
            .unwrap_or_default()
    }

    /// The agent wrote the session page while the session was open: the
    /// worker completes the job without a completion (the provider panics if
    /// called) and keeps the page. The page's frontmatter predates the job's
    /// last observation, as every real in-session write does, and a stale
    /// `observation_generation` left by an older build changes nothing.
    #[tokio::test]
    async fn session_consolidation_worker_keeps_an_agent_page() {
        for frontmatter in [
            serde_json::json!({"consolidated_by": "agent"}),
            serde_json::json!({"consolidated_by": "agent", "observation_generation": 0}),
        ] {
            let (_tmp, store, wiki, workspace_id, project_id, session_id) =
                session_with_written_page(frontmatter.clone()).await;
            let body = run_worker_once(
                &store,
                wiki,
                Arc::new(PanicLlm),
                workspace_id,
                project_id,
                session_id,
            )
            .await;
            assert!(
                body.contains("Written by the agent"),
                "{frontmatter}: {body}"
            );
        }
    }

    /// Control: a session page no agent wrote is consolidated as before.
    #[tokio::test]
    async fn session_consolidation_worker_replaces_a_non_agent_page() {
        let (_tmp, store, wiki, workspace_id, project_id, session_id) =
            session_with_written_page(serde_json::json!({"consolidated": true})).await;
        let body = run_worker_once(
            &store,
            wiki,
            Arc::new(SuccessfulConsolidationLlm),
            workspace_id,
            project_id,
            session_id,
        )
        .await;
        assert!(body.contains("Durable worker completed"), "{body}");
    }

    /// What the SessionEnd queue must do with a failure.
    #[derive(Clone, Copy)]
    enum QueueOutcome {
        /// The job settles terminal: no later claim ever returns it again.
        Terminal,
        /// The job keeps the bounded backoff: a claim past the first 30s
        /// backoff returns it as the next attempt.
        Requeued,
    }

    /// Run the real SessionEnd worker against a session whose structured LLM
    /// call fails with `failure`, and prove the queue settles the job as
    /// `outcome` after exactly `expected_calls` prompts.
    ///
    /// For [`QueueOutcome::Terminal`] the job settles with no
    /// `next_attempt_at` — a claim far enough in the future that a re-queued
    /// job (first backoff is 30s) would be claimable finds none — and no
    /// further prompt goes out. The settle window (5 × 100ms) is measured: a
    /// writer round-trip is millisecond-scale, so probing past it is
    /// deterministic for a single-row SQLite update. For
    /// [`QueueOutcome::Requeued`] that same future-clock claim returns the
    /// job as attempt 2.
    ///
    /// Either way the heuristic page the SessionEnd hook wrote before
    /// enqueuing is untouched.
    async fn session_end_worker_settles_as(
        failure: ConsolidationFailure,
        outcome: QueueOutcome,
        expected_calls: usize,
    ) {
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
                occurred_at: None,
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
                    occurred_at: None,
                    session_id,
                    workspace_id,
                    project_id,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "finish".into(),
                    body: "end the session".into(),
                    importance: 8,
                },
                &Sanitizer::default(),
            ))
            .await
            .unwrap();
        store.writer.end_session(session_id, None).await.unwrap();

        // The SessionEnd hook writes this heuristic page before enqueuing the
        // LLM job; a terminal failure must leave it exactly in place.
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let path = format!("sessions/{session_id}.md");
        write_test_page(
            &wiki,
            workspace_id,
            project_id,
            &path,
            "heuristic",
            Tier::Episodic,
        )
        .await;
        let heuristic_body = store
            .reader
            .page_body_by_ids(workspace_id, project_id, &path)
            .await
            .unwrap()
            .expect("the heuristic page was written")
            .body;
        store
            .writer
            .enqueue_session_consolidation(workspace_id, project_id, session_id)
            .await
            .unwrap();

        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let consolidator = Arc::new(Consolidator::new(
            store.reader.clone(),
            store.writer.clone(),
            wiki,
            Arc::new(FailingConsolidationLlm {
                failure,
                calls: calls.clone(),
            }),
            workspace_id,
            project_id,
        ));
        let notify = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(tokio::sync::Notify::new());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_session_consolidation_worker(
            store.writer.clone(),
            store.reader.clone(),
            consolidator,
            notify.clone(),
            cancel.child_token(),
            completed.clone(),
        ));
        notify.notify_one();

        // The prompt must go out exactly `expected_calls` times. A transient
        // failure also spends the consolidator's own bounded inner retries
        // (2s apart) before the queue ever sees it, so allow for those.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while calls.load(Ordering::SeqCst) < expected_calls {
            assert!(
                std::time::Instant::now() < deadline,
                "worker never finished calling the LLM"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Let the writer settle the failure row, then probe with a clock 60s
        // ahead: a re-queued job (first backoff 30s) is claimable there, a
        // terminal row never is.
        match outcome {
            QueueOutcome::Terminal => {
                for _ in 0..5 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let probe_now = jiff::Timestamp::now().as_microsecond() + 60 * 1_000_000;
                    assert!(
                        store
                            .writer
                            .claim_session_consolidation(probe_now, probe_now - 10 * 60 * 1_000_000)
                            .await
                            .unwrap()
                            .is_none(),
                        "the failure must not re-queue the same prompt"
                    );
                    assert_eq!(
                        calls.load(Ordering::SeqCst),
                        expected_calls,
                        "no immediate replay or queue re-send of the same prompt"
                    );
                }
            }
            QueueOutcome::Requeued => {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                let requeued = loop {
                    let probe_now = jiff::Timestamp::now().as_microsecond() + 60 * 1_000_000;
                    if let Some(job) = store
                        .writer
                        .claim_session_consolidation(probe_now, probe_now - 10 * 60 * 1_000_000)
                        .await
                        .unwrap()
                    {
                        break job;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "a failure a later attempt may clear must stay on the queue",
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                };
                assert_eq!(
                    requeued.attempts(),
                    2,
                    "the re-queued job must be the second attempt"
                );
            }
        }
        cancel.cancel();
        task.await.unwrap();
        let body = store
            .reader
            .page_body_by_ids(workspace_id, project_id, &path)
            .await
            .unwrap();
        assert_eq!(
            body.map(|page| page.body),
            Some(heuristic_body.clone()),
            "the heuristic page must survive a terminal failure untouched"
        );
    }

    // Mutation captured: treating a deterministic structured-response failure
    // like a transient one re-queues the same expensive prompt; the worker
    // must settle the job terminal after the single send.
    #[tokio::test]
    async fn session_end_worker_makes_malformed_json_and_unexpected_shape_terminal() {
        session_end_worker_settles_as(
            ConsolidationFailure::MalformedJson,
            QueueOutcome::Terminal,
            1,
        )
        .await;
        session_end_worker_settles_as(
            ConsolidationFailure::UnexpectedShape,
            QueueOutcome::Terminal,
            1,
        )
        .await;
    }

    // Mutation captured: removing either provider error from the terminal
    // queue set causes a second claim of the same truncated or empty output.
    #[tokio::test]
    async fn session_end_worker_makes_truncated_and_empty_output_terminal() {
        session_end_worker_settles_as(
            ConsolidationFailure::TruncatedResponse,
            QueueOutcome::Terminal,
            1,
        )
        .await;
        session_end_worker_settles_as(
            ConsolidationFailure::EmptyContent,
            QueueOutcome::Terminal,
            1,
        )
        .await;
    }

    /// A rejected credential and a malformed-request 4xx are deterministic:
    /// the identical prompt fails identically on every later attempt. Before
    /// the queue gate consulted `LlmError::is_transient` it hand-enumerated
    /// only the structured-response shapes, so one expired key burned the
    /// full attempt cap (5 prompts over ~7.5 minutes) per ended session and
    /// logged "queued for retry" four times on the way.
    #[tokio::test]
    async fn session_end_worker_makes_an_expired_key_and_a_4xx_terminal() {
        session_end_worker_settles_as(ConsolidationFailure::ExpiredAuth, QueueOutcome::Terminal, 1)
            .await;
        session_end_worker_settles_as(
            ConsolidationFailure::MalformedRequest,
            QueueOutcome::Terminal,
            1,
        )
        .await;
    }

    /// The other direction, which the polarity of the gate decides: a `429`
    /// never reached the model, so the queue must keep its bounded,
    /// backoff-scheduled retry. The consolidator spends its own three inner
    /// attempts first, then the queue re-queues the job as attempt 2.
    #[tokio::test]
    async fn session_end_worker_requeues_a_rate_limited_provider() {
        session_end_worker_settles_as(ConsolidationFailure::RateLimited, QueueOutcome::Requeued, 3)
            .await;
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
            evidence: Vec::new(),
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
            ai_memory_consolidate::LedgerRetention::default(),
            false,
            ai_memory_consolidate::ColdClusterDedup::default(),
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
            ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_LOW,
            ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
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
                    embedder.model_identity(),
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

    fn browser_headers(pairs: &[(&'static str, &'static str)]) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:49374".parse().unwrap());
        for (name, value) in pairs {
            headers.insert(*name, value.parse().unwrap());
        }
        headers
    }

    #[test]
    fn cross_origin_browser_request_detection() {
        for (pairs, cross) in [
            // Hooks, the CLI and MCP clients send neither header.
            (&[][..], false),
            (&[("sec-fetch-mode", "cors")][..], false),
            (&[("sec-fetch-site", "same-origin")][..], false),
            (&[("sec-fetch-site", "none")][..], false),
            (&[("origin", "http://127.0.0.1:49374")][..], false),
            (
                &[
                    ("origin", "http://127.0.0.1:49374"),
                    ("sec-fetch-site", "same-origin"),
                ][..],
                false,
            ),
            (&[("sec-fetch-site", "cross-site")][..], true),
            (&[("sec-fetch-site", "same-site")][..], true),
            (&[("origin", "https://evil.example")][..], true),
            (&[("origin", "http://localhost:49374")][..], true),
            (&[("origin", "null")][..], true),
            // Fetch metadata decides over a forged-looking Origin and vice versa.
            (
                &[
                    ("origin", "http://127.0.0.1:49374"),
                    ("sec-fetch-site", "cross-site"),
                ][..],
                true,
            ),
        ] {
            assert_eq!(
                is_cross_origin_browser_request(&browser_headers(pairs)),
                cross,
                "{pairs:?}"
            );
        }
    }

    #[test]
    fn non_script_browser_request_detection() {
        for (pairs, refused) in [
            // No fetch metadata (hooks, CLI, MCP clients): the origin check decides.
            (&[][..], false),
            (&[("sec-fetch-site", "same-origin")][..], false),
            // Script fetch()/XHR, as the web UI sends.
            (&[("sec-fetch-dest", "empty")][..], false),
            (&[("sec-fetch-dest", "EMPTY")][..], false),
            (
                &[
                    ("sec-fetch-site", "same-origin"),
                    ("sec-fetch-dest", "empty"),
                ][..],
                false,
            ),
            // Images, navigations, frames: refused even same-origin or `none`.
            (&[("sec-fetch-dest", "image")][..], true),
            (&[("sec-fetch-dest", "document")][..], true),
            (&[("sec-fetch-dest", "iframe")][..], true),
            (
                &[
                    ("sec-fetch-site", "same-origin"),
                    ("sec-fetch-dest", "image"),
                ][..],
                true,
            ),
            (
                &[("sec-fetch-site", "none"), ("sec-fetch-dest", "document")][..],
                true,
            ),
            (&[("sec-fetch-dest", "")][..], true),
        ] {
            assert_eq!(
                is_non_script_browser_request(&browser_headers(pairs)),
                refused,
                "{pairs:?}"
            );
        }
        let mut headers = browser_headers(&[]);
        headers.insert(
            "sec-fetch-dest",
            axum::http::HeaderValue::from_bytes(b"emp\xffty").unwrap(),
        );
        assert!(is_non_script_browser_request(&headers), "non-UTF-8 dest");
    }

    #[tokio::test]
    async fn cross_origin_browser_guard_applies_only_without_auth() {
        use tower::ServiceExt as _;

        let send = |auth_enabled: bool, pairs: &'static [(&'static str, &'static str)]| async move {
            let app = refuse_cross_origin_browser_requests(
                axum::Router::new().route("/handoff", axum::routing::get(|| async { "claimed" })),
                auth_enabled,
            );
            let mut request = Request::builder()
                .uri("/handoff")
                .header(header::HOST, "127.0.0.1:49374");
            for (name, value) in pairs {
                request = request.header(*name, *value);
            }
            app.oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status()
        };
        // Attack: an `<img src>` from a foreign page, auth off.
        assert_eq!(
            send(false, &[("sec-fetch-site", "cross-site")]).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            send(false, &[("origin", "https://evil.example")]).await,
            StatusCode::FORBIDDEN
        );
        // Attack: a same-origin image or a navigation from outside the browser.
        assert_eq!(
            send(
                false,
                &[
                    ("sec-fetch-site", "same-origin"),
                    ("sec-fetch-dest", "image")
                ]
            )
            .await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            send(
                false,
                &[("sec-fetch-site", "none"), ("sec-fetch-dest", "document")]
            )
            .await,
            StatusCode::FORBIDDEN
        );
        // Controls: a hook-style request, and the web UI's same-origin call.
        assert_eq!(send(false, &[]).await, StatusCode::OK);
        assert_eq!(
            send(false, &[("sec-fetch-site", "same-origin")]).await,
            StatusCode::OK
        );
        assert_eq!(
            send(
                false,
                &[
                    ("sec-fetch-site", "same-origin"),
                    ("sec-fetch-dest", "empty")
                ]
            )
            .await,
            StatusCode::OK
        );
        // With auth on, the bearer gate decides; this layer stays out of it.
        assert_eq!(
            send(true, &[("sec-fetch-site", "cross-site")]).await,
            StatusCode::OK
        );
        assert_eq!(
            send(
                true,
                &[
                    ("sec-fetch-site", "same-origin"),
                    ("sec-fetch-dest", "document")
                ]
            )
            .await,
            StatusCode::OK
        );
    }

    #[test]
    fn loopback_host_detection() {
        for (host, loopback) in [
            ("127.0.0.1:49374", true),
            ("127.0.0.1", true),
            ("127.8.9.10:49374", true),
            ("[::1]:49374", true),
            ("localhost:49374", true),
            ("LOCALHOST", true),
            ("ai-memory.localhost:49374", true),
            ("a.b.LocalHost", true),
            // Not potentially trustworthy to a browser: no fetch metadata.
            ("192.168.1.20:49374", false),
            ("10.0.0.5", false),
            ("0.0.0.0:49374", false),
            ("host.docker.internal:49374", false),
            ("memory.lan:49374", false),
            ("[::ffff:127.0.0.1]:49374", false),
            ("notlocalhost:49374", false),
            ("localhost.evil.example", false),
            (".localhost", false),
            ("", false),
        ] {
            assert_eq!(is_loopback_host(host), loopback, "{host:?}");
        }
    }

    #[test]
    fn handoff_fetch_client_marker_decision() {
        use axum::http::Method;

        let headers = |host: &'static str, marker: Option<&'static str>| {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(header::HOST, host.parse().unwrap());
            if let Some(marker) = marker {
                headers.insert(CLIENT_MARKER_HEADER, marker.parse().unwrap());
            }
            headers
        };
        for (method, path, host, marker, refused) in [
            // Attack: an `<img>` to a LAN host carries no fetch metadata.
            (Method::GET, "/handoff", "192.168.1.20:49374", None, true),
            (Method::HEAD, "/handoff", "192.168.1.20:49374", None, true),
            (
                Method::GET,
                "/handoff",
                "host.docker.internal:49374",
                None,
                true,
            ),
            (Method::GET, "/handoff", "memory.lan", Some(""), true),
            // Controls: a current hook names itself on any host.
            (
                Method::GET,
                "/handoff",
                "192.168.1.20:49374",
                Some("hook"),
                false,
            ),
            (
                Method::GET,
                "/handoff",
                "memory.lan",
                Some("hook-sh"),
                false,
            ),
            // Loopback: browsers send fetch metadata there, and hooks staged
            // before the marker existed must keep working.
            (Method::GET, "/handoff", "127.0.0.1:49374", None, false),
            (Method::GET, "/handoff", "localhost:49374", None, false),
            (Method::GET, "/handoff", "[::1]:49374", None, false),
            // Other routes and methods are not the state-changing GET.
            (Method::POST, "/hook", "192.168.1.20:49374", None, false),
            (
                Method::GET,
                "/admin/handoffs",
                "192.168.1.20:49374",
                None,
                false,
            ),
            (Method::GET, "/handoff/x", "192.168.1.20:49374", None, false),
            (Method::POST, "/handoff", "192.168.1.20:49374", None, false),
        ] {
            assert_eq!(
                handoff_fetch_lacks_client_marker(&method, path, &headers(host, marker)),
                refused,
                "{method} {path} host={host} marker={marker:?}"
            );
        }
        // A plain-HTTP proxy that rewrites `Host` to loopback: the browser
        // behind it addressed a non-loopback origin and sent no metadata.
        // Each of these alone marks a request as proxied.
        for name in [
            "Forwarded",
            "X-Forwarded-For",
            "X-Forwarded-Host",
            "X-Forwarded-Proto",
            "X-Forwarded-Server",
            "X-Forwarded-Port",
            "X-Original-Forwarded-For",
            "X-Real-IP",
            "X-Client-IP",
            "CF-Connecting-IP",
            "True-Client-IP",
            "Via",
        ] {
            assert!(
                PROXY_HEADERS.contains(&name.to_ascii_lowercase().as_str()),
                "{name}"
            );
        }
        for proxy_header in PROXY_HEADERS {
            let mut proxied = headers("127.0.0.1:49374", None);
            proxied.insert(proxy_header, "203.0.113.7".parse().unwrap());
            assert!(
                handoff_fetch_lacks_client_marker(&Method::GET, "/handoff", &proxied),
                "{proxy_header} without the marker"
            );
            proxied.insert(CLIENT_MARKER_HEADER, "hook".parse().unwrap());
            assert!(
                !handoff_fetch_lacks_client_marker(&Method::GET, "/handoff", &proxied),
                "{proxy_header} with the marker"
            );
        }
        let mut no_host = axum::http::HeaderMap::new();
        assert!(handoff_fetch_lacks_client_marker(
            &Method::GET,
            "/handoff",
            &no_host
        ));
        no_host.insert(CLIENT_MARKER_HEADER, "hook".parse().unwrap());
        assert!(!handoff_fetch_lacks_client_marker(
            &Method::GET,
            "/handoff",
            &no_host
        ));
    }

    #[tokio::test]
    async fn handoff_client_marker_guard_on_lan_hosts_and_under_a_base_path() {
        use tower::ServiceExt as _;

        let send = |auth_enabled: bool,
                    base: &'static str,
                    host: &'static str,
                    marker: Option<&'static str>| async move {
            let guarded = refuse_cross_origin_browser_requests(
                axum::Router::new().route("/handoff", axum::routing::get(|| async { "claimed" })),
                auth_enabled,
            );
            let app = if base.is_empty() {
                guarded
            } else {
                axum::Router::new().nest(base, guarded)
            };
            let mut request = Request::builder()
                .uri(format!("{base}/handoff?workspace=w&project=p"))
                .header(header::HOST, host);
            if let Some(marker) = marker {
                request = request.header(CLIENT_MARKER_HEADER, marker);
            }
            let response = app
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            (status, String::from_utf8_lossy(&body).into_owned())
        };
        for base in ["", "/memory"] {
            // Attack: no metadata, no Origin, no marker, LAN host.
            let (status, body) = send(false, base, "192.168.1.20:49374", None).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "base={base:?}");
            assert!(
                body.contains("install-hooks") && body.contains("AI_MEMORY_AUTH_TOKEN"),
                "the refusal must name both remedies: {body}"
            );
            // Control: the same request from a current hook.
            assert_eq!(
                send(false, base, "192.168.1.20:49374", Some("hook")).await,
                (StatusCode::OK, "claimed".to_string()),
                "base={base:?}"
            );
            // Legacy hook on a loopback install.
            assert_eq!(
                send(false, base, "127.0.0.1:49374", None).await.0,
                StatusCode::OK,
                "base={base:?}"
            );
            // Auth on: the bearer gate decides.
            assert_eq!(
                send(true, base, "192.168.1.20:49374", None).await.0,
                StatusCode::OK,
                "base={base:?}"
            );
        }
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

    #[test]
    fn human_mode_does_not_require_root_bearer() {
        let auth = AuthSettings {
            token_pepper: Some("pepper".into()),
            bearer_token: None,
            ..AuthSettings::default()
        };
        assert!(validate_existing_users_auth(true, false, true, &auth).is_ok());
    }

    #[tokio::test]
    async fn machine_identity_remains_authenticated_with_web_disabled_and_expired_keys() {
        use ai_memory_core::{ApiCredentialId, NewUser, UserRole};
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let web = split_web_routers(
            false,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                enable_api: false,
                web_ui_dir: None,
                cors_origins: &[],
                web_slug: "/web",
                base_href: "/web/",
                base_path: "",
                trusted_proxy_identity: false,
                recall: ai_memory_web::Recall::default(),
                runtime: None,
                health: None,
            },
        )
        .unwrap();
        let pepper = ai_memory_store::TokenPepper::new("identity-mount-test");
        let user = store
            .writer
            .create_human_user(
                NewUser {
                    username: "alice".into(),
                    name: None,
                    email: None,
                },
                UserRole::User,
                None,
                false,
            )
            .await
            .unwrap();
        let id = ApiCredentialId::new();
        store
            .writer
            .create_api_credential(
                id,
                user,
                "test".into(),
                ai_memory_store::hash_token("expired-test-token", &pepper),
                None,
            )
            .await
            .unwrap();
        let auth = Arc::new(
            AuthState::new(Some("root-test-token".into())).with_multiuser(
                pepper,
                store.reader.clone(),
                store.writer.clone(),
            ),
        );
        let app = mount_machine_routes(axum::Router::new(), store.reader.clone(), auth)
            .merge(web.protected)
            .merge(web.public);
        for (token, status) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("invalid-test-token"), StatusCode::UNAUTHORIZED),
            (Some("root-test-token"), StatusCode::OK),
            (Some("expired-test-token"), StatusCode::OK),
        ] {
            let mut request = Request::builder().uri("/identity");
            if let Some(token) = token {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            assert_eq!(
                app.clone()
                    .oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status(),
                status
            );
        }
        let conn = rusqlite::Connection::open(store.db_path()).unwrap();
        conn.execute(
            "UPDATE api_credentials SET expires_at = 1 WHERE id = ?1",
            [id.as_bytes()],
        )
        .unwrap();
        assert_eq!(
            app.oneshot(
                Request::builder()
                    .uri("/identity")
                    .header("authorization", "Bearer expired-test-token")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn api_only_mode_keeps_data_authenticated_and_web_absent() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let web = split_web_routers(
            false,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                enable_api: true,
                web_ui_dir: None,
                cors_origins: &[],
                web_slug: "/web",
                base_href: "/web/",
                base_path: "",
                trusted_proxy_identity: false,
                recall: ai_memory_web::Recall::default(),
                runtime: None,
                health: None,
            },
        )
        .unwrap();
        assert!(web.html_auth.is_none());
        let auth = Arc::new(AuthState::new(Some("secret".to_string())));
        let router = apply_host_layer(
            web.public
                .merge(web.protected.layer(axum::middleware::from_fn_with_state(
                    auth,
                    require_dual_auth,
                ))),
            vec!["localhost".to_string()],
        );

        let unauthenticated = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/projects")
                    .header("Host", "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

        let authenticated = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/projects")
                    .header("Host", "localhost")
                    .header("Authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authenticated.status(), StatusCode::OK);

        let absent_web = router
            .oneshot(
                Request::builder()
                    .uri("/web")
                    .header("Host", "localhost")
                    .header("Authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(absent_web.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn web_routes_are_inside_auth_layer() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let web = split_web_routers(
            true,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                enable_api: false,
                web_ui_dir: None,
                cors_origins: &[],
                web_slug: "/web",
                base_href: "/web/",
                base_path: "",
                trusted_proxy_identity: false,
                recall: ai_memory_web::Recall::default(),
                runtime: None,
                health: None,
            },
        )
        .unwrap();
        let auth = Arc::new(AuthState::new(Some("secret".to_string())));
        let html_auth = web
            .html_auth
            .expect("builtin wiki mount must return html_auth");
        let router = apply_host_layer(
            web.public.merge(
                web.protected
                    .layer(axum::middleware::from_fn_with_state(
                        auth,
                        require_dual_auth,
                    ))
                    .layer(axum::middleware::from_fn_with_state(
                        html_auth,
                        html_auth_redirect_mw,
                    )),
            ),
            vec!["localhost".to_string()],
        );

        // Non-HTML clients still see JSON 401.
        let json_401 = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/web")
                    .header("Host", "localhost")
                    .header("Accept", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(json_401.status(), StatusCode::UNAUTHORIZED);

        // With a bearer and no human accounts there is no login form that
        // could succeed, so a browser navigation gets the Basic challenge,
        // not a redirect to the login page.
        let html_challenge = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/web")
                    .header("Host", "localhost")
                    .header("Accept", "text/html")
                    .header("sec-fetch-dest", "document")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(html_challenge.status(), StatusCode::UNAUTHORIZED);
        assert!(html_challenge.headers().get(header::LOCATION).is_none());
        let challenge = html_challenge
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(challenge.starts_with("Basic "), "challenge: {challenge}");

        // The token as the Basic password opens the wiki.
        use base64::Engine as _;
        let basic = base64::engine::general_purpose::STANDARD.encode("root:secret");
        let signed_in = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/web")
                    .header("Host", "localhost")
                    .header("Accept", "text/html")
                    .header("sec-fetch-dest", "document")
                    .header("Authorization", format!("Basic {basic}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(signed_in.status(), StatusCode::OK);

        // Login HTML is public (no auth).
        let login = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/web/login")
                    .header("Host", "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let body = axum::body::to_bytes(login.into_body(), 64 * 1024)
            .await
            .unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("Sign in"), "login page body: {html}");
        assert!(
            html.contains("ai-memory-base-path"),
            "inject base-path meta"
        );

        // Change-password HTML is public (must_change_password flow).
        let change = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/web/change-password")
                    .header("Host", "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(change.status(), StatusCode::OK);
        let change_body = axum::body::to_bytes(change.into_body(), 64 * 1024)
            .await
            .unwrap();
        let change_html = std::str::from_utf8(&change_body).unwrap();
        assert!(
            change_html.contains("Change password"),
            "change-password page body: {change_html}"
        );

        // `/api/v1` stays JSON even with an HTML Accept header.
        let api = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/projects")
                    .header("Host", "localhost")
                    .header("Accept", "text/html")
                    .header("sec-fetch-dest", "document")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(api.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn custom_spa_is_public_while_api_stays_authenticated() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let ui = TempDir::new().unwrap();
        std::fs::write(
            ui.path().join("index.html"),
            "<html><body>spa</body></html>",
        )
        .unwrap();
        let web = split_web_routers(
            true,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                enable_api: false,
                web_ui_dir: Some(ui.path()),
                cors_origins: &[],
                web_slug: "/web",
                base_href: "/web/",
                base_path: "",
                trusted_proxy_identity: false,
                recall: ai_memory_web::Recall::default(),
                runtime: None,
                health: None,
            },
        )
        .unwrap();
        let auth = Arc::new(AuthState::new(Some("secret".to_string())));
        let router = apply_host_layer(
            web.public
                .merge(web.protected.layer(axum::middleware::from_fn_with_state(
                    auth,
                    require_dual_auth,
                ))),
            vec!["localhost".to_string()],
        );

        let spa = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/web")
                    .header("Host", "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(spa.status(), StatusCode::OK);

        let api = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/projects")
                    .header("Host", "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(api.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn custom_spa_root_fallback_preserves_host_owned_routes() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let ui = TempDir::new().unwrap();
        std::fs::write(
            ui.path().join("index.html"),
            "<html><head></head><body>root spa shell</body></html>",
        )
        .unwrap();
        let web = split_web_routers(
            true,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                enable_api: false,
                web_ui_dir: Some(ui.path()),
                cors_origins: &[],
                web_slug: "/",
                base_href: "/",
                base_path: "",
                trusted_proxy_identity: false,
                recall: ai_memory_web::Recall::default(),
                runtime: None,
                health: None,
            },
        )
        .unwrap();

        let mut protected_host = axum::Router::new();
        for path in [
            "/admin/status",
            "/mcp",
            "/hook",
            "/handoff",
            "/workstream/runs",
        ] {
            protected_host = protected_host.route(
                path,
                axum::routing::any(|| async { StatusCode::UNAUTHORIZED }),
            );
        }
        let public_auth = axum::Router::new().route(
            "/auth/login",
            axum::routing::post(|| async { StatusCode::NO_CONTENT }),
        );
        let auth = Arc::new(AuthState::new(Some("secret".to_string())));
        let router = protected_host
            .merge(public_auth)
            .merge(web.protected.layer(axum::middleware::from_fn_with_state(
                auth,
                require_dual_auth,
            )))
            .merge(web.public)
            .merge(ai_memory_web::favicon_router())
            .merge(healthz_router());

        // Mutation captured: dropping any host-owned route merge lets the root SPA
        // wildcard return its HTML shell instead of the reserved route response.
        for (method, path) in [
            (axum::http::Method::GET, "/admin/status"),
            (axum::http::Method::POST, "/mcp"),
            (axum::http::Method::POST, "/hook"),
            (axum::http::Method::GET, "/handoff"),
            (axum::http::Method::POST, "/workstream/runs"),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{path} must reach its authenticated host route"
            );
        }

        // A supervisor probing liveness sends no bearer token, so /healthz has to
        // answer 200 with auth configured — and it is a host-owned route like the
        // ones above, so the SPA wildcard must not serve its shell here either.
        let health = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);

        let api = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/projects")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(api.status(), StatusCode::UNAUTHORIZED);

        let auth = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(axum::http::Method::POST)
                    .uri("/auth/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(auth.status(), StatusCode::NO_CONTENT);

        let favicon = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/favicon.ico")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(favicon.status(), StatusCode::OK);
        assert_eq!(
            favicon.headers().get(header::CONTENT_TYPE),
            Some(&header::HeaderValue::from_static("image/png"))
        );

        let shell = router
            .oneshot(
                Request::builder()
                    .uri("/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(shell.status(), StatusCode::OK);
        let body = axum::body::to_bytes(shell.into_body(), 4096).await.unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("root spa shell"),
            "/login must still use the root SPA fallback"
        );
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
            evidence: Vec::new(),
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

    #[test]
    fn human_auth_intended_from_bootstrap_password_or_recovery() {
        let empty = AuthSettings::default();
        assert!(!human_auth_intended(&empty, false, false));
        assert!(human_auth_intended(&empty, true, false));
        assert!(human_auth_intended(&empty, false, true));
        let recovery = AuthSettings {
            recovery_token: Some(SecretString::from("break-glass-recovery-token-32chr")),
            ..AuthSettings::default()
        };
        assert!(human_auth_intended(&recovery, false, false));
        let initial = AuthSettings {
            initial_root_password: Some(SecretString::from("twelve-chars!!")),
            ..AuthSettings::default()
        };
        assert!(human_auth_intended(&initial, false, false));
    }

    #[test]
    fn human_mode_fails_closed_without_recoverable_root_or_recovery() {
        let err = require_recoverable_root_or_recovery(true, 0, false).unwrap_err();
        assert!(err.to_string().contains("no recoverable root"));
        require_recoverable_root_or_recovery(true, 0, true).unwrap();
        require_recoverable_root_or_recovery(true, 1, false).unwrap();
        require_recoverable_root_or_recovery(false, 0, false).unwrap();
    }

    #[tokio::test]
    async fn maybe_bootstrap_root_is_one_shot() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let first = AuthSettings {
            initial_root_password: Some(SecretString::from("twelve-chars!!")),
            ..AuthSettings::default()
        };
        maybe_bootstrap_root(&store, &first).await.unwrap();
        assert!(store.reader.bootstrap_completed().await.unwrap());
        assert_eq!(store.reader.count_recoverable_roots().await.unwrap(), 1);
        let second = AuthSettings {
            initial_root_password: Some(SecretString::from("different-pass!!")),
            ..AuthSettings::default()
        };
        maybe_bootstrap_root(&store, &second).await.unwrap();
        let login = store
            .reader
            .find_login_user_by_username("root".into())
            .await
            .unwrap()
            .unwrap();
        assert!(
            ai_memory_store::password::verify_password(
                "twelve-chars!!".into(),
                login.password_hash.clone().unwrap(),
            )
            .await
            .unwrap()
        );
        assert!(
            !ai_memory_store::password::verify_password(
                "different-pass!!".into(),
                login.password_hash.unwrap(),
            )
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn maybe_bootstrap_root_fails_closed_on_api_credential_collision() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let mut user = NewUser {
            username: "legacy".into(),
            name: None,
            email: None,
        };
        user.validate().unwrap();
        let user_id = store
            .writer
            .create_human_user(user, ai_memory_core::UserRole::User, None, false)
            .await
            .unwrap();
        let pepper = TokenPepper::new("pepper");
        let password = "twelve-chars!!";
        store
            .writer
            .create_api_credential(
                ApiCredentialId::new(),
                user_id,
                "legacy".into(),
                hash_token(password, &pepper),
                None,
            )
            .await
            .unwrap();
        let auth = AuthSettings {
            token_pepper: Some("pepper".into()),
            initial_root_password: Some(SecretString::from(password)),
            ..AuthSettings::default()
        };
        let err = maybe_bootstrap_root(&store, &auth).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("collides with an existing API credential"),
            "{err}"
        );
        assert!(!store.reader.bootstrap_completed().await.unwrap());

        let state =
            AuthState::new(None).with_multiuser(pepper, store.reader.clone(), store.writer.clone());
        let mut machine_request = Request::builder()
            .header("authorization", format!("Bearer {password}"))
            .body(Body::empty())
            .unwrap();
        assert!(matches!(
            ai_memory_mcp::auth::authenticate_bearer(&state, &mut machine_request)
                .await
                .unwrap(),
            ai_memory_mcp::auth::BearerAuth::Authenticated
        ));
        assert_eq!(
            machine_request
                .extensions()
                .get::<ai_memory_core::AuthLevel>(),
            Some(&ai_memory_core::AuthLevel::User)
        );

        let login = public_auth_router(Arc::new(state))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth/login")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "username": "legacy",
                            "password": password,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn internal_session_introspect_is_inside_host_guard_not_require_bearer() {
        let state = std::sync::Arc::new(
            AuthState::new(Some("root-bearer".into()))
                .with_trusted_proxy_bearer("proxy-bearer-token"),
        );
        let router = apply_host_layer(internal_auth_router(state), vec!["memory.example".into()]);
        let body = Body::from(r#"{"session":"","method":"GET"}"#);

        let missing_host = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/internal/auth/session-introspect")
                    .header("authorization", "Bearer proxy-bearer-token")
                    .header("content-type", "application/json")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_host.status(), StatusCode::BAD_REQUEST);
        let text = String::from_utf8(
            axum::body::to_bytes(missing_host.into_body(), 4096)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(text.contains("missing Host"), "{text}");

        let bad_host = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/internal/auth/session-introspect")
                    .header("Host", "evil.example")
                    .header("authorization", "Bearer proxy-bearer-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"session":"","method":"GET"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad_host.status(), StatusCode::FORBIDDEN);

        let allowed = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/internal/auth/session-introspect")
                    .header("Host", "memory.example")
                    .header("authorization", "Bearer proxy-bearer-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"session":"","method":"GET"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(allowed.into_body(), 4096)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(json["authenticated"], false);
        let body_text = json.to_string();
        assert!(
            !body_text.contains("X-Memory-Actor"),
            "proxy bearer without actor headers must reach introspect, not MissingIdentity: {body_text}"
        );
    }

    #[tokio::test]
    async fn configured_authority_secrets_cannot_match_api_credentials() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let mut user = NewUser {
            username: "legacy".into(),
            name: None,
            email: None,
        };
        user.validate().unwrap();
        let user_id = store
            .writer
            .create_human_user(user, ai_memory_core::UserRole::User, None, false)
            .await
            .unwrap();
        let pepper = TokenPepper::new("pepper");
        let secret = "configured-authority-collision-32chars";
        store
            .writer
            .create_api_credential(
                ApiCredentialId::new(),
                user_id,
                "legacy".into(),
                hash_token(secret, &pepper),
                None,
            )
            .await
            .unwrap();

        let cases = [
            AuthSettings {
                token_pepper: Some("pepper".into()),
                bearer_token: Some(secret.into()),
                ..AuthSettings::default()
            },
            AuthSettings {
                token_pepper: Some("pepper".into()),
                actor_proxy_bearer_token: Some(secret.into()),
                ..AuthSettings::default()
            },
            AuthSettings {
                token_pepper: Some("pepper".into()),
                recovery_token: Some(SecretString::from(secret)),
                ..AuthSettings::default()
            },
        ];
        for auth in cases {
            let err = validate_configured_secret_collisions(&store, &auth, false)
                .await
                .unwrap_err();
            assert!(
                err.to_string()
                    .contains("collides with an existing API credential")
            );
            assert!(!err.to_string().contains(secret));
        }

        let ignored_initial = AuthSettings {
            token_pepper: Some("pepper".into()),
            initial_root_password: Some(SecretString::from(secret)),
            ..AuthSettings::default()
        };
        validate_configured_secret_collisions(&store, &ignored_initial, false)
            .await
            .unwrap();
        assert!(
            validate_configured_secret_collisions(&store, &ignored_initial, true)
                .await
                .is_err()
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

    #[tokio::test]
    async fn keepalive_listener_enables_socket_keepalive_when_configured() {
        use axum::serve::Listener as _;

        let raw = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut listener = keepalive_listener(raw, 60);
        let addr = listener.local_addr().unwrap();

        let client = tokio::spawn(async move { tokio::net::TcpStream::connect(addr).await });
        let (accepted, _peer) = listener.accept().await;
        let _client = client.await.unwrap().unwrap();

        let sock_ref = socket2::SockRef::from(&accepted);
        assert!(
            sock_ref.keepalive().unwrap(),
            "SO_KEEPALIVE must be enabled when tcp_keepalive_secs > 0"
        );
    }

    #[tokio::test]
    async fn keepalive_listener_disables_socket_keepalive_when_idle_is_zero() {
        use axum::serve::Listener as _;

        let raw = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut listener = keepalive_listener(raw, 0);
        let addr = listener.local_addr().unwrap();

        let client = tokio::spawn(async move { tokio::net::TcpStream::connect(addr).await });
        let (accepted, _peer) = listener.accept().await;
        let _client = client.await.unwrap().unwrap();

        let sock_ref = socket2::SockRef::from(&accepted);
        assert!(
            !sock_ref.keepalive().unwrap(),
            "SO_KEEPALIVE must stay off when tcp_keepalive_secs = 0"
        );
    }
}
