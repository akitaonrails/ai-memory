//! Offline CLI guards plus on-demand end-to-end retrieval smoke tests.
//!
//! Requirements for the ignored server tests, not provisioned by the tests:
//! - `target/release/ai-memory` (`cargo build --release -p ai-memory-cli`)
//! - `evals/datasets/longmemeval_s.json` (run once with `--fetch`)
//!
//! ```bash
//! cargo test -p ai-memory-eval --test smoke -- --ignored
//! ```

use std::path::PathBuf;
use std::process::Command;

#[test]
fn raw_rag_rejects_invalid_windows_before_loading_models() {
    let out = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ai-memory-eval"))
        .current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."))
        .args([
            "retrieval",
            "--engine",
            "raw-rag",
            "--dataset-variant",
            "coding-v1",
            "--raw-rag-chunk-tokens",
            "0",
            "--out",
        ])
        .arg(out.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("chunk tokens must be"), "{stderr}");
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);
}

#[test]
fn raw_rag_model_downloads_are_explicit_and_do_not_need_a_server() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let out = tempfile::tempdir().unwrap();
    let models = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ai-memory-eval"))
        .current_dir(root)
        .args([
            "retrieval",
            "--engine",
            "raw-rag",
            "--dataset-variant",
            "coding-v1",
            "--server-bin",
            "does-not-exist",
            "--raw-rag-models",
        ])
        .arg(models.path())
        .arg("--out")
        .arg(out.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("--raw-rag-fetch-models"), "{stderr}");
    assert!(!stderr.contains("server binary"), "{stderr}");
    assert_eq!(std::fs::read_dir(models.path()).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);
}

#[test]
fn raw_rag_rejects_misleading_or_incompatible_run_settings() {
    for (extra, expected) in [
        (vec!["--candidate"], "runs alone"),
        (vec!["--ingest", "whole-session"], "runs alone"),
        (vec!["--raw-rag-top-k", "0"], "1 <= top-k"),
        (vec!["--ks", "0"], "--ks must be"),
        (vec!["--ks", "16"], "--ks must be"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_ai-memory-eval"))
            .current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."))
            .args([
                "retrieval",
                "--engine",
                "raw-rag",
                "--dataset-variant",
                "coding-v1",
            ])
            .args(extra)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(stderr.contains(expected), "{stderr}");
    }
    let output = Command::new(env!("CARGO_BIN_EXE_ai-memory-eval"))
        .args(["retrieval", "--raw-rag-fetch-models"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("require --engine raw-rag"));
}

#[test]
#[ignore = "needs both explicitly downloaded, pinned local models"]
fn raw_rag_scores_a_fixture_without_a_server_and_records_its_protocol() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let out = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ai-memory-eval"))
        .current_dir(root)
        .args([
            "retrieval",
            "--engine",
            "raw-rag",
            "--dataset-variant",
            "coding-v1",
            "--sample",
            "1",
            "--server-bin",
            "does-not-exist",
            "--out",
        ])
        .arg(out.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = std::fs::read_dir(out.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(run.join("report.json")).unwrap()).unwrap();
    assert_eq!(report["mode"], "raw-rag");
    assert_eq!(report["questions_scored"], 1);
    assert_eq!(report["raw_rag"]["chunk_tokens"], 256);
    assert_eq!(report["raw_rag"]["overlap_tokens"], 32);
    assert_eq!(report["raw_rag"]["candidates"], 128);
    assert_eq!(report["raw_rag"]["top_k"], 15);
    assert_eq!(report["raw_rag"]["turn_expansion"], true);
    assert!(
        report["raw_rag"]["per_question"][0]["selected_windows"]
            .as_array()
            .unwrap()
            .len()
            <= 15
    );
    assert!(report["per_question"][0]["hit_at"]["5"].is_number());
    let markdown = std::fs::read_to_string(run.join("report.md")).unwrap();
    assert!(markdown.contains("not paper-score reproduction"));
    assert!(!markdown.contains("MCP round trip"));
    assert!(!markdown.contains("Capture is production-shaped"));
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
