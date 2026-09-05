//! `client_activity` — per-client MCP tool-call buckets.

use ai_memory_store::{
    CLIENT_ACTIVITY_MAX_CLIENTS_PER_DAY, CLIENT_ACTIVITY_OVERFLOW_CLIENT, Store,
};

#[tokio::test]
async fn buckets_accumulate_and_aggregate_deterministically() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    // Two flushes into the same (client, day) bucket accumulate; a second
    // client and a second day stay separate.
    store
        .writer
        .bump_client_activity(vec![
            ("vscode".into(), 100, 3, 1),
            ("claude-desktop".into(), 100, 2, 0),
        ])
        .await
        .unwrap();
    store
        .writer
        .bump_client_activity(vec![
            ("vscode".into(), 100, 2, 0),
            ("vscode".into(), 101, 1, 1),
        ])
        .await
        .unwrap();

    let all = store.reader.client_activity_since(None).await.unwrap();
    let seen: Vec<(&str, u64, u64)> = all
        .iter()
        .map(|c| (c.client.as_str(), c.reads, c.writes))
        .collect();
    assert_eq!(
        seen,
        vec![("vscode", 6, 2), ("claude-desktop", 2, 0)],
        "summed across days, volume-desc",
    );

    // The window bound is inclusive on the day bucket.
    let recent = store.reader.client_activity_since(Some(101)).await.unwrap();
    let seen: Vec<(&str, u64, u64)> = recent
        .iter()
        .map(|c| (c.client.as_str(), c.reads, c.writes))
        .collect();
    assert_eq!(seen, vec![("vscode", 1, 1)]);

    // A future bound excludes everything rather than ignoring the argument.
    assert!(
        store
            .reader
            .client_activity_since(Some(4_000_000))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn equal_volumes_keep_a_stable_name_order() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    store
        .writer
        .bump_client_activity(vec![("zed".into(), 10, 1, 0), ("cursor".into(), 10, 1, 0)])
        .await
        .unwrap();
    let first = store.reader.client_activity_since(None).await.unwrap();
    let again = store.reader.client_activity_since(None).await.unwrap();
    assert_eq!(first, again);
    assert_eq!(first[0].client, "cursor", "name tiebreak, not scan order");
}

#[tokio::test]
async fn daily_client_cardinality_is_bounded_and_overflow_accumulates() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let day = 42;

    let named: Vec<_> = (0..CLIENT_ACTIVITY_MAX_CLIENTS_PER_DAY)
        .map(|idx| (format!("client-{idx:03}"), day, 1, 0))
        .collect();
    store.writer.bump_client_activity(named).await.unwrap();
    store
        .writer
        .bump_client_activity(vec![
            ("client-000".into(), day, 2, 0),
            ("overflow-a".into(), day, 1, 2),
            ("overflow-b".into(), day, 3, 4),
        ])
        .await
        .unwrap();

    let rows = store.reader.client_activity_since(Some(day)).await.unwrap();
    assert_eq!(rows.len(), CLIENT_ACTIVITY_MAX_CLIENTS_PER_DAY + 1);
    let first = rows.iter().find(|row| row.client == "client-000").unwrap();
    assert_eq!((first.reads, first.writes), (3, 0));
    let overflow = rows
        .iter()
        .find(|row| row.client == CLIENT_ACTIVITY_OVERFLOW_CLIENT)
        .unwrap();
    assert_eq!((overflow.reads, overflow.writes), (4, 6));

    store
        .writer
        .bump_client_activity(vec![("overflow-a".into(), day + 1, 2, 0)])
        .await
        .unwrap();
    let next_day = store
        .reader
        .client_activity_since(Some(day + 1))
        .await
        .unwrap();
    assert_eq!(next_day.len(), 1, "the client limit resets each UTC day");
    assert_eq!(next_day[0].client, "overflow-a");
}

#[tokio::test]
async fn malformed_client_labels_reject_the_whole_batch() {
    for invalid in [
        String::new(),
        " leading".into(),
        "double  space".into(),
        "bidi\u{202e}override".into(),
        "x".repeat(65),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let error = store
            .writer
            .bump_client_activity(vec![
                ("valid".into(), 10, 1, 0),
                (invalid.clone(), 10, 1, 0),
            ])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not normalized"), "{invalid:?}");
        assert!(
            store
                .reader
                .client_activity_since(None)
                .await
                .unwrap()
                .is_empty(),
            "validation must happen before any entry is written: {invalid:?}"
        );
    }
}
