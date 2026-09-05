//! Session-brief visibility for shared vs per-operator slots.
//!
//! Slots are injected into every session start for a project, so on a shared
//! server one operator's "what I am working on" becomes everybody's context.
//! Namespacing them (`_slots/<segment>/…`, the segment being
//! `IdentityKey::path_segment()`) fixes that, and must do so without breaking
//! anything already stored: a slot with no namespace is SHARED and stays
//! visible to everyone, exactly as every pre-existing slot is.

use ai_memory_core::{
    IdentityKey, NewPage, PagePath, ProjectId, SlotVisibility, Tier, WorkspaceId,
};
use ai_memory_store::Store;

async fn scope(store: &Store) -> (WorkspaceId, ProjectId) {
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "app".to_string(), None)
        .await
        .unwrap();
    (ws, proj)
}

/// The qualified key the auth layer resolves a username to; slot namespaces
/// come from its `path_segment()`, so the tests build paths through the same
/// API the engine uses instead of hand-writing segments.
fn user(name: &str) -> IdentityKey {
    IdentityKey::User(name.into())
}

fn ns(key: &IdentityKey) -> String {
    key.path_segment()
}

/// Slots are force-pinned by every write path, which is exactly why the brief
/// query cannot filter only its `_slots/*` arm.
async fn write_slot(store: &Store, ws: WorkspaceId, proj: ProjectId, path: &str) {
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path).unwrap(),
            title: path.into(),
            body: "body".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: true,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
        })
        .await
        .unwrap();
}

/// Brief paths for `viewer` with `[slots] per_user` ON.
async fn brief_paths(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    viewer: Option<&IdentityKey>,
) -> Vec<String> {
    brief_paths_with(store, ws, proj, SlotVisibility::for_viewer(true, viewer)).await
}

async fn brief_paths_with(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    slots: SlotVisibility,
) -> Vec<String> {
    store
        .reader
        .session_brief_pages_with_slot_visibility(ws, proj, 100, 100, slots)
        .await
        .unwrap()
        .0
        .into_iter()
        .map(|p| p.path)
        .collect()
}

/// DEFAULT CONFIG (`[slots] per_user` off). A nested slot path is then just a
/// slot page — `_slots/backend/context.md` is a perfectly legal path that a
/// deployment may have been carrying for a year — and it must keep reaching
/// every session brief. Namespacing has to be a rule the read path opts into,
/// not one it applies to a `None` viewer.
#[tokio::test]
async fn nested_slots_stay_shared_while_the_feature_is_off() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    write_slot(&store, ws, proj, "_slots/current-focus.md").await;
    write_slot(&store, ws, proj, "_slots/backend/context.md").await;

    // Both the unattributed brief and a named one: with the feature off the
    // viewer's identity buys nothing and hides nothing.
    let alice = user("alice");
    for viewer in [None, Some(&alice)] {
        let paths =
            brief_paths_with(&store, ws, proj, SlotVisibility::for_viewer(false, viewer)).await;
        assert!(
            paths.contains(&"_slots/backend/context.md".to_string()),
            "a pre-existing nested slot must survive the upgrade for {viewer:?}: {paths:?}"
        );
        assert!(
            paths.contains(&"_slots/current-focus.md".to_string()),
            "{viewer:?}"
        );
    }

    // Same for the default rule, which is what every caller that knows nothing
    // about operators gets.
    let defaulted = brief_paths_with(&store, ws, proj, SlotVisibility::default()).await;
    assert!(defaulted.contains(&"_slots/backend/context.md".to_string()));
}

/// The load-bearing case: a personal slot reaches its owner and nobody else,
/// while an un-namespaced slot still reaches everyone.
#[tokio::test]
async fn personal_slots_reach_only_their_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let alice = user("alice");
    let bob = user("bob");
    let alice_slot = format!("_slots/{}/current-focus.md", ns(&alice));
    let bob_slot = format!("_slots/{}/current-focus.md", ns(&bob));
    write_slot(&store, ws, proj, "_slots/current-focus.md").await;
    write_slot(&store, ws, proj, &alice_slot).await;
    write_slot(&store, ws, proj, &bob_slot).await;

    let seen = brief_paths(&store, ws, proj, Some(&alice)).await;
    assert!(seen.contains(&"_slots/current-focus.md".to_string()));
    assert!(seen.contains(&alice_slot));
    assert!(
        !seen.contains(&bob_slot),
        "Bob's working context must not be injected into Alice's session"
    );

    // An unidentified viewer sees the shared slot only.
    let anon = brief_paths(&store, ws, proj, None).await;
    assert!(anon.contains(&"_slots/current-focus.md".to_string()));
    assert!(!anon.contains(&alice_slot));
}

/// An operator named by a qualified OIDC issuer/subject pair
/// is still a named operator. Their qualified segment must admit their own
/// slot — and be distinct from an equal USERNAME's segment, so the two name
/// spaces cannot read each other's briefs.
#[tokio::test]
async fn oidc_operator_gets_a_namespace_and_sees_their_own_slot() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let carol = IdentityKey::Subject {
        issuer: "https://idp.example".into(),
        subject: "oidc-subject-carol".into(),
    };
    let carol_slot = format!("_slots/{}/current-focus.md", ns(&carol));
    // The identity a username "oidc-subject-carol" would produce — equal raw
    // value, different name space, different segment.
    let impostor = user("oidc-subject-carol");
    assert_ne!(ns(&carol), ns(&impostor));

    write_slot(&store, ws, proj, "_slots/current-focus.md").await;
    write_slot(&store, ws, proj, &carol_slot).await;

    let carols = brief_paths(&store, ws, proj, Some(&carol)).await;
    assert!(
        carols.contains(&carol_slot),
        "an OIDC operator without a username must see their own slot body: {carols:?}"
    );
    assert!(carols.contains(&"_slots/current-focus.md".to_string()));

    let impostors = brief_paths(&store, ws, proj, Some(&impostor)).await;
    assert!(
        !impostors.contains(&carol_slot),
        "a username equal to the subject must not read the subject's slot: {impostors:?}"
    );
}

/// Regression guard for the trap this design walks into: slots are pinned, and
/// the brief predicate is a disjunction that includes `pinned = 1`. Filtering
/// only the `_slots/*` arm changes nothing, because the personal slots come
/// straight back through `pinned`.
#[tokio::test]
async fn pinned_arm_does_not_leak_other_operators_slots() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let bob = user("bob");
    write_slot(
        &store,
        ws,
        proj,
        &format!("_slots/{}/secret-focus.md", ns(&bob)),
    )
    .await;

    let alice = user("alice");
    let seen = brief_paths(&store, ws, proj, Some(&alice)).await;
    assert!(
        !seen.iter().any(|p| p.contains("bob")),
        "a pinned personal slot must not reach another operator through the pinned arm: {seen:?}"
    );
}

/// Nothing else changes: ordinary pinned pages and `_rules/` are untouched by
/// slot namespacing, for every viewer.
#[tokio::test]
async fn rules_and_ordinary_pinned_pages_are_unaffected() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    write_slot(&store, ws, proj, "_rules/style.md").await;
    write_slot(&store, ws, proj, "notes/pinned.md").await;

    let alice = user("alice");
    let bob = user("bob");
    for viewer in [None, Some(&alice), Some(&bob)] {
        let paths = brief_paths(&store, ws, proj, viewer).await;
        assert!(paths.contains(&"_rules/style.md".to_string()), "{viewer:?}");
        assert!(paths.contains(&"notes/pinned.md".to_string()), "{viewer:?}");
    }
}

/// A path-hostile name cannot break out of its own namespace. Under the raw
/// naming scheme a viewer called `*` had to be refused a namespace outright
/// because the segment lands inside a SQL GLOB; `path_segment()` hashes
/// it instead, so the viewer keeps a working (and harmless) namespace while
/// everyone else's stays out of reach.
#[tokio::test]
async fn hostile_viewer_names_cannot_match_other_namespaces() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let alice = user("alice");
    let alice_slot = format!("_slots/{}/focus.md", ns(&alice));
    write_slot(&store, ws, proj, "_slots/shared.md").await;
    write_slot(&store, ws, proj, &alice_slot).await;

    let sneaky = user("*");
    let sneaky_ns = ns(&sneaky);
    assert!(
        sneaky_ns.starts_with("uh-"),
        "a GLOB metacharacter must use a bounded identifier: {sneaky_ns}"
    );
    let sneaky_slot = format!("_slots/{sneaky_ns}/focus.md");
    write_slot(&store, ws, proj, &sneaky_slot).await;

    let seen = brief_paths(&store, ws, proj, Some(&sneaky)).await;
    assert!(seen.contains(&"_slots/shared.md".to_string()));
    assert!(
        seen.contains(&sneaky_slot),
        "the bounded namespace is a real, readable namespace: {seen:?}"
    );
    assert!(
        !seen.contains(&alice_slot),
        "a wildcard name must not read every namespace: {seen:?}"
    );
}

/// The briefing snapshot feeds the consolidation prompt, so it obeys the same
/// rule as the brief — and, by default, still lists every slot there is.
#[tokio::test]
async fn briefing_slots_follow_the_same_visibility_rule() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let alice = user("alice");
    let bob = user("bob");
    let alice_slot = format!("_slots/{}/current-focus.md", ns(&alice));
    let bob_slot = format!("_slots/{}/current-focus.md", ns(&bob));
    write_slot(&store, ws, proj, "_slots/current-focus.md").await;
    write_slot(&store, ws, proj, &alice_slot).await;
    write_slot(&store, ws, proj, &bob_slot).await;

    let paths = |snapshot: ai_memory_store::BriefingSnapshot| {
        snapshot
            .slots
            .into_iter()
            .map(|s| s.path)
            .collect::<Vec<_>>()
    };

    let default = paths(
        store
            .reader
            .briefing_for_project_with_slot_visibility(
                ws,
                proj,
                10,
                ai_memory_core::OwnerFilter::Any,
                &SlotVisibility::default(),
            )
            .await
            .unwrap(),
    );
    assert_eq!(
        default.len(),
        3,
        "default config lists every slot: {default:?}"
    );

    let mine = SlotVisibility::for_viewer(true, Some(&alice));
    let seen = paths(
        store
            .reader
            .briefing_for_project_with_slot_visibility(
                ws,
                proj,
                10,
                ai_memory_core::OwnerFilter::Any,
                &mine,
            )
            .await
            .unwrap(),
    );
    assert!(seen.contains(&"_slots/current-focus.md".to_string()));
    assert!(seen.contains(&alice_slot));
    assert!(
        !seen.contains(&bob_slot),
        "Bob's slot must not reach a snapshot assembled for Alice: {seen:?}"
    );

    let workspace_wide = paths(
        store
            .reader
            .briefing_for_workspace_with_slot_visibility(
                ws,
                10,
                ai_memory_core::OwnerFilter::Any,
                &mine,
            )
            .await
            .unwrap(),
    );
    assert!(!workspace_wide.contains(&bob_slot));
}

/// The `recent_pages` pointer list is the sibling the slot filter kept missing.
///
/// Every briefing query returns two lists: a `slots` array, filtered by the
/// visibility rule, and `recent_pages` — every recently touched page. A
/// personal slot is still a page, so filtering only the first leaks the second:
/// the path and title of another operator's slot arrive in the pointer list and
/// the session brief renders them verbatim. Bodies stay withheld either way,
/// but a path and a title are already somebody's working context.
#[tokio::test]
async fn recent_pages_hide_other_operators_personal_slots() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let alice = user("alice");
    let bob = user("bob");
    let alice_slot = format!("_slots/{}/current-focus.md", ns(&alice));
    let bob_slot = format!("_slots/{}/current-focus.md", ns(&bob));
    write_slot(&store, ws, proj, "_slots/current-focus.md").await;
    write_slot(&store, ws, proj, &alice_slot).await;
    write_slot(&store, ws, proj, &bob_slot).await;
    write_slot(&store, ws, proj, "notes/ordinary.md").await;

    let mine = SlotVisibility::for_viewer(true, Some(&alice));
    let recent_paths = |pages: Vec<ai_memory_store::BriefingPage>| -> Vec<String> {
        pages.into_iter().map(|p| p.path).collect()
    };

    // All four briefing surfaces, because the query is copy-pasted across them
    // and fixing one is how this leak survived three review rounds.
    let brief = store
        .reader
        .session_brief_pages_with_slot_visibility(ws, proj, 100, 100, mine.clone())
        .await
        .unwrap()
        .1;
    let project = store
        .reader
        .briefing_for_project_with_slot_visibility(
            ws,
            proj,
            100,
            ai_memory_core::OwnerFilter::Any,
            &mine,
        )
        .await
        .unwrap()
        .recent_pages;
    let workspace = store
        .reader
        .briefing_for_workspace_with_slot_visibility(
            ws,
            100,
            ai_memory_core::OwnerFilter::Any,
            &mine,
        )
        .await
        .unwrap()
        .recent_pages;
    let global = store
        .reader
        .briefing_with_slot_visibility(100, ai_memory_core::OwnerFilter::Any, &mine)
        .await
        .unwrap()
        .recent_pages;

    for (surface, pages) in [
        ("session_brief_pages", brief),
        ("briefing_for_project", project),
        ("briefing_for_workspace", workspace),
        ("briefing", global),
    ] {
        let paths = recent_paths(pages);
        assert!(
            !paths.contains(&bob_slot),
            "{surface}: Bob's slot path must not reach Alice's pointer list: {paths:?}"
        );
        assert!(
            paths.contains(&alice_slot),
            "{surface}: Alice must still see her own: {paths:?}"
        );
        assert!(
            paths.contains(&"_slots/current-focus.md".to_string()),
            "{surface}: the shared slot reaches everyone: {paths:?}"
        );
        assert!(
            paths.contains(&"notes/ordinary.md".to_string()),
            "{surface}: an ordinary page must be untouched: {paths:?}"
        );
    }
}

#[tokio::test]
async fn foreign_slots_do_not_consume_the_viewers_query_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let viewer = user("zed");
    let own = format!("_slots/{}/focus.md", ns(&viewer));
    let foreign = format!("_slots/{}/focus.md", ns(&user("alice")));

    // Own first, foreign second: foreign is both newer and sorts before own.
    // A filter applied after either LIMIT would return an empty result.
    write_slot(&store, ws, proj, &own).await;
    write_slot(&store, ws, proj, &foreign).await;
    let visibility = SlotVisibility::for_viewer(true, Some(&viewer));

    let (core, recent) = store
        .reader
        .session_brief_pages_with_slot_visibility(ws, proj, 1, 1, visibility.clone())
        .await
        .unwrap();
    assert_eq!(core[0].path, own);
    assert_eq!(recent[0].path, own);

    let snapshot = store
        .reader
        .briefing_for_project_with_slot_visibility(
            ws,
            proj,
            1,
            ai_memory_core::OwnerFilter::Any,
            &visibility,
        )
        .await
        .unwrap();
    assert_eq!(snapshot.recent_pages[0].path, own);
}

/// DEFAULT CONFIG: with `[slots] per_user` off the pointer list is exactly what
/// it was before slots could be namespaced — every slot, nested ones included.
#[tokio::test]
async fn recent_pages_are_unfiltered_while_the_feature_is_off() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    write_slot(&store, ws, proj, "_slots/current-focus.md").await;
    write_slot(&store, ws, proj, "_slots/backend/context.md").await;

    let alice = user("alice");
    for slots in [
        SlotVisibility::default(),
        SlotVisibility::for_viewer(false, None),
        SlotVisibility::for_viewer(false, Some(&alice)),
    ] {
        let paths: Vec<String> = store
            .reader
            .session_brief_pages_with_slot_visibility(ws, proj, 100, 100, slots.clone())
            .await
            .unwrap()
            .1
            .into_iter()
            .map(|p| p.path)
            .collect();
        assert!(
            paths.contains(&"_slots/backend/context.md".to_string()),
            "a pre-existing nested slot must stay in the pointer list: {paths:?}"
        );
        assert!(paths.contains(&"_slots/current-focus.md".to_string()));
    }
}

/// Expiry and slot visibility are INDEPENDENT gates that both apply.
///
/// The two predicates were written by different changes over the same four
/// queries, and each one alone looks complete: hiding an expired page is right,
/// and hiding another operator's slot is right. Composing them wrong is what is
/// invisible — an `OR` would resurrect either half, and appending the expiry
/// cutoff after the OPTIONAL slot glob would shift the glob's parameter index
/// whenever the visibility rule needs no pattern.
#[tokio::test]
async fn expiry_and_slot_visibility_both_apply() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let alice = user("alice");
    let bob = user("bob");
    let alice_live = format!("_slots/{}/live.md", ns(&alice));
    let alice_stale = format!("_slots/{}/stale.md", ns(&alice));
    let bob_live = format!("_slots/{}/live.md", ns(&bob));

    let expired = jiff::Timestamp::now() - std::time::Duration::from_secs(3600);
    for (path, expires_at) in [
        (alice_live.as_str(), None),
        (alice_stale.as_str(), Some(expired)),
        (bob_live.as_str(), None),
        ("_slots/shared.md", None),
        ("notes/stale.md", Some(expired)),
    ] {
        store
            .writer
            .upsert_page(NewPage {
                workspace_id: ws,
                project_id: proj,
                path: PagePath::new(path).unwrap(),
                title: path.into(),
                body: "body".into(),
                tier: Tier::Semantic,
                frontmatter_json: serde_json::json!({}),
                // Pinned, as every slot write path forces — which is what makes
                // "expiry overrules the pin" a real assertion below.
                pinned: true,
                links: Vec::new(),
                author_id: None,
                expires_at,
                entities: Vec::new(),
            })
            .await
            .unwrap();
    }

    // Alice, with `[slots] per_user` ON: her own live slot and the shared one.
    let core = brief_paths(&store, ws, proj, Some(&alice)).await;
    assert!(core.contains(&alice_live));
    assert!(core.contains(&"_slots/shared.md".to_string()));
    assert!(
        !core.contains(&alice_stale),
        "her OWN slot is still gone once it expires: {core:?}"
    );
    assert!(
        !core.contains(&bob_live),
        "a live slot belonging to somebody else is still not hers: {core:?}"
    );
    assert!(
        !core.contains(&"notes/stale.md".to_string()),
        "expiry overrules the pin the write path applied: {core:?}"
    );

    // The `recent_pages` pointer list obeys both rules too — a path and a
    // title are already somebody's working context, and an expired page is
    // still expired.
    let recent: Vec<String> = store
        .reader
        .session_brief_pages_with_slot_visibility(
            ws,
            proj,
            100,
            100,
            SlotVisibility::for_viewer(true, Some(&alice)),
        )
        .await
        .unwrap()
        .1
        .into_iter()
        .map(|p| p.path)
        .collect();
    assert!(recent.contains(&alice_live));
    assert!(!recent.contains(&bob_live), "{recent:?}");
    assert!(
        !recent.contains(&"notes/stale.md".to_string()),
        "{recent:?}"
    );
    assert!(!recent.contains(&alice_stale), "{recent:?}");

    // And with the feature OFF (default config) expiry still applies on its
    // own, while every live slot is shared again.
    let default_core = brief_paths_with(&store, ws, proj, SlotVisibility::default()).await;
    assert!(default_core.contains(&bob_live));
    assert!(default_core.contains(&alice_live));
    assert!(!default_core.contains(&"notes/stale.md".to_string()));
    assert!(!default_core.contains(&alice_stale));
}
