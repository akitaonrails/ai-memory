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
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};

    use crate::e2e_support::{
        ServerGuard, hermetic, run_cli, session_count, start_serve, write_jsonl,
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

    fn status_json(data_dir: &Path, home: &Path, cwd: &Path, base: &str) -> Value {
        serde_json::from_str(&run_cli(
            &["status", "--json"],
            data_dir,
            home,
            Some(cwd),
            base,
        ))
        .expect("status JSON remains compatible")
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

    /// Start the real server (loopback, no auth, hermetic: no embedder, no
    /// wiki watcher — a machine-global inotify instance concurrent server
    /// children can exhaust, #745).
    async fn start_backfill_server(
        client: &reqwest::Client,
        data_dir: &Path,
        home: &Path,
        cwd: &Path,
    ) -> (ServerGuard, String) {
        start_serve(client, &data_dir.join("serve.log"), |port| {
            let mut cmd = hermetic(BIN);
            cmd.args([
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
            .env("AI_MEMORY_DATA_DIR", data_dir)
            .env("AI_MEMORY_HOME", home)
            .env("AI_MEMORY_EMBEDDING_PROVIDER", "none");
            cmd.current_dir(cwd);
            cmd
        })
        .await
    }

    fn backfill_report(data_dir: &Path, home: &Path, cwd: &Path, base: &str) -> Value {
        serde_json::from_str(&run_cli(
            &[
                "backfill",
                "--workspace",
                WORKSPACE,
                "--project",
                PROJECT,
                "--json",
            ],
            data_dir,
            home,
            Some(cwd),
            base,
        ))
        .expect("backfill --json report")
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

        let client = reqwest::Client::new();
        let (server, base) =
            start_backfill_server(&client, data_dir.path(), home.path(), &cwd).await;

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

        let observations_before = status_json(data_dir.path(), home.path(), &cwd, &base)["counts"]
            ["observations"]
            .as_u64()
            .unwrap();
        let forced: Value = serde_json::from_str(&run_cli(
            &[
                "backfill",
                "--workspace",
                WORKSPACE,
                "--project",
                PROJECT,
                "--force",
                "--json",
            ],
            data_dir.path(),
            home.path(),
            Some(&cwd),
            &base,
        ))
        .expect("forced replay report");
        assert_eq!(forced["imported_sessions"], 1);
        assert_eq!(
            session_count(&client, &base, WORKSPACE, PROJECT).await,
            1,
            "a real second replay must reuse the session",
        );
        let status = status_json(data_dir.path(), home.path(), &cwd, &base);
        assert!(status["ingest"]["replayed"].as_u64().unwrap_or(0) >= 2);
        assert!(status["ingest"]["ignored_end"].as_u64().unwrap_or(0) >= 1);
        assert_eq!(
            status["counts"]["observations"].as_u64(),
            Some(observations_before),
            "the second replay must create no duplicate observations"
        );

        drop(server);
    }

    const AG_MINE: &str = "a0d5ac62-2501-4780-b783-76d159c56cb3";
    const AG_THEIRS: &str = "9576275f-7c4e-4709-b372-22d1ad2a0af8";
    /// Rare, non-secret tokens: a secret-shaped canary would be redacted by the
    /// sanitizer and could not show a leak from another project.
    const AG_OWN: &str = "wombatnebulaagyown";
    const AG_FOREIGN: &str = "pangolinquasaragyforeign";
    const AG_RESUMED: &str = "ocelotpulsaragyresumed";

    fn push_varint(output: &mut Vec<u8>, mut value: usize) {
        while value >= 0x80 {
            output.push(u8::try_from(value & 0x7f).unwrap() | 0x80);
            value >>= 7;
        }
        output.push(u8::try_from(value).unwrap());
    }

    fn push_bytes_field(output: &mut Vec<u8>, field: u8, value: &[u8]) {
        output.push((field << 3) | 2);
        push_varint(output, value.len());
        output.extend_from_slice(value);
    }

    /// Plant an `agy` conversation store whose metadata names `workspace`: the
    /// workspace `file://` URI sits in field 1 of the message in field 1.
    fn plant_antigravity_conversation(home: &Path, id: &str, workspace: &Path) {
        let root = home.join(".gemini/antigravity-cli/conversations");
        fs::create_dir_all(&root).expect("conversations dir");
        let mut nested = Vec::new();
        push_bytes_field(
            &mut nested,
            1,
            format!("file://{}", workspace.display()).as_bytes(),
        );
        let mut metadata = Vec::new();
        push_bytes_field(&mut metadata, 1, &nested);
        let connection =
            rusqlite::Connection::open(root.join(format!("{id}.db"))).expect("conversation db");
        connection
            .execute_batch(
                "CREATE TABLE trajectory_metadata_blob (id text DEFAULT \"main\", data blob, PRIMARY KEY (id))",
            )
            .expect("metadata table");
        connection
            .execute(
                "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
                [metadata],
            )
            .expect("metadata row");
    }

    fn agy_prompt(id: &str, workspace: &Path, display: &str, millis: i64) -> Value {
        json!({
            "display": display,
            "timestamp": millis,
            "workspace": workspace.to_string_lossy(),
            "conversationId": id,
        })
    }

    /// `agy` keeps one `history.jsonl` for every project. Backfill imports this
    /// conversation's prompts from it, in order, and nothing typed in another
    /// conversation or another directory reaches the store.
    #[tokio::test]
    async fn backfill_imports_antigravity_prompts_from_history_and_nothing_foreign() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let home = tempfile::tempdir().expect("home");
        let project = tempfile::tempdir().expect("project cwd");
        let elsewhere = tempfile::tempdir().expect("other project");
        let cwd = fs::canonicalize(project.path()).expect("canonicalize project cwd");
        let other = fs::canonicalize(elsewhere.path()).expect("canonicalize other project");
        plant_antigravity_conversation(home.path(), AG_MINE, &cwd);
        plant_antigravity_conversation(home.path(), AG_THEIRS, &other);
        let past = 1_780_000_000_000;
        write_jsonl(
            &home.path().join(".gemini/antigravity-cli/history.jsonl"),
            &[
                agy_prompt(AG_MINE, &cwd, &format!("first {AG_OWN} prompt"), past),
                agy_prompt(
                    AG_THEIRS,
                    &other,
                    &format!("{AG_FOREIGN} elsewhere"),
                    past + 1,
                ),
                agy_prompt(AG_THEIRS, &cwd, &format!("{AG_FOREIGN} here"), past + 2),
                agy_prompt(AG_MINE, &other, &format!("{AG_RESUMED} resumed"), past + 3),
                agy_prompt(AG_MINE, &cwd, "second prompt", past + 4),
            ],
        );
        let client = reqwest::Client::new();
        let (server, base) =
            start_backfill_server(&client, data_dir.path(), home.path(), &cwd).await;

        let report = backfill_report(data_dir.path(), home.path(), &cwd, &base);

        assert_eq!(report["imported_sessions"], 1, "{report}");
        assert_eq!(report["imported_events"], 2, "{report}");
        assert_eq!(report["failed_sessions"], 0, "{report}");
        let hits = call_tool(
            &client,
            &base,
            "memory_query",
            json!({ "query": AG_OWN, "limit": 10, "workspace": WORKSPACE, "project": PROJECT }),
        )
        .await;
        assert!(
            hits.contains(AG_OWN),
            "imported prompt not searchable: {hits}"
        );
        assert!(
            tree_contains(data_dir.path(), AG_OWN).is_some(),
            "the own prompt must persist, or the absence checks prove nothing",
        );
        for foreign in [AG_FOREIGN, AG_RESUMED] {
            if let Some(path) = tree_contains(data_dir.path(), foreign) {
                panic!("{foreign} leaked into {}", path.display());
            }
        }

        drop(server);
    }

    /// Without `history.jsonl` the session imports with no events instead of
    /// failing the whole backfill.
    #[tokio::test]
    async fn backfill_succeeds_on_antigravity_without_history() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let home = tempfile::tempdir().expect("home");
        let project = tempfile::tempdir().expect("project cwd");
        let cwd = fs::canonicalize(project.path()).expect("canonicalize project cwd");
        plant_antigravity_conversation(home.path(), AG_MINE, &cwd);
        let client = reqwest::Client::new();
        let (server, base) =
            start_backfill_server(&client, data_dir.path(), home.path(), &cwd).await;

        let report = backfill_report(data_dir.path(), home.path(), &cwd, &base);

        assert_eq!(report["imported_sessions"], 1, "{report}");
        assert_eq!(report["imported_events"], 0, "{report}");
        assert_eq!(report["failed_sessions"], 0, "{report}");

        drop(server);
    }

    /// Open a live Antigravity session the way its `PreInvocation` hook would.
    async fn open_antigravity_session(client: &reqwest::Client, base: &str, cwd: &Path) {
        let url = format!(
            "{base}/hook?event=session-start&agent=antigravity-cli&workspace={WORKSPACE}\
             &project={PROJECT}&session_id={AG_MINE}"
        );
        let status = client
            .post(url)
            .json(&json!({ "session_id": AG_MINE, "cwd": cwd.to_string_lossy() }))
            .send()
            .await
            .expect("session-start hook")
            .status();
        assert!(status.is_success(), "session-start rejected: {status}");
    }

    fn finalize_antigravity(data_dir: &Path, home: &Path, cwd: &Path, base: &str, reopen: bool) {
        let mut args = vec![
            "finalize-session",
            "--agent",
            "antigravity-cli",
            "--workspace",
            WORKSPACE,
            "--project",
            PROJECT,
        ];
        if reopen {
            args.extend(["--reopen", "--session-id", AG_MINE]);
        }
        run_cli(&args, data_dir, home, Some(cwd), base);
    }

    async fn stored_prompts(client: &reqwest::Client, base: &str) -> usize {
        let text = call_tool(
            client,
            base,
            "memory_read_session_observations",
            json!({
                "workspace": WORKSPACE,
                "project": PROJECT,
                "session_id": AG_MINE,
                "kinds": ["user-prompt"],
                "limit": 100,
            }),
        )
        .await;
        let v: Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("non-JSON observations: {text}: {e}"));
        v["observations"]
            .as_array()
            .unwrap_or_else(|| panic!("missing observations: {text}"))
            .len()
    }

    /// Antigravity's live hooks carry no prompt, so `finalize-session` replays
    /// the session's prompts from `history.jsonl` before its session-end: the
    /// summary page is titled by the first prompt, a foreign prompt never
    /// lands, a repeated `--reopen` stores nothing twice, and a prompt typed
    /// after the first finalize arrives with the next one.
    #[tokio::test]
    async fn finalize_session_replays_antigravity_prompts_once() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let home = tempfile::tempdir().expect("home");
        let project = tempfile::tempdir().expect("project cwd");
        let elsewhere = tempfile::tempdir().expect("other project");
        let cwd = fs::canonicalize(project.path()).expect("canonicalize project cwd");
        let other = fs::canonicalize(elsewhere.path()).expect("canonicalize other project");
        plant_antigravity_conversation(home.path(), AG_MINE, &cwd);
        let history = home.path().join(".gemini/antigravity-cli/history.jsonl");
        let past = 1_780_000_000_000;
        let mut lines = vec![
            agy_prompt(AG_MINE, &cwd, &format!("first {AG_OWN} prompt"), past),
            agy_prompt(
                AG_THEIRS,
                &other,
                &format!("{AG_FOREIGN} elsewhere"),
                past + 1,
            ),
            agy_prompt(AG_MINE, &cwd, "second prompt", past + 2),
        ];
        write_jsonl(&history, &lines);
        let client = reqwest::Client::new();
        let (server, base) =
            start_backfill_server(&client, data_dir.path(), home.path(), &cwd).await;
        open_antigravity_session(&client, &base, &cwd).await;

        finalize_antigravity(data_dir.path(), home.path(), &cwd, &base, false);

        assert_eq!(stored_prompts(&client, &base).await, 2);
        let page = call_tool(
            &client,
            &base,
            "memory_read_page",
            json!({
                "workspace": WORKSPACE,
                "project": PROJECT,
                "path": format!("sessions/{AG_MINE}.md"),
            }),
        )
        .await;
        assert!(
            page.contains(AG_OWN),
            "the summary page must be built from the replayed prompts: {page}"
        );
        if let Some(path) = tree_contains(data_dir.path(), AG_FOREIGN) {
            panic!("{AG_FOREIGN} leaked into {}", path.display());
        }

        finalize_antigravity(data_dir.path(), home.path(), &cwd, &base, true);
        assert_eq!(
            stored_prompts(&client, &base).await,
            2,
            "a repeated --reopen must not store the prompts twice"
        );

        lines.push(agy_prompt(AG_MINE, &cwd, "third prompt", past + 3));
        write_jsonl(&history, &lines);
        finalize_antigravity(data_dir.path(), home.path(), &cwd, &base, true);
        assert_eq!(stored_prompts(&client, &base).await, 3);

        drop(server);
    }

    /// A live Antigravity session without `history.jsonl` still finalizes.
    #[tokio::test]
    async fn finalize_session_succeeds_on_antigravity_without_history() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let home = tempfile::tempdir().expect("home");
        let project = tempfile::tempdir().expect("project cwd");
        let cwd = fs::canonicalize(project.path()).expect("canonicalize project cwd");
        plant_antigravity_conversation(home.path(), AG_MINE, &cwd);
        let client = reqwest::Client::new();
        let (server, base) =
            start_backfill_server(&client, data_dir.path(), home.path(), &cwd).await;
        open_antigravity_session(&client, &base, &cwd).await;

        finalize_antigravity(data_dir.path(), home.path(), &cwd, &base, false);

        assert_eq!(stored_prompts(&client, &base).await, 0);
        drop(server);
    }
}
