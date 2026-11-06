//! Waking a bot straight from an observer webhook: the `projects` filter,
//! headers read from a file, and the message a `message_send` observer is told
//! about.
//!
//! A bot such as a chat-ops agent wakes when mail reaches its own project. The
//! webhook therefore has to fire only for that project, authenticate to the
//! bot's endpoint without a relay in between, and say which message arrived so
//! the bot can pop exactly that one. The header value is a secret: these tests
//! plant one and assert it never leaves the request it belongs to.

use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ai_memory_core::{ActorContext, AgentKind, MessageNature, NewAgentMessage, PagePath};
use ai_memory_store::Store;
use ai_memory_wiki::{
    AdmissionChain, AdmissionContext, AdmissionOp, FailurePolicy, Markdown, WebhookConfig,
    WebhookHeader, Wiki,
};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tempfile::TempDir;
use tracing_subscriber::fmt::MakeWriter;

type Seen = Arc<Mutex<Vec<(HeaderMap, Value)>>>;

async fn spawn_server(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// A webhook endpoint that records every request's headers and JSON body.
async fn recorder() -> (String, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let app = Router::new().route(
        "/hook",
        post(move |headers: HeaderMap, Json(payload): Json<Value>| {
            let sink = sink.clone();
            async move {
                sink.lock().unwrap().push((headers, payload));
                StatusCode::NO_CONTENT
            }
        }),
    );
    (format!("{}/hook", spawn_server(app).await), seen)
}

fn hook(url: &str, events: Vec<AdmissionOp>, blocking: bool) -> WebhookConfig {
    WebhookConfig {
        name: "bot-wake".into(),
        url: url.to_string(),
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Ignore,
        events,
        blocking,
        projects: Vec::new(),
        headers: Vec::new(),
    }
}

fn secret_file(dir: &TempDir, name: &str, contents: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(contents.as_bytes()).unwrap();
    path
}

fn message_ctx(workspace: &str, project: &str) -> AdmissionContext {
    AdmissionContext {
        op: AdmissionOp::MessageSend,
        workspace: workspace.into(),
        project: project.into(),
        ..AdmissionContext::default()
    }
}

/// Observer dispatch is fire-and-forget; wait (bounded) for `n` requests.
async fn wait_for(seen: &Seen, n: usize) {
    for _ in 0..200 {
        if seen.lock().unwrap().len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Headers the HTTP client adds on its own; everything else came from us.
fn caller_headers(headers: &HeaderMap) -> Vec<String> {
    let transport = [
        "host",
        "content-length",
        "accept",
        "accept-encoding",
        "user-agent",
        "connection",
    ];
    let mut names: Vec<String> = headers
        .keys()
        .map(|k| k.as_str().to_string())
        .filter(|k| !transport.contains(&k.as_str()))
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn a_projects_filter_fires_for_a_listed_project() {
    let (url, seen) = recorder().await;
    let mut cfg = hook(&url, vec![AdmissionOp::MessageSend], true);
    cfg.projects = vec!["fuad/grok-bot".into()];
    let chain = AdmissionChain::new(vec![cfg]).unwrap();

    chain
        .notify(None, &message_ctx("fuad", "grok-bot"))
        .await
        .unwrap();

    let rec = seen.lock().unwrap();
    assert_eq!(rec.len(), 1, "a listed project must reach the webhook");
    assert_eq!(rec[0].0["x-memory-op"], "message_send");
}

#[tokio::test]
async fn a_projects_filter_matches_workspace_wide_events_by_workspace() {
    let (url, seen) = recorder().await;
    let mut cfg = hook(&url, vec![AdmissionOp::PurgeWorkspace], true);
    cfg.projects = vec!["fuad/grok-bot".into()];
    let chain = AdmissionChain::new(vec![cfg]).unwrap();

    let purge = |workspace: &str| AdmissionContext {
        op: AdmissionOp::PurgeWorkspace,
        workspace: workspace.into(),
        ..AdmissionContext::default()
    };
    chain.notify(None, &purge("fuad")).await.unwrap();
    chain.notify(None, &purge("other")).await.unwrap();

    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "purging the workspace that holds a listed project is that project's event"
    );
}

#[tokio::test]
async fn a_projects_filter_skips_an_unlisted_project_on_every_path() {
    let (url, seen) = recorder().await;
    let mut blocking = hook(&url, vec![AdmissionOp::MessageSend], true);
    blocking.projects = vec!["fuad/grok-bot".into()];
    let mut observer = hook(&url, vec![AdmissionOp::MessageSend], false);
    observer.name = "bot-wake-observer".into();
    observer.projects = vec!["fuad/grok-bot".into()];
    let chain = AdmissionChain::new(vec![blocking, observer]).unwrap();

    let ctx = message_ctx("fuad", "next");
    chain.notify(None, &ctx).await.unwrap();
    chain.dispatch_notify_observers(None, &ctx);
    chain.dispatch_async(None, &json!({}), "", &ctx);
    // The control proves the observer path is live: the listed project reaches
    // both hooks (an `ignore` hook is an observer there too), so nothing from
    // `fuad/next` in the record is the filter, not a slow spawn.
    chain.dispatch_notify_observers(None, &message_ctx("fuad", "grok-bot"));
    wait_for(&seen, 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let rec = seen.lock().unwrap();
    assert_eq!(
        rec.len(),
        2,
        "only the listed project may reach the webhooks"
    );
    for (_, payload) in rec.iter() {
        assert_eq!(payload["ctx"]["project"], "grok-bot");
    }
}

#[test]
fn a_malformed_projects_entry_refuses_the_chain() {
    for entry in ["grok-bot", "fuad/", "/grok-bot", "fuad/grok/bot", ""] {
        let mut cfg = hook(
            "http://127.0.0.1:9/hook",
            vec![AdmissionOp::MessageSend],
            false,
        );
        cfg.projects = vec![entry.to_string()];
        let err = AdmissionChain::new(vec![cfg])
            .expect_err("a malformed entry must fail construction")
            .to_string();
        assert!(err.contains("bot-wake"), "error names the webhook: {err}");
        assert!(
            err.contains(&format!("'{entry}'")),
            "error names the entry: {err}"
        );
    }
}

#[tokio::test]
async fn a_webhook_without_the_new_keys_behaves_as_before() {
    let (url, seen) = recorder().await;
    let chain =
        AdmissionChain::new(vec![hook(&url, vec![AdmissionOp::MessageSend], true)]).unwrap();

    chain
        .notify(None, &message_ctx("fuad", "next"))
        .await
        .unwrap();
    chain
        .notify(None, &message_ctx("fuad", "grok-bot"))
        .await
        .unwrap();

    let rec = seen.lock().unwrap();
    assert_eq!(rec.len(), 2, "no filter means every project");
    for (headers, _) in rec.iter() {
        assert_eq!(
            caller_headers(headers),
            vec!["content-type".to_string(), "x-memory-op".to_string()],
            "no header beyond the existing two"
        );
    }
}

#[tokio::test]
async fn a_header_is_read_from_its_file_once_at_construction() {
    let dir = TempDir::new().unwrap();
    let path = secret_file(&dir, "wake", "Bearer s3cr3t\n");
    let (url, seen) = recorder().await;
    let mut cfg = hook(&url, vec![AdmissionOp::MessageSend], true);
    cfg.headers = vec![WebhookHeader {
        name: "Authorization".into(),
        value_file: path.clone(),
    }];
    let chain = AdmissionChain::new(vec![cfg]).unwrap();

    chain
        .notify(None, &message_ctx("fuad", "grok-bot"))
        .await
        .unwrap();
    std::fs::write(&path, "Bearer changed\n").unwrap();
    chain
        .notify(None, &message_ctx("fuad", "grok-bot"))
        .await
        .unwrap();

    let rec = seen.lock().unwrap();
    assert_eq!(rec.len(), 2);
    for (headers, _) in rec.iter() {
        assert_eq!(
            headers["authorization"], "Bearer s3cr3t",
            "the value is trimmed and fixed at construction"
        );
    }
}

#[tokio::test]
async fn a_header_rides_on_every_send_path() {
    let dir = TempDir::new().unwrap();
    let path = secret_file(&dir, "wake", "Bearer s3cr3t");
    let (url, seen) = recorder().await;
    let with_header = |mut cfg: WebhookConfig, name: &str| {
        cfg.name = name.into();
        cfg.headers = vec![WebhookHeader {
            name: "Authorization".into(),
            value_file: path.clone(),
        }];
        cfg
    };
    let chain = AdmissionChain::new(vec![
        with_header(hook(&url, vec![AdmissionOp::WritePage], true), "writer"),
        with_header(hook(&url, vec![AdmissionOp::Delete], true), "notifier"),
        with_header(
            hook(&url, vec![AdmissionOp::MessageSend], false),
            "observer",
        ),
        with_header(hook(&url, vec![AdmissionOp::Consolidate], false), "mirror"),
    ])
    .unwrap();

    let path_page = PagePath::new("notes/a.md").unwrap();
    let mut markdown = Markdown {
        frontmatter: json!({"title": "A"}),
        body: "body\n".into(),
    };
    let op = |op| AdmissionContext {
        op,
        ..AdmissionContext::default()
    };
    chain
        .run(&path_page, &mut markdown, &op(AdmissionOp::WritePage))
        .await
        .unwrap();
    chain
        .notify(Some("notes/a.md"), &op(AdmissionOp::Delete))
        .await
        .unwrap();
    chain.dispatch_notify_observers(None, &op(AdmissionOp::MessageSend));
    chain.dispatch_async(
        Some("notes/a.md"),
        &json!({}),
        "",
        &op(AdmissionOp::Consolidate),
    );
    wait_for(&seen, 4).await;

    let rec = seen.lock().unwrap();
    assert_eq!(
        rec.len(),
        4,
        "run, notify, observer and async paths all fire"
    );
    for (headers, _) in rec.iter() {
        assert_eq!(headers["authorization"], "Bearer s3cr3t");
    }
}

#[test]
fn an_invalid_header_refuses_the_chain_without_echoing_a_secret() {
    const PLANTED: &str = "PLANTED-SECRET-9f2c";
    let dir = TempDir::new().unwrap();
    let valid = secret_file(&dir, "valid", PLANTED);
    let empty = secret_file(&dir, "empty", "  \n");
    let control = secret_file(&dir, "control", &format!("{PLANTED}\u{7}tail"));
    let missing = dir.path().join("missing");
    let cases: Vec<(&str, std::path::PathBuf, bool)> = vec![
        ("Authorization", missing.clone(), true),
        ("Authorization", empty.clone(), true),
        ("Authorization", control.clone(), true),
        ("X-Memory-Op", valid.clone(), false),
        ("Content-Type", valid.clone(), false),
        ("Content-Length", valid.clone(), false),
        ("Host", valid.clone(), false),
        ("bad name", valid.clone(), false),
    ];
    for (name, path, names_path) in cases {
        let mut cfg = hook(
            "http://127.0.0.1:9/hook",
            vec![AdmissionOp::MessageSend],
            false,
        );
        cfg.headers = vec![WebhookHeader {
            name: name.into(),
            value_file: path.clone(),
        }];
        let err = AdmissionChain::new(vec![cfg])
            .expect_err("an invalid header must fail construction")
            .to_string();
        assert!(
            err.contains("bot-wake"),
            "{name}: error names the webhook: {err}"
        );
        if names_path {
            assert!(
                err.contains(&path.display().to_string()),
                "{name}: error names the file: {err}"
            );
        }
        assert!(
            !err.contains(PLANTED),
            "{name}: error leaked the secret: {err}"
        );
    }
}

#[derive(Clone, Default)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturedLog {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn a_planted_secret_never_reaches_logs_debug_or_errors() {
    const PLANTED: &str = "PLANTED-SECRET-41ab";
    let captured = CapturedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    // Under plain `cargo test` (the Windows CI job) every test in this binary
    // shares one process, and tracing-core caches each callsite's interest
    // process-wide. With this the only registered dispatcher, a callsite first
    // reached on another test's thread is judged against that thread's
    // default (none) and pinned to `never`, so this capture stays empty. As in
    // `ai-memory-llm`'s `capture_info_logs`, a second live `Dispatch` makes
    // every registration consult all dispatchers, and
    // `rebuild_interest_cache()` heals a callsite already pinned.
    let _companion = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    tracing::callsite::rebuild_interest_cache();

    // A port that refuses connections: bound, then released.
    let refused = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let dir = TempDir::new().unwrap();
    let path = secret_file(&dir, "wake", &format!("Bearer {PLANTED}"));
    let mut cfg = hook(
        &format!("http://{refused}/hook"),
        vec![AdmissionOp::MessageSend],
        true,
    );
    cfg.failure_policy = FailurePolicy::Reject;
    cfg.headers = vec![WebhookHeader {
        name: "Authorization".into(),
        value_file: path,
    }];
    let config_debug = format!("{cfg:?}");
    let chain = AdmissionChain::new(vec![cfg]).unwrap();
    let chain_debug = format!("{chain:?}");

    let err = chain
        .notify(None, &message_ctx("fuad", "grok-bot"))
        .await
        .expect_err("a refused connection under reject policy is an error")
        .to_string();

    let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("bot-wake"),
        "the failure is logged by name: {logs}"
    );
    for (what, text) in [
        ("logs", logs.as_str()),
        ("config Debug", config_debug.as_str()),
        ("chain Debug", chain_debug.as_str()),
        ("error", err.as_str()),
    ] {
        assert!(!text.contains(PLANTED), "{what} leaked the secret: {text}");
    }
}

#[tokio::test]
async fn a_redirect_to_another_origin_never_carries_the_header() {
    let (elsewhere, elsewhere_seen) = recorder().await;
    let first: Seen = Arc::new(Mutex::new(Vec::new()));
    let sink = first.clone();
    let target = elsewhere.clone();
    let app = Router::new().route(
        "/hook",
        post(move |headers: HeaderMap, Json(payload): Json<Value>| {
            let sink = sink.clone();
            let target = target.clone();
            async move {
                sink.lock().unwrap().push((headers, payload));
                Redirect::temporary(&target).into_response()
            }
        }),
    );
    let url = format!("{}/hook", spawn_server(app).await);
    let dir = TempDir::new().unwrap();
    let path = secret_file(&dir, "wake", "Bearer s3cr3t");
    let mut cfg = hook(&url, vec![AdmissionOp::MessageSend], true);
    cfg.headers = vec![WebhookHeader {
        name: "Authorization".into(),
        value_file: path,
    }];
    let chain = AdmissionChain::new(vec![cfg]).unwrap();

    chain
        .notify(None, &message_ctx("fuad", "grok-bot"))
        .await
        .unwrap();

    assert_eq!(
        first.lock().unwrap()[0].0["authorization"],
        "Bearer s3cr3t",
        "the configured origin gets the header"
    );
    assert!(
        elsewhere_seen.lock().unwrap().is_empty(),
        "another origin must receive nothing"
    );
}

struct MailFixture {
    _tmp: TempDir,
    wiki: Wiki,
    seen: Seen,
    ws: ai_memory_core::WorkspaceId,
    sender: ai_memory_core::ProjectId,
    bot: ai_memory_core::ProjectId,
}

/// A store-backed wiki whose observer webhook watches `message_send`.
async fn mail_fixture(events: Vec<AdmissionOp>) -> MailFixture {
    let (url, seen) = recorder().await;
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store.writer.get_or_create_workspace("fuad").await.unwrap();
    let sender = store
        .writer
        .get_or_create_project(ws, "next", None)
        .await
        .unwrap();
    let bot = store
        .writer
        .get_or_create_project(ws, "grok-bot", None)
        .await
        .unwrap();
    let chain = AdmissionChain::new(vec![hook(&url, events, false)]).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone())
        .with_admission_chain(chain);
    MailFixture {
        _tmp: tmp,
        wiki,
        seen,
        ws,
        sender,
        bot,
    }
}

fn mail(f: &MailFixture, to_agent: Option<AgentKind>) -> NewAgentMessage {
    NewAgentMessage {
        from_workspace_id: f.ws,
        from_project_id: f.sender,
        from_agent: AgentKind::Other,
        from_session_id: None,
        from_owner_user: None,
        to_workspace_id: f.ws,
        to_project_id: f.bot,
        to_agent,
        subject: Some("deploy request".into()),
        body: "please deploy the hotfix".into(),
        nature: MessageNature::Unknown,
        evidence: None,
    }
}

#[tokio::test]
async fn the_web_composer_and_edit_tell_observers_which_message_arrived() {
    let f = mail_fixture(vec![AdmissionOp::MessageSend]).await;

    let id = f
        .wiki
        .send_agent_message(mail(&f, Some(AgentKind::Codex)), ActorContext::anonymous())
        .await
        .unwrap();
    wait_for(&f.seen, 1).await;
    assert!(
        f.wiki
            .begin_agent_message_edit(f.ws, f.bot, id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        f.wiki
            .save_agent_message_edit(
                f.ws,
                f.bot,
                id,
                Some("deploy request".into()),
                "please deploy the hotfix today".into(),
                ActorContext::anonymous(),
            )
            .await
            .unwrap()
    );
    wait_for(&f.seen, 2).await;

    let rec = f.seen.lock().unwrap();
    assert_eq!(rec.len(), 2, "the send and the requeue each notify once");
    for (_, payload) in rec.iter() {
        let ctx = &payload["ctx"];
        assert_eq!(ctx["message_id"], id.to_string());
        assert_eq!(ctx["to_agent"], "codex");
        assert_eq!(ctx["from_workspace"], "fuad");
        assert_eq!(ctx["from_project"], "next");
        assert_eq!(ctx["project"], "grok-bot");
        let text = payload.to_string();
        assert!(!text.contains("deploy the hotfix"), "no body: {text}");
        assert!(!text.contains("deploy request"), "no subject: {text}");
    }
}

#[tokio::test]
async fn shared_mail_reports_no_recipient_harness() {
    let f = mail_fixture(vec![AdmissionOp::MessageSend]).await;

    f.wiki
        .send_agent_message(mail(&f, None), ActorContext::anonymous())
        .await
        .unwrap();
    wait_for(&f.seen, 1).await;

    let rec = f.seen.lock().unwrap();
    let ctx = &rec[0].1["ctx"];
    assert!(ctx["message_id"].is_string());
    assert!(
        ctx["to_agent"].is_null(),
        "shared mail has no harness: {ctx}"
    );
}

#[tokio::test]
async fn other_ops_carry_no_message_fields() {
    let (url, seen) = recorder().await;
    let chain = AdmissionChain::new(vec![hook(
        &url,
        vec![AdmissionOp::WritePage, AdmissionOp::HandoffBegin],
        true,
    )])
    .unwrap();
    let path = PagePath::new("notes/a.md").unwrap();
    let mut markdown = Markdown {
        frontmatter: json!({"title": "A"}),
        body: "body\n".into(),
    };
    let ctx = |op| AdmissionContext {
        op,
        workspace: "fuad".into(),
        project: "next".into(),
        ..AdmissionContext::default()
    };
    chain
        .run(&path, &mut markdown, &ctx(AdmissionOp::WritePage))
        .await
        .unwrap();
    chain
        .notify(None, &ctx(AdmissionOp::HandoffBegin))
        .await
        .unwrap();

    let rec = seen.lock().unwrap();
    assert_eq!(rec.len(), 2);
    for (_, payload) in rec.iter() {
        let ctx = payload["ctx"].as_object().unwrap();
        for key in ["message_id", "to_agent", "from_workspace", "from_project"] {
            assert!(!ctx.contains_key(key), "{key} leaked into {ctx:?}");
        }
    }
}
