//! Messaging queue changes refuse a project the server had to guess.
//!
//! A static MCP client (no lifecycle-hook session id on its requests) that
//! omits `workspace` + `project` resolves to the process-wide active-project
//! slot: whichever project published last, often another project the operator
//! has open in a different harness. A read can flag that guess; a pop there
//! claims the other project's mail, a no-id cancel clears its outbox, and a
//! send stamps it as the sender the recipient judges trust by. So
//! `memory_message_pop`, `memory_message_cancel` and `memory_message_send`
//! refuse an inferred scope and change nothing, while an explicit scope and a
//! caller bound to its own hook session proceed (`docs/security-boundaries.md`
//! row 8d).
//!
//! Every test points the shared slot at `project-b` the way a hook event from
//! another harness does (`ActiveProject::set_for` with that harness's session
//! id), then calls with no scope and no session coordinate.

use ai_memory_core::{ActiveProject, ActorKey, ProjectId, WorkspaceId};
use ai_memory_mcp::AiMemoryServer;
use ai_memory_store::Store;
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const WS: &str = "default";
const A: &str = "project-a";
const B: &str = "project-b";
const C: &str = "project-c";
/// The hook session of the harness working in project-b.
const SESSION_B: &str = "hook-session-b";

struct Harness {
    router: Router,
    store: Store,
    ws: WorkspaceId,
    a: ProjectId,
    b: ProjectId,
    c: ProjectId,
    _tmp: TempDir,
}

impl Harness {
    /// Three sibling projects; project-b's harness has just published, so the
    /// shared slot (and the keyed entry for `SESSION_B`) point at project-b.
    async fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let store = Store::open(tmp.path()).expect("store");
        let ws = store.writer.get_or_create_workspace(WS).await.expect("ws");
        let mut ids = Vec::new();
        for name in [A, B, C] {
            ids.push(
                store
                    .writer
                    .get_or_create_project(ws, name.to_string(), None)
                    .await
                    .expect("project"),
            );
        }
        let (a, b, c) = (ids[0], ids[1], ids[2]);
        let active = ActiveProject::new();
        active.set_for(
            &ActorKey {
                user: None,
                session_id: Some(SESSION_B.to_owned()),
            },
            ws,
            b,
            false,
        );
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");
        let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, a)
            .with_wiki(wiki)
            .with_active_project(active);
        let service = StreamableHttpService::new(
            move || Ok(server.clone()),
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default()
                .with_stateful_mode(false)
                .with_json_response(true),
        );
        Self {
            router: Router::new().nest_service("/mcp", service),
            store,
            ws,
            a,
            b,
            c,
            _tmp: tmp,
        }
    }

    /// One `tools/call`: `Ok(tool JSON)` or `Err(JSON-RPC error message)`.
    /// `session` is the hook session id a session-aware client forwards.
    async fn call(&self, session: Option<&str>, name: &str, args: Value) -> Result<Value, String> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": args },
        });
        let mut req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if let Some(session) = session {
            req = req.header("x-memory-actor-session-id", session);
        }
        let req = req.body(Body::from(body.to_string())).expect("mcp req");
        let resp = self.router.clone().oneshot(req).await.expect("oneshot");
        let bytes = axum::body::to_bytes(resp.into_body(), 4_000_000)
            .await
            .expect("body");
        let text = String::from_utf8(bytes.to_vec()).expect("utf8");
        let v: Value =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("non-JSON: {text}: {e}"));
        if let Some(err) = v.get("error") {
            return Err(err["message"].as_str().unwrap_or_default().to_owned());
        }
        let joined = v
            .pointer("/result/content")
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("missing result.content: {text}"))
            .iter()
            .filter_map(|i| i.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(serde_json::from_str(&joined)
            .unwrap_or_else(|e| panic!("tool text not JSON: {joined}: {e}")))
    }

    /// Send `from` → `to` with both coordinates explicit.
    async fn send(&self, from: &str, to: &str, body: &str) {
        self.call(
            None,
            "memory_message_send",
            json!({
                "from_workspace": WS, "from_project": from,
                "to_workspace": WS, "to_project": to, "body": body,
            }),
        )
        .await
        .expect("explicit send");
    }

    async fn pending_for(&self, project: ProjectId) -> u64 {
        self.store
            .reader
            .pending_message_count(self.ws, project)
            .await
            .expect("pending count")
    }

    async fn outbox_len(&self, project: ProjectId) -> usize {
        self.store
            .reader
            .list_messages(self.ws, project, ai_memory_core::MessageBox::Outbox, 50)
            .await
            .expect("outbox")
            .len()
    }
}

#[tokio::test]
async fn unscoped_pop_from_the_shared_slot_is_refused_and_claims_nothing() {
    let h = Harness::new().await;
    h.send(A, B, "for project-b only").await;

    let err = h
        .call(None, "memory_message_pop", json!({}))
        .await
        .expect_err("a pop whose inbox was guessed must be refused");
    assert!(
        err.contains("refusing to pop from default/project-b") && err.contains("shared_slot"),
        "the refusal names the inbox it would have used and how: {err}"
    );
    assert!(err.contains("workspace and project"), "{err}");
    assert_eq!(
        h.pending_for(h.b).await,
        1,
        "the refused pop must not claim project-b's message"
    );

    // Control: project-b's own harness, bound by its hook session, pops it.
    let popped = h
        .call(Some(SESSION_B), "memory_message_pop", json!({}))
        .await
        .expect("a session-bound pop proceeds");
    assert_eq!(popped["message"]["body"], "for project-b only");

    // Control: an explicit scope pops too.
    h.send(A, B, "second").await;
    let popped = h
        .call(
            None,
            "memory_message_pop",
            json!({ "workspace": WS, "project": B }),
        )
        .await
        .expect("an explicit pop proceeds");
    assert_eq!(popped["message"]["body"], "second");
    assert_eq!(h.pending_for(h.b).await, 0);
}

#[tokio::test]
async fn unscoped_cancel_from_the_shared_slot_is_refused_and_the_outbox_survives() {
    let h = Harness::new().await;
    h.send(B, C, "one").await;
    h.send(B, C, "two").await;

    for args in [
        json!({}),
        json!({ "message_id": "00000000-0000-0000-0000-000000000000" }),
    ] {
        let err = h
            .call(None, "memory_message_cancel", args)
            .await
            .expect_err("a cancel whose outbox was guessed must be refused");
        assert!(
            err.contains("default/project-b") && err.contains("shared_slot"),
            "{err}"
        );
    }
    assert_eq!(
        h.outbox_len(h.b).await,
        2,
        "the refused cancel must leave project-b's outbox intact"
    );
    assert_eq!(h.pending_for(h.c).await, 2);

    // Control: project-b's own session clears its outbox.
    let cancelled = h
        .call(Some(SESSION_B), "memory_message_cancel", json!({}))
        .await
        .expect("a session-bound cancel proceeds");
    assert_eq!(cancelled["cancelled"], 2);

    // Control: an explicit scope cancels too.
    h.send(B, C, "three").await;
    let cancelled = h
        .call(
            None,
            "memory_message_cancel",
            json!({ "workspace": WS, "project": B }),
        )
        .await
        .expect("an explicit cancel proceeds");
    assert_eq!(cancelled["cancelled"], 1);
}

#[tokio::test]
async fn unscoped_send_from_the_shared_slot_is_refused_and_explicit_sender_is_stamped() {
    let h = Harness::new().await;
    let to_c = json!({ "to_workspace": WS, "to_project": C, "body": "please do X" });

    let err = h
        .call(None, "memory_message_send", to_c.clone())
        .await
        .expect_err("a send whose sender was guessed must be refused");
    assert!(
        err.contains("refusing to send as default/project-b")
            && err.contains("shared_slot")
            && err.contains("from_workspace")
            && err.contains("from_project"),
        "{err}"
    );
    assert_eq!(h.pending_for(h.c).await, 0, "nothing was delivered");
    assert_eq!(h.outbox_len(h.b).await, 0, "nothing was filed as project-b");

    // Control: an explicit sender is the provenance the recipient sees.
    let mut explicit = to_c.clone();
    explicit["from_workspace"] = json!(WS);
    explicit["from_project"] = json!(A);
    h.call(None, "memory_message_send", explicit)
        .await
        .expect("an explicit sender proceeds");
    // Control: a session-bound sender is its own project.
    h.call(Some(SESSION_B), "memory_message_send", to_c)
        .await
        .expect("a session-bound sender proceeds");

    let inbox = h
        .call(
            None,
            "memory_message_list",
            json!({ "workspace": WS, "project": C }),
        )
        .await
        .expect("list C");
    let senders: Vec<String> = inbox["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|m| m["from_project_id"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(senders, vec![h.a.to_string(), h.b.to_string()]);
}

#[tokio::test]
async fn unscoped_list_names_the_mailbox_it_read_even_when_not_empty() {
    let h = Harness::new().await;
    h.send(A, B, "inbound").await;
    h.send(B, C, "outbound").await;

    for mailbox in ["inbox", "outbox"] {
        let listed = h
            .call(None, "memory_message_list", json!({ "box": mailbox }))
            .await
            .expect("an unscoped list still answers");
        assert_eq!(listed["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            listed["resolved_scope"],
            json!({ "workspace": WS, "project": B }),
            "a guessed {mailbox} listing must name whose mailbox it read: {listed}"
        );
        assert_eq!(listed["scope_source"], "shared_slot");
        assert!(
            listed["hint"]
                .as_str()
                .is_some_and(|h| h.contains("explicitly"))
        );

        // Controls: a session-bound or explicit listing is not a guess.
        for (session, args) in [
            (Some(SESSION_B), json!({ "box": mailbox })),
            (
                None,
                json!({ "box": mailbox, "workspace": WS, "project": B }),
            ),
        ] {
            let listed = h
                .call(session, "memory_message_list", args)
                .await
                .expect("list");
            assert_eq!(listed["messages"].as_array().map(Vec::len), Some(1));
            assert!(listed.get("resolved_scope").is_none(), "{listed}");
        }
    }
}
