//! End-to-end runs of both agentmemory routes against a real `ai-memory
//! serve` in a temporary data directory. Set `AI_MEMORY_IMPORTER_E2E_BIN` to
//! the `ai-memory` binary to run them (CI builds it next to the relay smoke);
//! without it they report a skip and pass, so the companion's own
//! `cargo test` stays independent of the root build.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

use super::DEFAULT_DEST_PREFIX;
use super::export::{self, ExportArgs};
use super::vault::{self, VaultArgs};
use crate::ImportClient;
use crate::mcp::tool_json;

const BIN_ENV: &str = "AI_MEMORY_IMPORTER_E2E_BIN";

struct Server {
    child: Child,
    url: String,
    _root: TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start an isolated server, or `None` when no binary is configured.
async fn start() -> Option<Server> {
    let Some(bin) = std::env::var_os(BIN_ENV).map(PathBuf::from) else {
        eprintln!("skipped: set {BIN_ENV} to an ai-memory binary to run this test");
        return None;
    };
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let data = root.path().join("data");
    std::fs::create_dir_all(&home).unwrap();
    let gitconfig = root.path().join("gitconfig");
    std::fs::write(&gitconfig, "").unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let child = Command::new(&bin)
        .arg("--data-dir")
        .arg(&data)
        .args(["serve", "--transport", "http", "--bind"])
        .arg(format!("127.0.0.1:{port}"))
        .arg("--no-watcher")
        .current_dir(root.path())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .env("AI_MEMORY_HOME", &home)
        .env("AI_MEMORY_DATA_DIR", &data)
        .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
        .env("AI_MEMORY_BACKFILL_ON_START", "false")
        .env("AI_MEMORY_CONSOLIDATE_ON_SESSION_END", "false")
        .env("AI_MEMORY_AUTO_IMPROVE__SCHEDULER__ENABLED", "false")
        .env("RUST_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ai-memory serve");
    let server = Server {
        child,
        url: format!("http://127.0.0.1:{port}"),
        _root: root,
    };
    let client = ImportClient::new(&server.url).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while client.ensure_conditional_writes().await.is_err() {
        assert!(
            Instant::now() < deadline,
            "ai-memory serve never became ready"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Some(server)
}

async fn tool(server: &Server, name: &str, arguments: Value) -> Value {
    let client = ImportClient::new(&server.url).unwrap();
    let result = client.call_tool(name, arguments).await.unwrap();
    tool_json(&result).unwrap_or(result)
}

fn manifest(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[tokio::test]
async fn e2e_vault_import_is_searchable_and_idempotent() {
    let Some(server) = start().await else { return };
    let vault_dir = tempfile::tempdir().unwrap();
    vault::tests::fixture_vault(vault_dir.path());
    let manifest_path = vault_dir.path().join("manifest.json");
    let args = |project: &str, create: bool| VaultArgs {
        dir: vault_dir.path().to_path_buf(),
        workspace: Some("e2e".into()),
        project: Some(project.into()),
        dest_prefix: DEFAULT_DEST_PREFIX.into(),
        server_url: server.url.clone(),
        apply: true,
        manifest_out: Some(manifest_path.clone()),
        create_destination: create,
        show_body: false,
    };

    let error = vault::run(args("missing", false)).await.unwrap_err();
    assert!(
        error.to_string().contains("must already exist"),
        "{error:#}"
    );

    vault::run(args("vault", true)).await.unwrap();
    let first = manifest(&manifest_path);
    assert_eq!(first["written_versions"], 5);

    let hits = tool(
        &server,
        "memory_query",
        json!({"workspace": "e2e", "project": "vault", "query": "Single writer owns the store"}),
    )
    .await;
    assert!(
        hits.to_string()
            .contains("imported/agentmemory/memories/mem-alpha.md"),
        "{hits}"
    );
    let page = tool(
        &server,
        "memory_read_page",
        json!({"workspace": "e2e", "project": "vault", "path": "imported/agentmemory/memories/mem-alpha.md"}),
    )
    .await;
    let body = page["body"].as_str().unwrap();
    assert!(
        body.contains("[[imported/agentmemory/memories/mem-beta]]"),
        "{body}"
    );

    vault::run(args("vault", false)).await.unwrap();
    let second = manifest(&manifest_path);
    assert_eq!(second["written_versions"], 0, "a rerun writes 0");
    assert_eq!(second["present_versions"], 5);
}

/// The server returns 12,000 characters of a body unless the caller asks for
/// more (#1827). A rerun asks for the whole body before comparing it, so a
/// page longer than that is still found present.
#[tokio::test]
async fn e2e_vault_rerun_finds_a_long_page_present() {
    let Some(server) = start().await else { return };
    let vault_dir = tempfile::tempdir().unwrap();
    let page = vault_dir.path().join("memories/long.md");
    std::fs::create_dir_all(page.parent().unwrap()).unwrap();
    let line = "Every mutation goes through one writer thread.\n";
    std::fs::write(
        &page,
        format!("---\nid: \"long\"\n---\n# Long\n\n{}", line.repeat(500)),
    )
    .unwrap();
    let manifest_path = vault_dir.path().join("manifest.json");
    let args = || VaultArgs {
        dir: vault_dir.path().to_path_buf(),
        workspace: Some("e2e".into()),
        project: Some("long".into()),
        dest_prefix: DEFAULT_DEST_PREFIX.into(),
        server_url: server.url.clone(),
        apply: true,
        manifest_out: Some(manifest_path.clone()),
        create_destination: true,
        show_body: false,
    };

    vault::run(args()).await.unwrap();
    assert_eq!(manifest(&manifest_path)["written_versions"], 1);
    vault::run(args()).await.unwrap();
    let second = manifest(&manifest_path);
    assert_eq!(second["written_versions"], 0, "{second}");
    assert_eq!(second["present_versions"], 1, "{second}");
}

#[tokio::test]
async fn e2e_export_import_rebuilds_chains_and_honours_ttl() {
    let Some(server) = start().await else { return };
    let mut data = export::tests::fixture_export();
    // A TTL far in the future stays visible; the fixture's past one is hidden.
    data["memories"][5]["forgetAfter"] = json!("2099-01-01T00:00:00Z");
    let fixture = export::tests::fixture_files(&data, &export::tests::fixture_mapping());
    let args = || ExportArgs {
        file: fixture.file.clone(),
        mapping: fixture.mapping.clone(),
        dest_prefix: DEFAULT_DEST_PREFIX.into(),
        server_url: server.url.clone(),
        apply: true,
        manifest_out: Some(fixture.manifest.clone()),
        create_destination: true,
        show_body: false,
        replay_observations: true,
        allow_partial: false,
    };
    export::run(args()).await.unwrap();
    let first = manifest(&fixture.manifest);
    assert_eq!(first["status"], "imported");
    assert_eq!(first["written_versions"], 14);
    assert!(
        first["hook_sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["status"] == "imported")
    );

    // The chain: three versions written, each conditional on the one before,
    // and the server's latest version is the last one.
    let chain = first["pages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["path"] == "imported/agentmemory/memories/mem-q1.md")
        .unwrap();
    let ids: Vec<&str> = chain["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["page_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 3);
    assert_eq!(
        ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
        3
    );
    let latest = tool(
        &server,
        "memory_read_page",
        json!({"workspace": "acme", "project": "acme-api", "path": "imported/agentmemory/memories/mem-q1.md"}),
    )
    .await;
    assert_eq!(latest["page_id"], ids[2]);
    assert!(
        latest["body"]
            .as_str()
            .unwrap()
            .contains("Queue bound is 4096")
    );

    let query = |workspace: &'static str, project: &'static str, text: &'static str| {
        tool(
            &server,
            "memory_query",
            json!({"workspace": workspace, "project": project, "query": text}),
        )
    };
    let expired = query("acme", "acme-api", "Staging host is temporary").await;
    assert!(!expired.to_string().contains("mem-ttl.md"), "{expired}");
    let live = query("acme", "acme-web", "Run the web build with npm ci").await;
    assert!(live.to_string().contains("mem-web.md"), "{live}");
    let other = query("other", "other-api", "Tabs in the other api").await;
    assert!(other.to_string().contains("mem-other-api.md"), "{other}");
    // The replayed observations landed as the project's latest completed
    // session.
    let observations = tool(
        &server,
        "memory_read_session_observations",
        json!({"workspace": "acme", "project": "acme-web"}),
    )
    .await;
    assert!(
        observations.to_string().contains("Looked at CSS"),
        "{observations}"
    );

    export::run(args()).await.unwrap();
    let second = manifest(&fixture.manifest);
    assert_eq!(second["written_versions"], 0, "a rerun writes 0");
    assert_eq!(second["present_versions"], 14);

    // The destination can evolve independently between imports. Keeping the
    // attribution footer does not mean the current body is still ours.
    let edited_body = format!(
        "Keep this destination correction.\n\n{}",
        latest["body"].as_str().unwrap()
    );
    let edited = tool(
        &server,
        "memory_write_page",
        json!({"workspace": "acme", "project": "acme-api",
            "path": "imported/agentmemory/memories/mem-q1.md",
            "body": edited_body, "expected_page_id": latest["page_id"]}),
    )
    .await;
    data["memories"].as_array_mut().unwrap().push(json!({
        "id": "mem_q4", "version": 4, "parentId": "mem_q3",
        "supersedes": ["mem_q3"], "project": "api",
        "title": "A new source version", "content": "Do not replace the destination edit."
    }));
    std::fs::write(&fixture.file, serde_json::to_vec(&data).unwrap()).unwrap();
    assert!(export::run(args()).await.is_err());
    let current = tool(
        &server,
        "memory_read_page",
        json!({"workspace": "acme", "project": "acme-api",
            "path": "imported/agentmemory/memories/mem-q1.md"}),
    )
    .await;
    assert_eq!(current["page_id"], edited["page_id"]);
    assert_eq!(current["body"], edited_body);
    assert_eq!(manifest(&fixture.manifest)["status"], "failed");
}
