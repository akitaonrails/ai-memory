//! Opt-in, live-LLM QA-accuracy mode for the retrieval harness (R2b).
//!
//! R2a scores *retrieval* (did the evidence session surface in the top k).
//! This module measures the next hop: end-to-end **answer quality**. Per
//! scored question, with QA enabled, it
//!
//! 1. obtains a candidate answer — either the server's own `answer` field
//!    (future `answer=true` feature; used verbatim when present) or, by
//!    default, a harness synthesis call over the retrieved snippets (the
//!    exact context an agent would receive);
//! 2. grades that candidate against the dataset's GOLD answer with an
//!    LLM-as-judge, following the LongMemEval convention (a boolean verdict
//!    plus a one-line reason);
//! 3. records the verdict, the answer source, and the answer-path latency +
//!    token estimate, which [`super::score`] aggregates into QA accuracy
//!    alongside the R2a triple.
//!
//! This makes REAL provider API calls and is gated behind a key. It is OFF
//! by default; when it is requested but a provider/key cannot be resolved,
//! the caller SKIPS QA (retrieval metrics are unaffected) rather than
//! failing the run. Provider construction and structured output reuse the
//! same `ai-memory-llm` path the product uses; keys are read from env by the
//! provider auth boundary and never logged, printed, or written to reports.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Result, bail};
use schemars::JsonSchema;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use ai_memory_llm::{
    AuthRequirement, ChatMessage, ChatRequest, LlmProvider, ProviderAuth, ProviderChoice,
    ProviderConfig, Role, complete_structured,
};

use super::RetrievalArgs;
use super::query::{HitContext, QueryOutcome};
use super::report::QaReportMeta;

/// chars/4 heuristic divisor, matching `query.rs`'s context-token estimate.
const CHARS_PER_TOKEN: usize = 4;
/// Cap on synthesized-answer output tokens (LongMemEval answers are short).
const ANSWER_MAX_TOKENS: u32 = 512;
/// Cap on grader output tokens (a verdict plus one sentence).
const GRADE_MAX_TOKENS: u32 = 256;

/// Per-question QA outcome: a candidate answer graded against the gold
/// answer. Serialized into the per-question report; carries no key material.
#[derive(Debug, Clone, Serialize)]
pub struct QaOutcome {
    /// LLM-judge verdict: did the candidate correctly answer the question?
    pub correct: bool,
    /// Where the candidate came from: `synthesized` (harness LLM over the
    /// retrieved snippets) or `memory_query` (a server-provided `answer`).
    pub answer_source: &'static str,
    /// The candidate answer that was graded (kept for forensics).
    pub answer: String,
    /// One-line judge rationale.
    pub grade_reason: String,
    /// Wall-clock of the answer step, milliseconds. 0 when the answer came
    /// straight from the `memory_query` result (no synthesis call was made).
    pub answer_latency_ms: u128,
    /// chars/4 estimate over the synthesis prompt + answer — the answer-path
    /// analogue of the retrieval context-token estimate. 0 for a
    /// server-provided answer.
    pub answer_tokens: usize,
}

/// Answer synthesized from retrieved snippets (structured output).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct SynthesizedAnswer {
    /// The answer to the question, grounded strictly in the snippets, or
    /// "I don't know." when the snippets do not contain it.
    answer: String,
}

/// LLM-as-judge verdict (structured output), LongMemEval-style.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct Grade {
    /// True when the candidate answer is factually correct for the question,
    /// judged against the gold answer.
    correct: bool,
    /// One-sentence justification for the verdict.
    reason: String,
}

/// A resolved QA engine: the answerer + grader providers and their names,
/// built once and shared (cloned `Arc`) across concurrent question tasks.
pub struct QaEngine {
    answerer: Arc<dyn LlmProvider>,
    grader: Arc<dyn LlmProvider>,
    answer_provider: String,
    answer_model: String,
    grader_provider: String,
    grader_model: String,
}

impl QaEngine {
    /// Resolve both providers from `--qa-*` args + env.
    ///
    /// Returns `Err` (which the caller turns into a clean SKIP) when QA is
    /// requested but a provider is missing, an API key/token cannot be
    /// resolved, or provider construction fails. The default env var for a
    /// provider's key is used when the operator does not name one, so
    /// `--qa --qa-provider gemini --qa-model <m>` works with `GEMINI_API_KEY`
    /// already in the environment.
    pub fn resolve(args: &RetrievalArgs) -> Result<Self> {
        let Some(answer_provider_str) = args.qa_provider.as_deref() else {
            bail!("QA mode needs --qa-provider (e.g. anthropic|gemini|openai|openai-compat)");
        };
        let Some(answer_model) = args.qa_model.as_deref() else {
            bail!("QA mode needs --qa-model");
        };
        let answerer = build_llm_provider(
            "answerer",
            answer_provider_str,
            answer_model,
            args.qa_base_url.as_deref(),
            args.qa_api_key.as_deref(),
            args.qa_api_key_env.as_deref(),
            args.qa_token_file.as_deref(),
        )?;

        // The grader defaults to the answerer's provider/model/key/URL (the
        // common LongMemEval setup: one judge for everything), each field
        // overridable independently.
        let grader_provider_str = args
            .qa_grader_provider
            .as_deref()
            .unwrap_or(answer_provider_str);
        let grader_model = args.qa_grader_model.as_deref().unwrap_or(answer_model);
        let grader = build_llm_provider(
            "grader",
            grader_provider_str,
            grader_model,
            args.qa_grader_base_url.or_ref(&args.qa_base_url),
            args.qa_grader_api_key.or_ref(&args.qa_api_key),
            args.qa_grader_api_key_env.or_ref(&args.qa_api_key_env),
            args.qa_grader_token_file
                .as_deref()
                .or(args.qa_token_file.as_deref()),
        )?;

        Ok(Self {
            answer_provider: answerer.name().to_string(),
            answer_model: answerer.model().to_string(),
            grader_provider: grader.name().to_string(),
            grader_model: grader.model().to_string(),
            answerer,
            grader,
        })
    }

    /// Report provenance (names only — never keys).
    pub fn report_meta(&self) -> QaReportMeta {
        QaReportMeta {
            answer_provider: self.answer_provider.clone(),
            answer_model: self.answer_model.clone(),
            grader_provider: self.grader_provider.clone(),
            grader_model: self.grader_model.clone(),
        }
    }

    /// Short one-line description for the run header (names only).
    pub fn summary(&self) -> String {
        format!(
            "answerer={} {} | grader={} {}",
            self.answer_provider, self.answer_model, self.grader_provider, self.grader_model
        )
    }

    /// Produce and grade a candidate answer for one question.
    pub async fn evaluate(
        &self,
        question: &str,
        gold_answer: &str,
        outcome: &QueryOutcome,
    ) -> Result<QaOutcome> {
        let (answer, answer_source, answer_latency_ms, answer_tokens) = match &outcome.answer {
            // Future `answer=true` feature: the server already produced the
            // answer, so grade it directly (no synthesis call).
            Some(server_answer) => (server_answer.clone(), "memory_query", 0u128, 0usize),
            None => {
                let synth = self.synthesize(question, &outcome.contexts).await?;
                (synth.answer, "synthesized", synth.latency_ms, synth.tokens)
            }
        };
        let grade = self.grade(question, gold_answer, &answer).await?;
        Ok(QaOutcome {
            correct: grade.correct,
            answer_source,
            answer,
            grade_reason: grade.reason,
            answer_latency_ms,
            answer_tokens,
        })
    }

    /// Synthesize an answer from the retrieved snippets (the context an agent
    /// would actually get), measuring latency and a chars/4 token estimate.
    async fn synthesize(&self, question: &str, contexts: &[HitContext]) -> Result<SynthResult> {
        let user = synthesis_user_prompt(question, contexts);
        let prompt_chars = SYNTHESIS_SYSTEM.chars().count() + user.chars().count();
        let request = ChatRequest {
            system: Some(SYNTHESIS_SYSTEM.to_string()),
            messages: vec![ChatMessage {
                role: Role::User,
                content: user,
            }],
            max_tokens: ANSWER_MAX_TOKENS,
            temperature: Some(0.0),
        };
        let started = Instant::now();
        let out: SynthesizedAnswer = complete_structured(self.answerer.as_ref(), request).await?;
        let latency_ms = started.elapsed().as_millis();
        let tokens = (prompt_chars + out.answer.chars().count()).div_ceil(CHARS_PER_TOKEN);
        Ok(SynthResult {
            answer: out.answer,
            latency_ms,
            tokens,
        })
    }

    /// Grade a candidate answer against the gold answer (LLM-as-judge).
    async fn grade(&self, question: &str, gold: &str, candidate: &str) -> Result<Grade> {
        let request = ChatRequest {
            system: Some(GRADE_SYSTEM.to_string()),
            messages: vec![ChatMessage {
                role: Role::User,
                content: grade_user_prompt(question, gold, candidate),
            }],
            max_tokens: GRADE_MAX_TOKENS,
            temperature: Some(0.0),
        };
        let grade: Grade = complete_structured(self.grader.as_ref(), request).await?;
        Ok(grade)
    }
}

/// Internal synthesis result carrier.
struct SynthResult {
    answer: String,
    latency_ms: u128,
    tokens: usize,
}

/// Convenience: borrow an inner `Option<String>` as `Option<&str>`, falling
/// back to a default, so grader overrides read cleanly.
trait OrRef {
    fn or_ref<'a>(&'a self, fallback: &'a Option<String>) -> Option<&'a str>;
}
impl OrRef for Option<String> {
    fn or_ref<'a>(&'a self, fallback: &'a Option<String>) -> Option<&'a str> {
        self.as_deref().or(fallback.as_deref())
    }
}

const SYNTHESIS_SYSTEM: &str = "You answer a user's question using ONLY the memory snippets \
    retrieved for them. Base your answer strictly on those snippets — do not use outside \
    knowledge or guess. Be concise and factual. If the snippets do not contain the answer, \
    reply exactly: I don't know.";

const GRADE_SYSTEM: &str = "You are a strict grader for a long-term-memory QA benchmark. Given a \
    question, the gold (reference) answer, and a candidate answer, decide whether the candidate \
    is CORRECT. Judge factual correctness, not wording: a paraphrase or extra detail is fine \
    when it matches the gold answer's facts. Mark it incorrect when it contradicts the gold \
    answer, omits the key fact the question asks for, or declines to answer. Return a boolean \
    `correct` and a one-sentence `reason`.";

/// Build the synthesis user prompt: the question plus ranked snippets.
fn synthesis_user_prompt(question: &str, contexts: &[HitContext]) -> String {
    let mut prompt = format!("Question:\n{question}\n\n");
    if contexts.is_empty() {
        prompt.push_str("Retrieved memory snippets: (none)\n");
    } else {
        prompt.push_str("Retrieved memory snippets (ranked, best first):\n");
        for (i, c) in contexts.iter().enumerate() {
            prompt.push_str(&format!("[{}] {}\n{}\n\n", i + 1, c.title, c.snippet));
        }
    }
    prompt.push_str("\nAnswer the question using only the snippets above.");
    prompt
}

/// Build the grader user prompt.
fn grade_user_prompt(question: &str, gold: &str, candidate: &str) -> String {
    format!(
        "Question:\n{question}\n\nGold answer:\n{gold}\n\nCandidate answer:\n{candidate}\n\n\
         Is the candidate answer correct?"
    )
}

/// Resolve one provider from explicit fields (mirrors the `ab` harness's
/// resolution, reused here for the QA answerer + grader). Fails closed when a
/// required key/token is absent so the caller can SKIP cleanly.
fn build_llm_provider(
    role: &str,
    provider_str: &str,
    model: &str,
    base_url: Option<&str>,
    api_key_inline: Option<&str>,
    api_key_env: Option<&str>,
    token_file: Option<&std::path::Path>,
) -> Result<Arc<dyn LlmProvider>> {
    let provider = match provider_str {
        "anthropic" => ProviderChoice::Anthropic,
        "openai" => ProviderChoice::OpenAi,
        "openai-compat" | "openai_compat" => ProviderChoice::OpenAiCompat,
        "openai-oauth" | "openai_oauth" => ProviderChoice::OpenAiOAuth,
        "codex" => ProviderChoice::Codex,
        "copilot" | "github-copilot" | "github_copilot" => ProviderChoice::Copilot,
        "gemini" | "google" => ProviderChoice::Gemini,
        other => bail!(
            "{role}: provider `{other}` not one of \
             anthropic|openai|openai-compat|openai-oauth|codex|copilot|gemini"
        ),
    };

    // The env var to read the key from: operator override, else the
    // provider's own canonical env var.
    let requirement = provider.auth_requirement();
    let default_env = match &requirement {
        AuthRequirement::RequiredApiKey { env_var }
        | AuthRequirement::OptionalApiKey { env_var } => Some(*env_var),
        _ => None,
    };
    let eff_env = api_key_env.or(default_env);

    let api_key = match api_key_inline {
        Some(s) if !s.is_empty() => Some(SecretString::from(s.to_string())),
        _ => eff_env.and_then(|env| match std::env::var(env) {
            Ok(v) if !v.is_empty() => Some(SecretString::from(v)),
            _ => None,
        }),
    };

    let auth = match requirement {
        AuthRequirement::RequiredApiKey { env_var } => {
            if api_key.is_none() {
                bail!(
                    "{role}: provider `{provider_str}` requires an API key; \
                     set {} (or pass --qa-api-key)",
                    eff_env.unwrap_or(env_var)
                );
            }
            ProviderAuth::required_api_key_from_env(env_var, api_key)
        }
        AuthRequirement::OptionalApiKey { env_var } => {
            ProviderAuth::optional_api_key_from_env(env_var, api_key)
        }
        AuthRequirement::OpenAiOAuthToken => {
            let tf = token_file.ok_or_else(|| {
                anyhow::anyhow!("{role}: --qa-token-file required for openai-oauth")
            })?;
            ProviderAuth::openai_oauth_token_file(tf.to_path_buf())
        }
        AuthRequirement::CodexAuthFile => {
            let tf = token_file
                .ok_or_else(|| anyhow::anyhow!("{role}: --qa-token-file required for codex"))?;
            ProviderAuth::codex(tf.to_path_buf(), "codex")
        }
        AuthRequirement::CopilotToken => {
            let tf = token_file
                .ok_or_else(|| anyhow::anyhow!("{role}: --qa-token-file required for copilot"))?;
            ProviderAuth::copilot(tf.to_path_buf(), None, None, base_url.map(str::to_string))
        }
        AuthRequirement::AnthropicOAuthToken => ProviderAuth::anthropic_oauth_token(api_key),
    };

    let config = ProviderConfig {
        provider,
        model: model.to_string(),
        auth,
        base_url: base_url.map(str::to_string),
        compat_strict: true,
        request_timeout_secs: ai_memory_llm::DEFAULT_REQUEST_TIMEOUT_SECS,
        reasoning_effort: None,
        extra_headers: ai_memory_llm::ExtraHeaders::default(),
    };
    ai_memory_llm::build_provider(config)
        .map_err(anyhow::Error::from)
        .map_err(|e| e.context(format!("{role}: building provider `{provider_str}`")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(title: &str, snippet: &str) -> HitContext {
        HitContext {
            title: title.into(),
            snippet: snippet.into(),
        }
    }

    #[test]
    fn synthesis_prompt_numbers_snippets_and_carries_the_question() {
        let p = synthesis_user_prompt(
            "What car?",
            &[ctx("Car", "A blue Civic"), ctx("More", "2019")],
        );
        assert!(p.contains("Question:\nWhat car?"));
        assert!(p.contains("[1] Car\nA blue Civic"));
        assert!(p.contains("[2] More\n2019"));
        assert!(p.contains("only the snippets"));
    }

    #[test]
    fn synthesis_prompt_handles_no_snippets() {
        let p = synthesis_user_prompt("Anything?", &[]);
        assert!(p.contains("(none)"), "{p}");
    }

    #[test]
    fn grade_prompt_lays_out_all_three_fields() {
        let p = grade_user_prompt("Q?", "gold-x", "cand-y");
        assert!(p.contains("Question:\nQ?"));
        assert!(p.contains("Gold answer:\ngold-x"));
        assert!(p.contains("Candidate answer:\ncand-y"));
    }

    #[test]
    fn unknown_provider_is_rejected() {
        let err = build_llm_provider("answerer", "nope", "m", None, None, None, None)
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not one of"), "{err}");
    }

    #[test]
    fn required_key_provider_without_a_key_fails_closed() {
        // Point at an env var that is (almost certainly) unset, and pass no
        // inline key: a required-key provider must refuse so the caller skips.
        let err = build_llm_provider(
            "answerer",
            "gemini",
            "gemini-2.5-flash",
            None,
            None,
            Some("AI_MEMORY_EVAL_QA_DEFINITELY_UNSET_KEY"),
            None,
        )
        .map(|_| ())
        .unwrap_err()
        .to_string();
        assert!(err.contains("requires an API key"), "{err}");
    }

    /// Live end-to-end QA smoke over a hand-made retrieval outcome (no eval
    /// server needed). `#[ignore]` so CI without keys stays green; even when
    /// run explicitly it skips cleanly when no key is present.
    #[tokio::test]
    #[ignore = "live LLM: requires GEMINI_API_KEY or ANTHROPIC_API_KEY"]
    async fn live_synthesize_and_grade_smoke() {
        let (provider, model, key_env) = if std::env::var("GEMINI_API_KEY").is_ok() {
            ("gemini", "gemini-2.5-flash", "GEMINI_API_KEY")
        } else if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            ("anthropic", "claude-3-5-haiku-latest", "ANTHROPIC_API_KEY")
        } else {
            eprintln!("skipping: no GEMINI_API_KEY / ANTHROPIC_API_KEY in env");
            return;
        };
        let answerer =
            build_llm_provider("answerer", provider, model, None, None, Some(key_env), None)
                .expect("provider builds with a key present");
        let engine = QaEngine {
            answer_provider: answerer.name().to_string(),
            answer_model: answerer.model().to_string(),
            grader_provider: answerer.name().to_string(),
            grader_model: answerer.model().to_string(),
            grader: answerer.clone(),
            answerer,
        };
        let outcome = QueryOutcome {
            retrieved: vec![],
            contexts: vec![ctx("Pet", "The user has a dog named Rex.")],
            answer: None,
            latency: std::time::Duration::ZERO,
            context_tokens: 0,
        };
        let qa = engine
            .evaluate("What is the user's dog's name?", "Rex", &outcome)
            .await
            .expect("QA evaluate succeeds with a live provider");
        assert_eq!(qa.answer_source, "synthesized");
        assert!(qa.correct, "expected the judge to accept: {qa:?}");
    }
}
