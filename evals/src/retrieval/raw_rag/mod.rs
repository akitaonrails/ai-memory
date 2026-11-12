//! Raw-RAG reference for the evaluator, independent of the production store.
//!
//! Protocol adapted from arXiv:2610.30755v1, Appendix B: overlapping source
//! windows, dense top-128, cross-encoder top-15, optional full-turn display.
//! This uses pinned MiniLM models and WordPiece, not the paper's models or
//! GPT-4 tokenization; its scores are not a reproduction of the paper.

mod chunks;
mod model;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use ai_memory_llm::{Embedder, LocalEmbedder};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokenizers::Tokenizer;

use super::{RunContext, dataset::Question, ingest::stored_session_uuid, query, report, score};

const CHUNK_TOKENS: usize = 256;
const OVERLAP_TOKENS: usize = 32;
const CANDIDATES: usize = 128;
const TOP_K: usize = 15;
const MODELS: &str = "evals/models";

#[derive(clap::Args, Debug)]
pub(super) struct Args {
    /// Raw-RAG source window length in local WordPiece tokens (not GPT-4 tokens).
    #[arg(long, default_value_t = CHUNK_TOKENS)]
    raw_rag_chunk_tokens: usize,
    /// Raw-RAG overlap between adjacent windows of one session.
    #[arg(long, default_value_t = OVERLAP_TOKENS)]
    raw_rag_overlap_tokens: usize,
    /// Dense candidates scored by the cross-encoder.
    #[arg(long, default_value_t = CANDIDATES)]
    raw_rag_candidates: usize,
    /// Reranked windows delivered as evidence. Cutoffs still score distinct sessions.
    #[arg(long, default_value_t = TOP_K)]
    raw_rag_top_k: usize,
    /// Display only selected windows instead of their complete intersecting turns.
    #[arg(long)]
    raw_rag_no_turn_expansion: bool,
    /// Root for both pinned local models. No model is fetched unless explicitly requested.
    #[arg(long, default_value = MODELS)]
    raw_rag_models: PathBuf,
    /// Explicitly download missing/invalid pinned models before Raw-RAG evaluation.
    #[arg(long)]
    raw_rag_fetch_models: bool,
}

impl Args {
    pub(super) fn is_default(&self) -> bool {
        self.raw_rag_chunk_tokens == CHUNK_TOKENS
            && self.raw_rag_overlap_tokens == OVERLAP_TOKENS
            && self.raw_rag_candidates == CANDIDATES
            && self.raw_rag_top_k == TOP_K
            && !self.raw_rag_no_turn_expansion
            && !self.raw_rag_fetch_models
            && self.raw_rag_models == std::path::Path::new(MODELS)
    }

    pub(super) fn validate(&self) -> Result<()> {
        if !(1..=510).contains(&self.raw_rag_chunk_tokens)
            || self.raw_rag_overlap_tokens >= self.raw_rag_chunk_tokens
        {
            bail!("chunk tokens must be 1..=510, with overlap smaller than the chunk");
        }
        if self.raw_rag_top_k == 0
            || self.raw_rag_candidates < self.raw_rag_top_k
            || self.raw_rag_candidates > 10_000
        {
            bail!("Raw-RAG needs 1 <= top-k <= candidates <= 10000");
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Metadata {
    protocol: &'static str,
    embedding_model: &'static str,
    embedding_files: Vec<(&'static str, &'static str)>,
    reranker_model: &'static str,
    reranker_revision: &'static str,
    reranker_files: Vec<(&'static str, &'static str)>,
    tokenizer: &'static str,
    chunk_tokens: usize,
    overlap_tokens: usize,
    candidates: usize,
    top_k: usize,
    turn_expansion: bool,
    window_boundaries: &'static str,
    repeated_turns: &'static str,
    model_load_ms: u128,
    pub per_question: Vec<QuestionMetadata>,
}

#[derive(Debug, Serialize)]
pub(crate) struct QuestionMetadata {
    question_id: String,
    windows_indexed: usize,
    candidates_scored: usize,
    selected_windows: Vec<WindowMetadata>,
    index_ms: u128,
    dense_ms: u128,
    rerank_ms: u128,
}

#[derive(Debug, Serialize)]
struct WindowMetadata {
    session_id: uuid::Uuid,
    token_start: usize,
    token_end: usize,
    byte_start: usize,
    byte_end: usize,
    cross_encoder_logit: f32,
}

pub(crate) fn markdown(metadata: &Metadata) -> String {
    let windows: usize = metadata
        .per_question
        .iter()
        .map(|q| q.windows_indexed)
        .sum();
    let index_ms: u128 = metadata.per_question.iter().map(|q| q.index_ms).sum();
    format!(
        "\n### Raw-RAG protocol\n\n\
         - reference: arXiv:2610.30755v1, Appendix B; local-model adaptation, not paper-score reproduction\n\
         - embedder: {}; reranker: {} at `{}`\n\
         - tokenizer: {}; windows {} tokens, overlap {}; session boundaries kept\n\
         - dense candidates: {}; selected windows: {}; full-turn display: {}\n\
         - {} windows indexed in {:.3} s; model load {:.3} s (downloads excluded)\n\
         - query latency excludes indexing; context tokens use the same chars/4 estimate as server runs\n\
         - hit/recall cutoffs count distinct sessions, not chunks; repeated selected turns are retained\n\n\
         Notes: hit@k needs any evidence session; recall@k measures the fraction found; \
         recall-all@k requires all evidence sessions. This engine indexes the supplied \
         benchmark history directly, without production capture or sanitization.\n",
        metadata.embedding_model,
        metadata.reranker_model,
        metadata.reranker_revision,
        metadata.tokenizer,
        metadata.chunk_tokens,
        metadata.overlap_tokens,
        metadata.candidates,
        metadata.top_k,
        metadata.turn_expansion,
        windows,
        index_ms as f64 / 1000.0,
        metadata.model_load_ms as f64 / 1000.0,
    )
}

fn load_tokenizer(root: &std::path::Path) -> Result<Tokenizer> {
    let path = ai_memory_llm::local::model_dir(root).join("tokenizer.json");
    let mut tokenizer = Tokenizer::from_file(&path)
        .map_err(|e| anyhow::anyhow!("loading {}: {e}", path.display()))?;
    tokenizer.with_padding(None);
    tokenizer
        .with_truncation(None)
        .map_err(|e| anyhow::anyhow!("configuring tokenizer: {e}"))?;
    Ok(tokenizer)
}

/// Refuse inputs the shared local embedder would truncate. The byte bound
/// mirrors its `truncate_for_embedding(text, 512 * 4)` admission window.
fn check_embedding_input(tokenizer: &Tokenizer, text: &str) -> Result<()> {
    let encoding = tokenizer
        .encode(text, true)
        .map_err(|e| anyhow::anyhow!("tokenizing dense input: {e}"))?;
    if text.len() > 6_144 || encoding.len() > 512 {
        bail!(
            "dense input exceeds local embedder limits; reduce --raw-rag-chunk-tokens (no silent truncation)"
        );
    }
    Ok(())
}

pub(super) async fn run(run: &RunContext<'_>) -> Result<report::Report> {
    let args = &run.args.raw_rag;
    args.validate()?;
    if run.questions.is_empty() {
        bail!("Raw-RAG needs at least one scored question");
    }
    if run.args.ks.is_empty()
        || run
            .args
            .ks
            .iter()
            .any(|&k| k == 0 || k > args.raw_rag_top_k)
    {
        bail!("Raw-RAG --ks must be nonempty and within 1..=--raw-rag-top-k");
    }
    if args.raw_rag_fetch_models {
        ai_memory_llm::fetch_model(&args.raw_rag_models).await?;
        model::fetch(&args.raw_rag_models).await?;
    }
    let load_started = Instant::now();
    let embedder = LocalEmbedder::load(&args.raw_rag_models)
        .context("loading dense model; pass --raw-rag-fetch-models to fetch pinned models")?;
    let tokenizer = load_tokenizer(&args.raw_rag_models)?;
    let reranker = Arc::new(model::CrossEncoder::load(&args.raw_rag_models)?);
    let model_load_ms = load_started.elapsed().as_millis();
    let mut scores = Vec::new();
    let mut per_question = Vec::new();
    for question in run.questions {
        let (outcome, metadata) = retrieve(question, args, &embedder, &tokenizer, &reranker)
            .await
            .with_context(|| format!("Raw-RAG question {}", question.question_id))?;
        let evidence: HashSet<uuid::Uuid> = question
            .answer_session_ids
            .iter()
            .map(|id| stored_session_uuid(question, id))
            .collect();
        let mut score = score::score_question(
            &question.question_id,
            &question.question_type,
            &evidence,
            &outcome.retrieved,
            &run.args.ks,
            outcome.latency.as_millis(),
            outcome.context_tokens,
        );
        score.ingest_ms = metadata.index_ms;
        if let Some(qa) = &run.qa {
            match qa
                .evaluate(&question.question, &question.gold_answer_text(), &outcome)
                .await
            {
                Ok(result) => score.qa = Some(result),
                Err(error) => {
                    tracing::warn!(question = question.question_id, error = %error, "QA failed; question left ungraded")
                }
            }
        }
        tracing::info!(
            question = question.question_id,
            windows = metadata.windows_indexed,
            candidates = metadata.candidates_scored,
            "Raw-RAG scored"
        );
        scores.push(score);
        per_question.push(metadata);
    }
    scores.sort_by(|a, b| a.question_id.cmp(&b.question_id));
    let metadata = Metadata {
        protocol: "Raw-RAG local-model adaptation (arXiv:2610.30755v1 Appendix B)",
        embedding_model: ai_memory_llm::LOCAL_MODEL,
        embedding_files: ai_memory_llm::local::MODEL_FILES.to_vec(),
        reranker_model: model::MODEL,
        reranker_revision: model::REVISION,
        reranker_files: model::FILES.to_vec(),
        tokenizer: "all-MiniLM-L6-v2 WordPiece (not GPT-4)",
        chunk_tokens: args.raw_rag_chunk_tokens,
        overlap_tokens: args.raw_rag_overlap_tokens,
        candidates: args.raw_rag_candidates,
        top_k: args.raw_rag_top_k,
        turn_expansion: !args.raw_rag_no_turn_expansion,
        window_boundaries: "within each supplied session; no cross-session windows",
        repeated_turns: "retained in selected-window relevance order",
        model_load_ms,
        per_question,
    };
    Ok(report::Report {
        generated_at: jiff::Timestamp::now().to_string(),
        commit: run.commit.to_string(),
        hardware: report::hardware(),
        dataset: run.variant.name,
        dataset_sha256: run.variant.sha256,
        mode: "raw-rag",
        config: format!(
            "engine=raw-rag chunks={} overlap={} candidates={} top_k={} turn_expansion={} concurrency=1",
            args.raw_rag_chunk_tokens,
            args.raw_rag_overlap_tokens,
            args.raw_rag_candidates,
            args.raw_rag_top_k,
            !args.raw_rag_no_turn_expansion
        ),
        embedding_cache: false,
        questions_scored: scores.len(),
        abstention_excluded: run.abstention_n,
        ks: run.args.ks.clone(),
        qa: run.qa.as_ref().map(|engine| engine.report_meta()),
        ingest: None,
        reachability: None,
        raw_rag: Some(metadata),
        slices: score::aggregate(&scores, &run.args.ks),
        per_question: scores,
    })
}

async fn retrieve(
    question: &Question,
    args: &Args,
    embedder: &LocalEmbedder,
    tokenizer: &Tokenizer,
    reranker: &Arc<model::CrossEncoder>,
) -> Result<(query::QueryOutcome, QuestionMetadata)> {
    let index_started = Instant::now();
    let sources = chunks::sources(question)?;
    let chunks = chunks::windows(
        &sources,
        tokenizer,
        args.raw_rag_chunk_tokens,
        args.raw_rag_overlap_tokens,
    )?;
    if chunks.is_empty() {
        bail!("question has no source windows");
    }
    let mut vectors = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        let text = chunk.text(&sources);
        check_embedding_input(tokenizer, text)?;
        vectors.push(embedder.embed_document(text).await?);
    }
    let index_ms = index_started.elapsed().as_millis();
    let started = Instant::now();
    check_embedding_input(tokenizer, &question.question)?;
    let query = embedder.embed_query(&question.question).await?;
    let candidates = chunks::dense_order(&query, &vectors, args.raw_rag_candidates)?;
    let dense_ms = started.elapsed().as_millis();
    let candidates_scored = candidates.len();
    let passages: Vec<(usize, String)> = candidates
        .iter()
        .map(|&index| (index, chunks[index].text(&sources).to_owned()))
        .collect();
    let model = Arc::clone(reranker);
    let question_text = question.question.clone();
    let rerank_started = Instant::now();
    let mut selected = tokio::task::spawn_blocking(move || {
        passages
            .into_iter()
            .map(|(index, text)| Ok((index, model.score(&question_text, &text)?)))
            .collect::<Result<Vec<_>>>()
    })
    .await
    .context("cross-encoder task panicked")??;
    selected.sort_by(|a, b| b.1.total_cmp(&a.1));
    selected.truncate(args.raw_rag_top_k);
    let rerank_ms = rerank_started.elapsed().as_millis();
    let mut contexts = Vec::new();
    let mut retrieved = Vec::new();
    let mut selected_windows = Vec::new();
    for (index, logit) in selected {
        let chunk = &chunks[index];
        let source = &sources[chunk.source];
        contexts.push(query::HitContext {
            title: source.title.clone(),
            snippet: chunk
                .evidence(&sources, !args.raw_rag_no_turn_expansion)
                .to_owned(),
        });
        retrieved.push(query::Retrieved {
            session_uuid: Some(source.id),
        });
        selected_windows.push(WindowMetadata {
            session_id: source.id,
            token_start: chunk.tokens.start,
            token_end: chunk.tokens.end,
            byte_start: chunk.bytes.start,
            byte_end: chunk.bytes.end,
            cross_encoder_logit: logit,
        });
    }
    let chars: usize = contexts
        .iter()
        .map(|hit| hit.title.chars().count() + hit.snippet.chars().count())
        .sum();
    Ok((
        query::QueryOutcome {
            retrieved,
            contexts,
            answer: None,
            latency: started.elapsed(),
            context_tokens: chars.div_ceil(4),
            raw_hits: 0,
        },
        QuestionMetadata {
            question_id: question.question_id.clone(),
            windows_indexed: chunks.len(),
            candidates_scored,
            selected_windows,
            index_ms,
            dense_ms,
            rerank_ms,
        },
    ))
}
