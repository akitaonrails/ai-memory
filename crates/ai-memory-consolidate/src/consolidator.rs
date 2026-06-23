//! Single-page session consolidator.
//!
//! Reads the observation log for a session, asks the configured LLM
//! for an updated [`ConsolidatedPage`], then writes it via
//! [`Wiki::write_page`] so the supersession chain + git auto-commit
//! kicks in automatically.

use std::sync::Arc;

use ai_memory_core::{Observation, PagePath, ProjectId, SessionId, Tier, WorkspaceId};
use ai_memory_llm::{ChatMessage, ChatRequest, LlmError, LlmProvider, Role, complete_structured};
use ai_memory_store::{ReaderPool, WriterHandle};
use ai_memory_wiki::{AdmissionContext, AdmissionOp, Wiki, WritePageRequest};
use thiserror::Error;
use tracing::{debug, info};

use crate::projection::{ObservationProjectionConfig, project_observations_prefer_recent};
use crate::types::{ConsolidatedBatch, ConsolidatedPage, ConsolidationOutcome, SlotKind};

/// Errors raised by the consolidator.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConsolidatorError {
    /// Domain-level error (e.g. invalid `PagePath`).
    #[error(transparent)]
    Memory(#[from] ai_memory_core::MemoryError),

    /// Underlying store error.
    #[error(transparent)]
    Store(#[from] ai_memory_store::StoreError),

    /// Underlying wiki error.
    #[error(transparent)]
    Wiki(#[from] ai_memory_wiki::WikiError),

    /// Underlying LLM error.
    #[error(transparent)]
    Llm(#[from] LlmError),

    /// JSON error.
    #[error("serde: {0}")]
    Serde(String),

    /// Session was not found.
    #[error("session not found: {0}")]
    SessionNotFound(SessionId),

    /// Session had no observations to consolidate.
    #[error("session {0} has no observations")]
    EmptySession(SessionId),
}

impl From<serde_json::Error> for ConsolidatorError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serde(value.to_string())
    }
}

/// Result alias used by the consolidator.
pub type ConsolidatorResult<T> = Result<T, ConsolidatorError>;

/// Karpathy-style single-page consolidator. Holds handles to the
/// store, wiki, and LLM provider so it can be reused across many
/// `consolidate_session` calls.
pub struct Consolidator {
    reader: ReaderPool,
    writer: WriterHandle,
    wiki: Wiki,
    llm: Arc<dyn LlmProvider>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
}

impl Consolidator {
    /// Construct a consolidator. Caller is responsible for selecting
    /// the LLM provider via the `ai-memory-llm` factory.
    #[must_use]
    pub fn new(
        reader: ReaderPool,
        writer: WriterHandle,
        wiki: Wiki,
        llm: Arc<dyn LlmProvider>,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> Self {
        Self {
            reader,
            writer,
            wiki,
            llm,
            workspace_id,
            project_id,
        }
    }

    /// Consolidate a single session into a refreshed
    /// `sessions/<id>.md` page.
    ///
    /// # Errors
    /// Returns [`ConsolidatorError`] for any store, wiki, or LLM
    /// failure.
    pub async fn consolidate_session(
        &self,
        session_id: SessionId,
        dry_run: bool,
    ) -> ConsolidatorResult<ConsolidationOutcome> {
        let observations = self.reader.observations_for_session(session_id).await?;
        if observations.is_empty() {
            return Err(ConsolidatorError::EmptySession(session_id));
        }

        // Look up the session's actual (workspace, project) IDs — the
        // hook router stamped them per-cwd at session start, so this
        // is the correct target for the resulting wiki page. The
        // server's startup IDs (self.workspace_id / self.project_id)
        // are the fallback for sessions that pre-date per-cwd routing.
        let (ws, proj) = self
            .reader
            .session_project_ids(session_id)
            .await?
            .unwrap_or((self.workspace_id, self.project_id));

        let path = PagePath::new(format!("sessions/{session_id}.md"))?;
        let current_body = self
            .wiki
            .read_page(ws, proj, &path)
            .map(|md| md.body)
            .unwrap_or_default();
        let request = build_request(session_id, &observations, &current_body);
        debug!(
            session = %session_id,
            provider = self.llm.name(),
            model = self.llm.model(),
            "consolidating session"
        );
        let page: ConsolidatedPage = complete_structured(&*self.llm, request).await?;

        if dry_run {
            return Ok(ConsolidationOutcome {
                path,
                dry_run: true,
                new_title: page.title,
                new_body_markdown: page.body_markdown,
                page_id: None,
                tags: page.tags,
            });
        }

        let frontmatter = build_frontmatter(&page);
        let id = self
            .wiki
            .write_page(WritePageRequest {
                workspace_id: ws,
                project_id: proj,
                path: path.clone(),
                frontmatter,
                body: page.body_markdown.clone(),
                tier: Tier::Episodic,
                pinned: false,
                title: None,
                admission_ctx: Some(AdmissionContext {
                    op: AdmissionOp::Consolidate,
                    ..Default::default()
                }),
                author_id: None,
                actor: ai_memory_core::ActorContext::anonymous(),
            })
            .await?;
        // Auto-commit the result so the supersession lands in git.
        let _ = self
            .wiki
            .commit_all(&format!(
                "consolidate(session {}): {}",
                short_id(&session_id.to_string()),
                page.title.chars().take(60).collect::<String>(),
            ))
            .map_err(|e| {
                tracing::warn!(error = %e, "consolidate auto-commit failed");
                e
            });
        info!(
            session = %session_id,
            page = %id,
            "session consolidated via LLM",
        );
        Ok(ConsolidationOutcome {
            path,
            dry_run: false,
            new_title: page.title,
            new_body_markdown: page.body_markdown,
            page_id: Some(id),
            tags: page.tags,
        })
    }

    /// Borrow the underlying writer (used by the MCP tool to ack the
    /// consolidate operation in the audit log).
    #[must_use]
    pub fn writer(&self) -> &WriterHandle {
        &self.writer
    }

    /// Borrow the underlying LLM provider. Used by lightweight LLM
    /// callers (`memory_explore`) that want to issue a one-shot
    /// completion without going through the full consolidate
    /// pipeline.
    #[must_use]
    pub fn llm(&self) -> Arc<dyn ai_memory_llm::LlmProvider> {
        self.llm.clone()
    }

    fn should_skip_high_resistance_slot_update(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        req: &WritePageRequest,
    ) -> ConsolidatorResult<bool> {
        if !is_slot_path(&req.path) {
            return Ok(false);
        }
        let existing = match self.wiki.read_page(workspace_id, project_id, &req.path) {
            Ok(md) => Some(md.frontmatter),
            Err(ai_memory_wiki::WikiError::Io(err))
                if err.kind() == std::io::ErrorKind::NotFound =>
            {
                None
            }
            Err(err) => return Err(err.into()),
        };
        Ok(should_skip_high_resistance_slot_update_from_frontmatter(
            &req.path,
            existing.as_ref(),
            &req.frontmatter,
        ))
    }

    async fn slot_snapshots(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> ConsolidatorResult<Vec<SlotSnapshot>> {
        let briefing = self
            .reader
            .briefing_for_project(workspace_id, project_id, 100)
            .await?;
        let mut slots = Vec::with_capacity(briefing.slots.len());
        for slot in briefing.slots {
            let path = PagePath::new(slot.path)?;
            let md = self.wiki.read_page(workspace_id, project_id, &path)?;
            slots.push(SlotSnapshot {
                path: path.as_str().to_string(),
                title: slot.title,
                slot_kind: slot_kind_from_frontmatter(&md.frontmatter),
                body: md.body,
            });
        }
        Ok(slots)
    }

    /// M7b multi-page consolidation: ask the LLM for a batch of page
    /// updates spanning sessions/, concepts/, decisions/, then write
    /// them all atomically (one SQL transaction).
    ///
    /// # Errors
    /// Returns [`ConsolidatorError`] for any store, wiki, or LLM
    /// failure. On error, no pages are written and no files moved.
    pub async fn consolidate_session_multi(
        &self,
        session_id: SessionId,
        dry_run: bool,
    ) -> ConsolidatorResult<Vec<ConsolidationOutcome>> {
        let observations = self.reader.observations_for_session(session_id).await?;
        if observations.is_empty() {
            return Err(ConsolidatorError::EmptySession(session_id));
        }
        // Resolve the session's actual (workspace, project) IDs from
        // its row — see `consolidate_session` for the rationale.
        let (ws, proj) = self
            .reader
            .session_project_ids(session_id)
            .await?
            .unwrap_or((self.workspace_id, self.project_id));
        let slots = self.slot_snapshots(ws, proj).await?;
        let request = build_batch_request_with_slots(session_id, &observations, &slots);
        debug!(
            session = %session_id,
            provider = self.llm.name(),
            "consolidating session (multi-page)",
        );
        let batch: ConsolidatedBatch =
            ai_memory_llm::complete_structured(&*self.llm, request).await?;

        let mut requests = Vec::with_capacity(batch.updates.len());
        let mut outcomes_preview = Vec::with_capacity(batch.updates.len());
        for upd in &batch.updates {
            let (req, outcome) = build_update(ws, proj, upd, dry_run)?;
            if self.should_skip_high_resistance_slot_update(ws, proj, &req)? {
                debug!(
                    path = %req.path.as_str(),
                    "skipping invariant slot update without explicit invariant contradiction signal",
                );
                continue;
            }
            requests.push(req);
            outcomes_preview.push(outcome);
        }

        if dry_run {
            return Ok(outcomes_preview);
        }

        let ids = self.wiki.apply_batch(requests).await?;
        let rationale_short = batch.rationale.chars().take(60).collect::<String>();
        let _ = self.wiki.commit_all(&format!(
            "consolidate-batch(session {}): {} page(s) — {}",
            short_id(&session_id.to_string()),
            ids.len(),
            rationale_short,
        ));

        let outcomes = outcomes_preview
            .into_iter()
            .zip(ids)
            .map(|(mut o, id)| {
                o.dry_run = false;
                o.page_id = Some(id);
                o
            })
            .collect();
        Ok(outcomes)
    }
}

/// Convert one LLM-produced batch update into the
/// `(WritePageRequest, ConsolidationOutcome)` pair the consolidator
/// hands to `Wiki::apply_batch`. Pulled out of
/// `consolidate_session_multi` so the rule-routing + frontmatter
/// assembly can be exercised in isolation if needed.
///
/// M20 contract: when `upd.kind == Rule`, ALWAYS route to
/// `_rules/<slug>.md` regardless of the LLM's suggested path. The
/// lint pass relies on `_rules/` being the single sweep-able
/// location for rule pages.
fn build_update(
    ws: WorkspaceId,
    proj: ProjectId,
    upd: &crate::types::ConsolidatedPageUpdate,
    dry_run: bool,
) -> ConsolidatorResult<(WritePageRequest, ConsolidationOutcome)> {
    let final_path = if upd.kind == crate::types::PageKind::Rule {
        let slug = slugify_for_rule(&upd.title);
        format!("_rules/{slug}.md")
    } else {
        upd.path.clone()
    };
    let path = PagePath::new(final_path)?;
    let tier = upd.tier;

    let mut fm = serde_json::Map::new();
    fm.insert("title".into(), serde_json::Value::String(upd.title.clone()));
    fm.insert(
        "tier".into(),
        serde_json::Value::String(tier_as_str(tier).into()),
    );
    // M20: surface the semantic classification into frontmatter so
    // the lint pass + downstream tooling can branch on it without
    // re-classifying.
    fm.insert(
        "kind".into(),
        serde_json::Value::String(upd.kind.as_str().into()),
    );
    if !upd.tags.is_empty() {
        fm.insert(
            "tags".into(),
            serde_json::Value::Array(
                upd.tags
                    .iter()
                    .map(|t| serde_json::Value::String(t.clone()))
                    .collect(),
            ),
        );
    }
    if is_slot_path(&path) {
        fm.insert(
            "slot_kind".into(),
            serde_json::Value::String(upd.slot_kind.as_str().into()),
        );
    }
    fm.insert("consolidated".into(), serde_json::Value::Bool(true));

    let req = WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: path.clone(),
        frontmatter: serde_json::Value::Object(fm),
        body: upd.body_markdown.clone(),
        tier,
        pinned: false,
        title: Some(upd.title.clone()),
        admission_ctx: Some(AdmissionContext {
            op: AdmissionOp::Consolidate,
            ..Default::default()
        }),
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
    };
    let outcome = ConsolidationOutcome {
        path,
        dry_run,
        new_title: upd.title.clone(),
        new_body_markdown: upd.body_markdown.clone(),
        page_id: None,
        tags: upd.tags.clone(),
    };
    Ok((req, outcome))
}

const fn tier_as_str(t: Tier) -> &'static str {
    match t {
        Tier::Working => "working",
        Tier::Episodic => "episodic",
        Tier::Semantic => "semantic",
        Tier::Procedural => "procedural",
    }
}

fn is_slot_path(path: &PagePath) -> bool {
    path.as_str().starts_with("_slots/")
}

fn slot_kind_from_frontmatter(frontmatter: &serde_json::Value) -> SlotKind {
    match frontmatter
        .get("slot_kind")
        .and_then(serde_json::Value::as_str)
    {
        Some("invariant") => SlotKind::Invariant,
        _ => SlotKind::State,
    }
}

#[derive(Debug, Clone)]
struct SlotSnapshot {
    path: String,
    title: String,
    slot_kind: SlotKind,
    body: String,
}

fn should_skip_high_resistance_slot_update_from_frontmatter(
    path: &PagePath,
    existing_frontmatter: Option<&serde_json::Value>,
    incoming_frontmatter: &serde_json::Value,
) -> bool {
    is_slot_path(path)
        && existing_frontmatter
            .map(|fm| slot_kind_from_frontmatter(fm) == SlotKind::Invariant)
            .unwrap_or(false)
        && slot_kind_from_frontmatter(incoming_frontmatter) != SlotKind::Invariant
}

/// Build the exact ChatRequest the consolidator sends for batch
/// multi-page consolidation. Exposed so off-tree A/B harnesses
/// (e.g. `evals/`) can exercise the same workload against
/// alternative providers without duplicating the prompt.
pub fn build_batch_request(session_id: SessionId, observations: &[Observation]) -> ChatRequest {
    build_batch_request_with_slots(session_id, observations, &[])
}

fn build_batch_request_with_slots(
    session_id: SessionId,
    observations: &[Observation],
    slots: &[SlotSnapshot],
) -> ChatRequest {
    let mut buf = String::new();
    buf.push_str(
        "You are compiling a Karpathy-style multi-page wiki update. Given the \
         session's observation log, produce a ConsolidatedBatch:\n\n",
    );
    buf.push_str("Session id: ");
    buf.push_str(&session_id.to_string());
    buf.push_str("\n\nObservations:\n");
    let projected = project_observations_prefer_recent(
        observations,
        &ObservationProjectionConfig::new(
            OBSERVATION_BUDGET_CHARS,
            MAX_PROJECTED_OBSERVATIONS,
            MAX_PROJECTED_OBSERVATION_BODY_CHARS,
        )
        .with_context_label("batch consolidation"),
    );
    buf.push_str(&projected.text);
    if !slots.is_empty() {
        buf.push_str("\nCurrent `_slots/` pages (for write-regime decisions):\n");
        for slot in slots {
            buf.push_str(&format!(
                "- {} | slot_kind={} | title={}\n",
                slot.path,
                slot.slot_kind.as_str(),
                one_line(&slot.title),
            ));
            if !slot.body.trim().is_empty() {
                buf.push_str("    body:\n");
                buf.push_str(&indent_for_prompt(&clip_for_prompt(&slot.body, 1_200)));
                buf.push('\n');
            }
        }
    }
    buf.push_str(
        "\nProduce up to 5 page updates. Use these path conventions:\n\
         - sessions/<session_id>.md  (episodic, this run's narrative)\n\
         - concepts/<slug>.md         (semantic, evergreen concept pages)\n\
         - decisions/<short>.md       (semantic, ADR-style records)\n\
         - gotchas/<slug>.md          (semantic, failure modes / surprises)\n\
         - _slots/<name>.md           (pinned memory slot; use sparingly)\n\
         \n## `tier` field — EXACTLY ONE of these four strings on every update\n\
         Never an integer, never a synonym, never one of the `slot_kind` values below.\n\
         - \"working\"      (the live in-progress slice of the session — rarely used here)\n\
         - \"episodic\"     (per-session narrative; the sessions/<id>.md page)\n\
         - \"semantic\"     (durable knowledge: concepts/, decisions/, gotchas/, rules)\n\
         - \"procedural\"   (repeated patterns extracted from many episodic pages)\n\
         \n## `kind` field — EXACTLY ONE of these four strings on every update\n\
         Never an integer, never \"session\" / \"concept\" / \"note\".\n\
         - \"decision\" (the project chose X over Y)\n\
         - \"gotcha\"   (a failure mode or surprise worth remembering)\n\
         - \"rule\"     (durable project convention: \"always X\", \"never Y\")\n\
         - \"fact\"     (everything else; the default — use this for session narratives and plain concept notes)\n\
         \nWhen you mark an update as `rule`, write the body as a clear \
         standalone instruction the agent could follow on every relevant \
         action. The path you suggest for a rule will be overridden — the \
         system routes rules to `_rules/<slug>.md` automatically and the \
         lint pass surfaces a hint to copy it into the project's CLAUDE.md.\
         \n## `slot_kind` field — OPTIONAL, ONLY for `_slots/*` paths\n\
         **Completely unrelated to `tier`.** A separate flag that controls the\n\
         write regime for pinned memory slots. Do NOT put these values in `tier`.\n\
         - \"state\"      (default; mutable current focus, pending items, working context)\n\
         - \"invariant\"  (high-resistance project rules, identity, or user preferences)\n\
         Do not emit an update for an existing invariant slot unless the observations directly contradict specific existing content. State slots may be refreshed normally.\n\
         \n## Required JSON keys on every update (use these EXACT names)\n\
         - \"path\"            (string)  required — the wiki path\n\
         - \"title\"           (string)  required — the page title\n\
         - \"body_markdown\"   (string)  required — the page body in Markdown; NOTE the underscore + the suffix `_markdown`, NOT just `body`\n\
         - \"tier\"            (string)  required — one of: working | episodic | semantic | procedural\n\
         - \"kind\"            (string)  required — one of: decision | gotcha | rule | fact\n\
         - \"tags\"            (array of string)  required — may be empty `[]`, but the key must be present\n\
         - \"slot_kind\"       (string) optional — ONLY for `_slots/*`; one of \"state\" or \"invariant\"; this is the SLOT WRITE REGIME, NOT a tier value\n\
         No other keys except optional `slot_kind` on `_slots/*`. No `body`, no `content`, no `summary`. Field names \
         are case-sensitive and the `_markdown` suffix matters.\n\
         \n## Output format (read this carefully)\n\
         Reply with ONE JSON object matching the ConsolidatedBatch schema, \
         and nothing else. NO prose preamble, NO trailing commentary, NO \
         markdown headers wrapping the JSON, NO ``` code fences. The very \
         first character of your reply must be `{` and the very last `}`. \
         Strings must be JSON strings (with double quotes), not numbers \
         and not bare identifiers.\n\
         \n## Top-level shape\n\
         {\n\
         \x20\x20\"updates\": [ /* 1-5 update objects with the keys above */ ],\n\
         \x20\x20\"rationale\": \"<one short sentence about why this batch>\"\n\
         }\n",
    );
    ChatRequest {
        system: Some(BATCH_SYSTEM_PROMPT.into()),
        messages: vec![ChatMessage {
            role: Role::User,
            content: buf,
        }],
        // Generous: 32K covers a multi-page consolidation comfortably.
        // Cheaper to over-allocate than to truncate JSON mid-response.
        max_tokens: 32_000,
        temperature: Some(0.2),
    }
}

/// System prompt for batch consolidation. Loaded at compile time
/// from `prompts/batch_consolidate_system.md` so the prompt itself
/// is plain-text-editable + version-controlled as a Markdown file
/// alongside the code. Public so off-tree harnesses (`evals/`) can
/// inspect the exact prompt without duplicating it.
pub const BATCH_SYSTEM_PROMPT: &str = include_str!("../prompts/batch_consolidate_system.md");

fn build_request(
    session_id: SessionId,
    observations: &[Observation],
    current_body: &str,
) -> ChatRequest {
    let mut buf = String::new();
    buf.push_str("Session id: ");
    buf.push_str(&session_id.to_string());
    buf.push_str("\nObservations (in order):\n\n");
    let projected = project_observations_prefer_recent(
        observations,
        &ObservationProjectionConfig::new(
            OBSERVATION_BUDGET_CHARS,
            MAX_PROJECTED_OBSERVATIONS,
            MAX_PROJECTED_OBSERVATION_BODY_CHARS,
        )
        .with_context_label("single-page consolidation"),
    );
    buf.push_str(&projected.text);
    if !current_body.trim().is_empty() {
        let current_body = prepare_current_body_for_prompt(current_body);
        buf.push_str("\nCurrent (heuristic) page body:\n\n```\n");
        buf.push_str(&current_body);
        buf.push_str("\n```\n");
    }

    ChatRequest {
        system: Some(SYSTEM_PROMPT.into()),
        messages: vec![ChatMessage {
            role: Role::User,
            content: buf,
        }],
        // Sized for reasoning models too (Kimi / o3-style): each
        // consolidation call may burn ~2k tokens on hidden reasoning
        // before any visible output. With 4000 we leave ~2000 for the
        // actual ConsolidatedPage JSON, which is plenty for our
        // ~5 KB max body_markdown. Non-reasoning models stop early
        // and don't pay extra for the higher cap.
        // Generous: 32K covers a multi-page consolidation comfortably.
        // Cheaper to over-allocate than to truncate JSON mid-response.
        max_tokens: 32_000,
        temperature: Some(0.2),
    }
}

/// Character budget for observations rendered into the consolidation prompt.
/// ~4 chars per English token → ~100k token budget for the observation
/// dump, leaving the other ~100k of a 200k-context model for the system
/// prompt, page conventions, slot snapshots, the structured-output schema,
/// and the LLM's output token reservation (max_tokens=32k). Conservative:
/// providers vary on what's a "token" and some count whitespace
/// differently; under-shooting the budget loses some context but never
/// causes a 400 from the provider.
const OBSERVATION_BUDGET_CHARS: usize = 400_000;
const MAX_PROJECTED_OBSERVATIONS: usize = 256;
const MAX_PROJECTED_OBSERVATION_BODY_CHARS: usize = 3_000;
const CURRENT_BODY_BUDGET_CHARS: usize = 20_000;

fn prepare_current_body_for_prompt(current_body: &str) -> String {
    let without_raw = elide_raw_observations_section(current_body);
    clip_current_body_for_prompt(&without_raw, CURRENT_BODY_BUDGET_CHARS)
}

fn elide_raw_observations_section(current_body: &str) -> String {
    let Some(raw_start) = current_body.find("## Raw observations") else {
        return current_body.to_string();
    };

    let after_raw = raw_start + "## Raw observations".len();
    let raw_end = current_body[after_raw..]
        .find("\n## ")
        .map(|offset| after_raw + offset + 1)
        .unwrap_or(current_body.len());

    let mut out = String::with_capacity(current_body.len().saturating_sub(raw_end - raw_start));
    out.push_str(current_body[..raw_start].trim_end());
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(
        "[Raw observations section omitted; SQLite observations are supplied separately.]",
    );
    if raw_end < current_body.len() {
        out.push_str("\n\n");
        out.push_str(current_body[raw_end..].trim_start());
    }
    out
}

fn clip_current_body_for_prompt(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        out.push_str("\n[current heuristic page body truncated]");
    }
    out
}

fn build_frontmatter(page: &ConsolidatedPage) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert(
        "title".into(),
        serde_json::Value::String(page.title.clone()),
    );
    map.insert("tier".into(), serde_json::Value::String("episodic".into()));
    if !page.tags.is_empty() {
        let tags = page
            .tags
            .iter()
            .map(|t| serde_json::Value::String(t.clone()))
            .collect();
        map.insert("tags".into(), serde_json::Value::Array(tags));
    }
    map.insert("consolidated".into(), serde_json::Value::Bool(true));
    serde_json::Value::Object(map)
}

fn one_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join(" / ")
        .chars()
        .take(240)
        .collect()
}

fn clip_for_prompt(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        out.push_str("\n[truncated]");
    }
    out
}

fn indent_for_prompt(s: &str) -> String {
    s.lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// ASCII-slug a rule title for the `_rules/<slug>.md` path.
///
/// Lower-cases, replaces runs of non-`[a-z0-9]` with `-`, trims
/// leading/trailing hyphens, and caps at 60 chars. Falls back to
/// `rule` when the input has no alphanumerics (e.g. a non-Latin
/// title) so we always produce a valid PagePath.
fn slugify_for_rule(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut prev_dash = true; // leading dashes get folded
    for c in title.chars() {
        let lower = c.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        return "rule".into();
    }
    if out.len() > 60 {
        out.truncate(60);
        while out.ends_with('-') {
            out.pop();
        }
    }
    out
}

fn short_id(s: &str) -> String {
    s.chars().take(8).collect()
}

/// System prompt for single-page consolidation. Loaded at compile
/// time from `prompts/single_consolidate_system.md`.
const SYSTEM_PROMPT: &str = include_str!("../prompts/single_consolidate_system.md");

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_core::{ObservationId, ObservationKind, ProjectId, SessionId, WorkspaceId};
    use jiff::Timestamp;

    /// Helper for prompt construction tests.
    fn obs_of_size(body_len: usize) -> Observation {
        test_obs(ObservationKind::Other, "t", &"x".repeat(body_len), 5)
    }

    fn test_obs(kind: ObservationKind, title: &str, body: &str, importance: u8) -> Observation {
        Observation {
            id: ObservationId::new(),
            workspace_id: WorkspaceId::new(),
            project_id: ProjectId::new(),
            session_id: SessionId::new(),
            kind,
            title: title.into(),
            body: body.into(),
            created_at: Timestamp::UNIX_EPOCH,
            importance,
            extension: None,
            source_event: None,
        }
    }

    #[test]
    fn build_request_uses_projected_observation_metadata() {
        let observations = vec![obs_of_size(10), obs_of_size(20)];
        let request = build_request(SessionId::new(), &observations, "");
        let prompt = &request.messages[0].content;
        assert!(prompt.contains("--- observation 1/2 ---"));
        assert!(prompt.contains("id:"));
        assert!(prompt.contains("created_at:"));
        assert!(prompt.contains("importance:"));
    }

    #[test]
    fn consolidation_prompts_prefer_later_corrections() {
        let correction_rule = "treat the later observation as authoritative";
        assert!(SYSTEM_PROMPT.contains(correction_rule));
        assert!(BATCH_SYSTEM_PROMPT.contains(correction_rule));
    }

    #[test]
    fn build_request_uses_recent_projection_for_late_corrections() {
        let mut observations: Vec<_> = (0..300)
            .map(|_| {
                test_obs(
                    ObservationKind::UserPrompt,
                    "high signal filler",
                    "fix failed error regression decision",
                    9,
                )
            })
            .collect();
        observations[1] = test_obs(
            ObservationKind::UserPrompt,
            "deploy plan",
            "Deploy is manual.",
            9,
        );
        observations[220] = test_obs(
            ObservationKind::PostToolUse,
            "deploy update",
            "Deploy uses Coolify plus Traefik.",
            5,
        );

        let request = build_request(SessionId::new(), &observations, "");
        let prompt = &request.messages[0].content;

        assert!(prompt.contains("Coolify plus Traefik"));
        assert!(!prompt.contains("Deploy is manual."));
    }

    #[test]
    fn build_batch_request_uses_recent_projection_for_late_corrections() {
        let mut observations: Vec<_> = (0..300)
            .map(|_| {
                test_obs(
                    ObservationKind::UserPrompt,
                    "high signal filler",
                    "fix failed error regression decision",
                    9,
                )
            })
            .collect();
        observations[1] = test_obs(
            ObservationKind::UserPrompt,
            "deploy plan",
            "Deploy is manual.",
            9,
        );
        observations[220] = test_obs(
            ObservationKind::PostToolUse,
            "deploy update",
            "Deploy uses Coolify plus Traefik.",
            5,
        );

        let request = build_batch_request(SessionId::new(), &observations);
        let prompt = &request.messages[0].content;

        assert!(prompt.contains("Coolify plus Traefik"));
        assert!(!prompt.contains("Deploy is manual."));
    }

    #[test]
    fn build_request_elides_raw_observations_from_current_body() {
        let raw_dump = (0..2_000)
            .map(|i| format!("- `other` @ 1970-01-01T00:00:00Z — raw-entry-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let current_body = format!(
            "# session\n\nKeep this summary.\n\n## Raw observations\n\n{raw_dump}\n\n_Synthesised by ai-memory._\n"
        );

        let request = build_request(SessionId::new(), &[], &current_body);
        let prompt = &request.messages[0].content;

        assert!(prompt.contains("Keep this summary."));
        assert!(prompt.contains("Raw observations section omitted"));
        assert!(!prompt.contains("raw-entry-0"));
        assert!(!prompt.contains("raw-entry-1999"));
    }

    #[test]
    fn build_request_clips_large_current_body_with_marker() {
        let current_body = format!(
            "# huge\n\n{}\n\n## Raw observations\n\n- should-not-appear\n",
            "x".repeat(CURRENT_BODY_BUDGET_CHARS + 10_000),
        );

        let request = build_request(SessionId::new(), &[], &current_body);
        let prompt = &request.messages[0].content;

        assert!(prompt.contains("[current heuristic page body truncated]"));
        assert!(!prompt.contains("should-not-appear"));
        assert!(prompt.len() < current_body.len());
    }

    /// Slugifier produces a clean ASCII path for typical English titles.
    #[test]
    fn slugify_handles_typical_rule_title() {
        assert_eq!(
            slugify_for_rule("Never ship code without a unit test"),
            "never-ship-code-without-a-unit-test"
        );
    }

    /// Punctuation + apostrophes collapse into single hyphens; no
    /// trailing hyphen lingers from a final non-alphanumeric.
    #[test]
    fn slugify_collapses_punctuation_and_trims() {
        assert_eq!(
            slugify_for_rule("Don't merge before lint!"),
            "don-t-merge-before-lint"
        );
        assert_eq!(slugify_for_rule("---hyphenated---"), "hyphenated");
    }

    /// Non-Latin / empty-after-cleanup titles fall back to a static
    /// slug instead of producing an invalid PagePath.
    #[test]
    fn slugify_falls_back_for_unprintable_titles() {
        assert_eq!(slugify_for_rule(""), "rule");
        assert_eq!(slugify_for_rule("!!!"), "rule");
        assert_eq!(slugify_for_rule("中文"), "rule");
    }

    /// Very long titles get capped at 60 chars with no trailing dash.
    #[test]
    fn slugify_caps_length() {
        let long = "a".repeat(200);
        let slug = slugify_for_rule(&long);
        assert!(slug.len() <= 60);
        assert!(!slug.ends_with('-'));
    }

    #[test]
    fn slot_update_defaults_to_state_frontmatter() {
        let update = crate::types::ConsolidatedPageUpdate {
            path: "_slots/current_focus.md".into(),
            tier: Tier::Semantic,
            kind: crate::types::PageKind::Fact,
            title: "Current focus".into(),
            body_markdown: "Ship the slot-kind PR.".into(),
            tags: Vec::new(),
            slot_kind: SlotKind::State,
        };
        let (req, _) = build_update(WorkspaceId::new(), ProjectId::new(), &update, true).unwrap();
        assert_eq!(req.frontmatter["slot_kind"], "state");
    }

    #[test]
    fn slot_update_preserves_explicit_invariant_frontmatter() {
        let update = crate::types::ConsolidatedPageUpdate {
            path: "_slots/project_context.md".into(),
            tier: Tier::Semantic,
            kind: crate::types::PageKind::Fact,
            title: "Project context".into(),
            body_markdown: "This repo uses a markdown wiki as source of truth.".into(),
            tags: Vec::new(),
            slot_kind: SlotKind::Invariant,
        };
        let (req, _) = build_update(WorkspaceId::new(), ProjectId::new(), &update, true).unwrap();
        assert_eq!(req.frontmatter["slot_kind"], "invariant");
    }

    #[test]
    fn invariant_slot_skips_state_rewrite_candidate() {
        let path = PagePath::new("_slots/project_context.md").unwrap();
        let existing = serde_json::json!({"title": "Project context", "slot_kind": "invariant"});
        let incoming = serde_json::json!({"title": "Project context", "slot_kind": "state"});
        assert!(should_skip_high_resistance_slot_update_from_frontmatter(
            &path,
            Some(&existing),
            &incoming,
        ));
    }

    #[test]
    fn invariant_slot_allows_explicit_invariant_rewrite_candidate() {
        let path = PagePath::new("_slots/project_context.md").unwrap();
        let existing = serde_json::json!({"title": "Project context", "slot_kind": "invariant"});
        let incoming = serde_json::json!({"title": "Project context", "slot_kind": "invariant"});
        assert!(!should_skip_high_resistance_slot_update_from_frontmatter(
            &path,
            Some(&existing),
            &incoming,
        ));
    }

    #[test]
    fn non_slot_paths_ignore_slot_kind_guard() {
        let path = PagePath::new("concepts/project-context.md").unwrap();
        let existing = serde_json::json!({"slot_kind": "invariant"});
        let incoming = serde_json::json!({"slot_kind": "state"});
        assert!(!should_skip_high_resistance_slot_update_from_frontmatter(
            &path,
            Some(&existing),
            &incoming,
        ));
    }

    #[test]
    fn missing_slot_kind_defaults_to_state() {
        assert_eq!(
            slot_kind_from_frontmatter(&serde_json::json!({"title": "Pending items"})),
            SlotKind::State,
        );
    }

    #[test]
    fn batch_request_includes_existing_slot_regimes() {
        let session_id = SessionId::new();
        let slots = vec![SlotSnapshot {
            path: "_slots/project_context.md".into(),
            title: "Project context".into(),
            slot_kind: SlotKind::Invariant,
            body: "This is stable unless a later observation contradicts it.".into(),
        }];
        let request = build_batch_request_with_slots(session_id, &[], &slots);
        let prompt = &request.messages[0].content;
        assert!(prompt.contains("Current `_slots/` pages"));
        assert!(prompt.contains("_slots/project_context.md | slot_kind=invariant"));
        assert!(prompt.contains("This is stable unless"));
    }

    #[test]
    fn page_update_deserialisation_defaults_slot_kind_to_state() {
        let update: crate::types::ConsolidatedPageUpdate =
            serde_json::from_value(serde_json::json!({
                "path": "_slots/current_focus.md",
                "tier": "semantic",
                "kind": "fact",
                "title": "Current focus",
                "body_markdown": "Keep the PR narrow.",
                "tags": []
            }))
            .unwrap();
        assert_eq!(update.slot_kind, SlotKind::State);
    }
}
