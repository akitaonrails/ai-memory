//! Routing a capture by repository identity (#708).
//!
//! A project name comes from a folder, and folder names collide: two unrelated
//! repositories both checked out as `api/` would share one project, and so one
//! grant. `resolve_project_by_identity` routes by the identity the client
//! resolved instead. These pin each way it can answer, and the two properties
//! that make it safe to switch on for installs that already hold data: an
//! existing project is claimed in place rather than split away, and somebody
//! who may not write to it cannot take its identity.

use ai_memory_core::repository_identity::{
    IdentitySource, IdentityStyle, MarkerAliases, RepositoryIdentity,
};
use ai_memory_core::{
    AgentKind, NewHandoff, NewPage, NewSession, NewUser, OwnerFilter, PagePath, ProjectId,
    SessionId, Tier, UserId, WorkspaceId,
};
use ai_memory_store::{
    AccessMode, GrantLevel, IdentityResolution, ProjectAccess, ProjectCoordinateCollisionReason,
    ProjectCoordinateStatus, ProjectPrincipal, ScopeName, ScopeResolutionError, Store,
    create_explicit_scope_guarded, lookup_existing_scope, lookup_existing_scope_guarded,
    resolve_many_existing_scopes,
};

fn remote(identity: &str) -> RepositoryIdentity {
    RepositoryIdentity {
        identity: identity.to_owned(),
        source: IdentitySource::GitRemote,
    }
}

async fn workspace(store: &Store) -> WorkspaceId {
    store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap()
}

async fn user(store: &Store, name: &str) -> UserId {
    store
        .writer
        .create_human_user(
            NewUser {
                username: name.to_owned(),
                name: None,
                email: None,
            },
            ai_memory_core::UserRole::User,
            None,
            false,
        )
        .await
        .unwrap()
}

/// `(name, identity, identity_source, repo_path)` straight from the row.
fn row(store: &Store, id: ProjectId) -> (String, String, String, Option<String>) {
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    conn.query_row(
        "SELECT name, identity, identity_source, repo_path FROM projects WHERE id = ?1",
        [id.as_bytes().to_vec()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .unwrap()
}

async fn resolve(
    store: &Store,
    ws: WorkspaceId,
    identity: &RepositoryIdentity,
    name: &str,
    creator: Option<UserId>,
) -> (ProjectId, IdentityResolution) {
    resolve_styled(store, ws, identity, IdentityStyle::HostPath, name, creator).await
}

async fn resolve_styled(
    store: &Store,
    ws: WorkspaceId,
    identity: &RepositoryIdentity,
    style: IdentityStyle,
    name: &str,
    creator: Option<UserId>,
) -> (ProjectId, IdentityResolution) {
    store
        .writer
        .resolve_project_by_identity(
            ws,
            identity.clone(),
            style,
            name,
            Some("/work/api".to_owned()),
            None,
            creator,
        )
        .await
        .unwrap()
}

async fn resolve_capture(
    store: &Store,
    ws: WorkspaceId,
    identity: &RepositoryIdentity,
    style: IdentityStyle,
    name: &str,
    creator: Option<UserId>,
) -> (ProjectId, IdentityResolution) {
    store
        .writer
        .resolve_project_by_identity_for_capture(
            ws,
            identity.clone(),
            style,
            name,
            Some("/work/api".to_owned()),
            None,
            creator,
        )
        .await
        .unwrap()
}

/// The collision the feature exists for: two unrelated `api` checkouts land
/// in two projects, the second named after its owner — and the same
/// repository, wherever it is checked out and whatever the folder is called,
/// lands back in its own.
#[tokio::test]
async fn two_unrelated_repositories_with_one_folder_name_stay_apart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;

    let (a, how) = resolve(&store, ws, &remote("github.com/orga/api"), "api", None).await;
    assert_eq!(how, IdentityResolution::Created);
    assert_eq!(
        row(&store, a),
        (
            "api".into(),
            "github.com/orga/api".into(),
            "git_remote".into(),
            Some("/work/api".into())
        )
    );

    let (b, how) = resolve(&store, ws, &remote("github.com/orgb/api"), "api", None).await;
    assert_eq!(how, IdentityResolution::Split);
    assert_ne!(a, b);
    let (name, identity, _, repo_path) = row(&store, b);
    assert_eq!(
        (name.as_str(), identity.as_str()),
        ("orgb-api", "github.com/orgb/api")
    );
    assert_eq!(
        repo_path, None,
        "a split project must not share the other's path"
    );

    // A third `api` whose owner-repo name is also taken gets a suffix.
    let (c, how) = resolve(&store, ws, &remote("gitlab.com/orgb/api"), "api", None).await;
    assert_eq!(how, IdentityResolution::Split);
    assert_eq!(row(&store, c).0, "orgb-api-2");

    // Same repository, another folder name: its own project, by identity.
    let (again, how) = resolve(
        &store,
        ws,
        &remote("github.com/orga/api"),
        "acme-api-clone",
        None,
    )
    .await;
    assert_eq!(how, IdentityResolution::Matched);
    assert_eq!(again, a);
}

/// An install upgrading with data: the project that already exists under the
/// folder name takes the identity in place, so its memory does not move.
#[tokio::test]
async fn an_existing_project_is_claimed_in_place() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let existing = store
        .writer
        .get_or_create_project(ws, "api", None)
        .await
        .unwrap();

    let (id, how) = resolve(&store, ws, &remote("github.com/orga/api"), "api", None).await;
    assert_eq!(how, IdentityResolution::Claimed);
    assert_eq!(id, existing);
    assert_eq!(row(&store, id).1, "github.com/orga/api");

    // Claimed identities are never overwritten: a different repository with
    // the same folder name splits instead of re-pointing this one.
    let (other, how) = resolve(&store, ws, &remote("github.com/orgb/api"), "api", None).await;
    assert_eq!(how, IdentityResolution::Split);
    assert_ne!(other, existing);
    assert_eq!(row(&store, existing).1, "github.com/orga/api");
}

/// With authorization on, claiming is a write. A user with no grant on the
/// unclaimed project must neither claim it nor split off a project carrying
/// its identity — either would let the first outsider after an upgrade own
/// the repository the team has been working in. They get the project back
/// unclaimed, for their grant check to refuse.
#[tokio::test]
async fn an_outsider_cannot_take_an_unclaimed_projects_identity() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let team_project = store
        .writer
        .get_or_create_project(ws, "api", None)
        .await
        .unwrap();
    // An open project admits everyone, outsider included; the question only
    // arises for a restricted one.
    store
        .writer
        .set_access_mode(team_project, ai_memory_store::AccessMode::Restricted)
        .await
        .unwrap();
    let member = user(&store, "member").await;
    let outsider = user(&store, "outsider").await;
    store
        .writer
        .grant_memory(member, team_project, GrantLevel::Write, None)
        .await
        .unwrap();

    let (id, how) = resolve(
        &store,
        ws,
        &remote("github.com/orga/api"),
        "api",
        Some(outsider),
    )
    .await;
    assert_eq!(how, IdentityResolution::Unclaimed);
    assert_eq!(id, team_project);
    assert_eq!(
        row(&store, team_project).1,
        "",
        "the outsider claimed nothing"
    );
    assert!(
        store
            .reader
            .grants_for(outsider, team_project)
            .await
            .unwrap()
            .is_empty()
    );

    let (id, how) = resolve(
        &store,
        ws,
        &remote("github.com/orga/api"),
        "api",
        Some(member),
    )
    .await;
    assert_eq!(how, IdentityResolution::Claimed);
    assert_eq!(id, team_project);
}

/// A project this call creates — directly or by splitting — records its
/// creator, as every other creation path does, and the choke point admits them
/// to it without a grant while refusing the other user.
#[tokio::test]
async fn a_created_or_split_project_admits_its_creator() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    store
        .writer
        .set_new_project_mode(AccessMode::Restricted)
        .await
        .unwrap();
    let ws = workspace(&store).await;
    let alice = user(&store, "alice").await;
    let bob = user(&store, "bob").await;

    let (a, how) = resolve(
        &store,
        ws,
        &remote("github.com/orga/api"),
        "api",
        Some(alice),
    )
    .await;
    assert_eq!(how, IdentityResolution::Created);
    let (b, how) = resolve(&store, ws, &remote("github.com/orgb/api"), "api", Some(bob)).await;
    assert_eq!(how, IdentityResolution::Split);

    let admits = |who, id| {
        let reader = store.reader.clone();
        async move {
            reader
                .authorize_project(
                    ws,
                    id,
                    ProjectPrincipal::user(who),
                    true,
                    ProjectAccess::Write,
                )
                .await
                .unwrap()
                .is_ok()
        }
    };
    for (who, own, other) in [(alice, a, b), (bob, b, a)] {
        assert!(
            store.reader.grants_for(who, own).await.unwrap().is_empty(),
            "the creator needs no grant"
        );
        assert!(admits(who, own).await, "the creator is admitted");
        assert!(
            !admits(who, other).await,
            "the other user's project refuses"
        );
    }
}

/// The cwd-prefix parent the router found is the candidate, not the name: a
/// capture from a subdirectory claims the repository it sits in.
#[tokio::test]
async fn the_prefix_parent_is_the_candidate_when_given() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let parent = store
        .writer
        .get_or_create_project(ws, "monorepo", Some("/work/monorepo".to_owned()))
        .await
        .unwrap();

    let (id, how) = store
        .writer
        .resolve_project_by_identity(
            ws,
            remote("github.com/acme/monorepo"),
            IdentityStyle::HostPath,
            "src",
            None,
            Some(parent),
            None,
        )
        .await
        .unwrap();
    assert_eq!(how, IdentityResolution::Claimed);
    assert_eq!(id, parent);
    assert!(
        store
            .reader
            .find_project(ws, "src".into())
            .await
            .unwrap()
            .is_none(),
        "no fragment project for the subdirectory"
    );
}

fn explicit(identity: &str) -> RepositoryIdentity {
    RepositoryIdentity {
        identity: identity.to_owned(),
        source: IdentitySource::Explicit,
    }
}

/// #1033: under the `path` style a new repository is named from its path
/// without the host, so every worktree and clone — whatever its folder is
/// called — lands in that one project.
#[tokio::test]
async fn the_path_style_names_a_new_project_from_the_host_less_path() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let repo = remote("gitlab.com/acme/group/api");

    let (id, how) = resolve_styled(&store, ws, &repo, IdentityStyle::Path, "main", None).await;
    assert_eq!(how, IdentityResolution::Created);
    assert_eq!(row(&store, id).0, "acme-group-api");
    assert_eq!(row(&store, id).1, "gitlab.com/acme/group/api");

    for folder in ["fix-1025", "main"] {
        let (again, how) =
            resolve_styled(&store, ws, &repo, IdentityStyle::Path, folder, None).await;
        assert_eq!((again, how), (id, IdentityResolution::Matched), "{folder}");
    }
    assert!(
        store
            .reader
            .find_project(ws, "main".into())
            .await
            .unwrap()
            .is_none(),
        "no folder-named project was created alongside it"
    );
}

/// The collision the path style must never merge: the same path on another
/// forge is a different repository. Whichever arrives second keeps the name it
/// would have had without the style — the folder name, or the split name when
/// that is taken too — and the first project is left untouched.
#[tokio::test]
async fn the_same_path_on_another_forge_falls_back_instead_of_merging() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let github = remote("github.com/acme/api");

    let (first, _) = resolve_styled(&store, ws, &github, IdentityStyle::Path, "api", None).await;
    assert_eq!(row(&store, first).0, "acme-api");

    let (gitlab, how) = resolve_styled(
        &store,
        ws,
        &remote("gitlab.com/acme/api"),
        IdentityStyle::Path,
        "api",
        None,
    )
    .await;
    assert_eq!(how, IdentityResolution::Created);
    assert_ne!(gitlab, first, "another forge's repository is never adopted");
    assert_eq!(row(&store, gitlab).0, "api");
    assert_eq!(row(&store, gitlab).1, "gitlab.com/acme/api");

    // Its folder is called `acme-api` too: that name belongs to the GitHub
    // project, so it splits, and the split name is taken as well.
    let (bitbucket, how) = resolve_styled(
        &store,
        ws,
        &remote("bitbucket.org/acme/api"),
        IdentityStyle::Path,
        "acme-api",
        None,
    )
    .await;
    assert_eq!(how, IdentityResolution::Split);
    assert_eq!(row(&store, bitbucket).0, "acme-api-2");

    assert_eq!(
        row(&store, first),
        (
            "acme-api".into(),
            "github.com/acme/api".into(),
            "git_remote".into(),
            Some("/work/api".into())
        ),
        "the first project kept its name, identity and path"
    );
}

/// A project already holding the path name without any identity is some other
/// checkout's folder project. Taking it would merge two repositories, so the
/// newcomer falls back and the holder stays unclaimed.
#[tokio::test]
async fn an_unclaimed_project_holding_the_path_name_is_not_taken() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let holder = store
        .writer
        .get_or_create_project(ws, "acme-api", None)
        .await
        .unwrap();

    let (id, how) = resolve_styled(
        &store,
        ws,
        &remote("github.com/acme/api"),
        IdentityStyle::Path,
        "api",
        None,
    )
    .await;
    assert_eq!(how, IdentityResolution::Created);
    assert_ne!(id, holder);
    assert_eq!(row(&store, id).0, "api");
    assert_eq!(row(&store, holder).1, "", "the holder was not claimed");
}

/// Opting an install in renames nothing: a repository that already has a
/// project — claimed by identity, or still unclaimed under its folder name —
/// resolves exactly as it would without the style.
#[tokio::test]
async fn opting_in_leaves_existing_projects_where_they_are() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;

    let claimed_repo = remote("github.com/acme/api");
    let (claimed, _) = resolve(&store, ws, &claimed_repo, "api", None).await;
    let (again, how) =
        resolve_styled(&store, ws, &claimed_repo, IdentityStyle::Path, "api", None).await;
    assert_eq!((again, how), (claimed, IdentityResolution::Matched));
    assert_eq!(row(&store, claimed).0, "api", "not renamed to acme-api");

    let legacy = store
        .writer
        .get_or_create_project(ws, "web", None)
        .await
        .unwrap();
    let (id, how) = resolve_styled(
        &store,
        ws,
        &remote("github.com/acme/web"),
        IdentityStyle::Path,
        "web",
        None,
    )
    .await;
    assert_eq!((id, how), (legacy, IdentityResolution::Claimed));
    assert_eq!(row(&store, legacy).0, "web");
    assert!(
        store
            .reader
            .find_project(ws, "acme-web".into())
            .await
            .unwrap()
            .is_none(),
        "no path-named twin split off the legacy project"
    );
}

#[tokio::test]
async fn old_client_omitted_style_stays_host_path_and_does_not_promote() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let repository = remote("github.com/acme/api");
    let project = resolve(&store, ws, &repository, "api", None).await.0;

    let (resolved, how) = resolve_capture(
        &store,
        ws,
        &repository,
        IdentityStyle::default(),
        "api-clone",
        None,
    )
    .await;

    assert_eq!(resolved, project);
    assert_eq!(how, IdentityResolution::Matched);
    assert_eq!(row(&store, project).0, "api");
}

#[tokio::test]
async fn explicit_path_capture_claims_and_promotes_an_identityless_legacy_name_in_one_transaction()
{
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let legacy = store
        .writer
        .get_or_create_project(ws, "api", None)
        .await
        .unwrap();

    let (project, how) = resolve_capture(
        &store,
        ws,
        &remote("github.com/acme/api"),
        IdentityStyle::Path,
        "api",
        None,
    )
    .await;

    assert_eq!(project, legacy);
    assert_eq!(how, IdentityResolution::Promoted);
    assert_eq!(row(&store, project).0, "acme-api");
    assert_eq!(row(&store, project).1, "github.com/acme/api");
}

#[tokio::test]
async fn explicit_path_capture_promotes_an_authorized_legacy_name_without_changing_uuid() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let repository = remote("github.com/acme/api");
    let project = resolve(&store, ws, &repository, "api", None).await.0;

    let (promoted, how) = resolve_capture(
        &store,
        ws,
        &repository,
        IdentityStyle::Path,
        "api-clone",
        None,
    )
    .await;

    assert_eq!(promoted, project);
    assert_eq!(how, IdentityResolution::Promoted);
    assert_eq!(row(&store, project).0, "acme-api");
}

#[tokio::test]
async fn unauthorized_capture_cannot_claim_or_promote_an_identityless_legacy_name() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let project = store
        .writer
        .get_or_create_project(ws, "api", None)
        .await
        .unwrap();
    store
        .writer
        .set_access_mode(project, AccessMode::Restricted)
        .await
        .unwrap();
    let outsider = user(&store, "claim-promotion-outsider").await;

    let (resolved, how) = resolve_capture(
        &store,
        ws,
        &remote("github.com/acme/api"),
        IdentityStyle::Path,
        "api",
        Some(outsider),
    )
    .await;

    assert_eq!(resolved, project);
    assert_eq!(how, IdentityResolution::Unclaimed);
    assert_eq!(row(&store, project).0, "api");
    assert_eq!(row(&store, project).1, "");
}

#[tokio::test]
async fn unauthorized_capture_resolves_but_cannot_promote_a_legacy_name() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let repository = remote("github.com/acme/api");
    let project = resolve(&store, ws, &repository, "api", None).await.0;
    store
        .writer
        .set_access_mode(project, AccessMode::Restricted)
        .await
        .unwrap();
    let outsider = user(&store, "promotion-outsider").await;

    let (resolved, how) = resolve_capture(
        &store,
        ws,
        &repository,
        IdentityStyle::Path,
        "api-clone",
        Some(outsider),
    )
    .await;

    assert_eq!(resolved, project);
    assert_eq!(how, IdentityResolution::Matched);
    assert_eq!(row(&store, project).0, "api");
}

/// The style only renames what has a host to drop: a declared identity keeps
/// the name the client sent.
#[tokio::test]
async fn the_path_style_leaves_a_declared_identity_named_as_sent() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let (id, how) = resolve_styled(
        &store,
        ws,
        &explicit("acme/platform"),
        IdentityStyle::Path,
        "platform",
        None,
    )
    .await;
    assert_eq!(how, IdentityResolution::Created);
    assert_eq!(row(&store, id).0, "platform");
}

fn preserved_page(ws: WorkspaceId, project: ProjectId) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: project,
        path: PagePath::new("notes/preserved.md").unwrap(),
        title: "Preserved".into(),
        body: "preserved page".into(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
    }
}

#[tokio::test]
async fn marker_aliases_require_same_remote_identity_and_preserve_uuid() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let repository = remote("github.com/acme/api");
    let project = resolve(&store, ws, &repository, "former-name", None)
        .await
        .0;
    store
        .writer
        .set_access_mode(project, AccessMode::Restricted)
        .await
        .unwrap();
    let writer = user(&store, "alias-writer").await;
    store
        .writer
        .grant_memory(writer, project, GrantLevel::Write, None)
        .await
        .unwrap();
    let aliases = MarkerAliases::new(["former-name"]).unwrap();

    let read = store
        .reader
        .resolve_existing_project_aliases(
            ws,
            "acme-api".into(),
            aliases.clone(),
            repository.clone(),
        )
        .await
        .unwrap();
    assert_eq!(read, Some(project));
    assert_eq!(
        row(&store, project).0,
        "former-name",
        "read does not rename"
    );

    let outsider = user(&store, "alias-outsider").await;
    let refused = store
        .writer
        .resolve_project_aliases_for_write(
            ws,
            "acme-api",
            aliases.clone(),
            repository.clone(),
            Some(ProjectPrincipal::user(outsider)),
            true,
        )
        .await;
    assert!(matches!(
        refused,
        Err(ai_memory_store::StoreError::Forbidden(_))
    ));
    assert_eq!(row(&store, project).0, "former-name");

    let promoted = store
        .writer
        .resolve_project_aliases_for_write(
            ws,
            "acme-api",
            aliases,
            repository,
            Some(ProjectPrincipal::user(writer)),
            true,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(promoted.project_id, project);
    assert_eq!(promoted.promoted_from.as_deref(), Some("former-name"));
    assert_eq!(row(&store, project).0, "acme-api");
}

#[tokio::test]
async fn marker_aliases_cannot_promote_to_an_arbitrary_marker_project() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let repository = remote("github.com/acme/api");
    let project = resolve(&store, ws, &repository, "former-name", None)
        .await
        .0;
    let aliases = MarkerAliases::new(["former-name"]).unwrap();

    let resolved = store
        .writer
        .resolve_project_aliases_for_write(
            ws,
            "attacker-chosen-name",
            aliases,
            repository,
            None,
            false,
        )
        .await
        .unwrap()
        .unwrap();

    assert_eq!(resolved.project_id, project);
    assert_eq!(resolved.promoted_from, None);
    assert_eq!(row(&store, project).0, "former-name");
}

#[tokio::test]
async fn hostile_ambiguous_and_cross_workspace_marker_aliases_fail_closed() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let repository = remote("github.com/acme/api");
    let legitimate = resolve(&store, ws, &repository, "former-name", None)
        .await
        .0;
    let hostile = resolve(
        &store,
        ws,
        &remote("gitlab.com/other/private"),
        "restricted",
        None,
    )
    .await
    .0;
    store
        .writer
        .set_access_mode(hostile, AccessMode::Restricted)
        .await
        .unwrap();
    let open_hostile = resolve(
        &store,
        ws,
        &remote("bitbucket.org/other/public"),
        "open-project",
        None,
    )
    .await
    .0;
    assert_ne!(open_hostile, legitimate);

    assert_eq!(
        store
            .reader
            .resolve_existing_project_aliases(
                ws,
                "acme-api".into(),
                MarkerAliases::new(["former-name"]).unwrap(),
                repository.clone(),
            )
            .await
            .unwrap(),
        Some(legitimate)
    );
    for hostile_name in ["restricted", "open-project"] {
        assert!(
            store
                .reader
                .resolve_existing_project_aliases(
                    ws,
                    "future-api".into(),
                    MarkerAliases::new([hostile_name]).unwrap(),
                    repository.clone(),
                )
                .await
                .is_err(),
            "{hostile_name}"
        );
    }
    assert!(
        store
            .reader
            .resolve_existing_project_aliases(
                ws,
                "future-api".into(),
                MarkerAliases::new(["former-name", "restricted"]).unwrap(),
                repository.clone(),
            )
            .await
            .is_err()
    );

    let other_ws = store.writer.get_or_create_workspace("other").await.unwrap();
    let _foreign = resolve(&store, other_ws, &repository, "foreign-only", None).await;
    assert_eq!(
        store
            .reader
            .resolve_existing_project_aliases(
                ws,
                "missing-canonical".into(),
                MarkerAliases::new(["foreign-only"]).unwrap(),
                remote("github.com/acme/else"),
            )
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn coordinate_diagnostic_classifies_exact_canonical_legacy_and_missing_without_mutation() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let repository = remote("github.com/acme/api");
    let project = resolve(&store, ws, &repository, "checkout-blue", None)
        .await
        .0;
    let before: (i64, String, i64) = rusqlite::Connection::open(store.db_path())
        .unwrap()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM projects), name, \
             (SELECT COUNT(*) FROM audit_log) FROM projects WHERE id = ?1",
            [project.as_bytes().to_vec()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();

    for (name, status, rename_eligible) in [
        ("checkout-blue", ProjectCoordinateStatus::Exact, false),
        ("acme-api", ProjectCoordinateStatus::CanonicalCompat, true),
        ("api", ProjectCoordinateStatus::LegacyCompat, false),
    ] {
        let diagnostic = store
            .reader
            .diagnose_project_coordinate(
                "default".into(),
                name.into(),
                Some(repository.clone()),
                IdentityStyle::Path,
            )
            .await
            .unwrap();
        assert_eq!(diagnostic.status, status, "{name}");
        assert_eq!(diagnostic.project_id, Some(project));
        assert_eq!(diagnostic.current_name.as_deref(), Some("checkout-blue"));
        assert_eq!(diagnostic.canonical_candidate.as_deref(), Some("acme-api"));
        assert_eq!(diagnostic.legacy_candidate.as_deref(), Some("api"));
        assert_eq!(diagnostic.identity_source.as_deref(), Some("git_remote"));
        assert_eq!(diagnostic.rename_eligible, rename_eligible);
    }
    let no_identity = store
        .reader
        .diagnose_project_coordinate(
            "default".into(),
            "acme-api".into(),
            None,
            IdentityStyle::HostPath,
        )
        .await
        .unwrap();
    assert_eq!(no_identity.status, ProjectCoordinateStatus::CanonicalCompat);
    assert_eq!(no_identity.project_id, Some(project));
    assert!(no_identity.rename_eligible);
    assert_eq!(no_identity.identity_style, None);

    let missing = store
        .reader
        .diagnose_project_coordinate(
            "default".into(),
            "unknown".into(),
            Some(repository),
            IdentityStyle::Path,
        )
        .await
        .unwrap();
    assert_eq!(missing.status, ProjectCoordinateStatus::Missing);
    assert_eq!(missing.project_id, Some(project));
    assert_eq!(missing.current_name.as_deref(), Some("checkout-blue"));
    assert_eq!(missing.canonical_candidate.as_deref(), Some("acme-api"));
    assert!(!missing.rename_eligible);

    let after: (i64, String, i64) = rusqlite::Connection::open(store.db_path())
        .unwrap()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM projects), name, \
             (SELECT COUNT(*) FROM audit_log) FROM projects WHERE id = ?1",
            [project.as_bytes().to_vec()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(after, before);
}

#[tokio::test]
async fn coordinate_diagnostic_reports_cross_forge_and_identityless_collisions() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let _github = resolve(
        &store,
        ws,
        &remote("github.com/acme/api"),
        "github-api",
        None,
    )
    .await
    .0;
    let gitlab = resolve(
        &store,
        ws,
        &remote("gitlab.com/acme/api"),
        "gitlab-api",
        None,
    )
    .await
    .0;
    let ambiguous = store
        .reader
        .diagnose_project_coordinate(
            "default".into(),
            "acme-api".into(),
            Some(remote("gitlab.com/acme/api")),
            IdentityStyle::Path,
        )
        .await
        .unwrap();
    assert_eq!(ambiguous.status, ProjectCoordinateStatus::Ambiguous);
    assert_eq!(
        ambiguous.collision_reason,
        Some(ProjectCoordinateCollisionReason::CrossForgeCollision)
    );
    assert_eq!(ambiguous.candidate_count, 2);
    assert_eq!(ambiguous.project_id, Some(gitlab));

    let identityless = store
        .writer
        .get_or_create_project(ws, "occupied", None)
        .await
        .unwrap();
    let identity_backed = resolve(
        &store,
        ws,
        &remote("github.com/acme/occupied"),
        "elsewhere",
        None,
    )
    .await
    .0;
    let occupied = store
        .reader
        .diagnose_project_coordinate(
            "default".into(),
            "occupied".into(),
            Some(remote("github.com/acme/occupied")),
            IdentityStyle::Path,
        )
        .await
        .unwrap();
    assert_eq!(occupied.status, ProjectCoordinateStatus::Ambiguous);
    assert_eq!(
        occupied.collision_reason,
        Some(ProjectCoordinateCollisionReason::UnclaimedIdentitylessProject)
    );
    assert_eq!(occupied.candidate_count, 2);
    assert_eq!(occupied.project_id, Some(identity_backed));
    assert_ne!(identityless, identity_backed);

    let unclaimed_only = store
        .writer
        .get_or_create_project(ws, "unclaimed-only", None)
        .await
        .unwrap();
    let unclaimed = store
        .reader
        .diagnose_project_coordinate(
            "default".into(),
            "unclaimed-only".into(),
            Some(remote("github.com/acme/unclaimed-only")),
            IdentityStyle::Path,
        )
        .await
        .unwrap();
    assert_eq!(unclaimed.status, ProjectCoordinateStatus::Ambiguous);
    assert_eq!(unclaimed.project_id, Some(unclaimed_only));
    assert_eq!(
        unclaimed.collision_reason,
        Some(ProjectCoordinateCollisionReason::UnclaimedIdentitylessProject)
    );
    assert!(!unclaimed.rename_eligible);

    let holder = resolve(
        &store,
        ws,
        &explicit("another/repository"),
        "acme-target-taken",
        None,
    )
    .await
    .0;
    let intended = resolve(
        &store,
        ws,
        &remote("github.com/acme/target-taken"),
        "legacy-target",
        None,
    )
    .await
    .0;
    let occupied = store
        .reader
        .diagnose_project_coordinate(
            "default".into(),
            "acme-target-taken".into(),
            Some(remote("github.com/acme/target-taken")),
            IdentityStyle::Path,
        )
        .await
        .unwrap();
    assert_eq!(occupied.status, ProjectCoordinateStatus::Ambiguous);
    assert_eq!(occupied.project_id, Some(intended));
    assert_eq!(
        occupied.collision_reason,
        Some(ProjectCoordinateCollisionReason::CanonicalTargetOccupied)
    );
    assert_eq!(occupied.candidate_count, 2);
    assert_ne!(holder, intended);

    let foreign = resolve(
        &store,
        ws,
        &remote("github.com/other/occupied-name"),
        "occupied-name",
        None,
    )
    .await
    .0;
    let foreign_occupied = store
        .reader
        .diagnose_project_coordinate(
            "default".into(),
            "occupied-name".into(),
            Some(remote("gitlab.com/acme/unrelated")),
            IdentityStyle::Path,
        )
        .await
        .unwrap();
    assert_eq!(foreign_occupied.status, ProjectCoordinateStatus::Ambiguous);
    assert_eq!(foreign_occupied.project_id, Some(foreign));
    assert_eq!(
        foreign_occupied.collision_reason,
        Some(ProjectCoordinateCollisionReason::CrossIdentityOccupied)
    );
    assert!(!foreign_occupied.rename_eligible);
}

#[tokio::test]
async fn canonical_and_legacy_reads_resolve_one_legacy_row_without_renaming() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let project = resolve(&store, ws, &remote("github.com/acme/api"), "api", None)
        .await
        .0;
    for name in ["api", "acme-api"] {
        assert_eq!(
            lookup_existing_scope(&store.reader, "default", name)
                .await
                .unwrap()
                .project_id,
            project
        );
    }
    assert_eq!(row(&store, project).0, "api");

    let deduplicated = resolve_many_existing_scopes(
        &store.reader,
        &[
            ScopeName::new("default", "api"),
            ScopeName::new("default", "acme-api"),
        ],
        10,
    )
    .await
    .unwrap();
    assert_eq!(deduplicated.len(), 1);
    assert_eq!(deduplicated[0].project_id, project);
}

#[tokio::test]
async fn authorized_write_promotes_in_place_and_preserves_dependents_and_legacy_lookup() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let owner = user(&store, "promotion-owner").await;
    let project = resolve(
        &store,
        ws,
        &remote("github.com/acme/api"),
        "api",
        Some(owner),
    )
    .await
    .0;
    store
        .writer
        .upsert_page(preserved_page(ws, project))
        .await
        .unwrap();
    let session = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id: session,
            workspace_id: ws,
            project_id: project,
            agent_kind: AgentKind::Codex,
            cwd: Some("/clone/api".into()),
            actor_user: None,
            origin: None,
        })
        .await
        .unwrap();
    store
        .writer
        .grant_memory(owner, project, GrantLevel::Write, None)
        .await
        .unwrap();
    store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: project,
            from_session_id: Some(session),
            from_agent: AgentKind::Codex,
            to_agent: None,
            cwd: Some("/clone/api".into()),
            summary: "continue".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        })
        .await
        .unwrap();

    let promoted = create_explicit_scope_guarded(
        &store.reader,
        &store.writer,
        "default",
        "acme-api",
        Some(owner),
    )
    .await
    .unwrap();
    assert_eq!(promoted.scope.project_id, project);
    assert_eq!(row(&store, project).0, "acme-api");
    let audit: (i64, Option<Vec<u8>>, String) = rusqlite::Connection::open(store.db_path())
        .unwrap()
        .query_row(
            "SELECT COUNT(*), MAX(author_id), MAX(detail) FROM audit_log \
             WHERE op = 'promote_project_name' AND project_id = ?1",
            [project.as_bytes().to_vec()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(audit.0, 1);
    assert_eq!(audit.1.as_deref(), Some(owner.as_bytes().as_slice()));
    assert!(audit.2.contains("\"from\":\"api\""));
    assert!(audit.2.contains("\"to\":\"acme-api\""));
    assert!(
        store
            .reader
            .page_body_by_ids(ws, project, "notes/preserved.md")
            .await
            .unwrap()
            .is_some()
    );
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    for table in ["sessions", "project_grants", "handoffs"] {
        let count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE project_id = ?1"),
                [project.as_bytes().to_vec()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "{table}");
    }
    assert_eq!(
        store
            .reader
            .list_handoffs(ws, project, None, OwnerFilter::Any, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        lookup_existing_scope(&store.reader, "default", "api")
            .await
            .unwrap()
            .project_id,
        project
    );
}

#[tokio::test]
async fn distinct_current_canonical_and_legacy_keys_resolve_without_guessing_checkout_names() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let project = resolve(
        &store,
        ws,
        &remote("gitlab.com/acme/group/api"),
        "checkout-blue",
        None,
    )
    .await
    .0;
    for key in ["checkout-blue", "acme-group-api", "api"] {
        assert_eq!(
            lookup_existing_scope(&store.reader, "default", key)
                .await
                .unwrap()
                .project_id,
            project
        );
    }
    assert_eq!(row(&store, project).0, "checkout-blue");
    for key in ["group-api", "checkout-red"] {
        assert!(
            lookup_existing_scope(&store.reader, "default", key)
                .await
                .unwrap_err()
                .is_not_found()
        );
    }
    let foreign = store
        .writer
        .get_or_create_workspace("foreign")
        .await
        .unwrap();
    for key in ["checkout-blue", "acme-group-api", "api"] {
        assert!(
            lookup_existing_scope(&store.reader, "foreign", key)
                .await
                .unwrap_err()
                .is_not_found()
        );
        assert!(
            store
                .reader
                .find_project(foreign, key.to_owned())
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn read_only_user_cannot_promote_a_legacy_name() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let owner = user(&store, "promotion-owner").await;
    let reader = user(&store, "promotion-reader").await;
    let project = resolve(
        &store,
        ws,
        &remote("github.com/acme/api"),
        "api",
        Some(owner),
    )
    .await
    .0;
    store
        .writer
        .set_access_mode(project, AccessMode::Restricted)
        .await
        .unwrap();
    store
        .writer
        .grant_memory(reader, project, GrantLevel::Read, None)
        .await
        .unwrap();
    assert_eq!(
        lookup_existing_scope_guarded(
            &store.reader,
            "default",
            "acme-api",
            Some(reader),
            ProjectAccess::Read,
        )
        .await
        .unwrap()
        .project_id,
        project
    );
    assert!(
        create_explicit_scope_guarded(
            &store.reader,
            &store.writer,
            "default",
            "acme-api",
            Some(reader),
        )
        .await
        .unwrap_err()
        .is_forbidden()
    );
    assert_eq!(row(&store, project).0, "api");
}

#[tokio::test]
async fn explicit_path_capture_falls_back_when_another_identity_claims_the_canonical_key() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let github = remote("github.com/acme/api");
    let gitlab = remote("gitlab.com/acme/api");
    let existing = resolve(&store, ws, &github, "api", None).await.0;
    let conflicting = resolve(&store, ws, &gitlab, "acme-api", None).await.0;

    let (resolved, how) =
        resolve_capture(&store, ws, &github, IdentityStyle::Path, "api-clone", None).await;

    assert_eq!(resolved, existing);
    assert_eq!(how, IdentityResolution::Matched);
    assert_eq!(row(&store, existing).0, "api");
    assert_eq!(row(&store, conflicting).0, "acme-api");
}

#[tokio::test]
async fn canonical_target_conflict_rolls_back_the_promotion() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let legacy = resolve(&store, ws, &remote("github.com/acme/api"), "api", None)
        .await
        .0;
    // A plain row under the canonical name, as an install that ran before the
    // capture path stopped minting twins would already hold. Creating it
    // through the writer would now bind it to the identity row above, which is
    // the fix under test, not the state this test is about.
    let conflict = ProjectId::new();
    rusqlite::Connection::open(store.db_path())
        .unwrap()
        .execute(
            "INSERT INTO projects (id, workspace_id, name, created_at) VALUES (?1, ?2, 'acme-api', 0)",
            rusqlite::params![conflict.as_bytes().to_vec(), ws.as_bytes().to_vec()],
        )
        .unwrap();
    // The empty, identity-less twin no longer makes the canonical name
    // ambiguous (#2018): it reaches the repository owner. The promotion of the
    // owner to that name still rolls back, because the twin holds it.
    let resolved =
        create_explicit_scope_guarded(&store.reader, &store.writer, "default", "acme-api", None)
            .await
            .unwrap();
    assert_eq!(resolved.scope.project_id, legacy);
    assert_eq!(row(&store, legacy).0, "api");
    assert_eq!(row(&store, conflict).0, "acme-api");
}

#[tokio::test]
async fn audit_failure_rolls_back_promotion_and_keeps_the_writer_alive() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let project = resolve(&store, ws, &remote("github.com/acme/api"), "api", None)
        .await
        .0;
    store
        .writer
        .upsert_page(preserved_page(ws, project))
        .await
        .unwrap();
    let owner = user(&store, "rollback-owner").await;
    store
        .writer
        .grant_memory(owner, project, GrantLevel::Write, None)
        .await
        .unwrap();
    let session = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: session,
            workspace_id: ws,
            project_id: project,
            agent_kind: AgentKind::Other,
            cwd: Some("/rollback/api".into()),
            actor_user: None,
            occurred_at: None,
            origin: None,
        })
        .await
        .unwrap();
    store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: project,
            from_session_id: Some(session),
            from_agent: AgentKind::Other,
            to_agent: None,
            cwd: None,
            summary: "rollback baton".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        })
        .await
        .unwrap();
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let snapshot = || {
        ["pages", "sessions", "project_grants", "handoffs"].map(|table| {
            let mut statement = conn
                .prepare(&format!("SELECT * FROM {table} WHERE project_id = ?1"))
                .unwrap();
            let columns = statement.column_count();
            statement
                .query_map([project.as_bytes().to_vec()], |row| {
                    (0..columns)
                        .map(|column| row.get::<_, rusqlite::types::Value>(column))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        })
    };
    let before = snapshot();
    assert!(before.iter().all(|rows| !rows.is_empty()));
    conn.execute_batch(
        "CREATE TRIGGER fail_promotion_audit BEFORE INSERT ON audit_log \
         WHEN NEW.op = 'promote_project_name' \
         BEGIN SELECT RAISE(ABORT, 'forced promotion audit failure'); END;",
    )
    .unwrap();

    assert!(
        create_explicit_scope_guarded(&store.reader, &store.writer, "default", "acme-api", None,)
            .await
            .is_err()
    );
    assert_eq!(row(&store, project).0, "api");
    let audit_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE op = 'promote_project_name'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(audit_count, 0);
    assert_eq!(snapshot(), before, "every dependent row is unchanged");
    assert!(
        store
            .reader
            .page_body_by_ids(ws, project, "notes/preserved.md")
            .await
            .unwrap()
            .is_some()
    );
    conn.execute_batch("DROP TRIGGER fail_promotion_audit")
        .unwrap();
    let later = store
        .writer
        .get_or_create_project(ws, "writer-still-alive", None)
        .await
        .unwrap();
    assert_ne!(later, project);
}

#[tokio::test]
async fn exact_name_that_is_another_forges_canonical_key_fails_closed() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let exact = resolve(
        &store,
        ws,
        &remote("github.com/unrelated/tool"),
        "acme-api",
        None,
    )
    .await
    .0;
    let canonical = resolve(
        &store,
        ws,
        &remote("gitlab.com/acme/api"),
        "gitlab-api",
        None,
    )
    .await
    .0;
    assert_ne!(exact, canonical);
    let error = lookup_existing_scope(&store.reader, "default", "acme-api")
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ScopeResolutionError::ProjectNameAmbiguous { .. }
    ));
}

/// A clone without the marker that declares `widget` creates `acme-widget`
/// with `widget` as its legacy key (#1144). The refusal names both projects
/// and the key each answers by, so the operator can tell which one to purge
/// or rename, but never names a restricted project: the project list hides
/// those names from callers without a grant, and this lookup runs before any
/// caller is authorized.
#[tokio::test]
async fn an_ambiguous_name_names_its_holders_but_not_a_restricted_one() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let declared = store
        .writer
        .get_or_create_project(ws, "widget", None)
        .await
        .unwrap();
    let clone = resolve_capture(
        &store,
        ws,
        &remote("github.com/acme/widget"),
        IdentityStyle::Path,
        "acme-widget",
        None,
    )
    .await
    .0;
    assert_ne!(declared, clone);
    assert_eq!(row(&store, clone).0, "acme-widget");

    let error = lookup_existing_scope(&store.reader, "default", "widget")
        .await
        .unwrap_err();
    assert!(error.is_bad_request());
    assert_eq!(
        error.to_string(),
        format!(
            "project 'widget' is ambiguous in workspace 'default': it is the name of \
             project 'widget' (empty) and the legacy key of project 'acme-widget' (owns \
             repository github.com/acme/widget); for the project of repository \
             github.com/acme/widget, pass project 'acme-widget'; the empty project 'widget' \
             can be renamed out of the way with \
             `ai-memory rename-project --project-id {declared} --to widget-empty`"
        )
    );

    store
        .writer
        .set_access_mode(clone, AccessMode::Restricted)
        .await
        .unwrap();
    let error = lookup_existing_scope(&store.reader, "default", "widget")
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "project 'widget' is ambiguous in workspace 'default': it is the name of \
             project 'widget' (empty) and the legacy key of a restricted project; the empty \
             project 'widget' can be renamed out of the way with \
             `ai-memory rename-project --project-id {declared} --to widget-empty`"
        )
    );
    assert!(!error.to_string().contains("acme-widget"));
}

#[tokio::test]
async fn an_ambiguous_name_on_the_write_path_names_the_workspace() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let declared = store
        .writer
        .get_or_create_project(ws, "widget", None)
        .await
        .unwrap();
    resolve_capture(
        &store,
        ws,
        &remote("github.com/acme/widget"),
        IdentityStyle::Path,
        "acme-widget",
        None,
    )
    .await;

    let error =
        create_explicit_scope_guarded(&store.reader, &store.writer, "default", "widget", None)
            .await
            .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "project 'widget' is ambiguous in workspace 'default': it is the name of \
             project 'widget' (empty) and the legacy key of project 'acme-widget' (owns \
             repository github.com/acme/widget); for the project of repository \
             github.com/acme/widget, pass project 'acme-widget'; the empty project 'widget' \
             can be renamed out of the way with \
             `ai-memory rename-project --project-id {declared} --to widget-empty`"
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_legacy_and_canonical_resolve_or_create_converge_on_one_uuid() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let project = resolve(&store, ws, &remote("github.com/acme/api"), "api", None)
        .await
        .0;
    let resolve =
        |name| create_explicit_scope_guarded(&store.reader, &store.writer, "default", name, None);
    let (a, b) = tokio::join!(resolve("api"), resolve("acme-api"));
    assert_eq!(a.unwrap().scope.project_id, project);
    assert_eq!(b.unwrap().scope.project_id, project);
    assert_eq!(row(&store, project).0, "acme-api");
}

/// Naming a project differently changes nothing about reads: a lookup is
/// still no-create, and the folder name a static client might guess resolves
/// to nothing rather than to a fresh project.
#[tokio::test]
async fn reads_of_a_path_named_project_stay_no_create() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let (id, _) = resolve_styled(
        &store,
        ws,
        &remote("github.com/acme/api"),
        IdentityStyle::Path,
        "api",
        None,
    )
    .await;

    assert_eq!(
        store
            .reader
            .find_project(ws, "acme-api".into())
            .await
            .unwrap(),
        Some(id)
    );
    for _ in 0..2 {
        assert_eq!(
            store.reader.find_project(ws, "api".into()).await.unwrap(),
            None,
            "a lookup of the folder name finds nothing and creates nothing"
        );
    }
}

/// A capture whose marker names a promoted project's legacy key lands in that
/// project instead of minting a twin (#1657).
///
/// The read path already resolves the legacy key — `lookup_existing_scope`
/// answers the promoted row — so the capture path creating a second row under
/// the same name left the name ambiguous for both, and every later explicit
/// call failed with `ProjectNameAmbiguous` until the twin was purged. The
/// capture path now reuses the row the read path picks.
#[tokio::test]
async fn a_capture_naming_a_promoted_projects_legacy_key_reuses_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let owner = user(&store, "promotion-owner").await;
    let project = resolve(
        &store,
        ws,
        &remote("github.com/acme/api"),
        "api",
        Some(owner),
    )
    .await
    .0;
    let promoted = create_explicit_scope_guarded(
        &store.reader,
        &store.writer,
        "default",
        "acme-api",
        Some(owner),
    )
    .await
    .unwrap();
    assert_eq!(promoted.scope.project_id, project);
    assert_eq!(row(&store, project).0, "acme-api");

    // What the router's marker branch calls, with the marker's `project` key.
    let (id, created) = store
        .writer
        .get_or_create_project_as(ws, "api", None, Some(owner))
        .await
        .unwrap();

    assert_eq!(id, project, "the capture must not mint a second project");
    assert!(!created, "nothing was created");
    assert_eq!(
        lookup_existing_scope(&store.reader, "default", "api")
            .await
            .unwrap()
            .project_id,
        project,
        "the name still resolves, which a twin would have made ambiguous"
    );
    let count: i64 = rusqlite::Connection::open(store.db_path())
        .unwrap()
        .query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1, "one project, not a twin");
}

/// A capture under a name that is genuinely new still creates the project, so
/// the legacy-key fallback does not turn capture into a read-only path.
#[tokio::test]
async fn a_capture_of_a_new_name_still_creates_its_project() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let owner = user(&store, "new-project-owner").await;

    let (id, created) = store
        .writer
        .get_or_create_project_as(ws, "brand-new", None, Some(owner))
        .await
        .unwrap();

    assert!(created);
    assert_eq!(row(&store, id).0, "brand-new");
    let creator: Option<Vec<u8>> = rusqlite::Connection::open(store.db_path())
        .unwrap()
        .query_row(
            "SELECT created_by FROM projects WHERE id = ?1",
            [id.as_bytes().to_vec()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        creator.as_deref(),
        Some(owner.as_bytes().as_slice()),
        "a created project must still record its creator"
    );
}

/// Control: when a row already carries the exact name, the capture still
/// lands there and creates nothing. The legacy-key fallback runs only after
/// the exact-name lookup misses, so a name that is only a name keeps its old
/// behaviour — here a twin left behind by the pre-fix code, which the operator
/// purges by id; the fallback must not turn three rows into four.
#[tokio::test]
async fn a_capture_of_an_existing_exact_name_still_lands_there() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = workspace(&store).await;
    let owner = user(&store, "ambiguous-owner").await;
    let project = resolve(
        &store,
        ws,
        &remote("github.com/acme/widget"),
        "widget",
        Some(owner),
    )
    .await
    .0;
    create_explicit_scope_guarded(
        &store.reader,
        &store.writer,
        "default",
        "acme-widget",
        Some(owner),
    )
    .await
    .unwrap();
    assert_eq!(row(&store, project).0, "acme-widget");
    let twin = ProjectId::new();
    rusqlite::Connection::open(store.db_path())
        .unwrap()
        .execute(
            "INSERT INTO projects (id, workspace_id, name, created_at) VALUES (?1, ?2, 'widget', 0)",
            rusqlite::params![twin.as_bytes().to_vec(), ws.as_bytes().to_vec()],
        )
        .unwrap();

    let (id, created) = store
        .writer
        .get_or_create_project_as(ws, "widget", None, Some(owner))
        .await
        .unwrap();

    assert_eq!(id, twin, "the exact-name row still answers to its own name");
    assert!(!created);
    let count: i64 = rusqlite::Connection::open(store.db_path())
        .unwrap()
        .query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2, "no third row");
}

/// The state a pre-guard server can hold (#2018): the repository owner kept
/// its folder name `api` (canonical key `acme-api`), and an empty,
/// identity-less row was created under the literal `acme-api` before new rows
/// were refused under another project's key.
async fn owner_and_empty_canonical_twin(store: &Store) -> (WorkspaceId, ProjectId, ProjectId) {
    let ws = workspace(store).await;
    let owner = store
        .writer
        .get_or_create_project(ws, "api", None)
        .await
        .unwrap();
    let twin = ProjectId::new();
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    conn.execute(
        "UPDATE projects SET identity = 'github.com/acme/api', identity_source = 'git_remote', \
         canonical_name = 'acme-api', legacy_name = 'api' WHERE id = ?1",
        [owner.as_bytes().to_vec()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO projects (id, workspace_id, name, created_at) VALUES (?1, ?2, 'acme-api', 0)",
        rusqlite::params![twin.as_bytes().to_vec(), ws.as_bytes().to_vec()],
    )
    .unwrap();
    (ws, owner, twin)
}

/// The repository's own remote-derived name reaches the project that owns
/// the repository when the only other holder is an empty, identity-less
/// leftover: on the read path, on the create-or-get write path, and without
/// creating another row (#2018).
#[tokio::test]
async fn a_canonical_name_shadowed_by_an_empty_twin_reaches_the_repository_owner() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (_, owner, twin) = owner_and_empty_canonical_twin(&store).await;

    let read = lookup_existing_scope(&store.reader, "default", "acme-api")
        .await
        .unwrap();
    assert_eq!(read.project_id, owner);
    assert_ne!(read.project_id, twin);
    let write =
        create_explicit_scope_guarded(&store.reader, &store.writer, "default", "acme-api", None)
            .await
            .unwrap();
    assert_eq!(write.scope.project_id, owner);
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 2, "no third project is created");
    // The owner's own name still reaches it directly.
    assert_eq!(
        lookup_existing_scope(&store.reader, "default", "api")
            .await
            .unwrap()
            .project_id,
        owner
    );
}

/// Adversarial: a twin that holds data, or any restricted candidate, keeps the
/// name ambiguous. The refusal says which project owns the repository, which
/// is empty, what to pass instead and how to move an empty one aside, and
/// never names a restricted project or its repository.
#[tokio::test]
async fn a_twin_with_data_or_a_restricted_candidate_stays_ambiguous_with_a_way_forward() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, _owner, twin) = owner_and_empty_canonical_twin(&store).await;
    store
        .writer
        .begin_session(ai_memory_core::NewSession {
            occurred_at: None,
            id: ai_memory_core::SessionId::new(),
            workspace_id: ws,
            project_id: twin,
            agent_kind: ai_memory_core::AgentKind::Codex,
            cwd: None,
            actor_user: None,
            origin: None,
        })
        .await
        .unwrap();
    let error = lookup_existing_scope(&store.reader, "default", "acme-api")
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ScopeResolutionError::ProjectNameAmbiguous { .. }
    ));
    assert_eq!(
        error.to_string(),
        "project 'acme-api' is ambiguous in workspace 'default': it is the name of project \
         'acme-api' and the canonical key of project 'api' (owns repository \
         github.com/acme/api); for the project of repository github.com/acme/api, pass \
         project 'api'"
    );

    // The same pair with the twin empty but the owner restricted: still
    // ambiguous, and the owner stays anonymous.
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (_, owner, twin) = owner_and_empty_canonical_twin(&store).await;
    store
        .writer
        .set_access_mode(owner, AccessMode::Restricted)
        .await
        .unwrap();
    let error = lookup_existing_scope(&store.reader, "default", "acme-api")
        .await
        .unwrap_err();
    let message = error.to_string();
    assert_eq!(
        message,
        format!(
            "project 'acme-api' is ambiguous in workspace 'default': it is the name of project \
             'acme-api' (empty) and the canonical key of a restricted project; the empty \
             project 'acme-api' can be renamed out of the way with \
             `ai-memory rename-project --project-id {twin} --to acme-api-empty`"
        )
    );
    assert!(!message.contains("github.com/acme/api"));
    assert!(!message.contains(&owner.to_string()));
}
