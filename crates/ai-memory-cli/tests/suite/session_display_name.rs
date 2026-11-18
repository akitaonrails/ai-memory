//! The installed Codex Stop command, durable batch body and dashboard row.

use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;

use serde_json::{Value, json};

use crate::e2e_support::{hermetic, start_serve, write_jsonl};

const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");
const SESSION: &str = "11111111-aaaa-4444-8888-0000ab12cd34";
const OTHER: &str = "22222222-bbbb-4444-8888-00005678abcd";

fn spool_stop(home: &Path, cwd: &Path, id: &str, server: &str) -> Value {
    let data = tempfile::tempdir_in(home).unwrap();
    let mut child = hermetic(BIN)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("CODEX_HOME", home.join("codex"))
        .env("AI_MEMORY_HOME", home)
        .arg("--data-dir")
        .arg(data.path())
        .args([
            "hook",
            "--agent",
            "codex",
            "--event",
            "stop",
            "--server-url",
            server,
        ])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.take().unwrap(),
        "{}",
        json!({"session_id": id, "cwd": cwd})
    )
    .unwrap();
    assert!(child.wait().unwrap().success());
    let file = std::fs::read_dir(data.path().join("hook-spool"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "json"))
        .unwrap();
    let entry: Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    let body: Value = serde_json::from_str(entry["body"].as_str().unwrap()).unwrap();
    json!({"url": entry["url"], "body": body})
}

#[tokio::test]
async fn session_display_name_survives_delayed_delivery_and_stays_in_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let codex = home.join("codex");
    let data = home.join("server");
    let project = home.join("app");
    for dir in [&codex, &data, &project] {
        std::fs::create_dir(dir).unwrap();
    }
    std::fs::write(
        project.join(".ai-memory.toml"),
        "workspace = \"team\"\nproject = \"app\"\n",
    )
    .unwrap();
    let client = reqwest::Client::new();
    let (_server, base) = start_serve(&client, &data.join("serve.log"), |port| {
        let mut cmd = hermetic(BIN);
        cmd.env("HOME", home)
            .env("USERPROFILE", home)
            .env("AI_MEMORY_HOME", home)
            .env("AI_MEMORY_DATA_DIR", &data)
            .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
            .current_dir(&project)
            .args([
                "serve",
                "--transport",
                "http",
                "--bind",
                &format!("127.0.0.1:{port}"),
                "--enable-web",
                "--no-watcher",
            ]);
        cmd
    })
    .await;
    let batch = |items: Vec<Value>| {
        let request = client.post(format!("{base}/hook/batch")).json(&items);
        async move {
            let result: Value = request
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(result["accepted"], items.len(), "{result}");
            result
        }
    };
    let page = |scope| {
        client
            .get(format!("{base}/web/w/team/{scope}/agents/codex"))
            .send()
    };
    let omitted = spool_stop(home, &project, SESSION, &base);
    batch(vec![spool_stop(home, &project, SESSION, &base)]).await;
    let html = page("app")
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains(">ab12cd34 ↗</a>"));

    let index = codex.join("session_index.jsonl");
    let first =
        json!({"id": SESSION, "thread_name": "Fix retries", "updated_at": "2025-02-01T12:30:00Z"});
    let foreign =
        json!({"id": OTHER, "thread_name": "Another thread", "updated_at": "2025-02-01T12:31:00Z"});
    write_jsonl(&index, &[first.clone(), foreign.clone()]);
    let old = spool_stop(home, &project, SESSION, &base);
    let delayed = spool_stop(home, &project, SESSION, &base);
    batch(vec![old]).await;
    let html = page("app").await.unwrap().text().await.unwrap();
    assert!(html.contains("Fix retries · ab12cd34 ↗"));
    assert!(!html.contains("Another thread"));

    let rename = json!({"id": SESSION, "thread_name": "Verify <retries>", "updated_at": "2025-02-01T12:32:00Z"});
    write_jsonl(&index, &[first, rename, foreign]);
    let new = spool_stop(home, &project, SESSION, &base);
    let unnamed = spool_stop(
        home,
        &project,
        "33333333-cccc-4444-8888-0000abcd1234",
        &base,
    );
    let result = batch(vec![new.clone(), delayed, new, omitted, unnamed]).await;
    assert_eq!(result["results"][2]["outcome"], "replayed");
    let html = page("app").await.unwrap().text().await.unwrap();
    assert!(html.contains(&format!("/s/{SESSION}\"")));
    assert!(html.contains("Verify ") && html.contains(" · ab12cd34 ↗"));
    assert!(!html.contains("<retries>") && !html.contains("Fix retries"));
    assert!(html.contains(">abcd1234 ↗</a>"));

    let sibling = home.join("sibling");
    std::fs::create_dir(&sibling).unwrap();
    std::fs::write(
        sibling.join(".ai-memory.toml"),
        "workspace = \"team\"\nproject = \"sibling\"\n",
    )
    .unwrap();
    write_jsonl(
        &index,
        &[json!({"id":SESSION, "thread_name":"Sibling name", "updated_at":"2025-02-02T12:00:00Z"})],
    );
    batch(vec![
        spool_stop(home, &sibling, OTHER, &base),
        spool_stop(home, &sibling, SESSION, &base),
    ])
    .await;
    let original = page("app").await.unwrap().text().await.unwrap();
    let sibling = page("sibling")
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(original.contains("Verify ") && !original.contains("Sibling name"));
    assert!(sibling.contains(">ab12cd34 ↗</a>") && !sibling.contains("Verify "));
}
