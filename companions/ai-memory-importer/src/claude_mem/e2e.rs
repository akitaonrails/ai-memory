//! Opt-in real-server coverage for the claude-mem SQLite route. Set
//! `AI_MEMORY_IMPORTER_E2E_BIN` to a freshly built `ai-memory` binary.

use std::fs;

use rusqlite::Connection;
use serde_json::{Value, json};

use super::{ClaudeMemArgs, DEFAULT_DEST_PREFIX, run};
use crate::agentmemory::e2e::{start, tool};

#[tokio::test]
async fn e2e_claude_mem_batch_is_searchable_private_and_idempotent() {
    let Some(server) = start().await else { return };
    tool(
        &server,
        "memory_write_page",
        json!({
            "workspace": "e2e",
            "project": "claude-mem",
            "path": "seed.md",
            "body": "# Import destination",
            "kind": "fact",
            "tier": "semantic",
            "create_only": true
        }),
    )
    .await;

    let files = tempfile::tempdir().unwrap();
    let database = files.path().join("claude-mem.db");
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE schema_versions(id INTEGER PRIMARY KEY, version INTEGER UNIQUE NOT NULL, applied_at TEXT NOT NULL);\
             INSERT INTO schema_versions(version,applied_at) VALUES(53,'now');\
             CREATE TABLE sdk_sessions(memory_session_id TEXT UNIQUE, project TEXT NOT NULL, cwd TEXT);\
             INSERT INTO sdk_sessions VALUES('memory-1','api','/checkout/api');\
             CREATE TABLE observations(id INTEGER PRIMARY KEY, memory_session_id TEXT, project TEXT NOT NULL, text TEXT, type TEXT NOT NULL, title TEXT, subtitle TEXT, facts TEXT, narrative TEXT, files_read TEXT, files_modified TEXT, created_at TEXT NOT NULL);\
             INSERT INTO observations VALUES(1,'memory-1','api',NULL,'decision','Use the bounded writer',NULL,'[]','Batch writes preserve atomicity','[]','[]','2026-10-31');\
             INSERT INTO observations VALUES(2,'memory-1','api','E2E-SENSITIVE-SENTINEL-1840','sensitive','Do not import',NULL,NULL,NULL,NULL,NULL,'2026-10-31');\
             CREATE TABLE session_summaries(id INTEGER PRIMARY KEY, memory_session_id TEXT, project TEXT NOT NULL, request TEXT, learned TEXT, created_at TEXT NOT NULL);\
             INSERT INTO session_summaries VALUES(7,'memory-1','api','Import safely','Only supported durable records cross the boundary','2026-10-31');",
        )
        .unwrap();
    drop(connection);
    let mapping = files.path().join("mapping.json");
    fs::write(
        &mapping,
        r#"{"projects":{"api":{"workspace":"e2e","project":"claude-mem"}}}"#,
    )
    .unwrap();
    let manifest = files.path().join("manifest.json");
    let args = || ClaudeMemArgs {
        database: Some(database.clone()),
        mapping: mapping.clone(),
        dest_prefix: DEFAULT_DEST_PREFIX.into(),
        server_url: server.url.clone(),
        apply: true,
        manifest_out: Some(manifest.clone()),
        show_body: false,
    };

    run(args()).await.unwrap();
    let first: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(first["written_versions"], 2, "{first}");
    let hits = tool(
        &server,
        "memory_query",
        json!({
            "workspace": "e2e",
            "project": "claude-mem",
            "query": "Batch writes preserve atomicity"
        }),
    )
    .await;
    assert!(
        hits.to_string()
            .contains("imported/claude-mem/decision/1.md"),
        "{hits}"
    );
    let sensitive = tool(
        &server,
        "memory_query",
        json!({
            "workspace": "e2e",
            "project": "claude-mem",
            "query": "E2E-SENSITIVE-SENTINEL-1840"
        }),
    )
    .await;
    assert!(
        !sensitive
            .to_string()
            .contains("E2E-SENSITIVE-SENTINEL-1840"),
        "{sensitive}"
    );

    run(args()).await.unwrap();
    let second: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(second["written_versions"], 0, "{second}");
    assert_eq!(second["present_versions"], 2, "{second}");
    assert_eq!(second["dropped"]["sensitive_observations"], 1);
    assert!(!second.to_string().contains("E2E-SENSITIVE-SENTINEL-1840"));
}
