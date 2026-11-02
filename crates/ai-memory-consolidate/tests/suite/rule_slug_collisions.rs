//! Rule pages whose titles collide inside one batch keep separate pages
//! (#1943).

use std::sync::Arc;

use ai_memory_core::{
    ActorContext, AgentKind, NewObservation, NewSession, ObservationKind, PagePath, Sanitized,
    Sanitizer, SessionId, Tier,
};
use ai_memory_llm::{ChatRequest, ChatResponse, LlmProvider, LlmResult};
use ai_memory_store::Store;
use ai_memory_wiki::{Wiki, WritePageRequest};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::Consolidator;

/// Answers one fixed batch for every structured call, so the run is a
/// pure function of the scripted response.
struct ScriptedLlm(Value);

#[async_trait::async_trait]
impl LlmProvider for ScriptedLlm {
    fn name(&self) -> &'static str {
        "scripted"
    }
    fn model(&self) -> &str {
        "scripted"
    }
    async fn complete(&self, _: ChatRequest) -> LlmResult<ChatResponse> {
        unreachable!("multi-page consolidation only uses structured completion")
    }
    async fn complete_structured_raw(&self, _: ChatRequest, _: Value) -> LlmResult<Value> {
        Ok(self.0.clone())
    }
}

/// Store + wiki + a seeded session, ready for a real (non-dry) batch run.
async fn fixture(tmp: &std::path::Path) -> (Store, Wiki, SessionId) {
    let store = Store::open(tmp).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let session = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id: session,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: None,
        })
        .await
        .unwrap();
    let sanitizer = Sanitizer::builtin();
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                occurred_at: None,
                session_id: session,
                workspace_id: ws,
                project_id: proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "convention".to_string(),
                body: "set the deploy convention".to_string(),
                importance: 3,
            },
            &sanitizer,
        ))
        .await
        .unwrap();
    let wiki = Wiki::new(tmp, store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    (store, wiki, session)
}

fn rule_update(title: &str, body: &str, entities: &[&str]) -> Value {
    json!({
        "path": "notes/ignored-for-rules.md",
        "tier": "semantic",
        "kind": "rule",
        "title": title,
        "body_markdown": body,
        "tags": [],
        "entities": entities,
    })
}

/// The long-title pair from the #1943 report shape: two different rules
/// whose pure-ASCII titles truncate into one 60-char slug window, so only
/// the same-batch guard, not the fold-loss digest, can keep them apart.
const LONG_PREFIX: &str = "always gate deploys on the checklist that ships with the repository ";

/// Two different rules that slug to one path must both survive the batch
/// as live pages with their own links and entities, and a mixed-script
/// rule in the same batch takes its fold-loss digest. Re-running the same
/// batch lands on the same names (adversarial: before the fix the second
/// rule superseded the first at the shared path and was silently lost).
#[tokio::test]
async fn colliding_rule_batch_keeps_both_pages() {
    let tmp = TempDir::new().unwrap();
    let (store, wiki, session) = fixture(tmp.path()).await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();

    // A real page for each rule's typed edge, so the relations resolve.
    let target = PagePath::new("gotchas/deploy-gate.md").unwrap();
    let target_id = wiki
        .write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: target.clone(),
            frontmatter: serde_json::json!({}),
            body: "The gate exists.".into(),
            tier: Tier::Semantic,
            pinned: false,
            title: Some("Deploy gate".into()),
            admission_ctx: None,
            author_id: None,
            actor: ActorContext::anonymous(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();

    let response = json!({
        "rationale": "two colliding rules plus one mixed-script rule",
        "updates": [
            {
                "path": "notes/ignored.md",
                "tier": "semantic",
                "kind": "rule",
                "title": format!("{LONG_PREFIX}alpha branch"),
                "body_markdown": "Rule for the alpha branch.",
                "tags": [],
                "entities": ["alpha", "checklist"],
                "relations": {"fixes": ["gotchas/deploy-gate.md"]},
            },
            {
                "path": "notes/ignored.md",
                "tier": "semantic",
                "kind": "rule",
                "title": format!("{LONG_PREFIX}beta branch"),
                "body_markdown": "Rule for the beta branch.",
                "tags": [],
                "entities": ["beta", "checklist"],
                "relations": {"causes": ["gotchas/deploy-gate.md"]},
            },
            rule_update("ACME 客户周报的格式与范围", "Rule for the report.", &["acme"]),
        ],
    });
    let consolidator = || {
        Consolidator::new(
            store.reader.clone(),
            store.writer.clone(),
            wiki.clone(),
            Arc::new(ScriptedLlm(response.clone())),
            ws,
            proj,
        )
    };

    let outcomes = consolidator()
        .consolidate_session_multi(session, false, ActorContext::anonymous(), None, None)
        .await
        .unwrap();
    assert_eq!(
        outcomes.len(),
        3,
        "every rule must be written, none dropped"
    );

    let mut rule_paths: Vec<&PagePath> = outcomes.iter().map(|o| &o.path).collect();
    rule_paths.sort_unstable();
    rule_paths.dedup();
    assert_eq!(
        rule_paths.len(),
        3,
        "colliding rules must not share a path: {:?}",
        outcomes.iter().map(|o| o.path.as_str()).collect::<Vec<_>>()
    );
    for outcome in &outcomes {
        assert!(outcome.path.as_str().starts_with("_rules/"));
        let stem = outcome
            .path
            .as_str()
            .trim_end_matches(".md")
            .rsplit('/')
            .next()
            .unwrap();
        let hex = &stem[stem.len() - 8..];
        assert!(
            stem[stem.len() - 9..].starts_with('-') && hex.chars().all(|c| c.is_ascii_hexdigit()),
            "each contested rule is named apart by an 8-hex digest: {}",
            outcome.path
        );
        outcome.path.ensure_portable().unwrap();
    }

    // Both long-titled rules live, each with its own title, entities and
    // typed link; the loss the fix prevents is one of them superseding
    // the other at the shared path.
    let mut ids = Vec::new();
    for (title, entities, link_type) in [
        (
            "always gate deploys on the checklist that ships with the repository alpha branch",
            "alpha",
            "fixes",
        ),
        (
            "always gate deploys on the checklist that ships with the repository beta branch",
            "beta",
            "causes",
        ),
    ] {
        let outcome = outcomes
            .iter()
            .find(|o| o.new_title == title)
            .unwrap_or_else(|| panic!("rule {title} must be one of the outcomes"));
        let stored = wiki.read_page(ws, proj, &outcome.path).unwrap();
        assert_eq!(stored.frontmatter["title"], title);
        assert_eq!(stored.frontmatter["entities"][0], entities);
        ids.push((outcome.page_id.unwrap(), link_type));
    }
    let db = rusqlite::Connection::open(store.db_path()).unwrap();
    for (page_id, link_type) in ids {
        let rows: Vec<String> = db
            .prepare("SELECT link_type FROM links WHERE from_page_id = ?1")
            .unwrap()
            .query_map([page_id.as_bytes()], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![link_type.to_string()],
            "page {page_id} keeps its own typed edge"
        );
        let linked: Vec<Vec<u8>> = db
            .prepare("SELECT to_page_id FROM links WHERE from_page_id = ?1")
            .unwrap()
            .query_map([page_id.as_bytes()], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(linked, vec![target_id.as_bytes().to_vec()]);
    }

    // Determinism: the same batch, re-consolidated, lands on the same
    // names instead of re-rolling which rule survives.
    let again = consolidator()
        .consolidate_session_multi(session, false, ActorContext::anonymous(), None, None)
        .await
        .unwrap();
    let mut first: Vec<&str> = outcomes.iter().map(|o| o.path.as_str()).collect();
    let mut second: Vec<&str> = again.iter().map(|o| o.path.as_str()).collect();
    first.sort_unstable();
    second.sort_unstable();
    assert_eq!(first, second);
}

/// Two updates for the same rule (the model restating one convention)
/// still share the plain slug and supersede each other on one page: the
/// guard must not fork a restatement into a digest-named duplicate
/// (control).
#[tokio::test]
async fn identical_rule_titles_still_share_one_page() {
    let tmp = TempDir::new().unwrap();
    let (store, wiki, session) = fixture(tmp.path()).await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let response = json!({
        "rationale": "one rule restated",
        "updates": [
            rule_update("Deploy checklists first", "First statement.", &[]),
            rule_update("Deploy checklists first", "Restated statement.", &[]),
        ],
    });

    let outcomes = Consolidator::new(
        store.reader.clone(),
        store.writer.clone(),
        wiki.clone(),
        Arc::new(ScriptedLlm(response)),
        ws,
        proj,
    )
    .consolidate_session_multi(session, false, ActorContext::anonymous(), None, None)
    .await
    .unwrap();

    assert_eq!(outcomes.len(), 2);
    assert_eq!(
        outcomes[0].path, outcomes[1].path,
        "identical titles keep one shared path"
    );
    assert_eq!(
        outcomes[0].path.as_str(),
        "_rules/deploy-checklists-first.md",
        "a pure-ASCII restatement keeps its plain slug, digest-free"
    );
    let stored = wiki
        .read_page(ws, proj, &outcomes[0].path)
        .expect("the shared rule page exists");
    assert_eq!(stored.frontmatter["title"], "Deploy checklists first");
    assert!(
        stored.body.contains("Restated statement."),
        "the restatement supersedes in place instead of forking a page"
    );
    assert!(!stored.body.contains("First statement."));
}

/// The model proposes a readable `_rules/<slug>.md` for a rule; the M20
/// routing (or the collision rename) writes it under a different final
/// path. Wikilinks in sibling bodies that target the proposed path must
/// come out pointing at the final live page: same-project links are by
/// design allowed to dangle, so before the fix nothing ever repaired
/// them and the sibling linked at a page that never exists (the second
/// half of #1943). A link to a page whose path did not move stays
/// byte-identical (control).
#[tokio::test]
async fn sibling_links_to_a_rerouted_rule_are_rewritten() {
    let tmp = TempDir::new().unwrap();
    let (store, wiki, session) = fixture(tmp.path()).await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let session_path = format!("sessions/{session}.md");
    let response = json!({
        "rationale": "one mixed-script rule, one colliding pair, one stable page",
        "updates": [
            {
                "path": "_rules/acme-client-weekly-report-format.md",
                "tier": "semantic",
                "kind": "rule",
                "title": "ACME 客户周报的格式与范围",
                "body_markdown": "Rule for the report.",
                "tags": [],
            },
            {
                "path": format!("_rules/{LONG_PREFIX}alpha branch.md"),
                "tier": "semantic",
                "kind": "rule",
                "title": format!("{LONG_PREFIX}alpha branch"),
                "body_markdown": "Rule for the alpha branch.",
                "tags": [],
            },
            {
                "path": format!("_rules/{LONG_PREFIX}beta branch.md"),
                "tier": "semantic",
                "kind": "rule",
                "title": format!("{LONG_PREFIX}beta branch"),
                "body_markdown": "Rule for the beta branch.",
                "tags": [],
            },
            {
                "path": "notes/context.md",
                "tier": "semantic",
                "kind": "fact",
                "title": "Context",
                "body_markdown": "Stable page.",
                "tags": [],
            },
            {
                "path": session_path,
                "tier": "episodic",
                "kind": "fact",
                "title": "Session narrative",
                "body_markdown": format!(
                    "Conventions set this session: the report rule \
                     [[_rules/acme-client-weekly-report-format]], the alpha gate \
                     [[_rules/{LONG_PREFIX}alpha branch]], and the context notes \
                     [[notes/context]]."
                ),
                "tags": [],
            },
        ],
    });

    let outcomes = Consolidator::new(
        store.reader.clone(),
        store.writer.clone(),
        wiki.clone(),
        Arc::new(ScriptedLlm(response)),
        ws,
        proj,
    )
    .consolidate_session_multi(session, false, ActorContext::anonymous(), None, None)
    .await
    .unwrap();
    assert_eq!(outcomes.len(), 5);

    let final_path = |title_prefix: &str| {
        outcomes
            .iter()
            .find(|o| o.new_title.starts_with(title_prefix))
            .unwrap_or_else(|| panic!("rule {title_prefix}… must be one of the outcomes"))
            .path
            .clone()
    };
    let report = final_path("ACME");
    let alpha = final_path(LONG_PREFIX);
    assert_ne!(
        report.as_str(),
        "_rules/acme-client-weekly-report-format.md"
    );
    assert_ne!(
        alpha.as_str(),
        format!("_rules/{LONG_PREFIX}alpha branch.md"),
        "the colliding pair is renamed apart"
    );
    // Every redirected target is a live page.
    for path in [&report, &alpha] {
        wiki.read_page(ws, proj, path)
            .unwrap_or_else(|e| panic!("{} must be live: {e}", path.as_str()));
    }

    let anchor = wiki
        .read_page(ws, proj, &PagePath::new(session_path).unwrap())
        .unwrap();
    assert!(
        anchor.body.contains(&format!("[[{}]]", report.as_str())),
        "the report link must point at the final path, not the proposal: {}",
        anchor.body
    );
    assert!(
        anchor.body.contains(&format!("[[{}]]", alpha.as_str())),
        "the alpha link must point at the collision-renamed path: {}",
        anchor.body
    );
    assert!(
        !anchor.body.contains("acme-client-weekly-report-format"),
        "the proposed path is gone from the body: {}",
        anchor.body
    );
    // Control: a link whose target did not move is copied byte-identically.
    assert!(
        anchor.body.contains("[[notes/context]]"),
        "an unmoved target keeps its wikilink untouched: {}",
        anchor.body
    );
    assert!(
        wiki.read_page(ws, proj, &PagePath::new("notes/context.md").unwrap())
            .is_ok(),
        "the stable page is live at its proposed path"
    );
}
