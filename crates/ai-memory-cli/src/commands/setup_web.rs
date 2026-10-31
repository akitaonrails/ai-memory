//! `ai-memory setup --web`: the setup plan as a page in your browser.
//!
//! A one-shot server inside this process, separate from the long-running
//! server and its `/web` UI (which is read-only by design). It serves one page
//! that shows the plan `setup --plan --json` prints, lets the operator tick the
//! agents to wire, and applies the choice by running `ai-memory setup --yes
//! --agent ...` as a child process whose output streams back to the page. Every
//! change therefore goes through the same code path as the terminal command.
//!
//! The page holds no credential and can only start that one command, but a
//! browser is a confused deputy, so the server takes the precautions a local
//! admin page needs:
//!
//! - it binds `127.0.0.1` on a port the OS picks, and answers only a `Host`
//!   of `127.0.0.1:<port>` or `localhost:<port>` (DNS rebinding);
//! - every route sits under a one-time 256-bit secret in the path, printed to
//!   the terminal that started it and compared in constant time;
//! - a change is a POST that must carry this page's `Origin` and a CSRF token
//!   the page reads from its own JSON, which a cross-origin page cannot read;
//! - agent names are checked against the values `--agent` accepts before they
//!   reach the child's argument list;
//! - responses forbid caching, framing and referrers, and the page runs under
//!   a Content-Security-Policy with a per-run nonce and no outside sources;
//! - it stops on Finish, after [`IDLE_TIMEOUT`] without a request, on
//!   Ctrl-C, or, with `--stop-on-stdin-eof`, when the process that started it
//!   closes its standard input (the desktop installer, so the page never
//!   outlives the app).

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use askama::Template;
use axum::Router;
use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use parking_lot::Mutex;
use subtle::ConstantTimeEq as _;
use tokio::io::AsyncBufReadExt as _;
use tokio::sync::Notify;

#[derive(Template)]
#[template(path = "setup_web.html")]
struct SetupPage<'a> {
    i18n: &'a ai_memory_core::i18n::Localizer,
    nonce: &'a str,
}

/// Stop when no request arrived for this long.
const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Keep at most this many output lines from the child.
const MAX_LINES: usize = 5_000;
/// The header carrying the CSRF token on a POST.
const CSRF_HEADER: &str = "x-ai-memory-setup";

/// Starts the `ai-memory setup` child with these arguments and returns it
/// with its stdout and stderr piped. Injectable so tests never run a setup.
pub(crate) type Launch =
    Arc<dyn Fn(&[OsString]) -> std::io::Result<tokio::process::Child> + Send + Sync>;

/// What the child run has produced so far.
#[derive(Debug, Default)]
struct RunState {
    started: bool,
    lines: Vec<String>,
    done: bool,
    ok: bool,
    agents: Vec<String>,
}

pub(crate) struct Wizard {
    i18n: ai_memory_core::i18n::Localizer,
    port: u16,
    secret: String,
    csrf: String,
    nonce: String,
    plan: serde_json::Value,
    /// Every `--agent` value, the only names `apply` accepts.
    accepted: Vec<String>,
    /// `setup` arguments every run carries (`--no-service`).
    setup_flags: Vec<OsString>,
    launch: Launch,
    run: Mutex<RunState>,
    last_seen: Mutex<Instant>,
    shutdown: Notify,
}

impl Wizard {
    /// A wizard for `plan` on `port`, with fresh secrets.
    ///
    /// # Errors
    /// When the OS random source fails.
    pub(crate) fn new(
        port: u16,
        plan: serde_json::Value,
        accepted: Vec<String>,
        setup_flags: Vec<OsString>,
        launch: Launch,
    ) -> Result<Self> {
        let random = |bytes| {
            ai_memory_mcp::auth::generate_token_hex(bytes)
                .map_err(|error| anyhow::anyhow!("generating a random secret: {error}"))
        };
        Ok(Self {
            i18n: ai_memory_core::i18n::Localizer::default(),
            port,
            secret: random(32)?,
            csrf: random(32)?,
            nonce: random(16)?,
            plan,
            accepted,
            setup_flags,
            launch,
            run: Mutex::new(RunState::default()),
            last_seen: Mutex::new(Instant::now()),
            shutdown: Notify::new(),
        })
    }

    /// The page's address.
    pub(crate) fn url(&self) -> String {
        format!("http://127.0.0.1:{}/s/{}/", self.port, self.secret)
    }

    fn host_allowed(&self, headers: &HeaderMap) -> bool {
        let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
            return false;
        };
        host == format!("127.0.0.1:{}", self.port) || host == format!("localhost:{}", self.port)
    }

    fn origin_allowed(&self, headers: &HeaderMap) -> bool {
        let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
            return false;
        };
        origin == format!("http://127.0.0.1:{}", self.port)
            || origin == format!("http://localhost:{}", self.port)
    }

    fn csrf_allowed(&self, headers: &HeaderMap) -> bool {
        headers
            .get(CSRF_HEADER)
            .is_some_and(|value| bool::from(value.as_bytes().ct_eq(self.csrf.as_bytes())))
    }
}

/// Run the wizard until it is finished, idle, or interrupted, and report
/// whether a setup ran and passed.
///
/// # Errors
/// When the listener cannot bind or the server fails.
pub(crate) async fn serve(
    i18n: &ai_memory_core::i18n::Localizer,
    plan: serde_json::Value,
    accepted: Vec<String>,
    setup_flags: Vec<OsString>,
    install_args: Vec<OsString>,
    open_browser: bool,
    stop_on_stdin_eof: bool,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .context(i18n.text("cli-setup-web-binding-the-setup-page-to-127-0-0-1"))?;
    let port = listener.local_addr()?.port();
    let exe = std::env::current_exe()
        .context(i18n.text("cli-service-host-locating-the-running-ai-memory-binary"))?;
    let mut install_args = install_args;
    install_args.extend([OsString::from("--language"), OsString::from(i18n.tag())]);
    let mut wizard = Wizard::new(
        port,
        plan,
        accepted,
        setup_flags,
        child_launcher(exe, install_args),
    )?;
    wizard.i18n = i18n.clone();
    let wizard = Arc::new(wizard);
    let url = wizard.url();
    println!(
        "{}",
        i18n.format(
            "cli-setup-web-open-the-setup-page-to-choose-your-agents-and-apply-url",
            &[("value0", url.to_string())]
        )
    );
    println!(
        "{}", i18n.format("cli-setup-web-it-answers-only-on-this-machine-over-ssh-forward-the-port-first-ssh-l-port-1", &[("value0", format!("{}", port)), ("value1", format!("{}", port))]));
    if open_browser {
        open_in_browser(&url);
    }

    let idle = Arc::clone(&wizard);
    let idle_i18n = i18n.clone();
    tokio::spawn(async move {
        let i18n = &idle_i18n;
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let running = {
                let run = idle.run.lock();
                run.started && !run.done
            };
            if !running && idle.last_seen.lock().elapsed() > IDLE_TIMEOUT {
                println!(
                    "{}",
                    i18n.text("cli-setup-web-the-setup-page-was-idle-for-15-minutes-stopping-it")
                );
                idle.shutdown.notify_one();
                return;
            }
        }
    });

    if stop_on_stdin_eof {
        let watcher = Arc::clone(&wizard);
        tokio::spawn(async move {
            stdin_closed(tokio::io::stdin()).await;
            watcher.shutdown.notify_one();
        });
    }

    let stop = Arc::clone(&wizard);
    axum::serve(listener, router(Arc::clone(&wizard)))
        .with_graceful_shutdown(async move {
            tokio::select! {
                () = stop.shutdown.notified() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        })
        .await
        .context(i18n.text("cli-setup-web-serving-the-setup-page"))?;

    let run = wizard.run.lock();
    match (run.started, run.done, run.ok) {
        (false, _, _) => println!("{}", i18n.text("cli-setup-web-the-setup-page-closed-nothing-was-changed")),
        (true, true, true) => println!("{}", super::setup::localized_next_step(i18n, &run.agents)),
        (true, true, false) => anyhow::bail!(
            "{}", i18n.text("cli-setup-web-setup-finished-with-problems-see-the-output-on-the-page-or-run-ai-memory-set")),
        (true, false, _) => anyhow::bail!("{}", i18n.text("cli-setup-web-the-setup-page-stopped-while-setup-was-still-running")),
    }
    Ok(())
}

/// The real launcher: this binary, pointed at the same install, running
/// `setup` with the page's arguments.
fn child_launcher(exe: PathBuf, install_args: Vec<OsString>) -> Launch {
    Arc::new(move |setup_args: &[OsString]| {
        tokio::process::Command::new(&exe)
            .args(&install_args)
            .arg("setup")
            .args(setup_args)
            .env("NO_COLOR", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
    })
}

/// Ask the desktop to open `url`; on a machine with no desktop, or when the
/// opener fails, the printed address is enough.
fn open_in_browser(url: &str) {
    let opener: Option<(&str, Vec<&str>)> = if cfg!(target_os = "macos") {
        Some(("open", vec![url]))
    } else if cfg!(windows) {
        Some(("rundll32", vec!["url.dll,FileProtocolHandler", url]))
    } else if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    {
        Some(("xdg-open", vec![url]))
    } else {
        None
    };
    if let Some((program, args)) = opener {
        let _ = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

pub(crate) fn router(wizard: Arc<Wizard>) -> Router {
    Router::new()
        .route("/s/{secret}/", get(page))
        .route("/s/{secret}/plan", get(plan))
        .route("/s/{secret}/progress", get(progress))
        .route("/s/{secret}/apply", post(apply))
        .route("/s/{secret}/finish", post(finish))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&wizard),
            guard,
        ))
        .with_state(wizard)
}

/// Every request: a loopback `Host` naming our port, the secret path, and
/// for a POST this page's `Origin` and CSRF token. Then the response headers
/// every answer carries.
async fn guard(State(wizard): State<Arc<Wizard>>, request: Request, next: Next) -> Response {
    if !wizard.host_allowed(request.headers()) {
        return (StatusCode::FORBIDDEN, "unexpected Host header").into_response();
    }
    let secret_ok = request
        .uri()
        .path()
        .strip_prefix("/s/")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(secret, _)| bool::from(secret.as_bytes().ct_eq(wizard.secret.as_bytes())));
    if !secret_ok {
        return StatusCode::NOT_FOUND.into_response();
    }
    if request.method() == Method::POST
        && !(wizard.origin_allowed(request.headers()) && wizard.csrf_allowed(request.headers()))
    {
        return (StatusCode::FORBIDDEN, "refused: not sent by the setup page").into_response();
    }
    *wizard.last_seen.lock() = Instant::now();
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    for (name, value) in [
        (header::CACHE_CONTROL, "no-store"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

async fn page(State(wizard): State<Arc<Wizard>>) -> Response {
    let nonce = &wizard.nonce;
    let csp = format!(
        "default-src 'none'; script-src 'nonce-{nonce}'; style-src 'nonce-{nonce}'; \
         connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );
    let body = match (SetupPage {
        i18n: &wizard.i18n,
        nonce,
    })
    .render()
    {
        Ok(body) => body,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    if let Ok(value) = HeaderValue::from_str(&csp) {
        headers.insert(header::CONTENT_SECURITY_POLICY, value);
    }
    response
}

async fn plan(State(wizard): State<Arc<Wizard>>) -> Response {
    axum::Json(serde_json::json!({
        "plan": wizard.plan,
        "csrf": wizard.csrf,
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
struct ProgressQuery {
    #[serde(default)]
    from: usize,
}

async fn progress(
    State(wizard): State<Arc<Wizard>>,
    Query(query): Query<ProgressQuery>,
) -> Response {
    let run = wizard.run.lock();
    let from = query.from.min(run.lines.len());
    axum::Json(serde_json::json!({
        "lines": &run.lines[from..],
        "next": run.lines.len(),
        "done": run.done,
        "ok": run.ok,
        "agents": run.agents,
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
struct ApplyBody {
    agents: Vec<String>,
}

async fn apply(
    State(wizard): State<Arc<Wizard>>,
    axum::Json(body): axum::Json<ApplyBody>,
) -> Response {
    let mut agents: Vec<String> = Vec::new();
    for agent in body.agents {
        if !wizard.accepted.contains(&agent) {
            return (StatusCode::BAD_REQUEST, format!("unknown agent {agent:?}")).into_response();
        }
        if !agents.contains(&agent) {
            agents.push(agent);
        }
    }
    if agents.is_empty() {
        return (StatusCode::BAD_REQUEST, "pick at least one agent").into_response();
    }
    {
        let mut run = wizard.run.lock();
        if run.started && !run.done {
            return (StatusCode::CONFLICT, "setup is already running").into_response();
        }
        *run = RunState {
            started: true,
            agents: agents.clone(),
            ..RunState::default()
        };
    }
    let mut args: Vec<OsString> = vec!["--yes".into()];
    for agent in &agents {
        args.push("--agent".into());
        args.push(agent.into());
    }
    args.extend(wizard.setup_flags.iter().cloned());
    let mut child = match (wizard.launch)(&args) {
        Ok(child) => child,
        Err(error) => {
            let mut run = wizard.run.lock();
            run.lines
                .push(format!("could not start ai-memory setup: {error}"));
            run.done = true;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not start ai-memory setup",
            )
                .into_response();
        }
    };
    let collector = Arc::clone(&wizard);
    tokio::spawn(async move {
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out = collect_lines(Arc::clone(&collector), stdout);
        let err = collect_lines(Arc::clone(&collector), stderr);
        tokio::join!(out, err);
        let ok = child.wait().await.is_ok_and(|status| status.success());
        let mut run = collector.run.lock();
        run.ok = ok;
        run.done = true;
    });
    StatusCode::ACCEPTED.into_response()
}

async fn collect_lines(wizard: Arc<Wizard>, stream: Option<impl tokio::io::AsyncRead + Unpin>) {
    let Some(stream) = stream else {
        return;
    };
    let mut lines = tokio::io::BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut run = wizard.run.lock();
        if run.lines.len() < MAX_LINES {
            run.lines.push(strip_ansi(&line));
        }
    }
}

/// Drop terminal escape sequences (`ESC [ ... letter`), so a coloured log
/// line reads as text on the page.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

/// Resolve once `input` reaches end of file or fails; whatever it carries
/// before that is ignored.
async fn stdin_closed(mut input: impl tokio::io::AsyncRead + Unpin) {
    use tokio::io::AsyncReadExt as _;
    let mut buffer = [0_u8; 256];
    while matches!(input.read(&mut buffer).await, Ok(read) if read > 0) {}
}

// The page posts `{}` here. Taking the body reads it off the socket: a
// response sent with the request body still unread, on a connection that then
// closes, can reach the client as a TCP reset instead of the response.
async fn finish(State(wizard): State<Arc<Wizard>>, _body: axum::body::Bytes) -> Response {
    {
        let run = wizard.run.lock();
        if run.started && !run.done {
            return (StatusCode::CONFLICT, "setup is still running").into_response();
        }
    }
    wizard.shutdown.notify_one();
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt as _;

    const PORT: u16 = 41_234;

    /// A launcher that records its arguments and runs a stand-in child which
    /// prints two lines and exits with `code`.
    fn recording(code: i32) -> (Launch, Arc<Mutex<Vec<Vec<String>>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&calls);
        let launch: Launch = Arc::new(move |args: &[OsString]| {
            seen.lock().push(
                args.iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect(),
            );
            let script =
                format!("echo == claude-code; echo '\u{1b}[1mdone\u{1b}[0m' 1>&2; exit {code}");
            let mut command = if cfg!(windows) {
                let mut command = tokio::process::Command::new("cmd");
                command.args([
                    "/C",
                    &format!("echo == claude-code & echo done 1>&2 & exit /b {code}"),
                ]);
                command
            } else {
                let mut command = tokio::process::Command::new("sh");
                command.args(["-c", &script]);
                command
            };
            command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
        });
        (launch, calls)
    }

    fn wizard(launch: Launch) -> Arc<Wizard> {
        Arc::new(
            Wizard::new(
                PORT,
                serde_json::json!({"agents": [{"agent": "claude-code"}]}),
                vec!["claude-code".into(), "codex".into()],
                vec!["--no-service".into()],
                launch,
            )
            .unwrap(),
        )
    }

    fn path(wizard: &Wizard, rest: &str) -> String {
        format!("/s/{}/{rest}", wizard.secret)
    }

    fn get(uri: &str, host: &str) -> Request {
        Request::builder()
            .uri(uri)
            .header(header::HOST, host)
            .body(Body::empty())
            .unwrap()
    }

    fn post(wizard: &Wizard, rest: &str) -> axum::http::request::Builder {
        Request::builder()
            .method(Method::POST)
            .uri(path(wizard, rest))
            .header(header::HOST, format!("127.0.0.1:{PORT}"))
            .header(header::CONTENT_TYPE, "application/json")
    }

    async fn send(wizard: &Arc<Wizard>, request: Request) -> (StatusCode, HeaderMap, String) {
        let response = router(Arc::clone(wizard)).oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    fn legit(wizard: &Wizard, rest: &str, body: &str) -> Request {
        post(wizard, rest)
            .header(header::ORIGIN, format!("http://127.0.0.1:{PORT}"))
            .header(CSRF_HEADER, &wizard.csrf)
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    /// The page needs the loopback Host for our port and the secret path; it
    /// carries a CSP with this run's nonce, and the nonce reaches its tags.
    #[tokio::test]
    async fn the_page_is_served_only_to_our_host_and_secret() {
        let wizard = wizard(recording(0).0);
        let host = format!("127.0.0.1:{PORT}");
        let (status, headers, body) = send(&wizard, get(&path(&wizard, ""), &host)).await;
        assert_eq!(status, StatusCode::OK);
        let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        assert!(csp.contains(&format!("'nonce-{}'", wizard.nonce)), "{csp}");
        assert!(csp.starts_with("default-src 'none'"), "{csp}");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
        assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
        assert!(body.contains(&format!("nonce=\"{}\"", wizard.nonce)));
        assert!(!body.contains("__NONCE__"));

        let (status, _, _) = send(
            &wizard,
            get(&path(&wizard, ""), &format!("localhost:{PORT}")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "localhost names the same server");

        // DNS rebinding: an attacker's name resolving to 127.0.0.1.
        for host in [
            format!("evil.example:{PORT}"),
            "127.0.0.1".to_owned(),
            format!("127.0.0.1:{}", PORT + 1),
        ] {
            let (status, _, _) = send(&wizard, get(&path(&wizard, ""), &host)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{host}");
        }
        let wrong = format!("/s/{}/", "0".repeat(64));
        for uri in [wrong.as_str(), "/", "/s/", "/plan"] {
            let (status, _, _) = send(&wizard, get(uri, &host)).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        }
    }

    /// The plan JSON hands the page its CSRF token; it is readable only by a
    /// same-origin page, since nothing here allows a cross-origin read.
    #[tokio::test]
    async fn the_plan_carries_the_csrf_token_and_no_cors_grant() {
        let wizard = wizard(recording(0).0);
        let (status, headers, body) = send(
            &wizard,
            get(&path(&wizard, "plan"), &format!("127.0.0.1:{PORT}")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(!headers.contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["csrf"], wizard.csrf);
        assert_eq!(json["plan"]["agents"][0]["agent"], "claude-code");
    }

    /// A change needs this page's Origin and the CSRF token; each missing or
    /// foreign piece is refused and starts nothing. Agent names outside the
    /// accepted list (including option-like strings) and an empty choice are
    /// refused before any child starts.
    #[tokio::test]
    async fn apply_refuses_cross_site_and_unknown_input_and_starts_nothing() {
        let (launch, calls) = recording(0);
        let wizard = wizard(launch);
        let body = r#"{"agents":["claude-code"]}"#;
        let cases = [
            post(&wizard, "apply")
                .header(CSRF_HEADER, &wizard.csrf)
                .body(Body::from(body))
                .unwrap(),
            post(&wizard, "apply")
                .header(header::ORIGIN, "http://evil.example")
                .header(CSRF_HEADER, &wizard.csrf)
                .body(Body::from(body))
                .unwrap(),
            post(&wizard, "apply")
                .header(header::ORIGIN, format!("http://127.0.0.1:{PORT}"))
                .body(Body::from(body))
                .unwrap(),
            post(&wizard, "apply")
                .header(header::ORIGIN, format!("http://127.0.0.1:{PORT}"))
                .header(CSRF_HEADER, "0".repeat(64))
                .body(Body::from(body))
                .unwrap(),
        ];
        for request in cases {
            let (status, _, _) = send(&wizard, request).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
        }
        for bad in [
            r#"{"agents":["--force"]}"#,
            r#"{"agents":["claude-code","rm -rf"]}"#,
            r#"{"agents":[]}"#,
        ] {
            let (status, _, _) = send(&wizard, legit(&wizard, "apply", bad)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        }
        assert!(
            calls.lock().is_empty(),
            "nothing may start: {:?}",
            calls.lock()
        );
        assert!(!wizard.run.lock().started);
    }

    /// The legitimate path: the child gets `--yes`, each picked agent once as
    /// `--agent`, and the wizard's flags; its output (escape codes stripped)
    /// and exit status reach the progress endpoint. A second apply while it
    /// runs is refused, and so is finishing before it ends.
    #[tokio::test]
    async fn apply_runs_setup_with_the_picked_agents_and_streams_its_output() {
        let (launch, calls) = recording(0);
        let wizard = wizard(launch);
        let (status, _, _) = send(
            &wizard,
            legit(
                &wizard,
                "apply",
                r#"{"agents":["codex","claude-code","codex"]}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(
            calls.lock().first().cloned(),
            Some(
                [
                    "--yes",
                    "--agent",
                    "codex",
                    "--agent",
                    "claude-code",
                    "--no-service"
                ]
                .map(str::to_owned)
                .to_vec()
            )
        );
        let host = format!("127.0.0.1:{PORT}");
        let deadline = Instant::now() + Duration::from_secs(20);
        let progress = loop {
            let (_, _, body) = send(&wizard, get(&path(&wizard, "progress?from=0"), &host)).await;
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            if json["done"] == true {
                break json;
            }
            assert!(Instant::now() < deadline, "the child never finished");
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(progress["ok"], true);
        let lines: Vec<&str> = progress["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| line.as_str().unwrap().trim())
            .collect();
        assert!(lines.contains(&"== claude-code"), "{lines:?}");
        assert!(lines.contains(&"done"), "escape codes stripped: {lines:?}");
        assert_eq!(
            progress["agents"],
            serde_json::json!(["codex", "claude-code"])
        );

        let (status, _, _) = send(&wizard, legit(&wizard, "finish", "{}")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn a_second_apply_or_a_finish_waits_for_the_running_setup() {
        let wizard = wizard(recording(0).0);
        wizard.run.lock().started = true;
        let (status, _, _) =
            send(&wizard, legit(&wizard, "apply", r#"{"agents":["codex"]}"#)).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _, _) = send(&wizard, legit(&wizard, "finish", "{}")).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    /// The stdin watcher waits through input and returns at end of file.
    #[tokio::test]
    async fn the_stdin_watcher_returns_when_the_writer_closes() {
        use tokio::io::AsyncWriteExt as _;
        let (mut writer, reader) = tokio::io::duplex(64);
        let watcher = tokio::spawn(stdin_closed(reader));
        writer.write_all(b"still here\n").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!watcher.is_finished(), "input alone must not stop it");
        drop(writer);
        tokio::time::timeout(Duration::from_secs(5), watcher)
            .await
            .expect("the watcher returns at end of file")
            .unwrap();
    }

    #[test]
    fn escape_sequences_are_stripped() {
        assert_eq!(strip_ansi("\u{1b}[1;32mok\u{1b}[0m done"), "ok done");
        assert_eq!(strip_ansi("plain"), "plain");
    }
}
