//! Behavioral tests for the relay.
//!
//! Every fixture is kept (`TempDir::keep()`): when one of these fails, the queue
//! database and the request bodies it produced are the evidence, and the test
//! prints where they are. Nothing here deletes a file.
//!
//! Scripted HTTP responses cover partial acknowledgements and delivery errors.
//! The real-server test in tests/e2e/external_relay_smoke.py checks persistence.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use ai_memory_relay::ack::{self, BatchAck};
use ai_memory_relay::identity::{self, InputEvent, ValidEvent};
use ai_memory_relay::queue::Queue;
use ai_memory_relay::relay::{self, FlushOptions};

// Compile the same queue source with private test hooks; the shipped library
// has no hook API. These hooks place a real SQLite holder at the WAL transition.
use ai_memory_relay::fsguard;
#[allow(dead_code)]
#[path = "../src/queue.rs"]
mod queue_under_test;

// ---------------------------------------------------------------- fixtures

/// A sandbox path that owns its temp dir, so the tree is removed when the test
/// ends instead of accumulating in the system temp directory.
struct Sandbox {
    _guard: tempfile::TempDir,
    path: PathBuf,
}

impl std::ops::Deref for Sandbox {
    type Target = PathBuf;

    fn deref(&self) -> &PathBuf {
        &self.path
    }
}

fn fixture(name: &str) -> Sandbox {
    let guard = tempfile::TempDir::new().unwrap();
    let path = guard.path().join(name);
    std::fs::create_dir_all(&path).unwrap();
    Sandbox {
        _guard: guard,
        path,
    }
}

fn queue_dir(root: &Path) -> PathBuf {
    root.join("queue")
}

fn bind(root: &Path, server: &str) -> PathBuf {
    let dir = queue_dir(root);
    relay::init(&dir, server, "example.runtime", "operator-a", "team", "app").unwrap();
    dir
}

fn event(event_id: &str, agent: &str, name: &str, session: &str) -> serde_json::Value {
    serde_json::json!({
        "event_id": event_id,
        "agent": agent,
        "event": name,
        "body": {"session_id": session, "cwd": "/work/app", "prompt": "continue"},
    })
}

fn write_events(root: &Path, file: &str, events: serde_json::Value) -> PathBuf {
    let path = root.join(file);
    std::fs::write(&path, serde_json::to_vec_pretty(&events).unwrap()).unwrap();
    path
}

fn enqueue(dir: &Path, root: &Path, file: &str, events: serde_json::Value) -> anyhow::Result<()> {
    let path = write_events(root, file, events);
    relay::enqueue(dir, &path).map(|_| ())
}

fn valid(event_id: &str, agent: &str, name: &str, session: &str) -> ValidEvent {
    let input: InputEvent = serde_json::from_value(event(event_id, agent, name, session)).unwrap();
    identity::validate(0, input, "example.runtime", "operator-a").unwrap()
}

/// Read through the public status surface rather than reopening the database.
fn pending_count(dir: &Path) -> i64 {
    let report = relay::status(dir).unwrap();
    let document: serde_json::Value = serde_json::from_str(&report.summary[0]).unwrap();
    document["pending_items"].as_i64().unwrap()
}

// ---------------------------------------------------------------- HTTP stub

/// One scripted response.
enum Reply {
    /// Status plus a JSON body.
    Json(u16, String),
    /// `200 {"accepted": <however many items arrived>}`. Used where the test is
    /// about batching, not about a specific ack shape.
    AcceptAll,
    /// Accept the request, read it, then close without answering: the shape of a
    /// lost response after the server already committed the batch.
    Close,
    /// A 302 the relay must refuse to follow.
    Redirect(String),
}

struct Stub {
    addr: SocketAddr,
    bodies: Arc<Mutex<Vec<serde_json::Value>>>,
    heads: Arc<Mutex<Vec<String>>>,
}

impl Stub {
    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn bodies(&self) -> Vec<serde_json::Value> {
        self.bodies.lock().unwrap().clone()
    }

    fn heads(&self) -> Vec<String> {
        self.heads.lock().unwrap().clone()
    }

    /// Event ids, per request, in wire order.
    fn sent_event_ids(&self) -> Vec<Vec<String>> {
        self.bodies()
            .iter()
            .map(|batch| {
                batch
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| {
                        query_of(item["url"].as_str().unwrap())
                            .get("ingest_key")
                            .cloned()
                            .unwrap()
                    })
                    .collect()
            })
            .collect()
    }
}

fn stub(replies: Vec<Reply>) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let heads = Arc::new(Mutex::new(Vec::new()));
    let (b, h) = (Arc::clone(&bodies), Arc::clone(&heads));
    thread::spawn(move || {
        for reply in replies {
            let Ok((mut socket, _)) = listener.accept() else {
                return;
            };
            let (head, body) = read_request(&mut socket);
            h.lock().unwrap().push(head);
            let mut items = 0usize;
            if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&body) {
                items = parsed.as_array().map(Vec::len).unwrap_or(0);
                b.lock().unwrap().push(parsed);
            }
            match reply {
                Reply::Json(status, payload) => respond(&mut socket, status, &payload),
                Reply::AcceptAll => {
                    respond(&mut socket, 200, &format!(r#"{{"accepted":{items}}}"#))
                }
                Reply::Redirect(location) => {
                    let _ = write!(
                        socket,
                        "HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\n\
                         connection: close\r\n\r\n"
                    );
                    let _ = socket.flush();
                }
                Reply::Close => drop(socket),
            }
        }
    });
    Stub {
        addr,
        bodies,
        heads,
    }
}

fn read_request(socket: &mut TcpStream) -> (String, Vec<u8>) {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 8192];
    let header_end = loop {
        let Ok(read) = socket.read(&mut buffer) else {
            return (String::new(), Vec::new());
        };
        if read == 0 {
            return (String::from_utf8_lossy(&bytes).to_string(), Vec::new());
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&bytes[..header_end]).to_string();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while bytes.len() - header_end < length {
        let Ok(read) = socket.read(&mut buffer) else {
            break;
        };
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    let body = bytes[header_end..].to_vec();
    (head, body)
}

fn respond(socket: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let _ = write!(
        socket,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.flush();
}

fn query_of(url: &str) -> HashMap<String, String> {
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or_default();
    url::form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn flush(dir: &Path) -> ai_memory_relay::relay::Report {
    relay::flush(dir, &FlushOptions::default()).unwrap()
}

// ------------------------------------------------------------- ack contract

#[test]
fn ack_releases_the_contiguous_prefix_when_indices_are_absent() {
    let parsed: BatchAck = serde_json::from_str(r#"{"accepted":2}"#).unwrap();
    assert_eq!(ack::validate(4, &parsed).unwrap(), vec![0, 1]);
}

#[test]
fn ack_releases_exactly_the_reported_indices_when_they_have_a_gap() {
    // The shape a per-source rate limit produces: item 1 skipped, 2 committed.
    let parsed: BatchAck =
        serde_json::from_str(r#"{"accepted":1,"accepted_indices":[0,2]}"#).unwrap();
    assert_eq!(ack::validate(3, &parsed).unwrap(), vec![0, 2]);
}

#[test]
fn ack_accepts_an_explicitly_empty_index_list() {
    let parsed: BatchAck = serde_json::from_str(r#"{"accepted":0,"accepted_indices":[]}"#).unwrap();
    assert_eq!(ack::validate(3, &parsed).unwrap(), Vec::<usize>::new());
}

#[test]
fn an_inconsistent_ack_preserves_the_whole_batch() {
    // Each of these would otherwise release an item the server never committed.
    let cases = [
        // index outside the batch
        (3, r#"{"accepted":0,"accepted_indices":[0,9]}"#),
        // duplicate index
        (3, r#"{"accepted":1,"accepted_indices":[0,0]}"#),
        // descending indexes
        (3, r#"{"accepted":0,"accepted_indices":[2,1]}"#),
        // `accepted` disagreeing with the contiguous prefix
        (3, r#"{"accepted":2,"accepted_indices":[0,2]}"#),
        // prefix longer than the batch
        (2, r#"{"accepted":5}"#),
        // failure outside the batch
        (2, r#"{"accepted":1,"failed_index":7}"#),
        // the failed item also claimed as accepted
        (
            3,
            r#"{"accepted":0,"accepted_indices":[1],"failed_index":1}"#,
        ),
        // an acceptance after the item the server stopped on
        (
            3,
            r#"{"accepted":0,"accepted_indices":[2],"failed_index":1}"#,
        ),
    ];
    for (len, payload) in cases {
        let parsed: BatchAck = serde_json::from_str(payload).unwrap();
        assert!(
            ack::validate(len, &parsed).is_err(),
            "should have been refused: {payload}"
        );
    }
}

#[test]
fn an_ack_without_accepted_is_unreadable_rather_than_zero() {
    // Defaulting a missing `accepted` to 0 would be safe; defaulting it to
    // anything else would not. Parsing must fail so the caller keeps the batch.
    assert!(serde_json::from_str::<BatchAck>(r#"{"failed_index":0}"#).is_err());
}

// --------------------------------------------------------- delivery outcomes

#[test]
fn a_partial_noncontiguous_ack_releases_only_the_acknowledged_items() {
    let root = fixture("partial-ack");
    let server = stub(vec![
        Reply::Json(200, r#"{"accepted":1,"accepted_indices":[0,2]}"#.into()),
        Reply::Json(200, r#"{"accepted":1}"#.into()),
        Reply::Json(200, r#"{"accepted":0,"accepted_indices":[]}"#.into()),
    ]);
    let dir = bind(&root, &server.url());
    enqueue(
        &dir,
        &root,
        "in.json",
        serde_json::json!([
            event("e-a", "claude-code", "session-start", "s-a"),
            event("e-b", "claude-code", "session-start", "s-b"),
            event("e-c", "codex", "session-start", "s-c"),
        ]),
    )
    .unwrap();

    let report = flush(&dir);
    assert!(report.failure.is_none(), "{report:?}");
    // s-b was skipped by the server, so exactly one event is still queued.
    assert_eq!(pending_count(&dir), 1);
    assert!(report.pending);
    assert_eq!(report.exit_code(), 3);
}

#[test]
fn a_lost_response_keeps_the_batch_and_retries_the_same_ingest_key() {
    let root = fixture("lost-response");
    // Three closes: the first attempt plus both transport retries.
    let server = stub(vec![
        Reply::Close,
        Reply::Close,
        Reply::Close,
        Reply::Json(200, r#"{"accepted":1}"#.into()),
    ]);
    let dir = bind(&root, &server.url());
    enqueue(
        &dir,
        &root,
        "in.json",
        serde_json::json!([event("e-1", "claude-code", "session-start", "s-1")]),
    )
    .unwrap();

    let first = flush(&dir);
    assert!(first.failure.is_some(), "a lost response is a failure");
    assert_eq!(pending_count(&dir), 1, "nothing may be released");

    let second = flush(&dir);
    assert!(second.failure.is_none(), "{second:?}");
    assert_eq!(pending_count(&dir), 0);

    let sent = server.sent_event_ids();
    assert_eq!(sent.len(), 4, "3 attempts then the successful retry");
    assert!(
        sent.windows(2).all(|pair| pair[0] == pair[1]),
        "the retry must carry the identical ingest key: {sent:?}"
    );
}

#[test]
fn a_reopened_queue_recognizes_an_acknowledged_event() {
    let root = fixture("restart-after-ack");
    let server = stub(vec![Reply::Json(200, r#"{"accepted":1}"#.into())]);
    let dir = bind(&root, &server.url());
    let payload = serde_json::json!([event("e-1", "claude-code", "session-start", "s-1")]);
    enqueue(&dir, &root, "in.json", payload.clone()).unwrap();
    flush(&dir);
    assert_eq!(pending_count(&dir), 0);

    // Reopening the queue must find the persisted receipt.
    enqueue(&dir, &root, "in.json", payload).unwrap();
    assert_eq!(pending_count(&dir), 0, "a delivered id must not come back");
    assert_eq!(server.bodies().len(), 1, "and must not be re-sent");
}

#[test]
fn an_input_file_with_a_utf8_byte_order_mark_is_enqueued() {
    let root = fixture("bom-input");
    let dir = bind(&root, "http://127.0.0.1:9");
    let path = root.join("in.json");
    let mut bytes = b"\xEF\xBB\xBF".to_vec();
    bytes.extend(
        serde_json::to_vec(&serde_json::json!([event(
            "e-bom",
            "claude-code",
            "session-start",
            "s-1"
        )]))
        .unwrap(),
    );
    std::fs::write(&path, bytes).unwrap();
    relay::enqueue(&dir, &path).unwrap();
    assert_eq!(pending_count(&dir), 1);
}

#[test]
fn an_unauthenticated_rejection_keeps_everything_pending() {
    let root = fixture("auth-rejected");
    let server = stub(vec![Reply::Json(401, r#"{"error":"unauthorized"}"#.into())]);
    let dir = bind(&root, &server.url());
    enqueue(
        &dir,
        &root,
        "in.json",
        serde_json::json!([
            event("e-1", "claude-code", "session-start", "s-1"),
            event("e-2", "codex", "session-start", "s-2"),
        ]),
    )
    .unwrap();

    let report = flush(&dir);
    let failure = report.failure.clone().expect("401 is a failure");
    assert!(failure.contains("HTTP 401"), "{failure}");
    assert_eq!(pending_count(&dir), 2);
    assert_eq!(report.exit_code(), 2);
}

#[test]
fn a_body_bearing_status_that_is_not_200_or_429_is_never_parsed_as_an_ack() {
    // A 413 whose body happens to be a well-formed ack must still release
    // nothing: only 200 and 429 carry a meaningful acknowledgement.
    for status in [413u16, 500] {
        let root = fixture(&format!("status-{status}"));
        let server = stub(vec![Reply::Json(status, r#"{"accepted":2}"#.into())]);
        let dir = bind(&root, &server.url());
        enqueue(
            &dir,
            &root,
            "in.json",
            serde_json::json!([
                event("e-1", "claude-code", "session-start", "s-1"),
                event("e-2", "codex", "session-start", "s-2"),
            ]),
        )
        .unwrap();

        let report = flush(&dir);
        assert!(report.failure.is_some(), "HTTP {status} must fail");
        assert_eq!(
            pending_count(&dir),
            2,
            "HTTP {status} must not release anything"
        );
    }
}

#[test]
fn a_429_applies_its_acknowledged_items_and_then_stops() {
    let root = fixture("rate-limited");
    let server = stub(vec![Reply::Json(429, r#"{"accepted":1}"#.into())]);
    let dir = bind(&root, &server.url());
    enqueue(
        &dir,
        &root,
        "in.json",
        serde_json::json!([
            event("e-1", "claude-code", "session-start", "s-1"),
            event("e-2", "codex", "session-start", "s-2"),
        ]),
    )
    .unwrap();

    let report = flush(&dir);
    assert!(
        report.failure.is_none(),
        "a 429 is backpressure, not failure"
    );
    assert_eq!(
        pending_count(&dir),
        1,
        "the acked item is gone, the other stays"
    );
    assert_eq!(server.bodies().len(), 1, "the flush stopped after the 429");
    assert!(
        report.summary.iter().any(|line| line.contains("429")),
        "{report:?}"
    );
}

#[test]
fn an_oversized_http_response_is_not_retried_and_keeps_the_batch() {
    for status in [200u16, 401, 403, 413, 429, 500] {
        let root = fixture(&format!("oversized-response-{status}"));
        // Even a valid acknowledgement must not release the item once its
        // response exceeds the body limit. The next response belongs to a
        // later explicit flush, not an automatic transport retry.
        let mut body = r#"{"accepted":1,"extra":"ACK_BODY_CANARY"}"#.to_owned();
        body.push_str(&" ".repeat(ai_memory_relay::http::MAX_ACK_BYTES + 1 - body.len()));
        let server = stub(vec![Reply::Json(status, body), Reply::AcceptAll]);
        let dir = bind(&root, &server.url());
        enqueue(
            &dir,
            &root,
            "in.json",
            serde_json::json!([event("e-1", "codex", "session-start", "s-1")]),
        )
        .unwrap();

        let report = flush(&dir);
        assert_eq!(
            server.bodies().len(),
            1,
            "HTTP {status} must stop this flush even when its body is oversized"
        );
        assert_eq!(pending_count(&dir), 1);
        assert_eq!(report.exit_code(), 2);
        let failure = report.failure.as_deref().expect("oversized response fails");
        assert!(failure.contains(&format!("HTTP {status}")), "{report:?}");
        assert!(!format!("{report:?}").contains("ACK_BODY_CANARY"));
        assert_eq!(
            Queue::open(&dir)
                .unwrap()
                .stats(ai_memory_relay::now_ms())
                .unwrap()
                .receipts,
            0
        );

        let next = flush(&dir);
        assert_eq!(next.exit_code(), 0, "a later flush may retry: {next:?}");
        assert_eq!(pending_count(&dir), 0);
        let sent = server.sent_event_ids();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0], sent[1], "the later retry keeps its identity");
    }
}

#[test]
fn an_ack_at_the_body_limit_is_accepted() {
    let root = fixture("ack-at-limit");
    let mut body = r#"{"accepted":1}"#.to_owned();
    body.push_str(&" ".repeat(ai_memory_relay::http::MAX_ACK_BYTES - body.len()));
    let server = stub(vec![Reply::Json(200, body)]);
    let dir = bind(&root, &server.url());
    enqueue(
        &dir,
        &root,
        "in.json",
        serde_json::json!([event("e-1", "codex", "session-start", "s-1")]),
    )
    .unwrap();

    let report = flush(&dir);
    assert_eq!(report.exit_code(), 0, "{report:?}");
    assert_eq!(pending_count(&dir), 0);
    assert_eq!(server.bodies().len(), 1);
}

#[test]
fn a_redirect_is_refused_and_the_batch_never_reaches_the_second_host() {
    let root = fixture("redirect");
    let elsewhere = TcpListener::bind("127.0.0.1:0").unwrap();
    elsewhere.set_nonblocking(true).unwrap();
    let elsewhere_addr = elsewhere.local_addr().unwrap();
    let server = stub(vec![
        Reply::Redirect(format!("http://{elsewhere_addr}/hook/batch")),
        Reply::Redirect(format!("http://{elsewhere_addr}/hook/batch")),
        Reply::Redirect(format!("http://{elsewhere_addr}/hook/batch")),
    ]);
    let dir = bind(&root, &server.url());
    enqueue(
        &dir,
        &root,
        "in.json",
        serde_json::json!([event("e-1", "claude-code", "session-start", "s-1")]),
    )
    .unwrap();

    let report = flush(&dir);
    assert!(report.failure.is_some(), "a 302 is not an acknowledgement");
    assert_eq!(pending_count(&dir), 1);
    assert!(
        elsewhere.accept().is_err(),
        "the redirect target must never be contacted"
    );
}

// ------------------------------------------------------------ session order

#[test]
fn a_batch_carries_one_head_per_session_in_enqueue_order() {
    let root = fixture("session-heads");
    let server = stub(vec![Reply::AcceptAll, Reply::AcceptAll, Reply::AcceptAll]);
    let dir = bind(&root, &server.url());
    enqueue(
        &dir,
        &root,
        "in.json",
        serde_json::json!([
            event("a-1", "claude-code", "session-start", "s-a"),
            event("a-2", "claude-code", "user-prompt-submit", "s-a"),
            event("a-3", "claude-code", "session-end", "s-a"),
            event("b-1", "codex", "session-start", "s-b"),
            event("b-2", "codex", "session-end", "s-b"),
        ]),
    )
    .unwrap();

    let report = flush(&dir);
    assert!(report.failure.is_none(), "{report:?}");
    assert_eq!(pending_count(&dir), 0);

    let batches = server.bodies();
    assert_eq!(batches.len(), 3, "5 events, 2 sessions: 3 rounds");
    let names: Vec<Vec<String>> = batches
        .iter()
        .map(|batch| {
            batch
                .as_array()
                .unwrap()
                .iter()
                .map(|item| query_of(item["url"].as_str().unwrap())["event"].clone())
                .collect()
        })
        .collect();
    assert_eq!(
        names,
        vec![
            vec!["session-start".to_string(), "session-start".to_string()],
            vec!["user-prompt-submit".to_string(), "session-end".to_string()],
            vec!["session-end".to_string()],
        ],
        "each batch takes the oldest event of each session, never two of one"
    );
    // The terminal event of s-a went last, after both of its predecessors.
    let sessions: Vec<Vec<String>> = batches
        .iter()
        .map(|batch| {
            batch
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["body"]["session_id"].as_str().unwrap().to_string())
                .collect()
        })
        .collect();
    assert_eq!(sessions[2], vec!["s-a".to_string()]);
}

#[test]
fn a_failed_head_defers_its_own_session_without_stalling_an_untried_one() {
    let root = fixture("poison-head");
    let server = stub(vec![
        // The server stopped on item 0 and never looked at item 1.
        Reply::Json(
            200,
            r#"{"accepted":0,"accepted_indices":[],"failed_index":0}"#.into(),
        ),
        Reply::Json(200, r#"{"accepted":1}"#.into()),
    ]);
    let dir = bind(&root, &server.url());
    enqueue(
        &dir,
        &root,
        "in.json",
        serde_json::json!([
            event("a-1", "claude-code", "session-start", "s-poison"),
            event("a-2", "claude-code", "session-end", "s-poison"),
            event("b-1", "codex", "session-start", "s-healthy"),
        ]),
    )
    .unwrap();

    let report = flush(&dir);
    assert!(report.failure.is_none(), "{report:?}");

    let sent = server.bodies();
    assert_eq!(sent.len(), 2, "the flush kept going after the failed head");
    let second: Vec<String> = sent[1]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["body"]["session_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        second,
        vec!["s-healthy".to_string()],
        "the untried session must be retried in the same flush, alone"
    );
    assert_eq!(
        pending_count(&dir),
        2,
        "both poisoned events stay: the head failed and its successor never passes it"
    );
}

// ------------------------------------------------------------ queue identity

#[test]
fn an_identical_reinit_is_recognized_and_a_changed_binding_is_refused() {
    let root = fixture("rebind");
    let dir = bind(&root, "http://127.0.0.1:9/");
    relay::init(
        &dir,
        "http://127.0.0.1:9/",
        "example.runtime",
        "operator-a",
        "team",
        "app",
    )
    .expect("an identical re-init is a no-op");

    let error = relay::init(
        &dir,
        "http://127.0.0.1:9/",
        "example.runtime",
        "operator-b",
        "team",
        "app",
    )
    .unwrap_err();
    assert!(error.to_string().contains("binding mismatch"), "{error:#}");
}

#[test]
fn the_same_id_with_different_content_is_refused_while_an_identical_one_is_recognized() {
    let root = fixture("collision");
    let dir = bind(&root, "http://127.0.0.1:9/");
    let original = serde_json::json!([event("e-1", "claude-code", "session-start", "s-1")]);
    enqueue(&dir, &root, "a.json", original.clone()).unwrap();

    enqueue(&dir, &root, "b.json", original).unwrap();
    assert_eq!(
        pending_count(&dir),
        1,
        "byte-identical: recognized, not doubled"
    );

    let mut altered = event("e-1", "claude-code", "session-start", "s-1");
    altered["body"]["prompt"] = serde_json::Value::String("something else".into());
    let error = enqueue(&dir, &root, "c.json", serde_json::json!([altered])).unwrap_err();
    assert!(
        error.to_string().contains("identity collision"),
        "{error:#}"
    );
    assert_eq!(pending_count(&dir), 1, "a refused file changes nothing");
}

#[test]
fn a_session_pinned_to_one_agent_refuses_a_second_agent() {
    let root = fixture("session-agent");
    let dir = bind(&root, "http://127.0.0.1:9/");
    enqueue(
        &dir,
        &root,
        "a.json",
        serde_json::json!([event("e-1", "claude-code", "session-start", "s-1")]),
    )
    .unwrap();

    let error = enqueue(
        &dir,
        &root,
        "b.json",
        serde_json::json!([event("e-2", "codex", "user-prompt-submit", "s-1")]),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("session identity conflict"),
        "{error:#}"
    );
    assert_eq!(pending_count(&dir), 1);
}

#[test]
fn an_invalid_envelope_is_refused_with_zero_side_effects() {
    let root = fixture("invalid-envelope");
    let dir = bind(&root, "http://127.0.0.1:9/");
    let cases = [
        // unknown envelope field
        serde_json::json!([{
            "event_id": "e", "agent": "codex", "event": "session-start",
            "body": {"session_id": "s", "cwd": "/w"}, "extra": true
        }]),
        // no session id
        serde_json::json!([{
            "event_id": "e", "agent": "codex", "event": "session-start",
            "body": {"cwd": "/w"}
        }]),
        // no cwd
        serde_json::json!([{
            "event_id": "e", "agent": "codex", "event": "session-start",
            "body": {"session_id": "s"}
        }]),
        // body is not an object
        serde_json::json!([{
            "event_id": "e", "agent": "codex", "event": "session-start", "body": "text"
        }]),
    ];
    for (index, case) in cases.iter().enumerate() {
        let error = enqueue(&dir, &root, &format!("bad-{index}.json"), case.clone()).unwrap_err();
        assert!(!error.to_string().is_empty());
        assert_eq!(pending_count(&dir), 0, "case {index} must not persist");
    }
}

#[test]
fn a_schema_error_never_echoes_the_input_back() {
    let root = fixture("no-value-echo");
    let dir = bind(&root, "http://127.0.0.1:9/");
    // `deny_unknown_fields` makes serde's Display render the offending key
    // verbatim (`unknown field \`…\``). A producer that puts content in a key
    // name would then have it echoed to a terminal or a log, so the relay
    // reports the error category and position instead.
    let sentinel = "sk-live-do-not-print-me";
    let payload = serde_json::json!([{
        "event_id": "e-1", "agent": "codex", "event": "session-start",
        "body": {"session_id": "s-1", "cwd": "/w"},
        sentinel: "leak",
    }]);
    let error = enqueue(&dir, &root, "unknown-field.json", payload).unwrap_err();
    let rendered = format!("{error:#}");
    assert!(
        !rendered.contains(sentinel),
        "the error echoed the input: {rendered}"
    );
    assert!(rendered.contains("schema error at line"), "{rendered}");
    assert_eq!(pending_count(&dir), 0);
}

// ------------------------------------------------------- persistence limits

#[test]
fn the_body_and_its_capture_protocol_block_reach_the_wire_untouched() {
    let root = fixture("verbatim-body");
    let server = stub(vec![Reply::Json(200, r#"{"accepted":1}"#.into())]);
    let dir = bind(&root, &server.url());
    let body = serde_json::json!({
        "session_id": "native-session-17",
        "cwd": "/work/app",
        "prompt": "Continue the parser fix.",
        "_ai_memory_capture": {"v": 1, "state": "metadata-only"},
        "tool_input": {"file_path": "/work/app/src/main.rs"},
    });
    enqueue(
        &dir,
        &root,
        "in.json",
        serde_json::json!([{
            "event_id": "run-17-prompt-4", "agent": "claude-code",
            "event": "user-prompt-submit", "body": body.clone(),
        }]),
    )
    .unwrap();
    flush(&dir);

    let sent = &server.bodies()[0][0];
    assert_eq!(sent["body"], body, "no local redaction, no dropped keys");
    let query = query_of(sent["url"].as_str().unwrap());
    assert_eq!(query["extension"], "example.runtime");
    assert_eq!(query["source_event"], "user-prompt-submit");
    assert_eq!(query["event"], "user-prompt-submit");
    assert_eq!(query["agent"], "claude-code");
    assert_eq!(query["workspace"], "team");
    assert_eq!(query["project"], "app");
    assert_eq!(query["ingest_key"].len(), 64);
    assert!(
        query["ingest_key"]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "the server only accepts its own key alphabet"
    );
}

#[test]
fn the_ingest_key_is_the_documented_tuple_and_ignores_payload() {
    let one = valid("e-1", "claude-code", "session-start", "s-1");
    let mut other_body = event("e-1", "claude-code", "session-start", "s-1");
    other_body["body"]["prompt"] = serde_json::Value::String("different".into());
    let input: InputEvent = serde_json::from_value(other_body).unwrap();
    let two = identity::validate(0, input, "example.runtime", "operator-a").unwrap();
    assert_eq!(
        one.ingest_key, two.ingest_key,
        "the key is identity, not content"
    );
    assert_ne!(one.body_sha256, two.body_sha256, "content is what differs");

    // A different producer namespace must not collide with the same event id.
    let input: InputEvent =
        serde_json::from_value(event("e-1", "claude-code", "session-start", "s-1")).unwrap();
    let elsewhere = identity::validate(0, input, "other.runtime", "operator-a").unwrap();
    assert_ne!(one.ingest_key, elsewhere.ingest_key);
}

#[test]
fn a_path_shaped_event_id_is_accepted_and_stays_idempotent() {
    // `event_id` only feeds the key hash; it is never a query parameter, so an
    // orchestrator that numbers events `run/123/event/2` must not be rejected.
    let root = fixture("path-event-id");
    let dir = bind(&root, "http://127.0.0.1:9/");
    let payload = serde_json::json!([{
        "event_id": "run/123/event/2", "agent": "claude-code",
        "event": "user-prompt-submit",
        "body": {"session_id": "native-17", "cwd": "/work/app"},
    }]);
    enqueue(&dir, &root, "a.json", payload.clone()).unwrap();
    assert_eq!(pending_count(&dir), 1);

    enqueue(&dir, &root, "b.json", payload).unwrap();
    assert_eq!(
        pending_count(&dir),
        1,
        "the same path-shaped id must be recognized, not duplicated"
    );
}

#[test]
fn an_event_past_the_retry_window_is_retained_and_never_resent() {
    let root = fixture("expired");
    let server = stub(vec![Reply::Json(200, r#"{"accepted":1}"#.into())]);
    let dir = bind(&root, &server.url());
    let mut queue = Queue::open(&dir).unwrap();
    let old = valid("e-old", "claude-code", "session-start", "s-old");
    let fresh = valid("e-new", "codex", "session-start", "s-new");
    queue
        .enqueue(&[old.clone(), fresh], ai_memory_relay::now_ms())
        .unwrap();
    // Its first durable attempt was 31 days ago: past the server's key expiry.
    let long_ago = ai_memory_relay::now_ms() - 31 * 24 * 60 * 60 * 1000;
    queue
        .stamp_attempt(std::slice::from_ref(&old.ingest_key), long_ago)
        .unwrap();
    drop(queue);

    let report = flush(&dir);
    assert!(report.failure.is_none(), "{report:?}");
    let sessions: Vec<String> = server.bodies()[0]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["body"]["session_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        sessions,
        vec!["s-new".to_string()],
        "only the in-window session goes on the wire"
    );
    assert_eq!(
        pending_count(&dir),
        1,
        "the expired event is kept, not dropped"
    );
    assert!(
        report.summary.iter().any(|line| line.contains("30-day")),
        "the operator has to be told: {report:?}"
    );
    let document: serde_json::Value =
        serde_json::from_str(&relay::status(&dir).unwrap().summary[0]).unwrap();
    assert_eq!(document["expired_items"], 1);
}

#[test]
fn a_full_queue_refuses_new_work_without_dropping_anything() {
    let root = fixture("queue-full");
    let dir = bind(&root, "http://127.0.0.1:9/");
    let mut queue = Queue::open(&dir).unwrap();
    // One 250 KiB body per event: 268 of them clear the 64 MiB ceiling.
    let filler = "x".repeat(250_000);
    let big = |id: &str, session: &str| -> ValidEvent {
        let input: InputEvent = serde_json::from_value(serde_json::json!({
            "event_id": id, "agent": "codex", "event": "post-tool-use",
            "body": {"session_id": session, "cwd": "/w", "blob": filler},
        }))
        .unwrap();
        identity::validate(0, input, "example.runtime", "operator-a").unwrap()
    };
    let first: Vec<ValidEvent> = (0..200)
        .map(|i| big(&format!("e-{i}"), &format!("s-{i}")))
        .collect();
    queue.enqueue(&first, ai_memory_relay::now_ms()).unwrap();

    let second: Vec<ValidEvent> = (200..300)
        .map(|i| big(&format!("e-{i}"), &format!("s-{i}")))
        .collect();
    let error = queue
        .enqueue(&second, ai_memory_relay::now_ms())
        .unwrap_err();
    assert!(error.to_string().contains("queue is full"), "{error:#}");
    drop(queue);
    assert_eq!(
        pending_count(&dir),
        200,
        "the refused file is all-or-nothing and the queue is intact"
    );
}

#[test]
fn replay_protection_counts_pending_and_receipts_together() {
    let root = fixture("protection-cap");
    let dir = bind(&root, "http://127.0.0.1:9/");
    let mut queue = Queue::open(&dir).unwrap();
    let fresh = valid("e-1", "claude-code", "session-start", "s-1");
    queue
        .enqueue(std::slice::from_ref(&fresh), ai_memory_relay::now_ms())
        .unwrap();
    drop(queue);
    // Fill the receipt table to one slot below the cap in the fixture itself, so
    // the single pending item already consumes the last slot. Generated in SQL:
    // 200k deliveries over HTTP would prove the same thing far more slowly.
    {
        let connection = rusqlite::Connection::open(dir.join("relay.sqlite")).unwrap();
        connection
            .execute(
                "INSERT INTO receipt(ingest_key, body_sha256, first_attempt_ms, delivered_at_ms)
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?1)
                 SELECT 'fill-' || i, 'fill', ?2, ?2 FROM n",
                rusqlite::params![
                    ai_memory_relay::queue::MAX_RECEIPTS - 1,
                    ai_memory_relay::now_ms()
                ],
            )
            .unwrap();
    }
    let mut queue = Queue::open(&dir).unwrap();

    let another = valid("e-2", "codex", "session-start", "s-2");
    let error = queue
        .enqueue(&[another], ai_memory_relay::now_ms())
        .unwrap_err();
    assert!(
        error.to_string().contains("replay protection is full"),
        "{error:#}"
    );
    // A duplicate must still be recognized while full: recognizing costs nothing.
    queue
        .enqueue(std::slice::from_ref(&fresh), ai_memory_relay::now_ms())
        .expect("an already-known id is recognized even at the cap");
}

// -------------------------------------------------------------- concurrency

#[test]
fn a_second_flush_is_refused_while_one_holds_the_lock() {
    let root = fixture("flush-lock");
    let dir = bind(&root, "http://127.0.0.1:9/");
    // Let the relay mint the lock file: created by hand it would inherit the
    // ambient umask, and the guard would (correctly) refuse a 0644 one.
    relay::flush(&dir, &FlushOptions::default()).expect("an empty queue flushes cleanly");
    let lock_path = dir.join("flush.lock");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let held = std::fs::OpenOptions::new()
        .write(true)
        .open(&lock_path)
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&held).unwrap();

    let error = relay::flush(&dir, &FlushOptions::default()).unwrap_err();
    assert!(error.to_string().contains("another flush"), "{error:#}");
    fs2::FileExt::unlock(&held).unwrap();
    assert!(lock_path.exists(), "the lock file is never removed");
}

#[test]
fn two_writers_enqueue_into_one_queue_without_losing_either() {
    let root = fixture("two-writers");
    let dir = bind(&root, "http://127.0.0.1:9/");
    let files: Vec<PathBuf> = (0..2)
        .map(|writer| {
            let events: Vec<serde_json::Value> = (0..25)
                .map(|i| {
                    event(
                        &format!("w{writer}-e{i}"),
                        "codex",
                        "post-tool-use",
                        &format!("w{writer}-s{i}"),
                    )
                })
                .collect();
            write_events(
                &root,
                &format!("writer-{writer}.json"),
                serde_json::Value::Array(events),
            )
        })
        .collect();

    let handles: Vec<_> = files
        .into_iter()
        .map(|file| {
            let dir = dir.clone();
            thread::spawn(move || relay::enqueue(&dir, &file).map(|_| ()))
        })
        .collect();
    for handle in handles {
        handle
            .join()
            .unwrap()
            .expect("concurrent writers both land");
    }
    assert_eq!(pending_count(&dir), 50);
}

// ---------------------------------------------------------- filesystem guards

#[test]
fn the_queue_database_and_its_sidecars_are_owner_only() {
    let root = fixture("private-files");
    let dir = bind(&root, "http://127.0.0.1:9/");
    let mut queue = Queue::open(&dir).unwrap();
    queue
        .enqueue(
            &[valid("e-1", "codex", "session-start", "s-1")],
            ai_memory_relay::now_ms(),
        )
        .unwrap();

    // Checked while the connection is open, which is when -wal and -shm exist.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["relay.sqlite", "relay.sqlite-wal", "relay.sqlite-shm"] {
            let path = dir.join(name);
            assert!(path.exists(), "{name} should exist during a write");
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "{name} is mode {:o}; SQLite copies the database's mode onto its sidecars, \
                 so the database must be created private, not chmod'ed afterwards",
                mode & 0o777
            );
        }
    }
    drop(queue);
}

#[test]
#[cfg(unix)]
fn a_symlinked_queue_directory_is_refused_without_following_it() {
    let root = fixture("symlink-dir");
    let real = root.join("real");
    std::fs::create_dir(&real).unwrap();
    let link = root.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let error = relay::status(&link).unwrap_err();
    assert!(error.to_string().contains("symlink"), "{error:#}");
    assert_eq!(
        std::fs::read_dir(&real).unwrap().count(),
        0,
        "the target must be untouched"
    );
    assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
}

#[test]
#[cfg(unix)]
fn a_group_readable_queue_directory_is_refused_and_not_repaired() {
    use std::os::unix::fs::PermissionsExt;
    let root = fixture("loose-dir");
    let dir = root.join("shared");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    let error = relay::status(&dir).unwrap_err();
    assert!(error.to_string().contains("owner-only"), "{error:#}");
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o755,
        "refusing must not silently chmod a directory the operator owns"
    );
}

#[test]
fn a_directory_holding_unrelated_files_is_refused() {
    let root = fixture("shared-dir");
    let dir = root.join("mixed");
    std::fs::create_dir(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::write(dir.join("notes.txt"), b"someone else's file").unwrap();

    let error = relay::status(&dir).unwrap_err();
    assert!(error.to_string().contains("unrelated files"), "{error:#}");
    assert_eq!(
        std::fs::read(dir.join("notes.txt")).unwrap(),
        b"someone else's file",
        "the file must be untouched"
    );
}

#[test]
fn a_foreign_database_is_refused_byte_for_byte() {
    let root = fixture("foreign-db");
    let dir = root.join("queue");
    std::fs::create_dir(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let db = dir.join("relay.sqlite");
    // Generic table names on purpose: `meta(key, value)` is exactly the shape a
    // name-based "interrupted init" guess would have adopted and overwritten.
    {
        let foreign = rusqlite::Connection::open(&db).unwrap();
        foreign
            .execute_batch(
                "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta(key, value) VALUES('app', 'something-else');
                 CREATE TABLE pending(id INTEGER PRIMARY KEY);",
            )
            .unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let before = std::fs::read(&db).unwrap();

    let error = relay::status(&dir).unwrap_err();
    assert!(
        error.to_string().contains("not a usable relay queue"),
        "{error:#}"
    );
    assert_eq!(
        std::fs::read(&db).unwrap(),
        before,
        "a stranger's database must come back byte-identical"
    );
}

#[test]
fn an_empty_sqlite_file_is_treated_as_fresh() {
    // What a rolled-back initialization leaves behind: a valid SQLite file with
    // no user objects. Opening it must succeed, not refuse forever.
    let root = fixture("rolled-back-db");
    let dir = root.join("queue");
    std::fs::create_dir(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let db = dir.join("relay.sqlite");
    {
        // A real interrupted initialization: create the schema, then roll it
        // back the way a crash mid-transaction would.
        let mut aborted = rusqlite::Connection::open(&db).unwrap();
        let transaction = aborted.transaction().unwrap();
        transaction
            .execute_batch(
                "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE pending(seq INTEGER PRIMARY KEY);",
            )
            .unwrap();
        transaction.rollback().unwrap();
        let objects: i64 = aborted
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(objects, 0, "the rollback must leave no user objects");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    relay::init(
        &dir,
        "http://127.0.0.1:9/",
        "example.runtime",
        "operator-a",
        "team",
        "app",
    )
    .expect("an object-free database is a fresh queue");
    assert_eq!(pending_count(&dir), 0);
}

// ------------------------------------------------------------------ privacy

#[test]
fn the_bearer_token_never_reaches_output_or_disk() {
    let root = fixture("token-privacy");
    let server = stub(vec![Reply::Json(200, r#"{"accepted":1}"#.into())]);
    let dir = bind(&root, &server.url());
    enqueue(
        &dir,
        &root,
        "in.json",
        serde_json::json!([event("e-1", "claude-code", "session-start", "s-1")]),
    )
    .unwrap();

    let token = "FAKEfake0123456789relaytoken";
    // Through the binary, so the assertion covers real stdout and stderr.
    let flushed = std::process::Command::new(env!("CARGO_BIN_EXE_ai-memory-relay"))
        .args(["flush", "--queue-dir"])
        .arg(&dir)
        .env_remove("AI_MEMORY_AUTH_TOKEN")
        .env("AI_MEMORY_AUTH_TOKEN", token)
        .output()
        .unwrap();
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&flushed.stdout),
        String::from_utf8_lossy(&flushed.stderr)
    );
    assert!(
        !rendered.contains(token),
        "the token reached output: {rendered}"
    );

    let status = std::process::Command::new(env!("CARGO_BIN_EXE_ai-memory-relay"))
        .args(["status", "--queue-dir"])
        .arg(&dir)
        .env_remove("AI_MEMORY_AUTH_TOKEN")
        .output()
        .unwrap();
    let rendered = String::from_utf8_lossy(&status.stdout).to_string();
    assert!(!rendered.contains(token));
    let document: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(document["pending_items"], 0, "the flush delivered it");

    // And the queue on disk must never have stored it.
    let stored = std::fs::read(dir.join("relay.sqlite")).unwrap();
    assert!(
        !String::from_utf8_lossy(&stored).contains(token),
        "the token was persisted"
    );
    // It did authenticate, though, and with exactly the value it was given.
    let authorization = server.heads()[0]
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("authorization:"))
        .expect("the request should still carry a bearer header")
        .to_string();
    assert_eq!(
        authorization.split_once(':').unwrap().1.trim(),
        format!("Bearer {token}")
    );
}

#[test]
fn status_reports_counts_and_limits_without_payload() {
    let root = fixture("status-shape");
    let dir = bind(&root, "http://127.0.0.1:9/");
    let secret = "prompt-text-that-must-not-appear-anywhere";
    enqueue(
        &dir,
        &root,
        "in.json",
        serde_json::json!([{
            "event_id": "e-1", "agent": "codex", "event": "user-prompt-submit",
            "body": {"session_id": "s-1", "cwd": "/private/work", "prompt": secret},
        }]),
    )
    .unwrap();

    let rendered = relay::status(&dir).unwrap().summary[0].clone();
    assert!(!rendered.contains(secret), "status leaked a payload");
    assert!(!rendered.contains("/private/work"), "status leaked a cwd");
    let document: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(document["pending_items"], 1);
    assert_eq!(document["pending_sessions"], 1);
    assert_eq!(
        document["limits"]["receipts"],
        ai_memory_relay::queue::MAX_RECEIPTS
    );
    assert_eq!(
        document["retry_window_ms"],
        ai_memory_relay::identity::RETRY_WINDOW_MS
    );
}

#[test]
fn a_batch_stays_under_the_wire_budget_when_bodies_are_large() {
    let root = fixture("byte-budget");
    let server = stub(vec![Reply::AcceptAll, Reply::AcceptAll, Reply::AcceptAll]);
    let dir = bind(&root, &server.url());
    let mut queue = Queue::open(&dir).unwrap();
    let filler = "y".repeat(250_000);
    let events: Vec<ValidEvent> = (0..40)
        .map(|i| {
            let input: InputEvent = serde_json::from_value(serde_json::json!({
                "event_id": format!("e-{i}"), "agent": "codex", "event": "post-tool-use",
                "body": {"session_id": format!("s-{i}"), "cwd": "/w", "blob": filler},
            }))
            .unwrap();
            identity::validate(0, input, "example.runtime", "operator-a").unwrap()
        })
        .collect();
    queue.enqueue(&events, ai_memory_relay::now_ms()).unwrap();
    drop(queue);

    // 40 sessions, one head each: the item count would allow a single batch of
    // 40, but 10 MB of bodies must not go out as one request.
    flush(&dir);
    let sizes: Vec<usize> = server
        .bodies()
        .iter()
        .map(|batch| serde_json::to_vec(batch).unwrap().len())
        .collect();
    assert!(
        sizes.len() >= 2,
        "one batch would exceed the budget: {sizes:?}"
    );
    for size in &sizes {
        assert!(
            *size <= 8 * 1024 * 1024,
            "a batch of {size} bytes is over the 8 MiB budget the server's 10 MiB \
             body limit leaves room for"
        );
    }
}

const V1_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta(
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS binding(
    id            INTEGER PRIMARY KEY CHECK(id = 1),
    server_url    TEXT NOT NULL,
    producer      TEXT NOT NULL,
    actor         TEXT NOT NULL,
    workspace     TEXT NOT NULL,
    project       TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS pending(
    seq              INTEGER PRIMARY KEY AUTOINCREMENT,
    ingest_key       TEXT NOT NULL UNIQUE,
    event_id         TEXT NOT NULL,
    agent            TEXT NOT NULL,
    event            TEXT NOT NULL,
    session_id       TEXT NOT NULL,
    cwd              TEXT NOT NULL,
    body_json        TEXT NOT NULL,
    body_sha256      TEXT NOT NULL,
    body_bytes       INTEGER NOT NULL,
    first_seen_ms    INTEGER NOT NULL,
    first_attempt_ms INTEGER,
    attempts         INTEGER NOT NULL DEFAULT 0,
    last_error       TEXT
);
CREATE INDEX IF NOT EXISTS pending_session ON pending(agent, session_id, seq);
CREATE TABLE IF NOT EXISTS receipt(
    ingest_key       TEXT PRIMARY KEY,
    body_sha256      TEXT NOT NULL,
    first_attempt_ms INTEGER NOT NULL,
    delivered_at_ms  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS receipt_first_attempt ON receipt(first_attempt_ms);
CREATE TABLE IF NOT EXISTS session_agent(
    session_id   TEXT PRIMARY KEY,
    agent        TEXT NOT NULL,
    last_seen_ms INTEGER NOT NULL
);
"#;

fn v1_fixture(name: &str) -> Sandbox {
    let root = fixture(name);
    let dir = queue_dir(&root);
    std::fs::create_dir(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = dir.join("relay.sqlite");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(V1_SCHEMA).unwrap();
    conn.execute_batch(
        "INSERT INTO meta VALUES('identity','ai-memory-relay-queue'),('schema_version','1'),('extra','preserve');
         INSERT INTO binding VALUES(1,'http://127.0.0.1:9/','example.runtime','operator-a','team','app',123);
         INSERT INTO pending VALUES(7,'key-a','event-a','codex','session-start','session-a','/w','{}','hash-a',2,100,101,4,'retry');
         INSERT INTO pending VALUES(11,'key-b','event-b','codex','session-end','session-a','/w','{}','hash-b',2,102,NULL,0,NULL);
         INSERT INTO receipt VALUES('receipt-a','receipt-hash',103,104);
         INSERT INTO session_agent VALUES('session-a','codex',105);"
    ).unwrap();
    drop(conn);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    Sandbox {
        _guard: root._guard,
        path: dir,
    }
}

fn snapshot(conn: &rusqlite::Connection) -> Vec<String> {
    [
        "SELECT * FROM meta WHERE key != 'schema_version' ORDER BY key",
        "SELECT * FROM binding",
        "SELECT * FROM pending ORDER BY seq",
        "SELECT ingest_key,body_sha256,first_attempt_ms,delivered_at_ms FROM receipt",
        "SELECT * FROM session_agent",
        "SELECT * FROM sqlite_sequence",
    ]
    .into_iter()
    .map(|sql| {
        let mut stmt = conn.prepare(sql).unwrap();
        let n = stmt.column_count();
        let rows: Vec<String> = stmt
            .query_map([], |r| {
                Ok((0..n)
                    .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                    .collect::<Vec<_>>()
                    .join("|"))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        rows.join("\n")
    })
    .collect()
}

#[test]
fn v1_migration_preserves_every_existing_value_and_concurrent_reopens() {
    let dir = v1_fixture("migration");
    let before = snapshot(&rusqlite::Connection::open(dir.join("relay.sqlite")).unwrap());
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let dir = dir.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                Queue::open(&dir).unwrap();
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let queue = Queue::open(&dir).unwrap();
    assert_eq!(queue.stats(200).unwrap().receipt_outcomes["unknown"], 1);
    let conn = rusqlite::Connection::open(dir.join("relay.sqlite")).unwrap();
    assert_eq!(snapshot(&conn), before);
    assert_eq!(
        conn.query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "2"
    );
    assert_eq!(
        conn.query_row("SELECT outcome FROM receipt", [], |r| r
            .get::<_, Option<String>>(0))
            .unwrap(),
        None
    );
}

#[test]
fn journal_transition_retries_sqlite_lock_without_losing_v1_values() {
    use queue_under_test::OpenPhase;
    let dir = v1_fixture("journal-transition");
    let holder = rusqlite::Connection::open(dir.join("relay.sqlite")).unwrap();
    let before = snapshot(&holder);
    let mut busy = 0;
    let result = queue_under_test::Queue::open_for_test(
        &dir,
        std::time::Duration::from_secs(10),
        &mut |phase| match phase {
            OpenPhase::BeforeJournal => holder.execute_batch("BEGIN IMMEDIATE;").unwrap(),
            OpenPhase::JournalBusy => {
                busy += 1;
                holder.execute_batch("ROLLBACK;").unwrap();
            }
            _ => {}
        },
    );
    assert_eq!(busy, 1, "the real holder must force SQLITE_BUSY");
    let queue = result.unwrap();
    assert_eq!(snapshot(&holder), before);
    assert_eq!(queue.stats(200).unwrap().receipt_outcomes["unknown"], 1);
    assert_eq!(
        holder
            .query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "wal"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["relay.sqlite", "relay.sqlite-wal", "relay.sqlite-shm"] {
            assert_eq!(
                std::fs::metadata(dir.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}

#[test]
fn a_foreign_identity_after_preflight_is_refused_without_mutation() {
    use queue_under_test::OpenPhase;
    let dir = v1_fixture("foreign-after-preflight");
    let path = dir.join("relay.sqlite");
    let holder = rusqlite::Connection::open(&path).unwrap();
    let mut before = Vec::new();
    let result = queue_under_test::Queue::open_for_test(
        &dir,
        std::time::Duration::from_secs(10),
        &mut |phase| {
            if phase == OpenPhase::AfterPreflight {
                holder
                    .execute("UPDATE meta SET value='foreign' WHERE key='identity'", [])
                    .unwrap();
                before = std::fs::read(&path).unwrap();
            }
        },
    );
    assert!(format!("{:#}", result.unwrap_err()).contains("not a usable relay queue"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    for name in fsguard::SIDECARS {
        assert!(!dir.join(name).exists(), "foreign queue gained {name}");
    }
}

#[test]
fn journal_transition_has_one_bounded_wait_budget() {
    use queue_under_test::OpenPhase;
    let dir = v1_fixture("journal-timeout");
    let path = dir.join("relay.sqlite");
    let holder = rusqlite::Connection::open(&path).unwrap();
    let before = snapshot(&holder);
    let timeout = std::time::Duration::from_millis(40);
    let start = std::time::Instant::now();
    let mut busy = 0;
    let error = queue_under_test::Queue::open_for_test(&dir, timeout, &mut |phase| match phase {
        OpenPhase::BeforeJournal => holder.execute_batch("BEGIN IMMEDIATE;").unwrap(),
        OpenPhase::JournalBusy => busy += 1,
        _ => {}
    })
    .unwrap_err();
    assert!(queue_under_test::is_lock_contention(&error), "{error:#}");
    assert!(busy > 0);
    assert!(start.elapsed() < std::time::Duration::from_secs(1));
    assert_eq!(snapshot(&holder), before);
    assert_eq!(
        holder
            .query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "delete"
    );
    holder.execute_batch("ROLLBACK;").unwrap();
    // The committed migration is usable after contention clears.
    Queue::open(&dir).unwrap();
}

#[test]
fn journal_reclassification_busy_preserves_committed_migration_and_retries_same_queue() {
    use queue_under_test::OpenPhase;
    let dir = v1_fixture("exclusive-reclassification");
    let path = dir.join("relay.sqlite");
    let holder = rusqlite::Connection::open(&path).unwrap();
    let before = snapshot(&holder);
    let mut committed = Vec::new();
    let mut busy = 0;
    let start = std::time::Instant::now();
    // Room for several attempts on every platform: Windows sleeps in ~15 ms
    // ticks, so a 40 ms budget could end after one slow attempt and one sleep.
    let result = queue_under_test::Queue::open_for_test(
        &dir,
        std::time::Duration::from_millis(200),
        &mut |phase| match phase {
            OpenPhase::BeforeJournal => {
                assert_eq!(
                    holder
                        .query_row(
                            "SELECT value FROM meta WHERE key='schema_version'",
                            [],
                            |r| r.get::<_, String>(0)
                        )
                        .unwrap(),
                    "2"
                );
                holder.prepare("SELECT outcome FROM receipt").unwrap();
                committed = std::fs::read(&path).unwrap();
                holder.execute_batch("BEGIN EXCLUSIVE;").unwrap();
            }
            OpenPhase::JournalBusy => busy += 1,
            _ => {}
        },
    );
    let error = result.unwrap_err();
    assert!(start.elapsed() < std::time::Duration::from_secs(1));
    assert!(busy > 1, "one short budget must contain retries");
    assert!(queue_under_test::is_lock_contention(&error));
    let display = format!("{error:#}");
    assert!(display.contains("queue busy"), "{display}");
    assert!(
        display.contains("retry opening the same queue"),
        "{display}"
    );
    for misleading in [
        "not a usable relay queue",
        "nothing was modified",
        "empty directory",
        "restore",
    ] {
        assert!(!display.contains(misleading), "{display}");
    }
    let cause = error.downcast_ref::<rusqlite::Error>().unwrap().to_string();
    assert_eq!(display.matches(&cause).count(), 1, "{display}");
    assert!(std::fs::read(&path).unwrap() == committed);
    assert_eq!(snapshot(&holder), before);
    holder.execute_batch("ROLLBACK;").unwrap();
    let queue = Queue::open(&dir).unwrap();
    assert_eq!(queue.stats(200).unwrap().receipt_outcomes["unknown"], 1);
    assert_eq!(snapshot(&holder), before);
}

#[test]
fn classification_read_failures_display_the_sqlite_cause_once_and_preserve_foreign_bytes() {
    for malformed_file in [false, true] {
        let dir = if malformed_file {
            let sandbox = fixture("invalid-sqlite");
            let dir = fsguard::prepare_queue_dir(&queue_dir(&sandbox)).unwrap();
            let path = dir.join("relay.sqlite");
            fsguard::create_private_file(&path).unwrap();
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(b"not a SQLite database")
                .unwrap();
            Sandbox {
                _guard: sandbox._guard,
                path: dir,
            }
        } else {
            let dir = v1_fixture("missing-meta");
            let conn = rusqlite::Connection::open(dir.join("relay.sqlite")).unwrap();
            conn.execute_batch("DROP TABLE meta;").unwrap();
            dir
        };
        let path = dir.join("relay.sqlite");
        let before = std::fs::read(&path).unwrap();
        let error = Queue::open(&dir).unwrap_err();
        assert!(!queue_under_test::is_lock_contention(&error));
        let display = format!("{error:#}");
        assert!(display.contains("not a usable relay queue"), "{display}");
        let cause = error.downcast_ref::<rusqlite::Error>().unwrap().to_string();
        assert_eq!(display.matches(&cause).count(), 1, "{display}");
        assert!(std::fs::read(&path).unwrap() == before);
        for name in fsguard::SIDECARS {
            assert!(!dir.join(name).exists());
        }
    }
}

#[test]
fn a_foreign_database_with_a_writer_is_refused_before_write_coordination() {
    let dir = v1_fixture("foreign-with-writer");
    let path = dir.join("relay.sqlite");
    let holder = rusqlite::Connection::open(&path).unwrap();
    holder
        .execute_batch("UPDATE meta SET value='foreign' WHERE key='identity'; BEGIN IMMEDIATE;")
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let mut reached_after_preflight = false;
    let error = queue_under_test::Queue::open_for_test(
        &dir,
        std::time::Duration::from_millis(40),
        &mut |_| reached_after_preflight = true,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("not a usable relay queue"),
        "{error:#}"
    );
    assert!(!reached_after_preflight);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    holder.execute_batch("ROLLBACK;").unwrap();
}

#[test]
fn a_foreign_identity_during_journal_retry_is_refused_without_mutation() {
    use queue_under_test::OpenPhase;
    for begin in ["BEGIN IMMEDIATE;", "BEGIN EXCLUSIVE;"] {
        let dir = v1_fixture("foreign-during-retry");
        let path = dir.join("relay.sqlite");
        let holder = rusqlite::Connection::open(&path).unwrap();
        let mut before = Vec::new();
        let mut busy = 0;
        let result = queue_under_test::Queue::open_for_test(
            &dir,
            std::time::Duration::from_secs(10),
            &mut |phase| match phase {
                OpenPhase::BeforeJournal => holder.execute_batch(begin).unwrap(),
                OpenPhase::JournalBusy => {
                    busy += 1;
                    holder
                        .execute_batch(
                            "ROLLBACK; UPDATE meta SET value='foreign' WHERE key='identity';",
                        )
                        .unwrap();
                    before = std::fs::read(&path).unwrap();
                }
                _ => {}
            },
        );
        let error = result.unwrap_err();
        assert!(format!("{error:#}").contains("not a usable relay queue"));
        assert!(!queue_under_test::is_lock_contention(&error));
        assert_eq!(busy, 1);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        for name in fsguard::SIDECARS {
            assert!(!dir.join(name).exists(), "foreign queue gained {name}");
        }
    }
}

#[test]
fn only_sqlite_busy_and_locked_are_retryable() {
    for (code, retry) in [
        (5, true),
        (6, true),
        (517, true),
        (262, true),
        (1, false),
        (8, false),
        (11, false),
        (17, false),
        (26, false),
    ] {
        let error = anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            Some("database is locked".into()),
        ))
        .context("configure queue");
        assert_eq!(
            queue_under_test::is_lock_contention(&error),
            retry,
            "code {code}"
        );
    }
    assert!(!queue_under_test::is_lock_contention(&anyhow::anyhow!(
        "database is locked"
    )));
    assert!(!queue_under_test::is_lock_contention(
        &rusqlite::Error::InvalidQuery.into()
    ));
}

#[test]
fn journal_retry_refuses_a_changed_version_or_empty_database_without_mutation() {
    use queue_under_test::OpenPhase;
    for mutation in [
        "UPDATE meta SET value='1' WHERE key='schema_version';",
        "DROP TABLE meta; DROP TABLE binding; DROP TABLE pending; DROP TABLE receipt; DROP TABLE session_agent;",
    ] {
        let dir = v1_fixture("changed-version-or-empty-db");
        let path = dir.join("relay.sqlite");
        let holder = rusqlite::Connection::open(&path).unwrap();
        let mut before = Vec::new();
        let mut busy = 0;
        let result = queue_under_test::Queue::open_for_test(
            &dir,
            std::time::Duration::from_secs(10),
            &mut |phase| match phase {
                OpenPhase::BeforeJournal => holder.execute_batch("BEGIN IMMEDIATE;").unwrap(),
                OpenPhase::JournalBusy => {
                    busy += 1;
                    holder.execute_batch("ROLLBACK;").unwrap();
                    holder.execute_batch(mutation).unwrap();
                    before = std::fs::read(&path).unwrap();
                }
                _ => {}
            },
        );
        assert!(
            format!("{:#}", result.unwrap_err()).contains("changed before journal configuration")
        );
        assert_eq!(busy, 1);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        for name in fsguard::SIDECARS {
            assert!(!dir.join(name).exists(), "changed queue gained {name}");
        }
    }
}

#[test]
fn fresh_queue_init_and_v1_reopens_work_across_processes() {
    for legacy in [false, true] {
        let dir = if legacy {
            v1_fixture("process-reopen")
        } else {
            let sandbox = fixture("process-init");
            let path = queue_dir(&sandbox);
            Sandbox {
                _guard: sandbox._guard,
                path,
            }
        };
        let before = legacy
            .then(|| snapshot(&rusqlite::Connection::open(dir.join("relay.sqlite")).unwrap()));
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let dir = dir.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    let mut command =
                        std::process::Command::new(env!("CARGO_BIN_EXE_ai-memory-relay"));
                    if legacy {
                        command.arg("status");
                    } else {
                        command.args([
                            "init",
                            "--server-url",
                            "http://127.0.0.1:9/",
                            "--producer",
                            "example.runtime",
                            "--actor",
                            "operator-a",
                            "--workspace",
                            "team",
                            "--project",
                            "app",
                        ]);
                    }
                    command.arg("--queue-dir").arg(dir);
                    barrier.wait();
                    command.output().unwrap()
                })
            })
            .collect();
        let outputs: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        for output in outputs {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let queue = Queue::open(&dir).unwrap();
        if let Some(before) = before {
            assert_eq!(
                snapshot(&rusqlite::Connection::open(queue.path()).unwrap()),
                before
            );
        }
        assert_eq!(queue.binding().unwrap().project, "app");
    }
}

#[test]
fn interrupted_and_failed_migrations_roll_back_and_recover() {
    let dir = v1_fixture("rollback-migration");
    let mut conn = rusqlite::Connection::open(dir.join("relay.sqlite")).unwrap();
    let before = snapshot(&conn);
    {
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        tx.execute_batch("ALTER TABLE receipt ADD COLUMN outcome TEXT; UPDATE meta SET value='2' WHERE key='schema_version';").unwrap();
        // Dropping an uncommitted transaction models interruption.
    }
    assert_eq!(snapshot(&conn), before);
    conn.execute_batch("CREATE TRIGGER refuse_migration BEFORE UPDATE ON meta BEGIN SELECT RAISE(ABORT,'injected migration failure'); END;").unwrap();
    assert!(Queue::open(&dir).is_err());
    assert_eq!(snapshot(&conn), before);
    assert!(conn.prepare("SELECT outcome FROM receipt").is_err());
    conn.execute_batch("DROP TRIGGER refuse_migration;")
        .unwrap();
    drop(conn);
    Queue::open(&dir).unwrap();
}

#[test]
fn unknown_schemas_refuse_without_mutation() {
    for version in ["3", "garbage", "0"] {
        let dir = v1_fixture("future-schema");
        let conn = rusqlite::Connection::open(dir.join("relay.sqlite")).unwrap();
        conn.execute(
            "UPDATE meta SET value=?1 WHERE key='schema_version'",
            [version],
        )
        .unwrap();
        drop(conn);
        let path = dir.join("relay.sqlite");
        let before = std::fs::read(&path).unwrap();
        assert!(Queue::open(&dir).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[test]
fn sparse_drop_ack_dequeues_and_records_outcomes() {
    let root = fixture("sparse-outcomes");
    let server = stub(vec![Reply::Json(429, r#"{"accepted":1,"accepted_indices":[0,2],"results":[{"index":0,"outcome":"stored"},{"index":2,"outcome":"dropped_policy"}]}"#.into())]);
    let dir = bind(&root, &server.url());
    let mut queue = Queue::open(&dir).unwrap();
    queue
        .enqueue(
            &(0..3)
                .map(|i| valid(&format!("e{i}"), "codex", "session-start", &format!("s{i}")))
                .collect::<Vec<_>>(),
            ai_memory_relay::now_ms(),
        )
        .unwrap();
    drop(queue);
    let report = flush(&dir);
    assert!(report.failure.is_none(), "{report:?}");
    assert_eq!(pending_count(&dir), 1);
    let stats = Queue::open(&dir)
        .unwrap()
        .stats(ai_memory_relay::now_ms())
        .unwrap();
    assert_eq!(stats.receipt_outcomes["stored"], 1);
    assert_eq!(stats.receipt_outcomes["dropped_policy"], 1);
    assert!(
        report
            .summary
            .iter()
            .any(|s| s.contains("\"dropped_policy\":1"))
    );
}

#[test]
fn malformed_results_keep_every_item_pending() {
    let cases = [
        r#"{"accepted":2,"results":[]}"#,
        r#"{"accepted":2,"results":null}"#,
        r#"{"accepted":2,"results":[{"index":0,"outcome":"stored"},{"index":1}]}"#,
        r#"{"accepted":2,"results":[{"index":0,"outcome":"stored"},{"index":1,"outcome":7}]}"#,
        r#"{"accepted":2,"results":[{"index":0,"outcome":"stored"},{"index":0,"outcome":"stored"}]}"#,
        r#"{"accepted":1,"results":[{"index":0,"outcome":"stored"},{"index":1,"outcome":"stored"}]}"#,
        r#"{"accepted":2,"results":[{"index":1,"outcome":"stored"},{"index":0,"outcome":"stored"}]}"#,
        r#"{"accepted":2,"results":[{"index":0,"outcome":"stored"},{"index":2,"outcome":"stored"}]}"#,
        r#"{"accepted":2,"failed_index":1,"results":[{"index":0,"outcome":"stored"},{"index":1,"outcome":"stored"}]}"#,
    ];
    for body in cases {
        let root = fixture("invalid-results");
        let server = stub(vec![Reply::Json(200, body.into())]);
        let dir = bind(&root, &server.url());
        let mut queue = Queue::open(&dir).unwrap();
        queue
            .enqueue(
                &[
                    valid("a", "codex", "session-start", "a"),
                    valid("b", "codex", "session-start", "b"),
                ],
                ai_memory_relay::now_ms(),
            )
            .unwrap();
        drop(queue);
        assert!(flush(&dir).failure.is_some(), "{body}");
        assert_eq!(pending_count(&dir), 2, "{body}");
        assert_eq!(
            Queue::open(&dir)
                .unwrap()
                .stats(ai_memory_relay::now_ms())
                .unwrap()
                .receipts,
            0
        );
    }
}

#[test]
fn old_server_and_future_outcome_strings_are_unknown() {
    for body in [
        r#"{"accepted":1}"#,
        r#"{"accepted":1,"results":[{"index":0,"outcome":"future_outcome"}]}"#,
    ] {
        let root = fixture("unknown-outcome");
        let server = stub(vec![Reply::Json(200, body.into())]);
        let dir = bind(&root, &server.url());
        let mut queue = Queue::open(&dir).unwrap();
        queue
            .enqueue(
                &[valid("a", "codex", "session-start", "a")],
                ai_memory_relay::now_ms(),
            )
            .unwrap();
        drop(queue);
        assert!(flush(&dir).failure.is_none());
        assert_eq!(
            Queue::open(&dir)
                .unwrap()
                .stats(ai_memory_relay::now_ms())
                .unwrap()
                .receipt_outcomes["unknown"],
            1
        );
    }
}

#[test]
fn receipt_conflicts_preserve_first_known_outcome_and_counts_use_retained_window() {
    let dir = v1_fixture("receipt-window");
    let mut queue = Queue::open(&dir).unwrap();
    let conn = rusqlite::Connection::open(dir.join("relay.sqlite")).unwrap();
    conn.execute_batch("INSERT INTO receipt VALUES('key-a','hash-a',101,102,'stored');")
        .unwrap();
    queue.confirm(&[("key-a".into(), "replayed")], 200).unwrap();
    let (hash, first, result): (String, i64, String) = conn
        .query_row(
            "SELECT body_sha256,first_attempt_ms,outcome FROM receipt WHERE ingest_key='key-a'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (hash, first, result),
        ("hash-a".into(), 101, "stored".into())
    );
    let now = identity::RETRY_WINDOW_MS + 102;
    let stats = queue.stats(now).unwrap();
    assert_eq!(stats.receipt_outcomes["stored"], 0);
    assert_eq!(stats.receipt_outcomes["unknown"], 1);
    queue.prune(now).unwrap();
    assert_eq!(queue.stats(now).unwrap().receipts, 1);
    let document: serde_json::Value =
        serde_json::from_str(&relay::status(&dir).unwrap().summary[0]).unwrap();
    assert_eq!(
        document["receipt_outcomes"].as_object().unwrap().len(),
        ack::OUTCOMES.len()
    );
}

#[test]
fn receipt_conflicts_promote_legacy_unknown_outcomes() {
    for previous in [None, Some("unknown")] {
        let dir = v1_fixture("receipt-promote");
        let mut queue = Queue::open(&dir).unwrap();
        let conn = rusqlite::Connection::open(dir.join("relay.sqlite")).unwrap();
        conn.execute(
            "INSERT INTO receipt VALUES('key-a','hash-a',101,102,?1)",
            [previous],
        )
        .unwrap();
        queue.confirm(&[("key-a".into(), "stored")], 200).unwrap();
        let actual: String = conn
            .query_row(
                "SELECT outcome FROM receipt WHERE ingest_key='key-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(actual, "stored");
    }
}

#[test]
fn every_current_outcome_is_persisted_and_flush_counts_acknowledgements() {
    let root = fixture("all-outcomes");
    let known = &ack::OUTCOMES[..ack::OUTCOMES.len() - 1];
    let results: Vec<_> = known
        .iter()
        .enumerate()
        .map(|(index, outcome)| serde_json::json!({"index":index,"outcome":outcome}))
        .collect();
    let body = serde_json::json!({"accepted":known.len(),"accepted_indices":(0..known.len()).collect::<Vec<_>>(),"results":results});
    let server = stub(vec![Reply::Json(200, body.to_string())]);
    let dir = bind(&root, &server.url());
    let mut queue = Queue::open(&dir).unwrap();
    queue
        .enqueue(
            &(0..known.len())
                .map(|i| valid(&format!("e{i}"), "codex", "session-start", &format!("s{i}")))
                .collect::<Vec<_>>(),
            ai_memory_relay::now_ms(),
        )
        .unwrap();
    drop(queue);
    let report = flush(&dir);
    assert!(report.failure.is_none(), "{report:?}");
    assert_eq!(pending_count(&dir), 0);
    let stats = Queue::open(&dir)
        .unwrap()
        .stats(ai_memory_relay::now_ms())
        .unwrap();
    let summary = report
        .summary
        .iter()
        .find_map(|s| s.strip_prefix("acknowledged_outcomes: "))
        .unwrap();
    let counts: serde_json::Value = serde_json::from_str(summary).unwrap();
    for outcome in known {
        assert_eq!(stats.receipt_outcomes[*outcome], 1);
        assert_eq!(counts[*outcome], 1);
    }
    assert_eq!(counts["unknown"], 0);
}

#[test]
fn results_match_sparse_partial_failure_and_empty_acks() {
    for (body, len, expected) in [
        (
            r#"{"accepted":1,"accepted_indices":[0,2],"failed_index":3,"results":[{"index":0,"outcome":"replayed"},{"index":2,"outcome":"dropped_collision"}]}"#,
            5,
            vec![0, 2],
        ),
        (
            r#"{"accepted":0,"accepted_indices":[],"results":[]}"#,
            0,
            vec![],
        ),
        (
            r#"{"accepted":0,"accepted_indices":[],"results":[]}"#,
            3,
            vec![],
        ),
    ] {
        let parsed: BatchAck = serde_json::from_str(body).unwrap();
        assert_eq!(ack::validate(len, &parsed).unwrap(), expected);
    }
}

#[test]
fn sensitive_envelope_is_rejected_before_enqueue_with_clean_control() {
    let root = fixture("sensitive-envelope");
    let dir = bind(&root, "http://127.0.0.1:49374");
    enqueue(
        &dir,
        &root,
        "before.json",
        serde_json::json!([event("opaque-界", "codex", "session-start", "sessão-界")]),
    )
    .unwrap();
    assert_eq!(pending_count(&dir), 1);
    for field in [
        "session_id",
        "cwd",
        "producer",
        "actor",
        "event_id",
        "agent",
        "event",
        "ingest_key",
    ] {
        let mut input = event("a", "codex", "session-start", "a");
        input["body"][field] = "Bearer abcdefghijklmnop".into();
        let error = enqueue(&dir, &root, "input.json", serde_json::json!([input])).unwrap_err();
        assert!(!error.to_string().contains("abcdefghijklmnop"));
        assert_eq!(pending_count(&dir), 1);
    }
    enqueue(
        &dir,
        &root,
        "control.json",
        serde_json::json!([event("a", "codex", "session-start", "a")]),
    )
    .unwrap();
    assert_eq!(pending_count(&dir), 2);
}

#[test]
fn new_native_bodies_are_sanitized_before_hash_enqueue_and_exact_retry() {
    let root = fixture("ingress-sanitized-retry");
    let server = stub(vec![Reply::Close, Reply::AcceptAll]);
    let dir = bind(&root, &server.url());
    let mut raw = event("native-event", "codex", "post-tool-use", "native-session");
    for (name, value) in [
        ("producer", "example.runtime"),
        ("event_id", "native-event"),
        ("agent", "codex"),
        ("event", "post-tool-use"),
        ("ingest_key", "native-ingest-key"),
    ] {
        raw["body"][name] = value.into();
    }
    raw["body"]["output"] =
        "\u{1b}[32mresult\u{1b}[0m\u{202e} token=getToken() Bearer abcdefghijklmnop".into();
    raw["body"]["tool_input"] = serde_json::json!({"nested": {"password": "process.env.X", "api-key": "tiny", "items": ["password=abcdefghi", "token=abcdefghijklmnop[REDACTED]"]}});
    raw["body"]["key_cases"] = serde_json::json!([{
        "accessToken":"fixture-value", "refreshToken":"fixture-value", "clientSecret":"fixture-value", "privateKey":"fixture-value", "x-api-key":"fixture-value", "OPENAI_API_KEY":"fixture-value", "credentials":"fixture-value", "cookie":"fixture-value"
    }]);
    raw["body"]["key_forms"] = serde_json::json!([{"auth":"fixture-value","jwt":"fixture-value","pwd":"fixture-value","bearer":"fixture-value","sessionKey":"fixture-value","signature":"fixture-value"}]);
    raw["body"]["plural_forms"] = serde_json::json!([{
        "api_keys":"fixture-value", "access_keys":"fixture-value", "private_keys":"fixture-value", "session_keys":"fixture-value",
        "apiKeys":["fixture-value"], "accessKeys":["fixture-value"], "privateKeys":["fixture-value"], "sessionKeys":["fixture-value"]
    }]);
    raw["body"]["controls"] = serde_json::json!({"author":"fixture-value","authority":"fixture-value","key":"fixture-value","keys":["fixture-value"]});
    raw["body"]["numeric_cases"] = serde_json::json!({"token":123456,"access_tokens":123456,"private_token_count":123456,"auth_token_usage":123456,"token_usage":{"input_tokens":12}});
    let metrics = serde_json::json!({"max_tokens":12,"input_tokens":34,"token_count":0,"token_usage":1.25,"output_tokens":9007199254740993_u64,"total_tokens":89,"totalTokens":89,"cache_read_input_tokens":2,"cache_creation_input_tokens":3,"prompt_tokens":5,"completion_tokens":7,"reasoning_tokens":11});
    raw["body"]["metrics"] = metrics.clone();
    enqueue(&dir, &root, "native.json", serde_json::json!([raw.clone()])).unwrap();
    enqueue(&dir, &root, "repeat.json", serde_json::json!([raw])).unwrap();
    assert_eq!(pending_count(&dir), 1);
    let conn = rusqlite::Connection::open(dir.join(ai_memory_relay::fsguard::DB_FILE)).unwrap();
    let (body, digest, key, id, agent, event, session): (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
    ) = conn
        .query_row(
            "SELECT body_json,body_sha256,ingest_key,event_id,agent,event,session_id FROM pending",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .unwrap();
    let safe: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (&id[..], &agent[..], &event[..], &session[..]),
        ("native-event", "codex", "post-tool-use", "native-session")
    );
    assert_eq!(
        key,
        identity::ingest_key(
            "example.runtime",
            "operator-a",
            &agent,
            &session,
            &event,
            &id
        )
    );
    for (name, value) in [
        ("producer", "example.runtime"),
        ("event_id", "native-event"),
        ("agent", "codex"),
        ("event", "post-tool-use"),
        ("ingest_key", "native-ingest-key"),
    ] {
        assert_eq!(safe[name], value);
    }
    assert_eq!(safe["cwd"], "/work/app");
    assert!(
        !safe["output"]
            .as_str()
            .unwrap()
            .contains(['\u{1b}', '\u{202e}'])
    );
    assert!(!body.contains("abcdefghijklmnop"));
    assert!(!body.contains("process.env.X"));
    assert!(!body.contains("tiny"));
    assert_eq!(safe["tool_input"]["nested"]["api-key"], "[REDACTED]");
    assert!(
        safe["key_forms"][0]
            .as_object()
            .unwrap()
            .values()
            .all(|value| value == "[REDACTED]")
    );
    assert!(
        safe["plural_forms"][0]
            .as_object()
            .unwrap()
            .values()
            .all(|value| value == "[REDACTED]")
    );
    assert!(
        safe["numeric_cases"]
            .as_object()
            .unwrap()
            .values()
            .all(|value| value == "[REDACTED]")
    );
    assert!(
        safe["controls"]
            == serde_json::json!({"author":"fixture-value","authority":"fixture-value","key":"fixture-value","keys":["fixture-value"]})
    );
    assert!(
        safe["key_cases"][0]
            .as_object()
            .unwrap()
            .values()
            .all(|value| value == "[REDACTED]")
    );
    assert!(safe["metrics"] == metrics);
    assert!(serde_json::to_vec(&safe["metrics"]).unwrap() == serde_json::to_vec(&metrics).unwrap());
    assert_eq!(
        safe["output"],
        ai_memory_client::sanitize_external_text(
            "\u{1b}[32mresult\u{1b}[0m\u{202e} token=getToken() Bearer abcdefghijklmnop"
        )
    );

    ai_memory_client::check_body(&safe).unwrap();
    let validated = identity::validate(
        0,
        InputEvent {
            event_id: id,
            agent,
            event,
            body: safe.clone(),
        },
        "example.runtime",
        "operator-a",
    )
    .unwrap();
    assert_eq!(validated.body_json, body);
    assert_eq!(validated.body_sha256, digest);
    assert!(flush(&dir).failure.is_none());
    assert_eq!(pending_count(&dir), 0);
    let sent = server.bodies();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], sent[1]);
    assert_eq!(sent[0][0]["body"], safe);
    assert_eq!(serde_json::to_string(&sent[0][0]["body"]).unwrap(), body);
    assert_eq!(
        Queue::open(&dir)
            .unwrap()
            .stats(ai_memory_relay::now_ms())
            .unwrap()
            .receipts,
        1
    );
}

#[test]
fn unsafe_legacy_pending_is_dropped_locally_and_never_blocks_its_session() {
    let root = fixture("unsafe-legacy-pending");
    let server = stub(vec![Reply::AcceptAll, Reply::AcceptAll]);
    let dir = bind(&root, &server.url());
    let mut legacy = valid("native-event", "codex", "session-start", "native-session");
    legacy.body_json = "{ \"prompt\": \"Bearer abcdefghijklmnop\", \"cwd\": \"/native/path\", \"session_id\": \"native-session\" }".into();
    let mut queue = Queue::open(&dir).unwrap();
    queue
        .enqueue(&[legacy.clone()], ai_memory_relay::now_ms())
        .unwrap();
    let later = valid("later-clean", "codex", "session-end", "native-session");
    let clean = valid("clean-event", "codex", "session-start", "clean-session");
    queue
        .enqueue(&[later.clone(), clean.clone()], ai_memory_relay::now_ms())
        .unwrap();
    drop(queue);

    let report = relay::flush(&dir, &FlushOptions::default()).unwrap();
    assert!(report.failure.is_none(), "{report:?}");
    assert!(!format!("{report:?}").contains("abcdefghijklmnop"));
    assert!(
        report
            .summary
            .iter()
            .any(|line| line.starts_with("1 event(s) queued before local sanitation")),
        "{report:?}"
    );
    assert!(!report.pending);
    assert_eq!(pending_count(&dir), 0);

    // The unsafe head never reached the wire; the clean session and the unsafe
    // session's later event both did.
    let sent: Vec<serde_json::Value> = server
        .bodies()
        .iter()
        .flat_map(|batch| batch.as_array().unwrap().clone())
        .collect();
    assert!(
        !serde_json::to_string(&sent)
            .unwrap()
            .contains("abcdefghijklmnop")
    );
    let ids: Vec<&str> = sent
        .iter()
        .map(|item| item["body"]["session_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&"clean-session"));
    assert!(ids.contains(&"native-session"));

    let conn = rusqlite::Connection::open(dir.join(ai_memory_relay::fsguard::DB_FILE)).unwrap();
    let outcome: Option<String> = conn
        .query_row(
            "SELECT outcome FROM receipt WHERE ingest_key = ?1",
            [&legacy.ingest_key],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(outcome.as_deref(), Some("dropped_policy"));
}

#[test]
fn legacy_binding_scope_spaces_are_preserved_on_delivery() {
    let root = fixture("legacy-scope-spaces");
    let server = stub(vec![Reply::AcceptAll]);
    let dir = queue_dir(&root);
    relay::init(
        &dir,
        &server.url(),
        "example.runtime",
        "operator-a",
        " team ",
        " app ",
    )
    .unwrap();
    enqueue(
        &dir,
        &root,
        "native.json",
        serde_json::json!([event("e", "codex", "session-start", "s")]),
    )
    .unwrap();
    assert!(flush(&dir).failure.is_none());
    let bodies = server.bodies();
    let url = url::Url::parse(bodies[0][0]["url"].as_str().unwrap()).unwrap();
    let pairs: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(pairs["workspace"], " team ");
    assert_eq!(pairs["project"], " app ");
}

#[test]
fn opaque_unicode_native_identity_ships_unchanged() {
    let root = fixture("privacy-unicode-control");
    let server = stub(vec![Reply::AcceptAll]);
    let dir = bind(&root, &server.url());
    let mut input = event("event-界", "codex", "session-start", " sessão-界/opaque ");
    input["body"]["cwd"] = "/work/界".into();
    input["body"]["_ai_memory_capture"] =
        serde_json::json!({"actor":"untrusted-label", "authority":"untrusted provenance"});
    let original = input["body"].clone();
    enqueue(&dir, &root, "input.json", serde_json::json!([input])).unwrap();
    assert!(flush(&dir).failure.is_none());
    assert_eq!(server.bodies()[0][0]["body"], original);
    let bodies = server.bodies();
    let url = url::Url::parse(bodies[0][0]["url"].as_str().unwrap()).unwrap();
    assert_eq!(bodies[0][0]["body"]["session_id"], " sessão-界/opaque ");
    assert_eq!(
        url.query_pairs()
            .find(|(key, _)| key == "ingest_key")
            .unwrap()
            .1,
        identity::ingest_key(
            "example.runtime",
            "operator-a",
            "codex",
            " sessão-界/opaque ",
            "session-start",
            "event-界"
        )
    );
    assert!(
        server
            .heads
            .lock()
            .unwrap()
            .iter()
            .all(|head| head.starts_with("POST /hook/batch "))
    );
}

#[test]
fn sensitive_outer_identity_and_body_authority_are_refused_before_enqueue() {
    let root = fixture("privacy-outer-refusal");
    let dir = bind(&root, "http://127.0.0.1:49374");
    enqueue(
        &dir,
        &root,
        "before.json",
        serde_json::json!([event("before", "codex", "session-start", "control")]),
    )
    .unwrap();
    for field in ["event_id", "agent", "event"] {
        let mut attack = event("native", "codex", "session-start", "session");
        attack[field] = "sk-1234567890abcdefghijklmnop".into();
        let result = enqueue(&dir, &root, "attack.json", serde_json::json!([attack]));
        assert!(result.is_err(), "sensitive outer identity must be refused");
        assert!(!result.unwrap_err().to_string().contains("abcdefghijklmnop"));
        assert_eq!(pending_count(&dir), 1);
    }
    for field in ["workspace", "project", "actor", "author_id", "headers"] {
        let mut attack = event("native", "codex", "session-start", "session");
        attack["body"][field] = "forged".into();
        assert!(enqueue(&dir, &root, "attack.json", serde_json::json!([attack])).is_err());
        assert_eq!(pending_count(&dir), 1);
    }
    enqueue(
        &dir,
        &root,
        "after.json",
        serde_json::json!([event("after", "codex", "session-end", "control")]),
    )
    .unwrap();
    assert_eq!(pending_count(&dir), 2);
}

#[test]
fn sensitive_binding_is_refused_before_queue_creation() {
    let root = fixture("privacy-binding-refusal");
    let safe = ["example.runtime", "operator-a", "team", "app"];
    let before = root.join("control-before");
    relay::init(
        &before,
        "http://127.0.0.1:49374",
        safe[0],
        safe[1],
        safe[2],
        safe[3],
    )
    .unwrap();
    for field in 0..4 {
        let mut attack = safe;
        attack[field] = "sk-1234567890abcdefghijklmnop";
        let dir = root.join(format!("attack-{field}"));
        let result = relay::init(
            &dir,
            "http://127.0.0.1:49374",
            attack[0],
            attack[1],
            attack[2],
            attack[3],
        );
        assert!(result.is_err(), "sensitive binding must be refused");
        assert!(!result.unwrap_err().to_string().contains("abcdefghijklmnop"));
        assert!(!dir.exists(), "sensitive binding must not create the queue");
    }
    relay::init(
        &root.join("control-after"),
        "http://127.0.0.1:49374",
        safe[0],
        safe[1],
        safe[2],
        safe[3],
    )
    .unwrap();
}

#[test]
fn unsafe_legacy_native_identity_is_dropped_and_its_session_continues() {
    let root = fixture("privacy-legacy-native-id");
    let server = stub(vec![Reply::AcceptAll, Reply::AcceptAll]);
    let dir = bind(&root, &server.url());
    let mut legacy = valid("old-event", "codex", "session-start", "old-session");
    legacy.event_id = "sk-1234567890abcdefghijklmnop".into();
    let mut queue = Queue::open(&dir).unwrap();
    let before = valid("before", "codex", "session-start", "clean-before");
    queue
        .enqueue(&[legacy.clone(), before.clone()], ai_memory_relay::now_ms())
        .unwrap();
    drop(queue);
    let report = flush(&dir);
    assert!(report.failure.is_none(), "{report:?}");
    assert!(!format!("{report:?}").contains("abcdefghijklmnop"));
    assert_eq!(pending_count(&dir), 0);

    enqueue(
        &dir,
        &root,
        "after.json",
        serde_json::json!([event("after", "codex", "session-end", "old-session")]),
    )
    .unwrap();
    let report = flush(&dir);
    assert!(report.failure.is_none(), "{report:?}");
    assert_eq!(pending_count(&dir), 0);

    let sent = server.bodies();
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[0][0]["body"],
        serde_json::from_str::<serde_json::Value>(&before.body_json).unwrap()
    );
    assert_eq!(sent[1][0]["body"]["session_id"], "old-session");
    assert!(
        server
            .heads
            .lock()
            .unwrap()
            .iter()
            .all(|head| !head.contains("abcdefghijklmnop")),
        "the unsafe native identity must never reach a request line"
    );
}
