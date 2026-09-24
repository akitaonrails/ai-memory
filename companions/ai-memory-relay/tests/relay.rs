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

// ---------------------------------------------------------------- fixtures

/// A kept sandbox. The path is printed so a failure points at the evidence.
fn fixture(name: &str) -> PathBuf {
    let root = tempfile::TempDir::new().unwrap().keep();
    let root = root.join(name);
    std::fs::create_dir_all(&root).unwrap();
    println!("fixture {name}: {}", root.display());
    root
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
