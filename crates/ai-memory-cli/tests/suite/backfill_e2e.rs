//! Use-case end-to-end test for `ai-memory backfill` (boot-time history import).
//!
//! Backfill exists to close the first-boot amnesia gap: when you install
//! ai-memory into a project you have already worked in, capture is forward-only,
//! so the very session you are resuming is invisible. Backfill imports the
//! pre-existing local harness transcripts **once** into a brand-new (empty)
//! project store by replaying their events through the same `/hook` ingress live
//! capture uses, so prior work becomes real sanitized observations that are
//! searchable via `memory_query`.
//!
//! This test drives the *shipped* `ai-memory backfill` subcommand against a real
//! spawned `ai-memory serve` HTTP server, exactly as an operator would, and
//! proves the whole user value in one flow:
//!
//! 1. **Import.** An empty store plus a planted Claude transcript for this cwd →
//!    backfill imports one session; the store's session count goes 0 → 1 and the
//!    transcript-only content is retrievable through `memory_query` (via the raw
//!    observation fallback, so this holds in zero-LLM / CI-safe mode).
//! 2. **Idempotency / emptiness gate.** A second `backfill` without `--force`
//!    is a no-op — it reports `skipped_non_empty` and imports nothing, so the
//!    session count stays at 1 (no duplicate import).
//! 3. **Sanitization.** A canary secret planted in the transcript is redacted
//!    end-to-end: it appears in no persisted state (wiki, db) and is not
//!    surfaced by `memory_query`. Mirrors `tests/e2e/handoff_smoke.sh`.
//!
//! Unix-gated: the fixture uses the POSIX-encoded `~/.claude/projects/<enc-cwd>/`
//! native layout (as the sibling `collect_local_sessions` unit test does);
//! cross-platform native-store discovery is owned by `ai-memory-workstream`.

#![cfg(unix)]

/// Spawns a server and several subprocesses: seconds, not milliseconds, so this
/// lives in the slow tier (`cargo tf` / CI), not the everyday loop.
mod slow {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};

    use crate::e2e_support::{
        ServerGuard, free_port, hermetic, run_cli, session_count, write_jsonl,
    };

    const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");
    const WORKSPACE: &str = "backfill-e2e-ws";
    const PROJECT: &str = "backfill-e2e-proj";
    /// A rare, non-secret token planted in the transcript; only this string can
    /// prove the imported content is what became retrievable.
    const UNIQUE: &str = "quokkazephyrbackfill";
    /// The greppable core of the canary secret. It is embedded in an `sk-…`
    /// shaped key, which the built-in sanitizer redacts wholesale — so this
    /// substring must survive nowhere in persisted state.
    const CANARY_MARKER: &str = "LEAKMEbackfill";
    /// OpenAI/Anthropic `sk-…` shape (16+ trailing key chars) → `[REDACTED:api_key]`.
    fn canary() -> String {
        format!("sk-canary{CANARY_MARKER}0123456789abcdef")
    }

    /// Recursively test whether any file under `root` contains `needle` (bytes,
    /// so it also catches the secret inside the binary SQLite db).
    fn tree_contains(root: &Path, needle: &str) -> Option<PathBuf> {
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else if let Ok(bytes) = fs::read(&path)
                    && bytes
                        .windows(needle.len())
                        .any(|window| window == needle.as_bytes())
                {
                    return Some(path);
                }
            }
        }
        None
    }

    /// Call a memory tool over the stateless Streamable-HTTP `/mcp` transport
    /// (the default `serve` mode: no `initialize` handshake needed) and return
    /// the joined text of the tool result.
    async fn call_tool(
        client: &reqwest::Client,
        base: &str,
        name: &str,
        arguments: Value,
    ) -> String {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        });
        let text = client
            .post(format!("{base}/mcp"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(body.to_string())
            .send()
            .await
            .expect("mcp request")
            .text()
            .await
            .expect("mcp body");
        let v: Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("non-JSON mcp reply: {text}: {e}"));
        assert!(v.get("error").is_none(), "JSON-RPC error: {text}");
        v.pointer("/result/content")
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("missing result.content: {text}"))
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn backfill_imports_local_history_into_an_empty_store_and_is_searchable() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let home = tempfile::tempdir().expect("home");
        let project = tempfile::tempdir().expect("project cwd");
        // The child's `std::env::current_dir()` returns the canonical path, and
        // the native-session discovery matches the transcript's `cwd` header
        // against it — so plant and encode with the same canonical path.
        let cwd = fs::canonicalize(project.path()).expect("canonicalize project cwd");

        // Plant a Claude transcript for this cwd under the native layout, with a
        // unique searchable phrase and a canary secret in the user prompt.
        let session_id = "11111111-2222-3333-4444-555555555555";
        let encoded = cwd.to_string_lossy().replace('/', "-");
        let session_dir = home.path().join(".claude").join("projects").join(encoded);
        fs::create_dir_all(&session_dir).expect("session dir");
        let prompt = format!(
            "Please note the {UNIQUE} architecture decision for later. \
             Admin token: {} (do not share).",
            canary()
        );
        write_jsonl(
            &session_dir.join(format!("{session_id}.jsonl")),
            &[
                json!({ "sessionId": session_id, "cwd": cwd.to_string_lossy() }),
                json!({
                    "type": "user",
                    "message": { "role": "user", "content": [{ "type": "text", "text": prompt }] },
                }),
                json!({
                    "type": "assistant",
                    "message": {
                        "role": "assistant",
                        "content": [{ "type": "text", "text": format!("Acknowledged the {UNIQUE} plan.") }],
                    },
                }),
            ],
        );

        // Start the real server (loopback, no auth, hermetic: no embedder, no
        // wiki watcher — a machine-global inotify instance concurrent server
        // children can exhaust, #745).
        let port = free_port();
        let base = format!("http://127.0.0.1:{port}");
        let server = ServerGuard(
            hermetic(BIN)
                .args([
                    "serve",
                    "--transport",
                    "http",
                    "--bind",
                    &format!("127.0.0.1:{port}"),
                    "--workspace",
                    WORKSPACE,
                    "--project",
                    PROJECT,
                    "--no-watcher",
                ])
                .current_dir(&cwd)
                .env("AI_MEMORY_DATA_DIR", data_dir.path())
                .env("AI_MEMORY_HOME", home.path())
                .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
                .env("RUST_LOG", "off")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn serve"),
        );

        let client = reqwest::Client::new();

        // Wait for the server to accept connections.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if client
                .get(format!("{base}/mcp"))
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .is_ok()
            {
                break;
            }
            assert!(Instant::now() < deadline, "server never became reachable");
            tokio::time::sleep(Duration::from_millis(150)).await;
        }

        // Phase 1 — the store is empty before backfill.
        assert_eq!(
            session_count(&client, &base, WORKSPACE, PROJECT).await,
            0,
            "a brand-new project store must start with no sessions",
        );

        // Import: run the shipped subcommand exactly as the operator would.
        let report: Value = serde_json::from_str(&run_cli(
            &[
                "backfill",
                "--workspace",
                WORKSPACE,
                "--project",
                PROJECT,
                "--json",
            ],
            data_dir.path(),
            home.path(),
            Some(&cwd),
            &base,
        ))
        .expect("backfill --json report");
        assert_eq!(
            report["imported_sessions"], 1,
            "backfill must import the one planted session: {report}"
        );
        assert!(
            report["imported_events"].as_u64().unwrap_or(0) >= 2,
            "the user prompt and assistant reply must both import: {report}"
        );
        assert_eq!(
            report["skipped_non_empty"], false,
            "the empty store must not trip the non-empty gate: {report}"
        );

        // Phase 1 — the session count went 0 → 1. `/hook/batch` commits before
        // it acks, but poll briefly to stay robust against any lag.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if session_count(&client, &base, WORKSPACE, PROJECT).await >= 1 {
                break;
            }
            assert!(Instant::now() < deadline, "imported session never appeared");
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        assert_eq!(
            session_count(&client, &base, WORKSPACE, PROJECT).await,
            1,
            "exactly one session must exist after import",
        );

        // Phase 1 — the transcript-only content is retrievable via memory_query.
        let hits = call_tool(
            &client,
            &base,
            "memory_query",
            json!({ "query": UNIQUE, "limit": 10, "workspace": WORKSPACE, "project": PROJECT }),
        )
        .await;
        assert!(
            hits.contains(UNIQUE),
            "memory_query must surface the imported transcript content: {hits}"
        );

        // Phase 3 — the canary never reaches persisted state, nor the query.
        assert!(
            !hits.contains(CANARY_MARKER),
            "the canary secret leaked into memory_query output: {hits}"
        );
        if let Some(path) = tree_contains(data_dir.path(), CANARY_MARKER) {
            let found = tree_contains(data_dir.path(), UNIQUE).is_some();
            panic!(
                "canary secret survived sanitization in {}; (unique phrase present in tree: {found})",
                path.display()
            );
        }
        // Guard against a false pass: prove the non-secret content DID persist,
        // so the canary-absence above reflects redaction, not an empty store.
        assert!(
            tree_contains(data_dir.path(), UNIQUE).is_some(),
            "the non-secret unique phrase must persist in the store",
        );

        // Phase 2 — a second backfill is a no-op (emptiness gate), no dup import.
        let report2: Value = serde_json::from_str(&run_cli(
            &[
                "backfill",
                "--workspace",
                WORKSPACE,
                "--project",
                PROJECT,
                "--json",
            ],
            data_dir.path(),
            home.path(),
            Some(&cwd),
            &base,
        ))
        .expect("second backfill --json report");
        assert_eq!(
            report2["skipped_non_empty"], true,
            "a populated store must trip the non-empty gate: {report2}"
        );
        assert_eq!(
            report2["imported_sessions"], 0,
            "the second run must import nothing: {report2}"
        );
        assert_eq!(
            session_count(&client, &base, WORKSPACE, PROJECT).await,
            1,
            "the re-run must not duplicate the imported session",
        );

        drop(server);
    }
}
