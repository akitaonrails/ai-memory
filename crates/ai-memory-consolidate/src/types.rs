//! Public-facing consolidation types.

use ai_memory_core::{PageId, PagePath, Tier};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// JSON-schema-validated structured output from the LLM. The Karpathy
/// wiki pattern is "compile then keep current"; this is what one
/// compile step produces for a single page.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConsolidatedPage {
    /// Page title; rendered as the first H1 by the wiki layer.
    pub title: String,
    /// Markdown body (no frontmatter; the wiki layer adds that).
    pub body_markdown: String,
    /// Up to ~5 short tags surfaced into the page's frontmatter.
    #[serde(default)]
    pub tags: Vec<String>,
    /// One line of plain prose saying what this page covers, shown beside the
    /// title in retrieval listings. See [`ConsolidatedPageUpdate::summary`]
    /// for the shape it has to keep. Defaults to absent so existing stored
    /// outputs still deserialise.
    #[serde(default)]
    pub summary: Option<String>,
    /// Typed edges to existing pages (2.0 item 3). One field per
    /// closed-vocabulary relation kind; values are wiki paths of the target
    /// pages. Only declare a relation when the session's evidence states it
    /// plainly — all-empty is the normal case.
    #[serde(default)]
    pub relations: Relations,
}

/// Typed edges emitted by consolidation, one field per closed relation
/// kind (`causes` / `fixes` / `contradicts`).
///
/// This is a **fixed-shape object, not an open map**, deliberately (#630).
/// An open `BTreeMap` renders as a JSON-Schema `additionalProperties` map,
/// which OpenAI's strict structured-output mode cannot express — the strict
/// normaliser closes it to `additionalProperties: false`, so the model was
/// structurally unable to ever emit a relation on every OpenAI-family
/// provider (the edges silently never appeared). Three named array fields
/// are strict-expressible and serialise to the same `relations:` frontmatter
/// object the wiki write boundary already parses, so nothing downstream
/// changes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Relations {
    /// Wiki paths this page describes a cause of.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causes: Vec<String>,
    /// Wiki paths whose described problem this page fixes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixes: Vec<String>,
    /// Wiki paths this page contradicts (the lint pass surfaces these).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contradicts: Vec<String>,
}

impl Relations {
    /// Whether no relation of any kind is declared (the normal case).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.causes.is_empty() && self.fixes.is_empty() && self.contradicts.is_empty()
    }

    /// The `(relation, targets)` pairs that are non-empty, in vocabulary
    /// order — what the write boundary turns into typed links.
    pub fn non_empty(&self) -> impl Iterator<Item = (ai_memory_core::Relation, &[String])> {
        [
            (ai_memory_core::Relation::Causes, &self.causes),
            (ai_memory_core::Relation::Fixes, &self.fixes),
            (ai_memory_core::Relation::Contradicts, &self.contradicts),
        ]
        .into_iter()
        .filter(|(_, targets)| !targets.is_empty())
        .map(|(rel, targets)| (rel, targets.as_slice()))
    }
}

/// Semantic classification of one consolidated page. Surfaced into
/// the page's frontmatter (`kind: rule`, etc.) so the lint pass can
/// differentiate "decisions the project has made" from "durable
/// rules the team enforces" from raw "facts that emerged today".
///
/// The `rule` variant is special: rule-tagged pages are auto-routed
/// to `_rules/<slug>.md` and the lint pass suggests adding them to
/// the project's CLAUDE.md / AGENTS.md so they fire on every turn,
/// not just on memory_query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum PageKind {
    /// Project-wide constraint or convention. Examples: "never
    /// commit without a test", "always run lint before merging".
    /// Gets routed to `_rules/<slug>.md` + a lint suggestion to
    /// migrate into CLAUDE.md.
    Rule,
    /// Decision the project made (ADR-shaped). Examples: "chose
    /// session cookies over JWT for auth", "rejected vector RAG
    /// in favour of Karpathy wiki".
    Decision,
    /// A failure mode or surprise worth remembering. Examples:
    /// "Claude Code's session-end fires twice when /exit is typed
    /// during a tool call".
    Gotcha,
    /// A reusable multi-step workflow or operating procedure.
    Procedure,
    /// Anything that doesn't fit a stronger category. The default —
    /// keeps existing call sites that don't classify explicitly
    /// working unchanged.
    #[default]
    Fact,
}

impl PageKind {
    /// Wire string for serialisation + frontmatter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rule => "rule",
            Self::Decision => "decision",
            Self::Gotcha => "gotcha",
            Self::Procedure => "procedure",
            Self::Fact => "fact",
        }
    }
}

/// Write-regime hint for `_slots/*.md` pages.
///
/// Slot pages are always pinned, but they do not all want the same
/// consolidation gradient. `State` slots are the mutable working set
/// (current focus, pending items); `Invariant` slots are high-resistance
/// project context or user preferences and should only be rewritten when
/// observations explicitly contradict existing content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum SlotKind {
    /// Stable context or preference. High write resistance.
    Invariant,
    /// Mutable current-state slot. Default for backwards compatibility.
    #[default]
    State,
}

impl SlotKind {
    /// Wire string for serialisation + frontmatter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Invariant => "invariant",
            Self::State => "state",
        }
    }
}

/// One update inside a multi-page consolidation batch (M7b).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConsolidatedPageUpdate {
    /// Relative wiki path (`concepts/foo.md`, `decisions/0001.md`, …).
    /// When `kind` is `Rule` the consolidator overrides this to
    /// `_rules/<slug>.md` regardless — see `PageKind::Rule`.
    pub path: String,
    /// Tier classification. Typed as the [`Tier`] enum (not a free
    /// `String`) so schemars emits a closed enum in the generated
    /// JSON schema. Before this change schemars produced
    /// `{ "type": "string" }` with no constraint, and both Kimi
    /// and qwen3 routinely emitted `tier: 2` (integer) instead of
    /// the documented string values.
    pub tier: Tier,
    /// Semantic classification. Defaults to `fact` if the LLM
    /// doesn't supply one — existing consolidations without this
    /// field still deserialise.
    #[serde(default)]
    pub kind: PageKind,
    /// New page title.
    pub title: String,
    /// New markdown body.
    pub body_markdown: String,
    /// One line of plain prose saying what this page covers, shown beside
    /// the title in retrieval listings. Write a complete sentence: not a
    /// heading, not a `- **key:** value` bullet, and not a repeat of the
    /// title — the reader drops all three and would fall back to echoing
    /// this field verbatim. Omit it rather than guessing. Defaults to absent
    /// so existing structured outputs still deserialise.
    #[serde(default)]
    pub summary: Option<String>,
    /// Optional tags surfaced into frontmatter.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Write-regime hint for `_slots/*.md` updates. Ignored for non-slot
    /// paths. Defaults to `state` so existing structured outputs keep their
    /// current behaviour.
    #[serde(default)]
    pub slot_kind: SlotKind,
    /// Salient nouns this page is about — the specific technologies,
    /// components, services, and files the content names. Indexed as a
    /// retrieval stream so a query naming one of them finds the page
    /// even when the wording differs. Normalised and capped
    /// (`ai_memory_core::normalize_entities`) before storage; defaults
    /// to empty so older structured outputs still deserialise.
    #[serde(default)]
    pub entities: Vec<String>,
}

/// Batch produced by [`ConsolidatorMulti`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConsolidatedBatch {
    /// Pages to create / update.
    pub updates: Vec<ConsolidatedPageUpdate>,
    /// Brief LLM-authored note about *why* this batch was produced.
    /// Surfaced in the auto-commit message.
    #[serde(default)]
    pub rationale: String,
}

/// Outcome of a single consolidation call.
#[derive(Debug, Clone, Serialize)]
pub struct ConsolidationOutcome {
    /// Path of the page that was (or would be) written.
    pub path: PagePath,
    /// Whether the call ran in dry-run mode.
    pub dry_run: bool,
    /// New title.
    pub new_title: String,
    /// New body. Hidden when content has not changed.
    pub new_body_markdown: String,
    /// Identifier of the page that is now `is_latest = 1`. `None` on
    /// dry-run.
    pub page_id: Option<PageId>,
    /// Tags applied to the page.
    pub tags: Vec<String>,
}
