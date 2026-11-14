//! On-demand end-to-end smoke for the retrieval harness (mirrors the
//! `writer_throughput` pattern: `#[ignore]`d, run deliberately).
//!
//! Requirements, both intentionally not provisioned by the test:
//! - `target/release/ai-memory` (`cargo build --release -p ai-memory-cli`)
//! - `evals/datasets/longmemeval_s.json` (run once with `--fetch`)
//!
//! ```bash
//! cargo test -p ai-memory-eval --test smoke -- --ignored
//! ```

use std::path::PathBuf;
use std::process::Command;

/// Inherited server settings must not silently change a baseline or an A/B
/// candidate; intentional settings are passed through the CLI and reported.
#[test]
#[ignore = "needs a built server binary and the bundled offline corpus"]
fn coding_corpus_ignores_ambient_server_settings_but_keeps_explicit_overrides() {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let server_bin = std::env::var_os("AI_MEMORY_EVAL_TEST_SERVER_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root.join("target/release/ai-memory"));
    assert!(server_bin.is_file(), "build the server binary first");
    let run = |override_baseline: bool| -> serde_json::Value {
        let out = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ai-memory-eval"));
        cmd.current_dir(&repo_root)
            .env_clear()
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("AI_MEMORY_INGEST__DROP_EVENTS", "user-prompt,stop")
            .args([
                "retrieval",
                "--dataset-variant",
                "coding-v1",
                "--sample",
                "1",
                "--concurrency",
                "1",
                "--candidate",
                "--candidate-server-env",
                "AI_MEMORY_INGEST__DROP_EVENTS=user-prompt,stop",
            ])
            .arg("--server-bin")
            .arg(&server_bin)
            .arg("--out")
            .arg(out.path());
        if override_baseline {
            cmd.args([
                "--server-env",
                "AI_MEMORY_INGEST__DROP_EVENTS=user-prompt,stop",
            ]);
        }
        for key in ["PATH", "SystemRoot", "TMPDIR", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        let result = cmd.output().unwrap();
        assert!(
            result.status.success(),
            "retrieval failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let run_dir = std::fs::read_dir(out.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir())
            .unwrap();
        serde_json::from_slice(&std::fs::read(run_dir.join("report.json")).unwrap()).unwrap()
    };
    let report = run(false);
    let baseline = &report["baseline"];
    let candidate = &report["candidate"];
    assert_eq!(baseline["slices"]["overall"]["hit_at"]["10"], 1.0);
    assert_eq!(candidate["slices"]["overall"]["hit_at"]["10"], 0.0);
    assert!(
        candidate["config"]
            .as_str()
            .unwrap()
            .contains("AI_MEMORY_INGEST__DROP_EVENTS=user-prompt,stop")
    );
    let report = run(true);
    assert_eq!(report["baseline"]["slices"]["overall"]["hit_at"]["10"], 0.0);
    assert!(
        report["baseline"]["config"]
            .as_str()
            .unwrap()
            .contains("AI_MEMORY_INGEST__DROP_EVENTS=user-prompt,stop")
    );
}

#[test]
#[ignore = "needs the release server binary and the downloaded dataset"]
fn the_harness_scores_a_tiny_sample_end_to_end() {
    let repo_root: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let server_bin = repo_root.join("target/release/ai-memory");
    assert!(
        server_bin.exists(),
        "build the server first: cargo build --release -p ai-memory-cli"
    );
    let dataset = repo_root.join("evals/datasets/longmemeval_s.json");
    assert!(
        dataset.exists(),
        "fetch the dataset first: cargo run -p ai-memory-eval -- retrieval --fetch --sample 1"
    );

    let out = tempfile::tempdir().unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_ai-memory-eval"))
        .current_dir(&repo_root)
        .args(["retrieval", "--sample", "2", "--concurrency", "2"])
        .arg("--server-bin")
        .arg(&server_bin)
        .arg("--out")
        .arg(out.path())
        .status()
        .unwrap();
    assert!(status.success(), "harness exited non-zero");

    // Exactly one run dir with a structurally complete report.
    let run_dir = std::fs::read_dir(out.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.is_dir())
        .expect("run dir created");
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(run_dir.join("report.json")).unwrap()).unwrap();
    assert_eq!(report["questions_scored"], 2);
    assert_eq!(report["mode"], "zero-llm");
    assert!(report["slices"]["overall"]["hit_at"]["5"].is_number());
    assert!(report["slices"]["overall"]["recall_all_at"]["5"].is_number());
    assert!(report["per_question"][0]["recall_all_at"]["5"].is_number());
    // The R2 triple lands next to accuracy.
    assert!(report["slices"]["overall"]["latency_p50_ms"].is_number());
    assert!(report["slices"]["overall"]["context_tokens_mean"].is_number());
    // Provenance that makes a published number auditable.
    assert!(report["commit"].as_str().unwrap().len() >= 7);
    assert!(
        report["dataset_sha256"]
            .as_str()
            .unwrap()
            .starts_with("08d8dad4")
    );
    assert!(run_dir.join("report.md").exists());
}

/// R2 A/B path: a baseline-vs-baseline run (`--candidate`, no differing
/// knob) must produce a delta report whose accuracy and context-token
/// deltas are exactly zero — the determinism guard.
#[test]
#[ignore = "needs the release server binary and the downloaded dataset"]
fn the_ab_path_reports_a_zero_delta_for_identical_configs() {
    let repo_root: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let server_bin = repo_root.join("target/release/ai-memory");
    assert!(
        server_bin.exists(),
        "build the server first: cargo build --release -p ai-memory-cli"
    );
    let dataset = repo_root.join("evals/datasets/longmemeval_s.json");
    assert!(
        dataset.exists(),
        "fetch the dataset first: cargo run -p ai-memory-eval -- retrieval --fetch --sample 1"
    );

    let out = tempfile::tempdir().unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_ai-memory-eval"))
        .current_dir(&repo_root)
        .args([
            "retrieval",
            "--candidate",
            "--sample",
            "2",
            "--concurrency",
            "2",
        ])
        .arg("--server-bin")
        .arg(&server_bin)
        .arg("--out")
        .arg(out.path())
        .status()
        .unwrap();
    assert!(status.success(), "A/B harness exited non-zero");

    let run_dir = std::fs::read_dir(out.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.is_dir())
        .expect("run dir created");
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(run_dir.join("report.json")).unwrap()).unwrap();
    // A/B report shape: baseline + candidate + precomputed delta.
    assert_eq!(report["baseline"]["questions_scored"], 2);
    assert_eq!(report["candidate"]["questions_scored"], 2);
    // Identical configs ⇒ deterministic accuracy and context; only latency
    // (wall-clock) may wobble, so it is not asserted zero here.
    let delta = &report["delta"];
    for k in ["1", "3", "5", "10"] {
        assert_eq!(delta["hit_at"][k], 0.0, "hit@{k} delta");
        assert_eq!(delta["recall_at"][k], 0.0, "recall@{k} delta");
        assert_eq!(delta["recall_all_at"][k], 0.0, "recall-all@{k} delta");
    }
    assert_eq!(delta["context_tokens_mean"], 0.0);
    assert_eq!(delta["context_tokens_median"], 0.0);
    assert!(run_dir.join("report.md").exists());
}

/// The bundled corpus exercises the same server and scorer without downloading
/// LongMemEval. Identical configurations must agree on evidence and context.
#[test]
#[ignore = "needs the release server binary; uses the bundled offline corpus"]
fn coding_corpus_replays_offline_with_identical_ab_results() {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let server_bin = repo_root.join("target/release/ai-memory");
    assert!(server_bin.exists(), "build the release server first");
    let out = tempfile::tempdir().unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_ai-memory-eval"))
        .current_dir(&repo_root)
        .args([
            "retrieval",
            "--dataset-variant",
            "coding-v1",
            "--candidate",
            "--concurrency",
            "1",
        ])
        .arg("--server-bin")
        .arg(&server_bin)
        .arg("--out")
        .arg(out.path())
        .status()
        .unwrap();
    assert!(status.success());
    let run_dir = std::fs::read_dir(out.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap();
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(run_dir.join("report.json")).unwrap()).unwrap();
    assert_eq!(report["baseline"]["dataset"], "coding_v1");
    for question in report["baseline"]["per_question"].as_array().unwrap() {
        assert_eq!(
            question["recall_at"]["10"], 1.0,
            "missing expected evidence: {}",
            question["question_id"]
        );
    }
    assert_eq!(report["baseline"]["questions_scored"], 4);
    assert_eq!(report["candidate"]["questions_scored"], 4);
    for metric in ["hit_at", "recall_at", "recall_all_at"] {
        for k in ["1", "3", "5", "10"] {
            assert_eq!(report["delta"][metric][k], 0.0, "{metric}@{k}");
        }
    }
    assert_eq!(report["delta"]["context_tokens_mean"], 0.0);
    assert_eq!(report["delta"]["context_tokens_median"], 0.0);
}
