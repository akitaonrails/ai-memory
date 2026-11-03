//! Page history and diff (`/w/{ws}/{proj}/h/*path`, `/api/v1/.../history/*path`)
//! and the session timeline (`/w/{ws}/{proj}/s/{session_id}`).
//!
//! Both views read by id, so each boundary is tested adversarially: a path,
//! version id or session id from another project or workspace must read as
//! not found, a restricted project must refuse a non-grantee, and captured
//! text must render inert. Every refusal has a legitimate control beside it.

use ai_memory_core::{
    ActorContext, AgentKind, AuthorizedViewer, IdentityKey, NewObservation, NewPage, NewSession,
    NewUser, ObservationKind, PageId, PagePath, ProjectId, Sanitized, Sanitizer, SessionId, Tier,
    UserId, UserRole, WorkspaceId,
};
use ai_memory_store::{GrantLevel, Store};
use ai_memory_web::{api_router, router};
use ai_memory_wiki::Wiki;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;

struct Fixture {
    _tmp: TempDir,
    store: Store,
    wiki: Wiki,
}

impl Fixture {
    async fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        Self {
            _tmp: tmp,
            store,
            wiki,
        }
    }

    async fn scope(&self, workspace: &str, project: &str) -> (WorkspaceId, ProjectId) {
        let ws = self
            .store
            .writer
            .get_or_create_workspace(workspace)
            .await
            .unwrap();
        let proj = self
            .store
            .writer
            .get_or_create_project(ws, project, None)
            .await
            .unwrap();
        (ws, proj)
    }

    async fn write(
        &self,
        scope: (WorkspaceId, ProjectId),
        path: &str,
        body: &str,
        author: Option<UserId>,
    ) -> PageId {
        self.write_expiring(scope, path, body, author, None).await
    }

    async fn write_expiring(
        &self,
        (ws, proj): (WorkspaceId, ProjectId),
        path: &str,
        body: &str,
        author: Option<UserId>,
        expires_at: Option<jiff::Timestamp>,
    ) -> PageId {
        self.store
            .writer
            .upsert_page(NewPage {
                workspace_id: ws,
                project_id: proj,
                path: PagePath::new(path).unwrap(),
                title: "Plan".into(),
                body: body.into(),
                tier: Tier::Semantic,
                frontmatter_json: serde_json::json!({"kind": "fact"}),
                pinned: false,
                links: Vec::new(),
                author_id: author,
                expires_at,
                entities: Vec::new(),
                evidence: Vec::new(),
            })
            .await
            .unwrap()
    }

    async fn user(&self, name: &str) -> UserId {
        self.store
            .writer
            .create_human_user(
                NewUser {
                    username: name.into(),
                    name: None,
                    email: None,
                },
                UserRole::User,
                None,
                false,
            )
            .await
            .unwrap()
    }

    async fn session(
        &self,
        (ws, proj): (WorkspaceId, ProjectId),
        owner: Option<&str>,
        rows: &[(&str, &str)],
    ) -> SessionId {
        let id = SessionId::new();
        self.store
            .writer
            .begin_session(NewSession {
                occurred_at: None,
                id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
                actor_user: owner.map(|name| IdentityKey::User(name.into()).storage_key()),
            })
            .await
            .unwrap();
        for (title, body) in rows {
            self.store
                .writer
                .insert_observation(Sanitized::new(
                    NewObservation {
                        occurred_at: None,
                        session_id: id,
                        workspace_id: ws,
                        project_id: proj,
                        kind: ObservationKind::UserPrompt,
                        extension: None,
                        source_event: None,
                        title: (*title).to_owned(),
                        body: (*body).to_owned(),
                        importance: 5,
                        file_path: None,
                    },
                    &Sanitizer::builtin(),
                ))
                .await
                .unwrap();
        }
        id
    }

    fn web(&self) -> axum::Router {
        router(self.store.reader.clone(), self.wiki.clone())
    }

    fn api(&self) -> axum::Router {
        api_router(self.store.reader.clone(), self.wiki.clone())
    }
}

/// GET `uri`, stamping what the auth layer would: a grant-checked viewer and
/// an actor identity.
async fn get_as(
    app: &axum::Router,
    uri: &str,
    viewer: Option<UserId>,
    actor: Option<&str>,
) -> (StatusCode, String) {
    let mut req = Request::builder().uri(uri);
    if let Some(viewer) = viewer {
        req = req.extension(AuthorizedViewer(viewer));
    }
    if let Some(user) = actor {
        req = req.extension(ActorContext {
            user: Some(user.to_owned()),
            ..ActorContext::default()
        });
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, String) {
    get_as(app, uri, None, None).await
}

async fn get_json(app: &axum::Router, uri: &str) -> Value {
    let (status, body) = get(app, uri).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    serde_json::from_str(&body).unwrap()
}

/// `/w/{ws}/{proj}/h/{rest}` as its `/api/v1` twin.
fn api_history_uri(web: &str) -> String {
    let rest = web.strip_prefix("/w/").unwrap();
    let (ws, rest) = rest.split_once('/').unwrap();
    let (proj, rest) = rest.split_once('/').unwrap();
    let rest = rest.strip_prefix("h/").unwrap();
    format!("/workspaces/{ws}/projects/{proj}/history/{rest}")
}

// ---------------------------------------------------------------------------
// V1: page history and diff
// ---------------------------------------------------------------------------

/// K supersessions leave K+1 versions, listed newest first in both views, and
/// the default diff is the latest against the version it replaced.
#[tokio::test]
async fn history_lists_k_plus_one_versions_newest_first_and_diffs_the_latest() {
    let f = Fixture::new().await;
    let scope = f.scope("default", "app").await;
    const K: usize = 4;
    let mut ids = Vec::new();
    for i in 0..=K {
        ids.push(
            f.write(
                scope,
                "notes/plan.md",
                &format!("# Plan\n\nshared line\nrevision {i}\n"),
                None,
            )
            .await,
        );
    }

    let json = get_json(
        &f.api(),
        "/workspaces/default/projects/app/history/notes/plan.md",
    )
    .await;
    assert_eq!(json["total"], (K + 1) as u64);
    let listed: Vec<String> = json["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_owned())
        .collect();
    let expected: Vec<String> = ids.iter().rev().map(ToString::to_string).collect();
    assert_eq!(listed, expected, "newest first, every version");
    let latest: Vec<bool> = json["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["is_latest"].as_bool().unwrap())
        .collect();
    assert!(latest[0]);
    assert!(latest[1..].iter().all(|l| !l), "{latest:?}");
    // Each version names the one it replaced: an unbroken chain.
    for pair in json["versions"].as_array().unwrap().windows(2) {
        assert_eq!(pair[0]["supersedes"], pair[1]["id"]);
    }

    let diff = &json["diff"];
    assert_eq!(diff["status"], "changed");
    assert_eq!(diff["to"]["id"], ids[K].to_string());
    assert_eq!(diff["from"]["id"], ids[K - 1].to_string());
    let lines: Vec<String> = diff["hunks"][0]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            format!(
                "{}{}",
                l["op"].as_str().unwrap(),
                l["text"].as_str().unwrap()
            )
        })
        .collect();
    assert!(lines.contains(&format!("-revision {}", K - 1)), "{lines:?}");
    assert!(lines.contains(&format!("+revision {K}")), "{lines:?}");
    assert!(lines.contains(&" shared line".to_owned()), "{lines:?}");

    // The HTML view lists the same K+1 versions in the same order.
    let (status, html) = get(&f.web(), "/w/default/app/h/notes/plan.md").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    let positions: Vec<usize> = ids
        .iter()
        .rev()
        .map(|id| {
            html.find(&format!("data-version=\"{id}\""))
                .unwrap_or_else(|| panic!("version {id} missing"))
        })
        .collect();
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "{positions:?}");
    assert_eq!(html.matches("data-version=").count(), K + 1);
    assert!(html.contains(&format!("-revision {}", K - 1)));
    assert!(html.contains(&format!("+revision {K}")));

    // The page view links to its history. It reads the body from disk, so
    // this page goes through the wiki.
    f.wiki
        .write_page(ai_memory_wiki::WritePageRequest {
            workspace_id: scope.0,
            project_id: scope.1,
            path: PagePath::new("notes/on-disk.md").unwrap(),
            frontmatter: serde_json::json!({"kind": "fact"}),
            body: "on disk".into(),
            tier: Tier::Semantic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: ActorContext::anonymous(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
    let (status, page) = get(&f.web(), "/w/default/app/p/notes/on-disk.md").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(
        page.contains(
            "href=\"w/default/app/h/notes/on-disk.md\" class=\"text-blue-600 dark:text-blue-400 underline\">History</a>"
        ),
        "{page}"
    );
}

/// Invariant #16: two operators write the same page from the same base. The
/// later write holds `is_latest`, and the other stays in the history with its
/// author — the loser is reachable and diffable, not gone.
#[tokio::test]
async fn history_keeps_both_sides_of_a_concurrent_divergent_write() {
    let f = Fixture::new().await;
    let scope = f.scope("default", "app").await;
    let alice = f.user("alice").await;
    let carol = f.user("carol").await;
    let base = f.write(scope, "notes/shared.md", "base text\n", None).await;
    let (a, c) = tokio::join!(
        f.write(scope, "notes/shared.md", "alice's text\n", Some(alice)),
        f.write(scope, "notes/shared.md", "carol's text\n", Some(carol)),
    );

    let json = get_json(
        &f.api(),
        "/workspaces/default/projects/app/history/notes/shared.md",
    )
    .await;
    assert_eq!(json["total"], 3);
    let versions = json["versions"].as_array().unwrap();
    let ids: Vec<&str> = versions.iter().map(|v| v["id"].as_str().unwrap()).collect();
    for id in [base, a, c] {
        assert!(
            ids.contains(&id.to_string().as_str()),
            "{id} missing: {ids:?}"
        );
    }
    let authors: Vec<&str> = versions
        .iter()
        .filter_map(|v| v["author"]["username"].as_str())
        .collect();
    assert!(
        authors.contains(&"alice") && authors.contains(&"carol"),
        "{authors:?}"
    );
    assert_eq!(
        versions.iter().filter(|v| v["is_latest"] == true).count(),
        1,
        "exactly one live version"
    );

    // Diff the two writers' versions against each other, by id.
    let (winner, loser) = if versions[0]["id"] == a.to_string() {
        (a, c)
    } else {
        (c, a)
    };
    let json = get_json(
        &f.api(),
        &format!(
            "/workspaces/default/projects/app/history/notes/shared.md?from={loser}&to={winner}"
        ),
    )
    .await;
    let rendered = serde_json::to_string(&json["diff"]["hunks"]).unwrap();
    assert!(rendered.contains("alice's text") && rendered.contains("carol's text"));

    let (status, html) = get(&f.web(), "/w/default/app/h/notes/shared.md").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("alice") && html.contains("carol"), "{html}");
    assert_eq!(html.matches("data-version=").count(), 3);
}

/// A path, a workspace and a raw version id from elsewhere all read as not
/// found; the history of a path never mixes in another project's rows at the
/// same path.
#[tokio::test]
async fn history_refuses_foreign_project_workspace_and_version_ids() {
    let f = Fixture::new().await;
    let home = f.scope("default", "home").await;
    let other = f.scope("default", "other").await;
    let far = f.scope("elsewhere", "home").await;
    let home_v1 = f.write(home, "notes/same.md", "home one\n", None).await;
    let home_v2 = f.write(home, "notes/same.md", "home two\n", None).await;
    let foreign = f
        .write(other, "notes/same.md", "OTHER-PROJECT-SECRET\n", None)
        .await;
    f.write(other, "notes/only-other.md", "OTHER-ONLY-SECRET\n", None)
        .await;
    let far_id = f
        .write(far, "notes/same.md", "OTHER-WORKSPACE-SECRET\n", None)
        .await;
    f.write(far, "notes/only-far.md", "FAR-ONLY-SECRET\n", None)
        .await;

    let web = f.web();
    let api = f.api();
    // Control: the home history is readable and holds home's two rows only.
    let json = get_json(
        &api,
        "/workspaces/default/projects/home/history/notes/same.md",
    )
    .await;
    assert_eq!(json["total"], 2, "{json}");
    let listed = json.to_string();
    assert!(listed.contains(&home_v1.to_string()) && listed.contains(&home_v2.to_string()));
    assert!(!listed.contains(&foreign.to_string()) && !listed.contains(&far_id.to_string()));
    let (status, html) = get(&web, "/w/default/home/h/notes/same.md").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!html.contains("SECRET"), "{html}");
    // Control: an explicit home pair diffs.
    let (status, _) = get(
        &web,
        &format!("/w/default/home/h/notes/same.md?from={home_v1}&to={home_v2}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let refused = [
        // Another project's path, under the home project.
        "/w/default/home/h/notes/only-other.md".to_owned(),
        // Another workspace's path, under the home workspace.
        "/w/default/home/h/notes/only-far.md".to_owned(),
        // A project or workspace that does not exist.
        "/w/default/missing/h/notes/same.md".to_owned(),
        "/w/nowhere/home/h/notes/same.md".to_owned(),
        // A raw version id from another project or workspace, as either side.
        format!("/w/default/home/h/notes/same.md?to={foreign}"),
        format!("/w/default/home/h/notes/same.md?from={foreign}"),
        format!("/w/default/home/h/notes/same.md?to={far_id}"),
        format!("/w/default/home/h/notes/same.md?from={far_id}&to={home_v2}"),
        // A home version id under a different path.
        format!("/w/default/home/h/notes/only-other.md?to={home_v2}"),
    ];
    for uri in &refused {
        let (status, body) = get(&web, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
        assert!(!body.contains("SECRET"), "{uri} leaked: {body}");
        let api_uri = api_history_uri(uri);
        let (status, body) = get(&api, &api_uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{api_uri}: {body}");
        assert!(!body.contains("SECRET"), "{api_uri} leaked: {body}");
    }

    // A malformed id is the caller's error, not a lookup.
    let (status, _) = get(&web, "/w/default/home/h/notes/same.md?to=not-a-uuid").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// A restricted project follows the ProjectAuthz read rule on both history
/// routes: a non-grantee is refused with a 403, a grantee and root read it.
#[tokio::test]
async fn history_and_timeline_honour_grants_in_a_restricted_project() {
    let f = Fixture::new().await;
    f.store
        .writer
        .set_new_project_mode(ai_memory_store::AccessMode::Restricted)
        .await
        .unwrap();
    let scope = f.scope("default", "client").await;
    f.write(scope, "secrets/rates.md", "rate one confidential\n", None)
        .await;
    f.write(scope, "secrets/rates.md", "rate two confidential\n", None)
        .await;
    let session = f
        .session(scope, None, &[("prompt", "confidential prompt")])
        .await;
    let alice = f.user("alice").await;
    let bob = f.user("bob").await;
    f.store
        .writer
        .grant_memory(alice, scope.1, GrantLevel::Read, None)
        .await
        .unwrap();

    let web = f.web();
    let api = f.api();
    let session_uri = format!("/w/default/client/s/{session}");
    for (app, uri) in [
        (&web, "/w/default/client/h/secrets/rates.md"),
        (
            &api,
            "/workspaces/default/projects/client/history/secrets/rates.md",
        ),
        (&web, session_uri.as_str()),
    ] {
        let (status, body) = get_as(app, uri, Some(bob), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
        assert!(!body.contains("confidential"), "{uri} leaked: {body}");
        let (status, body) = get_as(app, uri, Some(alice), None).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert!(body.contains("confidential"), "{uri}: {body}");
        let (status, _) = get_as(app, uri, None, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
    }
}

/// The version listing and the diff are both bounded: `limit` clamps and
/// pages, and a version past the diff's input cap renders as too large
/// instead of being diffed.
#[tokio::test]
async fn history_bounds_the_version_count_and_the_diff_size() {
    let f = Fixture::new().await;
    let scope = f.scope("default", "app").await;
    let mut ids = Vec::new();
    for i in 0..5 {
        ids.push(f.write(scope, "notes/p.md", &format!("v{i}\n"), None).await);
    }
    let api = f.api();
    let json = get_json(
        &api,
        "/workspaces/default/projects/app/history/notes/p.md?limit=2&offset=2",
    )
    .await;
    assert_eq!(json["total"], 5);
    assert_eq!(json["limit"], 2);
    let listed: Vec<&str> = json["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap())
        .collect();
    assert_eq!(listed, [ids[2].to_string(), ids[1].to_string()]);
    // The default diff still compares the newest version, not the slice's.
    assert_eq!(json["diff"]["to"]["id"], ids[4].to_string());
    let json = get_json(
        &api,
        "/workspaces/default/projects/app/history/notes/p.md?limit=1000000",
    )
    .await;
    assert_eq!(json["limit"], ai_memory_store::PAGE_VERSIONS_MAX_LIMIT);
    let json = get_json(
        &api,
        "/workspaces/default/projects/app/history/notes/p.md?limit=0",
    )
    .await;
    assert_eq!(json["versions"].as_array().unwrap().len(), 1);
    // An offset past the end lists nothing and still diffs the newest pair.
    let json = get_json(
        &api,
        &format!(
            "/workspaces/default/projects/app/history/notes/p.md?offset={}",
            usize::MAX
        ),
    )
    .await;
    assert!(json["versions"].as_array().unwrap().is_empty());
    assert_eq!(json["diff"]["to"]["id"], ids[4].to_string());
    let (status, _) = get(
        &f.web(),
        &format!("/w/default/app/h/notes/p.md?offset={}", usize::MAX),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // HTML paging links.
    let (_, html) = get(&f.web(), "/w/default/app/h/notes/p.md?limit=2").await;
    assert_eq!(html.matches("data-version=").count(), 2);
    assert!(html.contains("Older versions") && !html.contains("Newer versions"));

    let huge = "line\n".repeat(200_000);
    f.write(scope, "notes/p.md", &huge, None).await;
    let json = get_json(&api, "/workspaces/default/projects/app/history/notes/p.md").await;
    assert_eq!(json["diff"]["status"], "too_large", "{}", json["diff"]);
    assert!(json["diff"]["hunks"].as_array().unwrap().is_empty());
    let (status, html) = get(&f.web(), "/w/default/app/h/notes/p.md").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("No diff shown"));
    assert!(
        html.len() < 200_000,
        "the huge body is not echoed: {}",
        html.len()
    );
}

/// The TTL promise: an expired page is hidden from normal reads until the
/// forget sweep deletes it, and its history is hidden the same way. A
/// version whose own TTL passed is neither listed nor diffable, and once the
/// live version expires the whole path reads as not found (the sweep deletes
/// every row there). Expired ids read as not found, like foreign ones.
#[tokio::test]
async fn history_hides_expired_versions_and_expired_pages() {
    let f = Fixture::new().await;
    let scope = f.scope("default", "app").await;
    let past = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(1);
    let future = jiff::Timestamp::now() + jiff::SignedDuration::from_hours(24);

    // A live page whose live version expired: its whole history is gone.
    let kept = f
        .write(scope, "notes/gone.md", "EXPIRED-PAGE-OLD\n", None)
        .await;
    let expired = f
        .write_expiring(
            scope,
            "notes/gone.md",
            "EXPIRED-PAGE-NEW\n",
            None,
            Some(past),
        )
        .await;
    // An expired superseded version under an unexpired live one.
    let old_expired = f
        .write_expiring(
            scope,
            "notes/mixed.md",
            "EXPIRED-VERSION\n",
            None,
            Some(past),
        )
        .await;
    let live = f.write(scope, "notes/mixed.md", "live text\n", None).await;
    // Control: a TTL in the future is still visible.
    let not_yet = f
        .write_expiring(
            scope,
            "notes/later.md",
            "not yet expired\n",
            None,
            Some(future),
        )
        .await;

    let web = f.web();
    let api = f.api();
    for uri in [
        "/w/default/app/h/notes/gone.md".to_owned(),
        format!("/w/default/app/h/notes/gone.md?to={kept}"),
        format!("/w/default/app/h/notes/gone.md?to={expired}"),
        format!("/w/default/app/h/notes/mixed.md?from={old_expired}"),
        format!("/w/default/app/h/notes/mixed.md?to={old_expired}"),
    ] {
        for uri in [uri.clone(), api_history_uri(&uri)] {
            let app = if uri.starts_with("/w/") { &web } else { &api };
            let (status, body) = get(app, &uri).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
            assert!(!body.contains("EXPIRED"), "{uri} leaked: {body}");
        }
    }

    let json = get_json(
        &api,
        "/workspaces/default/projects/app/history/notes/mixed.md",
    )
    .await;
    assert_eq!(json["total"], 1, "{json}");
    assert_eq!(json["versions"][0]["id"], live.to_string());
    assert!(!json.to_string().contains(&old_expired.to_string()));
    // The live version's predecessor expired, so the default diff starts
    // from empty rather than exposing it.
    assert!(json["diff"]["from"].is_null(), "{}", json["diff"]);
    assert!(!json.to_string().contains("EXPIRED"));
    let (status, html) = get(&web, "/w/default/app/h/notes/mixed.md").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!html.contains("EXPIRED") && !html.contains(&old_expired.to_string()));

    let json = get_json(
        &api,
        "/workspaces/default/projects/app/history/notes/later.md",
    )
    .await;
    assert_eq!(json["versions"][0]["id"], not_yet.to_string());
    let (status, _) = get(
        &web,
        &format!("/w/default/app/h/notes/later.md?to={not_yet}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// Page text in a diff is escaped: a line of markup renders as text.
#[tokio::test]
async fn history_diff_renders_page_markup_inert() {
    let f = Fixture::new().await;
    let scope = f.scope("default", "app").await;
    f.write(scope, "notes/x.md", "safe\n", None).await;
    f.write(
        scope,
        "notes/x.md",
        "safe\n<script>alert('diff')</script>\n\"><img src=x onerror=alert(1)>\n",
        None,
    )
    .await;
    let (status, html) = get(&f.web(), "/w/default/app/h/notes/x.md").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!html.contains("<script>alert('diff')"), "{html}");
    assert!(!html.contains("<img src=x"), "{html}");
    assert!(html.contains("&lt;script&gt;"), "{html}");
}

// ---------------------------------------------------------------------------
// V2: session timeline
// ---------------------------------------------------------------------------

/// A 500-observation session renders a bounded page at a time, in capture
/// order, with working prev/next/jump; every render stays under a time bound.
#[tokio::test]
async fn session_timeline_pages_a_500_observation_session() {
    let f = Fixture::new().await;
    let scope = f.scope("default", "app").await;
    let rows: Vec<(String, String)> = (1..=500)
        .map(|i| (format!("step {i}"), format!("body of observation {i}")))
        .collect();
    let borrowed: Vec<(&str, &str)> = rows.iter().map(|(t, b)| (t.as_str(), b.as_str())).collect();
    let session = f.session(scope, None, &borrowed).await;
    let web = f.web();
    let base = format!("/w/default/app/s/{session}");

    let mut timings = Vec::new();
    let render = |uri: String| {
        let web = web.clone();
        async move {
            let started = std::time::Instant::now();
            let (status, html) = get(&web, &uri).await;
            (status, html, started.elapsed())
        }
    };

    let (status, html, took) = render(base.clone()).await;
    timings.push(took);
    assert_eq!(status, StatusCode::OK, "{html}");
    assert_eq!(
        html.matches("<li id=\"obs-").count(),
        50,
        "default page size"
    );
    assert!(html.contains("id=\"obs-1\"") && html.contains("id=\"obs-50\""));
    assert!(!html.contains("id=\"obs-51\""));
    assert!(html.find("step 1<").unwrap() < html.find("step 2<").unwrap());
    assert!(html.contains("1–50 of 500"));
    assert!(html.contains("offset=50&amp;limit=50"), "next link");
    assert!(html.contains("offset=450&amp;limit=50"), "last link");

    // Jump to an observation: the page that holds it.
    let (_, html, took) = render(format!("{base}?at=480")).await;
    timings.push(took);
    assert!(html.contains("id=\"obs-451\"") && html.contains("id=\"obs-480\""));
    assert!(html.contains("id=\"obs-500\"") && !html.contains("id=\"obs-450\""));
    // Out-of-range jumps clamp.
    let (_, html, _) = render(format!("{base}?at=99999")).await;
    assert!(html.contains("id=\"obs-500\""));
    let (_, html, _) = render(format!("{base}?at=0")).await;
    assert!(html.contains("id=\"obs-1\""));

    // A huge limit clamps to the API's maximum page size.
    let (_, html, took) = render(format!("{base}?limit=100000")).await;
    timings.push(took);
    assert_eq!(html.matches("<li id=\"obs-").count(), 200);

    // Walk every default page: ten pages, 500 rows, none twice.
    let mut seen = 0;
    for page in 0..10 {
        let (status, html, took) = render(format!("{base}?offset={}", page * 50)).await;
        timings.push(took);
        assert_eq!(status, StatusCode::OK);
        seen += html.matches("<li id=\"obs-").count();
    }
    assert_eq!(seen, 500);
    // Past the end is an empty page, not an error.
    for offset in ["10000", &usize::MAX.to_string()] {
        let (status, html, _) = render(format!("{base}?offset={offset}")).await;
        assert_eq!(status, StatusCode::OK, "{offset}");
        assert_eq!(html.matches("<li id=\"obs-").count(), 0);
    }

    timings.sort();
    let p95 = timings[(timings.len() * 95).div_ceil(100) - 1];
    assert!(
        p95 < std::time::Duration::from_secs(2),
        "session page render p95 {p95:?}: {timings:?}"
    );
}

/// "Guilty until proven": a raw session id from another project or another
/// workspace reads as not found under the home project, and so does an owned
/// session for a different operator. Controls: the home session, and the
/// owner reading their own.
#[tokio::test]
async fn session_timeline_refuses_foreign_session_ids() {
    let f = Fixture::new().await;
    let home = f.scope("default", "home").await;
    let other = f.scope("default", "other").await;
    let far = f.scope("elsewhere", "home").await;
    let own = f.session(home, None, &[("home", "home prompt")]).await;
    let foreign = f
        .session(other, None, &[("x", "OTHER-PROJECT-PROMPT")])
        .await;
    let far_id = f
        .session(far, None, &[("x", "OTHER-WORKSPACE-PROMPT")])
        .await;
    let owned = f
        .session(home, Some("alice"), &[("mine", "ALICE-ONLY-PROMPT")])
        .await;
    let web = f.web();

    let (status, body) = get(&web, &format!("/w/default/home/s/{own}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("home prompt"));

    for uri in [
        format!("/w/default/home/s/{foreign}"),
        format!("/w/default/home/s/{far_id}"),
        format!("/w/default/home/s/{}", SessionId::new()),
        format!("/w/elsewhere/other/s/{foreign}"),
    ] {
        let (status, body) = get(&web, &uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
        assert!(!body.contains("PROMPT"), "{uri} leaked: {body}");
    }
    let (status, _) = get(&web, "/w/default/home/s/not-a-session").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let owned_uri = format!("/w/default/home/s/{owned}");
    let (status, body) = get_as(&web, &owned_uri, None, Some("bob")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(!body.contains("ALICE-ONLY"));
    let (status, body) = get_as(&web, &owned_uri, None, Some("alice")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("ALICE-ONLY-PROMPT"));

    // The project page lists the visible sessions and links to them, never
    // another operator's.
    let (_, project) = get_as(&web, "/w/default/home", None, Some("bob")).await;
    let link = |id: SessionId| format!("href=\"w/default/home/s/{id}\"");
    assert!(project.contains(&link(own)), "{project}");
    assert!(!project.contains(&link(owned)), "{project}");
    assert!(!project.contains(&link(foreign)));
    let (_, project) = get_as(&web, "/w/default/home", None, Some("alice")).await;
    assert!(project.contains(&link(owned)), "{project}");
}

/// Captured text is untrusted: markup and attribute-injection payloads in an
/// observation's title and body render as inert text.
#[tokio::test]
async fn session_timeline_renders_captured_markup_inert() {
    let f = Fixture::new().await;
    let scope = f.scope("default", "app").await;
    let title = "<script>alert('title')</script>";
    let body =
        "<script>alert('body')</script>\n\"><img src=x onerror=alert(1)>\n' onmouseover='alert(2)";
    let session = f.session(scope, None, &[(title, body)]).await;
    // The payload must survive ingest, or this test proves nothing.
    let stored = f
        .store
        .reader
        .observations_for_session(session)
        .await
        .unwrap();
    assert!(stored[0].body.contains("<script>"), "{:?}", stored[0].body);
    assert!(
        stored[0].title.contains("<script>"),
        "{:?}",
        stored[0].title
    );

    let (status, html) = get(&f.web(), &format!("/w/default/app/s/{session}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!html.contains("<script>alert("), "{html}");
    assert!(!html.contains("<img src=x"), "{html}");
    assert!(!html.contains("' onmouseover='"), "{html}");
    assert!(!html.contains("\"><img src=x onerror="), "{html}");
    assert!(html.contains("&lt;script&gt;alert("), "{html}");
}

/// The session timeline's header search stays inside the session's project.
#[tokio::test]
async fn session_timeline_header_search_carries_the_project() {
    let f = Fixture::new().await;
    let scope = f.scope("default", "app").await;
    let session = f.session(scope, None, &[("step", "body")]).await;
    let (status, html) = get(&f.web(), &format!("/w/default/app/s/{session}")).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    let header = html.split("<main").next().unwrap();
    assert!(
        header.contains(r#"<input type="hidden" name="workspace" value="default">"#),
        "{header}"
    );
    assert!(
        header.contains(r#"<input type="hidden" name="project" value="app">"#),
        "{header}"
    );
    assert!(
        header.contains(r#"placeholder="Search default/app…""#),
        "{header}"
    );
}
