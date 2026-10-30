//! Concurrent reads share a bounded pool instead of growing the blocking pool.

use std::time::Duration;

use ai_memory_store::{Store, StoreError};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_beyond_the_pool_cap_wait_for_a_connection() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let reader = store.reader.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = std::sync::Arc::new(std::sync::Mutex::new(release_rx));
    let mut tasks = Vec::new();

    for _ in 0..4 {
        let reader = reader.clone();
        let started = started_tx.clone();
        let released = release_rx.clone();
        tasks.push(tokio::spawn(async move {
            reader
                .with_conn(move |_| {
                    started.send(()).unwrap();
                    released.lock().unwrap().recv().unwrap();
                    Ok(())
                })
                .await
                .unwrap();
        }));
    }
    for _ in 0..4 {
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    let fifth_reader = reader.clone();
    let fifth_started = started_tx.clone();
    let fifth_released = release_rx.clone();
    tasks.push(tokio::spawn(async move {
        fifth_reader
            .with_conn(move |_| {
                fifth_started.send(()).unwrap();
                fifth_released.lock().unwrap().recv().unwrap();
                Ok(())
            })
            .await
            .unwrap();
    }));

    assert!(
        started_rx.recv_timeout(Duration::from_millis(150)).is_err(),
        "a fifth read started before a connection was returned"
    );
    release_tx.send(()).unwrap();
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    for _ in 0..4 {
        release_tx.send(()).unwrap();
    }
    for task in tasks {
        task.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_the_pool_wakes_a_read_waiting_for_a_permit() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let reader = store.reader.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = std::sync::Arc::new(std::sync::Mutex::new(release_rx));
    let mut held = Vec::new();

    for _ in 0..4 {
        let reader = reader.clone();
        let started = started_tx.clone();
        let released = release_rx.clone();
        held.push(tokio::spawn(async move {
            reader
                .with_conn(move |_| {
                    started.send(()).unwrap();
                    released.lock().unwrap().recv().unwrap();
                    Ok(())
                })
                .await
        }));
    }
    for _ in 0..4 {
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    let waiting = tokio::spawn(async move { reader.with_conn(|_| Ok(())).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    store.reader.close();

    let refused = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .expect("the blocked read did not wake when the pool closed")
        .unwrap()
        .unwrap_err();
    assert!(matches!(refused, StoreError::ReaderClosed), "{refused:?}");

    for _ in 0..4 {
        release_tx.send(()).unwrap();
    }
    for task in held {
        task.await.unwrap().expect("an in-flight read may finish");
    }
}
