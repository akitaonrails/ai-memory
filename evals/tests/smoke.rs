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
    }
    assert_eq!(delta["context_tokens_mean"], 0.0);
    assert_eq!(delta["context_tokens_median"], 0.0);
    assert!(run_dir.join("report.md").exists());
}
