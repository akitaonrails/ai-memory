//! A stateful stand-in for the ai-memory MCP and hook endpoints the
//! agentmemory routes call. It enforces the server's conditional-write
//! semantics (`create_only`, `expected_page_id`), keeps each path's version
//! chain, and can fail a chosen write before or after applying it.

use std::collections::{BTreeMap, BTreeSet};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value, json};

use crate::test_http::{read_http_request, respond_json};

/// One stored version of a page.
#[derive(Debug, Clone)]
pub(crate) struct StoredVersion {
    pub(crate) page_id: String,
    pub(crate) body: String,
    pub(crate) arguments: Value,
}

/// Where and when the mock fails a `memory_write_page` call (1-based count of
/// write calls).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Failure {
    /// Answer HTTP 500 without storing anything.
    Before(usize),
    /// Store the write, then answer HTTP 500, as a crash after the server
    /// committed but before the client recorded it would look.
    After(usize),
}

#[derive(Debug, Default)]
pub(crate) struct MockState {
    /// `(workspace, project, path)` → versions, oldest first.
    pub(crate) pages: BTreeMap<(String, String, String), Vec<StoredVersion>>,
    pub(crate) scopes: BTreeSet<(String, String)>,
    /// Every request path the mock saw, in order.
    pub(crate) requests: Vec<String>,
    pub(crate) write_calls: usize,
    pub(crate) failure: Option<Failure>,
    /// Advertise `create_only` in `tools/list` (a 3.0+ server).
    pub(crate) conditional: bool,
    pub(crate) hook_batches: Vec<Value>,
    next_id: usize,
}

impl MockState {
    pub(crate) fn chain(&self, workspace: &str, project: &str, path: &str) -> Vec<StoredVersion> {
        self.pages
            .get(&(workspace.into(), project.into(), path.into()))
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn stored_versions(&self) -> usize {
        self.pages.values().map(Vec::len).sum()
    }
}

pub(crate) struct MockServer {
    pub(crate) url: String,
    pub(crate) state: Arc<Mutex<MockState>>,
}

impl MockServer {
    /// Start a mock that knows `scopes` and advertises conditional writes.
    pub(crate) fn start(scopes: &[(&str, &str)]) -> Self {
        let state = Arc::new(Mutex::new(MockState {
            conditional: true,
            scopes: scopes
                .iter()
                .map(|(w, p)| ((*w).to_owned(), (*p).to_owned()))
                .collect(),
            ..MockState::default()
        }));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let shared = Arc::clone(&state);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let (headers, body) = read_http_request(&mut stream);
                let path = headers
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                let (status, reply) = handle(&shared, &path, &body);
                respond_json(&mut stream, status, &reply.to_string());
            }
        });
        Self { url, state }
    }

    pub(crate) fn state(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.state.lock().unwrap()
    }
}

fn handle(state: &Mutex<MockState>, path: &str, body: &[u8]) -> (&'static str, Value) {
    let mut state = state.lock().unwrap();
    state.requests.push(path.to_owned());
    let request: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    if path.starts_with("/hook/batch") {
        let accepted = request.as_array().map_or(0, Vec::len);
        state.hook_batches.push(request);
        return ("200 OK", json!({"accepted": accepted}));
    }
    match request["method"].as_str() {
        Some("initialize") => (
            "200 OK",
            rpc(json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "serverInfo": {"name": "mock", "version": "3.2.10"}
            })),
        ),
        Some("tools/list") => {
            let mut properties = json!({"path": {}, "body": {}});
            if state.conditional {
                properties["create_only"] = json!({"type": "boolean"});
                properties["expected_page_id"] = json!({"type": "string"});
            }
            (
                "200 OK",
                rpc(
                    json!({"tools": [{"name": "memory_write_page", "inputSchema": {"properties": properties}}]}),
                ),
            )
        }
        Some("tools/call") => {
            let name = request["params"]["name"].as_str().unwrap_or_default();
            let arguments = request["params"]["arguments"].clone();
            let scope = (
                arguments["workspace"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                arguments["project"].as_str().unwrap_or_default().to_owned(),
            );
            match name {
                "memory_recent" => {
                    if state.scopes.contains(&scope) {
                        ("200 OK", tool_text(&json!([])))
                    } else {
                        (
                            "200 OK",
                            rpc(
                                json!({"isError": true, "content": [{"type": "text", "text": "scope not found"}]}),
                            ),
                        )
                    }
                }
                "memory_read_page" => {
                    let key = (
                        scope.0,
                        scope.1,
                        arguments["path"].as_str().unwrap_or_default().to_owned(),
                    );
                    match state.pages.get(&key).and_then(|chain| chain.last()) {
                        Some(latest) => {
                            let (body, truncated, total_chars) =
                                read_page_body(&latest.body, &arguments);
                            (
                                "200 OK",
                                tool_text(&json!({
                                    "path": key.2,
                                    "page_id": latest.page_id,
                                    "body": body,
                                    "truncated": truncated,
                                    "total_chars": total_chars,
                                })),
                            )
                        }
                        None => (
                            "200 OK",
                            rpc(
                                json!({"isError": true, "content": [{"type": "text", "text": "page not found"}]}),
                            ),
                        ),
                    }
                }
                "memory_write_page" => write(&mut state, scope, arguments),
                other => (
                    "200 OK",
                    rpc(
                        json!({"isError": true, "content": [{"type": "text", "text": format!("unknown tool {other}")}]}),
                    ),
                ),
            }
        }
        _ => ("400 Bad Request", json!({"error": "bad request"})),
    }
}

fn write(
    state: &mut MockState,
    scope: (String, String),
    arguments: Value,
) -> (&'static str, Value) {
    state.write_calls += 1;
    let call = state.write_calls;
    if matches!(state.failure, Some(Failure::Before(n)) if n == call) {
        return ("500 Internal Server Error", json!({"error": "injected"}));
    }
    let key = (
        scope.0.clone(),
        scope.1.clone(),
        arguments["path"].as_str().unwrap_or_default().to_owned(),
    );
    let current = state
        .pages
        .get(&key)
        .and_then(|chain| chain.last())
        .map(|v| v.page_id.clone());
    let create_only = arguments["create_only"].as_bool() == Some(true);
    let expected = arguments["expected_page_id"].as_str().map(str::to_owned);
    let holds = if create_only {
        current.is_none()
    } else if let Some(expected) = &expected {
        current.as_deref() == Some(expected.as_str())
    } else {
        true
    };
    if !holds {
        return (
            "200 OK",
            json!({"jsonrpc": "2.0", "id": 1, "error": {
                "code": -32600,
                "message": "precondition failed",
                "data": {"reason": "precondition_failed", "path": key.2, "current_page_id": current},
            }}),
        );
    }
    state.next_id += 1;
    let page_id = format!("page-{}", state.next_id);
    state.scopes.insert(scope);
    state
        .pages
        .entry(key.clone())
        .or_default()
        .push(StoredVersion {
            page_id: page_id.clone(),
            body: arguments["body"].as_str().unwrap_or_default().to_owned(),
            arguments,
        });
    if matches!(state.failure, Some(Failure::After(n)) if n == call) {
        return (
            "500 Internal Server Error",
            json!({"error": "injected after commit"}),
        );
    }
    (
        "200 OK",
        tool_text(&json!({"page_id": page_id, "path": key.2})),
    )
}

fn rpc(result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "result": result})
}

fn tool_text(value: &Value) -> Value {
    rpc(json!({"content": [{"type": "text", "text": value.to_string()}]}))
}

/// The body `memory_read_page` returns, as the server cuts it since 3.3.0
/// (#1827): at `max_chars` characters (12,000 when the caller sends none,
/// clamped to 500..=64,000) with a visible marker, plus `truncated` and
/// `total_chars` (`READ_PAGE_*` in ai-memory-mcp `server.rs`).
fn read_page_body(body: &str, arguments: &Value) -> (String, bool, usize) {
    let max_chars = arguments["max_chars"]
        .as_u64()
        .map_or(12_000, |requested| requested.clamp(500, 64_000)) as usize;
    let total_chars = body.chars().count();
    if total_chars <= max_chars {
        return (body.to_owned(), false, total_chars);
    }
    let mut cut: String = body.chars().take(max_chars).collect();
    cut.push_str(&format!(
        "\n[body truncated; {} chars omitted]",
        total_chars - max_chars
    ));
    (cut, true, total_chars)
}
