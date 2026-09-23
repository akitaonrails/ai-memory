//! LongMemEval (v1, S variant) dataset loading.
//!
//! The file is NOT committed (278 MB); it lives gitignored under
//! `evals/datasets/` and is fetched on demand from HuggingFace with a
//! pinned sha256. Dataset: `xiaowu0162/longmemeval` (MIT license).

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Canonical download URL for the S variant.
pub const LONGMEMEVAL_S_URL: &str =
    "https://huggingface.co/datasets/xiaowu0162/longmemeval/resolve/main/longmemeval_s";

/// sha256 of `longmemeval_s` as pinned on 2026-09-01. A drifted upstream
/// file fails loudly instead of silently changing published numbers.
pub const LONGMEMEVAL_S_SHA256: &str =
    "08d8dad4be43ee2049a22ff5674eb86725d0ce5ff434cde2627e5e8e7e117894";

/// One chat turn inside a haystack session.
#[derive(Debug, Clone, Deserialize)]
pub struct Turn {
    pub role: String,
    pub content: String,
}

/// One benchmark question with its private haystack of chat sessions.
#[derive(Debug, Clone, Deserialize)]
pub struct Question {
    pub question_id: String,
    pub question_type: String,
    pub question: String,
    /// Gold answer. Usually a JSON string; occasionally a richer value.
    /// Read via [`Question::gold_answer_text`] for the QA judge (R2b).
    pub answer: serde_json::Value,
    #[allow(dead_code)]
    pub question_date: String,
    pub haystack_dates: Vec<String>,
    pub haystack_session_ids: Vec<String>,
    pub haystack_sessions: Vec<Vec<Turn>>,
    /// Dataset session ids of the sessions holding the evidence.
    pub answer_session_ids: Vec<String>,
}

impl Question {
    /// Abstention questions (`*_abs`) deliberately have a false premise;
    /// no session contains an answer, so they are excluded from recall
    /// metrics and reported separately.
    pub fn is_abstention(&self) -> bool {
        self.question_id.ends_with("_abs")
    }

    /// The gold answer as text for the LLM-as-judge (R2b QA mode). A JSON
    /// string is unwrapped to its contents; any other shape is rendered as
    /// compact JSON so the judge still sees the reference value.
    pub fn gold_answer_text(&self) -> String {
        match &self.answer {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }
}

/// Load the dataset, verifying the pinned sha256 first.
pub fn load(path: &Path) -> Result<Vec<Question>> {
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "reading dataset at {} (use --fetch to download)",
            path.display()
        )
    })?;
    let digest = hex::encode(Sha256::digest(&bytes));
    if digest != LONGMEMEVAL_S_SHA256 {
        bail!(
            "dataset sha256 mismatch at {}: got {digest}, pinned {LONGMEMEVAL_S_SHA256}. \
             Refusing to benchmark against an unpinned file.",
            path.display()
        );
    }
    let questions: Vec<Question> =
        serde_json::from_slice(&bytes).context("parsing longmemeval_s JSON")?;
    for q in &questions {
        if q.haystack_sessions.len() != q.haystack_session_ids.len()
            || q.haystack_sessions.len() != q.haystack_dates.len()
        {
            bail!(
                "question {}: sessions/ids/dates length mismatch ({}/{}/{})",
                q.question_id,
                q.haystack_sessions.len(),
                q.haystack_session_ids.len(),
                q.haystack_dates.len()
            );
        }
    }
    Ok(questions)
}

/// Download the dataset to `path` (atomically: tmp + rename) and verify
/// the pinned sha256 before the rename lands it.
pub async fn fetch(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    tracing::info!(
        url = LONGMEMEVAL_S_URL,
        "downloading LongMemEval-S (278 MB)"
    );
    let resp = reqwest::get(LONGMEMEVAL_S_URL).await?.error_for_status()?;
    let bytes = resp.bytes().await?;
    let digest = hex::encode(Sha256::digest(&bytes));
    if digest != LONGMEMEVAL_S_SHA256 {
        bail!("downloaded dataset sha256 {digest} does not match pin {LONGMEMEVAL_S_SHA256}");
    }
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

mod hex {
    /// Minimal lowercase hex, avoiding a dependency for one call site.
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_json() -> serde_json::Value {
        serde_json::json!([{
            "question_id": "q1",
            "question_type": "single-session-user",
            "question": "what degree?",
            "answer": "Business",
            "question_date": "2023/05/30 (Tue) 23:40",
            "haystack_dates": ["2023/05/20 (Sat) 02:21"],
            "haystack_session_ids": ["sharegpt_abc_0"],
            "haystack_sessions": [[
                {"role": "user", "content": "I studied Business", "has_answer": true},
                {"role": "assistant", "content": "Nice."}
            ]],
            "answer_session_ids": ["sharegpt_abc_0"]
        }])
    }

    #[test]
    fn parses_the_dataset_shape() {
        let qs: Vec<Question> = serde_json::from_value(sample_json()).unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].haystack_sessions[0][0].role, "user");
        assert!(!qs[0].is_abstention());
    }

    #[test]
    fn gold_answer_unwraps_a_json_string_and_renders_other_shapes() {
        let mut qs: Vec<Question> = serde_json::from_value(sample_json()).unwrap();
        assert_eq!(qs[0].gold_answer_text(), "Business");
        qs[0].answer = serde_json::json!(["a", "b"]);
        assert_eq!(qs[0].gold_answer_text(), "[\"a\",\"b\"]");
    }

    #[test]
    fn abstention_is_detected_from_the_id_suffix() {
        let mut qs: Vec<Question> = serde_json::from_value(sample_json()).unwrap();
        qs[0].question_id = "q1_abs".into();
        assert!(qs[0].is_abstention());
    }

    #[test]
    fn a_tampered_dataset_fails_the_pin() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("longmemeval_s.json");
        std::fs::write(&p, serde_json::to_vec(&sample_json()).unwrap()).unwrap();
        let err = load(&p).unwrap_err().to_string();
        assert!(err.contains("sha256 mismatch"), "{err}");
    }
}
