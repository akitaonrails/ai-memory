use super::*;
use crate::agentmemory::mock::{Failure, MockServer};
use tempfile::tempdir;

/// A synthetic `ExportData` in agentmemory's documented shape. Nothing here is
/// real user data. Two repositories share the basename `api`; the mapping
/// tells them apart by session cwd.
pub(crate) fn fixture_export() -> Value {
    json!({
        "version": "0.9.30",
        "exportedAt": "2026-10-01T00:00:00.000Z",
        "sessions": [
            {"id": "ses_1", "project": "api", "cwd": "/work/acme/api", "startedAt": "2026-03-01T10:00:00.000Z",
             "endedAt": "2026-03-01T11:00:00.000Z", "status": "completed", "observationCount": 3,
             "firstPrompt": "Why does the queue stall?", "commitShas": ["abc1234"]},
            {"id": "ses_2", "project": "api", "cwd": "/work/other/api", "startedAt": "2026-03-02T10:00:00.000Z",
             "status": "completed", "observationCount": 0},
            {"id": "ses_3", "project": "web", "cwd": "/work/acme/web", "startedAt": "2026-03-03T10:00:00.000Z",
             "status": "abandoned", "observationCount": 1}
        ],
        "observations": {
            "ses_1": [
                {"id": "obs_2", "sessionId": "ses_1", "timestamp": "2026-03-01T10:20:00.000Z", "type": "edit",
                 "title": "Bounded the queue", "facts": ["capacity 1024"], "narrative": "Set a bound.",
                 "concepts": ["queue"], "files": ["src/queue.rs"], "importance": 6},
                {"id": "obs_1", "sessionId": "ses_1", "timestamp": "2026-03-01T10:10:00.000Z", "type": "read",
                 "title": "Read the queue", "facts": [], "narrative": "token=github_pat_1234567890abcdefghijklmnop",
                 "concepts": [], "files": [], "importance": 3},
                {"id": "obs_3", "sessionId": "ses_1", "timestamp": "2026-03-01T10:30:00.000Z", "type": "test",
                 "title": "Ran the tests", "facts": [], "narrative": "All green.", "concepts": [], "files": [], "importance": 4}
            ],
            "ses_3": [
                {"id": "obs_4", "sessionId": "ses_3", "timestamp": "2026-03-03T10:05:00.000Z", "type": "read",
                 "title": "Looked at CSS", "facts": [], "narrative": "Nothing.", "concepts": [], "files": [], "importance": 1}
            ]
        },
        "memories": [
            // Newest first on purpose: the importer must reorder.
            {"id": "mem_q3", "createdAt": "2026-03-05T00:00:00.000Z", "updatedAt": "2026-03-05T00:00:00.000Z",
             "type": "architecture", "title": "Queue bound is 4096", "content": "Raised again after load tests.",
             "concepts": ["queue"], "files": ["src/queue.rs"], "sessionIds": ["ses_1"], "strength": 0.9,
             "version": 3, "parentId": "mem_q2", "supersedes": ["mem_q2"], "isLatest": true,
             "relatedIds": ["mem_ttl", "mem_web", "mem_missing"]},
            {"id": "mem_q1", "createdAt": "2026-03-01T00:00:00.000Z", "updatedAt": "2026-03-01T00:00:00.000Z",
             "type": "architecture", "title": "Queue bound is 1024", "content": "First bound.",
             "concepts": ["queue"], "files": [], "sessionIds": ["ses_1"], "strength": 0.5,
             "version": 1, "isLatest": false},
            {"id": "mem_q2", "createdAt": "2026-03-03T00:00:00.000Z", "updatedAt": "2026-03-03T00:00:00.000Z",
             "type": "architecture", "title": "Queue bound is 2048", "content": "Raised once.",
             "concepts": ["queue"], "files": [], "sessionIds": ["ses_1"], "strength": 0.7,
             "version": 2, "parentId": "mem_q1", "supersedes": ["mem_q1"], "isLatest": false},
            {"id": "mem_ttl", "createdAt": "2026-03-01T00:00:00.000Z", "updatedAt": "2026-03-01T00:00:00.000Z",
             "type": "fact", "title": "Staging host is temporary", "content": "Gone after the migration.",
             "concepts": [], "files": [], "sessionIds": ["ses_1"], "strength": 0.4, "version": 1,
             "isLatest": true, "forgetAfter": "2026-04-01T00:00:00.000Z"},
            {"id": "mem_other_api", "createdAt": "2026-03-02T00:00:00.000Z", "updatedAt": "2026-03-02T00:00:00.000Z",
             "type": "preference", "title": "Tabs in the other api", "content": "This is the other repository.",
             "concepts": [], "files": [], "sessionIds": ["ses_2"], "strength": 0.4, "version": 1, "isLatest": true},
            {"id": "mem_web", "createdAt": "2026-03-03T00:00:00.000Z", "updatedAt": "2026-03-03T00:00:00.000Z",
             "type": "workflow", "title": "Run the web build with npm ci", "content": "npm ci then npm run build.",
             "concepts": ["build"], "files": [], "sessionIds": ["ses_3"], "strength": 0.6, "version": 1,
             "isLatest": true, "project": "web"}
        ],
        "summaries": [
            {"sessionId": "ses_1", "project": "api", "createdAt": "2026-03-01T11:00:00.000Z",
             "title": "Bounded the job queue", "narrative": "Found the stall and bounded the queue.",
             "keyDecisions": ["Bound the queue"], "filesModified": ["src/queue.rs"], "concepts": ["queue"],
             "observationCount": 3}
        ],
        "lessons": [
            {"id": "les_1", "content": "Always bound in-memory queues.", "context": "From the stall.",
             "confidence": 0.8, "reinforcements": 2, "source": "crystal", "sourceIds": ["crys_1"],
             "project": "api", "tags": ["queues"], "createdAt": "2026-03-04T00:00:00.000Z",
             "updatedAt": "2026-03-04T00:00:00.000Z", "decayRate": 0.01},
            {"id": "les_gone", "content": "Deleted lesson.", "context": "", "confidence": 0.1,
             "reinforcements": 0, "source": "manual", "sourceIds": [], "tags": [],
             "createdAt": "2026-03-04T00:00:00.000Z", "updatedAt": "2026-03-04T00:00:00.000Z",
             "decayRate": 0.01, "deleted": true}
        ],
        "insights": [
            {"id": "ins_1", "title": "Backpressure beats retries", "content": "Bounded queues removed the retry storm.",
             "confidence": 0.7, "reinforcements": 1, "sourceConceptCluster": [], "sourceMemoryIds": ["mem_q3"],
             "sourceLessonIds": ["les_1"], "sourceCrystalIds": [], "project": "api", "tags": [],
             "createdAt": "2026-03-06T00:00:00.000Z", "updatedAt": "2026-03-06T00:00:00.000Z", "decayRate": 0.01}
        ],
        "semanticMemories": [
            {"id": "sem_1", "fact": "The queue lives in src/queue.rs.", "confidence": 0.9,
             "sourceSessionIds": ["ses_1"], "sourceMemoryIds": [], "accessCount": 1,
             "lastAccessedAt": "2026-03-06T00:00:00.000Z", "strength": 0.8,
             "createdAt": "2026-03-06T00:00:00.000Z", "updatedAt": "2026-03-06T00:00:00.000Z"}
        ],
        "proceduralMemories": [
            {"id": "proc_1", "name": "Release the api", "steps": ["Run tests", "Tag"], "triggerCondition": "Before a release",
             "frequency": 2, "sourceSessionIds": ["ses_1"], "strength": 0.5,
             "createdAt": "2026-03-06T00:00:00.000Z", "updatedAt": "2026-03-06T00:00:00.000Z"}
        ],
        "crystals": [
            {"id": "crys_1", "narrative": "Fixed the queue stall.", "keyOutcomes": ["queue bounded"],
             "filesAffected": ["src/queue.rs"], "lessons": ["bound queues"], "sourceActionIds": ["act_1"],
             "sessionId": "ses_1", "createdAt": "2026-03-04T00:00:00.000Z"}
        ],
        "graphNodes": [{"id": "n1"}, {"id": "n2"}],
        "graphEdges": [{"id": "e1"}],
        "actions": [{"id": "act_1"}, {"id": "act_2"}],
        "signals": [{"id": "sig_1"}],
        "routines": [{"id": "rt_1"}],
        "accessLogs": [{"memoryId": "mem_q3", "count": 2, "lastAt": "2026-03-06T00:00:00.000Z", "recent": []}],
        "aFieldFromAFutureVersion": [1, 2]
    })
}

pub(crate) fn fixture_mapping() -> Value {
    json!({
        "projects": {
            "api": {"workspace": "acme", "project": "acme-api"},
            "web": {"workspace": "acme", "project": "acme-web"}
        },
        "cwds": {
            "/work/other/api": {"workspace": "other", "project": "other-api"}
        }
    })
}

pub(crate) struct Fixture {
    pub(crate) _dir: tempfile::TempDir,
    pub(crate) file: PathBuf,
    pub(crate) mapping: PathBuf,
    pub(crate) manifest: PathBuf,
}

pub(crate) fn fixture_files(export: &Value, mapping: &Value) -> Fixture {
    let dir = tempdir().unwrap();
    let file = dir.path().join("export.json");
    let mapping_path = dir.path().join("mapping.json");
    fs::write(&file, serde_json::to_vec_pretty(export).unwrap()).unwrap();
    fs::write(&mapping_path, serde_json::to_vec_pretty(mapping).unwrap()).unwrap();
    Fixture {
        manifest: dir.path().join("manifest.json"),
        file,
        mapping: mapping_path,
        _dir: dir,
    }
}

fn plan_fixture(export: &Value, mapping: &Value, replay: bool) -> ExportPlan {
    let files = fixture_files(export, mapping);
    let mapping = Mapping::load(&files.mapping).unwrap();
    plan(
        &files.file,
        &mapping,
        DEFAULT_DEST_PREFIX,
        replay,
        crate::DEFAULT_SERVER_URL,
    )
    .unwrap()
}

fn args(fixture: &Fixture, url: &str) -> ExportArgs {
    ExportArgs {
        file: fixture.file.clone(),
        mapping: fixture.mapping.clone(),
        dest_prefix: DEFAULT_DEST_PREFIX.into(),
        server_url: url.into(),
        apply: true,
        manifest_out: Some(fixture.manifest.clone()),
        create_destination: true,
        show_body: false,
        replay_observations: false,
        allow_partial: false,
    }
}

fn read_manifest(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

const CHAIN: &str = "imported/agentmemory/memories/mem-q1.md";

#[tokio::test]
async fn export_manifest_cannot_replace_its_inputs() {
    let fixture = fixture_files(&fixture_export(), &fixture_mapping());
    for input in [&fixture.file, &fixture.mapping] {
        let original = fs::read(input).unwrap();
        let mut dry = args(&fixture, crate::DEFAULT_SERVER_URL);
        dry.apply = false;
        dry.manifest_out = Some(input.clone());
        let error = run(dry).await.unwrap_err();
        assert!(error.to_string().contains("--manifest-out"), "{error:#}");
        assert_eq!(fs::read(input).unwrap(), original);
    }
}

#[test]
fn manifest_counts_equal_the_fixture() {
    let plan = plan_fixture(&fixture_export(), &fixture_mapping(), false);
    let manifest = Manifest::new(IMPORT_VERSION, String::new(), String::new(), &plan.pages);
    // Four memory lineages (q1..q3, ttl, other-api, web), six versions.
    assert_eq!(manifest.planned["memory_pages"], 4);
    assert_eq!(manifest.planned["memory_versions"], 6);
    assert_eq!(manifest.planned["lesson_pages"], 1);
    assert_eq!(manifest.planned["insight_pages"], 1);
    assert_eq!(manifest.planned["semantic-memory_pages"], 1);
    assert_eq!(manifest.planned["procedural-memory_pages"], 1);
    assert_eq!(manifest.planned["crystal_pages"], 1);
    assert_eq!(manifest.planned["session_pages"], 3);
    let expected: BTreeMap<String, usize> = [
        ("observations", 4),
        ("graphNodes", 2),
        ("graphEdges", 1),
        ("actions", 2),
        ("signals", 1),
        ("routines", 1),
        ("accessLogs", 1),
        ("aFieldFromAFutureVersion", 2),
        ("lessons (deleted)", 1),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    assert_eq!(plan.dropped, expected);
    assert!(plan.unmapped.is_empty());
    for page in &plan.pages {
        assert!(
            page.path.starts_with("imported/agentmemory/"),
            "{}",
            page.path
        );
    }
}

#[test]
fn versions_are_ordered_oldest_first_on_one_path_and_ttl_maps_to_expires_at() {
    let plan = plan_fixture(&fixture_export(), &fixture_mapping(), false);
    let chain = plan.pages.iter().find(|p| p.path == CHAIN).unwrap();
    let ids: Vec<_> = chain
        .versions
        .iter()
        .map(|v| v.source_id.as_str())
        .collect();
    assert_eq!(ids, ["mem_q1", "mem_q2", "mem_q3"]);
    assert_eq!(chain.versions[2].kind, "decision");
    let ttl = plan
        .pages
        .iter()
        .find(|p| p.path.ends_with("mem-ttl.md"))
        .unwrap();
    assert_eq!(
        ttl.versions[0].expires_at.as_deref(),
        Some("2026-04-01T00:00:00.000Z")
    );
    // Related links: same scope as a plain link, another scope qualified,
    // a missing record as plain text.
    let body = &chain.versions[2].body;
    assert!(
        body.contains("[[imported/agentmemory/memories/mem-ttl]]"),
        "{body}"
    );
    assert!(
        body.contains("[[acme/acme-web:imported/agentmemory/memories/mem-web]]"),
        "{body}"
    );
    assert!(body.contains("`mem_missing`"), "{body}");
}

#[test]
fn colliding_basenames_split_by_cwd() {
    // Newer exports carry `project` on memories too; the cited session's cwd
    // still decides which repository it was.
    let mut export = fixture_export();
    export["memories"][4]["project"] = json!("api");
    let plan = plan_fixture(&export, &fixture_mapping(), false);
    let other = plan
        .pages
        .iter()
        .find(|p| p.path.ends_with("mem-other-api.md"))
        .unwrap();
    assert_eq!(other.destination.workspace, "other");
    assert_eq!(other.destination.project, "other-api");
    let ours = plan.pages.iter().find(|p| p.path == CHAIN).unwrap();
    assert_eq!(ours.destination.project, "acme-api");
}

#[test]
fn text_is_redacted_before_the_plan_exists() {
    let mut export = fixture_export();
    export["memories"][0]["content"] = json!(concat!("key: sk-", "1234567890abcdefghijklmnop end")); // split for secret scanners
    let plan = plan_fixture(&export, &fixture_mapping(), true);
    let all = format!(
        "{:?}{}",
        plan.pages,
        serde_json::to_string(
            &plan
                .hook_sessions
                .iter()
                .map(|s| &s.events)
                .collect::<Vec<_>>()
        )
        .unwrap()
    );
    assert!(!all.contains("sk-1234567890abcdefghijklmnop"));
    assert!(!all.contains("github_pat_1234567890abcdefghijklmnop"));
    assert!(all.contains("REDACTED"));
}

#[test]
fn malformed_inputs_are_refused_before_planning() {
    let files = fixture_files(&fixture_export(), &json!({"projects": {}, "surprise": 1}));
    assert!(
        Mapping::load(&files.mapping).is_err(),
        "unknown mapping keys fail closed"
    );
    let files = fixture_files(
        &fixture_export(),
        &json!({"projects": {"api": {"workspace": "a/b", "project": "p"}}}),
    );
    assert!(
        Mapping::load(&files.mapping).is_err(),
        "a path separator is not a workspace"
    );

    let mapping = fixture_mapping();
    for (mutate, needle) in [
        (
            json!({"memories": [{"id": "sk-1234567890abcdefghijklmnop"}]}),
            "credential",
        ),
        (
            json!({"memories": [{"id": "m", "forgetAfter": "next tuesday"}]}),
            "forgetAfter",
        ),
        (json!({"memories": [{"id": "m"}, {"id": "m"}]}), "duplicate"),
        (json!({"memories": "nope"}), "array"),
        (json!({"sessions": [{"project": "api"}]}), "sessions[0]"),
    ] {
        let files = fixture_files(&mutate, &mapping);
        let loaded = Mapping::load(&files.mapping).unwrap();
        let error = plan(
            &files.file,
            &loaded,
            DEFAULT_DEST_PREFIX,
            false,
            crate::DEFAULT_SERVER_URL,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains(needle), "{needle}: {error:#}");
    }
    // Two ids whose slugs collide.
    let files = fixture_files(
        &json!({"lessons": [{"id": "A B"}, {"id": "a-b"}]}),
        &json!({"unscoped": {"workspace": "w", "project": "p"}}),
    );
    let loaded = Mapping::load(&files.mapping).unwrap();
    let error = plan(
        &files.file,
        &loaded,
        DEFAULT_DEST_PREFIX,
        false,
        crate::DEFAULT_SERVER_URL,
    )
    .unwrap_err();
    assert!(error.to_string().contains("collision"), "{error}");
}

#[tokio::test]
async fn apply_rebuilds_the_supersession_chain_conditionally() {
    let fixture = fixture_files(&fixture_export(), &fixture_mapping());
    let server = MockServer::start(&[]);
    run(args(&fixture, &server.url)).await.unwrap();
    let state = server.state();
    let chain = state.chain("acme", "acme-api", CHAIN);
    assert_eq!(chain.len(), 3, "chain length equals the version count");
    assert_eq!(chain[0].arguments["create_only"], true);
    assert_eq!(
        chain[1].arguments["expected_page_id"],
        chain[0].page_id.as_str()
    );
    assert_eq!(
        chain[2].arguments["expected_page_id"],
        chain[1].page_id.as_str()
    );
    assert!(chain[2].body.contains("Queue bound is 4096"));
    let ttl = state.chain(
        "acme",
        "acme-api",
        "imported/agentmemory/memories/mem-ttl.md",
    );
    assert_eq!(ttl[0].arguments["expires_at"], "2026-04-01T00:00:00.000Z");
    assert!(
        state.hook_batches.is_empty(),
        "observations are dropped by default"
    );
    assert!(
        state
            .pages
            .keys()
            .all(|(_, _, path)| !path.starts_with("sessions/"))
    );
    drop(state);
    let manifest = read_manifest(&fixture.manifest);
    assert_eq!(manifest["status"], "imported");
    assert_eq!(manifest["written_versions"], 14);
    assert_eq!(manifest["dropped"]["observations"], 4);
    assert_eq!(manifest["dropped"]["signals"], 1);
}

#[tokio::test]
async fn an_unmapped_project_aborts_live_mode_before_any_request() {
    let mut mapping = fixture_mapping();
    mapping["projects"].as_object_mut().unwrap().remove("web");
    let fixture = fixture_files(&fixture_export(), &mapping);
    let server = MockServer::start(&[]);
    let error = run(args(&fixture, &server.url)).await.unwrap_err();
    assert!(error.to_string().contains("web"), "{error:#}");
    assert!(
        server.state().requests.is_empty(),
        "no request before the refusal"
    );
    let manifest = read_manifest(&fixture.manifest);
    assert_eq!(manifest["status"], "failed");
    // mem_web and the ses_3 session page: every unmapped record is counted.
    assert_eq!(manifest["unmapped_projects"]["web"], 2);

    // The dry run reports the same gap and succeeds.
    let mut dry = args(&fixture, &server.url);
    dry.apply = false;
    run(dry).await.unwrap();
    assert!(server.state().requests.is_empty());
}

#[tokio::test]
async fn resume_after_an_injected_failure_writes_no_duplicates() {
    // Write 2 is the chain's second version: fail it before and after the
    // server stores it.
    for failure in [Failure::Before(2), Failure::After(2), Failure::After(7)] {
        let fixture = fixture_files(&fixture_export(), &fixture_mapping());
        let server = MockServer::start(&[]);
        server.state().failure = Some(failure);
        assert!(run(args(&fixture, &server.url)).await.is_err());
        assert_eq!(read_manifest(&fixture.manifest)["status"], "failed");
        server.state().failure = None;
        run(args(&fixture, &server.url)).await.unwrap();
        assert_eq!(server.state().stored_versions(), 14, "{failure:?}");
        assert_eq!(
            server.state().chain("acme", "acme-api", CHAIN).len(),
            3,
            "{failure:?}"
        );
        // And a third run writes nothing at all.
        run(args(&fixture, &server.url)).await.unwrap();
        assert_eq!(server.state().stored_versions(), 14, "{failure:?}");
        assert_eq!(read_manifest(&fixture.manifest)["written_versions"], 0);
    }
}

#[tokio::test]
async fn resume_refuses_an_edited_body_even_with_the_original_ingest_key() {
    let fixture = fixture_files(
        &json!({"memories": [
            {"id": "mem_q1", "project": "api", "version": 1,
             "title": "First policy", "content": "Initial source body."},
            {"id": "mem_q2", "project": "api", "version": 2, "parentId": "mem_q1",
             "title": "Second policy", "content": "New source body."}
        ]}),
        &fixture_mapping(),
    );
    let server = MockServer::start(&[]);
    server.state().failure = Some(Failure::Before(2));
    assert!(run(args(&fixture, &server.url)).await.is_err());

    // A normal destination edit preserves the import's attribution footer.
    // Its new page_id must not become the precondition for importing v2.
    let edited_body = {
        let mut state = server.state();
        state.failure = None;
        let chain = state
            .pages
            .get_mut(&("acme".into(), "acme-api".into(), CHAIN.into()))
            .unwrap();
        let mut edited = chain.last().unwrap().clone();
        edited.page_id = "destination-edit".into();
        edited
            .body
            .insert_str(0, "Keep this destination correction.\n\n");
        let body = edited.body.clone();
        chain.push(edited);
        body
    };

    let error = run(args(&fixture, &server.url)).await.unwrap_err();
    assert!(error.to_string().contains("did not write"), "{error:#}");
    let state = server.state();
    let chain = state.chain("acme", "acme-api", CHAIN);
    assert_eq!(chain.len(), 2, "no source version replaces the edit");
    assert_eq!(chain[1].page_id, "destination-edit");
    assert_eq!(chain[1].body, edited_body);
    assert_eq!(state.stored_versions(), 2, "the run stops at the conflict");
    let manifest = read_manifest(&fixture.manifest);
    assert_eq!(manifest["status"], "failed");
    assert_eq!(manifest["written_versions"], 0);
    assert_eq!(manifest["present_versions"], 0);
}

#[tokio::test]
async fn observations_replay_as_ordered_external_import_sessions_when_asked() {
    let fixture = fixture_files(&fixture_export(), &fixture_mapping());
    let server = MockServer::start(&[]);
    let mut replay = args(&fixture, &server.url);
    replay.replay_observations = true;
    run(replay).await.unwrap();
    let state = server.state();
    assert_eq!(
        state.hook_batches.len(),
        2,
        "one batch per session with observations"
    );
    let batch = state.hook_batches[0].as_array().unwrap();
    assert_eq!(batch.len(), 5, "start + 3 observations + end");
    let urls: Vec<&str> = batch
        .iter()
        .map(|item| item["url"].as_str().unwrap())
        .collect();
    assert!(urls[0].contains("event=session-start"));
    assert!(urls[4].contains("event=session-end"));
    assert!(urls.iter().all(|url| url.contains("agent=external-import")));
    assert!(urls[1].contains("source_event=agentmemory-observation"));
    // Oldest observation first, redacted.
    assert_eq!(batch[1]["body"]["title"], "Read the queue");
    assert!(
        !batch[1]["body"]["message"]
            .as_str()
            .unwrap()
            .contains("github_pat_")
    );
    assert_eq!(batch[2]["body"]["title"], "Bounded the queue");
    drop(state);
    let manifest = read_manifest(&fixture.manifest);
    assert!(manifest["dropped"].get("observations").is_none());
    assert_eq!(manifest["hook_sessions"][0]["status"], "imported");
}

#[tokio::test]
async fn a_partial_export_page_needs_allow_partial() {
    let mut export = fixture_export();
    export["pagination"] = json!({"offset": 0, "limit": 3, "total": 9, "hasMore": true});
    let fixture = fixture_files(&export, &fixture_mapping());
    let server = MockServer::start(&[]);
    let error = run(args(&fixture, &server.url)).await.unwrap_err();
    assert!(error.to_string().contains("--allow-partial"), "{error:#}");
    assert!(server.state().requests.is_empty());
    let mut partial = args(&fixture, &server.url);
    partial.allow_partial = true;
    run(partial).await.unwrap();
}
