//! Process-level Tokio runtime bounds for container deployments.
//!
//! These are the only ai-memory-specific environment variables read to
//! bootstrap the process's scheduler. `main.rs` reads them once *before* `Config::load()`: ai-memory's
//! own settings still have exactly one configuration read path. They are not
//! `Config` keys and are not visible to domain crates. Docker operators can set
//! them on high-core hosts where Tokio's default worker-per-CPU pool is too
//! large. A valid `AI_MEMORY_WORKER_THREADS` overrides Tokio's native
//! `TOKIO_WORKER_THREADS`; without that override, Tokio keeps its native
//! environment-then-`available_parallelism()` resolution. Invalid ai-memory
//! values also preserve that behavior, so a fallback can exceed our accepted
//! range. The blocking cap remains an additional pool, not a process-wide
//! thread limit.

use std::ffi::{OsStr, OsString};

/// Environment variable for Tokio's multi-thread worker count.
const WORKER_THREADS_ENV: &str = "AI_MEMORY_WORKER_THREADS";
/// Environment variable for Tokio's additional blocking-thread cap.
const MAX_BLOCKING_THREADS_ENV: &str = "AI_MEMORY_MAX_BLOCKING_THREADS";

// These controls reduce oversized pools rather than request unbounded resources.
// 256 covers high-core servers; 512 keeps Tokio's default blocking cap as the
// largest accepted override. When both are accepted their sum is 768; this is
// only the sum of those two pools, not a hard process-wide thread limit.
const MAX_WORKER_THREADS_LIMIT: usize = 256;
const MAX_BLOCKING_THREADS_LIMIT: usize = 512;

/// Optional bounds for the multi-thread runtime. `None` preserves Tokio's
/// native worker resolution or default blocking cap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeBounds {
    /// Worker threads, in addition to the thread running `block_on`.
    worker_threads: Option<usize>,
    /// Limit for threads spawned by `spawn_blocking`.
    max_blocking_threads: Option<usize>,
}

impl RuntimeBounds {
    /// Read both variables through an injectable lookup. Invalid values are
    /// ignored so a mistyped deployment variable cannot prevent startup.
    fn from_env<F>(lookup: F) -> Self
    where
        F: Fn(&str) -> Option<OsString>,
    {
        Self {
            worker_threads: parse_bound(
                lookup(WORKER_THREADS_ENV).as_deref(),
                MAX_WORKER_THREADS_LIMIT,
            ),
            max_blocking_threads: parse_bound(
                lookup(MAX_BLOCKING_THREADS_ENV).as_deref(),
                MAX_BLOCKING_THREADS_LIMIT,
            ),
        }
    }

    /// Read the process environment once, before configuration is loaded.
    #[must_use]
    pub fn from_process() -> Self {
        Self::from_env(|name| std::env::var_os(name))
    }

    /// Apply only valid bounds; absent or rejected fields retain Tokio's native
    /// worker resolution or default blocking cap.
    pub fn apply_to(&self, builder: &mut tokio::runtime::Builder) {
        if let Some(threads) = self.worker_threads {
            builder.worker_threads(threads);
        }
        if let Some(threads) = self.max_blocking_threads {
            builder.max_blocking_threads(threads);
        }
    }
}

fn parse_bound(value: Option<&OsStr>, limit: usize) -> Option<usize> {
    value
        .and_then(OsStr::to_str)
        .filter(|value| value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0 && *value <= limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, sync::Arc};

    fn vars(entries: &[(&str, &str)]) -> HashMap<String, OsString> {
        entries
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect()
    }

    #[test]
    fn unset_variables_leave_tokio_defaults() {
        let values = vars(&[]);
        assert_eq!(
            RuntimeBounds::from_env(|name| values.get(name).cloned()),
            RuntimeBounds::default()
        );
    }

    #[test]
    fn positive_values_are_bounds() {
        let values = vars(&[(WORKER_THREADS_ENV, "8"), (MAX_BLOCKING_THREADS_ENV, "16")]);
        assert_eq!(
            RuntimeBounds::from_env(|name| values.get(name).cloned()),
            RuntimeBounds {
                worker_threads: Some(8),
                max_blocking_threads: Some(16),
            }
        );
    }

    #[test]
    fn invalid_values_are_ignored_independently() {
        for value in ["0", "-1", "8.5", "", " 8", "+8", "999999999999999999999999"] {
            let values = vars(&[(WORKER_THREADS_ENV, value)]);
            assert_eq!(
                RuntimeBounds::from_env(|name| values.get(name).cloned()).worker_threads,
                None,
                "{value:?}"
            );
        }
        let values = vars(&[(WORKER_THREADS_ENV, "4"), (MAX_BLOCKING_THREADS_ENV, "0")]);
        assert_eq!(
            RuntimeBounds::from_env(|name| values.get(name).cloned()),
            RuntimeBounds {
                worker_threads: Some(4),
                max_blocking_threads: None,
            }
        );
    }

    #[test]
    fn upper_bounds_are_accepted() {
        let worker = MAX_WORKER_THREADS_LIMIT.to_string();
        let blocking = MAX_BLOCKING_THREADS_LIMIT.to_string();
        let values = vars(&[
            (WORKER_THREADS_ENV, worker.as_str()),
            (MAX_BLOCKING_THREADS_ENV, blocking.as_str()),
        ]);
        assert_eq!(
            RuntimeBounds::from_env(|name| values.get(name).cloned()),
            RuntimeBounds {
                worker_threads: Some(MAX_WORKER_THREADS_LIMIT),
                max_blocking_threads: Some(MAX_BLOCKING_THREADS_LIMIT),
            }
        );
    }

    #[test]
    fn above_upper_bounds_and_usize_max_are_ignored() {
        for (name, limit) in [
            (WORKER_THREADS_ENV, MAX_WORKER_THREADS_LIMIT),
            (MAX_BLOCKING_THREADS_ENV, MAX_BLOCKING_THREADS_LIMIT),
        ] {
            for value in [(limit + 1).to_string(), usize::MAX.to_string()] {
                let values = vars(&[(name, value.as_str())]);
                let bounds = RuntimeBounds::from_env(|key| values.get(key).cloned());
                let parsed = match name {
                    WORKER_THREADS_ENV => bounds.worker_threads,
                    MAX_BLOCKING_THREADS_ENV => bounds.max_blocking_threads,
                    _ => unreachable!(),
                };
                assert_eq!(parsed, None, "{name}={value}");
            }
        }
    }

    #[test]
    fn bounds_apply_to_the_multi_thread_builder() {
        let values = vars(&[(WORKER_THREADS_ENV, "2"), (MAX_BLOCKING_THREADS_ENV, "3")]);
        let bounds = RuntimeBounds::from_env(|name| values.get(name).cloned());
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        bounds.apply_to(&mut builder);
        let runtime = builder.enable_all().build().unwrap();
        assert_eq!(runtime.metrics().num_workers(), 2);
        let mut unbounded = tokio::runtime::Builder::new_multi_thread();
        RuntimeBounds::default().apply_to(&mut unbounded);
        let unbounded = unbounded.enable_all().build().unwrap();
        assert!(unbounded.metrics().num_workers() >= 1);
    }

    struct ReleaseGuard(std::sync::mpsc::Sender<()>);

    impl Drop for ReleaseGuard {
        fn drop(&mut self) {
            // Also runs while unwinding from an assertion, so runtime shutdown
            // never waits for a blocking task the failed test forgot to free.
            for _ in 0..4 {
                let _ = self.0.send(());
            }
        }
    }

    #[test]
    fn blocking_cap_queues_a_task_until_a_slot_is_released() {
        const CAP: usize = 2;
        let values = vars(&[(WORKER_THREADS_ENV, "1"), (MAX_BLOCKING_THREADS_ENV, "2")]);
        let bounds = RuntimeBounds::from_env(|name| values.get(name).cloned());
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        bounds.apply_to(&mut builder);
        let runtime = builder.enable_all().build().unwrap();
        runtime.block_on(async {
            let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
            let _release_guard = ReleaseGuard(release_tx.clone());
            let mut blockers = Vec::new();
            for id in 0..CAP {
                let started = started_tx.clone();
                let release = release_rx.clone();
                blockers.push(tokio::task::spawn_blocking(move || {
                    started.send(id).unwrap();
                    let _ = release
                        .lock()
                        .unwrap()
                        .recv_timeout(std::time::Duration::from_secs(10));
                }));
            }

            let mut initial = Vec::new();
            for _ in 0..CAP {
                initial.push(
                    tokio::time::timeout(std::time::Duration::from_secs(5), started_rx.recv())
                        .await
                        .expect("blocking slots did not start in time")
                        .expect("blocking start channel closed"),
                );
            }
            initial.sort_unstable();
            assert_eq!(initial, vec![0, 1]);

            let queued_started = started_tx.clone();
            let queued = tokio::task::spawn_blocking(move || {
                queued_started.send(CAP).unwrap();
            });
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(150), started_rx.recv(),)
                    .await
                    .is_err(),
                "the task beyond the cap ran before a blocking slot was released"
            );

            release_tx.send(()).unwrap();
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_secs(5), started_rx.recv(),)
                    .await
                    .expect("queued task did not start after a release")
                    .expect("blocking start channel closed"),
                CAP
            );
            release_tx.send(()).unwrap();

            for task in blockers.into_iter().chain(std::iter::once(queued)) {
                tokio::time::timeout(std::time::Duration::from_secs(5), task)
                    .await
                    .expect("blocking task did not exit in time")
                    .unwrap();
            }
        });
    }
}
