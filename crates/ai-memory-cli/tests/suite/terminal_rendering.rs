//! Exercise terminal output through the shipped CLI and loopback HTTP fixtures.

use std::path::Path;
use std::process::Output;
use std::time::Duration;

use axum::http::StatusCode;
use axum::routing::any;
use axum::{Json, Router};
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");
const TEXT: &str = "café\u{1b}[31mforged\r";
const SAFE: &str = "café?[31mforged?";
const ID: &str = "11111111-2222-4333-8444-555555555555";

struct Fixture {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn start(routes: impl FnOnce(&str) -> Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = routes(&url);
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { url, task }
    }

    async fn json(path: &str, body: Value) -> Self {
        Self::start(|_| Router::new().route(path, any(move || async { Json(body) }))).await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn command(root: &Path, url: &str, args: &[&str]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(BIN);
    cmd.env_clear();
    for key in ["PATH", "SYSTEMROOT", "WINDIR", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    cmd.env("HOME", root)
        .env("USERPROFILE", root)
        .env("AI_MEMORY_HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("share"))
        .env("AI_MEMORY_DATA_DIR", root.join("data"))
        .env("AI_MEMORY_SERVER_URL", url)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .current_dir(root)
        .args(args)
        .kill_on_drop(true);
    cmd
}

async fn cli(root: &Path, url: &str, args: &[&str]) -> Output {
    let mut cmd = command(root, url, args);
    tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .expect("CLI exceeded the fixture deadline")
        .unwrap()
}

fn assert_human(output: &Output, success: bool, replacements: usize) {
    assert_eq!(output.status.success(), success, "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for text in [&stdout, &stderr] {
        assert!(
            !text.contains(['\u{1b}', '\r']),
            "server control characters reached the terminal: {text:?}"
        );
    }
    assert!(
        stdout.matches(SAFE).count() + stderr.matches(SAFE).count() >= replacements,
        "the server fields must remain visible after rendering: {output:?}"
    );
}

#[rstest::rstest]
#[case::api_key_list(
    &["api-key", "list"], "/admin/api-credentials",
    json!({"credentials": [{"id": TEXT, "user_id": "user", "label": TEXT,
        "preview": TEXT, "created_at": 0}]}), 3)]
#[case::api_key_add(
    &["api-key", "add", "--username", "user", "--label", "label"],
    "/admin/api-credentials", json!({"credential": {"id": TEXT, "user_id": "user",
        "label": TEXT, "created_at": 0}, "token": TEXT}), 3)]
#[case::api_key_rotate(
    &["api-key", "rotate", "key", "--yes"], "/admin/api-credentials/key/rotate",
    json!({"credential": {"id": TEXT, "user_id": "user", "label": TEXT,
        "created_at": 0}, "token": TEXT}), 3)]
#[case::user_grants(
    &["user", "grants"], "/admin/projects/grants",
    json!({"grants": [{"username": TEXT, "workspace": TEXT, "project": TEXT,
        "level": TEXT}]}), 4)]
#[case::project_grants(
    &["project", "grants", "--workspace", "ws", "--project", "proj"],
    "/admin/projects/grants", json!({"grants": [{"username": TEXT,
        "workspace": TEXT, "project": TEXT, "level": TEXT}]}), 4)]
#[case::user_list(
    &["user", "list"], "/admin/users", json!({"users": [{"id": "user",
        "username": TEXT, "name": TEXT, "role": TEXT, "created_at": 0}]}), 3)]
#[case::user_add(
    &["user", "add-human", "--username", "user"], "/admin/human-users",
    json!({"user": {"id": "user", "username": TEXT, "name": TEXT, "email": TEXT,
        "role": TEXT, "created_at": 0}, "temporary_password": TEXT}), 5)]
#[case::user_reset_password(
    &["user", "reset-password", "user", "--yes"], "/admin/users/user/reset-password",
    json!({"user": {"id": "user", "username": TEXT, "name": TEXT, "email": TEXT,
        "role": TEXT, "created_at": 0}, "temporary_password": TEXT}), 5)]
#[case::user_patch(
    &["user", "patch", "user", "--name", "name"], "/admin/users/user",
    json!({"user": {"id": "user", "username": TEXT, "created_at": 0}}), 1)]
#[case::project_access(
    &["project", "access", "--project", "proj", "--mode", "restricted"],
    "/admin/projects/access", json!({"mode": TEXT, "previous": TEXT,
        "changed": true, "without_access": [TEXT]}), 3)]
#[case::checkpoints(
    &["checkpoints"], "/admin/checkpoints",
    json!([{"oid": "oid", "short_oid": TEXT, "time": 0, "summary": TEXT}]), 2)]
#[case::commit_created(
    &["commit", "--message", "message"], "/admin/commit",
    json!({"committed": true, "oid": TEXT}), 1)]
#[case::commit_unchanged(
    &["commit", "--message", "message"], "/admin/commit",
    json!({"committed": false, "reason": TEXT}), 1)]
#[case::delete_page(
    &["delete-page", "--workspace", "ws", "--project", "proj", "--path", "notes/a.md"],
    "/admin/delete-page", json!({"path": TEXT, "deleted": true}), 1)]
#[case::write_page(
    &["write-page", "--workspace", "ws", "--project", "proj", "--path", "notes/a.md", "--body", "body"],
    "/admin/write-page", json!({"page_id": "page", "path": TEXT, "manifest_warning": TEXT}), 2)]
#[case::restore_page(
    &["restore-page", "--workspace", "ws", "--project", "proj", "--path", "notes/a.md", "--from", "HEAD"],
    "/admin/restore-page", json!({"page_id": "page", "path": TEXT,
        "restored_from": TEXT, "pre_checkpoint": TEXT, "checkpoint": TEXT}), 4)]
#[case::handoffs(
    &["handoffs", "--workspace", "ws", "--project", "proj"], "/admin/handoffs",
    json!({"handoffs": [{"id": TEXT, "from_agent": TEXT, "to_agent": TEXT,
        "cwd": TEXT, "created_at_ms": 0}]}), 4)]
#[case::search_scoped(
    &["search", "query", "--workspace", "ws", "--project", "proj"], "/admin/search",
    json!([{"path": TEXT, "title": TEXT, "snippet": TEXT, "rank": -1.0}]), 3)]
#[case::search_global(
    &["search", "query", "--global"], "/admin/search",
    json!([{"workspace": TEXT, "project": TEXT, "path": TEXT, "title": TEXT,
        "snippet": TEXT, "rank": -1.0}]), 5)]
#[case::recent(
    &["recent", "--global"], "/admin/recent",
    json!([{"workspace": TEXT, "project": TEXT, "path": TEXT, "title": TEXT,
        "snippet": TEXT, "updated_at": TEXT}]), 6)]
#[case::list_projects(
    &["list-projects"], "/api/v1/projects", json!([{"workspace_name": TEXT,
        "project_name": TEXT, "project_id": TEXT, "page_count": 1, "last_updated": TEXT}]), 4)]
#[case::llm_test_server(
    &["llm-test", "--server"], "/admin/llm-test",
    json!({"provider": TEXT, "model": TEXT, "latency_ms": 1}), 2)]
#[case::purge_project(
    &["purge-project", "--workspace", "ws", "--project", "proj", "--confirm"],
    "/admin/purge-project", json!({"label": TEXT}), 1)]
#[case::rename_workstream_changed(
    &["rename-workstream", "--workspace", "ws", "--project", "proj", "--from", "old", "--to", "new"],
    "/workstream/rename", json!({"workstream_id": ID, "from": TEXT, "to": format!("new-{TEXT}")}), 2)]
#[case::rename_workstream_unchanged(
    &["rename-workstream", "--workspace", "ws", "--project", "proj", "--from", "old", "--to", "new"],
    "/workstream/rename", json!({"workstream_id": ID, "from": TEXT, "to": TEXT}), 1)]
#[case::workstreams(
    &["workstreams", "--workspace", "ws", "--project", "proj"], "/workstream/recent",
    json!([{"workstream_id": ID, "name": TEXT, "created_at": "2026-01-01T00:00:00Z",
        "last_active_at": "2026-01-01T00:00:00Z", "current": true}]), 1)]
#[case::pending_list(
    &["pending-writes", "list", "--workspace", "ws", "--project", "proj"],
    "/admin/pending-writes", json!([{"id": TEXT, "status": TEXT, "operation": TEXT,
        "target_path": TEXT, "kind": "rule", "title": "title", "confidence": 0.9}]), 4)]
#[case::reorg(
    &["reorg", "--dry-run"], "/admin/reorg", json!({"dry_run": true,
        "plan": [{"session_id": TEXT, "cwd": TEXT, "new_project": TEXT}],
        "summary": {"sessions_moved": 1, "observations_updated": 1,
            "pages_graveyarded": 0, "distinct_new_projects": 1}}), 3)]
#[case::audit_contamination(
    &["audit-contamination"], "/admin/audit-contamination", json!({"findings": [{
        "check": TEXT, "confidence": TEXT, "entity_kind": "session", "entity_id": TEXT,
        "landed_workspace": TEXT, "landed_project": TEXT}]}), 5)]
#[case::profile_list(
    &["profile", "list"], "/admin/profile/list", json!({
        "scope": {"workspace": TEXT, "project": TEXT}, "entries": [{"path": TEXT,
            "statement": TEXT, "applies_to": [TEXT], "enforced_by": false}]}), 5)]
#[case::merge_project(
    &["merge-project", "--workspace", "ws", "--from", "old", "--into", "new"],
    "/admin/merge-project", json!({"from": TEXT, "into": TEXT, "manifest_warning": TEXT}), 3)]
#[case::rename_project(
    &["rename-project", "--workspace", "ws", "--from", "old", "--to", "new"],
    "/admin/rename-project", json!({"from": TEXT, "manifest_warning": TEXT}), 2)]
#[tokio::test]
async fn human_server_fields_are_inert(
    #[case] args: &[&str],
    #[case] path: &str,
    #[case] body: Value,
    #[case] replacements: usize,
) {
    let root = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", root.path().join("gitconfig"))
            .arg(root.path())
            .status()
            .unwrap()
            .success()
    );
    let fixture = Fixture::json(path, body).await;
    assert_human(
        &cli(root.path(), &fixture.url, args).await,
        true,
        replacements,
    );
}

#[rstest::rstest]
#[case::unchanged(false, None)]
#[case::changed(true, Some(TEXT))]
#[case::created(true, None)]
#[tokio::test]
async fn grant_confirmation(#[case] changed: bool, #[case] previous: Option<&str>) {
    let fixture = Fixture::json(
        "/admin/users/user/grant",
        json!({
            "changed": changed, "previous": previous, "level": TEXT,
        }),
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let output = cli(
        root.path(),
        &fixture.url,
        &[
            "user",
            "grant",
            "--user",
            "user",
            "--workspace",
            "ws",
            "--project",
            "proj",
            "--level",
            "read",
        ],
    )
    .await;
    assert_human(&output, true, 1 + usize::from(previous.is_some()));
}

#[rstest::rstest]
#[case::project_raw("purge-project", false)]
#[case::project_json_error("purge-project", true)]
#[case::session_raw("purge-session", false)]
#[case::session_json_error("purge-session", true)]
#[tokio::test]
async fn purge_preview_error(#[case] command: &str, #[case] json_error: bool) {
    let body = if json_error {
        json!({"error": TEXT}).to_string()
    } else {
        TEXT.to_owned()
    };
    let path = format!("/admin/{command}");
    let fixture = Fixture::start(|_| {
        Router::new().route(&path, any(move || async { (StatusCode::CONFLICT, body) }))
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let mut args = vec![command, "--workspace", "ws", "--project", "proj"];
    if command == "purge-session" {
        args.extend(["--session-id", ID]);
    }
    assert_human(&cli(root.path(), &fixture.url, &args).await, false, 1);
}

#[tokio::test]
async fn purge_project_preview_label() {
    let fixture = Fixture::json(
        "/admin/purge-project",
        json!({"dry_run": true, "label": TEXT}),
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    assert_human(
        &cli(
            root.path(),
            &fixture.url,
            &["purge-project", "--workspace", "ws", "--project", "proj"],
        )
        .await,
        false,
        1,
    );
}

#[tokio::test]
async fn http_error_body() {
    let fixture = Fixture::start(|_| {
        Router::new().route(
            "/admin/api-credentials",
            any(|| async { (StatusCode::BAD_GATEWAY, TEXT) }),
        )
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    assert_human(
        &cli(root.path(), &fixture.url, &["api-key", "list"]).await,
        false,
        1,
    );
}

#[rstest::rstest]
#[case::page(&["read-page", "--workspace", "ws", "--project", "proj", "--path", "notes/a.md"],
    "/admin/read-page", json!({"path": TEXT, "title": TEXT,
        "body": format!("first line\n{TEXT}\nlast line\n"), "frontmatter": {}}), 3)]
#[case::pending_show(&["pending-writes", "show", "proposal", "--workspace", "ws", "--project", "proj"],
    "/admin/pending-writes/proposal", json!({"summary": {"id": "proposal", "status": TEXT,
        "operation": "create", "target_path": TEXT, "kind": "rule", "title": TEXT,
        "confidence": 0.9}, "rationale": TEXT, "body_markdown": format!("first line\n{TEXT}\nlast line\n"),
        "events": []}), 5)]
#[case::pending_diff(&["pending-writes", "diff", "proposal", "--workspace", "ws", "--project", "proj"],
    "/admin/pending-writes/proposal/diff", json!({"diff": format!("first line\n{TEXT}\nlast line\n")}), 1)]
#[case::workstream_search(&["workstream-search", "--workstream-id", ID],
    "/workstream/11111111-2222-4333-8444-555555555555/events", json!([{"sequence": 1,
        "event_id": "event", "agent": "codex", "native_session_id": "native", "kind": "message",
        "role": TEXT, "content": format!("first line\n{TEXT}\nlast line\n")}]), 2)]
#[tokio::test]
async fn multiline_content(
    #[case] args: &[&str],
    #[case] path: &str,
    #[case] body: Value,
    #[case] replacements: usize,
) {
    let fixture = Fixture::json(path, body).await;
    let root = tempfile::tempdir().unwrap();
    let output = cli(root.path(), &fixture.url, args).await;
    assert_human(&output, true, replacements);
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("first line\ncafé?[31mforged?\nlast line\n"),
        "{output:?}"
    );
}

#[tokio::test]
async fn json_keeps_server_text() {
    let body = json!({"path": TEXT, "workspace": "ws", "project": "proj", "title": TEXT,
        "body": "first\n\u{1b}[31m\r\u{202e}\u{2066}\u{2028}\u{2029}\nlast", "frontmatter": {}});
    let fixture = Fixture::json("/admin/read-page", body.clone()).await;
    let root = tempfile::tempdir().unwrap();
    let output = cli(
        root.path(),
        &fixture.url,
        &[
            "read-page",
            "--workspace",
            "ws",
            "--project",
            "proj",
            "--path",
            "notes/a.md",
            "--json",
        ],
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        body
    );
}

fn telemetry_report() -> Value {
    json!({"workspace": "ws", "project": "proj", "generated_at": "2026-01-01T00:00:00Z",
        "since_created_at": 0, "summary": TEXT, "params": {"since_days": 30, "top_limit": 10},
        "aggregate": {"run_count": 0, "runs_with_learning_proposals": 0,
            "proposals_by_status": [], "proposals_by_operation": [], "proposals_by_edit_mode": [],
            "proposals_by_kind": [], "maintenance_proposals_by_kind": [],
            "top_targets": [{"key": TEXT, "count": 1}], "rejections_by_reason": [],
            "repeated_rejection_fingerprints": [], "rejected_targets": []},
        "terminal_rates": {"denominator": 0, "approved_rate": 0.0, "rejected_rate": 0.0,
            "conflict_rate": 0.0, "failed_rate": 0.0},
        "findings": [{"kind": TEXT, "severity": TEXT, "message": TEXT}], "blind_spots": []})
}

fn curator_report() -> Value {
    json!({"workspace": "ws", "project": "proj", "generated_at": "2026-01-01T00:00:00Z",
        "dry_run": true, "summary": TEXT, "params": ai_memory_consolidate::CuratorParams::default(),
        "findings": [{"kind": TEXT, "severity": TEXT, "message": TEXT, "pages": []}]})
}

#[rstest::rstest]
#[case::auto_improve(
    &["auto-improve", "--workspace", "ws", "--project", "proj", "--session-id", "session"],
    "/admin/auto-improve", json!({"run_id": TEXT, "approval_required": true,
        "approval_policy": TEXT, "session_id": "session", "summary": "summary", "warnings": [],
        "rejected_candidates_count": 0, "proposals": [{"id": TEXT, "sidecar_path": TEXT,
            "status": TEXT, "page_id": TEXT}, {"id": TEXT, "sidecar_path": TEXT,
            "status": TEXT, "page_id": null}], "skipped": [{"target_path": TEXT, "reason": TEXT}]}), 11)]
#[case::telemetry(
    &["auto-improve-report", "--workspace", "ws", "--project", "proj"],
    "/admin/auto-improve/report", telemetry_report(), 5)]
#[case::telemetry_staged(
    &["auto-improve-report", "--workspace", "ws", "--project", "proj", "--stage"],
    "/admin/auto-improve/report", json!({"run_id": TEXT, "proposal_ids": [TEXT],
        "sidecar_paths": [TEXT], "report": telemetry_report(),
        "skipped": [{"target_path": TEXT, "reason": TEXT}]}), 6)]
#[case::curator(
    &["curator", "--workspace", "ws", "--project", "proj"],
    "/admin/curator", curator_report(), 4)]
#[case::curator_staged(
    &["curator", "--workspace", "ws", "--project", "proj", "--stage"],
    "/admin/curator", json!({"run_id": TEXT, "proposal_ids": [TEXT], "sidecar_paths": [TEXT],
        "report": curator_report(), "skipped": [{"target_path": TEXT, "reason": TEXT}]}), 5)]
#[case::move_project(
    &["move-project", "--from-workspace", "ws", "--project", "proj", "--to-workspace", "other", "--confirm"],
    "/admin/move-project", json!({"project": TEXT, "pages_copied": 1,
        "from": format!("ws/{TEXT}"), "manifest_warning": TEXT, "pages_skipped": [TEXT],
        "conflicts": [{"path": TEXT, "moved_to": TEXT}]}), 5)]
#[case::move_session(
    &["move-session", ID, "--to", "proj"], "/admin/move-session",
    json!({"session_id": TEXT, "from": {"workspace": TEXT, "project": TEXT},
        "to": {"workspace": TEXT, "project": TEXT}, "page": TEXT,
        "cwd_warning": TEXT, "manifest_warning": TEXT}), 8)]
#[case::profile_review(
    &["profile", "review"], "/admin/profile/review", json!({"share": TEXT,
        "entries": [{"path": TEXT, "statement": TEXT, "last_seen": TEXT, "managed": true,
            "evidence": [{"quote": TEXT, "project": TEXT}]}],
        "waiting": [{"statement": TEXT, "projects": 1, "needs": 2}],
        "pending_updates": [TEXT], "hand_edited": [TEXT], "forgotten": [TEXT]}), 10)]
#[case::profile_rebuild(
    &["profile", "rebuild"], "/admin/profile/rebuild", json!({"share": TEXT,
        "projects_harvested": 0, "candidates_added": 0, "entries_written": 0,
        "entries_unchanged": 0, "skipped_manual": [TEXT], "llm_calls": 0,
        "llm_fallbacks": 0, "errors": [TEXT]}), 3)]
#[tokio::test]
async fn report_server_fields_are_inert(
    #[case] args: &[&str],
    #[case] path: &str,
    #[case] body: Value,
    #[case] replacements: usize,
) {
    let fixture = Fixture::json(path, body).await;
    let root = tempfile::tempdir().unwrap();
    assert_human(
        &cli(root.path(), &fixture.url, args).await,
        true,
        replacements,
    );
}

#[tokio::test]
async fn bootstrap_stdout_and_warning() {
    let fixture = Fixture::json(
        "/admin/bootstrap",
        json!({"sources_collected": 1,
        "sources_sent": 1, "sources_dropped": 0, "sources_by_kind": {"git_commits": 0,
            "readme": 1, "doc_files": 0, "module_headers": 0, "project_rules": 0},
        "estimated_input_tokens": 10, "pages_written": [TEXT], "rationale": TEXT,
        "dry_run": false, "llm_chunks": 1, "manifest_warning": TEXT}),
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("README.md"), "Fixture project\n").unwrap();
    assert_human(
        &cli(
            root.path(),
            &fixture.url,
            &[
                "bootstrap",
                "--workspace",
                "ws",
                "--project",
                "proj",
                "--exclude-git",
            ],
        )
        .await,
        true,
        3,
    );
}

#[tokio::test]
async fn doctor_identity() {
    let fixture = Fixture::start(|_| {
        Router::new()
            .route(
                "/admin/sessions/by-agent",
                any(|| async { Json(json!({"by_agent": [{"agent": TEXT, "sessions": 1}]})) }),
            )
            .route(
                "/identity",
                any(|| async {
                    Json(json!({"version": TEXT, "level": TEXT,
            "operator": TEXT, "distinguishes_operators": true}))
                }),
            )
            .route(
                "/admin/project-coordinate",
                any(|| async {
                    Json(json!({"workspace": "ws",
            "requested_name": TEXT, "status": "exact", "current_name": TEXT,
            "rename_eligible": false, "candidate_count": 1}))
                }),
            )
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    assert_human(
        &cli(
            root.path(),
            &fixture.url,
            &["doctor", "--workspace", "ws", "--project", "proj"],
        )
        .await,
        true,
        6,
    );
}

#[tokio::test]
async fn status_server_fields() {
    let fixture = Fixture::json("/admin/status", json!({"version": TEXT, "data_dir": TEXT,
        "db_path": TEXT, "bind": TEXT, "counts": {"pages_latest": 0, "pages_all": 0,
            "sessions": 0, "observations": 0},
        "wiki_format": {"backup_archive": TEXT, "backup_archive_bytes": 10},
        "consolidation": {"failed": 1, "failed_sessions": [{"session_id": TEXT, "last_error": TEXT}]},
        "derived": {"pages_rows": 0, "pages_fts_rows": 0, "observations_rows": 0,
            "observations_fts_rows": 0, "latest_pages_missing_embeddings": 0, "embedding_rows": 0,
            "embedding_triples": [{"provider": TEXT, "model": TEXT, "dim": 1, "count": 1}],
            "typed_links_from_latest_pages": [[TEXT, 1]], "links_from_latest_pages": 0,
            "unresolved_links_from_latest_pages": 0, "stale_links_from_latest_pages": 0},
        "providers": {"llm": {"status": "error", "provider": TEXT, "model": TEXT,
            "last_error_message": TEXT, "retry_hint": TEXT}, "embedding": {"status": "disabled"},
            "llm_candidates": [{"provider": TEXT, "model": TEXT, "last_selected": false,
                "last_error_class": TEXT}], "llm_task_chains": [{"task": "rerank", "provider": TEXT,
                "model": TEXT, "status": "error", "candidates": [{"provider": TEXT, "model": TEXT,
                    "last_selected": false, "last_error_class": TEXT}]}]}})).await;
    let root = tempfile::tempdir().unwrap();
    assert_human(&cli(root.path(), &fixture.url, &["status"]).await, true, 20);
}

#[rstest::rstest]
#[case::success(false)]
#[case::error(true)]
#[tokio::test]
async fn finalize_session(#[case] error: bool) {
    let fixture = Fixture::start(|_| {
        Router::new()
            .route(
                "/admin/open-sessions",
                any(|| async {
                    Json(json!({"sessions": [{
            "session_id": TEXT, "cwd": null}]}))
                }),
            )
            .route(
                "/hook/batch",
                any(move || async move {
                    if error {
                        (StatusCode::BAD_GATEWAY, TEXT.to_owned())
                    } else {
                        (StatusCode::OK, json!({"accepted": 1}).to_string())
                    }
                }),
            )
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    assert_human(
        &cli(
            root.path(),
            &fixture.url,
            &[
                "finalize-session",
                "--workspace",
                "ws",
                "--project",
                "proj",
                "--all",
            ],
        )
        .await,
        !error,
        1,
    );
}

#[rstest::rstest]
#[case::complete(true)]
#[case::separate_code(false)]
#[tokio::test]
async fn oidc_device_display(#[case] complete: bool) {
    let fixture = Fixture::start(|url| {
        let discovery = json!({"device_authorization_endpoint": format!("{url}/device"),
            "token_endpoint": format!("{url}/token")});
        let mut device = json!({"device_code": "synthetic-code", "user_code": TEXT,
            "verification_uri": TEXT});
        if complete {
            device["verification_uri_complete"] = json!(TEXT);
        }
        Router::new()
            .route(
                "/.well-known/openid-configuration",
                any(move || async { Json(discovery) }),
            )
            .route("/device", any(move || async { Json(device) }))
            .route(
                "/token",
                any(|| async {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": "access_denied"})),
                    )
                }),
            )
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    assert_human(
        &cli(
            root.path(),
            &fixture.url,
            &[
                "auth",
                "login",
                "oidc-device",
                "--issuer",
                &fixture.url,
                "--client-id",
                "synthetic-client",
            ],
        )
        .await,
        false,
        if complete { 1 } else { 2 },
    );
}

#[tokio::test]
async fn unicode_terminal_controls() {
    let text = "café\u{202a}\u{202b}\u{202c}\u{202d}\u{202e}\u{2066}\u{2067}\u{2068}\u{2069}\u{2028}\u{2029}end";
    let fixture = Fixture::json(
        "/admin/read-page",
        json!({"path": "notes/a.md",
        "title": text, "body": "first\nlast\n", "frontmatter": {}}),
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let output = cli(
        root.path(),
        &fixture.url,
        &[
            "read-page",
            "--workspace",
            "ws",
            "--project",
            "proj",
            "--path",
            "notes/a.md",
        ],
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("café???????????end\n"), "{stdout:?}");
    assert!(stdout.contains("first\nlast\n"), "{stdout:?}");
}

fn message() -> Value {
    json!({"id": TEXT, "to_workspace_id": TEXT, "to_project_id": TEXT,
        "from_workspace_id": TEXT, "from_project_id": TEXT, "from_agent": TEXT,
        "from_owner_user": TEXT, "subject": TEXT, "body": TEXT, "state": "pending",
        "created_at": TEXT})
}

#[rstest::rstest]
#[case::inbox(false)]
#[case::outbox(true)]
#[tokio::test]
async fn message_list(#[case] outbox: bool) {
    let fixture = Fixture::json("/admin/messages", json!({"messages": [message()]})).await;
    let root = tempfile::tempdir().unwrap();
    let mut args = vec!["message", "list", "--workspace", "ws", "--project", "proj"];
    if outbox {
        args.push("--outbox");
    }
    assert_human(&cli(root.path(), &fixture.url, &args).await, true, 4);
}

#[tokio::test]
async fn message_send() {
    let fixture = Fixture::json("/admin/messages/send", json!({"message_id": TEXT})).await;
    let root = tempfile::tempdir().unwrap();
    assert_human(
        &cli(
            root.path(),
            &fixture.url,
            &[
                "message",
                "send",
                "--from-workspace",
                "ws",
                "--from-project",
                "proj",
                "--to-workspace",
                "other",
                "--to-project",
                "other",
                "body",
            ],
        )
        .await,
        true,
        1,
    );
}

#[rstest::rstest]
#[case::existing_controls(false)]
#[case::unicode_controls(true)]
#[tokio::test]
async fn message_pop_body(#[case] unicode: bool) {
    let mut message = message();
    if unicode {
        message["body"] = json!("first\n\t\u{202e}\u{2066}\u{2028}\u{2029}last\n");
    }
    let fixture = Fixture::json("/admin/messages/pop", json!({"message": message})).await;
    let root = tempfile::tempdir().unwrap();
    let output = cli(
        root.path(),
        &fixture.url,
        &["message", "pop", "--workspace", "ws", "--project", "proj"],
    )
    .await;
    assert_human(&output, true, if unicode { 7 } else { 8 });
    if unicode {
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("first\n\t????last\n"),
            "{output:?}"
        );
    }
}

fn profile_status() -> Value {
    json!({"enabled": TEXT, "share": TEXT, "effective_share": TEXT,
        "distinguishes_operators": false, "min_projects": 1, "inject_on_session_start": true,
        "digest_max_bytes": 100, "baseline_max_bytes": 100, "apply_max_lines": 10,
        "llm": false, "scope": {"workspace": TEXT, "project": TEXT}, "entries": 1,
        "digest_bytes": 10, "contribute_opt_outs": [{"workspace": TEXT, "project": TEXT}]})
}

#[tokio::test]
async fn message_pop_notice() {
    let fixture = Fixture::json(
        "/admin/messages/pop",
        json!({"message": message(), "security_notice": TEXT}),
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    assert_human(
        &cli(
            root.path(),
            &fixture.url,
            &["message", "pop", "--workspace", "ws", "--project", "proj"],
        )
        .await,
        true,
        9,
    );
}

#[tokio::test]
async fn profile_status_fields() {
    let fixture = Fixture::json("/admin/profile/status", profile_status()).await;
    let root = tempfile::tempdir().unwrap();
    assert_human(
        &cli(root.path(), &fixture.url, &["profile", "status"]).await,
        true,
        7,
    );
}

#[rstest::rstest]
#[case::show("show")]
#[case::forget("forget")]
#[tokio::test]
async fn profile_page(#[case] command: &str) {
    let fixture = Fixture::start(|_| {
        Router::new()
            .route(
                "/admin/profile/status",
                any(|| async { Json(profile_status()) }),
            )
            .route(
                "/admin/read-page",
                any(|| async { Json(json!({"path": TEXT, "body": TEXT})) }),
            )
            .route(
                "/admin/delete-page",
                any(|| async { Json(json!({"path": TEXT, "deleted": true})) }),
            )
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    assert_human(
        &cli(
            root.path(),
            &fixture.url,
            &["profile", command, "notes/a.md"],
        )
        .await,
        true,
        if command == "show" { 4 } else { 1 },
    );
}

#[tokio::test]
async fn profile_apply_preview() {
    let fixture = Fixture::json(
        "/admin/profile/apply",
        json!({"consume": true,
        "lines": [TEXT, "legitimate second line"], "omitted": 0}),
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    assert_human(
        &cli(
            root.path(),
            &fixture.url,
            &["profile", "apply", "--dry-run"],
        )
        .await,
        true,
        1,
    );
}

#[rstest::rstest]
#[case::discovery(true)]
#[case::poll(false)]
#[tokio::test]
async fn oidc_error_body(#[case] discovery_error: bool) {
    let fixture = Fixture::start(|url| {
        let discovery = json!({"device_authorization_endpoint": format!("{url}/device"),
            "token_endpoint": format!("{url}/token")});
        Router::new()
            .route(
                "/.well-known/openid-configuration",
                any(move || async move {
                    if discovery_error {
                        (StatusCode::BAD_GATEWAY, TEXT.to_owned())
                    } else {
                        (StatusCode::OK, discovery.to_string())
                    }
                }),
            )
            .route(
                "/device",
                any(|| async {
                    Json(json!({"device_code": "synthetic-code",
                "user_code": "TEST", "verification_uri": "https://example.test/verify"}))
                }),
            )
            .route(
                "/token",
                any(|| async { (StatusCode::BAD_REQUEST, Json(json!({"error": TEXT}))) }),
            )
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let output = cli(
        root.path(),
        &fixture.url,
        &[
            "auth",
            "login",
            "oidc-device",
            "--issuer",
            &fixture.url,
            "--client-id",
            "synthetic-client",
        ],
    )
    .await;
    assert_human(&output, false, if discovery_error { 0 } else { 1 });
    if discovery_error {
        // The provider error scrubber already removes ANSI colors; CR still needs rendering protection.
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("caféforged?"),
            "{output:?}"
        );
    }
}

#[rstest::rstest]
#[case::reply(false)]
#[case::error(true)]
#[tokio::test]
async fn provider_response(#[case] error: bool) {
    let fixture = Fixture::start(|_| {
        Router::new().route(
            "/v1/chat/completions",
            any(move || async move {
                if error {
                    (StatusCode::BAD_REQUEST, TEXT.to_owned())
                } else {
                    (
                        StatusCode::OK,
                        json!({"model": TEXT, "choices": [{"message": {"role": "assistant",
            "content": format!("first line\n{TEXT}\nlast line\n")}}]})
                        .to_string(),
                    )
                }
            }),
        )
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let output = cli(
        root.path(),
        &fixture.url,
        &[
            "llm-test",
            "--provider",
            "openai-compat",
            "--model",
            "fixture-model",
            "--prompt",
            "test",
            "--base-url",
            &fixture.url,
            "--api-key",
            "synthetic-key",
        ],
    )
    .await;
    assert_human(&output, !error, if error { 0 } else { 2 });
    if error {
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("caféforged?"),
            "{output:?}"
        );
    }
}

#[tokio::test]
async fn handoff_refusal_stderr() {
    let fixture = Fixture::start(|_| {
        Router::new()
            .route(
                "/hook/batch",
                any(|| async { Json(json!({"accepted": 1})) }),
            )
            .route(
                "/handoff",
                any(|| async { (StatusCode::FORBIDDEN, format!("{TEXT}tail")) }),
            )
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("input.json");
    std::fs::write(
        &input,
        json!({"session_id": ID, "cwd": root.path()}).to_string(),
    )
    .unwrap();
    let mut cmd = command(
        root.path(),
        &fixture.url,
        &["hook", "--agent", "codex", "--event", "session-start"],
    );
    cmd.stdin(std::fs::File::open(input).unwrap());
    let output = tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .unwrap()
        .unwrap();
    assert_human(&output, true, 1);
}

#[cfg(unix)]
#[tokio::test]
async fn repair_backfill_timestamps() {
    let fixture = Fixture::json(
        "/admin/repair-session-times",
        json!({"dry_run": true,
        "repaired": [{"session_id": TEXT, "old_started_at_us": 0, "new_started_at_us": 1}],
        "skipped": [{"session_id": "session", "reason": TEXT}]}),
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let sessions = root
        .path()
        .join(".claude/projects")
        .join(cwd.to_string_lossy().replace('/', "-"));
    std::fs::create_dir_all(&sessions).unwrap();
    let header = json!({"sessionId": ID, "cwd": cwd});
    let event = json!({"type": "user", "message": {"role": "user", "content": "hello"},
        "timestamp": "2020-01-01T00:00:00Z"});
    std::fs::write(
        sessions.join("session.jsonl"),
        format!("{header}\n{event}\n"),
    )
    .unwrap();
    assert_human(
        &cli(
            root.path(),
            &fixture.url,
            &[
                "repair-backfill-timestamps",
                "--workspace",
                "ws",
                "--project",
                "proj",
            ],
        )
        .await,
        true,
        2,
    );
}

#[rstest::rstest]
#[case::session("session_wrong_bucket", 6)]
#[case::collision("project_name_collision", 6)]
#[tokio::test]
async fn audit_known_findings(#[case] check: &str, #[case] replacements: usize) {
    let fixture = Fixture::json(
        "/admin/audit-contamination",
        json!({"findings": [{
        "check": check, "confidence": TEXT, "entity_kind": "session", "entity_id": TEXT,
        "landed_workspace": TEXT, "landed_project": TEXT, "expected_project": TEXT,
        "cwd": TEXT, "collision_kind": TEXT, "peer_projects": TEXT}]}),
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    assert_human(
        &cli(root.path(), &fixture.url, &["audit-contamination"]).await,
        true,
        replacements,
    );
}

#[tokio::test]
async fn move_session_batch() {
    let fixture = Fixture::json(
        "/admin/move-session",
        json!({"from": {"workspace": TEXT, "project": TEXT},
        "to": {"workspace": TEXT, "project": TEXT}, "total": 1, "moved": 1,
        "sessions": [{"session_id": TEXT, "page": TEXT, "cwd": TEXT}],
        "pre_checkpoint": TEXT, "checkpoint": TEXT}),
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    assert_human(
        &cli(
            root.path(),
            &fixture.url,
            &[
                "move-session",
                "--from-workspace",
                "ws",
                "--from-project",
                "proj",
                "--to",
                "other",
            ],
        )
        .await,
        true,
        9,
    );
}
