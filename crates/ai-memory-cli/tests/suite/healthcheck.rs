//! The container healthcheck must be cheap, bounded and independent of SQLite.
use std::{
    path::Path,
    process::Output,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::Redirect,
    routing::{get, post},
};
use serde_json::json;
use tokio::net::TcpListener;

use crate::e2e_support::{free_port, hermetic};

const PROXY_ENV: [&str; 8] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

fn run_command(
    bind: &str,
    args: &[String],
    envs: &[(String, String)],
    data: &Path,
    home: &Path,
) -> Output {
    let mut cmd = hermetic(env!("CARGO_BIN_EXE_ai-memory"));
    cmd.env("AI_MEMORY_HOME", home)
        .env("AI_MEMORY_DATA_DIR", data)
        .env("AI_MEMORY_BIND", bind)
        .arg("healthcheck")
        .args(args);
    for key in PROXY_ENV {
        cmd.env_remove(key);
    }
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().unwrap()
}

async fn healthcheck(bind: &str, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let bind = bind.to_owned();
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    let envs = envs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        let data = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        run_command(&bind, &args, &envs, data.path(), home.path())
    })
    .await
    .unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn local_probe_uses_bind_and_base_path_without_authorization_or_status() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bind = listener.local_addr().unwrap().to_string();
    let app = Router::new().route(
        "/wiki/healthz",
        get(|headers: HeaderMap| async move {
            assert!(!headers.contains_key(AUTHORIZATION));
            Json(json!({ "status": "ok" }))
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let output = healthcheck(
        &bind,
        &[],
        &[
            ("AI_MEMORY_BASE_PATH", "/wiki/"),
            ("AI_MEMORY_SERVER_URL", "http://127.0.0.1:1/not-local"),
            ("AI_MEMORY_AUTH_TOKEN", "synthetic-local-token"),
        ],
    )
    .await;
    assert_success(&output);
    server.abort();
}

#[tokio::test]
async fn explicit_url_uses_the_remote_target_and_configured_auth() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/healthz",
        get(|headers: HeaderMap| async move {
            if headers
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                == Some("Bearer synthetic-remote-token")
            {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::UNAUTHORIZED
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let unavailable_bind = format!("127.0.0.1:{}", free_port());

    let output = healthcheck(
        &unavailable_bind,
        &["--url", &base],
        &[("AI_MEMORY_AUTH_TOKEN", "synthetic-remote-token")],
    )
    .await;
    assert_success(&output);
    server.abort();
}

#[tokio::test]
async fn non_success_and_unreachable_exit_nonzero() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bind = listener.local_addr().unwrap().to_string();
    let app = Router::new().route("/healthz", get(|| async { "plain text is healthy" }));
    let ok_listener = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    assert_success(&healthcheck(&bind, &[], &[]).await);
    ok_listener.abort();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let failing_bind = listener.local_addr().unwrap().to_string();
    let app = Router::new().route(
        "/healthz",
        get(|| async { (StatusCode::SERVICE_UNAVAILABLE, "down") }),
    );
    let failing = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let output = healthcheck(&failing_bind, &[], &[]).await;
    assert!(!output.status.success(), "non-2xx must fail the probe");
    failing.abort();

    let output = healthcheck(
        &format!("127.0.0.1:{}", free_port()),
        &["--timeout-ms", "200"],
        &[],
    )
    .await;
    assert!(!output.status.success(), "unreachable must fail the probe");
}

async fn hanging_server() -> (
    String,
    Arc<Mutex<Option<Instant>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bind = listener.local_addr().unwrap().to_string();
    let request_started = Arc::new(Mutex::new(None));
    let handler_started = request_started.clone();
    let app = Router::new().route(
        "/healthz",
        get(move || {
            let request_started = handler_started.clone();
            async move {
                *request_started.lock().unwrap() = Some(Instant::now());
                tokio::time::sleep(Duration::from_secs(6)).await;
                "late"
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (bind, request_started, server)
}

fn assert_actual_timeout(output: &Output, request_started: &Mutex<Option<Instant>>) {
    assert!(!output.status.success(), "a timed-out probe must fail");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("healthcheck timed out"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let elapsed = request_started.lock().unwrap().unwrap().elapsed();
    assert!(
        elapsed < Duration::from_millis(1500),
        "the 100 ms request deadline was not applied: {elapsed:?}"
    );
}

#[tokio::test]
async fn flag_timeout_overrides_a_longer_env_timeout() {
    let (bind, request_started, server) = hanging_server().await;
    let output = healthcheck(
        &bind,
        &["--timeout-ms", "100"],
        &[("AI_MEMORY_HEALTHCHECK_TIMEOUT_MS", "2500")],
    )
    .await;
    assert_actual_timeout(&output, &request_started);
    server.abort();
}

#[tokio::test]
async fn env_timeout_is_applied_below_the_default() {
    let (bind, request_started, server) = hanging_server().await;
    let output = healthcheck(&bind, &[], &[("AI_MEMORY_HEALTHCHECK_TIMEOUT_MS", "100")]).await;
    assert_actual_timeout(&output, &request_started);
    server.abort();
}

#[tokio::test]
async fn flag_timeout_wins_over_a_shorter_env_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bind = listener.local_addr().unwrap().to_string();
    let app = Router::new().route(
        "/healthz",
        get(|| async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            StatusCode::OK
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let output = healthcheck(
        &bind,
        &["--timeout-ms", "1500"],
        &[("AI_MEMORY_HEALTHCHECK_TIMEOUT_MS", "1")],
    )
    .await;
    assert_success(&output);
    server.abort();
}

#[tokio::test]
async fn local_failure_cannot_be_masked_by_a_remote_2xx() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route("/healthz", get(|| async { StatusCode::OK }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let local_bind = format!("127.0.0.1:{}", free_port());

    let remote_output = healthcheck(&local_bind, &["--url", &remote], &[]).await;
    assert_success(&remote_output);
    let local_output = healthcheck(
        &local_bind,
        &["--timeout-ms", "200"],
        &[("AI_MEMORY_SERVER_URL", &remote)],
    )
    .await;
    assert!(
        !local_output.status.success(),
        "the configured remote 2xx must not mask a dead local listener"
    );
    server.abort();
}

#[tokio::test]
async fn local_probe_ignores_environment_proxies() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bind = listener.local_addr().unwrap().to_string();
    let app = Router::new().route("/healthz", get(|| async { StatusCode::OK }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", proxy_listener.local_addr().unwrap());
    let proxy_connections = Arc::new(AtomicUsize::new(0));
    let accepted = proxy_connections.clone();
    let proxy_server = tokio::spawn(async move {
        loop {
            let (stream, _) = proxy_listener.accept().await.unwrap();
            accepted.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(6)).await;
                drop(stream);
            });
        }
    });

    let output = healthcheck(
        &bind,
        &[],
        &[
            ("HTTP_PROXY", &proxy),
            ("HTTPS_PROXY", &proxy),
            ("ALL_PROXY", &proxy),
            ("http_proxy", &proxy),
            ("https_proxy", &proxy),
            ("all_proxy", &proxy),
        ],
    )
    .await;
    assert_success(&output);
    assert_eq!(proxy_connections.load(Ordering::SeqCst), 0);
    proxy_server.abort();
    server.abort();
}

#[tokio::test]
async fn redirect_is_an_error_and_the_destination_is_not_contacted() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bind = listener.local_addr().unwrap().to_string();
    let destination_hits = Arc::new(AtomicUsize::new(0));
    let hits = destination_hits.clone();
    let app = Router::new()
        .route(
            "/healthz",
            get(|| async { Redirect::temporary("/destination") }),
        )
        .route(
            "/destination",
            get(move || {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let output = healthcheck(&bind, &[], &[]).await;
    assert!(!output.status.success(), "3xx must fail the probe");
    assert_eq!(destination_hits.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn local_probe_neither_refreshes_nor_rewrites_synthetic_oidc_state() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bind = listener.local_addr().unwrap().to_string();
    let app = Router::new().route(
        "/healthz",
        get(|headers: HeaderMap| async move {
            assert!(!headers.contains_key(AUTHORIZATION));
            StatusCode::OK
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let refresh_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let refresh_endpoint = format!("http://{}/token", refresh_listener.local_addr().unwrap());
    let refresh_hits = Arc::new(AtomicUsize::new(0));
    let hits = refresh_hits.clone();
    let refresh_app = Router::new().route(
        "/token",
        post(move || {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                Json(json!({
                    "access_token": "new-synthetic-access",
                    "refresh_token": "rotated-synthetic-refresh",
                    "expires_in": 300
                }))
            }
        }),
    );
    let refresh_server =
        tokio::spawn(async move { axum::serve(refresh_listener, refresh_app).await.unwrap() });

    let data = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let auth_path = data.path().join("auth.json");
    let fixture = serde_json::to_vec_pretty(&json!({
        "oidc": {
            "type": "oauth",
            "access": "synthetic-access",
            "refresh": "synthetic-refresh",
            "expires": 0,
            "issuer": "https://issuer.invalid",
            "client_id": "synthetic-client",
            "token_endpoint": refresh_endpoint
        }
    }))
    .unwrap();
    std::fs::write(&auth_path, &fixture).unwrap();

    let data_path = data.path().to_owned();
    let home_path = home.path().to_owned();
    let output =
        tokio::task::spawn_blocking(move || run_command(&bind, &[], &[], &data_path, &home_path))
            .await
            .unwrap();
    assert_success(&output);
    assert_eq!(refresh_hits.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(&auth_path).unwrap(), fixture);
    assert!(
        !data.path().join("logs").exists(),
        "the local probe must return before file logging is initialized"
    );
    refresh_server.abort();
    server.abort();
}

#[tokio::test]
async fn healthcheck_uses_the_current_thread_runtime() {
    assert_eq!(
        ai_memory_cli::runtime_flavor::RuntimeFlavor::for_argv(&[
            "ai-memory".to_owned(),
            "healthcheck".to_owned()
        ]),
        ai_memory_cli::runtime_flavor::RuntimeFlavor::CurrentThread
    );
}
