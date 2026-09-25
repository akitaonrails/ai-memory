//! Routing a capture by repository identity (#708).
//!
//! A project name comes from a folder, and folder names collide: two unrelated
//! repositories both checked out as `api/` would share one project, and so one
//! grant. `resolve_project_by_identity` routes by the identity the client
//! resolved instead. These pin each way it can answer, and the two properties
//! that make it safe to switch on for installs that already hold data: an
//! existing project is claimed in place rather than split away, and somebody
//! who may not write to it cannot take its identity.

use ai_memory_core::repository_identity::{IdentitySource, RepositoryIdentity};
use ai_memory_core::{NewUser, ProjectId, UserId, WorkspaceId};
use ai_memory_store::{
    AccessMode, GrantLevel, IdentityResolution, ProjectAccess, ProjectPrincipal, Store,
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

async fn user(store: &Store, name: &str, byte: u8) -> UserId {
    store
        .writer
        .create_user(
            NewUser {
                username: name.to_owned(),
                name: None,
                email: None,
            },
            [byte; ai_memory_store::TOKEN_HASH_LEN],
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
    store
        .writer
        .resolve_project_by_identity(
            ws,
            identity.clone(),
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
    let member = user(&store, "member", 1).await;
    let outsider = user(&store, "outsider", 2).await;
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
    let alice = user(&store, "alice", 1).await;
    let bob = user(&store, "bob", 2).await;

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
